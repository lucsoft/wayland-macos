//! A minimal Wayland compositor for macOS: each Wayland `xdg_toplevel` becomes a
//! native `NSWindow`. The Wayland protocol runs on a background thread; AppKit
//! owns the main thread. See `wayland.rs` and `mac.rs`.

mod input;
mod mac;
mod rail;
mod wayland;

use std::sync::Arc;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSScreen};

fn main() {
    // Back-end selection. Default: native Wayland compositor (we are the
    // compositor; protocol forwarded via waypipe). With
    // `--use-microsoft-rail-protocol`: the WSLg-style RAIL client back-end
    // (Weston composites in the container, we draw its RAIL windows). Both sit
    // behind the same WinCmd/InputBus seam — see `src/rail.rs`.
    let use_rail = std::env::args().any(|a| a == "--use-microsoft-rail-protocol");

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // Match the display's backing scale (2 on Retina) so apps render crisply.
    // Use the max backing scale across all screens — mainScreen can report 1 in a
    // background/agent launch context even when the real display is Retina.
    let scale = NSScreen::screens(mtm)
        .iter()
        .map(|s| s.backingScaleFactor().round() as i32)
        .max()
        .filter(|&s| s >= 1)
        .unwrap_or(1);
    eprintln!("[wl] display scale = {scale}");
    input::set_scale(scale);

    // Advertise the real screen as the virtual output's size (physical pixels),
    // so clients can resize windows up to the full display instead of a fixed
    // 1920x1080 default.
    if let Some(screen) = NSScreen::mainScreen(mtm) {
        let frame = screen.frame();
        input::set_output_size(
            (frame.size.width as i32) * scale,
            (frame.size.height as i32) * scale,
        );
    }

    // Shared input channel: AppKit (main thread) -> Wayland thread.
    let bus = Arc::new(input::InputBus::new());
    mac::set_input_bus(bus.clone());

    // Watch the macOS pasteboard so the clipboard bridge can mirror it to
    // Wayland clients (paste direction).
    mac::start_clipboard_watch(bus.clone());

    // Run the selected back-end off the main thread; it marshals window
    // operations back to AppKit via the main GCD queue.
    if use_rail {
        eprintln!("[wl] back-end = RAIL (--use-microsoft-rail-protocol)");
        std::thread::spawn(move || rail::run(bus));
    } else {
        eprintln!("[wl] back-end = native Wayland compositor");
        std::thread::spawn(move || wayland::run(bus));
    }

    app.run();
}
