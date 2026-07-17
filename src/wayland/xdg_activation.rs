//! xdg_activation_v1 (window activation / focus requests)
//!
//! A client asks the compositor to focus a surface — the Wayland equivalent of
//! "raise that window". Used for "already running, focus the existing window",
//! notification click-to-raise, and cross-app focus hand-off (e.g. a launcher
//! activating the app it just started).
//!
//! A client first obtains a token (`get_activation_token` → configure it →
//! `commit`), which we echo back via `done`, and then passes that token to
//! `activate { token, surface }`. We do not do cross-client startup-notification
//! matching, so the token is opaque: any committed token is accepted, and
//! `activate` simply focuses the named surface's window.
//!
//! `use super::*` pulls in `State`, the shared records, and the protocol imports.

use super::*;
use wayland_protocols::xdg::activation::v1::server::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::{self, XdgActivationV1},
};

impl GlobalDispatch<XdgActivationV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XdgActivationV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<XdgActivationV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &XdgActivationV1,
        request: xdg_activation_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_activation_v1::Request::GetActivationToken { id } => {
                data_init.init(id, ());
            }
            xdg_activation_v1::Request::Activate { surface, .. } => {
                // Focus the window backing this surface. The token is not
                // validated (we do not track startup-notification handshakes).
                if let Some(window_id) = state.window_for_surface(&surface.id()) {
                    mac::post(WinCmd::Activate { id: window_id });
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgActivationTokenV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        token: &XdgActivationTokenV1,
        request: xdg_activation_token_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // set_serial / set_app_id / set_surface only add hints we don't use; the
        // one request that matters is commit, which must be answered with `done`.
        if let xdg_activation_token_v1::Request::Commit = request {
            let serial = state.serial();
            token.done(format!("wlmac-{serial}"));
        }
    }
}
