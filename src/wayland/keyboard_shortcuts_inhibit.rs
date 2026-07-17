//! zwp_keyboard_shortcuts_inhibit_v1 (let a client grab compositor shortcuts)
//!
//! A client (terminal, remote-desktop viewer, VM) asks that the compositor stop
//! intercepting keyboard shortcuts for one of its surfaces, so key combinations
//! reach the client instead. We acknowledge the request with `active`.
//!
//! Note on scope: this compositor does not currently consume any keyboard
//! shortcuts on the Wayland side — every key on the focused surface is already
//! forwarded (see `State::process_input`). macOS-level shortcut interception
//! (e.g. Cmd-based combos handled by AppKit) would need a `src/mac.rs` change to
//! fully honour; emitting `active` already satisfies clients that gate behaviour
//! on a granted inhibitor.
//!
//! `use super::*` pulls in `State`, the shared records, and the protocol imports.

use super::*;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::server::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::{self, ZwpKeyboardShortcutsInhibitManagerV1},
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
};

impl GlobalDispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpKeyboardShortcutsInhibitManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpKeyboardShortcutsInhibitManagerV1,
        request: zwp_keyboard_shortcuts_inhibit_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_keyboard_shortcuts_inhibit_manager_v1::Request::InhibitShortcuts { id, .. } =
            request
        {
            let inhibitor = data_init.init(id, ());
            // Grant the inhibitor immediately: shortcuts now pass through to the
            // client's surface.
            inhibitor.active();
        }
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpKeyboardShortcutsInhibitorV1,
        _: zwp_keyboard_shortcuts_inhibitor_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy` exists; the resource is dropped automatically.
    }
}
