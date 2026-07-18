//! RAIL windowing *policy*, extracted from the C bridge so it is unit-testable.
//!
//! The RDP/RAIL bridge (`csrc/rail_bridge.c`) is a thin transport: it decodes
//! window orders and rdpgfx surface updates off the wire and asks this module the
//! questions that used to be answered by ad-hoc C state:
//!
//!   1. **Classification** — should this window order become a real NSWindow, or
//!      is it an auxiliary window we must not mirror? (`window_order`)
//!   2. **Routing** — a surface carrying pixels arrived; which mirrored window do
//!      those pixels belong to? (`resolve`)
//!
//! Keeping this in Rust means the existing `cargo test --bin wayland-macos`
//! harness exercises it headlessly (no FreeRDP, no RDP server, no AppKit) — the
//! same reason `csd_margin`/`logical_size` live as pure functions in `mac.rs`.
//! The C side calls the `#[no_mangle]` wrappers at the bottom; the pure `Router`
//! is what the tests drive.
//!
//! ## The two bugs this fixes
//!
//! **Tooltip-over-content.** The old C router, when a surface had neither an
//! explicit WSLg map nor its own `windowId`, fell back to `g_main_window_id` —
//! "the most recently created window." With a second window in play (a tooltip, a
//! sub-surface) an unmapped surface smeared its pixels onto whatever window was
//! newest. `resolve` here honours that ownerless fallback only when there is
//! **exactly one** window, so an ambiguous surface is dropped, not misrouted.
//!
//! **The Firefox double ("shadow") window.** weston's rdprail-shell remotes a
//! GTK/CSD app as *two* RAIL windows: a mostly-transparent toplevel carrying the
//! drop-shadow/frame, which **owns** a nearly-full-size child window that holds
//! the actual opaque content (verified from live weston: the frame is ~95%
//! transparent, the child ~99% opaque and geometrically inset inside the frame).
//! Mirroring both spawns an empty phantom next to the real window. The frame is
//! not marked with any tool/popup style bit — the only reliable signal is
//! structural: it **owns a child that covers ≥80% of it**. So a window with such
//! a covering child is classified as a frame and dropped; the covering child is
//! kept. A real dialog is much smaller than its owner, so it never triggers this.

use std::collections::BTreeMap;

/// Win32 extended-style bits weston sets on RAIL window orders (mirrors the
/// `WS_EX_*` constants in `<freerdp/window.h>`; kept here so the policy needs no
/// FreeRDP headers and stays testable without the `rail` feature).
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

/// weston reports "no owner" as either 0 or 0xFFFFFFFF depending on the order;
/// normalise both to "unowned".
const OWNER_NONE_A: u32 = 0;
const OWNER_NONE_B: u32 = u32::MAX;

/// A covering child must fill at least this fraction of its owner's area for the
/// owner to be treated as a redundant frame. Firefox's content child covers ~92%;
/// a dialog covers far less. 0.8 leaves margin on both sides.
const FRAME_COVER_RATIO: f64 = 0.8;

/// Slack (in RAIL desktop px) when testing whether a child sits inside its owner —
/// the shadow frame extends a few px beyond the content on every side.
const CONTAIN_SLACK: i32 = 8;

/// Whether a window is mirrored to a native NSWindow or suppressed as an
/// auxiliary (frame/tooltip/tool) window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Mirror,
    Filtered,
}

/// A window's RAIL desktop rectangle (physical px, top-left origin).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn area(&self) -> i64 {
        (self.w.max(0) as i64) * (self.h.max(0) as i64)
    }

    /// True if `self` fills ≥ `FRAME_COVER_RATIO` of `owner` and sits (roughly)
    /// inside it — i.e. `self` is the content and `owner` is a frame around it.
    fn covers(&self, owner: &Rect) -> bool {
        if owner.area() == 0 || self.area() == 0 {
            return false;
        }
        if (self.area() as f64) < FRAME_COVER_RATIO * (owner.area() as f64) {
            return false;
        }
        let t = CONTAIN_SLACK;
        self.x >= owner.x - t
            && self.y >= owner.y - t
            && self.x + self.w <= owner.x + owner.w + t
            && self.y + self.h <= owner.y + owner.h + t
    }
}

#[derive(Clone, Copy)]
struct Win {
    kind: Kind,
    owner: Option<u32>,
    rect: Rect,
}

/// Outcome of feeding a window order through [`Router::window_order`]. The C
/// bridge maps these onto "emit a create", "emit nothing", or "emit a delete then
/// nothing" so create/update/promotion all funnel through one decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order {
    /// A newly mirrored window — the bridge should emit `window_create`.
    NewMirror,
    /// An already-known mirrored window — a plain geometry/title update.
    KnownMirror,
    /// A window that was mirrored and is now reclassified as auxiliary — the
    /// bridge should destroy the NSWindow it created.
    NowFiltered,
    /// An auxiliary window (still) suppressed — the bridge emits nothing.
    Filtered,
}

/// Classify a window purely from its RAIL style bits. `WS_EX_TOOLWINDOW` is the
/// canonical "not a primary app window" flag; an *owned* window that is also
/// `WS_EX_NOACTIVATE` is a non-focusable helper (shadow/tooltip). Owned-but-
/// activatable windows (real modal dialogs) are kept.
fn is_decoration(exstyle: u32, owner: Option<u32>) -> bool {
    if exstyle & WS_EX_TOOLWINDOW != 0 {
        return true;
    }
    if owner.is_some() && exstyle & WS_EX_NOACTIVATE != 0 {
        return true;
    }
    false
}

fn normalize_owner(owner: u32) -> Option<u32> {
    if owner == OWNER_NONE_A || owner == OWNER_NONE_B {
        None
    } else {
        Some(owner)
    }
}

/// All routing state for one RAIL session: which windows exist (kind, owner,
/// geometry) and which rdpgfx surface maps to which window.
#[derive(Default)]
pub struct Router {
    windows: BTreeMap<u32, Win>,
    surf_map: BTreeMap<u16, u32>,
    /// Windows that were mirrored and have since been reclassified as frames — the
    /// bridge drains this to destroy their NSWindows (see [`Router::pop_demoted`]).
    demoted: Vec<u32>,
}

impl Router {
    pub fn new() -> Self {
        Router::default()
    }

    /// True if some other window is an owned child that covers `id` — i.e. `id`
    /// is a frame wrapped around its content child.
    fn has_covering_child(&self, id: u32) -> bool {
        let parent = match self.windows.get(&id) {
            Some(w) => w.rect,
            None => return false,
        };
        self.windows.iter().any(|(&cid, c)| {
            cid != id && c.owner == Some(id) && c.rect.covers(&parent)
        })
    }

    /// Feed a window order. `style` is `Some((extended_style, owner))` when the
    /// order carried the STYLE field, `None` for a geometry-only order (which
    /// never reclassifies: an unknown window fails open to `Mirror`, a known one
    /// keeps its kind). `rect` is the window's RAIL desktop rectangle.
    pub fn window_order(&mut self, id: u32, style: Option<(u32, u32)>, rect: (i32, i32, i32, i32)) -> Order {
        let rect = Rect { x: rect.0, y: rect.1, w: rect.2, h: rect.3 };
        let prev = self.windows.get(&id).map(|w| w.kind);
        let owner = style.and_then(|(_, o)| normalize_owner(o)).or_else(|| {
            // Keep a previously-learned owner across geometry-only orders.
            self.windows.get(&id).and_then(|w| w.owner)
        });
        self.windows.insert(id, Win { kind: prev.unwrap_or(Kind::Mirror), owner, rect });

        // Classify this window. Style bits first (tool/tooltip), then the
        // structural frame test (owns a covering child). Absent style + known
        // window keeps its kind.
        let kind = match style {
            Some((exstyle, _)) if is_decoration(exstyle, owner) => Kind::Filtered,
            _ if self.has_covering_child(id) => Kind::Filtered,
            None if prev.is_some() => prev.unwrap(),
            _ => Kind::Mirror,
        };
        self.windows.get_mut(&id).unwrap().kind = kind;

        // Adding this window may have turned its OWNER into a frame (the covering
        // child usually arrives after the frame). Demote a now-covered owner that
        // we had already mirrored so the bridge destroys its phantom NSWindow.
        if let Some(p) = owner {
            if self.windows.get(&p).map(|w| w.kind) == Some(Kind::Mirror) && self.has_covering_child(p) {
                self.windows.get_mut(&p).unwrap().kind = Kind::Filtered;
                self.demoted.push(p);
            }
        }

        match (prev, kind) {
            (Some(Kind::Mirror), Kind::Mirror) => Order::KnownMirror,
            (_, Kind::Mirror) => Order::NewMirror, // new, or promoted from Filtered
            (Some(Kind::Mirror), Kind::Filtered) => Order::NowFiltered,
            (_, Kind::Filtered) => Order::Filtered,
        }
    }

    /// Pop the next window the router decided to demote from mirrored to frame, or
    /// `None` when the queue is drained. The bridge destroys each one's NSWindow.
    pub fn pop_demoted(&mut self) -> Option<u32> {
        self.demoted.pop()
    }

    /// A window was deleted. Returns whether it had been mirrored (so the bridge
    /// only emits a `window_delete` for a window the Rust side actually created).
    pub fn window_deleted(&mut self, id: u32) -> bool {
        let was_mirror = self.windows.remove(&id).map(|w| w.kind) == Some(Kind::Mirror);
        self.surf_map.retain(|_, &mut w| w != id);
        self.demoted.retain(|&d| d != id);
        was_mirror
    }

    /// Record the WSLg `MapWindowForSurface` association.
    pub fn map_surface(&mut self, surface_id: u16, window_id: u32) {
        self.surf_map.insert(surface_id, window_id);
    }

    /// Drop every surface mapping for a window (`UnmapWindowForSurface`).
    pub fn unmap_window(&mut self, window_id: u32) {
        self.surf_map.retain(|_, &mut w| w != window_id);
    }

    /// Resolve a surface's pixels to the mirrored window that should show them,
    /// or `None` to drop them. Order of preference: the explicit WSLg map, then
    /// the surface's own `windowId`, then — only when there is exactly one window
    /// so there is no ambiguity — that sole window. A surface that resolves to a
    /// filtered (auxiliary) window is dropped, never smeared onto another window.
    pub fn resolve(&self, surface_id: u16, surface_window_id: u32) -> Option<u32> {
        if let Some(&w) = self.surf_map.get(&surface_id) {
            return self.mirrored(w);
        }
        if surface_window_id != 0 {
            return self.mirrored(surface_window_id);
        }
        if self.windows.len() == 1 {
            let (&id, w) = self.windows.iter().next().unwrap();
            return (w.kind == Kind::Mirror).then_some(id);
        }
        None
    }

    /// `Some(id)` iff `id` is a live mirrored window.
    fn mirrored(&self, id: u32) -> Option<u32> {
        (self.windows.get(&id).map(|w| w.kind) == Some(Kind::Mirror)).then_some(id)
    }
}

// --- C bridge FFI ---------------------------------------------------------
//
// The bridge runs on the single FreeRDP event-loop thread, so one global router
// behind a Mutex is sufficient (and the Mutex is uncontended). These wrappers are
// the only entry points the C side uses; the policy above is what tests drive.

use std::os::raw::c_int;
use std::sync::Mutex;

static ROUTER: Mutex<Router> = Mutex::new(Router {
    windows: BTreeMap::new(),
    surf_map: BTreeMap::new(),
    demoted: Vec::new(),
});

fn with_router<T>(f: impl FnOnce(&mut Router) -> T) -> T {
    f(&mut ROUTER.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Clear all routing state (call at session start so a reconnect doesn't inherit
/// stale windows/mappings).
#[no_mangle]
pub extern "C" fn rail_route_reset() {
    with_router(|r| *r = Router::new());
}

/// Classify/track a window order. `has_style` is non-zero when `exstyle`/`owner`
/// are meaningful; `x,y,w,h` is the window's RAIL desktop rectangle. Returns an
/// [`Order`] discriminant: 1 = new mirror (emit create), 0 = known mirror (plain
/// update), -1 = now filtered (destroy), -2 = filtered (suppress). After calling
/// this, drain [`rail_route_pop_demoted`] to destroy any owners this order turned
/// into frames.
#[no_mangle]
pub extern "C" fn rail_route_window_order(
    id: u32,
    has_style: c_int,
    exstyle: u32,
    owner: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> c_int {
    let style = (has_style != 0).then_some((exstyle, owner));
    match with_router(|r| r.window_order(id, style, (x, y, w, h))) {
        Order::NewMirror => 1,
        Order::KnownMirror => 0,
        Order::NowFiltered => -1,
        Order::Filtered => -2,
    }
}

/// Pop the next window id the router demoted from mirrored to frame (its NSWindow
/// must be destroyed), or 0 when drained.
#[no_mangle]
pub extern "C" fn rail_route_pop_demoted() -> u32 {
    with_router(|r| r.pop_demoted()).unwrap_or(0)
}

/// Returns non-zero iff the deleted window had been mirrored (bridge should emit
/// `window_delete`).
#[no_mangle]
pub extern "C" fn rail_route_window_deleted(id: u32) -> c_int {
    with_router(|r| r.window_deleted(id)) as c_int
}

#[no_mangle]
pub extern "C" fn rail_route_map_surface(surface_id: u16, window_id: u32) {
    with_router(|r| r.map_surface(surface_id, window_id));
}

#[no_mangle]
pub extern "C" fn rail_route_unmap_window(window_id: u32) {
    with_router(|r| r.unmap_window(window_id));
}

/// Resolve a surface to its mirrored window id, or 0 to drop the pixels.
#[no_mangle]
pub extern "C" fn rail_route_resolve(surface_id: u16, surface_window_id: u32) -> u32 {
    with_router(|r| r.resolve(surface_id, surface_window_id)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Style helpers mirroring what weston puts on the wire.
    const APP: Option<(u32, u32)> = Some((0, OWNER_NONE_B)); // toplevel, no owner
    fn owned_by(p: u32) -> Option<(u32, u32)> {
        Some((0, p)) // ordinary owned window (dialog or content child)
    }
    fn tool() -> Option<(u32, u32)> {
        Some((WS_EX_TOOLWINDOW, OWNER_NONE_B))
    }
    fn tooltip(owner: u32) -> Option<(u32, u32)> {
        Some((WS_EX_NOACTIVATE, owner))
    }
    fn r(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
        (x, y, w, h)
    }

    #[test]
    fn classifies_app_vs_decoration() {
        assert!(!is_decoration(0, None), "plain toplevel is an app window");
        assert!(is_decoration(WS_EX_TOOLWINDOW, None), "tool window is decoration");
        assert!(is_decoration(WS_EX_NOACTIVATE, Some(5)), "owned non-activatable helper");
        assert!(!is_decoration(WS_EX_NOACTIVATE, None), "unowned non-activatable kept");
        assert!(!is_decoration(0, Some(5)), "owned but activatable dialog kept");
    }

    #[test]
    fn window_order_transitions() {
        let mut r0 = Router::new();
        assert_eq!(r0.window_order(1, APP, r(0, 0, 800, 600)), Order::NewMirror);
        assert_eq!(r0.window_order(1, APP, r(0, 0, 800, 600)), Order::KnownMirror);
        assert_eq!(r0.window_order(2, tool(), r(0, 0, 100, 40)), Order::Filtered);
        assert_eq!(r0.window_order(2, None, r(0, 0, 100, 40)), Order::Filtered);
        assert_eq!(r0.window_order(3, None, r(0, 0, 400, 300)), Order::NewMirror);
    }

    #[test]
    fn tooltip_surface_never_steals_the_main_window() {
        let mut r0 = Router::new();
        r0.window_order(1, APP, r(0, 0, 800, 600));
        r0.window_order(2, tooltip(1), r(10, 10, 120, 40));
        assert_eq!(r0.resolve(200, 0), None, "ambiguous unmapped surface is dropped");
        r0.map_surface(50, 1);
        assert_eq!(r0.resolve(50, 0), Some(1));
    }

    // --- the Firefox frame/content case, straight from live weston --------

    #[test]
    fn covering_child_demotes_the_frame_and_keeps_content() {
        let mut r0 = Router::new();
        // The transparent shadow-frame toplevel arrives first and is mirrored.
        assert_eq!(
            r0.window_order(3, APP, r(-26, -23, 1332, 1092)),
            Order::NewMirror
        );
        assert!(r0.pop_demoted().is_none(), "nothing to demote yet");
        // Its content child (owned by 3, covering it) arrives and is mirrored...
        assert_eq!(
            r0.window_order(4, owned_by(3), r(0, 0, 1280, 1040)),
            Order::NewMirror
        );
        // ...which reveals id=3 as a frame: it must be destroyed.
        assert_eq!(r0.pop_demoted(), Some(3), "frame id=3 is demoted");
        assert!(r0.pop_demoted().is_none(), "only one demotion");
        // The frame's surface is now dropped; the content's is kept.
        r0.map_surface(2, 3);
        r0.map_surface(3, 4);
        assert_eq!(r0.resolve(2, 0), None, "transparent frame gets no pixels");
        assert_eq!(r0.resolve(3, 0), Some(4), "content window renders");
    }

    #[test]
    fn small_dialog_does_not_demote_its_owner() {
        let mut r0 = Router::new();
        r0.window_order(1, APP, r(0, 0, 1200, 900));
        // A modal dialog: owned, but far smaller than its owner and not covering.
        assert_eq!(
            r0.window_order(2, owned_by(1), r(400, 300, 300, 200)),
            Order::NewMirror
        );
        assert!(r0.pop_demoted().is_none(), "the main window is NOT a frame");
    }

    #[test]
    fn frame_created_after_its_child_is_filtered_immediately() {
        // Defensive: if the covering child is already present when the frame order
        // arrives, the frame is classified as filtered up front (no flash).
        let mut r0 = Router::new();
        r0.window_order(4, owned_by(3), r(0, 0, 1280, 1040));
        assert_eq!(
            r0.window_order(3, APP, r(-26, -23, 1332, 1092)),
            Order::Filtered,
            "frame with an already-present covering child is filtered"
        );
    }

    #[test]
    fn standalone_app_is_never_treated_as_a_frame() {
        let mut r0 = Router::new();
        // weston-terminal: one window, no owned child.
        assert_eq!(r0.window_order(1, APP, r(-32, -32, 806, 491)), Order::NewMirror);
        assert!(r0.pop_demoted().is_none());
        r0.map_surface(1, 1);
        assert_eq!(r0.resolve(1, 0), Some(1));
    }

    #[test]
    fn mapped_surface_wins_over_the_fallback() {
        let mut r0 = Router::new();
        r0.window_order(1, APP, r(0, 0, 800, 600));
        r0.window_order(2, APP, r(0, 0, 800, 600));
        r0.map_surface(10, 2);
        assert_eq!(r0.resolve(10, 0), Some(2));
        assert_eq!(r0.resolve(11, 0), None, "two windows -> unmapped surface dropped");
    }

    #[test]
    fn delete_reports_mirror_state_and_clears_maps() {
        let mut r0 = Router::new();
        r0.window_order(1, APP, r(0, 0, 800, 600));
        r0.window_order(2, tool(), r(0, 0, 100, 40));
        r0.map_surface(10, 1);
        assert!(r0.window_deleted(1), "mirrored window -> emit delete");
        assert!(!r0.window_deleted(2), "filtered window -> no delete");
        assert_eq!(r0.resolve(10, 0), None);
    }
}
