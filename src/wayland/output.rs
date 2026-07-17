//! wl_output (one virtual output describing the screen)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// wl_output (one virtual output describing the screen)
// ---------------------------------------------------------------------------

impl GlobalDispatch<WlOutput, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlOutput>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let output = data_init.init(resource, ());
        output.geometry(
            0,
            0,
            340,
            190,
            wl_output::Subpixel::Unknown,
            "wayland-macos".to_string(),
            "virtual".to_string(),
            wl_output::Transform::Normal,
        );
        let (out_w, out_h) = crate::input::output_size();
        output.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            out_w,
            out_h,
            60_000,
        );
        if output.version() >= 2 {
            output.scale(crate::input::scale());
        }
        // v4 adds stable name/description events (fuzzel and other layer-shell
        // clients look these up); they must precede the atomic `done`.
        if output.version() >= 4 {
            output.name("WL-1".to_string());
            output.description("wayland-macos virtual output".to_string());
        }
        if output.version() >= 2 {
            output.done();
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlOutput,
        _request: wl_output::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // wl_output.release (v3+) is a destructor; wayland-server tears the
        // resource down automatically. No other requests exist.
    }
}
