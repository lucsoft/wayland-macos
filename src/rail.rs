//! RAIL back-end (`--use-microsoft-rail-protocol`).
//!
//! Alternative to the native Wayland compositor (`src/wayland/`). Instead of
//! *being* the compositor and forwarding the Wayland protocol across the
//! container boundary (waypipe), this mode is the client half of the WSLg
//! model:
//!
//! ```text
//! Linux apps ──Wayland──> Weston (RDP backend + RAIL shell, in container)
//!                              │  composites; serves per-window RAIL over RDP
//!                              │ RDP/RAIL over TCP
//!                              ▼
//!                    FreeRDP RAIL client (here) ──> WinCmd ──> NSWindow
//! ```
//!
//! It shares the exact same seam as the Wayland back-end: it drives AppKit
//! through `mac::post` (`WinCmd`) and consumes `InputEvent`s from the
//! [`InputBus`]. That keeps `src/mac.rs` transport-agnostic — the AppKit side
//! does not know or care whether windows come from Wayland or RDP/RAIL.
//!
//! The RDP/RAIL protocol itself is handled by FreeRDP 3 through a small C bridge
//! (`csrc/rail_bridge.c`); this module is the Rust half: it starts the bridge,
//! translates its window/surface callbacks into `WinCmd`, and forwards input.
//!
//! Built only with the `rail` Cargo feature (which links FreeRDP). Without it,
//! the flag prints a hint to rebuild — see the `#[cfg(not(feature = "rail"))]`
//! stub at the bottom.

use std::sync::Arc;

use crate::input::InputBus;

#[cfg(feature = "rail")]
mod imp {
    use super::*;
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::os::fd::OwnedFd;
    use std::ptr;

    use crate::input::InputEvent;
    use crate::mac::{self, WinCmd};

    // RDP PTR_FLAGS_* (freerdp/input.h).
    const PTR_FLAGS_MOVE: u16 = 0x0800;
    const PTR_FLAGS_DOWN: u16 = 0x8000;
    const PTR_FLAGS_BUTTON1: u16 = 0x1000; // left
    const PTR_FLAGS_BUTTON2: u16 = 0x2000; // right
    const PTR_FLAGS_BUTTON3: u16 = 0x4000; // middle
    const PTR_FLAGS_WHEEL: u16 = 0x0200;
    const PTR_FLAGS_WHEEL_NEGATIVE: u16 = 0x0100;

    // Linux evdev button codes (as delivered on the InputBus).
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;

    /// Mirrors `rail_callbacks` in csrc/rail_bridge.h.
    #[repr(C)]
    struct RailCallbacks {
        user: *mut c_void,
        window_create:
            extern "C" fn(*mut c_void, u32, i32, i32, u32, u32, *const c_char),
        window_update: extern "C" fn(*mut c_void, u32, i32, i32, u32, u32),
        window_title: extern "C" fn(*mut c_void, u32, *const c_char),
        window_delete: extern "C" fn(*mut c_void, u32),
        window_surface: extern "C" fn(*mut c_void, u32, u32, u32, u32, *const u8),
        disconnected: extern "C" fn(*mut c_void),
    }

    extern "C" {
        fn rail_run(
            host: *const c_char,
            port: c_int,
            app: *const c_char,
            cb: *const RailCallbacks,
        ) -> c_int;
        fn rail_send_pointer(window_id: u32, local_x: i32, local_y: i32, flags: u16);
        fn rail_send_key(scancode: u16, down: c_int);
        fn rail_stop();
    }

    fn cstr(p: *const c_char) -> String {
        if p.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    // --- bridge callbacks (run on the FreeRDP event-loop thread) -----------
    //
    // RAIL geometry arrives in RDP desktop pixels rendered at scale 1 by Weston.
    // We pass those through as both physical size and logical destination size
    // (dst_w/dst_h), so the window is sized correctly in points; crisp Retina
    // scaling is a later refinement (would require telling Weston to render at
    // the Mac's backing scale). `decorated: true` gives each app a native macOS
    // titlebar.

    extern "C" fn on_window_create(
        _user: *mut c_void,
        id: u32,
        _x: i32,
        _y: i32,
        w: u32,
        h: u32,
        title: *const c_char,
    ) {
        // RAIL emits 0x0 utility/system windows (owned popups, shell helpers)
        // that never get content; skip them so we don't spawn stray NSWindows.
        if w == 0 || h == 0 {
            return;
        }
        eprintln!("[rail] window create id={id} {w}x{h} title={:?}", cstr(title));
        mac::post(WinCmd::Create {
            id,
            width: w as i32,
            height: h as i32,
            // dst 0: size the window from buffer pixels / output scale (like a
            // Wayland CSD window), not from raw pixels-as-points.
            dst_w: 0,
            dst_h: 0,
            // RAIL surfaces already include the app's own decorations (CSD —
            // e.g. weston-terminal's titlebar), so the NSWindow must be
            // borderless. Adding a native titlebar here double-decorates.
            decorated: false,
            title: cstr(title),
            geom: (0, 0, 0, 0),
        });
    }

    extern "C" fn on_window_update(
        _user: *mut c_void,
        _id: u32,
        _x: i32,
        _y: i32,
        _w: u32,
        _h: u32,
    ) {
        // Size follows the next surface frame (which resizes the NSWindow to the
        // buffer); nothing to do for geometry-only updates. Placement is left to
        // the compositor, as in Wayland mode.
    }

    extern "C" fn on_window_title(_user: *mut c_void, id: u32, title: *const c_char) {
        mac::post(WinCmd::Title {
            id,
            title: cstr(title),
        });
    }

    extern "C" fn on_window_delete(_user: *mut c_void, id: u32) {
        eprintln!("[rail] window delete id={id}");
        mac::post(WinCmd::Destroy { id });
    }

    extern "C" fn on_window_surface(
        _user: *mut c_void,
        id: u32,
        w: u32,
        h: u32,
        stride: u32,
        pixels: *const u8,
    ) {
        if pixels.is_null() || w == 0 || h == 0 {
            return;
        }
        static FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
        if FIRST.swap(false, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("[rail] first surface frame id={id} {w}x{h} stride={stride}");
        }
        let len = stride as usize * h as usize;
        let pixels = unsafe { std::slice::from_raw_parts(pixels, len) }.to_vec();
        mac::post(WinCmd::Frame {
            id,
            width: w as i32,
            height: h as i32,
            stride: stride as i32,
            // dst 0: derive logical size from buffer pixels / output scale.
            dst_w: 0,
            dst_h: 0,
            pixels,
            // RAIL/gfx surfaces are ordinary 8-bit BGRA (SDR); no HDR metadata.
            format: mac::PixelFormat::Bgra8888,
            color: None,
            geom: (0, 0, 0, 0),
        });
    }

    extern "C" fn on_disconnected(_user: *mut c_void) {
        eprintln!("[rail] RDP session disconnected");
    }

    /// A pipe whose read end blocks (the drain loop sleeps on it) and whose write
    /// end is non-blocking (so `InputBus::push` from AppKit never stalls).
    fn waker_pipe() -> (OwnedFd, OwnedFd) {
        let (r, w) = rustix::pipe::pipe().expect("pipe");
        let flags = rustix::fs::fcntl_getfl(&w).expect("F_GETFL");
        rustix::fs::fcntl_setfl(&w, flags | rustix::fs::OFlags::NONBLOCK).expect("F_SETFL");
        (r, w)
    }

    fn button_flag(button: u32) -> u16 {
        match button {
            BTN_LEFT => PTR_FLAGS_BUTTON1,
            BTN_RIGHT => PTR_FLAGS_BUTTON2,
            BTN_MIDDLE => PTR_FLAGS_BUTTON3,
            _ => 0,
        }
    }

    pub fn run(bus: Arc<InputBus>) {
        let host = std::env::var("RAIL_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port: i32 = std::env::var("RAIL_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3389);
        // RemoteApp program to launch via RAIL Client Execute. Must be non-empty
        // — an empty program makes the WSLg server return an error ExecResult
        // that FreeRDP's rail channel fails to parse (kills the session). Default
        // matches the container's bundled app; override with RAIL_APP.
        let app = std::env::var("RAIL_APP")
            .ok()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| "weston-terminal".to_string());
        eprintln!("[rail] connecting to {host}:{port} (RemoteApp/RAIL)");

        let (wake_r, wake_w) = waker_pipe();
        bus.set_waker(wake_w);

        // FreeRDP event loop on its own thread (blocking).
        let host_c = CString::new(host).expect("host");
        let app_c = CString::new(app).expect("app");
        std::thread::spawn(move || {
            let cb = RailCallbacks {
                user: ptr::null_mut(),
                window_create: on_window_create,
                window_update: on_window_update,
                window_title: on_window_title,
                window_delete: on_window_delete,
                window_surface: on_window_surface,
                disconnected: on_disconnected,
            };
            let rc = unsafe { rail_run(host_c.as_ptr(), port, app_c.as_ptr(), &cb) };
            eprintln!("[rail] rail_run returned {rc}");
        });

        // Input drain loop: block on the waker pipe, then forward to RDP.
        let mut last_x = 0i32;
        let mut last_y = 0i32;
        let mut buf = [0u8; 64];
        loop {
            // Blocks until AppKit pushes input and writes the wake byte.
            let _ = rustix::io::read(&wake_r, &mut buf);
            for ev in bus.drain() {
                match ev {
                    InputEvent::PointerEnter { window_id, x, y }
                    | InputEvent::PointerMotion { window_id, x, y } => {
                        last_x = x as i32;
                        last_y = y as i32;
                        unsafe {
                            rail_send_pointer(window_id, last_x, last_y, PTR_FLAGS_MOVE)
                        };
                    }
                    InputEvent::PointerButton {
                        window_id,
                        button,
                        pressed,
                    } => {
                        let flags = button_flag(button)
                            | if pressed { PTR_FLAGS_DOWN } else { 0 };
                        unsafe { rail_send_pointer(window_id, last_x, last_y, flags) };
                    }
                    InputEvent::PointerAxis { dx, dy } => {
                        // Vertical wheel only; RDP encodes rotation in the low 8 bits.
                        let step = if dy.abs() >= dx.abs() { dy } else { 0.0 };
                        if step != 0.0 {
                            let mag = 120u16.min((step.abs() * 10.0) as u16).max(1);
                            let mut flags = PTR_FLAGS_WHEEL | (mag & 0x00FF);
                            if step > 0.0 {
                                flags |= PTR_FLAGS_WHEEL_NEGATIVE; // scroll down
                            }
                            unsafe { rail_send_pointer(0, last_x, last_y, flags) };
                        }
                    }
                    InputEvent::Key { keycode, pressed } => {
                        // evdev keycodes coincide with RDP set-1 scancodes for the
                        // main key block (extended keys need refinement).
                        unsafe { rail_send_key(keycode as u16, pressed as c_int) };
                    }
                    // Focus / Resize / popups / clipboard aren't wired for RAIL yet.
                    _ => {}
                }
            }
        }
    }

    /// Ask the running session to disconnect. Currently unused (the process exits
    /// on quit), but part of the bridge contract.
    #[allow(dead_code)]
    pub fn stop() {
        unsafe { rail_stop() };
    }
}

#[cfg(feature = "rail")]
pub use imp::run;

/// Stub used when the crate is built without the `rail` feature (the default):
/// FreeRDP isn't linked, so the mode can't run. Point the user at the rebuild.
#[cfg(not(feature = "rail"))]
pub fn run(_bus: Arc<InputBus>) {
    eprintln!(
        "[rail] --use-microsoft-rail-protocol requires a build with the `rail` \
         feature (FreeRDP)."
    );
    eprintln!("[rail] Rebuild with:  cargo build --features rail   (needs `brew install freerdp`)");
    eprintln!("[rail] Falling back to no windows; run without the flag for the native compositor.");
}
