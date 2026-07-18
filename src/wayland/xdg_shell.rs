//! xdg_wm_base, xdg_surface, xdg_toplevel, xdg_popup, xdg_positioner
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// xdg_shell
// ---------------------------------------------------------------------------

impl GlobalDispatch<XdgWmBase, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XdgWmBase>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &XdgWmBase,
        request: xdg_wm_base::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                data_init.init(id, surface);
            }
            xdg_wm_base::Request::CreatePositioner { id } => {
                let positioner = data_init.init(id, ());
                state
                    .positioners
                    .insert(positioner.id(), PositionerState::default());
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgPositioner, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &XdgPositioner,
        request: xdg_positioner::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let entry = state.positioners.entry(resource.id()).or_default();
        match request {
            xdg_positioner::Request::SetSize { width, height } => entry.size = (width, height),
            xdg_positioner::Request::SetAnchorRect {
                x,
                y,
                width,
                height,
            } => entry.anchor_rect = (x, y, width, height),
            xdg_positioner::Request::SetOffset { x, y } => entry.offset = (x, y),
            xdg_positioner::Request::SetAnchor { anchor } => {
                entry.anchor = match anchor {
                    WEnum::Value(v) => v as u32,
                    WEnum::Unknown(u) => u,
                };
            }
            xdg_positioner::Request::SetGravity { gravity } => {
                entry.gravity = match gravity {
                    WEnum::Value(v) => v as u32,
                    WEnum::Unknown(u) => u,
                };
            }
            xdg_positioner::Request::SetConstraintAdjustment {
                constraint_adjustment,
            } => entry.constraint_adjustment = constraint_adjustment.into(),
            _ => {}
        }
    }
}

/// `xdg_surface` carries the `wl_surface` it wraps as user data.
impl Dispatch<XdgSurface, WlSurface> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        xdg_surface: &XdgSurface,
        request: xdg_surface::Request,
        wl_surface: &WlSurface,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_surface::Request::SetWindowGeometry {
                x,
                y,
                width,
                height,
            } => {
                if let Some(rec) = state.surfaces.get_mut(&wl_surface.id()) {
                    rec.geometry_offset = (x, y);
                    rec.geometry_size = (width, height);
                }
            }
            xdg_surface::Request::GetToplevel { id } => {
                let toplevel = data_init.init(id, xdg_surface.clone());
                let window_id = state.alloc_window_id();
                state.toplevels.insert(
                    toplevel.id(),
                    ToplevelRec {
                        toplevel: toplevel.clone(),
                        xdg_surface: xdg_surface.clone(),
                        wl_surface: wl_surface.clone(),
                        title: "Wayland Window".to_string(),
                        app_id: String::new(),
                        window_id,
                        configured: false,
                        created_window: false,
                        maximized: false,
                        fullscreen: false,
                        wants_ssd: false,
                        pending_icon: None,
                    },
                );
                state
                    .surface_toplevel
                    .insert(wl_surface.id(), toplevel.id());
                state.window_surface.insert(window_id, wl_surface.clone());
            }
            xdg_surface::Request::GetPopup {
                id,
                parent,
                positioner,
            } => {
                let mut pos = state
                    .positioners
                    .get(&positioner.id())
                    .copied()
                    .unwrap_or_default();
                let (w, h) = if pos.size != (0, 0) {
                    pos.size
                } else {
                    (200, 200)
                };
                // Resolve placement from anchor + gravity + offset (relative to the
                // parent surface). popup_origin needs the resolved size.
                pos.size = (w, h);
                let (x, y) = pos.popup_origin();
                // The flipped candidate (anchor+gravity inverted on each axis),
                // used by create_popup when the popup would fall off a screen edge.
                let (x_flip, y_flip) = (pos.origin_x(true), pos.origin_y(true));
                debug!(
                    target: "wl",
                    "popup positioner: anchor_rect={:?} anchor={} gravity={} offset={:?} size=({w},{h}) constraint={:#b} -> origin=({x},{y}) flipped=({x_flip},{y_flip})",
                    pos.anchor_rect, pos.anchor, pos.gravity, pos.offset, pos.constraint_adjustment,
                );
                let parent_window = parent
                    .as_ref()
                    .and_then(|ps| ps.data::<WlSurface>())
                    .and_then(|s| state.window_for_surface(&s.id()))
                    .unwrap_or(0);

                let popup = data_init.init(id, ());
                let window_id = state.alloc_window_id();
                state.popups.insert(
                    popup.id(),
                    PopupRec {
                        popup: popup.clone(),
                        xdg_surface: xdg_surface.clone(),
                        wl_surface: wl_surface.clone(),
                        parent_window,
                        window_id,
                        x,
                        y,
                        x_flip,
                        y_flip,
                        constraint: pos.constraint_adjustment,
                        w,
                        h,
                        configured: false,
                        created_window: false,
                        dismissed: false,
                    },
                );
                state.surface_popup.insert(wl_surface.id(), popup.id());
                state.window_surface.insert(window_id, wl_surface.clone());
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgPopup, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        popup: &XdgPopup,
        request: xdg_popup::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_popup::Request::Grab { .. } => {
                // The menu wants an exclusive pointer grab: route all pointer
                // input to it and dismiss on outside click.
                if let Some(p) = state.popups.get(&popup.id()) {
                    debug!(target: "wl", "popup grab -> window {}", p.window_id);
                    mac::post(WinCmd::SetGrab {
                        window: Some(p.window_id),
                    });
                }
            }
            xdg_popup::Request::Destroy => {
                state.reap_popup(&popup.id());
            }
            _ => {}
        }
    }

    /// Also fires on abrupt client disconnect (see the toplevel impl).
    fn destroyed(state: &mut Self, _client: ClientId, resource: &XdgPopup, _data: &()) {
        state.reap_popup(&resource.id());
    }
}

/// `xdg_toplevel` carries its `xdg_surface` as user data.
impl Dispatch<XdgToplevel, XdgSurface> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        toplevel: &XdgToplevel,
        request: xdg_toplevel::Request,
        _data: &XdgSurface,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_toplevel::Request::SetTitle { title } => {
                if let Some(t) = state.toplevels.get_mut(&toplevel.id()) {
                    t.title = title.clone();
                    if t.created_window {
                        mac::post(WinCmd::Title {
                            id: t.window_id,
                            title,
                        });
                    }
                }
            }
            xdg_toplevel::Request::SetAppId { app_id } => {
                // Records the app identifier used to name the native macOS app in
                // --multiplex mode. Usually arrives before the first buffer, so it
                // is available by the time we spawn the window-host (see present).
                if let Some(t) = state.toplevels.get_mut(&toplevel.id()) {
                    t.app_id = app_id;
                }
            }
            xdg_toplevel::Request::Move { .. } => {
                // The client (dragging its CSD headerbar) asks us to move the
                // window; hand off to a native NSWindow drag.
                if let Some(t) = state.toplevels.get(&toplevel.id()) {
                    if t.created_window {
                        mac::post(WinCmd::StartMove { id: t.window_id });
                    }
                }
            }
            xdg_toplevel::Request::Resize { edges, .. } => {
                // The client dragged a CSD resize edge; drive an interactive
                // NSWindow resize on that edge.
                let edges = match edges {
                    WEnum::Value(e) => e as u32,
                    WEnum::Unknown(v) => v,
                };
                if let Some(t) = state.toplevels.get(&toplevel.id()) {
                    if t.created_window {
                        mac::post(WinCmd::StartResize {
                            id: t.window_id,
                            edges,
                        });
                    }
                }
            }
            xdg_toplevel::Request::SetMaximized => {
                if let Some(t) = state.toplevels.get_mut(&toplevel.id()) {
                    t.maximized = true;
                    mac::post(WinCmd::Maximize {
                        id: t.window_id,
                        on: true,
                    });
                }
            }
            xdg_toplevel::Request::UnsetMaximized => {
                if let Some(t) = state.toplevels.get_mut(&toplevel.id()) {
                    t.maximized = false;
                    mac::post(WinCmd::Maximize {
                        id: t.window_id,
                        on: false,
                    });
                }
            }
            xdg_toplevel::Request::SetFullscreen { .. } => {
                if let Some(t) = state.toplevels.get_mut(&toplevel.id()) {
                    t.fullscreen = true;
                    mac::post(WinCmd::Fullscreen {
                        id: t.window_id,
                        on: true,
                    });
                }
            }
            xdg_toplevel::Request::UnsetFullscreen => {
                if let Some(t) = state.toplevels.get_mut(&toplevel.id()) {
                    t.fullscreen = false;
                    mac::post(WinCmd::Fullscreen {
                        id: t.window_id,
                        on: false,
                    });
                }
            }
            xdg_toplevel::Request::SetMinimized => {
                if let Some(t) = state.toplevels.get(&toplevel.id()) {
                    mac::post(WinCmd::Minimize { id: t.window_id });
                }
            }
            xdg_toplevel::Request::SetMinSize { width, height } => {
                if let Some(t) = state.toplevels.get(&toplevel.id()) {
                    mac::post(WinCmd::SetMinSize {
                        id: t.window_id,
                        width,
                        height,
                    });
                }
            }
            xdg_toplevel::Request::SetMaxSize { width, height } => {
                if let Some(t) = state.toplevels.get(&toplevel.id()) {
                    mac::post(WinCmd::SetMaxSize {
                        id: t.window_id,
                        width,
                        height,
                    });
                }
            }
            xdg_toplevel::Request::Destroy => {
                state.reap_toplevel(&toplevel.id());
            }
            _ => {}
        }
    }

    /// Also fires when the client disconnects abruptly (e.g. the container is
    /// killed), so windows are torn down even without an explicit destroy.
    fn destroyed(state: &mut Self, _client: ClientId, resource: &XdgToplevel, _data: &XdgSurface) {
        state.reap_toplevel(&resource.id());
    }
}
