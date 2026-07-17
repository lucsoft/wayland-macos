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

/// Generate a GitHub-style identicon as BGRA pixels (premultiplied, byte order
/// B,G,R,A — same as `make_cgimage` expects), deterministic from `seed`. A 5x5
/// left-right-mirrored grid of a hash-picked accent color on a light tile. Used
/// as the default Dock icon when the client provides no real artwork.
fn identicon_bgra(seed: &str) -> (i32, i32, i32, Vec<u8>) {
    const N: usize = 512;
    // FNV-1a over the seed → deterministic hash, no deps.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in seed.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Pleasant accent palette (r,g,b); index by hash.
    const PALETTE: [(u8, u8, u8); 12] = [
        (0x2f, 0x81, 0xf7), (0xe3, 0x4c, 0x26), (0x2d, 0xa4, 0x4e), (0x8a, 0x63, 0xd2),
        (0xf0, 0x9d, 0x1a), (0x1a, 0xbc, 0x9c), (0xd6, 0x33, 0x84), (0x00, 0x96, 0x88),
        (0x79, 0x55, 0x48), (0x5c, 0x6b, 0xc0), (0xc0, 0x39, 0x2b), (0x60, 0x7d, 0x8b),
    ];
    let (fr, fg, fb) = PALETTE[(h % PALETTE.len() as u64) as usize];
    let (br, bg, bb) = (0xf0u8, 0xf0u8, 0xf0u8); // light neutral tile

    let mut px = vec![0u8; N * N * 4];
    for i in 0..N * N {
        let o = i * 4;
        px[o] = bb;
        px[o + 1] = bg;
        px[o + 2] = br;
        px[o + 3] = 255;
    }
    let margin = N / 8;
    let usable = N - 2 * margin;
    let cell = usable / 5;
    for row in 0..5usize {
        for col in 0..3usize {
            if (h >> (row * 3 + col)) & 1 == 1 {
                let x0 = margin + col * cell;
                let y0 = margin + row * cell;
                for y in y0..y0 + cell {
                    for x in x0..x0 + cell {
                        // Write the pixel and its horizontal mirror, so the icon is
                        // exactly left-right symmetric regardless of cell rounding.
                        for px_x in [x, N - 1 - x] {
                            let o = (y * N + px_x) * 4;
                            px[o] = fb;
                            px[o + 1] = fg;
                            px[o + 2] = fr;
                            px[o + 3] = 255;
                        }
                    }
                }
            }
        }
    }
    (N as i32, N as i32, (N * 4) as i32, px)
}

#[cfg(test)]
mod tests {
    use super::identicon_bgra;

    #[test]
    fn identicon_is_shaped_deterministic_and_mirrored() {
        let (w, h, stride, px) = identicon_bgra("org.gnome.Console");
        assert_eq!((w, h, stride), (512, 512, 512 * 4));
        assert_eq!(px.len(), (w * h * 4) as usize);

        // Deterministic per seed; different seeds generally differ.
        assert_eq!(identicon_bgra("org.gnome.Console").3, px);
        assert_ne!(identicon_bgra("org.kde.konsole").3, px);

        // Not a blank tile: some foreground (non-background) pixel exists.
        assert!(
            px.chunks_exact(4).any(|p| p[0] != 0xf0 || p[1] != 0xf0 || p[2] != 0xf0),
            "identicon has no foreground cells"
        );

        // Left-right mirrored (the identicon grid mirrors columns).
        let n = w as usize;
        for y in (0..n).step_by(37) {
            for x in (0..n).step_by(37) {
                let o = (y * n + x) * 4;
                let m = (y * n + (n - 1 - x)) * 4;
                assert_eq!(px[o..o + 4], px[m..m + 4], "asymmetry at ({x},{y})");
            }
        }
    }
}

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

    // Default Dock icon: a generated identicon from the app name, shown until (and
    // unless) the client provides real artwork via xdg_toplevel_icon (which
    // arrives later as a WinCmd::SetIcon and overrides this).
    let (iw, ih, istride, ipx) = identicon_bgra(&name);
    crate::mac::set_app_icon(mtm, iw, ih, istride, &ipx);

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
