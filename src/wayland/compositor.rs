//! wl_compositor, wl_surface, wl_region, wl_callback, wl_subcompositor
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// wl_compositor
// ---------------------------------------------------------------------------

impl GlobalDispatch<WlCompositor, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlCompositor>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WlCompositor,
        request: wl_compositor::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                let surface = data_init.init(id, ());
                // Ask the client to render at the display scale (HiDPI).
                if surface.version() >= 6 {
                    surface.preferred_buffer_scale(crate::input::scale());
                }
                state.surfaces.insert(surface.id(), SurfaceRec::default());
            }
            wl_compositor::Request::CreateRegion { id } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        surface: &WlSurface,
        request: wl_surface::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => {
                if let Some(rec) = state.surfaces.get_mut(&surface.id()) {
                    rec.pending_buffer = buffer;
                }
            }
            wl_surface::Request::Frame { callback } => {
                let cb = data_init.init(callback, ());
                if let Some(rec) = state.surfaces.get_mut(&surface.id()) {
                    rec.frame_callbacks.push(cb);
                }
            }
            wl_surface::Request::Commit => {
                state.handle_commit(surface);
            }
            wl_surface::Request::Destroy => {
                let sid = surface.id();
                if let Some(tl_id) = state.surface_toplevel.remove(&sid) {
                    if let Some(t) = state.toplevels.remove(&tl_id) {
                        if t.created_window {
                            mac::post(WinCmd::Destroy { id: t.window_id });
                        }
                    }
                }
                if let Some(sub_obj) = state.surface_subsurface.get(&sid).cloned() {
                    state.reap_subsurface(&sub_obj);
                }
                if let Some(ls_obj) = state.surface_layer.get(&sid).cloned() {
                    state.reap_layer_surface(&ls_obj);
                }
                if state.cursor_surface.as_ref() == Some(&sid) {
                    state.cursor_surface = None;
                }
                state.surfaces.remove(&sid);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlRegion, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlRegion,
        _request: wl_region::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlCallback,
        _request: <WlCallback as Resource>::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// wl_data_device_manager (clipboard/DnD)
//
// The Dispatch/GlobalDispatch handlers for the data-device family live in the
// clipboard bridge (`crate::wayland::clipboard`), which bridges the Wayland
// selection to the macOS pasteboard. The global is still created in `run`.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// wl_subcompositor / wl_subsurface
//
// A subsurface is a child surface composited onto a parent at an offset. KWin
// renders its whole nested output (and cursor) into subsurfaces of its toplevel,
// so these must be composited as sublayers of the root window (see
// present_subsurface / WinCmd::SubFrame).
// ---------------------------------------------------------------------------

impl GlobalDispatch<WlSubcompositor, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlSubcompositor>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlSubcompositor, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WlSubcompositor,
        request: wl_subcompositor::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_subcompositor::Request::GetSubsurface {
            id,
            surface,
            parent,
        } = request
        {
            let sub = data_init.init(id, ());
            // The spec forbids a surface being its own parent (a subsurface
            // cycle); reject it here rather than let it hang the parent-chain
            // walk in resolve_subsurface. wlroots/Smithay likewise raise
            // bad_surface at get_subsurface time. The object is left inert (never
            // inserted into the maps); the protocol error disconnects the client.
            if surface.id() == parent.id() {
                resource.post_error(
                    wl_subcompositor::Error::BadSurface,
                    "a surface cannot be its own subsurface parent",
                );
                return;
            }
            let sub_id = state.alloc_window_id();
            state.subsurfaces.insert(
                sub.id(),
                SubsurfaceRec {
                    surface: surface.id(),
                    parent: parent.id(),
                    x: 0,
                    y: 0,
                    sub_id,
                },
            );
            state.surface_subsurface.insert(surface.id(), sub.id());
        }
    }
}

impl Dispatch<WlSubsurface, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &WlSubsurface,
        request: wl_subsurface::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_subsurface::Request::SetPosition { x, y } => {
                // Position is double-buffered in the spec (applied on parent
                // commit); applying it immediately is fine for our use.
                if let Some(rec) = state.subsurfaces.get_mut(&resource.id()) {
                    rec.x = x;
                    rec.y = y;
                }
            }
            wl_subsurface::Request::Destroy => {
                state.reap_subsurface(&resource.id());
            }
            _ => {}
        }
    }
}
