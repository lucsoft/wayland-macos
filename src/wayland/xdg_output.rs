//! zxdg_output_manager_v1 (logical output geometry)
//!
//! Reports each `wl_output`'s logical position/size/name. KWin's nested backend
//! (and Plasma) rely on this to establish output geometry — without it
//! plasmashell logs "requesting unexisting screen geometry -1" and can't place
//! its panel/desktop.
//!
//! `use super::*` pulls in `State`, the shared records, and the protocol imports.

use super::*;

impl GlobalDispatch<ZxdgOutputManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZxdgOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZxdgOutputManagerV1,
        request: zxdg_output_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_output_manager_v1::Request::GetXdgOutput { id, output } = request {
            let xdg = data_init.init(id, ());
            // Logical geometry = physical output size / scale.
            let (pw, ph) = crate::input::output_size();
            let scale = crate::input::scale().max(1);
            xdg.logical_position(0, 0);
            xdg.logical_size((pw / scale).max(1), (ph / scale).max(1));
            if xdg.version() >= 2 {
                xdg.name("WL-1".to_string());
                xdg.description("wayland-macos virtual output".to_string());
            }
            if xdg.version() < 3 {
                // v1/v2: xdg_output.done applies the state.
                xdg.done();
            } else {
                // v3+: xdg_output.done is deprecated; the logical geometry is
                // applied atomically by the *following* wl_output.done. We already
                // sent wl_output.done during the wl_output bind (before this
                // get_xdg_output), so without another one the client never commits
                // the logical geometry — Qt then falls back to a 0x0 placeholder
                // screen, which breaks popup/menu positioning. Send a fresh
                // wl_output.done now to commit the xdg_output state.
                if output.version() >= 2 {
                    output.done();
                }
            }
        }
    }
}

impl Dispatch<ZxdgOutputV1, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZxdgOutputV1,
        _: <ZxdgOutputV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
