//! A minimal Wayland compositor for macOS: each Wayland `xdg_toplevel` becomes a
//! native `NSWindow`. The Wayland protocol runs on a background thread; AppKit
//! owns the main thread. See `wayland.rs` and `mac.rs`.

mod host;
mod input;
mod ipc;
mod mac;
mod rail;
mod router;
mod wayland;

use std::sync::Arc;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSScreen};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Window-host child (`--window-host <socket>`): in --multiplex mode the
    // compositor re-executes itself once per Wayland app to give each app its own
    // NSApplication (its own Dock tile / Cmd-Tab entry). This branch never
    // returns — it runs that app's AppKit loop. See src/host.rs.
    if let Some(i) = args.iter().position(|a| a == "--window-host") {
        let sock = args.get(i + 1).expect("--window-host needs a socket path");
        host::run_window_host(sock);
    }

    // Back-end selection. Default: native Wayland compositor (we are the
    // compositor; protocol forwarded via waypipe). With
    // `--use-microsoft-rail-protocol`: the WSLg-style RAIL client back-end
    // (Weston composites in the container, we draw its RAIL windows). Both sit
    // behind the same WinCmd/InputBus seam — see `src/rail.rs`.
    let use_rail = args.iter().any(|a| a == "--use-microsoft-rail-protocol");

    // --multiplex: hide "wayland-macos" itself and surface each Wayland app as
    // its own native macOS app (see src/router.rs). The compositor becomes an
    // Accessory (no Dock tile / no Cmd-Tab) and owns no windows; per-app
    // window-host children own the NSWindows.
    let multiplex = args.iter().any(|a| a == "--multiplex");

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(if multiplex {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    });

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

    // Detect the macOS keyboard layout HERE, on the main thread, and cache it.
    // The Carbon TIS/TSM API is main-thread-only; calling it from the Wayland
    // thread (as make_keymap_file used to) races AppKit's TSM init inside
    // app.run() and aborts the process ("TIS/TSM API ... in two threads").
    input::set_mac_layout(mac::macos_keyboard_layout());

    // Shared input channel: AppKit (main thread) -> Wayland thread.
    let bus = Arc::new(input::InputBus::new());
    mac::set_input_bus(bus.clone());

    // In multiplex mode, turn on WinCmd routing to per-app hosts. Their input
    // comes back over the same sockets and is pushed into this same `bus`, so the
    // Wayland thread consumes it exactly as in the single-process path.
    if multiplex {
        eprintln!("[wl] multiplex = on (per-app window hosts; compositor hidden)");
        let (out_w, out_h) = input::output_size();
        router::enable(bus.clone(), scale, out_w, out_h);
    }

    // Watch the macOS pasteboard so the clipboard integration can mirror it to
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
