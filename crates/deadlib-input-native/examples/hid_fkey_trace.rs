//! Standalone IOHID probe: prints every keyboard (usage page 0x07) and
//! consumer-control (usage page 0x0C) HID element value, with a monotonic
//! millisecond timestamp, so we can see exactly what fires when F11/F12 are
//! pressed on a Mac keyboard — with and without `fn` held — and in what order.
//!
//! Run:  cargo run -p deadlib-input-native --example hid_fkey_trace
//! Then press F11, then F12, each (a) without fn and (b) with fn held.
//! Ctrl-C to quit. Paste the output.
//!
//! macOS only.
#![allow(clippy::missing_safety_doc)]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("hid_fkey_trace is macOS-only");
}

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::ptr;
    use std::time::Instant;

    type CFAllocatorRef = *const c_void;
    type CFRunLoopRef = *mut c_void;
    type CFStringRef = *const c_void;
    type CFTypeRef = *const c_void;

    type IOHIDManagerRef = *mut c_void;
    type IOHIDValueRef = *mut c_void;
    type IOHIDElementRef = *mut c_void;
    type IOReturn = i32;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopRun();
        static kCFRunLoopDefaultMode: CFStringRef;
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDManagerCreate(alloc: CFAllocatorRef, options: u32) -> IOHIDManagerRef;
        fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: CFTypeRef);
        fn IOHIDManagerRegisterInputValueCallback(
            manager: IOHIDManagerRef,
            callback: extern "C" fn(*mut c_void, IOReturn, *mut c_void, IOHIDValueRef),
            context: *mut c_void,
        );
        fn IOHIDManagerScheduleWithRunLoop(
            manager: IOHIDManagerRef,
            run_loop: CFRunLoopRef,
            mode: CFStringRef,
        );
        fn IOHIDManagerOpen(manager: IOHIDManagerRef, options: u32) -> IOReturn;
        fn IOHIDValueGetElement(value: IOHIDValueRef) -> IOHIDElementRef;
        fn IOHIDValueGetIntegerValue(value: IOHIDValueRef) -> isize;
        fn IOHIDElementGetUsagePage(elem: IOHIDElementRef) -> u32;
        fn IOHIDElementGetUsage(elem: IOHIDElementRef) -> u32;
    }

    struct Clock(Instant);

    extern "C" fn on_input(
        ctx: *mut c_void,
        _res: IOReturn,
        _sender: *mut c_void,
        value: IOHIDValueRef,
    ) {
        unsafe {
            let clock = &*(ctx as *const Clock);
            let elem = IOHIDValueGetElement(value);
            if elem.is_null() {
                return;
            }
            let page = IOHIDElementGetUsagePage(elem) as u16;
            let usage = IOHIDElementGetUsage(elem) as u16;
            // Keyboard/keypad (0x07) and Consumer (0x0C) only — skip mouse/other noise.
            if page != 0x07 && page != 0x0C {
                return;
            }
            let v = IOHIDValueGetIntegerValue(value) as i64;
            let ms = clock.0.elapsed().as_secs_f64() * 1000.0;
            let label = match (page, usage) {
                (0x07, 0x44) => " <- F11 (keyboard)",
                (0x07, 0x45) => " <- F12 (keyboard)",
                (0x0C, 0xE9) => " <- Volume Up (consumer)",
                (0x0C, 0xEA) => " <- Volume Down (consumer)",
                (0x0C, 0xE2) => " <- Mute (consumer)",
                _ => "",
            };
            println!(
                "{ms:9.2}ms  page=0x{page:04X} usage=0x{usage:04X} value={v}{label}"
            );
        }
    }

    pub fn run() {
        unsafe {
            let manager = IOHIDManagerCreate(ptr::null(), 0);
            assert!(!manager.is_null(), "IOHIDManagerCreate failed");
            let clock = Box::into_raw(Box::new(Clock(Instant::now())));
            IOHIDManagerSetDeviceMatching(manager, ptr::null());
            IOHIDManagerRegisterInputValueCallback(manager, on_input, clock.cast());
            let rl = CFRunLoopGetCurrent();
            IOHIDManagerScheduleWithRunLoop(manager, rl, kCFRunLoopDefaultMode);
            let r = IOHIDManagerOpen(manager, 0);
            if r != 0 {
                eprintln!(
                    "IOHIDManagerOpen returned 0x{r:08X}. If this is an access error, grant \
                     Input Monitoring to the terminal in System Settings > Privacy & Security."
                );
            }
            println!(
                "Listening for keyboard (0x07) and consumer (0x0C) HID events.\n\
                 Press F11, then F12 — each WITHOUT fn, then WITH fn held. Ctrl-C to quit.\n"
            );
            CFRunLoopRun();
        }
    }
}
