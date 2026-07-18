//! RAIL back-end (built with `--features rail`).
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
//! the compositor selects the native Wayland back-end and this module is a stub
//! that never runs — see the `#[cfg(not(feature = "rail"))]` stub at the bottom.

use std::sync::Arc;

use crate::input::InputBus;

#[cfg(feature = "rail")]
mod imp {
    use super::*;
    use log::{debug, error, info, warn};
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::os::fd::OwnedFd;
    use std::ptr;
    use std::sync::Mutex;

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
        window_move_start: extern "C" fn(*mut c_void, u32),
        cursor: extern "C" fn(*mut c_void, u32, u32, u32, u32, u32, *const u8),
        cursor_hidden: extern "C" fn(*mut c_void),
        cursor_default: extern "C" fn(*mut c_void),
        log: extern "C" fn(*mut c_void, c_int, *const c_char),
        window_icon: extern "C" fn(*mut c_void, u32, u32, u32, u32, *const u8),
    }

    /// Mirrors `rail_monitor` in csrc/rail_bridge.h.
    #[repr(C)]
    struct RailMonitor {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        scale: u32,
        is_primary: i32,
    }

    extern "C" {
        fn rail_run(
            host: *const c_char,
            port: c_int,
            app: *const c_char,
            desktop_w: u32,
            desktop_h: u32,
            scale: u32,
            monitors: *const RailMonitor,
            monitor_count: u32,
            cb: *const RailCallbacks,
        ) -> c_int;
        fn rail_send_pointer(window_id: u32, local_x: i32, local_y: i32, flags: u16);
        fn rail_send_key(scancode: u16, down: c_int);
        fn rail_stop();
    }

    /// Window ids the RAIL back-end has opened and not yet deleted. On session
    /// disconnect the server stops sending per-window `window_delete`s, so we
    /// destroy whatever is still live here (otherwise the NSWindows linger with
    /// a frozen last frame). Touched from the FreeRDP event-loop thread only.
    static LIVE_WINDOWS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

    /// Global render scale advertised to weston (the primary monitor's backing
    /// factor). weston reports window geometry/positions in physical pixels at
    /// this scale, so the callbacks divide by it to get logical points. Set once
    /// in `run()` before the FreeRDP thread starts.
    static RAIL_SCALE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

    fn rail_scale() -> i32 {
        RAIL_SCALE.load(std::sync::atomic::Ordering::Relaxed).max(1)
    }

    /// Each window's logical size in points (physical geometry ÷ scale), tracked
    /// from the window orders. Used as the present destination so a HiDPI buffer
    /// (a 2x app on a Retina display) is shown crisp at its logical size.
    static WIN_LOGICAL: Mutex<std::collections::BTreeMap<u32, (i32, i32)>> =
        Mutex::new(std::collections::BTreeMap::new());

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
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        title: *const c_char,
    ) {
        // RAIL emits 0x0 utility/system windows (owned popups, shell helpers)
        // that never get content; skip them so we don't spawn stray NSWindows.
        if w == 0 || h == 0 {
            return;
        }
        let name = cstr(title);
        // weston reports geometry in physical pixels at the advertised scale;
        // convert to logical points for the (scale-agnostic) AppKit side.
        let s = rail_scale();
        let (lw, lh) = ((w as i32 / s).max(1), (h as i32 / s).max(1));
        info!(target: "rail", "window create id={id} {w}x{h} (logical {lw}x{lh}) title={name:?}");
        LIVE_WINDOWS.lock().unwrap().push(id);
        WIN_LOGICAL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (lw, lh));
        // --multiplex: give each RAIL toplevel its own per-app window-host (its
        // own Dock tile / Cmd-Tab entry). RAIL has no per-window app id in this
        // build, so key the host by the window id (one tile per window) and name
        // it from the title (fallback to the RemoteApp program). A no-op when the
        // router is disabled, so this is safe to call unconditionally.
        let display = if name.is_empty() { "weston-terminal" } else { &name };
        mac::assign_window(id, id, display, true);
        mac::post(WinCmd::Create {
            id,
            width: lw,
            height: lh,
            dst_w: lw,
            dst_h: lh,
            // RAIL surfaces already include the app's own decorations (CSD —
            // e.g. weston-terminal's titlebar), so the NSWindow must be
            // borderless. Adding a native titlebar here double-decorates.
            decorated: false,
            title: cstr(title),
            geom: (0, 0, 0, 0),
        });
        // Place at weston's chosen position so the mirror is aligned from the
        // start (no jump on the first move, and weston's edges map to the
        // screen's — see WinCmd::Move). Position is physical → logical.
        mac::post(WinCmd::Move { id, x: x / s, y: y / s });
    }

    extern "C" fn on_window_update(
        _user: *mut c_void,
        id: u32,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) {
        // weston does a server-side interactive move: it owns the window position
        // and streams new offsets here (physical pixels). Mirror them onto the
        // NSWindow so it follows the drag, converting to logical points; and keep
        // the tracked logical size current for surface presentation.
        let s = rail_scale();
        if w > 0 && h > 0 {
            WIN_LOGICAL
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, ((w as i32 / s).max(1), (h as i32 / s).max(1)));
        }
        mac::post(WinCmd::Move { id, x: x / s, y: y / s });
    }

    extern "C" fn on_window_title(_user: *mut c_void, id: u32, title: *const c_char) {
        mac::post(WinCmd::Title {
            id,
            title: cstr(title),
        });
    }

    /// A RAIL window icon (32-bit BGRA). In --multiplex mode this becomes the
    /// app's Dock icon (routed to the owning host); single-process it sets this
    /// process's Dock icon. Apps without an icon never trigger this.
    extern "C" fn on_window_icon(
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
        let len = stride as usize * h as usize;
        let pixels = unsafe { std::slice::from_raw_parts(pixels, len) }.to_vec();
        debug!(target: "rail", "window icon id={id} {w}x{h}");
        mac::post(WinCmd::SetIcon {
            id,
            width: w as i32,
            height: h as i32,
            stride: stride as i32,
            pixels,
        });
    }

    extern "C" fn on_window_delete(_user: *mut c_void, id: u32) {
        info!(target: "rail", "window delete id={id}");
        LIVE_WINDOWS.lock().unwrap().retain(|&w| w != id);
        WIN_LOGICAL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
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
        // Present the buffer at the window's logical size: a HiDPI-aware app on a
        // 2x display renders a 2x buffer, shown crisp at its logical points; a
        // non-aware app renders 1x, shown ~1:1. Fall back to buffer÷scale if we
        // have no tracked size yet.
        let s = rail_scale();
        let (dst_w, dst_h) = WIN_LOGICAL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .copied()
            .unwrap_or(((w as i32 / s).max(1), (h as i32 / s).max(1)));
        let len = stride as usize * h as usize;
        let pixels = unsafe { std::slice::from_raw_parts(pixels, len) }.to_vec();
        mac::post(WinCmd::Frame {
            id,
            width: w as i32,
            height: h as i32,
            stride: stride as i32,
            dst_w,
            dst_h,
            pixels,
            // RAIL/gfx surfaces are ordinary 8-bit BGRA (SDR); no HDR metadata.
            format: mac::PixelFormat::Bgra8888,
            color: None,
            geom: (0, 0, 0, 0),
        });
    }

    /// The server asked us to move a window locally (the user grabbed the app's
    /// own CSD titlebar). Trigger the existing native NSWindow drag — the next
    /// pointer-drag event hands off to `performWindowDragWithEvent`.
    extern "C" fn on_window_move_start(_user: *mut c_void, id: u32) {
        debug!(target: "rail", "server local move-size (move) id={id}");
        mac::post(WinCmd::StartMove { id });
    }

    /// The server changed the cursor to a bitmap (RDP pointer update). Turn it
    /// into the shared `SetCursorImage` path, which builds an NSCursor and applies
    /// it via the views' cursor rects (cursor state is global on the mac side, so
    /// this isn't per-window). RDP pointer bitmaps arrive at device resolution
    /// regardless of the desktop scale, so the buffer (and its hotspot) is scale-1;
    /// `make_cursor` therefore keeps it at 1:1 point size (rather than halving it
    /// on a Retina display, which made the cursor tiny).
    extern "C" fn on_cursor(
        _user: *mut c_void,
        w: u32,
        h: u32,
        stride: u32,
        hotspot_x: u32,
        hotspot_y: u32,
        pixels: *const u8,
    ) {
        if pixels.is_null() || w == 0 || h == 0 {
            return;
        }
        let len = stride as usize * h as usize;
        let pixels = unsafe { std::slice::from_raw_parts(pixels, len) }.to_vec();
        mac::post(WinCmd::SetCursorImage {
            width: w as i32,
            height: h as i32,
            stride: stride as i32,
            hotspot_x: hotspot_x as i32,
            hotspot_y: hotspot_y as i32,
            // RDP pointer bitmaps are at device resolution (scale 1).
            scale: 1,
            pixels,
        });
    }

    /// The server hid the cursor (null pointer).
    extern "C" fn on_cursor_hidden(_user: *mut c_void) {
        mac::post(WinCmd::HideCursor);
    }

    /// The server asked for the default cursor; map to the system arrow (shape 1
    /// falls through to `NSCursor::arrowCursor` in `map_cursor`).
    extern "C" fn on_cursor_default(_user: *mut c_void) {
        mac::post(WinCmd::SetCursor { shape: 1 });
    }

    extern "C" fn on_disconnected(_user: *mut c_void) {
        warn!(target: "rail", "RDP session disconnected");
        // The session is gone; close every window it opened. Draining the list
        // also stops a late `window_delete` from double-destroying.
        let live: Vec<u32> = std::mem::take(&mut *LIVE_WINDOWS.lock().unwrap());
        for id in live {
            mac::post(WinCmd::Destroy { id });
        }
    }

    /// Bridge log callback (see `rail_callbacks.log`): re-emit the C bridge's
    /// messages through the `log` facade so they share the `rail` target,
    /// thread name, and RUST_LOG filtering with the rest of the process. Levels
    /// mirror the RAIL_LOG_* constants in `rail_bridge.h`.
    extern "C" fn on_log(_user: *mut c_void, level: c_int, msg: *const c_char) {
        let msg = cstr(msg);
        match level {
            0 => error!(target: "rail", "{msg}"),
            1 => warn!(target: "rail", "{msg}"),
            2 => info!(target: "rail", "{msg}"),
            _ => debug!(target: "rail", "{msg}"),
        }
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
        info!(target: "rail", "connecting to {host}:{port} (RemoteApp/RAIL)");

        let (wake_r, wake_w) = waker_pipe();
        bus.set_waker(wake_w);

        // Render scale: weston-mirror scales globally (not per-monitor), so use
        // the PRIMARY monitor's backing factor. A HiDPI-aware app then renders
        // crisp on a Retina primary; windows on a standard monitor render at the
        // same scale (correct size, softness varies). The desktop is advertised
        // in PHYSICAL pixels at that scale so weston's 2x geometry fits.
        let mac_scale = crate::input::scale().max(1);
        let (phys_w, phys_h) = crate::input::output_size();
        let logical_w = (phys_w / mac_scale).max(1);
        let logical_h = (phys_h / mac_scale).max(1);
        let mons = crate::input::monitors();
        let s = mons
            .iter()
            .find(|m| m.primary)
            .map(|m| m.scale)
            .unwrap_or(mac_scale)
            .max(1);
        RAIL_SCALE.store(s, std::sync::atomic::Ordering::Relaxed);
        let out_w = logical_w * s;
        let out_h = logical_h * s;
        let scale = s as u32;

        // Monitor layout, in the physical (×s) desktop space to match. weston
        // ignores the per-monitor scale (global only) but uses the geometry to lay
        // the displays out, which keeps cross-monitor dragging aligned.
        let monitors: Vec<RailMonitor> = mons
            .iter()
            .map(|m| RailMonitor {
                x: m.x * s,
                y: m.y * s,
                width: (m.width.max(1) * s) as u32,
                height: (m.height.max(1) * s) as u32,
                scale: s as u32,
                is_primary: m.primary as i32,
            })
            .collect();
        info!(target: "rail", "advertising {} monitor(s), desktop {out_w}x{out_h} scale={s}", monitors.len());

        // FreeRDP event loop on its own thread (blocking).
        let host_c = CString::new(host).expect("host");
        let app_c = CString::new(app).expect("app");
        std::thread::Builder::new().name("rail-rdp".into()).spawn(move || {
            let cb = RailCallbacks {
                user: ptr::null_mut(),
                window_create: on_window_create,
                window_update: on_window_update,
                window_title: on_window_title,
                window_delete: on_window_delete,
                window_surface: on_window_surface,
                disconnected: on_disconnected,
                window_move_start: on_window_move_start,
                cursor: on_cursor,
                cursor_hidden: on_cursor_hidden,
                cursor_default: on_cursor_default,
                log: on_log,
                window_icon: on_window_icon,
            };
            let rc = unsafe {
                rail_run(
                    host_c.as_ptr(),
                    port,
                    app_c.as_ptr(),
                    out_w as u32,
                    out_h as u32,
                    scale,
                    monitors.as_ptr(),
                    monitors.len() as u32,
                    &cb,
                )
            };
            warn!(target: "rail", "rail_run returned {rc}");
        }).expect("spawn rail-rdp thread");

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
/// FreeRDP isn't linked, so the mode can't run. The back-end is chosen at build
/// time (`main.rs` gates on `cfg!(feature = "rail")`), so this is never reached
/// in a default build — it exists only to satisfy the compiler.
#[cfg(not(feature = "rail"))]
pub fn run(_bus: Arc<InputBus>) {
    log::error!(
        target: "rail",
        "the RAIL back-end requires a build with the `rail` feature (FreeRDP)."
    );
    log::error!(target: "rail", "Rebuild with:  cargo build --features rail   (needs `brew install freerdp`)");
}
