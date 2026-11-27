//! Picks up songs written into the song folders from outside the game.
//!
//! A pack copied in over the network (rsync, scp, a synced folder) or linked
//! into place lands while the game is running. The OS file watcher reports the
//! writes; once a top-level folder has been quiet for [`SETTLE`] the packs
//! touched inside it are offered to Select Music, which reloads just those
//! packs through the same path a finished download uses.
//!
//! Everything but the final hand-over runs on one worker thread, so the game
//! thread only ever swaps out a short list of folders.

use log::{info, warn};
use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher};
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a top-level folder must see no writes before its packs are
/// reloaded, so a copy still in flight is not scanned half-written.
const SETTLE: Duration = Duration::from_secs(5);
/// How often the worker looks for settled folders while any are pending.
const TICK: Duration = Duration::from_millis(500);
/// The song scanner skips this prefix (staging folders, AppleDouble files).
const SKIPPED_PREFIX: &str = "._";

enum Message {
    Changed(PathBuf),
    /// A reload started elsewhere: of these packs, or of the whole library.
    Reloaded(Option<Vec<PathBuf>>),
}

#[derive(Default)]
pub(crate) struct Service {
    link: Option<Link>,
}

struct Link {
    tx: Sender<Message>,
    ready: Arc<Mutex<Vec<PathBuf>>>,
}

impl Service {
    pub(crate) const fn is_started(&self) -> bool {
        self.link.is_some()
    }

    /// Starts watching `roots`. Does nothing the second time.
    pub(crate) fn start(&mut self, roots: Vec<PathBuf>) {
        self.start_with(roots, SETTLE, cached_pack_dirs_under);
    }

    fn start_with(
        &mut self,
        roots: Vec<PathBuf>,
        settle: Duration,
        cached_under: fn(&Path) -> Vec<PathBuf>,
    ) {
        if self.link.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ready = Arc::new(Mutex::new(Vec::new()));
        self.link = Some(Link {
            tx: tx.clone(),
            ready: Arc::clone(&ready),
        });
        let spawned = std::thread::Builder::new()
            .name("song-watch".to_owned())
            .spawn(move || run(roots, &rx, tx, &ready, settle, cached_under));
        if let Err(error) = spawned {
            warn!("Song folder watch: could not start its thread: {error}");
        }
    }

    /// Pack folders changed on disk and quiet since, each handed out once.
    pub(crate) fn take_ready(&mut self) -> Vec<PathBuf> {
        let Some(link) = &self.link else {
            return Vec::new();
        };
        let mut ready = link.ready.lock().unwrap_or_else(PoisonError::into_inner);
        if ready.is_empty() {
            Vec::new()
        } else {
            std::mem::take(&mut *ready)
        }
    }

    /// A reload of `pack_dirs` (or of the whole library, for `None`) has
    /// started, so changes seen up to now need no reload of their own.
    pub(crate) fn note_reload(&self, pack_dirs: Option<&[PathBuf]>) {
        if let Some(link) = &self.link {
            let _ = link
                .tx
                .send(Message::Reloaded(pack_dirs.map(<[PathBuf]>::to_vec)));
        }
    }
}

fn cached_pack_dirs_under(path: &Path) -> Vec<PathBuf> {
    deadsync_simfile::runtime_cache::get_song_cache()
        .iter()
        .filter(|pack| pack.directory.starts_with(path))
        .map(|pack| pack.directory.clone())
        .collect()
}

fn run(
    roots: Vec<PathBuf>,
    rx: &Receiver<Message>,
    tx: Sender<Message>,
    ready: &Mutex<Vec<PathBuf>>,
    settle: Duration,
    cached_under: fn(&Path) -> Vec<PathBuf>,
) {
    let handler = move |event: notify::Result<notify::Event>| match event {
        Ok(event) => {
            if event.need_rescan() {
                warn!("Song folder watch: the OS dropped file events; some changes may be missed.");
            }
            if is_write(&event.kind) {
                for path in event.paths {
                    let _ = tx.send(Message::Changed(path));
                }
            }
        }
        Err(error) => log::debug!("Song folder watch: {error}"),
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(watcher) => watcher,
        Err(error) => {
            warn!("Song folder watch is off: {error}");
            return;
        }
    };
    let mut watched = Vec::with_capacity(roots.len());
    for root in roots {
        match watcher.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => watched.push(WatchedRoot::new(root)),
            Err(error) => warn!(
                "Song folder watch: not watching '{}': {error}",
                root.display()
            ),
        }
    }
    if watched.is_empty() {
        return;
    }
    info!(
        "Song folder watch: watching {} song folder(s) for new and changed packs.",
        watched.len()
    );

    let mut pending = Pending::default();
    loop {
        let message = if pending.is_empty() {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(TICK)
        };
        match message {
            Ok(Message::Changed(path)) => pending.note(&watched, &path, Instant::now()),
            Ok(Message::Reloaded(None)) => {
                pending.clear();
                lock(ready).clear();
            }
            Ok(Message::Reloaded(Some(dirs))) => {
                pending.forget_covered(&dirs, cached_under);
                lock(ready).retain(|dir| !dirs.contains(dir));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let settled = pending.take_settled(Instant::now(), settle, cached_under);
        if settled.is_empty() {
            continue;
        }
        info!(
            "Song folder watch: {} pack folder(s) changed on disk: {}",
            settled.len(),
            settled
                .iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut ready = lock(ready);
        for dir in settled {
            if !ready.contains(&dir) {
                ready.push(dir);
            }
        }
    }
}

fn lock(ready: &Mutex<Vec<PathBuf>>) -> std::sync::MutexGuard<'_, Vec<PathBuf>> {
    ready.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether an event means file content or the folder layout changed. Metadata
/// alone does not: sync clients restamp attributes on files they never rewrite.
const fn is_write(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(kind) => !matches!(kind, ModifyKind::Metadata(_)),
        EventKind::Access(kind) => matches!(kind, AccessKind::Close(AccessMode::Write)),
        _ => false,
    }
}

struct WatchedRoot {
    /// As the song scanner spells it; reload requests must match that.
    root: PathBuf,
    /// Some backends report resolved paths instead.
    resolved: Option<PathBuf>,
}

impl WatchedRoot {
    fn new(root: PathBuf) -> Self {
        let resolved = std::fs::canonicalize(&root)
            .ok()
            .filter(|resolved| resolved != &root);
        Self { root, resolved }
    }

    fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(&self.root).ok().or_else(|| {
            self.resolved
                .as_ref()
                .and_then(|resolved| path.strip_prefix(resolved).ok())
        })
    }
}

/// What changed below one top-level folder of a song root.
struct Dirty {
    last_write: Instant,
    /// The folder itself appeared, moved or went away.
    own: bool,
    /// Its immediate children with changes at or below them.
    children: BTreeSet<OsString>,
}

/// Top-level folders with unsettled writes, by their path under a song root.
#[derive(Default)]
struct Pending {
    tops: HashMap<PathBuf, Dirty>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.tops.is_empty()
    }

    fn clear(&mut self) {
        self.tops.clear();
    }

    fn note(&mut self, roots: &[WatchedRoot], path: &Path, now: Instant) {
        let Some((root, relative)) = roots
            .iter()
            .find_map(|root| Some((root, root.relative(path)?)))
        else {
            return;
        };
        let mut names = Vec::with_capacity(2);
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return;
            };
            if name.to_string_lossy().starts_with(SKIPPED_PREFIX) {
                return;
            }
            if names.len() < 2 {
                names.push(name);
            }
        }
        let Some(top) = names.first() else {
            return;
        };
        let dirty = self.tops.entry(root.root.join(top)).or_insert(Dirty {
            last_write: now,
            own: false,
            children: BTreeSet::new(),
        });
        dirty.last_write = now;
        match names.get(1) {
            Some(child) => {
                dirty.children.insert((*child).to_owned());
            }
            None => dirty.own = true,
        }
    }

    /// The packs of every folder quiet for `settle`, which then stop pending.
    fn take_settled(
        &mut self,
        now: Instant,
        settle: Duration,
        cached_under: impl Fn(&Path) -> Vec<PathBuf>,
    ) -> Vec<PathBuf> {
        let mut packs = Vec::new();
        self.tops.retain(|top, dirty| {
            let quiet = now
                .checked_duration_since(dirty.last_write)
                .is_some_and(|elapsed| elapsed >= settle);
            if quiet {
                for pack in pack_dirs(top, dirty, &cached_under) {
                    if !packs.contains(&pack) {
                        packs.push(pack);
                    }
                }
            }
            !quiet
        });
        packs.sort();
        packs
    }

    /// Drops pending folders whose packs a reload that just started covers.
    fn forget_covered(
        &mut self,
        reloading: &[PathBuf],
        cached_under: impl Fn(&Path) -> Vec<PathBuf>,
    ) {
        self.tops.retain(|top, dirty| {
            !pack_dirs(top, dirty, &cached_under)
                .iter()
                .all(|pack| reloading.contains(pack))
        });
    }
}

/// The pack folders a changed top-level folder stands for.
///
/// A top-level folder is a pack when songs sit directly in it, and otherwise a
/// series whose children are the packs, as the song scanner reads it. A folder
/// that is gone stands for whichever loaded packs lived there.
fn pack_dirs(
    top: &Path,
    dirty: &Dirty,
    cached_under: &impl Fn(&Path) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    if !top.is_dir() {
        return if top.exists() {
            // A loose file beside the packs.
            Vec::new()
        } else {
            cached_under(top)
        };
    }
    if has_direct_songs(top) {
        return vec![top.to_path_buf()];
    }
    let mut packs = Vec::new();
    let mut push = |pack: PathBuf| {
        if !packs.contains(&pack) {
            packs.push(pack);
        }
    };
    for child in &dirty.children {
        let pack = top.join(child);
        if pack.is_dir() {
            push(pack);
        } else if !pack.exists() {
            cached_under(&pack).into_iter().for_each(&mut push);
        }
    }
    if dirty.own {
        // Linked or moved in whole: nothing below it was seen changing.
        child_dirs(top).into_iter().for_each(&mut push);
    }
    packs
}

fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(SKIPPED_PREFIX)
        })
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

fn has_direct_songs(dir: &Path) -> bool {
    child_dirs(dir).iter().any(|song| has_simfile(song))
}

fn has_simfile(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("sm") || extension.eq_ignore_ascii_case("ssc")
            })
            && path.is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn none_cached(_: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    fn add_song(pack: &Path, song: &str) {
        let dir = pack.join(song);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("song.ssc"), b"#TITLE:Song;").unwrap();
    }

    fn roots(root: &Path) -> Vec<WatchedRoot> {
        vec![WatchedRoot::new(root.to_path_buf())]
    }

    #[test]
    fn a_pack_waits_until_its_writes_stop() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        add_song(&root.join("Pack"), "Song");
        let roots = roots(root);
        let settle = Duration::from_secs(5);
        let start = Instant::now();
        let mut pending = Pending::default();

        pending.note(&roots, &root.join("Pack/Song/song.ssc"), start);
        pending.note(
            &roots,
            &root.join("Pack/Song/music.ogg"),
            start + Duration::from_secs(4),
        );
        assert!(
            pending
                .take_settled(start + Duration::from_secs(6), settle, none_cached)
                .is_empty(),
            "still being written two seconds ago"
        );
        assert_eq!(
            pending.take_settled(start + Duration::from_secs(9), settle, none_cached),
            [root.join("Pack")]
        );
        assert!(pending.is_empty(), "handed out once");
    }

    #[test]
    fn a_series_folder_yields_only_the_packs_touched() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        add_song(&root.join("Series/New Pack"), "Song");
        add_song(&root.join("Series/Old Pack"), "Song");
        let roots = roots(root);
        let now = Instant::now();
        let mut pending = Pending::default();

        pending.note(&roots, &root.join("Series/New Pack/Song/song.ssc"), now);
        assert_eq!(
            pending.take_settled(now, Duration::ZERO, none_cached),
            [root.join("Series/New Pack")]
        );
    }

    #[test]
    fn a_folder_moved_in_whole_is_read_as_pack_or_series() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        add_song(&root.join("Pack"), "Song");
        add_song(&root.join("Series/One"), "Song");
        add_song(&root.join("Series/Two"), "Song");
        fs::write(root.join("notes.txt"), b"not a pack").unwrap();
        let roots = roots(root);
        let now = Instant::now();
        let mut pending = Pending::default();

        pending.note(&roots, &root.join("Pack"), now);
        pending.note(&roots, &root.join("Series"), now);
        pending.note(&roots, &root.join("notes.txt"), now);
        assert_eq!(
            pending.take_settled(now, Duration::ZERO, none_cached),
            [
                root.join("Pack"),
                root.join("Series/One"),
                root.join("Series/Two")
            ]
        );
    }

    #[test]
    fn a_removed_folder_stands_for_the_packs_loaded_from_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let roots = roots(&root);
        let now = Instant::now();
        let mut pending = Pending::default();
        let gone = root.join("Series");
        let loaded = [gone.join("One"), gone.join("Two")];

        pending.note(&roots, &gone, now);
        let settled = pending.take_settled(now, Duration::ZERO, |path: &Path| {
            loaded
                .iter()
                .filter(|pack| pack.starts_with(path))
                .cloned()
                .collect()
        });
        assert_eq!(settled, loaded);
    }

    #[test]
    fn staging_folders_and_foreign_paths_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let roots = roots(root);
        let now = Instant::now();
        let mut pending = Pending::default();

        pending.note(
            &roots,
            &root.join("._deadsync-song-1.part/Song/song.ssc"),
            now,
        );
        pending.note(&roots, &root.join("Pack/._Song/song.ssc"), now);
        pending.note(&roots, Path::new("/somewhere/else/Pack/Song/song.ssc"), now);
        pending.note(&roots, root, now);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_reload_started_elsewhere_settles_what_it_covers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        add_song(&root.join("Downloaded"), "Song");
        add_song(&root.join("Copied"), "Song");
        let roots = roots(root);
        let now = Instant::now();
        let mut pending = Pending::default();

        pending.note(&roots, &root.join("Downloaded/Song/song.ssc"), now);
        pending.note(&roots, &root.join("Copied/Song/song.ssc"), now);
        pending.forget_covered(&[root.join("Downloaded")], none_cached);
        assert_eq!(
            pending.take_settled(now, Duration::ZERO, none_cached),
            [root.join("Copied")]
        );
    }

    #[test]
    fn only_content_and_layout_changes_count_as_writes() {
        use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind};
        assert!(is_write(&EventKind::Create(CreateKind::Folder)));
        assert!(is_write(&EventKind::Remove(RemoveKind::File)));
        assert!(is_write(&EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        ))));
        assert!(is_write(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(!is_write(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any
        ))));
        assert!(!is_write(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
    }

    /// The whole path, through the OS watcher: a pack copied in is offered
    /// once its writes stop, and a pack linked in is offered too.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_pack_copied_in_while_watching_is_offered() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("songs");
        let elsewhere = temp.path().join("groups/Linked");
        fs::create_dir_all(&root).unwrap();
        add_song(&elsewhere, "Song");

        let mut service = Service::default();
        service.start_with(vec![root.clone()], Duration::from_millis(200), none_cached);
        // The watch is set up on the worker thread.
        std::thread::sleep(Duration::from_millis(300));
        add_song(&root.join("Copied"), "Song");
        std::os::unix::fs::symlink(&elsewhere, root.join("Linked")).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut offered = Vec::new();
        while offered.len() < 2 && Instant::now() < deadline {
            offered.extend(service.take_ready());
            std::thread::sleep(Duration::from_millis(50));
        }
        offered.sort();
        assert_eq!(offered, [root.join("Copied"), root.join("Linked")]);
    }
}
