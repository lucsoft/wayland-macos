//! A minimal Wayland compositor for macOS: each Wayland `xdg_toplevel` becomes a
//! native `NSWindow`. The Wayland protocol runs on a background thread; AppKit
//! owns the main thread. See `wayland.rs` and `mac.rs`.

mod bridges;
mod input;
mod mac;
mod wayland;

use std::sync::Arc;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSScreen};

fn main() {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // Match the display's backing scale (2 on Retina) so apps render crisply.
    let scale = NSScreen::mainScreen(mtm)
        .map(|s| s.backingScaleFactor().round() as i32)
        .unwrap_or(1);
    input::set_scale(scale);

    // Shared input channel: AppKit (main thread) -> Wayland thread.
    let bus = Arc::new(input::InputBus::new());
    mac::set_input_bus(bus.clone());

    // Watch the macOS pasteboard so the clipboard bridge can mirror it to
    // Wayland clients (paste direction).
    mac::start_clipboard_watch(bus.clone());

    // Run the Wayland compositor off the main thread; it marshals window
    // operations back to AppKit via the main GCD queue.
    std::thread::spawn(move || wayland::run(bus));

    app.run();
}
