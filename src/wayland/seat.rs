//! wl_seat, wl_pointer, wl_keyboard, wl_touch
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// wl_seat (advertised so toolkits proceed; no input is delivered yet)
// ---------------------------------------------------------------------------

impl GlobalDispatch<WlSeat, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlSeat>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        seat.capabilities(wl_seat::Capability::Pointer | wl_seat::Capability::Keyboard);
        if seat.version() >= 2 {
            seat.name("seat0".to_string());
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WlSeat,
        request: wl_seat::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                let pointer = data_init.init(id, ());
                state.pointers.push(pointer);
            }
            wl_seat::Request::GetKeyboard { id } => {
                let keyboard = data_init.init(id, ());
                // Every keyboard needs the keymap before it can interpret keys.
                if let Some((file, size)) = &state.keymap {
                    keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1, file.as_fd(), *size);
                }
                if keyboard.version() >= 4 {
                    // 25 keys/sec after a 600ms delay.
                    keyboard.repeat_info(25, 600);
                }
                state.keyboards.push(keyboard);
            }
            wl_seat::Request::GetTouch { id } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlPointer,
        request: wl_pointer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_pointer::Request::SetCursor {
            surface,
            hotspot_x,
            hotspot_y,
            ..
        } = request
        {
            match surface {
                Some(surf) => {
                    // Designate this surface as the cursor; its next commit turns
                    // into a native cursor image (see handle_commit). If it already
                    // has content pending, apply it immediately.
                    state.cursor_surface = Some(surf.id());
                    state.cursor_hotspot = (hotspot_x, hotspot_y);
                    state.apply_cursor_surface(&surf.id());
                }
                None => {
                    // A null surface hides the pointer.
                    state.cursor_surface = None;
                    mac::post(WinCmd::HideCursor);
                }
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlKeyboard,
        _: <WlKeyboard as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<WlTouch, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlTouch,
        _: <WlTouch as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
