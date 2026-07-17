//! wp_viewporter (surface crop/scale)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// wp_viewporter: surface crop/scale. KWin's nested backend requires this global
// AND depends on `set_destination` to declare its output size (it attaches a
// tiny buffer and scales it up), so we honor the destination as the surface's
// logical size. `set_source` (crop) is accepted but not yet applied.
// ---------------------------------------------------------------------------

impl GlobalDispatch<WpViewporter, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpViewporter>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpViewporter, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpViewporter,
        request: wp_viewporter::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_viewporter::Request::GetViewport { id, surface } = request {
            // Tag the viewport with its surface so set_destination can update it.
            data_init.init(id, surface.id());
        }
    }
}

/// A `wp_viewport`'s data is the `wl_surface` id it controls.
impl Dispatch<WpViewport, ObjectId> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpViewport,
        request: wp_viewport::Request,
        surface_id: &ObjectId,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_viewport::Request::SetDestination { width, height } = request {
            if let Some(rec) = state.surfaces.get_mut(surface_id) {
                // (-1, -1) unsets the destination (spec).
                rec.viewport_dst = if width > 0 && height > 0 {
                    Some((width, height))
                } else {
                    None
                };
            }
        }
    }
}
