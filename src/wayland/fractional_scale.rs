//! wp_fractional_scale_v1 (fractional scale hints)
//!
//! Lets a client learn the preferred scale for a surface as a fraction, so
//! toolkits (GTK4, Qt6) can render crisply at non-integer ratios instead of
//! guessing from `wl_output.scale` (which is integer-only). The client renders
//! at the reported scale and uses `wp_viewport` to declare its logical size.
//!
//! The scale is transmitted as `scale * 120` (a fraction with denominator 120).
//! We report the compositor's backing scale, which is fixed at startup (see
//! `src/main.rs`), so a single `preferred_scale` at creation time suffices.
//!
//! `use super::*` pulls in `State`, the shared records, and the protocol imports.

use super::*;
use wayland_protocols::wp::fractional_scale::v1::server::{
    wp_fractional_scale_manager_v1::{self, WpFractionalScaleManagerV1},
    wp_fractional_scale_v1::WpFractionalScaleV1,
};

/// Fixed-point denominator the protocol uses for the scale fraction.
const SCALE_DENOMINATOR: u32 = 120;

impl GlobalDispatch<WpFractionalScaleManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpFractionalScaleManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpFractionalScaleManagerV1,
        request: wp_fractional_scale_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_fractional_scale_manager_v1::Request::GetFractionalScale { id, .. } = request {
            let frac = data_init.init(id, ());
            // Report the output scale as a fraction (scale * 120). Our scale is a
            // startup-fixed integer, so this is sent once and never changes.
            let scale = crate::input::scale().max(1) as u32;
            frac.preferred_scale(scale * SCALE_DENOMINATOR);
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpFractionalScaleV1,
        _: <WpFractionalScaleV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
