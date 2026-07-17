//! xdg_wm_dialog_v1 / xdg_dialog_v1 (modal dialog hints)
//!
//! A client marks an `xdg_toplevel` as a (modal) dialog. We map "modal" to a
//! raised macOS window level so the dialog floats above its ordinary windows.
//! We intentionally do NOT run a blocking modal session — that would freeze the
//! compositor's main thread.
//!
//! The `xdg_dialog_v1` carries the `XdgToplevel` it was created for as user data,
//! which we resolve to a native window id via `state.toplevels`.
//!
//! `use super::*` pulls in `State`, the shared records, and the protocol imports.

use super::*;
use wayland_protocols::xdg::dialog::v1::server::{
    xdg_dialog_v1::{self, XdgDialogV1},
    xdg_wm_dialog_v1::{self, XdgWmDialogV1},
};

impl GlobalDispatch<XdgWmDialogV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XdgWmDialogV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<XdgWmDialogV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &XdgWmDialogV1,
        request: xdg_wm_dialog_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_wm_dialog_v1::Request::GetXdgDialog { id, toplevel } = request {
            // Keep the toplevel so [un]set_modal can resolve its window.
            data_init.init(id, toplevel);
        }
    }
}

/// `xdg_dialog_v1` carries its `XdgToplevel` as user data.
impl Dispatch<XdgDialogV1, XdgToplevel> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &XdgDialogV1,
        request: xdg_dialog_v1::Request,
        toplevel: &XdgToplevel,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let modal = match request {
            xdg_dialog_v1::Request::SetModal => true,
            xdg_dialog_v1::Request::UnsetModal => false,
            _ => return,
        };
        if let Some(t) = state.toplevels.get(&toplevel.id()) {
            mac::post(WinCmd::SetModal {
                id: t.window_id,
                modal,
            });
        }
    }
}
