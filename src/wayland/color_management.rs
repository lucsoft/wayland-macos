//! wp_color_manager_v1 (color management / HDR)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.
//!
//! This lets HDR-aware clients declare the color characteristics of their
//! content: a transfer function (PQ / HLG for HDR, sRGB for SDR) and primaries
//! (BT.2020, Display P3, sRGB). We resolve a client's parametric image
//! description into a compact [`ColorDesc`] (see `crate::mac`) and, on
//! `set_image_description`, stage it on the target surface. `present` then
//! carries it to the AppKit side, which tags the frame's `CGImage` with a
//! matching color space and opts the layer into Extended Dynamic Range.
//!
//! Only the parametric path is offered (no ICC profiles, no Windows scRGB); that
//! is what real HDR toolkits use to say "this is BT.2100 PQ".

use super::*;
use wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1, wp_color_management_surface_feedback_v1,
    wp_color_management_surface_v1,
    wp_color_manager_v1::{self, Feature, Primaries as WpPrimaries, RenderIntent, TransferFunction},
    wp_image_description_creator_params_v1, wp_image_description_info_v1, wp_image_description_v1,
};

/// Parameters accumulated on a `wp_image_description_creator_params_v1` before
/// the client calls `create`. Stored on `State::param_creators` keyed by the
/// creator's object id (the request handler only gets a shared `&Data`, so the
/// mutable accumulator lives on `State`).
#[derive(Default, Clone)]
pub(crate) struct ParamAccum {
    tf: Option<TransferFn>,
    primaries: Option<Primaries>,
    max_luminance: Option<f32>,
    ref_luminance: Option<f32>,
}

/// Map a protocol transfer function to the subset we act on. HDR curves map to
/// `Pq`/`Hlg`; the sRGB family collapses to `Srgb`; anything else is unsupported.
fn map_tf(tf: TransferFunction) -> Option<TransferFn> {
    match tf {
        TransferFunction::St2084Pq => Some(TransferFn::Pq),
        TransferFunction::Hlg => Some(TransferFn::Hlg),
        TransferFunction::Srgb | TransferFunction::ExtSrgb => Some(TransferFn::Srgb),
        _ => None,
    }
}

fn map_primaries(p: WpPrimaries) -> Option<Primaries> {
    match p {
        WpPrimaries::Srgb => Some(Primaries::Srgb),
        WpPrimaries::DisplayP3 => Some(Primaries::DisplayP3),
        WpPrimaries::Bt2020 => Some(Primaries::Bt2020),
        _ => None,
    }
}

fn to_wp_tf(tf: TransferFn) -> TransferFunction {
    match tf {
        TransferFn::Pq => TransferFunction::St2084Pq,
        TransferFn::Hlg => TransferFunction::Hlg,
        TransferFn::Srgb => TransferFunction::Srgb,
    }
}

fn to_wp_primaries(p: Primaries) -> WpPrimaries {
    match p {
        Primaries::Srgb => WpPrimaries::Srgb,
        Primaries::DisplayP3 => WpPrimaries::DisplayP3,
        Primaries::Bt2020 => WpPrimaries::Bt2020,
    }
}

/// The color description the compositor reports as the display's / preferred
/// profile (for `get_output` and surface feedback). macOS manages the actual
/// display transform, so plain sRGB is a safe, non-committal answer that doesn't
/// push clients into HDR they didn't ask for.
fn display_preferred_desc() -> ColorDesc {
    ColorDesc {
        tf: TransferFn::Srgb,
        primaries: Primaries::Srgb,
        max_luminance: None,
        ref_luminance: None,
    }
}

/// Register a resolved description under a fresh `wp_image_description_v1` and
/// tell the client it's ready. The identity is the object's protocol id (unique,
/// non-zero) — good enough for clients that only compare identities for equality.
fn ready_image_description(
    state: &mut State,
    img: &WpImageDescriptionV1,
    desc: ColorDesc,
) {
    state.image_descs.insert(img.id(), desc);
    img.ready(img.id().protocol_id());
}

// ---------------------------------------------------------------------------
// wp_color_manager_v1 (manager global)
// ---------------------------------------------------------------------------

impl GlobalDispatch<WpColorManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let mgr = data_init.init(resource, ());
        // Advertise what we honor: the parametric creator, luminance + mastering
        // metadata, the three primaries and the SDR + HDR transfer functions.
        mgr.supported_intent(RenderIntent::Perceptual);
        mgr.supported_feature(Feature::Parametric);
        mgr.supported_feature(Feature::SetLuminances);
        mgr.supported_feature(Feature::SetMasteringDisplayPrimaries);
        mgr.supported_primaries_named(WpPrimaries::Srgb);
        mgr.supported_primaries_named(WpPrimaries::DisplayP3);
        mgr.supported_primaries_named(WpPrimaries::Bt2020);
        mgr.supported_tf_named(TransferFunction::Srgb);
        mgr.supported_tf_named(TransferFunction::St2084Pq);
        mgr.supported_tf_named(TransferFunction::Hlg);
        mgr.done();
    }
}

impl Dispatch<WpColorManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpColorManagerV1,
        request: wp_color_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_manager_v1::Request::GetOutput { id, .. } => {
                data_init.init(id, ());
            }
            wp_color_manager_v1::Request::GetSurface { id, surface } => {
                // Tag the surface-color object with the wl_surface it controls.
                data_init.init(id, surface.id());
            }
            wp_color_manager_v1::Request::GetSurfaceFeedback { id, .. } => {
                data_init.init(id, ());
            }
            wp_color_manager_v1::Request::CreateParametricCreator { obj } => {
                let creator = data_init.init(obj, ());
                state.param_creators.insert(creator.id(), ParamAccum::default());
            }
            // We only advertised the parametric feature; the ICC and Windows
            // creators are a protocol error if used (well-behaved clients check
            // supported_feature first).
            wp_color_manager_v1::Request::CreateIccCreator { .. }
            | wp_color_manager_v1::Request::CreateWindowsScrgb { .. } => {
                resource.post_error(
                    wp_color_manager_v1::Error::UnsupportedFeature,
                    "only the parametric image-description creator is supported",
                );
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// wp_image_description_creator_params_v1 (accumulate tf/primaries/luminance)
// ---------------------------------------------------------------------------

impl Dispatch<WpImageDescriptionCreatorParamsV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpImageDescriptionCreatorParamsV1,
        request: wp_image_description_creator_params_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let id = resource.id();
        match request {
            wp_image_description_creator_params_v1::Request::SetTfNamed {
                tf: WEnum::Value(tf),
            } => {
                if let (Some(mapped), Some(acc)) = (map_tf(tf), state.param_creators.get_mut(&id)) {
                    acc.tf = Some(mapped);
                }
            }
            wp_image_description_creator_params_v1::Request::SetPrimariesNamed {
                primaries: WEnum::Value(p),
            } => {
                if let (Some(mapped), Some(acc)) =
                    (map_primaries(p), state.param_creators.get_mut(&id))
                {
                    acc.primaries = Some(mapped);
                }
            }
            wp_image_description_creator_params_v1::Request::SetLuminances {
                max_lum,
                reference_lum,
                ..
            } => {
                if let Some(acc) = state.param_creators.get_mut(&id) {
                    acc.max_luminance = Some(max_lum as f32);
                    acc.ref_luminance = Some(reference_lum as f32);
                }
            }
            wp_image_description_creator_params_v1::Request::SetMasteringLuminance {
                max_lum,
                ..
            } => {
                if let Some(acc) = state.param_creators.get_mut(&id) {
                    // max_lum here is in cd/m² already (min is *10000, ignored).
                    acc.max_luminance = Some(max_lum as f32);
                }
            }
            wp_image_description_creator_params_v1::Request::Create { image_description } => {
                // `create` consumes the creator (destructor); build the result.
                let acc = state.param_creators.remove(&id).unwrap_or_default();
                let img = data_init.init(image_description, ());
                match (acc.tf, acc.primaries) {
                    (Some(tf), Some(primaries)) => {
                        let desc = ColorDesc {
                            tf,
                            primaries,
                            max_luminance: acc.max_luminance,
                            ref_luminance: acc.ref_luminance,
                        };
                        ready_image_description(state, &img, desc);
                    }
                    _ => {
                        // A transfer function and primaries are both required.
                        img.failed(
                            wp_image_description_v1::Cause::Unsupported,
                            "image description needs both a transfer function and primaries"
                                .to_string(),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// wp_image_description_v1 (a resolved description)
// ---------------------------------------------------------------------------

impl Dispatch<WpImageDescriptionV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpImageDescriptionV1,
        request: wp_image_description_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_image_description_v1::Request::GetInformation { information } => {
                let info = data_init.init(information, ());
                if let Some(desc) = state.image_descs.get(&resource.id()).copied() {
                    info.primaries_named(to_wp_primaries(desc.primaries));
                    info.tf_named(to_wp_tf(desc.tf));
                    if let (Some(maxl), Some(refl)) = (desc.max_luminance, desc.ref_luminance) {
                        // min luminance is reported *10000 per the protocol; we
                        // don't track it, so report 0.
                        info.luminances(0, maxl as u32, refl as u32);
                    }
                }
                // `done` is a destructor: it delivers the batch and ends the object.
                info.done();
            }
            wp_image_description_v1::Request::Destroy => {
                state.image_descs.remove(&resource.id());
            }
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpImageDescriptionInfoV1,
        _request: wp_image_description_info_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Info objects are write-only (events); the client issues no requests.
    }
}

// ---------------------------------------------------------------------------
// wp_color_management_surface_v1 (bind a description to a wl_surface)
// ---------------------------------------------------------------------------

/// Data is the `wl_surface` id this object color-manages.
impl Dispatch<WpColorManagementSurfaceV1, ObjectId> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementSurfaceV1,
        request: wp_color_management_surface_v1::Request,
        surface_id: &ObjectId,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_surface_v1::Request::SetImageDescription {
                image_description,
                ..
            } => {
                // Copy the resolved description onto the surface; it's applied on
                // the next commit (so it's double-buffered with the buffer).
                let desc = state.image_descs.get(&image_description.id()).copied();
                if let Some(rec) = state.surfaces.get_mut(surface_id) {
                    rec.pending_color = Some(desc);
                }
            }
            wp_color_management_surface_v1::Request::UnsetImageDescription => {
                if let Some(rec) = state.surfaces.get_mut(surface_id) {
                    rec.pending_color = Some(None);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// wp_color_management_surface_feedback_v1 (what the compositor prefers)
// ---------------------------------------------------------------------------

impl Dispatch<WpColorManagementSurfaceFeedbackV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_surface_feedback_v1::Request::GetPreferred {
                image_description,
            }
            | wp_color_management_surface_feedback_v1::Request::GetPreferredParametric {
                image_description,
            } => {
                let img = data_init.init(image_description, ());
                ready_image_description(state, &img, display_preferred_desc());
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// wp_color_management_output_v1 (the display's description)
// ---------------------------------------------------------------------------

impl Dispatch<WpColorManagementOutputV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_color_management_output_v1::Request::GetImageDescription { image_description } =
            request
        {
            let img = data_init.init(image_description, ());
            ready_image_description(state, &img, display_preferred_desc());
        }
    }
}
