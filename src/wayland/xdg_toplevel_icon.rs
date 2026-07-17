//! xdg_toplevel_icon_v1 (staging): per-toplevel icons.
//!
//! Clients set a toplevel's icon either by themed name or by pixel buffer(s). In
//! --multiplex mode each app is its own macOS process, so a buffer icon becomes
//! that app's Dock/Cmd-Tab icon via `WinCmd::SetIcon` (→ setApplicationIconImage).
//! A name-only icon can't be resolved to artwork here (we have no Linux icon
//! theme on macOS), so the host keeps its generated identicon fallback.
//!
//! Icon state accumulates on the `xdg_toplevel_icon_v1` object (`IconRec` in
//! `mod.rs`); `set_icon` snapshots the best buffer's pixels and routes them to the
//! toplevel's window.

use super::*;

use wayland_protocols::xdg::toplevel_icon::v1::server::{
    xdg_toplevel_icon_manager_v1::{self, XdgToplevelIconManagerV1},
    xdg_toplevel_icon_v1::{self, XdgToplevelIconV1},
};

impl GlobalDispatch<XdgToplevelIconManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XdgToplevelIconManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let mgr = data_init.init(resource, ());
        // Advertise the icon sizes we can make good use of, then `done`.
        for size in [64, 128, 256] {
            mgr.icon_size(size);
        }
        mgr.done();
    }
}

impl Dispatch<XdgToplevelIconManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &XdgToplevelIconManagerV1,
        request: xdg_toplevel_icon_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_toplevel_icon_manager_v1::Request::CreateIcon { id } => {
                let icon = data_init.init(id, ());
                state.icon_objects.insert(icon.id(), IconRec::default());
            }
            xdg_toplevel_icon_manager_v1::Request::SetIcon { toplevel, icon } => {
                let Some((window_id, created)) = state
                    .toplevels
                    .get(&toplevel.id())
                    .map(|t| (t.window_id, t.created_window))
                else {
                    return;
                };
                // No icon object → "reset"; keep the host's identicon fallback.
                let Some(icon) = icon else {
                    if let Some(t) = state.toplevels.get_mut(&toplevel.id()) {
                        t.pending_icon = None;
                    }
                    return;
                };
                let Some(rec) = state.icon_objects.get(&icon.id()) else {
                    return;
                };
                // Pick the largest 8-bit BGRA buffer (by area); ignore name-only
                // icons (no themed-name resolution on macOS) and exotic pixel
                // formats (the Dock icon path expects plain BGRA).
                let Some(pb) = rec
                    .buffers
                    .iter()
                    .filter_map(|(buf, _scale)| buffer_to_pixels(buf))
                    .filter(|pb| pb.format == crate::mac::PixelFormat::Bgra8888)
                    .max_by_key(|pb| (pb.width as i64) * (pb.height as i64))
                else {
                    return;
                };
                let (w, h, stride, pixels) = (pb.width, pb.height, pb.stride, pb.bytes);
                if created {
                    // Window (and its host) already exist → apply now.
                    mac::post(WinCmd::SetIcon {
                        id: window_id,
                        width: w,
                        height: h,
                        stride,
                        pixels,
                    });
                } else if let Some(t) = state.toplevels.get_mut(&toplevel.id()) {
                    // Icon set before the first frame; flush it in `present` once
                    // the window/host is created (routing needs the window to exist).
                    t.pending_icon = Some((w, h, stride, pixels));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgToplevelIconV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &XdgToplevelIconV1,
        request: xdg_toplevel_icon_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_toplevel_icon_v1::Request::SetName { icon_name } => {
                if let Some(rec) = state.icon_objects.get_mut(&resource.id()) {
                    rec.name = Some(icon_name);
                }
            }
            xdg_toplevel_icon_v1::Request::AddBuffer { buffer, scale } => {
                if let Some(rec) = state.icon_objects.get_mut(&resource.id()) {
                    rec.buffers.push((buffer, scale));
                }
            }
            xdg_toplevel_icon_v1::Request::Destroy => {
                state.icon_objects.remove(&resource.id());
            }
            _ => {}
        }
    }
}
