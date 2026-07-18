//! macOS / AppKit side of the compositor.
//!
//! All AppKit calls must happen on the main thread. The Wayland compositor runs
//! on a background thread and marshals work here via `post()`, which enqueues a
//! closure on the main GCD queue (i.e. the AppKit run loop). A main-thread-only
//! `thread_local` registry maps Wayland toplevel ids to their `NSWindow`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ptr::{copy_nonoverlapping, null, null_mut};
use std::sync::Arc;

use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use log::{debug, error, info};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSCursor, NSEvent, NSEventModifierFlags, NSImage,
    NSModalPanelWindowLevel, NSNormalWindowLevel, NSPasteboard, NSPasteboardTypeString,
    NSPopUpMenuWindowLevel, NSScreen, NSStatusWindowLevel, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSWindow, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowOrderingMode,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
use objc2_quartz_core::{CALayer, CATransaction};
use objc2_core_graphics::{
    kCGColorSpaceDisplayP3, kCGColorSpaceExtendedLinearDisplayP3,
    kCGColorSpaceExtendedLinearITUR_2020, kCGColorSpaceITUR_2100_HLG, kCGColorSpaceITUR_2100_PQ,
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetData, CGBitmapInfo,
    CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo,
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

/// The current macOS keyboard layout as an xkb layout name (e.g. "de"), so the
/// compositor's keymap can follow the Mac's keyboard automatically. Uses the
/// Carbon Text Input Source API. Returns None for layouts we don't map.
pub fn macos_keyboard_layout() -> Option<String> {
    use std::ffi::CStr;
    use std::os::raw::{c_char, c_void};

    // Both frameworks genuinely need `kind = "framework"`; clippy's
    // duplicated_attributes lint misreads the repeated key as redundant.
    #[allow(clippy::duplicated_attributes)]
    #[link(name = "Carbon", kind = "framework")]
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
        fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *const c_void;
        static kTISPropertyInputSourceID: *const c_void;
        fn CFRelease(cf: *const c_void);
        fn CFStringGetCStringPtr(s: *const c_void, encoding: u32) -> *const c_char;
        fn CFStringGetCString(s: *const c_void, buf: *mut c_char, size: isize, encoding: u32) -> bool;
    }
    const KCFSTRING_ENCODING_UTF8: u32 = 0x0800_0100;

    // e.g. "com.apple.keylayout.German"
    let id = unsafe {
        let src = TISCopyCurrentKeyboardLayoutInputSource();
        if src.is_null() {
            return None;
        }
        let prop = TISGetInputSourceProperty(src, kTISPropertyInputSourceID);
        if prop.is_null() {
            CFRelease(src);
            return None;
        }
        let s = {
            let ptr = CFStringGetCStringPtr(prop, KCFSTRING_ENCODING_UTF8);
            if !ptr.is_null() {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            } else {
                let mut buf = [0 as c_char; 256];
                if CFStringGetCString(prop, buf.as_mut_ptr(), buf.len() as isize, KCFSTRING_ENCODING_UTF8) {
                    CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
                } else {
                    String::new()
                }
            }
        };
        CFRelease(src);
        s
    };

    let name = id.rsplit('.').next().unwrap_or("");
    let xkb = match name {
        n if n.starts_with("German") => "de",
        n if n.starts_with("US") || n == "ABC" || n.starts_with("USInternational") => "us",
        n if n.starts_with("British") => "gb",
        n if n.starts_with("French") => "fr",
        n if n.starts_with("Spanish") => "es",
        n if n.starts_with("Italian") => "it",
        n if n.starts_with("Swiss") => "ch",
        n if n.starts_with("Dutch") => "nl",
        n if n.starts_with("Portuguese") => "pt",
        n if n.starts_with("Danish") => "dk",
        n if n.starts_with("Norwegian") => "no",
        n if n.starts_with("Swedish") => "se",
        _ => return None,
    };
    Some(xkb.to_string())
}

// --- Clipboard (NSPasteboard <-> the Wayland selection) ---------------------
//
// The clipboard integration (see `wayland::clipboard`) owns the Wayland side.
// Here we only touch `NSPasteboard`, always on the main thread:
//
//  * `set_clipboard` writes text pulled from a Wayland client.
//  * `start_clipboard_watch` polls `changeCount` and, when the pasteboard
//    changes, pushes the new text onto the input bus so the Wayland side can
//    offer it to clients.
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
/// Called by the clipboard integration when a Wayland client copies.
pub fn set_clipboard(text: String) {
    DispatchQueue::main().exec_async(move || {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let ok = pb.setString_forType(&NSString::from_str(&text), unsafe { NSPasteboardTypeString });
        if !ok {
            error!(target: "mac", "clipboard: setString failed");
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

        // Apply the client-requested cursor (from wp_cursor_shape) over the view.
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            if let Some(cursor) = CURSOR.with(|c| c.borrow().clone()) {
                self.addCursorRect_cursor(self.bounds(), &cursor);
            }
        }

        // Keep the hosted root layer filling the view the instant the view resizes
        // (e.g. during a native live resize), independent of buffer commits — so
        // content tracks the window smoothly instead of jittering between frames.
        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, new_size: CGSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: new_size] };
            if let Some(layer) = self.layer() {
                without_animations(|| {
                    layer.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), new_size));
                });
            }
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
            // Interactive resize / move take precedence over forwarding motion.
            if self.try_resize_window() {
                return;
            }
            if self.try_drag_window(ev) {
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
            // Pointer enter only — keyboard focus is click-to-focus (see
            // windowDidBecomeKey), it does NOT follow the pointer.
            push(InputEvent::PointerEnter { window_id: id, x, y });
        }
        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _ev: &NSEvent) {
            if grabbed().is_some() {
                return;
            }
            let id = self.ivars().window_id.get();
            push(InputEvent::PointerLeave { window_id: id });
        }
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, ev: &NSEvent) {
            self.handle_button(ev, true);
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, ev: &NSEvent) {
            // End any interactive window move / resize.
            DRAG.with(|d| *d.borrow_mut() = None);
            RESIZE.with(|r| *r.borrow_mut() = None);
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

    /// If a move is pending for this window, hand off to the native window drag
    /// (edge-snapping + drag-to-top maximize come for free), returning true so
    /// motion isn't forwarded to the client.
    fn try_drag_window(&self, ev: &NSEvent) -> bool {
        let id = self.ivars().window_id.get();
        let pending = DRAG.with(|d| {
            if d.borrow().as_ref().map(|s| s.window_id) == Some(id) {
                *d.borrow_mut() = None; // native drag takes over from here
                true
            } else {
                false
            }
        });
        if pending {
            if let Some(window) = self.window() {
                window.performWindowDragWithEvent(ev);
            }
            return true;
        }
        false
    }

    fn button(&self, ev: &NSEvent, pressed: bool) {
        push(InputEvent::PointerButton {
            window_id: self.ivars().window_id.get(),
            button: btn_code(ev.buttonNumber()),
            pressed,
        });
    }

    /// If an interactive resize is active for this window, resize it to follow the
    /// mouse on the requested edges and return true (motion isn't forwarded).
    fn try_resize_window(&self) -> bool {
        let id = self.ivars().window_id.get();
        let Some((edges, anchor_mouse, anchor_frame)) = RESIZE.with(|r| {
            r.borrow()
                .as_ref()
                .filter(|s| s.window_id == id)
                .map(|s| (s.edges, s.anchor_mouse, s.anchor_frame))
        }) else {
            return false;
        };
        // Compute the desired logical size from the drag and ask the *client* to
        // repaint at that size. The NSWindow is not resized here — present_frame
        // grows/shrinks it to match the buffer that comes back, keeping the
        // corner opposite the dragged edge anchored. This avoids the window
        // running ahead of the buffer (which stretches the content).
        let mouse = NSEvent::mouseLocation();
        let dx = mouse.x - anchor_mouse.x;
        let dy = mouse.y - anchor_mouse.y; // macOS y grows upward
        let mut w = anchor_frame.size.width;
        let mut h = anchor_frame.size.height;
        if edges & 4 != 0 {
            w = anchor_frame.size.width - dx; // left
        }
        if edges & 8 != 0 {
            w = anchor_frame.size.width + dx; // right
        }
        if edges & 1 != 0 {
            h = anchor_frame.size.height + dy; // top
        }
        if edges & 2 != 0 {
            h = anchor_frame.size.height - dy; // bottom
        }
        let w = w.max(120.0) as i32;
        let h = h.max(80.0) as i32;
        push(InputEvent::Resize {
            window_id: id,
            width: w,
            height: h,
        });
        true
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
        let button = btn_code(ev.buttonNumber());
        if !pressed {
            // How far the pointer travelled since the press: a click barely moves, a
            // drag crosses into the menu.
            let dragged = IMPLICIT_GRAB_ORIGIN.with(|o| o.get()).is_some_and(|p| {
                let m = NSEvent::mouseLocation();
                ((m.x - p.x).powi(2) + (m.y - p.y).powi(2)).sqrt() > 6.0
            });
            IMPLICIT_GRAB_ORIGIN.with(|o| o.set(None));
            // Press-drag-release onto a menu item: the button was pressed on the
            // parent (a menu button) and released, after dragging, inside the
            // grabbing popup. Route the release to the popup so the item activates —
            // the implicit grab would send it to the parent instead. Only when the
            // pointer actually dragged: a release that didn't move is the opening
            // click, which must NOT select the item the popup mapped under (a
            // combobox/menu opening at the cursor).
            if dragged {
                if let Some(gid) = grabbed() {
                    if let Some((x, y, true)) = global_in_window(gid) {
                        debug!(target: "mac", "button release drag-selected popup {gid}");
                        push(InputEvent::PointerMotion { window_id: gid, x, y });
                        push(InputEvent::PointerButton { window_id: gid, button, pressed: false });
                        IMPLICIT_GRAB.with(|g| g.set(None));
                        return;
                    }
                }
            }
            // A release belongs to the surface that received the press (the implicit
            // pointer grab): AppKit delivers mouseUp to the pressing view even if a
            // popup mapped on top in between. Routing it here — instead of to the
            // popup grab — stops the click that OPENED a menu from also selecting
            // the item the popup happens to map under.
            if let Some(w) = IMPLICIT_GRAB.with(|g| g.take()) {
                debug!(target: "mac", "button release -> implicit-grab window {w} (dragged={dragged})");
                push(InputEvent::PointerButton {
                    window_id: w,
                    button,
                    pressed: false,
                });
            }
            return;
        }
        IMPLICIT_GRAB_ORIGIN.with(|o| o.set(Some(NSEvent::mouseLocation())));
        if let Some(gid) = grabbed() {
            if let Some((x, y, inside)) = global_in_window(gid) {
                debug!(
                    target: "mac",
                    "button press with popup grab on {gid}: cursor {}", if inside { "inside -> route to popup" } else { "outside -> dismiss" }
                );
                if inside {
                    // Ensure the popup has the pointer at the click location.
                    push(InputEvent::PointerMotion {
                        window_id: gid,
                        x,
                        y,
                    });
                    push(InputEvent::PointerButton {
                        window_id: gid,
                        button,
                        pressed: true,
                    });
                    IMPLICIT_GRAB.with(|g| g.set(Some(gid)));
                } else {
                    push(InputEvent::PopupDismiss { window_id: gid });
                    IMPLICIT_GRAB.with(|g| g.set(None));
                }
            }
            return;
        }
        debug!(
            target: "mac",
            "button press on window {} (no popup grab active)", self.ivars().window_id.get()
        );
        self.button(ev, true);
        IMPLICIT_GRAB.with(|g| g.set(Some(self.ivars().window_id.get())));
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

        // The hook AppKit uses to keep a window's title bar below the menu bar.
        // We extend it to also keep the window out of any docked-bar reserved
        // zone, so a top bar behaves like the menu bar: the window can't be
        // dragged or zoomed under it. Fullscreen windows are exempt (they cover
        // bars intentionally).
        #[unsafe(method(constrainFrameRect:toScreen:))]
        fn constrain_frame_rect(&self, frame: CGRect, screen: Option<&NSScreen>) -> CGRect {
            // RAIL: weston is the sole authority on window position — it does
            // server-side moves, drags, and hides a popup by moving it far
            // OFF-SCREEN. AppKit's default constraint keeps a window's title bar
            // on-screen, which would clamp such a hidden popup back into view so it
            // never disappears. Bypass constraining entirely in RAIL mode.
            if RAIL_MODE.load(std::sync::atomic::Ordering::Relaxed) {
                return frame;
            }
            // Fullscreen windows (they cover bars) and layer windows/bars
            // themselves (they own the reserved zone and sit at the screen edge)
            // are positioned exactly by us — bypass AppKit's constraint entirely.
            if let Some(view) = self.contentView() {
                if let Ok(v) = view.downcast::<WaylandView>() {
                    let id = v.ivars().window_id.get();
                    let exempt = FULLSCREEN.with(|f| f.borrow().contains(&id))
                        || LAYER_WINDOWS.with(|f| f.borrow().contains(&id));
                    if exempt {
                        return frame;
                    }
                }
            }
            let mut r: CGRect =
                unsafe { msg_send![super(self), constrainFrameRect: frame, toScreen: screen] };
            let (top, right, bottom, left) = crate::input::reserved_insets();
            if top == 0 && right == 0 && bottom == 0 && left == 0 {
                return r;
            }
            let visible = match screen {
                Some(s) => s.visibleFrame(),
                None => match self.screen() {
                    Some(s) => s.visibleFrame(),
                    None => return r,
                },
            };
            let work = apply_reserved_insets(visible);
            // macOS y grows upward. Clamp the top edge first (the menu-bar analog),
            // then the other edges, but only nudge an edge inward when the window
            // actually fits along that axis so a clamp never fights another.
            let work_top = work.origin.y + work.size.height;
            if r.origin.y + r.size.height > work_top {
                r.origin.y = work_top - r.size.height;
            }
            if r.origin.y < work.origin.y && r.size.height <= work.size.height {
                r.origin.y = work.origin.y;
            }
            let work_right = work.origin.x + work.size.width;
            if r.origin.x + r.size.width > work_right {
                r.origin.x = work_right - r.size.width;
            }
            if r.origin.x < work.origin.x && r.size.width <= work.size.width {
                r.origin.x = work.origin.x;
            }
            r
        }
    }
);

impl WaylandWindow {
    // Returns the object as its `NSWindow` base class rather than `Self`; callers
    // only need the `NSWindow` API surface.
    #[allow(clippy::new_ret_no_self)]
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
        // When the window is deactivated (the user clicks another app/window), tell
        // the client it lost focus. Without this, a client that dismisses menus on
        // focus-out (e.g. Firefox's in-content app menu) keeps them open forever,
        // because mouseExited only fires if the pointer physically leaves the view.
        // Click-to-focus (the macOS convention): keyboard focus follows the key
        // window, set when the user clicks a window or the app activates — NOT the
        // pointer. (Focus-following-the-pointer churned wl_keyboard.enter/leave on
        // every hover, which unsettled clients — e.g. Firefox menus.)
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notif: &NSNotification) {
            // An open menu holds keyboard focus until dismissed; don't steal it.
            if popup_open() {
                return;
            }
            push(InputEvent::Focus {
                window_id: self.ivars().window_id,
                focused: true,
            });
        }
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notif: &NSNotification) {
            // The app lost key focus (clicked another app / our other window / the
            // desktop). Signal a *real* deactivation so the engine can dismiss an
            // open menu whose dismissing click never reached any of our views. This
            // is deliberately NOT the pointer-leave path: a popup opening under the
            // cursor fires mouseExited on the toplevel, and treating that as
            // deactivation would kill the popup the instant it appears.
            let id = self.ivars().window_id;
            push(InputEvent::AppDeactivated { window_id: id });
            push(InputEvent::Focus { window_id: id, focused: false });
        }
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
            // During an interactive edge resize, try_resize_window is the sole
            // authority on the requested size and present_frame drives the frame
            // from the buffer — so ignore these notifications (which present's own
            // setFrame would otherwise echo back into a feedback loop).
            let resizing = RESIZE
                .with(|r| r.borrow().as_ref().map(|s| s.window_id))
                == Some(self.ivars().window_id);
            if resizing {
                return;
            }
            // Skip echoes of sizes we already asked the client to paint.
            if self.ivars().last.get() == (w, h) {
                return;
            }
            self.ivars().last.set((w, h));
            // Ask the client to paint its CONTENT (window geometry) at the window
            // size minus its CSD shadow margin. A CSD client pads the buffer with
            // the margin, so a plain `configure(window size)` would return a buffer
            // that's `window + margin`, which present_frame would then grow the
            // window to — a runaway loop. Subtracting the margin makes the returned
            // buffer match the current window size, so it settles immediately.
            // try_borrow, not borrow: setContentSize in present_frame runs inside
            // WINDOWS.borrow_mut() and synchronously re-enters here — a plain
            // borrow would panic (double borrow). In that case the margin we'd
            // read is the one present_frame just set, so falling back is fine.
            let (mw, mh) = WINDOWS.with(|wm| {
                wm.try_borrow()
                    .ok()
                    .and_then(|m| m.get(&self.ivars().window_id).map(|e| e.csd_margin))
                    .unwrap_or((0, 0))
            });
            push(InputEvent::Resize {
                window_id: self.ivars().window_id,
                width: (w - mw).max(1),
                height: (h - mh).max(1),
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

/// Transfer function (electro-optical curve) of a surface's content, from the
/// color-management protocol. `Pq`/`Hlg` are the HDR curves that light up the
/// display's Extended Dynamic Range headroom; `Srgb` is ordinary SDR.
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum TransferFn {
    Srgb,
    Pq,
    Hlg,
}

/// Color primaries (gamut) of a surface's content.
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Primaries {
    Srgb,
    DisplayP3,
    Bt2020,
}

/// A surface's color characteristics, negotiated via `wp_color_manager_v1`.
/// Built on the Wayland side (see `color_management.rs`) and carried to the
/// AppKit side so `make_cgimage` can pick a matching `CGColorSpace` and the
/// layer can opt into EDR.
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ColorDesc {
    pub tf: TransferFn,
    pub primaries: Primaries,
    /// Maximum mastering/target luminance in cd/m² (nits), if the client set it.
    /// Captured for completeness; not used for tone-mapping (the OS composites).
    pub max_luminance: Option<f32>,
    /// Reference (SDR) white luminance in cd/m², if the client set it.
    pub ref_luminance: Option<f32>,
}

impl ColorDesc {
    /// True when this content needs HDR/EDR output (a PQ or HLG transfer curve).
    pub fn is_hdr(&self) -> bool {
        matches!(self.tf, TransferFn::Pq | TransferFn::Hlg)
    }

}

/// Memory layout of the raw pixels in a `WinCmd::Frame`/`SubFrame`, so the AppKit
/// side interprets them without a lossy 8-bit conversion. Mirrors the shm formats
/// accepted in `shm.rs` (see `buffer_to_pixels`).
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum PixelFormat {
    /// 32-bit, byte order B,G,R,X/A (Wayland XRGB8888/ARGB8888 little-endian).
    Bgra8888,
    /// 32-bit, 2:10:10:10 packed (Wayland xRGB2101010 & friends, little-endian).
    Rgb2101010,
    /// 64-bit, four 16-bit floats per pixel (Wayland xRGB16161616F & friends).
    Rgba16F,
}

/// A command sent from the Wayland thread to the AppKit main thread.
///
/// `Serialize`/`Deserialize` let the exact same command cross a process boundary
/// in `--multiplex` mode, where the AppKit "main thread" lives in a separate
/// per-app window-host process (see `src/ipc.rs`, `src/host.rs`).
#[derive(serde::Serialize, serde::Deserialize)]
pub enum WinCmd {
    /// Create a native window for a Wayland toplevel.
    Create {
        id: u32,
        width: i32,
        height: i32,
        /// Logical (point) size from `wp_viewport.set_destination`, or (0,0) to
        /// derive it from the buffer pixels and the output scale.
        dst_w: i32,
        dst_h: i32,
        /// The client wants server-side decorations: give the window a native
        /// macOS titlebar instead of a borderless (CSD) frame.
        decorated: bool,
        title: String,
        /// CSD window geometry `(x, y, w, h)` in logical points: the content bounds
        /// within the buffer, excluding shadow margins. (0,0,0,0) = client set none.
        geom: (i32, i32, i32, i32),
    },
    /// Present a new frame (a committed shm buffer) into a window.
    Frame {
        id: u32,
        width: i32,
        height: i32,
        stride: i32,
        /// Logical (point) size from `wp_viewport.set_destination`, or (0,0) to
        /// derive it from the buffer pixels and the output scale.
        dst_w: i32,
        dst_h: i32,
        /// Raw pixels in `format`'s layout (BGRA8888 by default; 10-bit / f16 for HDR).
        pixels: Vec<u8>,
        /// Memory layout of `pixels`.
        format: PixelFormat,
        /// The surface's negotiated color characteristics, if any (HDR/wide-gamut).
        /// `None` = ordinary sRGB SDR.
        color: Option<ColorDesc>,
        /// CSD window geometry `(x, y, w, h)` in logical points (see Create). When
        /// set, the window is sized to `(w,h)` and the buffer is cropped to the
        /// content, so a shadow-padded buffer doesn't grow the window each resize.
        geom: (i32, i32, i32, i32),
    },
    /// Update a window title.
    Title { id: u32, title: String },
    /// Begin an interactive drag of the window (from `xdg_toplevel.move`).
    StartMove { id: u32 },
    /// Position a window at an absolute RDP desktop offset (`x`,`y` in logical
    /// points, top-left origin). Used by the RAIL back-end to mirror weston's
    /// server-side window placement/move: weston owns the position, so the
    /// NSWindow tracks it exactly (converted to macOS' bottom-left origin). Absolute
    /// (not relative) so weston's edges map to the screen's — the window can reach
    /// the very top rather than being offset by the compositor's own placement.
    Move { id: u32, x: i32, y: i32 },
    /// Begin an interactive resize on the given edges (from `xdg_toplevel.resize`).
    StartResize { id: u32, edges: u32 },
    /// Create a borderless popup window (menu/dropdown) positioned relative to
    /// its parent (logical/point coordinates, top-left of the parent surface).
    /// `x_flip`/`y_flip` are the same origin with the positioner's anchor+gravity
    /// inverted per axis, and `constraint` its `set_constraint_adjustment` bitmask;
    /// together they let `create_popup` flip/slide the popup back on-screen.
    CreatePopup {
        id: u32,
        parent_id: u32,
        x: i32,
        y: i32,
        x_flip: i32,
        y_flip: i32,
        constraint: u32,
        width: i32,
        height: i32,
    },
    /// Set (or clear) the popup that currently grabs the pointer.
    SetGrab { window: Option<u32> },
    /// The focused client requested a cursor shape (wp_cursor_shape).
    SetCursor { shape: u32 },
    /// The client set a custom cursor from a surface buffer (wl_pointer.set_cursor).
    SetCursorImage {
        width: i32,
        height: i32,
        stride: i32,
        /// Hotspot in the cursor buffer's own coordinate space (see `scale`).
        hotspot_x: i32,
        hotspot_y: i32,
        /// Scale factor of the cursor buffer: how many buffer pixels map to one
        /// logical point. Wayland cursors are rendered at the output backing
        /// factor; RAIL (RDP) pointer bitmaps arrive at device resolution (1).
        scale: i32,
        pixels: Vec<u8>,
    },
    /// Hide the cursor (wl_pointer.set_cursor with a null surface).
    HideCursor,
    /// Composite a subsurface as a sublayer of a window. `sub_id` keys the sublayer;
    /// `x`,`y` are the position within the parent (logical points, top-left origin).
    SubFrame {
        window_id: u32,
        sub_id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: i32,
        dst_w: i32,
        dst_h: i32,
        pixels: Vec<u8>,
        /// Memory layout of `pixels`.
        format: PixelFormat,
        /// The subsurface's negotiated color characteristics, if any.
        color: Option<ColorDesc>,
    },
    /// Remove a subsurface's sublayer.
    SubDestroy { window_id: u32, sub_id: u32 },
    /// Maximize / unmaximize a window (xdg_toplevel.[un]set_maximized).
    Maximize { id: u32, on: bool },
    /// Fullscreen / unfullscreen a window.
    Fullscreen { id: u32, on: bool },
    /// Minimize a window.
    Minimize { id: u32 },
    /// Set the window's minimum content size (logical points).
    SetMinSize { id: u32, width: i32, height: i32 },
    /// Set the window's maximum content size (0 = unlimited).
    SetMaxSize { id: u32, width: i32, height: i32 },
    /// Raise and focus a window (xdg_activation_v1.activate).
    Activate { id: u32 },
    /// Mark a window as a modal dialog / clear that mark (xdg_dialog_v1).
    /// Modal dialogs float above their normal-level windows.
    SetModal { id: u32, modal: bool },
    /// Set the app's Dock/Cmd-Tab icon from raw BGRA pixels (from
    /// `xdg_toplevel_icon_v1`, or a generated fallback). In --multiplex mode this
    /// targets the window's host process (one app per client). `id` identifies the
    /// owning window for routing.
    SetIcon {
        id: u32,
        width: i32,
        height: i32,
        stride: i32,
        pixels: Vec<u8>,
    },
    /// Destroy a window.
    Destroy { id: u32 },
    /// Create a docked bar/panel (wlr-layer-shell): a borderless floating window
    /// anchored to a screen edge. `anchor` bitfield: top=1, bottom=2, left=4,
    /// right=8. `width`/`height` are physical buffer pixels.
    CreateLayer {
        id: u32,
        width: i32,
        height: i32,
        anchor: u32,
        margin_top: i32,
        margin_right: i32,
        margin_bottom: i32,
        margin_left: i32,
        /// The surface wants keyboard input (e.g. a launcher): make the window key
        /// so macOS routes keystrokes to it. A plain bar (false) only orders front.
        keyboard: bool,
    },
}

struct WinEntry {
    window: Retained<NSWindow>,
    view: Retained<WaylandView>,
    // Kept alive because NSWindow holds only a weak delegate reference.
    // Popups have no delegate (they don't resize).
    _delegate: Option<Retained<WinDelegate>>,
    cur_w: i32,
    cur_h: i32,
    /// Server-decorated (native titlebar): the window size is macOS/user-driven,
    /// so present_frame must not resize it from the (lagging) buffer — doing so
    /// creates a configure↔resize feedback loop that oscillates between two sizes.
    decorated: bool,
    /// CSD shadow-margin size (logical points): buffer minus the client's window
    /// geometry. A resize configure asks for (window size − margin) so a client
    /// that pads its buffer with shadows doesn't grow the window each round-trip.
    csd_margin: (i32, i32),
    /// Subsurface sublayers keyed by sub_id (see WinCmd::SubFrame).
    sublayers: HashMap<u32, Retained<CALayer>>,
}

struct DragState {
    window_id: u32,
}

struct ResizeState {
    window_id: u32,
    edges: u32, // xdg_toplevel ResizeEdge bitmask: top=1 bottom=2 left=4 right=8
    anchor_mouse: CGPoint,
    anchor_frame: CGRect,
}

thread_local! {
    static WINDOWS: RefCell<HashMap<u32, WinEntry>> = RefCell::new(HashMap::new());
    // Active interactive window move (from xdg_toplevel.move). The move request
    // round-trips through the container, so we can't use the live NSEvent — we
    // anchor here and follow the global mouse ourselves.
    static DRAG: RefCell<Option<DragState>> = const { RefCell::new(None) };
    // Active interactive resize (from xdg_toplevel.resize).
    static RESIZE: RefCell<Option<ResizeState>> = const { RefCell::new(None) };
    // Window id of a popup that has grabbed the pointer (from xdg_popup.grab).
    // While set, all pointer input routes to that popup (menus), and a click
    // outside it dismisses the menu.
    static GRAB: Cell<Option<u32>> = const { Cell::new(None) };
    // Implicit pointer grab: the window that received the current held button-press.
    // Its matching release is delivered here regardless of any popup grab, so the
    // click that OPENS a menu doesn't also select the item the popup maps under.
    static IMPLICIT_GRAB: Cell<Option<u32>> = const { Cell::new(None) };
    // Screen location where the held button was pressed, so a release can tell a
    // click (opened a menu — honor the implicit grab) from a drag (press-drag-
    // release onto a menu item — route the release to the grabbing popup so the
    // item activates).
    static IMPLICIT_GRAB_ORIGIN: Cell<Option<CGPoint>> = const { Cell::new(None) };
    // The cursor the focused client last requested (via wp_cursor_shape); applied
    // through the view's cursor rects.
    static CURSOR: RefCell<Option<Retained<NSCursor>>> = const { RefCell::new(None) };
    // Pre-maximize/fullscreen frames, so unmaximize/unfullscreen can restore them.
    static SAVED_FRAMES: RefCell<HashMap<u32, CGRect>> = RefCell::new(HashMap::new());
    // Windows currently in fullscreen: they intentionally cover docked bars, so
    // constrainFrameRect: must not clamp them into the reserved work area.
    static FULLSCREEN: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    // Layer-shell windows (docked bars, launchers). They are positioned exactly by
    // us (a bar defines the reserved zone and sits at the very screen edge), so
    // constrainFrameRect: must leave them alone — otherwise a bar is pushed out of
    // its own exclusive zone.
    static LAYER_WINDOWS: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    // Currently-mapped popup windows (menus). While any is open, focus-follows-
    // mouse is suppressed: a menu holds keyboard focus until dismissed, so moving
    // the pointer off it must not send a keyboard-leave (which a non-grabbing menu,
    // e.g. a Firefox context menu, treats as "close").
    static POPUP_WINDOWS: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
}

/// True while any popup (menu) is open. Used to keep keyboard focus pinned to the
/// menu instead of following the pointer.
fn popup_open() -> bool {
    POPUP_WINDOWS.with(|p| !p.borrow().is_empty())
}

/// Fill the window's screen (visible area for maximize, whole screen for
/// fullscreen), or restore its saved frame. Resizing triggers `windowDidResize`,
/// which sends the client a configure at the new size.
fn set_window_fill(id: u32, on: bool, fullscreen: bool) {
    WINDOWS.with(|w| {
        let map = w.borrow();
        let Some(e) = map.get(&id) else { return };
        if on {
            SAVED_FRAMES.with(|s| {
                s.borrow_mut().entry(id).or_insert_with(|| e.window.frame());
            });
            // Track fullscreen so constrainFrameRect: lets it cover docked bars.
            FULLSCREEN.with(|f| {
                if fullscreen {
                    f.borrow_mut().insert(id);
                } else {
                    f.borrow_mut().remove(&id);
                }
            });
            if let Some(screen) = e.window.screen() {
                let target = if fullscreen {
                    // Fullscreen intentionally covers everything, bars included.
                    screen.frame()
                } else {
                    // Maximize fills the work area minus any docked bars' reserved
                    // zones, so a maximized window doesn't sit under a bar.
                    apply_reserved_insets(screen.visibleFrame())
                };
                e.window.setFrame_display(target, true);
            }
        } else {
            FULLSCREEN.with(|f| {
                f.borrow_mut().remove(&id);
            });
            if let Some(saved) = SAVED_FRAMES.with(|s| s.borrow_mut().remove(&id)) {
                e.window.setFrame_display(saved, true);
            }
        }
    });
}

/// Center a newly-created window within the work area (visible frame minus the
/// reserved bar zones), clamping so its top never crosses under a top bar.
fn place_in_work_area(mtm: MainThreadMarker, window: &NSWindow, lw: f64, lh: f64) {
    let Some(screen) = window.screen().or_else(|| NSScreen::mainScreen(mtm)) else {
        window.center();
        return;
    };
    let work = apply_reserved_insets(screen.visibleFrame());
    let x = work.origin.x + (work.size.width - lw).max(0.0) / 2.0;
    let mut y = work.origin.y + (work.size.height - lh).max(0.0) / 2.0;
    // macOS y grows upward; keep the window's top edge inside the work area.
    let work_top = work.origin.y + work.size.height;
    if y + lh > work_top {
        y = work_top - lh;
    }
    window.setFrameOrigin(CGPoint::new(x, y));
}

/// Shrink a screen rect by the work-area insets reserved by docked bars
/// (layer-shell exclusive zones). macOS y is bottom-up, so a top inset trims the
/// height and a bottom inset also raises the origin.
fn apply_reserved_insets(rect: CGRect) -> CGRect {
    let (top, right, bottom, left) = crate::input::reserved_insets();
    CGRect::new(
        CGPoint::new(rect.origin.x + left as f64, rect.origin.y + bottom as f64),
        CGSize::new(
            (rect.size.width - (left + right) as f64).max(1.0),
            (rect.size.height - (top + bottom) as f64).max(1.0),
        ),
    )
}

/// Map a wp_cursor_shape shape id to the closest NSCursor.
#[allow(deprecated)] // resize*Cursor are deprecated but the replacements are 15.0+
fn map_cursor(shape: u32) -> Retained<NSCursor> {
    match shape {
        2 => NSCursor::contextualMenuCursor(),        // context_menu
        4 => NSCursor::pointingHandCursor(),          // pointer
        8 => NSCursor::crosshairCursor(),             // crosshair
        9 | 10 => NSCursor::IBeamCursor(),            // text / vertical_text
        12 => NSCursor::dragCopyCursor(),             // copy
        14 | 15 => NSCursor::operationNotAllowedCursor(), // no_drop / not_allowed
        16 => NSCursor::openHandCursor(),             // grab
        17 => NSCursor::closedHandCursor(),           // grabbing
        18 | 25 | 26 | 30 => NSCursor::resizeLeftRightCursor(), // e/w/ew/col resize
        19 | 22 | 27 | 31 => NSCursor::resizeUpDownCursor(),    // n/s/ns/row resize
        _ => NSCursor::arrowCursor(),
    }
}

/// Re-run every view's `resetCursorRects` so a newly set CURSOR takes effect.
fn refresh_cursor_rects() {
    WINDOWS.with(|w| {
        for e in w.borrow().values() {
            e.window.invalidateCursorRectsForView(&e.view);
        }
    });
}

/// Build an `NSCursor` from a client cursor-surface buffer. The buffer is in
/// physical pixels; the image is shown at logical points (÷scale) and the hotspot
/// is already in surface-local (logical) coordinates.
/// Set this process's Dock / Cmd-Tab icon from raw BGRA pixels. In --multiplex
/// mode each app is its own process, so this sets that one app's icon; the source
/// is either `xdg_toplevel_icon_v1` artwork or the generated identicon fallback.
pub(crate) fn set_app_icon(
    mtm: MainThreadMarker,
    width: i32,
    height: i32,
    stride: i32,
    pixels: &[u8],
) {
    let Some(image) = make_cgimage(width, height, stride, pixels, PixelFormat::Bgra8888, None)
    else {
        error!(target: "mac", "failed to build CGImage for app icon");
        return;
    };
    let size = CGSize::new(width as f64, height as f64);
    let ns_image = NSImage::initWithCGImage_size(mtm.alloc::<NSImage>(), &image, size);
    // Safe: NSImage built above is a valid image; the call just swaps the Dock icon.
    unsafe {
        NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&ns_image));
    }
    info!(target: "mac", "set app icon {width}x{height}");
}

fn make_cursor(
    mtm: MainThreadMarker,
    width: i32,
    height: i32,
    stride: i32,
    hotspot_x: i32,
    hotspot_y: i32,
    scale: i32,
    pixels: &[u8],
) -> Option<Retained<NSCursor>> {
    // Cursor buffers are always ordinary 8-bit BGRA (SDR).
    let image = make_cgimage(width, height, stride, pixels, PixelFormat::Bgra8888, None)?;
    // The buffer is `scale` pixels per logical point, so the NSImage's point size
    // is the pixel size divided by it. Macs upscale the image to physical pixels,
    // so a device-resolution (scale-1) cursor keeps its apparent size on a Retina
    // display instead of rendering half-size. The hotspot is already in logical
    // points (Wayland's surface-local space; RAIL is scale-1 so equal either way).
    let scale = (scale.max(1)) as f64;
    let size = CGSize::new(width as f64 / scale, height as f64 / scale);
    let ns_image = NSImage::initWithCGImage_size(mtm.alloc::<NSImage>(), &image, size);
    let hotspot = CGPoint::new(hotspot_x as f64, hotspot_y as f64);
    Some(NSCursor::initWithImage_hotSpot(
        mtm.alloc::<NSCursor>(),
        &ns_image,
        hotspot,
    ))
}

/// A fully transparent 1x1 cursor, used to hide the pointer.
fn empty_cursor(mtm: MainThreadMarker) -> Retained<NSCursor> {
    let pixels = [0u8; 4]; // transparent BGRA
    make_cursor(mtm, 1, 1, 4, 0, 0, 1, &pixels).unwrap_or_else(NSCursor::arrowCursor)
}

fn grabbed() -> Option<u32> {
    let g = GRAB.with(|g| g.get())?;
    // Self-heal: if the grabbing popup's window no longer exists (e.g. a menu that
    // was abandoned before it painted), clear the grab instead of swallowing all
    // input forever.
    let exists = WINDOWS.with(|w| w.borrow().contains_key(&g));
    if !exists {
        GRAB.with(|c| c.set(None));
        return None;
    }
    Some(g)
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
            inside.then_some((*id, gm.x - f.origin.x, f.size.height - (gm.y - f.origin.y)))
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

/// Test hook: when a sink is installed, `post` diverts every `WinCmd` into it
/// instead of dispatching to the AppKit main queue. This is the seam headless
/// tests use to observe the protocol layer's output without touching AppKit.
/// Production behavior is unchanged when no sink is set.
static POST_SINK: std::sync::OnceLock<std::sync::Mutex<Option<std::sync::mpsc::Sender<WinCmd>>>> =
    std::sync::OnceLock::new();

/// Install a channel that captures every `WinCmd` passed to `post`.
/// Test-only seam; unused in the shipped binary.
#[allow(dead_code)]
pub fn set_post_sink(tx: std::sync::mpsc::Sender<WinCmd>) {
    let cell = POST_SINK.get_or_init(|| std::sync::Mutex::new(None));
    *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
}

/// Enqueue a window command onto the AppKit main thread.
pub fn post(cmd: WinCmd) {
    if let Some(m) = POST_SINK.get() {
        if let Some(tx) = m.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(cmd);
            return;
        }
    }
    // --multiplex mode: this process owns no windows; hand the command to the
    // per-app window-host that owns the target window (see src/router.rs).
    if crate::router::is_enabled() {
        crate::router::route(cmd);
        return;
    }
    DispatchQueue::main().exec_async(move || handle(cmd));
}

/// In `--multiplex` mode, tell the router which app a soon-to-be-created window
/// belongs to (spawning that app's host on first use). A no-op otherwise, so the
/// Wayland engine can call it unconditionally before posting a `Create`.
pub fn assign_window(win_id: u32, app_key: u32, name: &str, regular: bool) {
    crate::router::assign_window(win_id, app_key, name, regular);
}

/// In `--multiplex` mode, push updated docked-bar insets to every window-host so
/// their windows keep avoiding a bar that lives in another process. No-op otherwise.
pub fn broadcast_insets(top: i32, right: i32, bottom: i32, left: i32) {
    crate::router::broadcast_insets(top, right, bottom, left);
}

fn handle(cmd: WinCmd) {
    // Safe: exec_async on the main queue always runs on the main thread.
    let mtm = MainThreadMarker::new().expect("must run on main thread");
    match cmd {
        WinCmd::Create {
            id,
            width,
            height,
            dst_w,
            dst_h,
            decorated,
            title,
            geom,
        } => create_window(
            mtm,
            id,
            width.max(1),
            height.max(1),
            dst_w,
            dst_h,
            decorated,
            &title,
            geom,
        ),
        WinCmd::Frame {
            id,
            width,
            height,
            stride,
            dst_w,
            dst_h,
            pixels,
            format,
            color,
            geom,
        } => present_frame(
            mtm, id, width, height, stride, dst_w, dst_h, &pixels, format, color, geom,
        ),
        WinCmd::Title { id, title } => {
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    e.window.setTitle(&NSString::from_str(&title));
                }
            });
        }
        WinCmd::StartMove { id } => {
            // Mark a move pending; the view's next mouseDragged hands off to the
            // native window drag (which gives edge-snapping + drag-to-top maximize).
            DRAG.with(|d| *d.borrow_mut() = Some(DragState { window_id: id }));
        }
        WinCmd::StartResize { id, edges } => {
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    RESIZE.with(|r| {
                        *r.borrow_mut() = Some(ResizeState {
                            window_id: id,
                            edges,
                            anchor_mouse: NSEvent::mouseLocation(),
                            anchor_frame: e.window.frame(),
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
            x_flip,
            y_flip,
            constraint,
            width,
            height,
        } => create_popup(
            mtm,
            id,
            parent_id,
            x,
            y,
            x_flip,
            y_flip,
            constraint,
            width.max(1),
            height.max(1),
        ),
        WinCmd::SetGrab { window } => {
            let was = GRAB.with(|g| g.replace(window));
            // Grab just ended: restore focus to the window under the cursor.
            if window.is_none() && was.is_some() {
                restore_focus_under_cursor();
            }
        }
        WinCmd::SetCursor { shape } => {
            CURSOR.with(|c| *c.borrow_mut() = Some(map_cursor(shape)));
            refresh_cursor_rects();
        }
        WinCmd::SetCursorImage {
            width,
            height,
            stride,
            hotspot_x,
            hotspot_y,
            scale,
            pixels,
        } => {
            if let Some(cursor) =
                make_cursor(mtm, width, height, stride, hotspot_x, hotspot_y, scale, &pixels)
            {
                CURSOR.with(|c| *c.borrow_mut() = Some(cursor));
                refresh_cursor_rects();
            }
        }
        WinCmd::HideCursor => {
            CURSOR.with(|c| *c.borrow_mut() = Some(empty_cursor(mtm)));
            refresh_cursor_rects();
        }
        WinCmd::SubFrame {
            window_id,
            sub_id,
            x,
            y,
            width,
            height,
            stride,
            dst_w,
            dst_h,
            pixels,
            format,
            color,
        } => present_subframe(
            window_id, sub_id, x, y, width, height, stride, dst_w, dst_h, &pixels, format, color,
        ),
        WinCmd::SubDestroy { window_id, sub_id } => {
            WINDOWS.with(|w| {
                if let Some(entry) = w.borrow_mut().get_mut(&window_id) {
                    if let Some(layer) = entry.sublayers.remove(&sub_id) {
                        layer.removeFromSuperlayer();
                    }
                }
            });
        }
        WinCmd::Maximize { id, on } => set_window_fill(id, on, false),
        WinCmd::Fullscreen { id, on } => set_window_fill(id, on, true),
        WinCmd::Minimize { id } => {
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    e.window.miniaturize(None);
                }
            });
        }
        WinCmd::SetMinSize { id, width, height } => {
            let s = crate::input::scale();
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    e.window.setContentMinSize(CGSize::new(
                        (width / s).max(1) as f64,
                        (height / s).max(1) as f64,
                    ));
                }
            });
        }
        WinCmd::SetMaxSize { id, width, height } => {
            let s = crate::input::scale();
            // 0 means "no limit"; use a very large value.
            let big = 1_000_000.0;
            let mw = if width > 0 { (width / s) as f64 } else { big };
            let mh = if height > 0 { (height / s) as f64 } else { big };
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    e.window.setContentMaxSize(CGSize::new(mw, mh));
                }
            });
        }
        WinCmd::Activate { id } => {
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    let app = NSApplication::sharedApplication(mtm);
                    #[allow(deprecated)]
                    app.activateIgnoringOtherApps(true);
                    e.window.makeKeyAndOrderFront(None);
                }
            });
        }
        WinCmd::Move { id, x, y } => {
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    // Map the RDP desktop offset (top-left origin, y down) onto the
                    // macOS global coordinate space (bottom-left origin, y up). The
                    // RAIL desktop spans ALL monitors (see main.rs), so convert
                    // against the union bounding box of every screen — not the
                    // window's current screen, which would shift the basis mid-drag
                    // and make crossing to a second monitor jump.
                    let screens = NSScreen::screens(mtm);
                    let (mut min_x, mut max_top) = (f64::MAX, f64::MIN);
                    for s in screens.iter() {
                        let f = s.frame();
                        min_x = min_x.min(f.origin.x);
                        max_top = max_top.max(f.origin.y + f.size.height);
                    }
                    if min_x.is_finite() && max_top.is_finite() {
                        let wf = e.window.frame();
                        e.window.setFrameOrigin(CGPoint::new(
                            min_x + x as f64,
                            max_top - y as f64 - wf.size.height,
                        ));
                    }
                }
            });
        }
        WinCmd::SetModal { id, modal } => {
            WINDOWS.with(|w| {
                if let Some(e) = w.borrow().get(&id) {
                    // A modal dialog floats above ordinary windows; unsetting
                    // returns it to the normal level. We deliberately do not run
                    // a blocking modal session (that would freeze the compositor).
                    e.window.setLevel(if modal {
                        NSModalPanelWindowLevel
                    } else {
                        NSNormalWindowLevel
                    });
                }
            });
        }
        WinCmd::SetIcon {
            id: _,
            width,
            height,
            stride,
            pixels,
        } => set_app_icon(mtm, width, height, stride, &pixels),
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
            LAYER_WINDOWS.with(|f| {
                f.borrow_mut().remove(&id);
            });
            FULLSCREEN.with(|f| {
                f.borrow_mut().remove(&id);
            });
            SAVED_FRAMES.with(|s| {
                s.borrow_mut().remove(&id);
            });
            // A menu closing re-enables focus-follows-mouse (once no popup remains).
            let was_popup = POPUP_WINDOWS.with(|p| p.borrow_mut().remove(&id));
            if grab_ended {
                restore_focus_under_cursor();
            } else if was_popup && !popup_open() {
                // A non-grabbing menu closed (no grab to end): hand focus back to
                // whatever window is now under the cursor, since focus-follows-mouse
                // was suppressed while it was open.
                restore_focus_under_cursor();
            }
        }
        WinCmd::CreateLayer {
            id,
            width,
            height,
            anchor,
            margin_top,
            margin_right,
            margin_bottom,
            margin_left,
            keyboard,
        } => create_layer_window(
            mtm,
            id,
            width.max(1),
            height.max(1),
            anchor,
            (margin_top, margin_right, margin_bottom, margin_left),
            keyboard,
        ),
    }
}

/// Logical (point) size of a window: the `wp_viewport` destination if the client
/// set one, else the physical buffer size divided by the output scale. This is the
/// full buffer (including any CSD shadow margins), so content is never clipped.
fn logical_size(width: i32, height: i32, dst_w: i32, dst_h: i32, scale: i32) -> (f64, f64) {
    if dst_w > 0 && dst_h > 0 {
        (dst_w as f64, dst_h as f64)
    } else {
        ((width / scale).max(1) as f64, (height / scale).max(1) as f64)
    }
}

/// The CSD shadow-margin size (logical points) — the buffer minus the client's
/// window geometry. Used so a resize configure asks for the *content* size, not the
/// padded buffer size; otherwise the window grows by the margin each round-trip.
/// (0,0) when the client set no geometry or is using a viewport.
fn csd_margin(width: i32, height: i32, dst_w: i32, dst_h: i32, geom: (i32, i32, i32, i32), scale: i32) -> (i32, i32) {
    let (_, _, gw, gh) = geom;
    if dst_w > 0 || dst_h > 0 || gw <= 0 || gh <= 0 {
        return (0, 0);
    }
    let (bw, bh) = ((width / scale).max(1), (height / scale).max(1));
    ((bw - gw).max(0), (bh - gh).max(0))
}

/// Origin (in the parent view's y-up layer space) for a subsurface placed at the
/// Wayland top-left offset `(x, y)` with logical size `lh` tall, inside a window
/// `win_h` points tall. Wayland's y grows downward and the layer's grows upward,
/// so y is flipped against the window height.
///
/// `win_h` MUST be the window's *live* height. A stale height (e.g. a decorated
/// window's cached size, which `present_frame` intentionally stops updating during
/// a user resize) mis-places the sublayer: KWin draws its whole desktop into one
/// full-output subsurface, so if it lands low the newly-exposed top of the window
/// is left uncovered and shows the black root layer — the "black bar on resize".
fn subsurface_origin(win_h: f64, x: i32, y: i32, lh: f64) -> (f64, f64) {
    (x as f64, win_h - y as f64 - lh)
}

#[allow(clippy::too_many_arguments)]
/// True when running the RAIL back-end (a `--features rail` build). Set
/// once at startup; read in `create_window` to keep RAIL windows non-resizable
/// (see the style block there).
pub static RAIL_MODE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn create_window(
    mtm: MainThreadMarker,
    id: u32,
    width: i32,
    height: i32,
    dst_w: i32,
    dst_h: i32,
    decorated: bool,
    title: &str,
    geom: (i32, i32, i32, i32),
) {
    // The window/view are in logical points. Prefer an explicit viewport
    // destination; otherwise convert the physical buffer size by the output scale.
    let scale = crate::input::scale();
    let (lw, lh) = logical_size(width, height, dst_w, dst_h, scale);
    let margin = csd_margin(width, height, dst_w, dst_h, geom, scale);
    let rect = CGRect::new(CGPoint::new(120.0, 120.0), CGSize::new(lw, lh));
    // Server-side decorations: give the window a native macOS titlebar (title +
    // traffic-light buttons). CSD toolkits (GTK/libadwaita) instead draw their own
    // titlebar, rounded corners and shadow into the buffer's transparent margins,
    // so those windows are fully borderless (native chrome would double up).
    let style = if decorated {
        NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
    } else if RAIL_MODE.load(std::sync::atomic::Ordering::Relaxed) {
        // RAIL windows carry the app's own CSD (titlebar + resize borders) and
        // delegate move/resize to the remote server via forwarded pointer motion.
        // A natively Resizable borderless window makes macOS steal titlebar drags
        // as edge-resizes (the titlebar sits flush at the top edge), so weston
        // never sees the drag and never sends a move order. Keep it non-resizable.
        NSWindowStyleMask::Borderless
    } else {
        NSWindowStyleMask::Borderless | NSWindowStyleMask::Resizable
    };
    let window = WaylandWindow::new(mtm, rect, style);
    // We keep the NSWindow alive via a `Retained` in WINDOWS; `close()` must not
    // also release it, or we'd over-release and crash.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(title));
    // A decorated window keeps its native shadow; a borderless CSD window draws its
    // own shadow into the buffer margins, so suppress the native one there.
    window.setHasShadow(decorated);

    let view = WaylandView::new(
        mtm,
        id,
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(lw, lh)),
    );
    // Layer-HOSTING (own layer set before wantsLayer), not layer-backed: we add
    // subsurface sublayers ourselves, and AppKit's auto-managed backing layer
    // would not composite manually-added sublayers (they'd stay invisible).
    let root = CALayer::new();
    root.setContentsScale(scale as f64);
    root.setOpaque(false);
    // A hosted layer isn't auto-sized by AppKit; give it the view's bounds (kept
    // in sync in present_frame) so the main contents fill it.
    root.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(lw, lh)));
    let cg = NSColor::clearColor().CGColor();
    root.setBackgroundColor(Some(&cg));
    view.setLayer(Some(&root));
    view.setWantsLayer(true);
    // CSD toolkits (GTK) use translucent shadow margins, so present the window
    // transparent behind the buffer — otherwise the system background shows
    // through those margins. A server-decorated window's content fills the frame
    // opaquely, so keep it opaque there.
    window.setOpaque(decorated);
    if !decorated {
        window.setBackgroundColor(Some(&NSColor::clearColor()));
    }

    window.setAcceptsMouseMovedEvents(true);
    window.setContentView(Some(&view));

    // Forward native (edge) resizes back to the client (logical points).
    let delegate = WinDelegate::new(mtm, id, lw as i32, lh as i32);
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    // Place the window inside the work area (visible frame minus any docked-bar
    // reserved zones) so it doesn't spawn under a bar. macOS's own `center()`
    // ignores our reserved zones; you can still drag a window under the bar.
    place_in_work_area(mtm, &window, lw, lh);
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
                cur_w: lw as i32,
                cur_h: lh as i32,
                decorated,
                csd_margin: margin,
                sublayers: HashMap::new(),
            },
        );
    });
    info!(
        target: "mac",
        "created NSWindow for toplevel {id} ({lw}x{lh} pts, buffer {width}x{height})"
    );
}

/// Create a docked bar/panel (wlr-layer-shell): a borderless, floating window
/// anchored to a screen edge per the anchor bitfield (top=1, bottom=2, left=4,
/// right=8) and margins. It floats above normal windows. A plain bar doesn't take
/// key focus; a keyboard-interactive surface (a launcher) is made key so macOS
/// routes keystrokes to it.
fn create_layer_window(
    mtm: MainThreadMarker,
    id: u32,
    width: i32,
    height: i32,
    anchor: u32,
    margins: (i32, i32, i32, i32), // (top, right, bottom, left)
    keyboard: bool,
) {
    let scale = crate::input::scale();
    let lw = (width / scale).max(1) as f64;
    let lh = (height / scale).max(1) as f64;
    let (mt, mr, mb, ml) = (
        margins.0 as f64,
        margins.1 as f64,
        margins.2 as f64,
        margins.3 as f64,
    );
    let (top, bottom, left, right) = (
        anchor & 1 != 0,
        anchor & 2 != 0,
        anchor & 4 != 0,
        anchor & 8 != 0,
    );

    // Use the visible frame (excludes the macOS menu bar and Dock) so a top bar
    // docks just below the menu bar and a bottom bar above the Dock.
    let sf = NSScreen::mainScreen(mtm)
        .map(|s| s.visibleFrame())
        .unwrap_or_else(|| CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(lw, lh)));
    let (sx, sy, sw, sh) = (sf.origin.x, sf.origin.y, sf.size.width, sf.size.height);

    let x = if left && !right {
        sx + ml
    } else if right && !left {
        sx + sw - lw - mr
    } else {
        sx + (sw - lw) / 2.0 // spanning or unspecified -> centered
    };
    // macOS y grows upward; the screen's top edge is at sy + sh.
    let y = if top && !bottom {
        sy + sh - lh - mt
    } else if bottom && !top {
        sy + mb
    } else {
        sy + (sh - lh) / 2.0
    };

    // A layer window is positioned exactly by us; exempt it from the reserved-zone
    // constraint (a bar owns that zone and must sit at the screen edge).
    LAYER_WINDOWS.with(|f| {
        f.borrow_mut().insert(id);
    });
    let rect = CGRect::new(CGPoint::new(x, y), CGSize::new(lw, lh));
    let window = WaylandWindow::new(mtm, rect, NSWindowStyleMask::Borderless);
    unsafe { window.setReleasedWhenClosed(false) };
    window.setHasShadow(false);
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    // Float above normal windows like a bar/dock, and show on every Space
    // (including over fullscreen apps) so it behaves like a real menu/dock bar.
    window.setLevel(NSStatusWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::Stationary,
    );

    let view = WaylandView::new(
        mtm,
        id,
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(lw, lh)),
    );
    let root = CALayer::new();
    root.setContentsScale(scale as f64);
    root.setOpaque(false);
    root.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(lw, lh)));
    view.setLayer(Some(&root));
    view.setWantsLayer(true);
    window.setAcceptsMouseMovedEvents(true);
    window.setContentView(Some(&view));
    if keyboard {
        // A launcher (fuzzel) needs keystrokes: make it key + first responder and
        // activate the app so macOS routes keyDown to the view (which forwards to
        // the focused Wayland surface).
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&view));
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    } else {
        // A plain bar shouldn't steal focus from apps.
        window.orderFront(None);
    }

    WINDOWS.with(|w| {
        w.borrow_mut().insert(
            id,
            WinEntry {
                window,
                view,
                _delegate: None,
                cur_w: lw as i32,
                cur_h: lh as i32,
                decorated: false,
                csd_margin: (0, 0),
                sublayers: HashMap::new(),
            },
        );
    });
    info!(target: "mac", "created layer window {id} ({lw}x{lh} pts) anchor={anchor}");
}

/// Resolve one axis of `xdg_positioner` constraint adjustment. `pos` is the
/// popup's un-flipped start on this axis, `flipped` the start with the anchor +
/// gravity inverted, `size` its extent, and `[min, max]` the visible bounds. When
/// `flip` is allowed and `pos` falls off an edge while `flipped` is fully
/// on-screen, the flipped placement wins. A final slide keeps the popup on-screen
/// regardless (pinning to the min edge if it is larger than the work area),
/// matching the popup's original always-on-screen guarantee.
fn constrain_axis(pos: f64, flipped: f64, size: f64, min: f64, max: f64, flip: bool) -> f64 {
    let off_screen = |p: f64| p < min || p + size > max;
    let mut pos = if flip && off_screen(pos) && !off_screen(flipped) {
        flipped
    } else {
        pos
    };
    // Slide back in: pin the far edge, then the near edge (near wins if oversized).
    if pos + size > max {
        pos = max - size;
    }
    if pos < min {
        pos = min;
    }
    pos
}

/// Create a borderless popup window (menu/dropdown) as a child of its parent,
/// positioned at `(x, y)` logical points from the parent surface's top-left.
/// `x_flip`/`y_flip` are the same origin with the positioner's anchor+gravity
/// inverted per axis, and `constraint` its `set_constraint_adjustment` bitmask —
/// used to flip/slide the popup back on-screen.
#[allow(clippy::too_many_arguments)]
fn create_popup(
    mtm: MainThreadMarker,
    id: u32,
    parent_id: u32,
    x: i32,
    y: i32,
    x_flip: i32,
    y_flip: i32,
    constraint: u32,
    width: i32,
    height: i32,
) {
    let scale = crate::input::scale();
    let lw = (width / scale).max(1) as f64;
    let lh = (height / scale).max(1) as f64;

    // Screen origins from the parent (macOS frames are bottom-left origin): the
    // requested placement and its per-axis flipped alternative. The positioner
    // coordinates are relative to the parent *surface* — i.e. the content view —
    // so anchor to the content rect, not the whole window frame. For a decorated
    // (titled) window the content view sits below the native titlebar; using the
    // frame would push every popup up by the titlebar height.
    let (mut origin, flip_origin, parent_screen, dbg) = WINDOWS
        .with(|w| {
            w.borrow().get(&parent_id).map(|p| {
                let frame = p.window.frame();
                let f = p.window.contentRectForFrameRect(frame);
                let top = f.origin.y + f.size.height;
                (
                    CGPoint::new(f.origin.x + x as f64, top - y as f64 - lh),
                    CGPoint::new(f.origin.x + x_flip as f64, top - y_flip as f64 - lh),
                    p.window.screen(),
                    (frame, f, p.decorated),
                )
            })
        })
        .unwrap_or((
            CGPoint::new(200.0, 200.0),
            CGPoint::new(200.0, 200.0),
            None,
            (CGRect::default(), CGRect::default(), false),
        ));

    // Constraint adjustment: a client's xdg_positioner asks the compositor to
    // flip/slide a menu that would fall off a screen edge. We prefer the flipped
    // placement when the requested one runs off-screen (e.g. Konsole's hamburger
    // menu near the right edge opens down-left, aligned under the button, instead
    // of running off the right), then slide as a fallback so the whole popup stays
    // visible. Without flipping, an edge-anchored menu opens on the wrong side of
    // its button.
    let screen = parent_screen.or_else(|| NSScreen::mainScreen(mtm));
    if let Some(s) = screen {
        let vf = s.visibleFrame();
        const FLIP_X: u32 = 4;
        const FLIP_Y: u32 = 8;
        origin.x = constrain_axis(
            origin.x,
            flip_origin.x,
            lw,
            vf.origin.x,
            vf.origin.x + vf.size.width,
            constraint & FLIP_X != 0,
        );
        origin.y = constrain_axis(
            origin.y,
            flip_origin.y,
            lh,
            vf.origin.y,
            vf.origin.y + vf.size.height,
            constraint & FLIP_Y != 0,
        );
    }

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
                decorated: false,
                csd_margin: (0, 0),
                sublayers: HashMap::new(),
            },
        );
    });
    // A menu is open: pin keyboard focus to it (see POPUP_WINDOWS / mouse_exited).
    POPUP_WINDOWS.with(|p| {
        p.borrow_mut().insert(id);
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
    let (pframe, pcontent, pdecorated) = dbg;
    info!(
        target: "mac",
        "created popup {id} under {parent_id}: requested pt=({x},{y}) flip=({x_flip},{y_flip}) \
         constraint={constraint:#b} scale={scale} logical={lw:.0}x{lh:.0} | parent decorated={pdecorated} \
         frame=({:.0},{:.0} {:.0}x{:.0}) content=({:.0},{:.0} {:.0}x{:.0}) -> popup screen origin=({:.0},{:.0})",
        pframe.origin.x, pframe.origin.y, pframe.size.width, pframe.size.height,
        pcontent.origin.x, pcontent.origin.y, pcontent.size.width, pcontent.size.height,
        origin.x, origin.y,
    );
}

#[allow(clippy::too_many_arguments)]
fn present_frame(
    mtm: MainThreadMarker,
    id: u32,
    width: i32,
    height: i32,
    stride: i32,
    dst_w: i32,
    dst_h: i32,
    pixels: &[u8],
    format: PixelFormat,
    color: Option<ColorDesc>,
    geom: (i32, i32, i32, i32),
) {
    let Some(image) = make_cgimage(width, height, stride, pixels, format, color) else {
        error!(target: "mac", "failed to build CGImage for {id}");
        return;
    };
    let scale = crate::input::scale();
    let margin = csd_margin(width, height, dst_w, dst_h, geom, scale);

    WINDOWS.with(|w| {
        let mut map = w.borrow_mut();
        let Some(entry) = map.get_mut(&id) else { return };
        // Track the current CSD shadow margin so windowDidResize can subtract it
        // from the size it asks the client to paint (avoids the resize growth loop).
        entry.csd_margin = margin;

        // Resize the window whenever its logical size changes. The image is built
        // from the raw buffer (physical px) and scaled to fill; the window itself
        // is sized in points, from the viewport destination or buffer/scale.
        let (lw, lh) = logical_size(width, height, dst_w, dst_h, scale);
        // During a native live resize (a decorated window's edge drag) macOS owns
        // the window size — following the mouse — while the client repaints via the
        // configure we sent in windowDidResize. Setting the size from the buffer
        // here would fight that and make the window flicker between sizes.
        // A server-decorated window's size is macOS/user-authoritative: the client
        // renders to match the configure we send on windowDidResize, and its buffer
        // lags a frame — so resizing the window from the buffer here would bounce it
        // between the two in-flight sizes forever. Just fill content; never resize.
        let live_resize = entry.decorated || entry.window.inLiveResize();
        // cur_w/cur_h track the logical point size so a viewport-only size change
        // (buffer pixels unchanged) still triggers a resize.
        if !live_resize && (entry.cur_w != lw as i32 || entry.cur_h != lh as i32) {

            // During an interactive resize the window follows the buffer that
            // the client actually painted — so content never stretches — and we
            // keep the corner opposite the dragged edge fixed. Outside a resize,
            // a plain content-size change keeps the origin put.
            let resize_info = RESIZE.with(|r| {
                r.borrow()
                    .as_ref()
                    .filter(|s| s.window_id == id)
                    .map(|s| (s.edges, s.anchor_frame))
            });
            if let Some((edges, anchor)) = resize_info {
                let mut ox = anchor.origin.x;
                let mut oy = anchor.origin.y;
                if edges & 4 != 0 {
                    ox = anchor.origin.x + anchor.size.width - lw; // left: pin right
                }
                if edges & 2 != 0 {
                    oy = anchor.origin.y + anchor.size.height - lh; // bottom: pin top
                }
                entry.window.setFrame_display(
                    CGRect::new(CGPoint::new(ox, oy), CGSize::new(lw, lh)),
                    true,
                );
            } else {
                entry.window.setContentSize(CGSize::new(lw, lh));
            }
            entry.cur_w = lw as i32;
            entry.cur_h = lh as i32;
        }
        let _ = mtm;

        if let Some(layer) = entry.view.layer() {
            // Opt the layer into EDR when the content is HDR, so highlights above
            // SDR white use the display's extended headroom instead of clipping.
            set_layer_hdr(&entry.window, &layer, color);
            // Keep the hosted root layer sized to the view's ACTUAL current bounds
            // (AppKit won't size a layer-hosting layer for us). Using the live view
            // size — not the stale cur_w/cur_h — means the content fills the window
            // smoothly during a native live resize instead of lagging behind it.
            // Disable implicit animations so it applies instantly (no resize jitter).
            let b = entry.view.bounds();
            let img_ptr: *const CGImage = &*image;
            let obj: &AnyObject = unsafe { &*(img_ptr as *const AnyObject) };
            without_animations(|| {
                layer.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), b.size));
                // A CGImageRef is a CFType that responds to retain/release, so it
                // can be handed to `-[CALayer setContents:]` (typed as `id`).
                unsafe { layer.setContents(Some(obj)) };
            });
        }
    });
}

/// Toggle Extended Dynamic Range on a layer to match its content. HDR content
/// (PQ/HLG transfer) asks the layer for the display's EDR headroom so highlights
/// exceed SDR white; SDR content clears the flag so a window that stops sending
/// HDR returns to normal. Logs the display's headroom once — on a Mac/display
/// with no EDR (headroom ≤ 1.0) the flag is harmless and the wide-gamut image
/// simply tone-maps to SDR.
fn set_layer_hdr(window: &NSWindow, layer: &CALayer, color: Option<ColorDesc>) {
    let hdr = color.map(|c| c.is_hdr()).unwrap_or(false);
    // `wantsExtendedDynamicRangeContent` is the broadly-available EDR opt-in
    // (its deprecation just points at the newer `preferredDynamicRange`).
    #[allow(deprecated)]
    if layer.wantsExtendedDynamicRangeContent() != hdr {
        #[allow(deprecated)]
        layer.setWantsExtendedDynamicRangeContent(hdr);
        if hdr {
            let headroom = window
                .screen()
                .map(|s| s.maximumPotentialExtendedDynamicRangeColorComponentValue())
                .unwrap_or(1.0);
            info!(target: "mac", "EDR enabled; display headroom {headroom:.2}x SDR white");
        }
    }
}

/// Run `f` with implicit CALayer animations disabled, so geometry/content changes
/// apply instantly. Without this, every `setFrame`/`setContents` triggers a ~0.25s
/// implicit animation that lags and jitters during a live window resize.
fn without_animations<R>(f: impl FnOnce() -> R) -> R {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    let r = f();
    CATransaction::commit();
    r
}

/// Composite a subsurface as a CALayer sublayer of a window's view. The image is
/// the subsurface buffer (physical px); the sublayer is placed at the subsurface
/// position (logical points, converted from Wayland top-left to CALayer y-up).
#[allow(clippy::too_many_arguments)]
fn present_subframe(
    window_id: u32,
    sub_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    stride: i32,
    dst_w: i32,
    dst_h: i32,
    pixels: &[u8],
    format: PixelFormat,
    color: Option<ColorDesc>,
) {
    let Some(image) = make_cgimage(width, height, stride, pixels, format, color) else {
        return;
    };
    let scale = crate::input::scale();
    let (lw, lh) = logical_size(width, height, dst_w, dst_h, scale);

    WINDOWS.with(|w| {
        let mut map = w.borrow_mut();
        let Some(entry) = map.get_mut(&window_id) else { return };
        let Some(root) = entry.view.layer() else { return };
        // Use the LIVE view height, not the cached cur_h: for a server-decorated
        // window (KWin) present_frame stops maintaining cur_h during a user resize,
        // so cur_h goes stale and the subsurface would be flipped against the wrong
        // height — leaving a black bar where KWin's output no longer reaches.
        let win_h = entry.view.bounds().size.height;

        let sublayer = entry
            .sublayers
            .entry(sub_id)
            .or_insert_with(|| {
                let l = CALayer::new();
                root.addSublayer(&l);
                l
            })
            .clone();

        // Wayland (x,y) is a top-left offset; the view's layer is y-up.
        let (_, oy) = subsurface_origin(win_h, x, y, lh);
        let img_ptr: *const CGImage = &*image;
        let obj: &AnyObject = unsafe { &*(img_ptr as *const AnyObject) };
        set_layer_hdr(&entry.window, &sublayer, color);
        // Disable implicit animations so a moving/updating subsurface (e.g. KWin's
        // cursor) tracks instantly rather than animating behind the pointer.
        without_animations(|| {
            sublayer.setContentsScale(scale as f64);
            sublayer.setFrame(CGRect::new(CGPoint::new(x as f64, oy), CGSize::new(lw, lh)));
            unsafe { sublayer.setContents(Some(obj)) };
        });
    });
}

/// The Core Graphics color space a frame should be tagged with, chosen from its
/// pixel format and negotiated color description. Pure (no CG calls) so the
/// mapping can be unit-tested headlessly; `cg_colorspace` turns it into a real
/// `CGColorSpace`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum CgSpace {
    /// Plain device RGB — ordinary sRGB SDR, the default before HDR existed.
    DeviceRgb,
    /// Wide-gamut SDR (Display P3).
    DisplayP3,
    /// HDR BT.2100 PQ (ST 2084) — 10-bit integer content.
    Pq2100,
    /// HDR BT.2100 HLG.
    Hlg2100,
    /// Extended-linear Display P3 — float content, values > 1.0 map to EDR.
    ExtLinearP3,
    /// Extended-linear BT.2020 — float content with a wide gamut.
    ExtLinear2020,
}

/// Pick the color space for a frame. Float (`Rgba16F`) content is linear light,
/// so it always uses an extended-linear space (values above 1.0 are the HDR
/// headroom). Integer content is tagged by its transfer function / primaries.
fn cg_space_for(format: PixelFormat, color: Option<ColorDesc>) -> CgSpace {
    match format {
        PixelFormat::Rgba16F => match color.map(|c| c.primaries) {
            Some(Primaries::Bt2020) => CgSpace::ExtLinear2020,
            _ => CgSpace::ExtLinearP3,
        },
        _ => match color {
            Some(c) if c.tf == TransferFn::Pq => CgSpace::Pq2100,
            Some(c) if c.tf == TransferFn::Hlg => CgSpace::Hlg2100,
            Some(c) if c.primaries != Primaries::Srgb => CgSpace::DisplayP3,
            _ => CgSpace::DeviceRgb,
        },
    }
}

fn cg_colorspace(space: CgSpace) -> Option<objc2_core_foundation::CFRetained<CGColorSpace>> {
    // The name statics are `extern "C"` globals; dereferencing them is unsafe.
    let name = unsafe {
        match space {
            CgSpace::DeviceRgb => return CGColorSpace::new_device_rgb(),
            CgSpace::DisplayP3 => kCGColorSpaceDisplayP3,
            CgSpace::Pq2100 => kCGColorSpaceITUR_2100_PQ,
            CgSpace::Hlg2100 => kCGColorSpaceITUR_2100_HLG,
            CgSpace::ExtLinearP3 => kCGColorSpaceExtendedLinearDisplayP3,
            CgSpace::ExtLinear2020 => kCGColorSpaceExtendedLinearITUR_2020,
        }
    };
    CGColorSpace::with_name(Some(name))
}

/// Bytes per pixel for a format.
fn bytes_per_pixel(format: PixelFormat) -> i32 {
    match format {
        PixelFormat::Bgra8888 | PixelFormat::Rgb2101010 => 4,
        PixelFormat::Rgba16F => 8,
    }
}

/// Build a CGImage from a raw buffer (row 0 = top). For ordinary SDR BGRA8888 it
/// copies through a bitmap context (as it always has). For wide-gamut / HDR
/// content it builds the image directly with `CGImageCreate` over a `CFData`
/// copy of the buffer, tagged with a matching `CGColorSpace` — a bitmap context
/// can't express 10-bit packing or a PQ/extended-linear space.
// The `CGBitmapInfo` byte-order/float flags are marked deprecated in the objc2
// binding but are the documented way to describe the packing to `CGImageCreate`.
#[allow(deprecated)]
fn make_cgimage(
    width: i32,
    height: i32,
    stride: i32,
    pixels: &[u8],
    format: PixelFormat,
    color: Option<ColorDesc>,
) -> Option<objc2_core_foundation::CFRetained<CGImage>> {
    let bpp = bytes_per_pixel(format);
    if width <= 0 || height <= 0 || stride < width * bpp {
        return None;
    }
    let needed = (stride as usize) * (height as usize);
    if pixels.len() < needed {
        return None;
    }

    let space = cg_space_for(format, color);

    // Fast path: plain SDR BGRA8888 through a bitmap context (unchanged).
    if format == PixelFormat::Bgra8888 && space == CgSpace::DeviceRgb {
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
        return CGBitmapContextCreateImage(Some(&ctx));
    }

    // Wide-gamut / HDR path: CGImageCreate over a CFData copy of the buffer.
    //
    // (bits/component, bits/pixel, bitmap flags, row stride, bytes). Alpha bits go
    // in the low 5 bits of the bitmap info (CGImageAlphaInfo). Core Graphics does
    // NOT accept a 10-bit/32-bpp RGB image, so 10-bit content is expanded to a
    // 16-bit RGBA image (a documented 64-bpp layout) keeping its color space.
    let color_space = cg_colorspace(space)?;
    let (bpc, bppx, info, row_stride, owned): (usize, usize, CGBitmapInfo, usize, Vec<u8>) =
        match format {
            PixelFormat::Rgb2101010 => (
                16,
                64,
                CGBitmapInfo::from_bits_retain(CGImageAlphaInfo::NoneSkipLast.0)
                    | CGBitmapInfo::ByteOrder16Little,
                (width as usize) * 8,
                expand_2101010_to_rgba16(width, height, stride, pixels),
            ),
            // 64-bit four half-floats; Wayland abgr16161616f is R,G,B,A in memory
            // (little-endian) → alpha last, 16-bit LE order, float components.
            PixelFormat::Rgba16F => (
                16,
                64,
                CGBitmapInfo::from_bits_retain(CGImageAlphaInfo::PremultipliedLast.0)
                    | CGBitmapInfo::ByteOrder16Little
                    | CGBitmapInfo::FloatComponents,
                stride as usize,
                pixels[..needed].to_vec(),
            ),
            // Wide-gamut 8-bit (e.g. Display P3 SDR): same packing as the SDR path.
            PixelFormat::Bgra8888 => (
                8,
                32,
                CGBitmapInfo::from_bits_retain(CGImageAlphaInfo::PremultipliedFirst.0)
                    | CGBitmapInfo::ByteOrder32Little,
                stride as usize,
                pixels[..needed].to_vec(),
            ),
        };

    let data = unsafe {
        CFData::new(
            None,
            owned.as_ptr(),
            owned.len() as objc2_core_foundation::CFIndex,
        )
    }?;
    let provider = CGDataProvider::with_cf_data(Some(&data))?;

    unsafe {
        CGImage::new(
            width as usize,
            height as usize,
            bpc,
            bppx,
            row_stride,
            Some(&color_space),
            info,
            Some(&provider),
            null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
}

/// Expand a Wayland `argb2101010`/`xrgb2101010` buffer (32-bit LE, `A:R:G:B`
/// 2:10:10:10) into a 16-bit-per-channel RGBA buffer (little-endian, alpha
/// opaque). Core Graphics can't ingest 10-bit RGB directly, but 16-bit RGBA is a
/// supported 64-bpp layout; the 10→16 bit expansion (`v<<6 | v>>4`) fills the
/// range so the color-space tag (PQ/HLG/…) still sees the intended signal.
fn expand_2101010_to_rgba16(width: i32, height: i32, stride: i32, pixels: &[u8]) -> Vec<u8> {
    let (w, h, s) = (width as usize, height as usize, stride as usize);
    let mut out = vec![0u8; w * h * 8];
    for y in 0..h {
        for x in 0..w {
            let i = y * s + x * 4;
            let v = u32::from_le_bytes([pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]);
            let expand = |c: u32| -> [u8; 2] { (((c << 6) | (c >> 4)) as u16).to_le_bytes() };
            let r = expand((v >> 20) & 0x3ff);
            let g = expand((v >> 10) & 0x3ff);
            let b = expand(v & 0x3ff);
            let o = (y * w + x) * 8;
            out[o..o + 2].copy_from_slice(&r);
            out[o + 2..o + 4].copy_from_slice(&g);
            out[o + 4..o + 6].copy_from_slice(&b);
            out[o + 6..o + 8].copy_from_slice(&0xffffu16.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        bytes_per_pixel, cg_space_for, constrain_axis, csd_margin, logical_size, make_cgimage,
        subsurface_origin, CgSpace, ColorDesc, PixelFormat, Primaries, TransferFn,
    };

    /// `constrain_axis` implements the `xdg_positioner` flip/slide adjustment on
    /// one axis. Screen bounds are `[0, 1000]` throughout; the popup is 300 wide.
    #[test]
    fn constrain_axis_flips_and_slides() {
        // Fits already: no change (flipped candidate ignored).
        assert_eq!(constrain_axis(100.0, 700.0, 300.0, 0.0, 1000.0, true), 100.0);

        // Requested placement runs off the right edge (850..1150); the flipped
        // candidate (550..850) is fully on-screen, so it wins — a right-edge menu
        // opening leftward under its button.
        assert_eq!(constrain_axis(850.0, 550.0, 300.0, 0.0, 1000.0, true), 550.0);

        // Same overflow but flip not allowed: fall back to a slide that pins the
        // far edge (1000 - 300 = 700).
        assert_eq!(constrain_axis(850.0, 550.0, 300.0, 0.0, 1000.0, false), 700.0);

        // Requested runs off-screen and the flipped candidate is *also* off-screen
        // (e.g. -100): keep the requested side and slide it in (1000 - 600 = 400).
        assert_eq!(constrain_axis(500.0, -100.0, 600.0, 0.0, 1000.0, true), 400.0);

        // Larger than the work area: pin to the near (min) edge.
        assert_eq!(constrain_axis(-50.0, -50.0, 1200.0, 0.0, 1000.0, false), 0.0);
    }

    fn desc(tf: TransferFn, primaries: Primaries) -> ColorDesc {
        ColorDesc {
            tf,
            primaries,
            max_luminance: None,
            ref_luminance: None,
        }
    }

    /// `make_cgimage` must actually produce a `CGImage` for every supported layout,
    /// not just the SDR one. Core Graphics is picky about which (bits, bpp, alpha,
    /// byte-order, color-space) combinations it accepts and returns null for the
    /// rest — this exercises the real `CGImageCreate` headlessly (no AppKit) so a
    /// bad HDR pixel-format spec is caught here instead of as a blank window.
    #[test]
    fn make_cgimage_builds_every_supported_format() {
        // Plain SDR BGRA (the historical path).
        let buf4 = vec![0u8; 16 * 16 * 4];
        assert!(
            make_cgimage(16, 16, 16 * 4, &buf4, PixelFormat::Bgra8888, None).is_some(),
            "SDR BGRA8888"
        );
        // Wide-gamut SDR (Display P3, 8-bit).
        assert!(
            make_cgimage(
                16,
                16,
                16 * 4,
                &buf4,
                PixelFormat::Bgra8888,
                Some(desc(TransferFn::Srgb, Primaries::DisplayP3))
            )
            .is_some(),
            "Display-P3 8-bit"
        );
        // 10-bit PQ (BT.2100).
        assert!(
            make_cgimage(
                16,
                16,
                16 * 4,
                &buf4,
                PixelFormat::Rgb2101010,
                Some(desc(TransferFn::Pq, Primaries::Bt2020))
            )
            .is_some(),
            "10-bit PQ"
        );
        // float16, linear (extended-range EDR).
        let buf8 = vec![0u8; 16 * 16 * 8];
        assert!(
            make_cgimage(
                16,
                16,
                16 * 8,
                &buf8,
                PixelFormat::Rgba16F,
                Some(desc(TransferFn::Srgb, Primaries::Bt2020))
            )
            .is_some(),
            "float16 linear BT.2020"
        );
    }

    /// The transfer-function / primaries → Core Graphics color space mapping that
    /// `make_cgimage` relies on (pure; no AppKit/CG involved).
    #[test]
    fn cg_space_maps_transfer_and_primaries() {
        // No color description + 8-bit → plain device RGB (the SDR default).
        assert_eq!(
            cg_space_for(PixelFormat::Bgra8888, None),
            CgSpace::DeviceRgb
        );
        // PQ / HLG → the BT.2100 HDR spaces regardless of the (10-bit) format.
        assert_eq!(
            cg_space_for(PixelFormat::Rgb2101010, Some(desc(TransferFn::Pq, Primaries::Bt2020))),
            CgSpace::Pq2100
        );
        assert_eq!(
            cg_space_for(PixelFormat::Rgb2101010, Some(desc(TransferFn::Hlg, Primaries::Bt2020))),
            CgSpace::Hlg2100
        );
        // Wide-gamut SDR (Display P3, sRGB transfer) → Display P3.
        assert_eq!(
            cg_space_for(PixelFormat::Bgra8888, Some(desc(TransferFn::Srgb, Primaries::DisplayP3))),
            CgSpace::DisplayP3
        );
        // Float content is linear light → an extended-linear space (EDR headroom).
        assert_eq!(
            cg_space_for(PixelFormat::Rgba16F, Some(desc(TransferFn::Srgb, Primaries::Bt2020))),
            CgSpace::ExtLinear2020
        );
        assert_eq!(
            cg_space_for(PixelFormat::Rgba16F, None),
            CgSpace::ExtLinearP3
        );
    }

    /// Bytes-per-pixel per format (10-bit stays 4 bytes; float16 is 8).
    #[test]
    fn bytes_per_pixel_per_format() {
        assert_eq!(bytes_per_pixel(PixelFormat::Bgra8888), 4);
        assert_eq!(bytes_per_pixel(PixelFormat::Rgb2101010), 4);
        assert_eq!(bytes_per_pixel(PixelFormat::Rgba16F), 8);
    }

    /// A `wp_viewport` destination wins over the buffer size; otherwise the buffer
    /// is divided by the output scale.
    #[test]
    fn logical_size_prefers_viewport_then_scales_buffer() {
        assert_eq!(logical_size(2, 2, 1280, 800, 2), (1280.0, 800.0));
        assert_eq!(logical_size(800, 600, 0, 0, 2), (400.0, 300.0));
    }

    /// CSD margin is the buffer minus the client's window geometry, and is zero
    /// when a viewport is in use or no geometry was set.
    #[test]
    fn csd_margin_is_buffer_minus_geometry() {
        // 420x320 buffer, 400x300 content geometry, scale 1 -> 20x20 shadow.
        assert_eq!(csd_margin(420, 320, 0, 0, (10, 10, 400, 300), 1), (20, 20));
        // Viewport in use -> no margin.
        assert_eq!(csd_margin(420, 320, 1280, 800, (10, 10, 400, 300), 1), (0, 0));
    }

    /// Regression for the "black bar on resize" bug (KWin renders its whole
    /// desktop into one full-output subsurface). With the *live* window height, a
    /// full-output subsurface at (0,0) covers the window exactly — no black gap.
    /// With a *stale* (pre-resize) height it lands low and leaves the top
    /// uncovered, which is what showed the black bar. This pins the invariant the
    /// AppKit `present_subframe` must satisfy; the real on-screen check is
    /// scripts/e2e-resize-kwin.sh.
    #[test]
    fn subsurface_full_output_covers_resized_window() {
        // Window doubled to 600pt tall; KWin repaints its output at the new size.
        let win_h = 600.0;
        let (ox, oy) = subsurface_origin(win_h, 0, 0, 600.0);
        assert_eq!((ox, oy), (0.0, 0.0), "full-output subsurface sits at the origin");
        // Covered vertical span is [oy, oy + lh]; it must span the whole window.
        assert!(
            oy <= 0.0 && oy + 600.0 >= win_h,
            "the subsurface covers the full window height (no black gap)"
        );

        // The bug: flipping the same full-output subsurface against the stale
        // pre-resize height (300) leaves [300, 600] at the top uncovered -> black.
        let (_, oy_stale) = subsurface_origin(300.0, 0, 0, 600.0);
        assert!(
            oy_stale + 600.0 < win_h,
            "a stale height leaves an uncovered (black) region at the top"
        );
    }
}
