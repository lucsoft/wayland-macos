//! wp_single_pixel_buffer_v1 (1x1 solid-colour buffers)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// wp_single_pixel_buffer: lets clients make a 1x1 solid-colour wl_buffer. KWin's
// nested Wayland backend requires this global or it refuses to start.
// ---------------------------------------------------------------------------

impl GlobalDispatch<WpSinglePixelBufferManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpSinglePixelBufferManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpSinglePixelBufferManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpSinglePixelBufferManagerV1,
        request: wp_single_pixel_buffer_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_single_pixel_buffer_manager_v1::Request::CreateU32RgbaBuffer {
            id,
            r,
            g,
            b,
            a,
        } = request
        {
            // The channel values are u32 where u32::MAX == 1.0; take the high byte
            // for 8-bit. Store as BGRA to match the little-endian ARGB8888 order
            // `present` emits.
            let to8 = |v: u32| (v >> 24) as u8;
            data_init.init(
                id,
                BufferData {
                    kind: BufferKind::SinglePixel {
                        bgra: [to8(b), to8(g), to8(r), to8(a)],
                    },
                },
            );
        }
    }
}
