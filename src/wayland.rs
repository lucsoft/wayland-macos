//! Wayland compositor side.
//!
//! Runs on a background thread. Implements just enough of the Wayland protocol
//! (`wl_compositor`, `wl_shm`, `xdg_shell`) for a client to create a toplevel and
//! present shm buffers. Each `xdg_toplevel` is mapped to a native `NSWindow`, and
//! every committed buffer is copied out and handed to the AppKit thread via
//! `mac::post`.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Instant;

use memmap2::{Mmap, MmapOptions};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use wayland_server::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_callback::WlCallback,
    wl_compositor::{self, WlCompositor},
    wl_data_device_manager::WlDataDeviceManager,
    wl_keyboard::{self, WlKeyboard},
    wl_output::{self, WlOutput},
    wl_pointer::{self, WlPointer},
    wl_region::{self, WlRegion},
    wl_seat::{self, WlSeat},
    wl_shm::{self, WlShm},
    wl_shm_pool::{self, WlShmPool},
    wl_subcompositor::{self, WlSubcompositor},
    wl_subsurface::WlSubsurface,
    wl_surface::{self, WlSurface},
    wl_touch::WlTouch,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource, WEnum,
};
use wayland_protocols::xdg::decoration::zv1::server::{
    zxdg_decoration_manager_v1::{self, ZxdgDecorationManagerV1},
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};
use wayland_protocols::xdg::shell::server::{
    xdg_popup::{self, XdgPopup},
    xdg_positioner::{self, XdgPositioner},
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use crate::input::{InputBus, InputEvent};
use crate::mac::{self, WinCmd};

const COMPOSITOR_VERSION: u32 = 6;
const SHM_VERSION: u32 = 1;
const XDG_WM_BASE_VERSION: u32 = 6;
const OUTPUT_VERSION: u32 = 2;
const SEAT_VERSION: u32 = 5;
const DATA_DEVICE_MANAGER_VERSION: u32 = 3;
const SUBCOMPOSITOR_VERSION: u32 = 1;
const DECORATION_VERSION: u32 = 1;

const OUTPUT_WIDTH: i32 = 1920;
const OUTPUT_HEIGHT: i32 = 1080;

// ---------------------------------------------------------------------------
// Shared memory
// ---------------------------------------------------------------------------

/// A client shm pool: an mmap of the fd the client passed via `wl_shm.create_pool`.
struct PoolMem {
    _fd: OwnedFd,
    map: Option<Mmap>,
}

fn map_pool(fd: OwnedFd, size: i32) -> PoolMem {
    let map = if size > 0 {
        // Read-only shared mapping; the client keeps writing into it, we snapshot on commit.
        unsafe { MmapOptions::new().len(size as usize).map(&fd) }.ok()
    } else {
        None
    };
    if map.is_none() {
        eprintln!("[wl] warning: failed to mmap shm pool ({size} bytes)");
    }
    PoolMem { _fd: fd, map }
}

/// User data for a `wl_buffer`: where its pixels live inside a pool.
struct BufferData {
    pool: Arc<PoolMem>,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    #[allow(dead_code)]
    format: u32,
}

// ---------------------------------------------------------------------------
// Compositor state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SurfaceRec {
    pending_buffer: Option<WlBuffer>,
    frame_callbacks: Vec<WlCallback>,
    window_id: Option<u32>,
    /// `xdg_surface.set_window_geometry` origin within the buffer — i.e. the CSD
    /// shadow margin. Used to align popups to actual content, not the buffer edge.
    geometry_offset: (i32, i32),
}

struct ToplevelRec {
    toplevel: XdgToplevel,
    xdg_surface: XdgSurface,
    wl_surface: WlSurface,
    title: String,
    window_id: u32,
    configured: bool,
    created_window: bool,
}

/// Placement request built up on an `xdg_positioner` before `get_popup`.
#[derive(Default, Clone, Copy)]
struct PositionerState {
    size: (i32, i32),
    anchor_rect: (i32, i32, i32, i32),
    offset: (i32, i32),
}

struct PopupRec {
    popup: XdgPopup,
    xdg_surface: XdgSurface,
    wl_surface: WlSurface,
    parent_window: u32,
    window_id: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    configured: bool,
    created_window: bool,
}

pub struct State {
    next_window_id: u32,
    next_serial: u32,
    start: Instant,
    surfaces: HashMap<ObjectId, SurfaceRec>,
    toplevels: HashMap<ObjectId, ToplevelRec>,
    /// wl_surface id -> xdg_toplevel id
    surface_toplevel: HashMap<ObjectId, ObjectId>,
    /// window id -> its wl_surface (for input focus/enter)
    window_surface: HashMap<u32, WlSurface>,
    /// xdg_positioner id -> its accumulated placement
    positioners: HashMap<ObjectId, PositionerState>,
    /// xdg_popup id -> record
    popups: HashMap<ObjectId, PopupRec>,
    /// wl_surface id -> xdg_popup id
    surface_popup: HashMap<ObjectId, ObjectId>,
    pointers: Vec<WlPointer>,
    keyboards: Vec<WlKeyboard>,
    /// window the pointer is currently over (the surface GTK thinks has the
    /// pointer); used to send a proper leave before any new enter.
    pointer_focus: Option<u32>,
    /// window currently holding keyboard focus
    focus_window: Option<u32>,
    /// shared xkb keymap file handed to every wl_keyboard
    keymap: Option<(std::fs::File, u32)>,
    /// Bridges to native macOS services (clipboard, ...).
    pub(crate) bridges: crate::bridges::Bridges,
}

impl State {
    fn new() -> Self {
        State {
            next_window_id: 1,
            next_serial: 1,
            start: Instant::now(),
            surfaces: HashMap::new(),
            toplevels: HashMap::new(),
            surface_toplevel: HashMap::new(),
            window_surface: HashMap::new(),
            positioners: HashMap::new(),
            popups: HashMap::new(),
            surface_popup: HashMap::new(),
            pointers: Vec::new(),
            keyboards: Vec::new(),
            pointer_focus: None,
            focus_window: None,
            keymap: make_keymap_file(),
            bridges: crate::bridges::Bridges::new(),
        }
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn serial(&mut self) -> u32 {
        let s = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        s
    }

    fn alloc_window_id(&mut self) -> u32 {
        let id = self.next_window_id;
        self.next_window_id += 1;
        id
    }

    fn handle_commit(&mut self, surface: &WlSurface) {
        let sid = surface.id();
        let tl_id = self.surface_toplevel.get(&sid).cloned();
        let pending = self
            .surfaces
            .get_mut(&sid)
            .and_then(|r| r.pending_buffer.take());

        // xdg_shell handshake: the first commit (typically with no buffer) must be
        // answered with a configure before the client will attach content.
        if let Some(ref tl_id) = tl_id {
            let needs_configure = self
                .toplevels
                .get(tl_id)
                .map(|t| !t.configured)
                .unwrap_or(false);
            if needs_configure {
                let serial = self.serial();
                if let Some(t) = self.toplevels.get(tl_id) {
                    // 0x0 lets the client pick its own size.
                    t.toplevel.configure(0, 0, Vec::new());
                    t.xdg_surface.configure(serial);
                }
                if let Some(t) = self.toplevels.get_mut(tl_id) {
                    t.configured = true;
                }
            }
        }

        // xdg_popup handshake: same as toplevels, but we also tell the client
        // where the popup goes (computed from its positioner).
        if let Some(pp_id) = self.surface_popup.get(&sid).cloned() {
            let needs_configure = self
                .popups
                .get(&pp_id)
                .map(|p| !p.configured)
                .unwrap_or(false);
            if needs_configure {
                let serial = self.serial();
                if let Some(p) = self.popups.get(&pp_id) {
                    p.popup.configure(p.x, p.y, p.w, p.h);
                    p.xdg_surface.configure(serial);
                }
                if let Some(p) = self.popups.get_mut(&pp_id) {
                    p.configured = true;
                }
            }
        }

        // Present an attached buffer.
        if let Some(buffer) = pending {
            self.present(&sid, &buffer);
            buffer.release();
        }

        // Fire frame callbacks so the client draws its next frame.
        let time = self.start.elapsed().as_millis() as u32;
        if let Some(rec) = self.surfaces.get_mut(&sid) {
            for cb in rec.frame_callbacks.drain(..) {
                cb.done(time);
            }
        }
    }

    fn present(&mut self, sid: &ObjectId, buffer: &WlBuffer) {
        let Some(bd) = buffer.data::<BufferData>() else {
            return;
        };
        let Some(ref map) = bd.pool.map else { return };
        if bd.width <= 0 || bd.height <= 0 || bd.stride < bd.width * 4 {
            return;
        }
        let start = bd.offset as usize;
        let len = (bd.stride as usize) * (bd.height as usize);
        if start + len > map.len() {
            return;
        }
        let mut pixels = vec![0u8; len];
        pixels.copy_from_slice(&map[start..start + len]);

        // Route the frame to the toplevel or popup that owns this surface.
        if let Some(tl_id) = self.surface_toplevel.get(sid).cloned() {
            let Some(t) = self.toplevels.get_mut(&tl_id) else {
                return;
            };
            if !t.created_window {
                mac::post(WinCmd::Create {
                    id: t.window_id,
                    width: bd.width,
                    height: bd.height,
                    title: t.title.clone(),
                });
                t.created_window = true;
                if let Some(sr) = self.surfaces.get_mut(sid) {
                    sr.window_id = Some(t.window_id);
                }
            }
            mac::post(WinCmd::Frame {
                id: t.window_id,
                width: bd.width,
                height: bd.height,
                stride: bd.stride,
                pixels,
            });
            return;
        }

        if let Some(pp_id) = self.surface_popup.get(sid).cloned() {
            // This popup's own shadow-margin offset within its buffer.
            let popup_geom = self.surfaces.get(sid).map(|s| s.geometry_offset).unwrap_or((0, 0));
            let (px, py, parent_window, window_id, created) = {
                let p = &self.popups[&pp_id];
                (p.x, p.y, p.parent_window, p.window_id, p.created_window)
            };
            // The parent's content offset within its buffer (its shadow margin).
            let parent_geom = self
                .window_surface
                .get(&parent_window)
                .map(|s| s.id())
                .and_then(|pid| self.surfaces.get(&pid))
                .map(|s| s.geometry_offset)
                .unwrap_or((0, 0));
            // Popup buffer top-left = parent content origin + configured offset,
            // minus the popup's own geometry offset.
            let adj_x = parent_geom.0 + px - popup_geom.0;
            let adj_y = parent_geom.1 + py - popup_geom.1;

            if !created {
                mac::post(WinCmd::CreatePopup {
                    id: window_id,
                    parent_id: parent_window,
                    x: adj_x,
                    y: adj_y,
                    width: bd.width,
                    height: bd.height,
                });
                if let Some(p) = self.popups.get_mut(&pp_id) {
                    p.created_window = true;
                }
            }
            mac::post(WinCmd::Frame {
                id: window_id,
                width: bd.width,
                height: bd.height,
                stride: bd.stride,
                pixels,
            });
        }
    }

    /// The native window id backing a surface, whether toplevel or popup.
    fn window_for_surface(&self, sid: &ObjectId) -> Option<u32> {
        if let Some(tl) = self.surface_toplevel.get(sid) {
            return self.toplevels.get(tl).map(|t| t.window_id);
        }
        if let Some(pp) = self.surface_popup.get(sid) {
            return self.popups.get(pp).map(|p| p.window_id);
        }
        None
    }

    fn process_input(&mut self, dh: &DisplayHandle, ev: InputEvent) {
        match ev {
            InputEvent::PointerEnter { window_id, x, y } => {
                // Already here? (e.g. a real mouseEntered after we synthesized one.)
                if self.pointer_focus == Some(window_id) {
                    return;
                }
                let Some(surface) = self.window_surface.get(&window_id).cloned() else {
                    return;
                };
                // The pointer can only be on one surface: leave the previous one
                // first, or the client ignores the new enter (this is why a popup
                // opening under the cursor needed a manual out-and-back before).
                if let Some(prev) = self.pointer_focus.take() {
                    if let Some(prev_surface) = self.window_surface.get(&prev).cloned() {
                        let serial = self.serial();
                        for p in pointers_for(&self.pointers, &prev_surface) {
                            p.leave(serial, &prev_surface);
                            frame_pointer(p);
                        }
                    }
                }
                let serial = self.serial();
                for p in pointers_for(&self.pointers, &surface) {
                    p.enter(serial, &surface, x, y);
                    frame_pointer(p);
                }
                self.pointer_focus = Some(window_id);
            }
            InputEvent::PointerMotion { window_id, x, y } => {
                let Some(surface) = self.window_surface.get(&window_id).cloned() else {
                    return;
                };
                let t = self.time();
                for p in pointers_for(&self.pointers, &surface) {
                    p.motion(t, x, y);
                    frame_pointer(p);
                }
            }
            InputEvent::PointerButton {
                window_id,
                button,
                pressed,
            } => {
                let Some(surface) = self.window_surface.get(&window_id).cloned() else {
                    return;
                };
                let serial = self.serial();
                let t = self.time();
                let bstate = if pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                for p in pointers_for(&self.pointers, &surface) {
                    p.button(serial, t, button, bstate);
                    frame_pointer(p);
                }
            }
            InputEvent::PointerAxis { dx, dy } => {
                let t = self.time();
                for p in self.pointers.iter().filter(|p| p.is_alive()) {
                    if dy != 0.0 {
                        p.axis(t, wl_pointer::Axis::VerticalScroll, dy);
                    }
                    if dx != 0.0 {
                        p.axis(t, wl_pointer::Axis::HorizontalScroll, dx);
                    }
                    frame_pointer(p);
                }
            }
            InputEvent::PointerLeave { window_id } => {
                // Ignore a stale leave if we already moved focus elsewhere (e.g.
                // the synthetic enter for a popup already left this surface).
                if self.pointer_focus != Some(window_id) {
                    return;
                }
                let Some(surface) = self.window_surface.get(&window_id).cloned() else {
                    return;
                };
                let serial = self.serial();
                for p in pointers_for(&self.pointers, &surface) {
                    p.leave(serial, &surface);
                    frame_pointer(p);
                }
                self.pointer_focus = None;
            }
            InputEvent::Focus { window_id, focused } => {
                let Some(surface) = self.window_surface.get(&window_id).cloned() else {
                    return;
                };
                let serial = self.serial();
                for k in keyboards_for(&self.keyboards, &surface) {
                    if focused {
                        k.enter(serial, &surface, Vec::new());
                    } else {
                        k.leave(serial, &surface);
                    }
                }
                self.focus_window = if focused { Some(window_id) } else { None };
                // Now that the client is focused (and its toolkit is up), it's
                // safe to advertise the macOS clipboard selection to it.
                if focused {
                    if let Some(client) = surface.client() {
                        self.bridges.clipboard.advertise_to_client(dh, &client);
                    }
                }
            }
            InputEvent::Key { keycode, pressed } => {
                let Some(surface) = self
                    .focus_window
                    .and_then(|w| self.window_surface.get(&w).cloned())
                else {
                    return;
                };
                let serial = self.serial();
                let t = self.time();
                let ks = if pressed {
                    wl_keyboard::KeyState::Pressed
                } else {
                    wl_keyboard::KeyState::Released
                };
                for k in keyboards_for(&self.keyboards, &surface) {
                    k.key(serial, t, keycode, ks);
                }
            }
            InputEvent::Modifiers { depressed, locked } => {
                let Some(surface) = self
                    .focus_window
                    .and_then(|w| self.window_surface.get(&w).cloned())
                else {
                    return;
                };
                let serial = self.serial();
                for k in keyboards_for(&self.keyboards, &surface) {
                    k.modifiers(serial, depressed, 0, locked, 0);
                }
            }
            InputEvent::Resize {
                window_id,
                width,
                height,
            } => {
                // Ask the toplevel to repaint at the new size.
                let res = self
                    .toplevels
                    .values()
                    .find(|t| t.window_id == window_id)
                    .map(|t| (t.toplevel.clone(), t.xdg_surface.clone()));
                if let Some((toplevel, xdg_surface)) = res {
                    let serial = self.serial();
                    let states = (xdg_toplevel::State::Activated as u32)
                        .to_ne_bytes()
                        .to_vec();
                    toplevel.configure(width.max(1), height.max(1), states);
                    xdg_surface.configure(serial);
                }
            }
            InputEvent::PopupDismiss { window_id } => {
                // Click outside a grabbing menu: tell the client to dismiss it.
                let popup = self
                    .popups
                    .values()
                    .find(|p| p.window_id == window_id)
                    .map(|p| p.popup.clone());
                eprintln!("[wl] popup dismiss window {window_id}");
                if let Some(popup) = popup {
                    popup.popup_done();
                }
                mac::post(WinCmd::SetGrab { window: None });
            }
            // TODO(clipboard): placeholder so the build stays exhaustive; the
            // pasteboard->selection bridge logic lives elsewhere / is WIP.
            InputEvent::MacClipboard { .. } => {}
        }
    }
}

/// Live pointers belonging to the same client as `surface`.
fn pointers_for<'a>(
    pointers: &'a [WlPointer],
    surface: &'a WlSurface,
) -> impl Iterator<Item = &'a WlPointer> {
    pointers
        .iter()
        .filter(move |p| p.is_alive() && same_client(*p, surface))
}

fn keyboards_for<'a>(
    keyboards: &'a [WlKeyboard],
    surface: &'a WlSurface,
) -> impl Iterator<Item = &'a WlKeyboard> {
    keyboards
        .iter()
        .filter(move |k| k.is_alive() && same_client(*k, surface))
}

fn frame_pointer(p: &WlPointer) {
    if p.version() >= 5 {
        p.frame();
    }
}

fn same_client<R: Resource>(res: &R, surface: &WlSurface) -> bool {
    match (res.client(), surface.client()) {
        (Some(a), Some(b)) => a.id() == b.id(),
        _ => false,
    }
}

/// Write the embedded xkb keymap to a file and return it plus its size
/// (including the trailing NUL, as required by `wl_keyboard.keymap`).
fn make_keymap_file() -> Option<(std::fs::File, u32)> {
    use std::io::Write;
    const KEYMAP: &str = include_str!("keymap.xkb");
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let path = format!("{dir}/wlmac-keymap-{}", std::process::id());
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    f.write_all(KEYMAP.as_bytes()).ok()?;
    f.write_all(&[0]).ok()?; // NUL terminator
    f.flush().ok()?;
    let _ = std::fs::remove_file(&path); // unlink; fd stays valid
    Some((f, (KEYMAP.len() + 1) as u32))
}

// ---------------------------------------------------------------------------
// Client bookkeeping
// ---------------------------------------------------------------------------

struct ClientState;
impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

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
// wl_shm
// ---------------------------------------------------------------------------

impl GlobalDispatch<WlShm, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlShm>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<WlShm, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlShm,
        request: wl_shm::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, size } = request {
            data_init.init(id, Arc::new(map_pool(fd, size)));
        }
    }
}

impl Dispatch<WlShmPool, Arc<PoolMem>> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlShmPool,
        request: wl_shm_pool::Request,
        data: &Arc<PoolMem>,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                let format = match format {
                    WEnum::Value(f) => f as u32,
                    WEnum::Unknown(v) => v,
                };
                data_init.init(
                    id,
                    BufferData {
                        pool: data.clone(),
                        offset,
                        width,
                        height,
                        stride,
                        format,
                    },
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, BufferData> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlBuffer,
        _request: wl_buffer::Request,
        _data: &BufferData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

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
            xdg_surface::Request::SetWindowGeometry { x, y, .. } => {
                if let Some(rec) = state.surfaces.get_mut(&wl_surface.id()) {
                    rec.geometry_offset = (x, y);
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
                        window_id,
                        configured: false,
                        created_window: false,
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
                let pos = state
                    .positioners
                    .get(&positioner.id())
                    .copied()
                    .unwrap_or_default();
                // Position relative to the parent surface: below the anchor rect,
                // plus the requested offset (good enough for dropdown menus).
                let x = pos.anchor_rect.0 + pos.offset.0;
                let y = pos.anchor_rect.1 + pos.anchor_rect.3 + pos.offset.1;
                let (w, h) = if pos.size != (0, 0) {
                    pos.size
                } else {
                    (200, 200)
                };
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
                        w,
                        h,
                        configured: false,
                        created_window: false,
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
                    eprintln!("[wl] popup grab -> window {}", p.window_id);
                    mac::post(WinCmd::SetGrab {
                        window: Some(p.window_id),
                    });
                }
            }
            xdg_popup::Request::Destroy => {
                if let Some(p) = state.popups.remove(&popup.id()) {
                    eprintln!("[wl] popup destroy window {}", p.window_id);
                    state.surface_popup.remove(&p.wl_surface.id());
                    state.window_surface.remove(&p.window_id);
                    if p.created_window {
                        mac::post(WinCmd::Destroy { id: p.window_id });
                    }
                }
            }
            _ => {}
        }
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
            xdg_toplevel::Request::Move { .. } => {
                // The client (dragging its CSD headerbar) asks us to move the
                // window; hand off to a native NSWindow drag.
                if let Some(t) = state.toplevels.get(&toplevel.id()) {
                    if t.created_window {
                        mac::post(WinCmd::StartMove { id: t.window_id });
                    }
                }
            }
            xdg_toplevel::Request::Destroy => {
                if let Some(t) = state.toplevels.remove(&toplevel.id()) {
                    state.surface_toplevel.remove(&t.wl_surface.id());
                    if t.created_window {
                        mac::post(WinCmd::Destroy { id: t.window_id });
                    }
                }
            }
            _ => {}
        }
    }
}

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
        output.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            OUTPUT_WIDTH,
            OUTPUT_HEIGHT,
            60_000,
        );
        if output.version() >= 2 {
            output.scale(crate::input::scale());
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
    }
}

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
        _: &mut Self,
        _: &Client,
        _: &WlPointer,
        _: <WlPointer as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
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

// ---------------------------------------------------------------------------
// wl_data_device_manager (clipboard/DnD)
//
// The Dispatch/GlobalDispatch handlers for the data-device family live in the
// clipboard bridge (`crate::bridges::clipboard`), which bridges the Wayland
// selection to the macOS pasteboard. The global is still created in `run`.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// wl_subcompositor (subsurfaces — advertised; not composited in this PoC)
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
        _state: &mut Self,
        _client: &Client,
        _resource: &WlSubcompositor,
        request: wl_subcompositor::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_subcompositor::Request::GetSubsurface { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<WlSubsurface, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlSubsurface,
        _: <WlSubsurface as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// xdg-decoration: force server-side decorations so GTK drops its own titlebar
// (the NSWindow titlebar becomes the only chrome, and buffers stay opaque).
// ---------------------------------------------------------------------------

impl GlobalDispatch<ZxdgDecorationManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZxdgDecorationManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZxdgDecorationManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZxdgDecorationManagerV1,
        request: zxdg_decoration_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_decoration_manager_v1::Request::GetToplevelDecoration { id, .. } = request {
            let deco = data_init.init(id, ());
            // Tell the client we decorate: it must not draw its own titlebar.
            deco.configure(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        }
    }
}

impl Dispatch<ZxdgToplevelDecorationV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &ZxdgToplevelDecorationV1,
        request: zxdg_toplevel_decoration_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Whatever the client asks for, keep decorations server-side.
        match request {
            zxdg_toplevel_decoration_v1::Request::SetMode { .. }
            | zxdg_toplevel_decoration_v1::Request::UnsetMode => {
                resource.configure(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Entry point, runs on a background thread.
pub fn run(bus: Arc<InputBus>) {
    ensure_runtime_dir();

    let mut display: Display<State> = Display::new().expect("create wayland display");
    let dh = display.handle();

    dh.create_global::<State, WlCompositor, _>(COMPOSITOR_VERSION, ());
    dh.create_global::<State, WlShm, _>(SHM_VERSION, ());
    dh.create_global::<State, XdgWmBase, _>(XDG_WM_BASE_VERSION, ());
    dh.create_global::<State, WlOutput, _>(OUTPUT_VERSION, ());
    dh.create_global::<State, WlSeat, _>(SEAT_VERSION, ());
    dh.create_global::<State, WlDataDeviceManager, _>(DATA_DEVICE_MANAGER_VERSION, ());
    dh.create_global::<State, WlSubcompositor, _>(SUBCOMPOSITOR_VERSION, ());
    dh.create_global::<State, ZxdgDecorationManagerV1, _>(DECORATION_VERSION, ());

    let socket = ListeningSocket::bind_auto("wayland", 1..32).expect("bind wayland socket");
    let name = socket
        .socket_name()
        .and_then(|s| s.to_str())
        .unwrap_or("wayland-?")
        .to_string();
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
    eprintln!("[wl] listening on {runtime}/{name}");
    eprintln!("[wl] point a client at it with:");
    eprintln!("       export XDG_RUNTIME_DIR={runtime}");
    eprintln!("       export WAYLAND_DISPLAY={name}");

    let mut state = State::new();
    let mut dh = display.handle();

    // Self-pipe: the AppKit thread writes a byte here to wake this poll loop
    // when it has queued input events.
    let (wake_r, wake_w) = make_pipe();
    let wake_r_fd = wake_r.as_raw_fd();
    bus.set_waker(wake_w);

    let display_fd = display.as_fd().as_raw_fd();
    let socket_fd = socket.as_raw_fd();

    loop {
        let mut fds = [
            libc::pollfd {
                fd: display_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_r_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("[wl] poll error: {err}");
            break;
        }

        // Accept any pending client connections.
        loop {
            match socket.accept() {
                Ok(Some(stream)) => {
                    if let Err(e) = dh.insert_client(stream, Arc::new(ClientState)) {
                        eprintln!("[wl] insert_client failed: {e}");
                    } else {
                        eprintln!("[wl] client connected");
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[wl] accept error: {e}");
                    break;
                }
            }
        }

        // Drain queued input events (and the wakeup byte(s)).
        if fds[2].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 64];
            while unsafe {
                libc::read(wake_r_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            } > 0
            {}
            for ev in bus.drain() {
                match ev {
                    // The macOS pasteboard changed: let the clipboard bridge
                    // re-advertise it to Wayland clients.
                    InputEvent::MacClipboard { text } => {
                        state.bridges.clipboard.set_mac_selection(&dh, text);
                    }
                    other => state.process_input(&dh, other),
                }
            }
        }

        if let Err(e) = display.dispatch_clients(&mut state) {
            eprintln!("[wl] dispatch error: {e}");
        }
        if let Err(e) = display.flush_clients() {
            eprintln!("[wl] flush error: {e}");
        }
        // The flush transmitted any clipboard `send` fds; drop our write ends so
        // the reader threads see EOF.
        state.bridges.clipboard.flush_done();
    }
}

/// A non-blocking pipe `(read, write)`.
fn make_pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        panic!("pipe: {}", std::io::Error::last_os_error());
    }
    for fd in fds {
        let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        unsafe { libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) };
    }
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Wayland requires `XDG_RUNTIME_DIR`; macOS doesn't set one, so create it.
fn ensure_runtime_dir() {
    if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
        return;
    }
    let uid = unsafe { libc::getuid() };
    let dir = format!("/tmp/wayland-macos-{uid}");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    std::env::set_var("XDG_RUNTIME_DIR", &dir);
}
