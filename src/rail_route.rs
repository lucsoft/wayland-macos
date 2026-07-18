//! RAIL windowing *policy*, extracted from the C bridge so it is unit-testable.
//!
//! The RDP/RAIL bridge (`csrc/rail_bridge.c`) is a thin transport: it decodes
//! window orders and rdpgfx surface updates off the wire and asks this module two
//! questions that used to be answered by ad-hoc C state:
//!
//!   1. **Classification** — should this window order become a real NSWindow, or
//!      is it an auxiliary (CSD shadow / tooltip / sub-surface helper) window we
//!      must not mirror? (`window_order`)
//!   2. **Routing** — a surface carrying pixels arrived; which mirrored window do
//!      those pixels belong to? (`resolve`)
//!
//! Keeping this in Rust means the existing `cargo test --bin wayland-macos`
//! harness exercises it headlessly (no FreeRDP, no RDP server, no AppKit) — the
//! same reason `csd_margin`/`logical_size` live as pure functions in `mac.rs`.
//! The C side calls the `#[no_mangle]` wrappers at the bottom; the pure `Router`
//! is what the tests drive.
//!
//! ## The bug this fixes
//!
//! The old C router, when a surface had neither an explicit WSLg map nor its own
//! `windowId`, fell back to `g_main_window_id` — "the most recently created
//! window." With a single app that was fine, but as soon as a second window
//! existed (a tooltip, a sub-surface helper) an unmapped surface would smear its
//! pixels onto whatever window was newest — e.g. a tooltip's bitmap painted over
//! the main window ("the tooltip replaced the content"). `resolve` here only
//! honours that ownerless fallback when there is **exactly one** window, so an
//! ambiguous surface is dropped rather than misrouted.

use std::collections::BTreeMap;

/// Win32 extended-style bits weston sets on RAIL window orders (mirrors the
/// `WS_EX_*` constants in `<freerdp/window.h>`; kept here so the policy needs no
/// FreeRDP headers and stays testable without the `rail` feature).
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

/// Whether a window is mirrored to a native NSWindow or suppressed as an
/// auxiliary (shadow/tooltip/tool/sub-surface) window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Mirror,
    Filtered,
}

/// Outcome of feeding a window order through [`Router::window_order`]. The C
/// bridge maps these onto "emit a create", "emit nothing", or "emit a delete
/// then nothing" so create/update/promotion all funnel through one decision.
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
/// canonical "not a primary app window" flag; an *owned* (`owner != 0`) window
/// that is also `WS_EX_NOACTIVATE` is a non-focusable helper (shadow/tooltip).
/// Owned-but-activatable windows (real modal dialogs) are kept.
fn is_decoration(exstyle: u32, owner: u32) -> bool {
    if exstyle & WS_EX_TOOLWINDOW != 0 {
        return true;
    }
    if owner != 0 && exstyle & WS_EX_NOACTIVATE != 0 {
        return true;
    }
    false
}

/// All routing state for one RAIL session: which windows exist (and whether they
/// are mirrored) and which rdpgfx surface maps to which window.
#[derive(Default)]
pub struct Router {
    windows: BTreeMap<u32, Kind>,
    surf_map: BTreeMap<u16, u32>,
}

impl Router {
    pub fn new() -> Self {
        Router::default()
    }

    /// Feed a window order. `style` is `Some((extended_style, owner))` when the
    /// order carried the STYLE field, `None` for a geometry-only order. Absent
    /// style means "don't reclassify": an unknown window fails open to `Mirror`
    /// (so a geometry-only order is never silently lost), a known window keeps
    /// its current kind.
    pub fn window_order(&mut self, id: u32, style: Option<(u32, u32)>) -> Order {
        let prev = self.windows.get(&id).copied();
        let kind = match (style, prev) {
            (Some((exstyle, owner)), _) => {
                if is_decoration(exstyle, owner) {
                    Kind::Filtered
                } else {
                    Kind::Mirror
                }
            }
            (None, Some(k)) => k,
            (None, None) => Kind::Mirror, // fail open — never lose a real window
        };
        self.windows.insert(id, kind);
        match (prev, kind) {
            (Some(Kind::Mirror), Kind::Mirror) => Order::KnownMirror,
            (_, Kind::Mirror) => Order::NewMirror, // new, or promoted from Filtered
            (Some(Kind::Mirror), Kind::Filtered) => Order::NowFiltered,
            (_, Kind::Filtered) => Order::Filtered,
        }
    }

    /// A window was deleted. Returns whether it had been mirrored (so the bridge
    /// only emits a `window_delete` for a window the Rust side actually created).
    pub fn window_deleted(&mut self, id: u32) -> bool {
        let was_mirror = self.windows.remove(&id) == Some(Kind::Mirror);
        self.surf_map.retain(|_, &mut w| w != id);
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
        // Ownerless, unmapped surface: safe to attribute only when a single
        // window exists. Any second window makes the target ambiguous — dropping
        // beats painting a tooltip over the app (see the module note).
        if self.windows.len() == 1 {
            let (&id, &kind) = self.windows.iter().next().unwrap();
            return (kind == Kind::Mirror).then_some(id);
        }
        None
    }

    /// `Some(id)` iff `id` is a live mirrored window.
    fn mirrored(&self, id: u32) -> Option<u32> {
        (self.windows.get(&id) == Some(&Kind::Mirror)).then_some(id)
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
/// are meaningful. Returns an [`Order`] discriminant: 1 = new mirror (emit
/// create), 0 = known mirror (plain update), -1 = now filtered (destroy), -2 =
/// filtered (suppress).
#[no_mangle]
pub extern "C" fn rail_route_window_order(
    id: u32,
    has_style: c_int,
    exstyle: u32,
    owner: u32,
) -> c_int {
    let style = (has_style != 0).then_some((exstyle, owner));
    match with_router(|r| r.window_order(id, style)) {
        Order::NewMirror => 1,
        Order::KnownMirror => 0,
        Order::NowFiltered => -1,
        Order::Filtered => -2,
    }
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
    const APP: Option<(u32, u32)> = Some((0, 0)); // plain toplevel, no owner
    fn tooltip(owner: u32) -> Option<(u32, u32)> {
        Some((WS_EX_NOACTIVATE, owner)) // owned + non-activatable = helper
    }
    fn tool() -> Option<(u32, u32)> {
        Some((WS_EX_TOOLWINDOW, 0))
    }

    #[test]
    fn classifies_app_vs_decoration() {
        assert!(!is_decoration(0, 0), "plain toplevel is an app window");
        assert!(is_decoration(WS_EX_TOOLWINDOW, 0), "tool window is decoration");
        assert!(
            is_decoration(WS_EX_NOACTIVATE, 5),
            "owned non-activatable helper is decoration"
        );
        assert!(
            !is_decoration(WS_EX_NOACTIVATE, 0),
            "unowned non-activatable window is kept (not a helper)"
        );
        assert!(
            !is_decoration(0, 5),
            "owned but activatable window is a real dialog — kept"
        );
    }

    #[test]
    fn window_order_transitions() {
        let mut r = Router::new();
        assert_eq!(r.window_order(1, APP), Order::NewMirror);
        assert_eq!(r.window_order(1, APP), Order::KnownMirror);
        // A tool/tooltip window is filtered from the first order.
        assert_eq!(r.window_order(2, tool()), Order::Filtered);
        assert_eq!(r.window_order(2, None), Order::Filtered, "stays suppressed");
        // Geometry-only order for an unseen window fails open to a real window.
        assert_eq!(r.window_order(3, None), Order::NewMirror);
    }

    #[test]
    fn filtered_window_can_be_promoted_and_demoted() {
        let mut r = Router::new();
        assert_eq!(r.window_order(9, tool()), Order::Filtered);
        // A later STYLE order that clears the decoration bits promotes it.
        assert_eq!(r.window_order(9, APP), Order::NewMirror);
        // ...and the reverse demotes a mirrored window (bridge destroys it).
        assert_eq!(r.window_order(9, tool()), Order::NowFiltered);
    }

    #[test]
    fn single_window_absorbs_unmapped_surface() {
        // The original, legitimate reason the fallback existed: a lone app whose
        // content surface arrives without a MapWindowForSurface.
        let mut r = Router::new();
        r.window_order(1, APP);
        assert_eq!(r.resolve(100, 0), Some(1));
    }

    #[test]
    fn tooltip_surface_never_steals_the_main_window() {
        // The reported bug: main window + a filtered tooltip window. A tooltip
        // pixel surface arrives unmapped and ownerless — it must be dropped, NOT
        // painted onto the main window.
        let mut r = Router::new();
        r.window_order(1, APP); // main Firefox window
        r.window_order(2, tooltip(1)); // owned tooltip -> filtered
        assert_eq!(
            r.resolve(200, 0),
            None,
            "ambiguous unmapped surface must not smear onto the main window"
        );
        // The main window's own content still routes correctly when weston maps it.
        r.map_surface(50, 1);
        assert_eq!(r.resolve(50, 0), Some(1));
    }

    #[test]
    fn mapped_surface_wins_over_the_fallback() {
        let mut r = Router::new();
        r.window_order(1, APP);
        r.window_order(2, APP);
        r.map_surface(10, 2);
        assert_eq!(r.resolve(10, 0), Some(2), "explicit map routes to window 2");
        // Two mirrored windows and an unmapped surface -> ambiguous -> dropped.
        assert_eq!(r.resolve(11, 0), None);
    }

    #[test]
    fn surface_mapped_to_filtered_window_is_dropped() {
        let mut r = Router::new();
        r.window_order(1, APP);
        r.window_order(2, tooltip(1));
        r.map_surface(10, 2); // maps onto the filtered tooltip
        assert_eq!(r.resolve(10, 0), None, "filtered window gets no pixels");
    }

    #[test]
    fn delete_reports_mirror_state_and_clears_maps() {
        let mut r = Router::new();
        r.window_order(1, APP);
        r.window_order(2, tooltip(1));
        r.map_surface(10, 1);
        assert!(r.window_deleted(1), "mirrored window -> emit delete");
        assert!(!r.window_deleted(2), "filtered window -> no delete");
        // Its surface mapping is gone, so a now-lone... nothing is left; resolve drops.
        assert_eq!(r.resolve(10, 0), None);
    }
}
