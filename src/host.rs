//! Window-host child process (`--window-host <socket>`), one per Wayland app in
//! `--multiplex` mode.
//!
//! A host owns a real `NSApplication`, so it gets its own Dock tile and Cmd-Tab
//! entry (activation policy `Regular`) — that is the whole point of multiplex
//! mode: "wayland-macos" itself stays hidden (`Accessory`) and each app shows up
//! as its own macOS app. The host runs the *same* AppKit code as the in-process
//! path — it just receives `WinCmd`s over a socket instead of a channel, and
//! sends `InputEvent`s back the same way.
//!
//! Threading inside the host mirrors the normal process:
//!   - main thread: `NSApplication.run()` (all `NSWindow` work, via `mac::post`
//!     → GCD main queue → `mac::handle`, exactly as in-process).
//!   - a reader thread: decodes `Downlink` frames and re-posts them.
//!   - a forwarder thread: drains the local `InputBus` and writes `Uplink`s.

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

use crate::input::InputBus;
use crate::ipc::{read_frame, write_frame, Downlink, Uplink};

/// Entry point for a window-host child. `sock_path` is the Unix socket the
/// compositor is listening on for this app. Never returns (runs the AppKit loop).
pub fn run_window_host(sock_path: &str) -> ! {
    let stream = UnixStream::connect(sock_path)
        .unwrap_or_else(|e| panic!("[host] connect {sock_path}: {e}"));

    // First frame must be Hello: it carries the display environment we need
    // before creating any window.
    let mut reader = stream
        .try_clone()
        .expect("[host] clone socket for reader");
    let hello: Downlink = read_frame(&mut reader)
        .expect("[host] read Hello")
        .expect("[host] socket closed before Hello");
    let (regular, name) = match hello {
        Downlink::Hello {
            scale,
            out_w,
            out_h,
            insets,
            regular,
            name,
        } => {
            crate::input::set_scale(scale);
            crate::input::set_output_size(out_w, out_h);
            crate::input::set_reserved_insets(insets.0, insets.1, insets.2, insets.3);
            (regular, name)
        }
        _ => panic!("[host] first frame was not Hello"),
    };
    eprintln!("[host] up: name={name:?} regular={regular}");

    let mtm = MainThreadMarker::new().expect("[host] main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(if regular {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    });

    // Input sink: an InputBus whose waker pipe wakes a forwarder thread, which
    // drains it and writes each event up the socket. This reuses InputBus
    // verbatim — the only difference from the in-process path is where the
    // drained events go (socket, not the local Wayland thread).
    let bus = Arc::new(InputBus::new());
    crate::mac::set_input_bus(bus.clone());

    let (waker_r, waker_w) =
        rustix::pipe::pipe().expect("[host] input waker pipe");
    bus.set_waker(waker_w);

    // Forwarder: local InputBus -> Uplink::Input frames.
    {
        let mut up = stream.try_clone().expect("[host] clone socket for uplink");
        let bus = bus.clone();
        std::thread::spawn(move || {
            let _ = write_frame(&mut up, &Uplink::Ready);
            let mut drain = [0u8; 64];
            loop {
                // Block until the bus is woken, then flush everything queued.
                match rustix::io::read(&waker_r, &mut drain) {
                    Ok(0) => break, // waker closed
                    Ok(_) => {}
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => break,
                }
                for ev in bus.drain() {
                    if write_frame(&mut up, &Uplink::Input(ev)).is_err() {
                        return;
                    }
                }
            }
        });
    }

    // Reader: Downlink frames -> AppKit main thread (GCD), same as in-process.
    std::thread::spawn(move || loop {
        match read_frame::<_, Downlink>(&mut reader) {
            Ok(Some(Downlink::Cmd(cmd))) => crate::mac::post(cmd),
            Ok(Some(Downlink::Insets(t, r, b, l))) => {
                crate::input::set_reserved_insets(t, r, b, l)
            }
            Ok(Some(Downlink::Shutdown)) | Ok(None) => {
                std::process::exit(0);
            }
            Ok(Some(Downlink::Hello { .. })) => {} // ignore a second Hello
            Err(e) => {
                eprintln!("[host] read error: {e}");
                std::process::exit(0);
            }
        }
    });

    app.run();
    std::process::exit(0);
}
