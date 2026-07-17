//! Cross-thread input channel: AppKit (main thread) captures `NSEvent`s and
//! pushes normalized events here; the Wayland thread drains them and forwards
//! `wl_pointer` / `wl_keyboard` events. A self-pipe wakes the Wayland poll loop.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

/// Integer display scale (1 or 2), set once at startup from the main screen's
/// backing scale factor. Shared by the AppKit side (buffer→point conversion) and
/// the Wayland side (`wl_output` scale + `wl_surface.preferred_buffer_scale`).
pub static SCALE: AtomicI32 = AtomicI32::new(1);

pub fn scale() -> i32 {
    SCALE.load(Ordering::Relaxed).max(1)
}

pub fn set_scale(s: i32) {
    SCALE.store(s.max(1), Ordering::Relaxed);
}

/// The virtual output's physical size in pixels, set once at startup from the
/// main screen. Clients won't grow a window past the output's logical size, so
/// this must reflect the real display or resizing is capped well below it.
pub static OUTPUT_W: AtomicI32 = AtomicI32::new(1920);
pub static OUTPUT_H: AtomicI32 = AtomicI32::new(1080);

pub fn output_size() -> (i32, i32) {
    (
        OUTPUT_W.load(Ordering::Relaxed).max(1),
        OUTPUT_H.load(Ordering::Relaxed).max(1),
    )
}

pub fn set_output_size(w: i32, h: i32) {
    OUTPUT_W.store(w.max(1), Ordering::Relaxed);
    OUTPUT_H.store(h.max(1), Ordering::Relaxed);
}

/// Work-area insets (logical points) reserved by docked bars (layer-shell
/// exclusive zones): (top, right, bottom, left). Toplevels avoid these so they
/// don't sit under a bar.
pub static RESERVED_TOP: AtomicI32 = AtomicI32::new(0);
pub static RESERVED_RIGHT: AtomicI32 = AtomicI32::new(0);
pub static RESERVED_BOTTOM: AtomicI32 = AtomicI32::new(0);
pub static RESERVED_LEFT: AtomicI32 = AtomicI32::new(0);

pub fn reserved_insets() -> (i32, i32, i32, i32) {
    (
        RESERVED_TOP.load(Ordering::Relaxed),
        RESERVED_RIGHT.load(Ordering::Relaxed),
        RESERVED_BOTTOM.load(Ordering::Relaxed),
        RESERVED_LEFT.load(Ordering::Relaxed),
    )
}

pub fn set_reserved_insets(top: i32, right: i32, bottom: i32, left: i32) {
    RESERVED_TOP.store(top.max(0), Ordering::Relaxed);
    RESERVED_RIGHT.store(right.max(0), Ordering::Relaxed);
    RESERVED_BOTTOM.store(bottom.max(0), Ordering::Relaxed);
    RESERVED_LEFT.store(left.max(0), Ordering::Relaxed);
}

/// Input events, already translated to Wayland conventions:
/// coordinates are surface-local top-left pixels, keycodes are evdev, and
/// modifier masks are xkb masks.
pub enum InputEvent {
    PointerEnter { window_id: u32, x: f64, y: f64 },
    PointerMotion { window_id: u32, x: f64, y: f64 },
    PointerButton { window_id: u32, button: u32, pressed: bool },
    PointerAxis { dx: f64, dy: f64 },
    PointerLeave { window_id: u32 },
    Key { keycode: u32, pressed: bool },
    Modifiers { depressed: u32, locked: u32 },
    /// Keyboard focus gained/lost for a window.
    Focus { window_id: u32, focused: bool },
    /// The user resized the NSWindow; ask the client to repaint at this size
    /// (logical/surface units).
    Resize { window_id: u32, width: i32, height: i32 },
    /// A click landed outside a grabbing popup; dismiss it (`xdg_popup.popup_done`).
    PopupDismiss { window_id: u32 },
    /// The macOS pasteboard changed (or was read for the first time). Carries the
    /// current plain-text contents, if any. Consumed by the clipboard bridge,
    /// which re-advertises it to Wayland clients as a selection. `None` means the
    /// pasteboard holds no text we can offer.
    MacClipboard { text: Option<String> },
}

pub struct InputBus {
    queue: Mutex<VecDeque<InputEvent>>,
    waker: Mutex<Option<OwnedFd>>,
}

impl InputBus {
    pub fn new() -> Self {
        InputBus {
            queue: Mutex::new(VecDeque::new()),
            waker: Mutex::new(None),
        }
    }

    /// The Wayland thread registers the write end of its wakeup pipe here.
    pub fn set_waker(&self, fd: OwnedFd) {
        *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(fd);
    }

    /// Called from the AppKit main thread.
    pub fn push(&self, ev: InputEvent) {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(ev);
        if let Some(fd) = self.waker.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = rustix::io::write(fd, &[1u8]);
        }
    }

    /// Called from the Wayland thread after the pipe wakes it.
    pub fn drain(&self) -> Vec<InputEvent> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()
    }
}
