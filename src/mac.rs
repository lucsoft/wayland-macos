//! macOS / AppKit side of the compositor.
//!
//! All AppKit calls must happen on the main thread. The Wayland compositor runs
//! on a background thread and marshals work here via `post()`, which enqueues a
//! closure on the main GCD queue (i.e. the AppKit run loop). A main-thread-only
//! `thread_local` registry maps Wayland toplevel ids to their `NSWindow`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::{copy_nonoverlapping, null_mut};
use std::sync::Arc;

use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSEvent, NSEventModifierFlags, NSPasteboard,
    NSPasteboardTypeString, NSPopUpMenuWindowLevel, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSWindow, NSWindowDelegate, NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetData, CGColorSpace,
    CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString};

use crate::input::{InputBus, InputEvent};

// --- Input bus (set once on the main thread) -------------------------------

thread_local! {
    static INPUT_BUS: RefCell<Option<Arc<InputBus>>> = const { RefCell::new(None) };
}

/// Called once from `main()` on the main thread.
pub fn set_input_bus(bus: Arc<InputBus>) {
    INPUT_BUS.with(|b| *b.borrow_mut() = Some(bus));
}

fn push(ev: InputEvent) {
    INPUT_BUS.with(|b| {
        if let Some(bus) = b.borrow().as_ref() {
            bus.push(ev);
        }
    });
}

// --- Clipboard bridge (NSPasteboard <-> the clipboard bridge) ---------------
//
// The clipboard bridge (see `bridges::clipboard`) owns the Wayland side. Here we
// only touch `NSPasteboard`, always on the main thread:
//
//  * `set_clipboard` writes text the bridge pulled from a Wayland client.
//  * `start_clipboard_watch` polls `changeCount` and, when the pasteboard
//    changes, pushes the new text onto the input bus so the bridge can offer it
//    to Wayland clients.
//
// Both run on the main thread, so pasteboard access is serialized. The polling
// interval — a compromise between latency and idle wakeups.
const CLIPBOARD_POLL: Duration = Duration::from_millis(400);

thread_local! {
    /// `(last_seen_change, change_we_wrote)`. Main-thread only.
    ///
    /// `last_seen_change` starts at -1 so the first tick emits whatever is
    /// already on the pasteboard. `change_we_wrote` records the `changeCount`
    /// produced by our own `set_clipboard`, so we don't bounce a Wayland client's
    /// own copy straight back to it as a fresh selection.
    static PB_STATE: Cell<(isize, isize)> = const { Cell::new((-1, -1)) };
}

/// Write `text` to the macOS pasteboard (marshalled to the main thread).
/// Called by the clipboard bridge when a Wayland client copies.
pub fn set_clipboard(text: String) {
    DispatchQueue::main().exec_async(move || {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let ok = pb.setString_forType(&NSString::from_str(&text), unsafe { NSPasteboardTypeString });
        if !ok {
            eprintln!("[mac] clipboard: setString failed");
        }
        // Mark this change as self-authored so the watcher won't re-emit it.
        let c = pb.changeCount();
        PB_STATE.with(|s| s.set((c, c)));
    });
}

/// Start watching the macOS pasteboard for changes. Call once from `main()`.
pub fn start_clipboard_watch(bus: Arc<InputBus>) {
    schedule_clipboard_tick(bus);
}

fn schedule_clipboard_tick(bus: Arc<InputBus>) {
    let Ok(when) = DispatchTime::try_from(CLIPBOARD_POLL) else {
        return;
    };
    let _ = DispatchQueue::main().after(when, move || {
        clipboard_tick(&bus);
        schedule_clipboard_tick(bus);
    });
}

fn clipboard_tick(bus: &InputBus) {
    let pb = NSPasteboard::generalPasteboard();
    let count = pb.changeCount();
    let (last_seen, self_wrote) = PB_STATE.with(|s| s.get());
    if count == last_seen {
        return;
    }
    PB_STATE.with(|s| s.set((count, self_wrote)));
    if count == self_wrote {
        // This is the change our own `set_clipboard` made; the source client
        // already owns it, so don't offer it back.
        return;
    }
    let text = pb
        .stringForType(unsafe { NSPasteboardTypeString })
        .map(|s| s.to_string());
    bus.push(InputEvent::MacClipboard { text });
}

// --- WaylandView: a layer-backed NSView that forwards input -----------------

struct ViewIvars {
    window_id: Cell<u32>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "WaylandView"]
    #[ivars = ViewIvars]
    struct WaylandView;

    impl WaylandView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // Deliver the click that activates the window to the view too.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _ev: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            let mtm = MainThreadMarker::from(self);
            for area in self.trackingAreas().iter() {
                self.removeTrackingArea(&area);
            }
            let opts = NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect;
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(
                    mtm.alloc(),
                    self.bounds(),
                    opts,
                    Some(self),
                    None,
                )
            };
            self.addTrackingArea(&area);
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, ev: &NSEvent) {
            self.handle_motion(ev);
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, ev: &NSEvent) {
            // While an interactive move is active, drag the window instead of
            // forwarding motion to the client.
            if self.try_drag_window() {
                return;
            }
            self.handle_motion(ev);
        }
        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, ev: &NSEvent) {
            self.handle_motion(ev);
        }
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, ev: &NSEvent) {
            // While a popup grabs the pointer, ignore native enter/leave for other
            // windows — the pointer logically stays on the popup.
            if grabbed().is_some() {
                return;
            }
            let (x, y) = self.local(ev);
            let id = self.ivars().window_id.get();
            push(InputEvent::PointerEnter { window_id: id, x, y });
            push(InputEvent::Focus { window_id: id, focused: true });
        }
        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _ev: &NSEvent) {
            if grabbed().is_some() {
                return;
            }
            let id = self.ivars().window_id.get();
            push(InputEvent::PointerLeave { window_id: id });
            push(InputEvent::Focus { window_id: id, focused: false });
        }
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, ev: &NSEvent) {
            self.handle_button(ev, true);
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, ev: &NSEvent) {
            // End any interactive window move.
            DRAG.with(|d| *d.borrow_mut() = None);
            self.handle_button(ev, false);
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, ev: &NSEvent) {
            self.handle_button(ev, true);
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, ev: &NSEvent) {
            self.handle_button(ev, false);
        }
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, ev: &NSEvent) {
            self.handle_button(ev, true);
        }
        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, ev: &NSEvent) {
            self.handle_button(ev, false);
        }
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, ev: &NSEvent) {
            let dx = ev.scrollingDeltaX();
            let dy = ev.scrollingDeltaY();
            // macOS scroll deltas are inverted relative to Wayland axis sign.
            push(InputEvent::PointerAxis { dx: -dx, dy: -dy });
        }
        #[unsafe(method(keyDown:))]
        fn key_down(&self, ev: &NSEvent) {
            self.key(ev, true);
        }
        #[unsafe(method(keyUp:))]
        fn key_up(&self, ev: &NSEvent) {
            self.key(ev, false);
        }
        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, ev: &NSEvent) {
            let flags = ev.modifierFlags();
            let (depressed, locked) = modifier_masks(flags);
            push(InputEvent::Modifiers { depressed, locked });
        }
    }
);

impl WaylandView {
    fn new(mtm: MainThreadMarker, window_id: u32, frame: CGRect) -> Retained<Self> {
        let this = mtm.alloc::<WaylandView>().set_ivars(ViewIvars {
            window_id: Cell::new(window_id),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Event location in surface-local, top-left, pixel coordinates.
    fn local(&self, ev: &NSEvent) -> (f64, f64) {
        let win_pt = ev.locationInWindow();
        let p = self.convertPoint_fromView(win_pt, None);
        let h = self.bounds().size.height;
        (p.x, h - p.y)
    }

    fn motion(&self, ev: &NSEvent) {
        let (x, y) = self.local(ev);
        push(InputEvent::PointerMotion {
            window_id: self.ivars().window_id.get(),
            x,
            y,
        });
    }

    /// If an interactive move is active for this window, move it to follow the
    /// global mouse and return true (so motion isn't forwarded to the client).
    fn try_drag_window(&self) -> bool {
        let id = self.ivars().window_id.get();
        DRAG.with(|d| {
            let borrow = d.borrow();
            let Some(state) = borrow.as_ref() else {
                return false;
            };
            if state.window_id != id {
                return false;
            }
            let mouse = NSEvent::mouseLocation();
            let origin = CGPoint::new(
                state.anchor_origin.x + (mouse.x - state.anchor_mouse.x),
                state.anchor_origin.y + (mouse.y - state.anchor_mouse.y),
            );
            if let Some(window) = self.window() {
                window.setFrameOrigin(origin);
            }
            true
        })
    }

    fn button(&self, ev: &NSEvent, pressed: bool) {
        push(InputEvent::PointerButton {
            window_id: self.ivars().window_id.get(),
            button: btn_code(ev.buttonNumber()),
            pressed,
        });
    }

    /// Motion, honoring an active popup grab (route everything to the popup).
    fn handle_motion(&self, ev: &NSEvent) {
        if let Some(gid) = grabbed() {
            if let Some((x, y, _)) = global_in_window(gid) {
                push(InputEvent::PointerMotion {
                    window_id: gid,
                    x,
                    y,
                });
            }
            return;
        }
        self.motion(ev);
    }

    /// Button, honoring an active popup grab: clicks inside the popup go to it;
    /// a click outside dismisses the menu.
    fn handle_button(&self, ev: &NSEvent, pressed: bool) {
        if let Some(gid) = grabbed() {
            if let Some((x, y, inside)) = global_in_window(gid) {
                if inside {
                    if pressed {
                        // Ensure the popup has the pointer at the click location.
                        push(InputEvent::PointerMotion {
                            window_id: gid,
                            x,
                            y,
                        });
                    }
                    push(InputEvent::PointerButton {
                        window_id: gid,
                        button: btn_code(ev.buttonNumber()),
                        pressed,
                    });
                } else if pressed {
                    push(InputEvent::PopupDismiss { window_id: gid });
                }
            }
            return;
        }
        self.button(ev, pressed);
    }

    fn key(&self, ev: &NSEvent, pressed: bool) {
        let kc = ev.keyCode();
        if let Some(evdev) = macos_to_evdev(kc) {
            push(InputEvent::Key {
                keycode: evdev,
                pressed,
            });
        }
    }
}

/// Translate NSEvent modifier flags to xkb modifier masks (default us keymap).
fn modifier_masks(f: NSEventModifierFlags) -> (u32, u32) {
    let mut depressed = 0u32;
    let mut locked = 0u32;
    if f.contains(NSEventModifierFlags::Shift) {
        depressed |= 1; // Shift
    }
    if f.contains(NSEventModifierFlags::Control) {
        depressed |= 4; // Control
    }
    if f.contains(NSEventModifierFlags::Option) {
        depressed |= 8; // Mod1 / Alt
    }
    if f.contains(NSEventModifierFlags::Command) {
        depressed |= 64; // Mod4 / Super
    }
    if f.contains(NSEventModifierFlags::CapsLock) {
        locked |= 2; // Lock
    }
    (depressed, locked)
}

/// macOS virtual keycode -> Linux evdev keycode (common keys).
fn macos_to_evdev(kc: u16) -> Option<u32> {
    Some(match kc {
        0x00 => 30, 0x01 => 31, 0x02 => 32, 0x03 => 33, 0x04 => 35, 0x05 => 34, // A S D F H G
        0x06 => 44, 0x07 => 45, 0x08 => 46, 0x09 => 47, 0x0B => 48, // Z X C V B
        0x0C => 16, 0x0D => 17, 0x0E => 18, 0x0F => 19, 0x10 => 21, 0x11 => 20, // Q W E R Y T
        0x12 => 2, 0x13 => 3, 0x14 => 4, 0x15 => 5, 0x16 => 7, 0x17 => 6, // 1 2 3 4 6 5
        0x18 => 13, 0x19 => 10, 0x1A => 8, 0x1B => 12, 0x1C => 9, 0x1D => 11, // = 9 7 - 8 0
        0x1E => 27, 0x1F => 24, 0x20 => 22, 0x21 => 26, 0x22 => 23, 0x23 => 25, // ] O U [ I P
        0x24 => 28, 0x25 => 38, 0x26 => 36, 0x27 => 40, 0x28 => 37, 0x29 => 39, // Ret L J ' K ;
        0x2A => 43, 0x2B => 51, 0x2C => 53, 0x2D => 49, 0x2E => 50, 0x2F => 52, // \ , / N M .
        0x30 => 15, 0x31 => 57, 0x32 => 41, 0x33 => 14, 0x35 => 1, // Tab Space ` Backspace Esc
        0x37 => 125, 0x38 => 42, 0x39 => 58, 0x3A => 56, 0x3B => 29, // Cmd Shift Caps Opt Ctrl
        0x3C => 54, 0x3D => 100, 0x3E => 97, 0x36 => 126, // RShift ROpt RCtrl RCmd
        0x7B => 105, 0x7C => 106, 0x7D => 108, 0x7E => 103, // Left Right Down Up
        _ => return None,
    })
}

// --- WaylandWindow: borderless window that can still become key -------------
//
// GTK draws its own client-side decorations (title bar, rounded corners, shadow),
// so we want no native chrome at all. A plain borderless NSWindow can't become
// the key window (so it wouldn't get keyboard focus); this subclass overrides
// that.

define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "WaylandWindow"]
    struct WaylandWindow;

    impl WaylandWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            true
        }
    }
);

impl WaylandWindow {
    fn new(mtm: MainThreadMarker, rect: CGRect, style: NSWindowStyleMask) -> Retained<NSWindow> {
        let this = mtm.alloc::<WaylandWindow>();
        // WaylandWindow doesn't override init, so send the inherited designated
        // initializer straight to the allocated instance.
        let window: Retained<WaylandWindow> = unsafe {
            msg_send![
                this,
                initWithContentRect: rect,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        window.into_super()
    }
}

// --- Window delegate: forward NSWindow resizes back to the client -----------

struct DelegateIvars {
    window_id: u32,
    last: Cell<(i32, i32)>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WaylandWindowDelegate"]
    #[ivars = DelegateIvars]
    struct WinDelegate;

    unsafe impl NSObjectProtocol for WinDelegate {}

    unsafe impl NSWindowDelegate for WinDelegate {
        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, notif: &NSNotification) {
            let Some(obj) = notif.object() else {
                return;
            };
            let Ok(window) = obj.downcast::<NSWindow>() else {
                return;
            };
            let Some(view) = window.contentView() else {
                return;
            };
            let size = view.frame().size;
            let (w, h) = (size.width as i32, size.height as i32);
            // Skip echoes of sizes we already asked the client to paint.
            if self.ivars().last.get() == (w, h) {
                return;
            }
            self.ivars().last.set((w, h));
            push(InputEvent::Resize {
                window_id: self.ivars().window_id,
                width: w,
                height: h,
            });
        }
    }
);

impl WinDelegate {
    fn new(mtm: MainThreadMarker, window_id: u32, w: i32, h: i32) -> Retained<Self> {
        let this = mtm.alloc::<WinDelegate>().set_ivars(DelegateIvars {
            window_id,
            last: Cell::new((w, h)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// A command sent from the Wayland thread to the AppKit main thread.
pub enum WinCmd {
    /// Create a native window for a Wayland toplevel.
    Create {
        id: u32,
        width: i32,
        height: i32,
        title: String,
    },
    /// Present a new frame (a committed shm buffer) into a window.
    Frame {
        id: u32,
        width: i32,
        height: i32,
        stride: i32,
        /// Raw pixels, 32-bit, byte order B,G,R,X (Wayland XRGB8888/ARGB8888 LE).
        pixels: Vec<u8>,
    },
    /// Update a window title.
    Title { id: u32, title: String },
    /// Begin an interactive drag of the window (from `xdg_toplevel.move`).
    StartMove { id: u32 },
    /// Create a borderless popup window (menu/dropdown) positioned relative to
    /// its parent (logical/point coordinates, top-left of the parent surface).
    CreatePopup {
        id: u32,
        parent_id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// Set (or clear) the popup that currently grabs the pointer.
    SetGrab { window: Option<u32> },
    /// Destroy a window.
    Destroy { id: u32 },
}

struct WinEntry {
    window: Retained<NSWindow>,
    view: Retained<WaylandView>,
    // Kept alive because NSWindow holds only a weak delegate reference.
    // Popups have no delegate (they don't resize).
    _delegate: Option<Retained<WinDelegate>>,
    cur_w: i32,
    cur_h: i32,
}

struct DragState {
    window_id: u32,
    anchor_mouse: CGPoint,
    anchor_origin: CGPoint,
}

thread_local! {
    static WINDOWS: RefCell<HashMap<u32, WinEntry>> = RefCell::new(HashMap::new());
    // Active interactive window move (from xdg_toplevel.move). The move request
    // round-trips through the container, so we can't use the live NSEvent — we
    // anchor here and follow the global mouse ourselves.
    static DRAG: RefCell<Option<DragState>> = const { RefCell::new(None) };
    // Window id of a popup that has grabbed the pointer (from xdg_popup.grab).
    // While set, all pointer input routes to that popup (menus), and a click
    // outside it dismisses the menu.
    static GRAB: Cell<Option<u32>> = const { Cell::new(None) };
}

fn grabbed() -> Option<u32> {
    GRAB.with(|g| g.get())
}

/// macOS NSEvent button number -> evdev button code.
fn btn_code(n: isize) -> u32 {
    // BTN_LEFT=0x110, BTN_RIGHT=0x111, BTN_MIDDLE=0x112
    match n {
        0 => 0x110,
        1 => 0x111,
        2 => 0x112,
        other => 0x110 + other as u32,
    }
}

/// Compute the pointer position (in the target window's top-left view points)
/// from the current global mouse, and whether the mouse is inside that window.
fn global_in_window(window_id: u32) -> Option<(f64, f64, bool)> {
    WINDOWS.with(|w| {
        w.borrow().get(&window_id).map(|e| {
            let f = e.window.frame();
            let gm = NSEvent::mouseLocation();
            let vx = gm.x - f.origin.x;
            let vy = gm.y - f.origin.y; // from bottom
            let inside = vx >= 0.0 && vy >= 0.0 && vx <= f.size.width && vy <= f.size.height;
            (vx, f.size.height - vy, inside) // top-left coords
        })
    })
}

/// After a popup grab ends, the pointer/keyboard were logically on the popup and
/// the parent's native enter was suppressed during the grab — so hand focus to
/// whatever window is now under the cursor (usually the parent), or the client
/// stays unresponsive until the window is re-activated.
fn restore_focus_under_cursor() {
    let target = WINDOWS.with(|w| {
        let gm = NSEvent::mouseLocation();
        w.borrow().iter().find_map(|(id, e)| {
            let f = e.window.frame();
            let inside = gm.x >= f.origin.x
                && gm.x <= f.origin.x + f.size.width
                && gm.y >= f.origin.y
                && gm.y <= f.origin.y + f.size.height;
            inside.then(|| (*id, gm.x - f.origin.x, f.size.height - (gm.y - f.origin.y)))
        })
    });
    if let Some((id, x, y)) = target {
        push(InputEvent::PointerEnter { window_id: id, x, y });
        push(InputEvent::Focus {
            window_id: id,
            focused: true,
        });
    }
}

/// Enqueue a window command onto the AppKit main thread.
pub fn post(cmd: WinCmd) {
    DispatchQueue::main().exec_async(move || handle(cmd));
}

fn handle(cmd: WinCmd) {
    // Safe: exec_async on the main queue always runs on the main thread.
    let mtm = MainThreadMarker::new().expect("must run on main thread");
    match cmd {
        WinCmd::Create {
            id,
            width,
            height,
            title,
        } => create_window(mtm, id, width.max(1), height.max(1), &title),
        WinCmd::Frame {
            id,
            width,
            height,
            stride,
            pixels,
        } => present_frame(mtm, id, width, height, stride, &pixels),
        WinCmd::Title { id, title } => {
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    e.window.setTitle(&NSString::from_str(&title));
                }
            });
        }
        WinCmd::StartMove { id } => {
            // Anchor the drag at the current window origin + global mouse; the
            // view's mouseDragged handler moves the window to follow.
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    let anchor_origin = e.window.frame().origin;
                    let anchor_mouse = NSEvent::mouseLocation();
                    DRAG.with(|d| {
                        *d.borrow_mut() = Some(DragState {
                            window_id: id,
                            anchor_mouse,
                            anchor_origin,
                        });
                    });
                }
            });
        }
        WinCmd::CreatePopup {
            id,
            parent_id,
            x,
            y,
            width,
            height,
        } => create_popup(mtm, id, parent_id, x, y, width.max(1), height.max(1)),
        WinCmd::SetGrab { window } => {
            let was = GRAB.with(|g| g.replace(window));
            // Grab just ended: restore focus to the window under the cursor.
            if window.is_none() && was.is_some() {
                restore_focus_under_cursor();
            }
        }
        WinCmd::Destroy { id } => {
            // If the grabbing popup is going away, clear the grab.
            let grab_ended = GRAB.with(|g| {
                if g.get() == Some(id) {
                    g.set(None);
                    true
                } else {
                    false
                }
            });
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow_mut().remove(&id) {
                    e.window.close();
                }
            });
            if grab_ended {
                restore_focus_under_cursor();
            }
        }
    }
}

fn create_window(mtm: MainThreadMarker, id: u32, width: i32, height: i32, title: &str) {
    // The buffer is in physical pixels; the window/view are in logical points.
    let scale = crate::input::scale();
    let lw = (width / scale).max(1) as f64;
    let lh = (height / scale).max(1) as f64;
    let rect = CGRect::new(CGPoint::new(120.0, 120.0), CGSize::new(lw, lh));
    // Fully borderless: GTK draws its own title bar, rounded corners and shadow,
    // so any native chrome (even a hidden titlebar) would show its own rounded
    // frame in GTK's transparent margins. `Resizable` still allows edge resizing.
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::Resizable;
    let window = WaylandWindow::new(mtm, rect, style);
    // We keep the NSWindow alive via a `Retained` in WINDOWS; `close()` must not
    // also release it, or we'd over-release and crash.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(title));
    // No native shadow — GTK's CSD already draws one into the buffer margins.
    window.setHasShadow(false);

    let view = WaylandView::new(
        mtm,
        id,
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(lw, lh)),
    );
    view.setWantsLayer(true);
    // Toolkit apps (GTK) use client-side decorations with translucent shadow
    // margins, so present the window fully transparent behind the buffer —
    // otherwise the system window background shows through those margins.
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    if let Some(layer) = view.layer() {
        layer.setOpaque(false);
        // The layer contents are a physical-pixel image; map to logical points.
        layer.setContentsScale(scale as f64);
        let cg = NSColor::clearColor().CGColor();
        layer.setBackgroundColor(Some(&cg));
    }

    window.setAcceptsMouseMovedEvents(true);
    window.setContentView(Some(&view));

    // Forward native (edge) resizes back to the client (logical points).
    let delegate = WinDelegate::new(mtm, id, lw as i32, lh as i32);
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    window.center();
    window.makeKeyAndOrderFront(None);
    window.makeFirstResponder(Some(&view));

    // We're an unbundled binary; without activating, macOS routes mouseDown/
    // keyDown to whatever app is frontmost, so clicks/keys never reach the view.
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    WINDOWS.with(|w| {
        w.borrow_mut().insert(
            id,
            WinEntry {
                window,
                view,
                _delegate: Some(delegate),
                cur_w: width,
                cur_h: height,
            },
        );
    });
    eprintln!("[mac] created NSWindow for toplevel {id} ({width}x{height})");
}

/// Create a borderless popup window (menu/dropdown) as a child of its parent,
/// positioned at `(x, y)` logical points from the parent surface's top-left.
fn create_popup(
    mtm: MainThreadMarker,
    id: u32,
    parent_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) {
    let scale = crate::input::scale();
    let lw = (width / scale).max(1) as f64;
    let lh = (height / scale).max(1) as f64;

    // Screen origin from the parent (macOS frames are bottom-left origin).
    let origin = WINDOWS
        .with(|w| {
            w.borrow().get(&parent_id).map(|p| {
                let f = p.window.frame();
                CGPoint::new(
                    f.origin.x + x as f64,
                    (f.origin.y + f.size.height) - y as f64 - lh,
                )
            })
        })
        .unwrap_or_else(|| CGPoint::new(200.0, 200.0));

    // Use the WaylandWindow subclass so the popup can become key (menu keyboard
    // navigation) and receive mouse input consistently.
    let window = WaylandWindow::new(
        mtm,
        CGRect::new(origin, CGSize::new(lw, lh)),
        NSWindowStyleMask::Borderless,
    );
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(false);
    window.setAcceptsMouseMovedEvents(true);
    window.setLevel(NSPopUpMenuWindowLevel);

    let view = WaylandView::new(mtm, id, CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(lw, lh)));
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setOpaque(false);
        layer.setContentsScale(scale as f64);
        layer.setBackgroundColor(Some(&NSColor::clearColor().CGColor()));
    }
    window.setContentView(Some(&view));

    // Attach to the parent so it floats above and follows it.
    WINDOWS.with(|w| {
        if let Some(p) = w.borrow().get(&parent_id) {
            unsafe {
                p.window
                    .addChildWindow_ordered(&window, NSWindowOrderingMode::Above);
            }
        }
    });
    window.orderFront(None);

    WINDOWS.with(|w| {
        w.borrow_mut().insert(
            id,
            WinEntry {
                window,
                view,
                _delegate: None,
                cur_w: width,
                cur_h: height,
            },
        );
    });

    // Menus usually open under the cursor. NSTrackingArea only emits mouseEntered
    // when the mouse *crosses* the boundary, so a popup appearing beneath a
    // stationary cursor gets no enter until the user moves out and back in.
    // Synthesize the enter when the cursor is inside — OR when this popup holds
    // the grab (its pointer is routed here regardless of where the cursor is; the
    // grab suppresses the normal mouseEntered, so without this a menu opening away
    // from the cursor would never receive a pointer enter and stay unresponsive).
    let gm = NSEvent::mouseLocation();
    let (mx, my) = (gm.x - origin.x, gm.y - origin.y);
    let inside = mx >= 0.0 && my >= 0.0 && mx <= lw && my <= lh;
    let is_grab = GRAB.with(|g| g.get()) == Some(id);
    if inside || is_grab {
        push(InputEvent::PointerEnter {
            window_id: id,
            x: mx,
            y: lh - my,
        });
        push(InputEvent::Focus {
            window_id: id,
            focused: true,
        });
    }
    eprintln!("[mac] created popup {id} under {parent_id} at ({x},{y}) {width}x{height}");
}

fn present_frame(
    mtm: MainThreadMarker,
    id: u32,
    width: i32,
    height: i32,
    stride: i32,
    pixels: &[u8],
) {
    let Some(image) = make_cgimage(width, height, stride, pixels) else {
        eprintln!("[mac] failed to build CGImage for {id}");
        return;
    };

    WINDOWS.with(|w| {
        let mut map = w.borrow_mut();
        let Some(entry) = map.get_mut(&id) else { return };

        // Resize the window whenever the buffer dimensions change. The buffer is
        // physical pixels; the window content size is logical points.
        if entry.cur_w != width || entry.cur_h != height {
            let scale = crate::input::scale();
            let lw = (width / scale).max(1) as f64;
            let lh = (height / scale).max(1) as f64;
            entry.window.setContentSize(CGSize::new(lw, lh));
            entry.cur_w = width;
            entry.cur_h = height;
        }
        let _ = mtm;

        if let Some(layer) = entry.view.layer() {
            // A CGImageRef is a CFType that responds to retain/release, so it can
            // be handed to `-[CALayer setContents:]` (typed as `id`).
            let img_ptr: *const CGImage = &*image;
            let obj: &AnyObject = unsafe { &*(img_ptr as *const AnyObject) };
            unsafe { layer.setContents(Some(obj)) };
        }
    });
}

/// Build a CGImage from a raw 32-bit little-endian buffer (row 0 = top).
fn make_cgimage(
    width: i32,
    height: i32,
    stride: i32,
    pixels: &[u8],
) -> Option<objc2_core_foundation::CFRetained<CGImage>> {
    if width <= 0 || height <= 0 || stride < width * 4 {
        return None;
    }
    let needed = (stride as usize) * (height as usize);
    if pixels.len() < needed {
        return None;
    }

    let color_space = CGColorSpace::new_device_rgb()?;
    // Wayland ARGB8888 is premultiplied; XRGB8888 has alpha bytes = 0xFF so it
    // reads as fully opaque under the same interpretation. 32-bit little-endian
    // means B,G,R,A byte order in memory.
    let bitmap_info =
        CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;

    let ctx = unsafe {
        CGBitmapContextCreate(
            null_mut(),
            width as usize,
            height as usize,
            8,
            stride as usize,
            Some(&color_space),
            bitmap_info,
        )
    }?;

    let dst = CGBitmapContextGetData(Some(&ctx));
    if dst.is_null() {
        return None;
    }
    unsafe {
        copy_nonoverlapping(pixels.as_ptr(), dst as *mut u8, needed);
    }

    CGBitmapContextCreateImage(Some(&ctx))
}
