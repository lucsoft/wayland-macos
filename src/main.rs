//! A minimal Wayland compositor for macOS: each Wayland `xdg_toplevel` becomes a
//! native `NSWindow`. The Wayland protocol runs on a background thread; AppKit
//! owns the main thread. See `wayland.rs` and `mac.rs`.
//!
//! This is the `wayland-macos-core` binary — the compositor proper. The
//! user-facing entry point is the `wayland-macos` orchestrator CLI (`src/cli.rs`),
//! which builds and launches this alongside pulseaudio/waypipe/socat.

mod host;
mod input;
mod ipc;
mod mac;
mod rail;
mod router;
mod wayland;

use std::sync::Arc;

use std::io::Write;

use log::info;
use objc2::MainThreadMarker;

/// stderr logger tagged with timestamp, level, target, and **thread name** — the
/// thread is the key axis of this codebase (main/AppKit vs `wayland`/`rail` vs
/// per-app host threads). Level defaults to `info`; filter with `RUST_LOG`.
fn init_logger() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            let ts = buf.timestamp();
            let thread = std::thread::current().name().unwrap_or("?").to_owned();
            writeln!(
                buf,
                "[{ts} {:5} {}/{thread}] {}",
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();
}
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSScreen};

/// Size (in points) of the bounding box that encloses every screen — the flat
/// desktop RAIL advertises so a window can be dragged across all monitors. The
/// origin conversion (this box's top-left ↔ each RDP offset) lives in
/// `mac.rs`'s `WinCmd::Move`, which recomputes the same union.
fn screen_union_size(mtm: MainThreadMarker) -> Option<(f64, f64)> {
    let screens = NSScreen::screens(mtm);
    let (mut min_x, mut min_y, mut max_x, mut max_y) =
        (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for s in screens.iter() {
        let f = s.frame();
        min_x = min_x.min(f.origin.x);
        min_y = min_y.min(f.origin.y);
        max_x = max_x.max(f.origin.x + f.size.width);
        max_y = max_y.max(f.origin.y + f.size.height);
    }
    (min_x.is_finite() && max_x.is_finite()).then_some((max_x - min_x, max_y - min_y))
}

fn main() {
    // Log to stderr via the `log` facade. Default level is `info`; override per
    // target with e.g. `RUST_LOG=wl=debug,mac=info`. Targets mirror the former
    // eprintln prefixes: `wl`, `mac`, `rail`, `router`, `host`, `clipboard`, …
    // The thread name is included because the whole architecture is a thread
    // seam (main/AppKit ↔ `wayland`/`rail` ↔ per-app host threads).
    init_logger();

    let args: Vec<String> = std::env::args().collect();

    // Window-host child (`--window-host <socket>`): in --multiplex mode the
    // compositor re-executes itself once per Wayland app to give each app its own
    // NSApplication (its own Dock tile / Cmd-Tab entry). This branch never
    // returns — it runs that app's AppKit loop. See src/host.rs.
    if let Some(i) = args.iter().position(|a| a == "--window-host") {
        let sock = args.get(i + 1).expect("--window-host needs a socket path");
        host::run_window_host(sock);
    }

    // Back-end selection is a build-time choice. Default (no `rail` feature):
    // native Wayland compositor (we are the compositor; protocol forwarded via
    // waypipe). A build with `--features rail` is a RAIL-only build: the
    // WSLg-style RAIL client back-end (Weston composites in the container, we
    // draw its RAIL windows). Both sit behind the same WinCmd/InputBus seam —
    // see `src/rail.rs`.
    let use_rail = cfg!(feature = "rail");
    // Record the back-end early: the router (multiplex, below) reads it to tell
    // each host to keep RAIL windows non-resizable.
    mac::RAIL_MODE.store(use_rail, std::sync::atomic::Ordering::Relaxed);

    // --multiplex: hide the compositor itself and surface each Wayland app as
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
    info!(target: "wl", "display scale = {scale}");
    input::set_scale(scale);

    // Advertise the virtual output's size (physical pixels). Native Wayland is
    // single-output: the main screen. RAIL instead spans **all** monitors as one
    // flat desktop (the union bounding box) so windows can be dragged across
    // displays — weston owns placement over that whole desktop and we mirror it
    // (see WinCmd::Move). Without this weston clamps to one screen, so a window
    // can't cross to a second monitor.
    let out_frame = if use_rail {
        screen_union_size(mtm)
    } else {
        NSScreen::mainScreen(mtm).map(|s| {
            let f = s.frame();
            (f.size.width, f.size.height)
        })
    };
    if let Some((w, h)) = out_frame {
        input::set_output_size((w as i32) * scale, (h as i32) * scale);
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
        info!(target: "wl", "multiplex = on (per-app window hosts; compositor hidden)");
        let (out_w, out_h) = input::output_size();
        router::enable(bus.clone(), scale, out_w, out_h);
    }

    // Watch the macOS pasteboard so the clipboard integration can mirror it to
    // Wayland clients (paste direction).
    mac::start_clipboard_watch(bus.clone());

    // Run the selected back-end off the main thread; it marshals window
    // operations back to AppKit via the main GCD queue.
    if use_rail {
        info!(target: "wl", "back-end = RAIL (--features rail)");
        std::thread::Builder::new()
            .name("rail".into())
            .spawn(move || rail::run(bus))
            .expect("spawn rail thread");
    } else {
        info!(target: "wl", "back-end = native Wayland compositor");
        std::thread::Builder::new()
            .name("wayland".into())
            .spawn(move || wayland::run(bus))
            .expect("spawn wayland thread");
    }

    app.run();
}
