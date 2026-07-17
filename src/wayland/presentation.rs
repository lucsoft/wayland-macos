//! wp_presentation (present timing feedback)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// wp_presentation: report when a surface's content was shown. KWin's nested
// backend requires this global and uses the feedback to pace its render loop.
// We advertise CLOCK_MONOTONIC and fire `presented` on the next present().
// ---------------------------------------------------------------------------

impl GlobalDispatch<WpPresentation, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpPresentation>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let presentation = data_init.init(resource, ());
        presentation.clock_id(libc::CLOCK_MONOTONIC as u32);
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpPresentation,
        request: wp_presentation::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_presentation::Request::Feedback { surface, callback } = request {
            let fb = data_init.init(callback, ());
            state
                .presentation_feedback
                .entry(surface.id())
                .or_default()
                .push(fb);
        }
    }
}

impl Dispatch<WpPresentationFeedback, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpPresentationFeedback,
        _: <WpPresentationFeedback as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
