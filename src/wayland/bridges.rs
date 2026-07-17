//! Bridges: connections between Wayland protocol facilities and native macOS
//! services.
//!
//! A *bridge* mirrors a capability that Linux clients expect from a Wayland
//! compositor onto the equivalent macOS service — e.g. the Wayland selection
//! (`wl_data_device`) onto `NSPasteboard`. The point is to keep each such
//! connection self-contained instead of scattering it through `wayland.rs`.
//!
//! > **Not** an [xdg-desktop-portal]. In the Linux ecosystem a "portal" is a
//! > specific D-Bus service (`org.freedesktop.portal.*`) that sandboxed apps
//! > call to reach outside their sandbox. That is a different thing, is D-Bus
//! > based, and isn't what lives here (macOS has no D-Bus). The clipboard in
//! > particular is *core* Wayland protocol, not a portal. "Bridge" is our own
//! > term for the Wayland ⇆ macOS glue.
//!
//! [xdg-desktop-portal]: https://flatpak.github.io/xdg-desktop-portal/
//!
//! ## Anatomy of a bridge
//!
//! Each bridge, by convention:
//!
//! * lives in its own module under `wayland::` (e.g. [`super::clipboard`]);
//! * owns whatever cross-thread state it needs, exposed as a field on
//!   [`Bridges`] (which [`crate::wayland::State`] holds as `state.bridges`) so
//!   the Wayland `Dispatch` handlers can reach it;
//! * implements the `Dispatch` / `GlobalDispatch` handlers for *its* protocol
//!   objects `for State`, in its own module rather than in `mod.rs`
//!   (Rust lets trait impls for `State` live anywhere in the crate);
//! * marshals to and from the AppKit main thread through [`crate::mac`] and the
//!   input bus, honoring the "AppKit only on the main thread" rule.
//!
//! The Wayland globals themselves are still created in [`crate::wayland::run`];
//! a bridge only supplies the handlers and the state they hang off. To add one:
//! create the module, add a field here, and (if it needs one) create its
//! global in `run`.
//!
//! ## Implemented
//!
//! * [`super::clipboard`] — `wl_data_device` ⇆ `NSPasteboard`.
//!
//! Note that some existing Wayland ⇆ macOS glue predates this module and still
//! lives elsewhere — input forwarding (`NSEvent` → `wl_pointer`/`wl_keyboard`)
//! in `mac.rs`/`input.rs`, and window lifecycle (`xdg_toplevel` ⇆ `NSWindow`)
//! in `mod.rs`/`mac.rs`. They are bridges in spirit and could move here.

/// Aggregate of every bridge's state, held as a single `state.bridges` field on
/// [`crate::wayland::State`]. Grow this as bridges are added.
#[derive(Default)]
pub struct Bridges {
    pub clipboard: super::clipboard::Clipboard,
}

impl Bridges {
    pub fn new() -> Self {
        Self::default()
    }
}
