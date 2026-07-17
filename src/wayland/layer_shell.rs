//! zwlr_layer_shell_v1 / zwlr_layer_surface_v1 (wlr-layer-shell)
//!
//! Lets clients create surfaces docked to a screen edge (bars, panels, docks) —
//! e.g. waybar. Each layer surface becomes a borderless, floating NSWindow
//! positioned by its anchor + margins (see create_layer_window in mac.rs).
//!
//! Flow: get_layer_surface -> set_size/anchor/margin -> initial commit (no
//! buffer) -> we send `configure(serial, w, h)` -> client acks + attaches a
//! buffer -> we map the docked window (handle_commit / present).
//!
//! `use super::*` pulls in `State`, the shared records, and the protocol imports.

use super::*;

impl GlobalDispatch<ZwlrLayerShellV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrLayerShellV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrLayerShellV1,
        request: zwlr_layer_shell_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_layer_shell_v1::Request::GetLayerSurface { id, surface, .. } = request {
            let ls = data_init.init(id, ());
            let window_id = state.alloc_window_id();
            state.layer_surfaces.insert(
                ls.id(),
                LayerSurfaceRec {
                    layer_surface: ls.clone(),
                    wl_surface: surface.clone(),
                    window_id,
                    anchor: 0,
                    size: (0, 0),
                    margin: (0, 0, 0, 0),
                    exclusive: 0,
                    keyboard_interactivity: 0,
                    configured: false,
                    created_window: false,
                },
            );
            state.surface_layer.insert(surface.id(), ls.id());
            // Register the window↔surface mapping so keyboard focus/enter can
            // reach the layer surface (e.g. fuzzel needs keyboard input to type).
            state.window_surface.insert(window_id, surface.clone());
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrLayerSurfaceV1,
        request: zwlr_layer_surface_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(rec) = state.layer_surfaces.get_mut(&resource.id()) else {
            return;
        };
        match request {
            zwlr_layer_surface_v1::Request::SetSize { width, height } => {
                rec.size = (width, height);
            }
            zwlr_layer_surface_v1::Request::SetAnchor { anchor } => {
                rec.anchor = match anchor {
                    WEnum::Value(a) => a.bits(),
                    WEnum::Unknown(u) => u,
                };
                state.recompute_reserved();
            }
            zwlr_layer_surface_v1::Request::SetMargin {
                top,
                right,
                bottom,
                left,
            } => {
                rec.margin = (top, right, bottom, left);
            }
            zwlr_layer_surface_v1::Request::SetExclusiveZone { zone } => {
                rec.exclusive = zone;
                state.recompute_reserved();
            }
            zwlr_layer_surface_v1::Request::SetKeyboardInteractivity { keyboard_interactivity } => {
                rec.keyboard_interactivity = match keyboard_interactivity {
                    WEnum::Value(k) => k as u32,
                    WEnum::Unknown(u) => u,
                };
            }
            zwlr_layer_surface_v1::Request::Destroy => {
                state.reap_layer_surface(&resource.id());
            }
            // set_layer / set_exclusive_edge / get_popup / ack_configure:
            // accepted, no-op here.
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _c: ClientId, resource: &ZwlrLayerSurfaceV1, _d: &()) {
        state.reap_layer_surface(&resource.id());
    }
}
