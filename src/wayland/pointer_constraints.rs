//! zwp_pointer_constraints_v1 (pointer lock/confine)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// Pointer constraints + relative pointer: KWin's nested backend requires both
// globals. We accept the requests and create the objects, but do not actually
// lock/confine the pointer or synthesize relative motion — enough for KWin to
// start and run windowed (it doesn't need a real pointer lock nested).
// ---------------------------------------------------------------------------

impl GlobalDispatch<ZwpPointerConstraintsV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpPointerConstraintsV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpPointerConstraintsV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpPointerConstraintsV1,
        request: zwp_pointer_constraints_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_pointer_constraints_v1::Request::LockPointer { id, .. } => {
                data_init.init(id, ());
            }
            zwp_pointer_constraints_v1::Request::ConfinePointer { id, .. } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpLockedPointerV1,
        _: <ZwpLockedPointerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ZwpConfinedPointerV1, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpConfinedPointerV1,
        _: <ZwpConfinedPointerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
