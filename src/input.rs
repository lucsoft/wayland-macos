//! Cross-thread input channel: AppKit (main thread) captures `NSEvent`s and
//! pushes normalized events here; the Wayland thread drains them and forwards
//! `wl_pointer` / `wl_keyboard` events. A self-pipe wakes the Wayland poll loop.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
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
        *self.waker.lock().unwrap() = Some(fd);
    }

    /// Called from the AppKit main thread.
    pub fn push(&self, ev: InputEvent) {
        self.queue.lock().unwrap().push_back(ev);
        if let Some(fd) = self.waker.lock().unwrap().as_ref() {
            let byte = [1u8];
            unsafe {
                libc::write(fd.as_raw_fd(), byte.as_ptr() as *const libc::c_void, 1);
            }
        }
    }

    /// Called from the Wayland thread after the pipe wakes it.
    pub fn drain(&self) -> Vec<InputEvent> {
        self.queue.lock().unwrap().drain(..).collect()
    }
}
