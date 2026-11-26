//! `deadsync --reload-songs`: asks an already running game to reload songs.
//!
//! Songs copied into the song folders from outside the game (over the
//! network, by a script) are not on the wheel until the library is rescanned.
//! The flag leaves a small request file in the save folder and exits; the
//! running game finds it on Select Music, removes it, and reloads either the
//! packs it names or, when it names none, the whole library.

use log::{info, warn};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const FLAG: &str = "--reload-songs";
/// How often Select Music looks for a request file.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Songs and courses, as "Reload Songs/Courses" does.
    Library,
    /// Only these pack folders.
    Packs(Vec<PathBuf>),
}

/// The pack folders named after [`FLAG`], or `None` when the flag is absent.
/// No folders means the whole library.
#[must_use]
pub fn packs_from_args(args: &[String]) -> Option<Vec<String>> {
    let flag = args.iter().position(|arg| arg == FLAG)?;
    Some(
        args[flag + 1..]
            .iter()
            .take_while(|arg| !arg.starts_with("--"))
            .filter(|arg| !arg.trim().is_empty())
            .cloned()
            .collect(),
    )
}

/// Leaves a request for the running game: one pack folder per line, or an
/// empty file for the whole library. Lines are added to a request the game
/// has not picked up yet, and an empty request stays empty.
pub fn write(path: &Path, packs: &[String]) -> io::Result<()> {
    let waiting = std::fs::read_to_string(path).ok();
    let whole_library = packs.is_empty()
        || waiting
            .as_deref()
            .is_some_and(|text| lines(text).next().is_none());
    let mut content = String::new();
    if !whole_library {
        for pack in waiting
            .as_deref()
            .into_iter()
            .flat_map(lines)
            .chain(packs.iter().map(String::as_str))
        {
            content.push_str(pack);
            content.push('\n');
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Written beside it and renamed, so the game never reads half a request.
    let staged = path.with_extension("tmp");
    std::fs::write(&staged, content)?;
    std::fs::rename(&staged, path)
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty())
}

fn parse(text: &str, songs_root: &Path) -> Request {
    let mut packs = Vec::new();
    for line in lines(text) {
        let pack = songs_root.join(line);
        if !packs.contains(&pack) {
            packs.push(pack);
        }
    }
    if packs.is_empty() {
        Request::Library
    } else {
        Request::Packs(packs)
    }
}

/// Takes the request file, if there is one. The file is removed before the
/// request counts: a file that cannot be removed would be obeyed forever.
fn take(path: &Path, songs_root: &Path) -> Option<Request> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            warn!(
                "Song reload request '{}' is unreadable: {error}",
                path.display()
            );
            String::new()
        }
    };
    if let Err(error) = std::fs::remove_file(path) {
        warn!(
            "Ignoring song reload request '{}': it cannot be removed: {error}",
            path.display()
        );
        return None;
    }
    let request = parse(&text, songs_root);
    match &request {
        Request::Library => info!("Song reload requested from outside: whole library."),
        Request::Packs(packs) => info!(
            "Song reload requested from outside: {}",
            packs
                .iter()
                .map(|pack| pack.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
    Some(request)
}

/// Looks for the request file at most once per [`POLL_INTERVAL`].
#[derive(Default)]
pub(crate) struct Poller {
    next_poll: Option<Instant>,
}

impl Poller {
    pub(crate) fn poll(&mut self, path: &Path, songs_root: &Path, now: Instant) -> Option<Request> {
        if self.next_poll.is_some_and(|next| now < next) {
            return None;
        }
        self.next_poll = Some(now + POLL_INTERVAL);
        take(path, songs_root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn the_flag_takes_pack_folders_up_to_the_next_flag() {
        assert_eq!(packs_from_args(&args(&["--console"])), None);
        assert_eq!(packs_from_args(&args(&[FLAG])), Some(Vec::new()));
        assert_eq!(
            packs_from_args(&args(&[FLAG, "New Pack", "ITG/Other", "--console"])),
            Some(args(&["New Pack", "ITG/Other"]))
        );
    }

    #[test]
    fn a_request_names_packs_under_the_songs_folder_and_is_taken_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("save/reload_songs.txt");
        let songs = Path::new("/data/songs");

        write(&path, &args(&["New Pack"])).unwrap();
        write(&path, &args(&["ITG/Other", "/elsewhere/Third"])).unwrap();
        assert_eq!(
            take(&path, songs),
            Some(Request::Packs(vec![
                songs.join("New Pack"),
                songs.join("ITG/Other"),
                PathBuf::from("/elsewhere/Third"),
            ]))
        );
        assert!(!path.exists());
        assert_eq!(take(&path, songs), None);
    }

    #[test]
    fn a_request_naming_no_pack_is_for_the_whole_library() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reload_songs.txt");
        let songs = Path::new("/data/songs");

        write(&path, &[]).unwrap();
        // Packs asked for afterwards are covered by the library reload.
        write(&path, &args(&["New Pack"])).unwrap();
        assert_eq!(take(&path, songs), Some(Request::Library));

        // A bare `touch` of the file works as well.
        std::fs::write(&path, b"").unwrap();
        assert_eq!(take(&path, songs), Some(Request::Library));
    }

    #[test]
    fn the_poller_looks_once_per_interval() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reload_songs.txt");
        let songs = Path::new("/data/songs");
        let start = Instant::now();
        let mut poller = Poller::default();

        assert_eq!(poller.poll(&path, songs, start), None);
        write(&path, &[]).unwrap();
        assert_eq!(poller.poll(&path, songs, start + POLL_INTERVAL / 2), None);
        assert_eq!(
            poller.poll(&path, songs, start + POLL_INTERVAL),
            Some(Request::Library)
        );
    }
}
