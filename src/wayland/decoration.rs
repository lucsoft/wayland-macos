//! zxdg_decoration_manager_v1 (force server-side decorations)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// xdg-decoration: force server-side decorations so GTK drops its own titlebar
// (the NSWindow titlebar becomes the only chrome, and buffers stay opaque).
// ---------------------------------------------------------------------------

impl GlobalDispatch<ZxdgDecorationManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZxdgDecorationManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZxdgDecorationManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZxdgDecorationManagerV1,
        request: zxdg_decoration_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_decoration_manager_v1::Request::GetToplevelDecoration { id, toplevel } = request
        {
            let deco = data_init.init(id, ());
            // Tell the client we decorate: it must not draw its own titlebar. The
            // client engaging this protocol means it wants a server frame, so give
            // its window a native macOS titlebar (see create_window).
            if let Some(t) = state.toplevels.get_mut(&toplevel.id()) {
                t.wants_ssd = true;
            }
            deco.configure(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        }
    }
}

impl Dispatch<ZxdgToplevelDecorationV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &ZxdgToplevelDecorationV1,
        request: zxdg_toplevel_decoration_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Whatever the client asks for, keep decorations server-side.
        match request {
            zxdg_toplevel_decoration_v1::Request::SetMode { .. }
            | zxdg_toplevel_decoration_v1::Request::UnsetMode => {
                resource.configure(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            }
            _ => {}
        }
    }
}
