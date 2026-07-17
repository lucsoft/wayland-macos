//! wp_cursor_shape_v1 (client cursor selection)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// wp_cursor_shape: clients request a named cursor; we map it to an NSCursor.
// ---------------------------------------------------------------------------

impl GlobalDispatch<WpCursorShapeManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpCursorShapeManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpCursorShapeManagerV1,
        request: wp_cursor_shape_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_cursor_shape_manager_v1::Request::GetPointer {
                cursor_shape_device,
                ..
            } => {
                data_init.init(cursor_shape_device, ());
            }
            wp_cursor_shape_manager_v1::Request::GetTabletToolV2 {
                cursor_shape_device,
                ..
            } => {
                data_init.init(cursor_shape_device, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpCursorShapeDeviceV1,
        request: wp_cursor_shape_device_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_cursor_shape_device_v1::Request::SetShape { shape, .. } = request {
            let shape = match shape {
                WEnum::Value(s) => s as u32,
                WEnum::Unknown(v) => v,
            };
            mac::post(WinCmd::SetCursor { shape });
        }
    }
}
