//! zwp_relative_pointer_v1 (relative motion)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


impl GlobalDispatch<ZwpRelativePointerManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpRelativePointerManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpRelativePointerManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpRelativePointerManagerV1,
        request: zwp_relative_pointer_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_relative_pointer_manager_v1::Request::GetRelativePointer { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpRelativePointerV1,
        _: <ZwpRelativePointerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
