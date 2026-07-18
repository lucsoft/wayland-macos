//! Wayland compositor side.
//!
//! Runs on a background thread. Implements just enough of the Wayland protocol
//! (`wl_compositor`, `wl_shm`, `xdg_shell`) for a client to create a toplevel and
//! present shm buffers. Each `xdg_toplevel` is mapped to a native `NSWindow`, and
//! every committed buffer is copied out and handed to the AppKit thread via
//! `mac::post`.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use memmap2::{Mmap, MmapOptions};
use rustix::event::{PollFd, PollFlags, Timespec};
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
    wl_subsurface::{self, WlSubsurface},
    wl_surface::{self, WlSurface},
    wl_touch::WlTouch,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource, WEnum,
};
use wayland_protocols::wp::cursor_shape::v1::server::{
    wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::{self, WpCursorShapeManagerV1},
};
use wayland_protocols::wp::single_pixel_buffer::v1::server::wp_single_pixel_buffer_manager_v1::{
    self, WpSinglePixelBufferManagerV1,
};
use wayland_protocols::wp::viewporter::server::{
    wp_viewport::{self, WpViewport},
    wp_viewporter::{self, WpViewporter},
};
use wayland_protocols::wp::pointer_constraints::zv1::server::{
    zwp_confined_pointer_v1::ZwpConfinedPointerV1,
    zwp_locked_pointer_v1::ZwpLockedPointerV1,
    zwp_pointer_constraints_v1::{self, ZwpPointerConstraintsV1},
};
use wayland_protocols::wp::relative_pointer::zv1::server::{
    zwp_relative_pointer_manager_v1::{self, ZwpRelativePointerManagerV1},
    zwp_relative_pointer_v1::ZwpRelativePointerV1,
};
use wayland_protocols::wp::presentation_time::server::{
    wp_presentation::{self, WpPresentation},
    wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_protocols::xdg::decoration::zv1::server::{
    zxdg_decoration_manager_v1::{self, ZxdgDecorationManagerV1},
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};
use wayland_protocols::xdg::xdg_output::zv1::server::{
    zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
    zxdg_output_v1::ZxdgOutputV1,
};
use wayland_protocols::xdg::shell::server::{
    xdg_popup::{self, XdgPopup},
    xdg_positioner::{self, XdgPositioner},
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols::xdg::activation::v1::server::xdg_activation_v1::XdgActivationV1;
use wayland_protocols::xdg::toplevel_icon::v1::server::xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1;
use wayland_protocols::xdg::dialog::v1::server::xdg_wm_dialog_v1::XdgWmDialogV1;
use wayland_protocols::wp::fractional_scale::v1::server::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::server::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use wayland_protocols::wp::primary_selection::zv1::server::zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1;
use wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use crate::input::{InputBus, InputEvent};
use crate::mac::{self, ColorDesc, PixelFormat, Primaries, TransferFn, WinCmd};
use wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::WpColorManagementOutputV1,
    wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1,
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::WpColorManagerV1,
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_info_v1::WpImageDescriptionInfoV1,
    wp_image_description_v1::WpImageDescriptionV1,
};

const COMPOSITOR_VERSION: u32 = 6;
const SHM_VERSION: u32 = 1;
const XDG_WM_BASE_VERSION: u32 = 6;
const OUTPUT_VERSION: u32 = 4;
const SEAT_VERSION: u32 = 5;
const DATA_DEVICE_MANAGER_VERSION: u32 = 3;
const SUBCOMPOSITOR_VERSION: u32 = 1;
const DECORATION_VERSION: u32 = 1;
const CURSOR_SHAPE_VERSION: u32 = 1;
const SINGLE_PIXEL_BUFFER_VERSION: u32 = 1;
const VIEWPORTER_VERSION: u32 = 1;
const POINTER_CONSTRAINTS_VERSION: u32 = 1;
const RELATIVE_POINTER_VERSION: u32 = 1;
const PRESENTATION_VERSION: u32 = 1;
const XDG_OUTPUT_VERSION: u32 = 3;
const XDG_ACTIVATION_VERSION: u32 = 1;
const FRACTIONAL_SCALE_VERSION: u32 = 1;
const XDG_DIALOG_VERSION: u32 = 1;
const KEYBOARD_SHORTCUTS_INHIBIT_VERSION: u32 = 1;
const PRIMARY_SELECTION_VERSION: u32 = 1;
const LAYER_SHELL_VERSION: u32 = 4;
const COLOR_MANAGEMENT_VERSION: u32 = 1;
const XDG_TOPLEVEL_ICON_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Shared memory
// ---------------------------------------------------------------------------

// A client shm pool: an mmap of the fd the client passed via `wl_shm.create_pool`.
// The map lives behind an `RwLock` because the pool is shared (`Arc<PoolMem>`)
// across its buffers, yet `wl_shm_pool.resize` can re-mmap it to a larger size.

mod compositor;
mod shm;
mod xdg_shell;
mod output;
mod seat;
mod decoration;
mod cursor_shape;
mod single_pixel;
mod viewporter;
mod pointer_constraints;
mod relative_pointer;
mod presentation;
mod xdg_output;
mod xdg_activation;
mod fractional_scale;
mod xdg_dialog;
mod keyboard_shortcuts_inhibit;
mod primary_selection;
mod layer_shell;
mod clipboard;
mod color_management;
mod xdg_toplevel_icon;

struct PoolMem {
    fd: OwnedFd,
    map: RwLock<Option<Mmap>>,
}

impl PoolMem {
    /// Re-map the pool at a new size, keeping the same fd. Clients grow a pool
    /// (via `wl_shm_pool.resize`) and then place larger buffers in it; without
    /// this, `present` would reject every buffer past the original size. Some
    /// toolkits (KWin) create a tiny pool and grow it repeatedly.
    fn resize(&self, size: i32) {
        if size <= 0 {
            return;
        }
        match unsafe { MmapOptions::new().len(size as usize).map(&self.fd) } {
            Ok(m) => *self.map.write().unwrap_or_else(|e| e.into_inner()) = Some(m),
            Err(e) => warn!(target: "wl", "failed to remap shm pool ({size} bytes): {e}"),
        }
    }
}

fn map_pool(fd: OwnedFd, size: i32) -> PoolMem {
    let map = if size > 0 {
        // Read-only shared mapping; the client keeps writing into it, we snapshot on commit.
        unsafe { MmapOptions::new().len(size as usize).map(&fd) }.ok()
    } else {
        None
    };
    if map.is_none() {
        warn!(target: "wl", "failed to mmap shm pool ({size} bytes)");
    }
    PoolMem {
        fd,
        map: RwLock::new(map),
    }
}

/// CLOCK_MONOTONIC as (seconds, nanoseconds) — the clock advertised to
/// presentation-time clients.
fn monotonic_now() -> (u64, u32) {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Raw pixels copied out of a `wl_buffer` (physical px), tagged with their memory
/// layout so the AppKit side can build a matching `CGImage` without a lossy
/// conversion. Used for both window presentation and cursor surfaces.
struct PixelBuf {
    width: i32,
    height: i32,
    stride: i32,
    format: PixelFormat,
    bytes: Vec<u8>,
}

/// Map a `wl_shm` format code to the `PixelFormat` the AppKit side understands
/// and its bytes-per-pixel. Formats we don't specifically recognise are treated
/// as ordinary 8-bit BGRA (4 bytes) — the historical behaviour.
fn shm_pixel_format(format: u32) -> (PixelFormat, usize) {
    // Compare against the `wl_shm::Format` fourcc values (cast is a const expr).
    const XRGB2101010: u32 = wl_shm::Format::Xrgb2101010 as u32;
    const ARGB2101010: u32 = wl_shm::Format::Argb2101010 as u32;
    const ABGR16161616F: u32 = wl_shm::Format::Abgr16161616f as u32;
    match format {
        XRGB2101010 | ARGB2101010 => (PixelFormat::Rgb2101010, 4),
        ABGR16161616F => (PixelFormat::Rgba16F, 8),
        _ => (PixelFormat::Bgra8888, 4),
    }
}

/// Copy a `wl_buffer`'s contents out to raw pixels (physical px). Used for both
/// window presentation and cursor surfaces.
fn buffer_to_pixels(buffer: &WlBuffer) -> Option<PixelBuf> {
    let bd = buffer.data::<BufferData>()?;
    match &bd.kind {
        BufferKind::Shm {
            pool,
            offset,
            width,
            height,
            stride,
            format,
        } => {
            // Reject anything that could index outside the pool. Every step uses
            // checked arithmetic on `usize`: a client controls all of these fields,
            // and an unchecked `offset as usize` (a negative offset becomes ~1.8e19)
            // or an `i32` multiply (`width * bpp`, `stride * height`) would slip past
            // the bounds check and panic the slice below — taking the whole
            // compositor, and every window, down. See `shm_buffer_*` tests.
            if *width <= 0 || *height <= 0 || *offset < 0 || *stride < 0 {
                return None;
            }
            let (pf, bpp) = shm_pixel_format(*format);
            let w = *width as usize;
            let h = *height as usize;
            let off = *offset as usize;
            let strd = *stride as usize;
            if strd < w.checked_mul(bpp)? {
                return None;
            }
            let len = strd.checked_mul(h)?;
            let end = off.checked_add(len)?;
            let mut bytes = vec![0u8; len];
            let guard = pool.map.read().unwrap_or_else(|e| e.into_inner());
            let map = guard.as_ref()?;
            if end > map.len() {
                return None;
            }
            bytes.copy_from_slice(&map[off..end]);
            Some(PixelBuf {
                width: *width,
                height: *height,
                stride: *stride,
                format: pf,
                bytes,
            })
        }
        BufferKind::SinglePixel { bgra } => Some(PixelBuf {
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Bgra8888,
            bytes: bgra.to_vec(),
        }),
    }
}

/// User data for a `wl_buffer`.
struct BufferData {
    kind: BufferKind,
}

enum BufferKind {
    /// Pixels live inside a client shm pool.
    Shm {
        pool: Arc<PoolMem>,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        /// The `wl_shm` format fourcc; selects the `PixelFormat` (8-bit vs 10-bit
        /// vs float16) the frame is decoded as (see `shm_pixel_format`).
        format: u32,
    },
    /// A 1x1 solid-colour buffer (`wp_single_pixel_buffer`). Stored as one BGRA
    /// pixel to match the little-endian ARGB8888 byte order `present` emits.
    /// KWin's Wayland backend refuses to start unless this protocol is offered.
    SinglePixel { bgra: [u8; 4] },
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
    /// `xdg_surface.set_window_geometry` size (logical points) — the window's
    /// visible bounds excluding CSD shadow margins. When set, the native window is
    /// sized to this (not the full buffer), so a client that pads its buffer with
    /// shadows doesn't make the window grow on every resize round-trip.
    geometry_size: (i32, i32),
    /// `wp_viewport.set_destination`: the surface's logical size in points,
    /// decoupled from the buffer's pixel size. KWin renders a tiny (often 1x1)
    /// buffer and declares the real output size this way, so the window must
    /// follow the destination, not the buffer.
    viewport_dst: Option<(i32, i32)>,
    /// `wp_color_management_surface_v1.set_image_description`: the color
    /// characteristics staged for the next commit. `Some(desc)` sets it,
    /// `Some(None)` unsets it, `None` means unchanged this cycle.
    pending_color: Option<Option<ColorDesc>>,
    /// The color description currently in effect (double-buffered from
    /// `pending_color` on commit). `None` = ordinary sRGB SDR.
    color: Option<ColorDesc>,
}

struct ToplevelRec {
    toplevel: XdgToplevel,
    xdg_surface: XdgSurface,
    wl_surface: WlSurface,
    title: String,
    /// `xdg_toplevel.set_app_id` — the app identifier (e.g. "org.gnome.Console").
    /// In --multiplex mode this names the app's native macOS Dock/Cmd-Tab entry.
    app_id: String,
    window_id: u32,
    configured: bool,
    created_window: bool,
    maximized: bool,
    fullscreen: bool,
    /// The client engaged the xdg-decoration protocol expecting us to draw the
    /// window frame (server-side). Such windows get a native macOS titlebar;
    /// CSD toolkits (GTK/libadwaita, which never create a decoration) stay
    /// borderless and draw their own chrome into the buffer.
    wants_ssd: bool,
    /// Icon pixels (w, h, stride, BGRA) from `xdg_toplevel_icon` that arrived
    /// before the window existed; flushed as `WinCmd::SetIcon` once it's created.
    pending_icon: Option<(i32, i32, i32, Vec<u8>)>,
}

/// Icon state accumulated on an `xdg_toplevel_icon_v1` before `set_icon` assigns
/// it to a toplevel: an optional themed name and/or one or more pixel buffers (at
/// different scales). See `xdg_toplevel_icon.rs`.
#[derive(Default)]
struct IconRec {
    name: Option<String>,
    buffers: Vec<(WlBuffer, i32)>,
}

/// Placement request built up on an `xdg_positioner` before `get_popup`.
#[derive(Default, Clone, Copy)]
struct PositionerState {
    size: (i32, i32),
    anchor_rect: (i32, i32, i32, i32),
    offset: (i32, i32),
    /// `xdg_positioner.anchor` edge/corner of the anchor rect (enum value).
    anchor: u32,
    /// `xdg_positioner.gravity` direction the popup extends (enum value).
    gravity: u32,
}

impl PositionerState {
    /// Popup top-left relative to the parent, resolving anchor + gravity + offset.
    /// Anchor/gravity enum values (shared by both): 0 none, 1 top, 2 bottom,
    /// 3 left, 4 right, 5 top_left, 6 bottom_left, 7 top_right, 8 bottom_right.
    fn popup_origin(&self) -> (i32, i32) {
        let (ax, ay, aw, ah) = self.anchor_rect;
        // Anchor point on the anchor rect.
        let anchor_x = match self.anchor {
            3 | 5 | 6 => ax,           // left / top_left / bottom_left
            4 | 7 | 8 => ax + aw,      // right / top_right / bottom_right
            _ => ax + aw / 2,          // none / top / bottom -> horizontal center
        };
        let anchor_y = match self.anchor {
            1 | 5 | 7 => ay,           // top / top_left / top_right
            2 | 6 | 8 => ay + ah,      // bottom / bottom_left / bottom_right
            _ => ay + ah / 2,          // none / left / right -> vertical center
        };
        // Gravity: which way the popup extends from the anchor point, i.e. which
        // corner of the popup sits at the anchor point.
        let (pw, ph) = self.size;
        let x = match self.gravity {
            3 | 5 | 6 => anchor_x - pw, // left: popup is to the left
            4 | 7 | 8 => anchor_x,      // right: popup is to the right
            _ => anchor_x - pw / 2,     // none/top/bottom: centered horizontally
        };
        let y = match self.gravity {
            1 | 5 | 7 => anchor_y - ph, // top: popup is above
            2 | 6 | 8 => anchor_y,      // bottom: popup is below
            _ => anchor_y - ph / 2,     // none/left/right: centered vertically
        };
        (x + self.offset.0, y + self.offset.1)
    }
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

/// A `zwlr_layer_surface_v1`: a docked bar/panel anchored to a screen edge.
struct LayerSurfaceRec {
    layer_surface: ZwlrLayerSurfaceV1,
    wl_surface: WlSurface,
    window_id: u32,
    /// Anchor bitfield: top=1, bottom=2, left=4, right=8.
    anchor: u32,
    /// Requested size (0 in a dimension = compositor decides = span the output).
    size: (u32, u32),
    /// Margins from the anchored edges: (top, right, bottom, left).
    margin: (i32, i32, i32, i32),
    /// Exclusive zone (logical points) to reserve so other windows don't overlap
    /// the bar. Positive = reserve that distance from the anchored edge.
    exclusive: i32,
    /// Keyboard interactivity: 0 = none, 1 = exclusive, 2 = on-demand. Non-zero
    /// means the surface wants keyboard input (e.g. fuzzel, a launcher) and we
    /// give it focus when it maps.
    keyboard_interactivity: u32,
    configured: bool,
    created_window: bool,
}

/// A `wl_subsurface`: a child surface composited onto a parent surface.
struct SubsurfaceRec {
    /// The child `wl_surface` this subsurface gives a role to.
    surface: ObjectId,
    /// The parent `wl_surface` (may itself be another subsurface).
    parent: ObjectId,
    /// Position within the parent surface (surface-local coordinates).
    x: i32,
    y: i32,
    /// Stable id used as the CALayer sublayer key on the AppKit side.
    sub_id: u32,
}

pub struct State {
    next_window_id: u32,
    next_serial: u32,
    start: Instant,
    /// wl client -> stable small app id, used in --multiplex mode to route each
    /// window to the per-app window-host process. Never pruned (ids stay unique).
    client_app_ids: HashMap<ClientId, u32>,
    /// xdg_toplevel_icon_v1 object id -> its accumulated icon state.
    icon_objects: HashMap<ObjectId, IconRec>,
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
    /// wl_surface id -> presentation-feedback objects awaiting the next present
    presentation_feedback: HashMap<ObjectId, Vec<WpPresentationFeedback>>,
    /// The surface a client designated as the pointer cursor (wl_pointer.set_cursor),
    /// and its hotspot in surface-local coordinates.
    cursor_surface: Option<ObjectId>,
    cursor_hotspot: (i32, i32),
    /// wl_subsurface object id -> record. KWin composites its whole nested output
    /// into a subsurface, so these must be composited onto their root window.
    subsurfaces: HashMap<ObjectId, SubsurfaceRec>,
    /// wl_surface id -> the wl_subsurface object that gives it the subsurface role.
    surface_subsurface: HashMap<ObjectId, ObjectId>,
    /// zwlr_layer_surface object id -> record (docked bars/panels).
    layer_surfaces: HashMap<ObjectId, LayerSurfaceRec>,
    /// wl_surface id -> its zwlr_layer_surface object.
    surface_layer: HashMap<ObjectId, ObjectId>,
    /// Clipboard: `wl_data_device` selection ⇆ the macOS `NSPasteboard`.
    pub(crate) clipboard: clipboard::Clipboard,
    /// X11-style primary (middle-click) selection, client-to-client only.
    primary_selection: primary_selection::PrimarySelection,
    /// `wp_image_description_v1` object id -> the resolved color description it
    /// represents. A surface references one of these via `set_image_description`.
    image_descs: HashMap<ObjectId, ColorDesc>,
    /// In-flight `wp_image_description_creator_params_v1` accumulators, keyed by
    /// the creator object id; consumed when the client calls `create`.
    param_creators: HashMap<ObjectId, color_management::ParamAccum>,
}

impl State {
    fn new() -> Self {
        Self::with_keymap(make_keymap_file())
    }

    /// Headless constructor for tests: skips `make_keymap_file()` (no xkbcommon /
    /// xkeyboard-config dependency and no Carbon layout probe), so tests run on CI
    /// without a display. Behaviour is otherwise identical to `new`.
    #[cfg(test)]
    fn new_headless() -> Self {
        Self::with_keymap(None)
    }

    fn with_keymap(keymap: Option<(std::fs::File, u32)>) -> Self {
        State {
            next_window_id: 1,
            next_serial: 1,
            start: Instant::now(),
            client_app_ids: HashMap::new(),
            icon_objects: HashMap::new(),
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
            keymap,
            presentation_feedback: HashMap::new(),
            cursor_surface: None,
            cursor_hotspot: (0, 0),
            subsurfaces: HashMap::new(),
            surface_subsurface: HashMap::new(),
            layer_surfaces: HashMap::new(),
            surface_layer: HashMap::new(),
            clipboard: clipboard::Clipboard::default(),
            primary_selection: primary_selection::PrimarySelection::default(),
            image_descs: HashMap::new(),
            param_creators: HashMap::new(),
        }
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Send `presented` to every presentation-feedback object waiting on `sid`,
    /// timestamped with CLOCK_MONOTONIC (the clock we advertise in `clock_id`).
    /// The event is a destructor, so the objects are consumed.
    fn fire_presentation_feedback(&mut self, sid: &ObjectId) {
        let Some(feedbacks) = self.presentation_feedback.remove(sid) else {
            return;
        };
        let (sec, nsec) = monotonic_now();
        let sec_hi = (sec >> 32) as u32;
        let sec_lo = (sec & 0xffff_ffff) as u32;
        // ~60Hz refresh in nanoseconds; seq unknown, report 0.
        let refresh = 16_666_667u32;
        for fb in feedbacks {
            fb.presented(
                sec_hi,
                sec_lo,
                nsec,
                refresh,
                0,
                0,
                wp_presentation_feedback::Kind::Vsync,
            );
        }
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

    /// If the just-designated cursor surface already has a buffer attached (client
    /// attached before calling set_cursor), turn it into a cursor now. The common
    /// toolkit order is set_cursor *then* commit, which handle_commit covers.
    fn apply_cursor_surface(&mut self, sid: &ObjectId) {
        let buffer = self
            .surfaces
            .get_mut(sid)
            .and_then(|r| r.pending_buffer.take());
        if let Some(buffer) = buffer {
            if let Some(pb) = buffer_to_pixels(&buffer) {
                let (hx, hy) = self.cursor_hotspot;
                mac::post(WinCmd::SetCursorImage {
                    width: pb.width,
                    height: pb.height,
                    stride: pb.stride,
                    hotspot_x: hx,
                    hotspot_y: hy,
                    // Wayland cursor buffers are rendered at the output scale.
                    scale: crate::input::scale(),
                    pixels: pb.bytes,
                });
            }
            buffer.release();
        }
    }

    fn handle_commit(&mut self, surface: &WlSurface) {
        let sid = surface.id();
        let tl_id = self.surface_toplevel.get(&sid).cloned();
        let pending = self
            .surfaces
            .get_mut(&sid)
            .and_then(|r| r.pending_buffer.take());

        // Apply any staged color description (double-buffered, like the buffer).
        if let Some(rec) = self.surfaces.get_mut(&sid) {
            if let Some(new_color) = rec.pending_color.take() {
                rec.color = new_color;
            }
        }

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
                    // Advertise which window-management actions we support, or GTK
                    // greys out the maximize/minimize/fullscreen buttons.
                    if t.toplevel.version() >= 5 {
                        // WindowMenu=1, Maximize=2, Fullscreen=3, Minimize=4
                        let caps: Vec<u8> = [1u32, 2, 3, 4]
                            .iter()
                            .flat_map(|c| c.to_ne_bytes())
                            .collect();
                        t.toplevel.wm_capabilities(caps);
                    }
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

        // wlr-layer-shell handshake: on the first commit tell the bar its size.
        // If it left a dimension 0 (anchored to opposite edges) we span the output.
        if let Some(ls_id) = self.surface_layer.get(&sid).cloned() {
            let needs_configure = self
                .layer_surfaces
                .get(&ls_id)
                .map(|l| !l.configured)
                .unwrap_or(false);
            if needs_configure {
                let serial = self.serial();
                let s = crate::input::scale().max(1);
                let (pw, ph) = crate::input::output_size();
                let (ow, oh) = (pw / s, ph / s);
                if let Some(l) = self.layer_surfaces.get(&ls_id) {
                    let w = if l.size.0 > 0 { l.size.0 as i32 } else { ow };
                    let h = if l.size.1 > 0 { l.size.1 as i32 } else { oh };
                    l.layer_surface
                        .configure(serial, w.max(1) as u32, h.max(1) as u32);
                }
                if let Some(l) = self.layer_surfaces.get_mut(&ls_id) {
                    l.configured = true;
                }
            }
        }

        // Present an attached buffer.
        if let Some(buffer) = pending {
            if self.cursor_surface.as_ref() == Some(&sid) {
                // This surface is the pointer cursor: turn its buffer into a
                // native cursor image instead of a window.
                if let Some(pb) = buffer_to_pixels(&buffer) {
                    let (hx, hy) = self.cursor_hotspot;
                    mac::post(WinCmd::SetCursorImage {
                        width: pb.width,
                        height: pb.height,
                        stride: pb.stride,
                        hotspot_x: hx,
                        hotspot_y: hy,
                        // Wayland cursor buffers are rendered at the output scale.
                        scale: crate::input::scale(),
                        pixels: pb.bytes,
                    });
                }
            } else if self.surface_subsurface.contains_key(&sid) {
                // A subsurface: composite it as a sublayer of its root window.
                // (KWin renders its whole nested output into a subsurface.)
                self.present_subsurface(&sid, &buffer);
            } else if self.surface_layer.contains_key(&sid) {
                // A docked bar/panel (wlr-layer-shell).
                self.present_layer(&sid, &buffer);
            } else {
                self.present(&sid, &buffer);
            }
            buffer.release();
        }
        // Frame callbacks and presentation feedback are deferred to the periodic
        // vblank tick (see vblank_tick) so the client is paced to a steady ~60Hz
        // rather than acked instantly — KWin's render loop needs a real vblank
        // cadence to keep its nested output stable.
    }

    /// Fire deferred frame callbacks and presentation feedback for every surface.
    /// Called on a ~16ms cadence from the run loop to emulate a vblank.
    fn vblank_tick(&mut self) {
        let time = self.start.elapsed().as_millis() as u32;
        for rec in self.surfaces.values_mut() {
            for cb in rec.frame_callbacks.drain(..) {
                cb.done(time);
            }
        }
        let sids: Vec<ObjectId> = self.presentation_feedback.keys().cloned().collect();
        for sid in sids {
            self.fire_presentation_feedback(&sid);
        }
    }

    /// Tear down a toplevel and its native window. Safe to call more than once
    /// and for unknown ids — used by both explicit destroy and client disconnect.
    fn reap_toplevel(&mut self, toplevel_id: &ObjectId) {
        if let Some(t) = self.toplevels.remove(toplevel_id) {
            self.surface_toplevel.remove(&t.wl_surface.id());
            self.window_surface.remove(&t.window_id);
            if t.created_window {
                mac::post(WinCmd::Destroy { id: t.window_id });
            }
        }
    }

    /// Tear down a popup and its native window (see `reap_toplevel`).
    fn reap_popup(&mut self, popup_id: &ObjectId) {
        if let Some(p) = self.popups.remove(popup_id) {
            self.surface_popup.remove(&p.wl_surface.id());
            self.window_surface.remove(&p.window_id);
            // Always send Destroy — even if the popup never mapped a window. A
            // popup that requested a grab but never painted (e.g. a menu the client
            // abandoned) still left the mac-side pointer grab set; Destroy clears it
            // by window id, so input isn't swallowed forever. Destroy is a no-op for
            // a window that was never created.
            mac::post(WinCmd::Destroy { id: p.window_id });
        }
    }

    fn present(&mut self, sid: &ObjectId, buffer: &WlBuffer) {
        // Resolve the frame to raw pixels (+layout) regardless of buffer kind.
        let Some(pb) = buffer_to_pixels(buffer) else {
            return;
        };
        let (width, height, stride, format, pixels) =
            (pb.width, pb.height, pb.stride, pb.format, pb.bytes);
        // The surface's negotiated color characteristics (HDR/wide-gamut), if any.
        let color = self.surfaces.get(sid).and_then(|s| s.color);

        // Logical (point) size: a `wp_viewport` destination if the client set one,
        // else (0, 0) — AppKit then derives the size from the buffer pixels and the
        // output scale. KWin declares its real output size via viewport while
        // attaching a tiny buffer, so without this the window would be 1x1.
        let (dst_w, dst_h) = self
            .surfaces
            .get(sid)
            .and_then(|s| s.viewport_dst)
            .unwrap_or((0, 0));

        // CSD window geometry (content bounds within the buffer, excluding shadow
        // margins), in logical points. (0,0,0,0) when the client set none.
        let (geom_off, geom_sz) = self
            .surfaces
            .get(sid)
            .map(|s| (s.geometry_offset, s.geometry_size))
            .unwrap_or(((0, 0), (0, 0)));
        let geom = (geom_off.0, geom_off.1, geom_sz.0, geom_sz.1);

        // Route the frame to the toplevel or popup that owns this surface.
        if let Some(tl_id) = self.surface_toplevel.get(sid).cloned() {
            let Some(t) = self.toplevels.get_mut(&tl_id) else {
                return;
            };
            if !t.created_window {
                // --multiplex: assign this window to its client's app before the
                // Create so the router spawns/targets the right host. Disjoint
                // field borrow (client_app_ids vs toplevels), so `t` stays valid.
                let app_key =
                    app_key_for(&mut self.client_app_ids, t.wl_surface.client().map(|c| c.id()));
                let name = app_display_name(&t.app_id, &t.title);
                mac::assign_window(t.window_id, app_key, &name, true);
                mac::post(WinCmd::Create {
                    id: t.window_id,
                    width,
                    height,
                    dst_w,
                    dst_h,
                    decorated: t.wants_ssd,
                    title: t.title.clone(),
                    geom,
                });
                t.created_window = true;
                // Flush an icon that arrived before the window existed (routing
                // needs win_to_app, populated by assign_window just above).
                if let Some((iw, ih, istride, ipx)) = t.pending_icon.take() {
                    mac::post(WinCmd::SetIcon {
                        id: t.window_id,
                        width: iw,
                        height: ih,
                        stride: istride,
                        pixels: ipx,
                    });
                }
                if let Some(sr) = self.surfaces.get_mut(sid) {
                    sr.window_id = Some(t.window_id);
                }
            }
            mac::post(WinCmd::Frame {
                id: t.window_id,
                width,
                height,
                stride,
                dst_w,
                dst_h,
                pixels,
                format,
                color,
                geom,
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
                    width,
                    height,
                });
                if let Some(p) = self.popups.get_mut(&pp_id) {
                    p.created_window = true;
                }
            }
            mac::post(WinCmd::Frame {
                id: window_id,
                width,
                height,
                stride,
                dst_w,
                dst_h,
                pixels,
                format,
                color,
                geom: (0, 0, 0, 0),
            });
        }
    }

    /// Tear down a layer surface (docked bar) and its native window.
    fn reap_layer_surface(&mut self, ls_obj: &ObjectId) {
        if let Some(rec) = self.layer_surfaces.remove(ls_obj) {
            self.surface_layer.remove(&rec.wl_surface.id());
            self.window_surface.remove(&rec.window_id);
            if self.focus_window == Some(rec.window_id) {
                self.focus_window = None;
            }
            if rec.created_window {
                mac::post(WinCmd::Destroy { id: rec.window_id });
            }
        }
        self.recompute_reserved();
    }

    /// Recompute the work-area insets reserved by docked bars (their exclusive
    /// zones) so toplevels avoid overlapping them, and publish to the AppKit side.
    /// A bar anchored to a single edge reserves its exclusive zone from that edge.
    fn recompute_reserved(&self) {
        let (mut top, mut right, mut bottom, mut left) = (0i32, 0, 0, 0);
        for rec in self.layer_surfaces.values() {
            let z = rec.exclusive;
            if z <= 0 {
                continue;
            }
            let a = rec.anchor;
            let (t, b, l, r) = (a & 1 != 0, a & 2 != 0, a & 4 != 0, a & 8 != 0);
            if t && !b {
                top = top.max(z);
            } else if b && !t {
                bottom = bottom.max(z);
            }
            if l && !r {
                left = left.max(z);
            } else if r && !l {
                right = right.max(z);
            }
        }
        crate::input::set_reserved_insets(top, right, bottom, left);
        // In multiplex mode the bar and the toplevels live in different host
        // processes, so push the insets to each host's copy too (no-op otherwise).
        crate::mac::broadcast_insets(top, right, bottom, left);
    }

    /// Map/update a docked bar (wlr-layer-shell): create a borderless floating
    /// NSWindow anchored to a screen edge on first buffer, then push frames.
    fn present_layer(&mut self, sid: &ObjectId, buffer: &WlBuffer) {
        let Some(ls_id) = self.surface_layer.get(sid).cloned() else {
            return;
        };
        let Some(pb) = buffer_to_pixels(buffer) else {
            return;
        };
        let (width, height, stride, format, pixels) =
            (pb.width, pb.height, pb.stride, pb.format, pb.bytes);
        let color = self.surfaces.get(sid).and_then(|s| s.color);
        let (window_id, anchor, margin, created, kbd, surface) = {
            let l = &self.layer_surfaces[&ls_id];
            (
                l.window_id,
                l.anchor,
                l.margin,
                l.created_window,
                l.keyboard_interactivity,
                l.wl_surface.clone(),
            )
        };
        if !created {
            // --multiplex: a layer-shell bar gets its own host too, but as an
            // Accessory app (regular=false) so the bar itself has no Dock tile.
            let app_key =
                app_key_for(&mut self.client_app_ids, surface.client().map(|c| c.id()));
            mac::assign_window(window_id, app_key, "bar", false);
            mac::post(WinCmd::CreateLayer {
                id: window_id,
                width,
                height,
                anchor,
                margin_top: margin.0,
                margin_right: margin.1,
                margin_bottom: margin.2,
                margin_left: margin.3,
                keyboard: kbd != 0,
            });
            if let Some(l) = self.layer_surfaces.get_mut(&ls_id) {
                l.created_window = true;
            }
            if let Some(sr) = self.surfaces.get_mut(sid) {
                sr.window_id = Some(window_id);
            }
            // A layer surface that wants keyboard input (e.g. fuzzel) should get
            // focus as soon as it maps, without needing a pointer hover first.
            if kbd != 0 && self.focus_window != Some(window_id) {
                let serial = self.serial();
                for k in keyboards_for(&self.keyboards, &surface) {
                    k.enter(serial, &surface, Vec::new());
                }
                self.focus_window = Some(window_id);
            }
        }
        mac::post(WinCmd::Frame {
            id: window_id,
            width,
            height,
            stride,
            dst_w: 0,
            dst_h: 0,
            pixels,
            format,
            color,
            geom: (0, 0, 0, 0),
        });
    }

    /// Walk a subsurface's parent chain to its root window, summing positions.
    /// Returns `(window_id, offset_x, offset_y, leaf_sub_id)` in surface points.
    fn resolve_subsurface(&self, surface_id: &ObjectId) -> Option<(u32, i32, i32, u32)> {
        let leaf_obj = self.surface_subsurface.get(surface_id)?;
        let sub_id = self.subsurfaces.get(leaf_obj)?.sub_id;
        let (mut ox, mut oy) = (0, 0);
        let mut cur = surface_id.clone();
        // Break cycles: the wl_subsurface spec forbids a surface being its own
        // ancestor, but a malformed client can still create one (surface ==
        // parent, or a longer loop). Without this guard the walk never reaches a
        // root window and spins forever, hanging the whole wayland thread — and
        // with it every client and all input. Track visited surfaces and stop if
        // one repeats.
        let mut seen = std::collections::HashSet::new();
        loop {
            if !seen.insert(cur.clone()) {
                return None;
            }
            let sub_obj = self.surface_subsurface.get(&cur)?;
            let rec = self.subsurfaces.get(sub_obj)?;
            ox += rec.x;
            oy += rec.y;
            if let Some(wid) = self.window_for_surface(&rec.parent) {
                return Some((wid, ox, oy, sub_id));
            }
            cur = rec.parent.clone();
        }
    }

    /// Tear down a subsurface and remove its sublayer. Called on explicit destroy
    /// and when its surface is destroyed.
    fn reap_subsurface(&mut self, sub_obj: &ObjectId) {
        let Some(rec) = self.subsurfaces.get(sub_obj) else {
            return;
        };
        let sub_id = rec.sub_id;
        let window = self.resolve_subsurface(&rec.surface).map(|(w, _, _, _)| w);
        let surface = rec.surface.clone();
        self.subsurfaces.remove(sub_obj);
        self.surface_subsurface.remove(&surface);
        if let Some(window_id) = window {
            mac::post(WinCmd::SubDestroy { window_id, sub_id });
        }
    }

    /// Composite a subsurface's buffer as a sublayer of its root window.
    fn present_subsurface(&mut self, sid: &ObjectId, buffer: &WlBuffer) {
        let Some((window_id, ox, oy, sub_id)) = self.resolve_subsurface(sid) else {
            return;
        };
        let Some(pb) = buffer_to_pixels(buffer) else {
            return;
        };
        let (width, height, stride, format, pixels) =
            (pb.width, pb.height, pb.stride, pb.format, pb.bytes);
        let color = self.surfaces.get(sid).and_then(|s| s.color);
        let (dst_w, dst_h) = self
            .surfaces
            .get(sid)
            .and_then(|s| s.viewport_dst)
            .unwrap_or((0, 0));
        mac::post(WinCmd::SubFrame {
            window_id,
            sub_id,
            x: ox,
            y: oy,
            width,
            height,
            stride,
            dst_w,
            dst_h,
            pixels,
            format,
            color,
        });
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

    /// Move pointer focus to `window_id`, sending leave to the previous surface and
    /// enter to the new one. No-op if already focused there. Returns the focused
    /// surface. A `wl_pointer.enter` MUST precede any motion — some clients (KWin's
    /// nested backend) crash on motion with no prior enter.
    fn ensure_pointer_focus(&mut self, window_id: u32, x: f64, y: f64) -> Option<WlSurface> {
        let surface = self.window_surface.get(&window_id).cloned()?;
        if self.pointer_focus == Some(window_id) {
            return Some(surface);
        }
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
        Some(surface)
    }

    fn process_input(&mut self, dh: &DisplayHandle, ev: InputEvent) {
        match ev {
            InputEvent::PointerEnter { window_id, x, y } => {
                self.ensure_pointer_focus(window_id, x, y);
            }
            InputEvent::PointerMotion { window_id, x, y } => {
                // Guarantee an enter first: a window can appear under the cursor
                // (macOS sends mouseMoved but not mouseEntered), and motion without
                // a prior enter crashes KWin's nested backend.
                let Some(surface) = self.ensure_pointer_focus(window_id, x, y) else {
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
                // Ensure the client has pointer focus before a button (same reason
                // as motion): a click on a freshly-shown window may arrive first.
                self.ensure_pointer_focus(window_id, 0.0, 0.0);
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
                // Deduplicate: sending a second keyboard enter while the client
                // already holds focus (or a leave when it doesn't) is a protocol
                // violation — Qt logs "Unexpected wl_keyboard.enter" and mishandles
                // it (broken menus, null-pointer warnings).
                if focused && self.focus_window == Some(window_id) {
                    return;
                }
                if !focused && self.focus_window != Some(window_id) {
                    return;
                }
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
                        self.clipboard.advertise_to_client(dh, &client);
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
                // Ask the toplevel to repaint at the new size, reflecting its
                // maximized/fullscreen state so it decorates itself correctly.
                let res = self
                    .toplevels
                    .values()
                    .find(|t| t.window_id == window_id)
                    .map(|t| {
                        (
                            t.toplevel.clone(),
                            t.xdg_surface.clone(),
                            t.maximized,
                            t.fullscreen,
                        )
                    });
                if let Some((toplevel, xdg_surface, maximized, fullscreen)) = res {
                    let serial = self.serial();
                    let mut states = Vec::new();
                    states.extend_from_slice(&(xdg_toplevel::State::Activated as u32).to_ne_bytes());
                    if maximized {
                        states
                            .extend_from_slice(&(xdg_toplevel::State::Maximized as u32).to_ne_bytes());
                    }
                    if fullscreen {
                        states.extend_from_slice(
                            &(xdg_toplevel::State::Fullscreen as u32).to_ne_bytes(),
                        );
                    }
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
                debug!(target: "wl", "popup dismiss window {window_id}");
                if let Some(popup) = popup {
                    popup.popup_done();
                }
                mac::post(WinCmd::SetGrab { window: None });
            }
            // TODO(clipboard): placeholder so the build stays exhaustive; the
            // pasteboard->selection logic lives elsewhere / is WIP.
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

/// Map a client to its stable small app id (see `State::client_app_ids`),
/// assigning a fresh one on first sight. `None` client → 0. Takes only the map so
/// it can be called while another `State` field (e.g. `toplevels`) is borrowed.
fn app_key_for(map: &mut HashMap<ClientId, u32>, cid: Option<ClientId>) -> u32 {
    match cid {
        Some(cid) => match map.get(&cid) {
            Some(k) => *k,
            None => {
                let k = map.len() as u32;
                map.insert(cid, k);
                k
            }
        },
        None => 0,
    }
}

/// Human-facing app name for a toplevel's native macOS Dock/Cmd-Tab entry in
/// --multiplex mode. Prefers the app id's last dotted component, capitalized
/// (e.g. "org.gnome.Console" -> "Console"), else the window title, else a
/// fallback. Reserved characters that can't appear in a symlink name are stripped
/// (the name becomes an executable path basename — see router::spawn_helper).
fn app_display_name(app_id: &str, title: &str) -> String {
    let raw = if !app_id.is_empty() {
        let last = app_id.rsplit('.').next().unwrap_or(app_id);
        let mut c = last.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => app_id.to_string(),
        }
    } else if !title.is_empty() && title != "Wayland Window" {
        title.to_string()
    } else {
        "wayland-macos".to_string()
    };
    // Keep it a safe, single-path-component basename.
    let cleaned: String = raw
        .chars()
        .map(|ch| if ch == '/' || ch == '\0' || ch == ':' { '-' } else { ch })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "wayland-macos".to_string()
    } else {
        cleaned.chars().take(64).collect()
    }
}

fn same_client<R: Resource>(res: &R, surface: &WlSurface) -> bool {
    match (res.client(), surface.client()) {
        (Some(a), Some(b)) => a.id() == b.id(),
        _ => false,
    }
}

/// Compile an xkb keymap at runtime for the desired layout (via libxkbcommon +
/// xkeyboard-config) and write it to an unlinked temp file, returning the file
/// plus its size (incl. the trailing NUL, as `wl_keyboard.keymap` requires).
///
/// Layout resolution: `WLMAC_LAYOUT` env (e.g. "de") → the current macOS keyboard
/// layout → "us". No pre-baked keymap file, no build-time generation.
fn make_keymap_file() -> Option<(std::fs::File, u32)> {
    use std::io::Write;
    use xkbcommon::xkb;

    // Layout was detected on the main thread and cached (see input::mac_layout);
    // we must NOT call the Carbon TIS API from this (Wayland) thread — it races
    // AppKit's TSM init and aborts the process.
    let layout = std::env::var("WLMAC_LAYOUT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(crate::input::mac_layout)
        .unwrap_or_else(|| "us".to_string());

    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "",       // rules (default: evdev)
        "",       // model
        &layout,  // layout, e.g. "de"
        "",       // variant
        None,     // options
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    info!(target: "wl", "xkb keymap: layout '{layout}' ({} bytes)", text.len());

    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let path = format!("{dir}/wlmac-keymap-{}", std::process::id());
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    f.write_all(text.as_bytes()).ok()?;
    f.write_all(&[0]).ok()?; // NUL terminator
    f.flush().ok()?;
    let _ = std::fs::remove_file(&path); // unlink; fd stays valid
    Some((f, (text.len() + 1) as u32))
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
    dh.create_global::<State, WpCursorShapeManagerV1, _>(CURSOR_SHAPE_VERSION, ());
    dh.create_global::<State, WpSinglePixelBufferManagerV1, _>(SINGLE_PIXEL_BUFFER_VERSION, ());
    dh.create_global::<State, WpViewporter, _>(VIEWPORTER_VERSION, ());
    dh.create_global::<State, ZwpPointerConstraintsV1, _>(POINTER_CONSTRAINTS_VERSION, ());
    dh.create_global::<State, ZwpRelativePointerManagerV1, _>(RELATIVE_POINTER_VERSION, ());
    dh.create_global::<State, WpPresentation, _>(PRESENTATION_VERSION, ());
    dh.create_global::<State, ZxdgOutputManagerV1, _>(XDG_OUTPUT_VERSION, ());
    dh.create_global::<State, XdgActivationV1, _>(XDG_ACTIVATION_VERSION, ());
    dh.create_global::<State, WpFractionalScaleManagerV1, _>(FRACTIONAL_SCALE_VERSION, ());
    dh.create_global::<State, XdgWmDialogV1, _>(XDG_DIALOG_VERSION, ());
    dh.create_global::<State, ZwpKeyboardShortcutsInhibitManagerV1, _>(
        KEYBOARD_SHORTCUTS_INHIBIT_VERSION,
        (),
    );
    dh.create_global::<State, ZwpPrimarySelectionDeviceManagerV1, _>(PRIMARY_SELECTION_VERSION, ());
    dh.create_global::<State, ZwlrLayerShellV1, _>(LAYER_SHELL_VERSION, ());
    dh.create_global::<State, WpColorManagerV1, _>(COLOR_MANAGEMENT_VERSION, ());
    dh.create_global::<State, XdgToplevelIconManagerV1, _>(XDG_TOPLEVEL_ICON_VERSION, ());

    let socket = ListeningSocket::bind_auto("wayland", 1..32).expect("bind wayland socket");
    let name = socket
        .socket_name()
        .and_then(|s| s.to_str())
        .unwrap_or("wayland-?")
        .to_string();
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
    info!(target: "wl", "listening on {runtime}/{name}");
    info!(
        target: "wl",
        "point a client at it with:\n       export XDG_RUNTIME_DIR={runtime}\n       export WAYLAND_DISPLAY={name}"
    );

    let mut state = State::new();
    let mut dh = display.handle();

    // Self-pipe: the AppKit thread writes a byte here to wake this poll loop
    // when it has queued input events.
    let (wake_r, wake_w) = make_pipe();
    bus.set_waker(wake_w);

    // Emulate a ~60Hz vblank: frame callbacks and presentation feedback are
    // deferred (see vblank_tick) and released on this cadence, so clients are
    // paced to a steady refresh. KWin's nested render loop depends on a real
    // vblank rhythm to keep its output stable.
    const FRAME_INTERVAL: Duration = Duration::from_millis(16);
    let mut last_tick = Instant::now();

    loop {
        // Wake at least every frame interval so the vblank tick fires on time.
        let remaining = FRAME_INTERVAL.saturating_sub(last_tick.elapsed());
        let timeout = Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: remaining.subsec_nanos() as i64,
        };
        // Poll the display, listening socket, and the AppKit wakeup pipe. The
        // PollFds borrow those fds only for the poll call itself, so they're
        // dropped before `dispatch_clients` (which needs `&mut display`).
        let wake_ready = {
            let mut fds = [
                PollFd::new(&display, PollFlags::IN),
                PollFd::new(&socket, PollFlags::IN),
                PollFd::new(&wake_r, PollFlags::IN),
            ];
            match rustix::event::poll(&mut fds, Some(&timeout)) {
                Ok(_) => fds[2].revents().contains(PollFlags::IN),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => {
                    error!(target: "wl", "poll error: {e}");
                    break;
                }
            }
        };

        // Accept any pending client connections.
        loop {
            match socket.accept() {
                Ok(Some(stream)) => {
                    if let Err(e) = dh.insert_client(stream, Arc::new(ClientState)) {
                        error!(target: "wl", "insert_client failed: {e}");
                    } else {
                        info!(target: "wl", "client connected");
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!(target: "wl", "accept error: {e}");
                    break;
                }
            }
        }

        // Drain queued input events (and the wakeup byte(s)).
        if wake_ready {
            let mut buf = [0u8; 64];
            while rustix::io::read(&wake_r, &mut buf).unwrap_or(0) > 0 {}
            for ev in bus.drain() {
                match ev {
                    // The macOS pasteboard changed: let the clipboard
                    // re-advertise it to Wayland clients.
                    InputEvent::MacClipboard { text } => {
                        state.clipboard.set_mac_selection(&dh, text);
                    }
                    other => state.process_input(&dh, other),
                }
            }
        }

        // Release deferred frame callbacks / presentation feedback on the vblank
        // cadence, giving clients (KWin) a steady refresh rhythm.
        if last_tick.elapsed() >= FRAME_INTERVAL {
            state.vblank_tick();
            last_tick = Instant::now();
        }

        if let Err(e) = display.dispatch_clients(&mut state) {
            error!(target: "wl", "dispatch error: {e}");
        }
        if let Err(e) = display.flush_clients() {
            error!(target: "wl", "flush error: {e}");
        }
        // The flush transmitted any clipboard `send` fds; drop our write ends so
        // the reader threads see EOF. Same for the primary selection.
        state.clipboard.flush_done();
        state.primary_selection.flush_done();
    }
}

/// A non-blocking pipe `(read, write)`. macOS has no `pipe2`, so both ends are
/// switched to non-blocking with `fcntl` after creation.
fn make_pipe() -> (OwnedFd, OwnedFd) {
    let (r, w) = rustix::pipe::pipe().unwrap_or_else(|e| panic!("pipe: {e}"));
    for fd in [&r, &w] {
        let flags = rustix::fs::fcntl_getfl(fd).unwrap_or_else(|e| panic!("fcntl F_GETFL: {e}"));
        rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
            .unwrap_or_else(|e| panic!("fcntl F_SETFL: {e}"));
    }
    (r, w)
}

/// Wayland requires `XDG_RUNTIME_DIR`; macOS doesn't set one, so create it.
fn ensure_runtime_dir() {
    if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
        return;
    }
    let uid = rustix::process::getuid().as_raw();
    let dir = format!("/tmp/wayland-macos-{uid}");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    std::env::set_var("XDG_RUNTIME_DIR", &dir);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Register every global the compositor offers on `dh`. Mirrors the `create_global`
/// block in `run()`; used by the in-process test harness so tests exercise the
/// real `GlobalDispatch`/`Dispatch` code paths.
#[cfg(test)]
fn register_test_globals(dh: &DisplayHandle) {
    dh.create_global::<State, WlCompositor, _>(COMPOSITOR_VERSION, ());
    dh.create_global::<State, WlShm, _>(SHM_VERSION, ());
    dh.create_global::<State, XdgWmBase, _>(XDG_WM_BASE_VERSION, ());
    dh.create_global::<State, WlOutput, _>(OUTPUT_VERSION, ());
    dh.create_global::<State, WlSeat, _>(SEAT_VERSION, ());
    dh.create_global::<State, WlDataDeviceManager, _>(DATA_DEVICE_MANAGER_VERSION, ());
    dh.create_global::<State, WlSubcompositor, _>(SUBCOMPOSITOR_VERSION, ());
    dh.create_global::<State, ZxdgDecorationManagerV1, _>(DECORATION_VERSION, ());
    dh.create_global::<State, WpCursorShapeManagerV1, _>(CURSOR_SHAPE_VERSION, ());
    dh.create_global::<State, WpSinglePixelBufferManagerV1, _>(SINGLE_PIXEL_BUFFER_VERSION, ());
    dh.create_global::<State, WpViewporter, _>(VIEWPORTER_VERSION, ());
    dh.create_global::<State, ZwpPointerConstraintsV1, _>(POINTER_CONSTRAINTS_VERSION, ());
    dh.create_global::<State, ZwpRelativePointerManagerV1, _>(RELATIVE_POINTER_VERSION, ());
    dh.create_global::<State, WpPresentation, _>(PRESENTATION_VERSION, ());
    dh.create_global::<State, ZxdgOutputManagerV1, _>(XDG_OUTPUT_VERSION, ());
    dh.create_global::<State, XdgActivationV1, _>(XDG_ACTIVATION_VERSION, ());
    dh.create_global::<State, WpFractionalScaleManagerV1, _>(FRACTIONAL_SCALE_VERSION, ());
    dh.create_global::<State, XdgWmDialogV1, _>(XDG_DIALOG_VERSION, ());
    dh.create_global::<State, ZwpKeyboardShortcutsInhibitManagerV1, _>(
        KEYBOARD_SHORTCUTS_INHIBIT_VERSION,
        (),
    );
    dh.create_global::<State, ZwpPrimarySelectionDeviceManagerV1, _>(PRIMARY_SELECTION_VERSION, ());
    dh.create_global::<State, ZwlrLayerShellV1, _>(LAYER_SHELL_VERSION, ());
    dh.create_global::<State, WpColorManagerV1, _>(COLOR_MANAGEMENT_VERSION, ());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Proof-of-concept: the shm pool maps its backing fd and `resize` re-maps it
    /// to a larger size (the KWin "tiny pool, grow repeatedly" pattern). Exercises
    /// real `wl_shm_pool` bookkeeping with no AppKit/GPU involved.
    #[test]
    fn shm_pool_maps_and_resizes() {
        // Back the pool with a real (over-allocated) temp file, as a client fd would.
        let mut path = std::env::temp_dir();
        path.push(format!("wlmac-test-pool-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open temp pool file");
        file.write_all(&[0u8; 8192]).expect("write pool backing bytes");
        file.flush().expect("flush pool file");
        let _ = std::fs::remove_file(&path); // unlink; the fd stays valid

        let fd = OwnedFd::from(file);
        let pool = map_pool(fd, 4096);
        let len = |p: &PoolMem| p.map.read().unwrap_or_else(|e| e.into_inner()).as_ref().map(|m| m.len());
        assert_eq!(len(&pool), Some(4096), "initial mapping matches create size");

        // Grow the pool: resize must re-map at the new (larger) length.
        pool.resize(8192);
        assert_eq!(len(&pool), Some(8192), "resize remaps to the grown size");

        // A non-positive size is ignored (guards against bogus resize requests).
        pool.resize(0);
        assert_eq!(len(&pool), Some(8192), "resize(0) leaves the mapping intact");
    }

    /// --multiplex Dock/Cmd-Tab naming: `app_display_name` prefers the app id's
    /// last dotted component (capitalized), falls back to the title, and never
    /// yields a string that would break a symlink basename (see spawn_helper).
    #[test]
    fn app_display_name_prefers_app_id_then_title() {
        // app_id wins, last dotted component, capitalized.
        assert_eq!(app_display_name("org.gnome.Console", "Terminal"), "Console");
        assert_eq!(app_display_name("org.mozilla.firefox", "Mozilla Firefox"), "Firefox");
        assert_eq!(app_display_name("kgx", ""), "Kgx");
        // No app_id → use the title, unless it's the placeholder.
        assert_eq!(app_display_name("", "My Editor"), "My Editor");
        assert_eq!(app_display_name("", "Wayland Window"), "wayland-macos");
        assert_eq!(app_display_name("", ""), "wayland-macos");
        // Path/colon/NUL separators can't appear in a symlink basename.
        assert!(!app_display_name("com.foo/bar", "").contains('/'));
        assert_eq!(app_display_name("a/b:c", ""), "A-b-c");
    }

    /// Regression #7: `xdg_positioner` anchor + gravity + offset resolve to the
    /// correct popup origin (see `PositionerState::popup_origin`). Enum values:
    /// 1 top, 2 bottom, 5 top_left, 6 bottom_left, 7 top_right, 8 bottom_right.
    #[test]
    fn positioner_anchor_gravity_resolves_popup_origin() {
        // bottom-left anchor + bottom-right gravity: popup drops below the rect's
        // bottom-left corner. Anchor point = (0, 30); gravity right/bottom keeps
        // the popup's top-left there.
        let mut p = PositionerState {
            size: (100, 50),
            anchor_rect: (0, 0, 200, 30), // (x, y, w, h)
            anchor: 6,
            gravity: 8,
            ..Default::default()
        };
        assert_eq!(p.popup_origin(), (0, 30), "bottom-left/bottom-right -> below the rect");

        // top anchor + top gravity: popup sits above, centered on the rect's top
        // edge. Anchor point = (100, 0); gravity top lifts the popup by its height,
        // gravity's horizontal 'none' centers it (100 - 100/2 = 50).
        p.anchor = 1;
        p.gravity = 1;
        assert_eq!(p.popup_origin(), (50, -50), "top/top -> above the rect");

        // The offset is added on top of the resolved origin.
        p.offset = (5, 7);
        assert_eq!(p.popup_origin(), (55, -43), "offset shifts the origin");
    }

    /// `wl_shm` format codes map to the right `PixelFormat` + bytes-per-pixel:
    /// 8-bit BGRA (the default), 10-bit (4 bytes), float16 (8 bytes).
    #[test]
    fn shm_format_maps_to_pixel_format() {
        assert_eq!(
            shm_pixel_format(wl_shm::Format::Xrgb8888 as u32),
            (PixelFormat::Bgra8888, 4)
        );
        assert_eq!(
            shm_pixel_format(wl_shm::Format::Xrgb2101010 as u32),
            (PixelFormat::Rgb2101010, 4)
        );
        assert_eq!(
            shm_pixel_format(wl_shm::Format::Argb2101010 as u32),
            (PixelFormat::Rgb2101010, 4)
        );
        assert_eq!(
            shm_pixel_format(wl_shm::Format::Abgr16161616f as u32),
            (PixelFormat::Rgba16F, 8)
        );
        // An unrecognised format falls back to 8-bit BGRA (historical behaviour).
        assert_eq!(shm_pixel_format(0xDEAD_BEEF), (PixelFormat::Bgra8888, 4));
    }
}

// ---------------------------------------------------------------------------
// In-process client <-> server integration harness
//
// Drives the real protocol code paths headlessly: a `wayland_server::Display`
// with every global registered, a `UnixStream::pair()` client connection driven
// by `wayland-client`, and `mac::post` rerouted to a channel so emitted `WinCmd`s
// can be asserted on. No NSApplication / NSWindow / GPU is involved.
//
// This module deliberately does NOT `use super::*`: the client-side protocol
// types (`WlSurface`, `WlShm`, ...) collide by name with the server-side ones the
// engine uses, so it imports the engine bits it needs explicitly and the client
// bits from `wayland_client`.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod harness_tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::Receiver;
    use std::sync::{Arc, Mutex, MutexGuard};

    use wayland_server::{Display, DisplayHandle};

    use super::{register_test_globals, ClientState, State};
    use crate::input::InputEvent;
    use crate::mac::{self, WinCmd};

    use wayland_client::protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    };
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
    use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
    use wayland_protocols::wp::viewporter::client::{
        wp_viewport::WpViewport, wp_viewporter::WpViewporter,
    };
    use wayland_protocols::xdg::shell::client::{
        xdg_surface::{self, XdgSurface},
        xdg_toplevel::XdgToplevel,
        xdg_wm_base::{self, XdgWmBase},
    };
    use wayland_protocols::xdg::activation::v1::client::{
        xdg_activation_token_v1::{self, XdgActivationTokenV1},
        xdg_activation_v1::XdgActivationV1,
    };
    use wayland_protocols::wp::color_management::v1::client::{
        wp_color_management_surface_v1::WpColorManagementSurfaceV1,
        wp_color_manager_v1::{Primaries as CmPrimaries, RenderIntent, TransferFunction, WpColorManagerV1},
        wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
        wp_image_description_v1::{self, WpImageDescriptionV1},
    };

    /// Serializes harness tests: `mac::post`'s sink is process-global, and cargo
    /// runs tests in parallel, so two harnesses must not share the channel.
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    /// Distinguishes the temp files backing each shm pool.
    static FILE_SEQ: AtomicU32 = AtomicU32::new(0);

    /// A pointer event as observed by the client (order matters for regression #5).
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum PtrEv {
        Enter,
        Motion,
        Leave,
        Button,
    }

    /// Client-side dispatch state: records what the server sent us.
    #[derive(Default)]
    struct CState {
        globals: Vec<(u32, String, u32)>,
        seat_caps: u32,
        seat_name: Option<String>,
        shm_formats: Vec<u32>,
        pointer_events: Vec<PtrEv>,
        /// Token string echoed back via `xdg_activation_token_v1.done`.
        activation_token: Option<String>,
        /// `(width, height)` of every `xdg_toplevel.configure` the client received.
        toplevel_configures: Vec<(i32, i32)>,
        /// Set when a `wp_image_description_v1.ready` event arrives (the color
        /// description the client built was accepted by the compositor).
        image_desc_ready: bool,
    }

    /// The bound client proxies plus its connection/queue/state.
    struct ClientSide {
        eq: EventQueue<CState>,
        qh: QueueHandle<CState>,
        cstate: CState,
        compositor: WlCompositor,
        shm: WlShm,
        wm_base: XdgWmBase,
        seat: WlSeat,
        subcompositor: WlSubcompositor,
        viewporter: WpViewporter,
        single_pixel: WpSinglePixelBufferManagerV1,
        // Connection is kept last so it drops last; dropping it closes the socket.
        _conn: Connection,
    }

    struct Harness {
        display: Display<State>,
        dh: DisplayHandle,
        state: State,
        rx: Receiver<WinCmd>,
        client: Option<ClientSide>,
        /// Temp files backing shm pools, kept mapped for the test's lifetime.
        keep_files: Vec<std::fs::File>,
        _lock: MutexGuard<'static, ()>,
    }

    impl Harness {
        fn new() -> Self {
            let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

            let (tx, rx) = std::sync::mpsc::channel();
            mac::set_post_sink(tx);

            let mut display = Display::<State>::new().expect("create display");
            let mut dh = display.handle();
            register_test_globals(&dh);
            let mut state = State::new_headless();

            let (srv, cli) = UnixStream::pair().expect("socketpair");
            dh.insert_client(srv, Arc::new(ClientState))
                .expect("insert client");

            let conn = Connection::from_socket(cli).expect("client connection");
            let mut eq = conn.new_event_queue::<CState>();
            let qh = eq.handle();
            let registry = conn.display().get_registry(&qh, ());
            let mut cstate = CState::default();

            // Pump until the globals are advertised.
            for _ in 0..16 {
                let _ = eq.flush();
                let _ = display.dispatch_clients(&mut state);
                let _ = display.flush_clients();
                if let Some(g) = eq.prepare_read() {
                    let _ = g.read();
                }
                let _ = eq.dispatch_pending(&mut cstate);
            }

            // Bind, at their advertised versions, the globals the tests use.
            let ver = |iface: &str| -> (u32, u32) {
                cstate
                    .globals
                    .iter()
                    .find(|g| g.1.as_str() == iface)
                    .map(|g| (g.0, g.2))
                    .unwrap_or_else(|| panic!("global {iface} was not advertised"))
            };
            let (n, v) = ver(WlCompositor::interface().name);
            let compositor: WlCompositor = registry.bind(n, v, &qh, ());
            let (n, v) = ver(WlShm::interface().name);
            let shm: WlShm = registry.bind(n, v, &qh, ());
            let (n, v) = ver(XdgWmBase::interface().name);
            let wm_base: XdgWmBase = registry.bind(n, v, &qh, ());
            let (n, v) = ver(WlSeat::interface().name);
            let seat: WlSeat = registry.bind(n, v, &qh, ());
            let (n, v) = ver(WlSubcompositor::interface().name);
            let subcompositor: WlSubcompositor = registry.bind(n, v, &qh, ());
            let (n, v) = ver(WpViewporter::interface().name);
            let viewporter: WpViewporter = registry.bind(n, v, &qh, ());
            let (n, v) = ver(WpSinglePixelBufferManagerV1::interface().name);
            let single_pixel: WpSinglePixelBufferManagerV1 = registry.bind(n, v, &qh, ());

            Harness {
                display,
                dh,
                state,
                rx,
                client: Some(ClientSide {
                    eq,
                    qh,
                    cstate,
                    compositor,
                    shm,
                    wm_base,
                    seat,
                    subcompositor,
                    viewporter,
                    single_pixel,
                    _conn: conn,
                }),
                keep_files: Vec::new(),
                _lock: lock,
            }
        }

        fn client(&mut self) -> &mut ClientSide {
            self.client.as_mut().expect("client still connected")
        }

        /// Exchange messages both ways until quiescent.
        fn pump(&mut self) {
            for _ in 0..12 {
                if let Some(c) = self.client.as_mut() {
                    let _ = c.eq.flush();
                }
                let _ = self.display.dispatch_clients(&mut self.state);
                let _ = self.display.flush_clients();
                if let Some(c) = self.client.as_mut() {
                    if let Some(g) = c.eq.prepare_read() {
                        let _ = g.read();
                    }
                    let _ = c.eq.dispatch_pending(&mut c.cstate);
                }
            }
        }

        /// Drive only the server (used after the client has disconnected).
        fn drive_server(&mut self) {
            for _ in 0..12 {
                let _ = self.display.dispatch_clients(&mut self.state);
                let _ = self.display.flush_clients();
            }
        }

        /// Drop the client, closing its socket (simulates a container being killed).
        fn disconnect(&mut self) {
            self.client = None;
        }

        /// All `WinCmd`s emitted so far.
        fn cmds(&self) -> Vec<WinCmd> {
            self.rx.try_iter().collect()
        }

        /// Create a toplevel and complete the initial configure handshake.
        fn make_toplevel(&mut self) -> (WlSurface, XdgSurface, XdgToplevel) {
            let (surface, xdg, toplevel) = {
                let c = self.client();
                let surface = c.compositor.create_surface(&c.qh, ());
                let xdg = c.wm_base.get_xdg_surface(&surface, &c.qh, ());
                let toplevel = xdg.get_toplevel(&c.qh, ());
                surface.commit();
                (surface, xdg, toplevel)
            };
            self.pump();
            (surface, xdg, toplevel)
        }

        /// Make an shm buffer in a pool of `pool_size` bytes (optionally resized to
        /// `resize_to`), placing the buffer at `offset` with the given dimensions.
        fn shm_buffer(
            &mut self,
            pool_size: i32,
            resize_to: Option<i32>,
            offset: i32,
            w: i32,
            h: i32,
        ) -> WlBuffer {
            let stride = w * 4;
            let need = offset + stride * h;
            let file_size = pool_size.max(resize_to.unwrap_or(0)).max(need).max(1);

            let seq = FILE_SEQ.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("wlmac-harness-{}-{}", std::process::id(), seq));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .expect("open shm file");
            let _ = std::fs::remove_file(&path); // unlink; the fd stays valid
            file.set_len(file_size as u64).expect("set_len");

            let c = self.client.as_mut().expect("client still connected");
            let pool: WlShmPool = c.shm.create_pool(file.as_fd(), pool_size, &c.qh, ());
            if let Some(rs) = resize_to {
                pool.resize(rs);
            }
            let buffer = pool.create_buffer(offset, w, h, stride, wl_shm::Format::Xrgb8888, &c.qh, ());
            self.keep_files.push(file);
            buffer
        }

        /// Like `shm_buffer`, but fills the buffer with `bytes` first so a test can
        /// snapshot the exact pixels the compositor forwards. `bytes` is the raw
        /// buffer content (XRGB8888 little-endian == memory order B,G,R,X); its
        /// length must be `w*4*h`. The pool is mapped MAP_SHARED, so a write to the
        /// backing fd here is what `buffer_to_pixels` reads on commit.
        fn shm_buffer_filled(&mut self, w: i32, h: i32, bytes: &[u8]) -> WlBuffer {
            use std::os::unix::fs::FileExt;
            let stride = w * 4;
            assert_eq!(bytes.len(), (stride * h) as usize, "pixel data must be w*4*h");
            let file_size = (stride * h).max(1);

            let seq = FILE_SEQ.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("wlmac-harness-fill-{}-{}", std::process::id(), seq));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .expect("open shm file");
            let _ = std::fs::remove_file(&path); // unlink; the fd stays valid
            file.set_len(file_size as u64).expect("set_len");
            file.write_all_at(bytes, 0).expect("write pixel pattern");

            let c = self.client.as_mut().expect("client still connected");
            let pool: WlShmPool = c.shm.create_pool(file.as_fd(), file_size, &c.qh, ());
            let buffer =
                pool.create_buffer(0, w, h, stride, wl_shm::Format::Xrgb8888, &c.qh, ());
            self.keep_files.push(file);
            buffer
        }

        /// Make a 10-bit `argb2101010` shm buffer (4 bytes/pixel), as an HDR client
        /// would when it renders PQ content. The bytes are zeroed — the test only
        /// asserts on the format/color the compositor forwards, not the pixels.
        fn shm_buffer_10bit(&mut self, w: i32, h: i32) -> WlBuffer {
            let stride = w * 4;
            let file_size = (stride * h).max(1);
            let seq = FILE_SEQ.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("wlmac-harness-10bit-{}-{}", std::process::id(), seq));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .expect("open shm file");
            let _ = std::fs::remove_file(&path); // unlink; the fd stays valid
            file.set_len(file_size as u64).expect("set_len");

            let c = self.client.as_mut().expect("client still connected");
            let pool: WlShmPool = c.shm.create_pool(file.as_fd(), file_size, &c.qh, ());
            let buffer =
                pool.create_buffer(0, w, h, stride, wl_shm::Format::Argb2101010, &c.qh, ());
            self.keep_files.push(file);
            buffer
        }

        /// The window id of the sole toplevel (tests create exactly one).
        fn only_window_id(&self) -> u32 {
            self.state
                .toplevels
                .values()
                .next()
                .expect("a toplevel exists")
                .window_id
        }
    }

    // --- helpers ----------------------------------------------------------

    fn count<F: Fn(&WinCmd) -> bool>(cmds: &[WinCmd], f: F) -> usize {
        cmds.iter().filter(|c| f(c)).count()
    }

    // === Regression #1: shm_pool.resize ==================================

    /// A pool grown via `resize` accepts a buffer placed in the grown region;
    /// `present` then emits Create + Frame at the buffer's size.
    #[test]
    fn shm_pool_resize_accepts_buffer_in_grown_region() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();

        // Tiny 256-byte pool, grown to 8192; buffer lives at offset 4096.
        let buf = h.shm_buffer(256, Some(8192), 4096, 16, 16);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        h.pump();

        let cmds = h.cmds();
        assert_eq!(
            count(&cmds, |c| matches!(c, WinCmd::Create { .. })),
            1,
            "the grown buffer creates the window"
        );
        let frame = cmds
            .iter()
            .find_map(|c| match c {
                WinCmd::Frame { width, height, .. } => Some((*width, *height)),
                _ => None,
            })
            .expect("a Frame was emitted for the grown buffer");
        assert_eq!(frame, (16, 16), "frame carries the buffer's dimensions");
    }

    /// A buffer that reaches past the (un-resized) pool map is rejected: no Create,
    /// no Frame. This guards the `buffer_to_pixels` bounds check.
    #[test]
    fn shm_buffer_beyond_pool_size_is_rejected() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();

        // 256-byte pool, never resized; a 32x32 buffer needs 4096 bytes.
        let buf = h.shm_buffer(256, None, 0, 32, 32);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        h.pump();

        let cmds = h.cmds();
        assert_eq!(
            count(&cmds, |c| matches!(c, WinCmd::Create { .. } | WinCmd::Frame { .. })),
            0,
            "an out-of-bounds buffer produces no window and no frame"
        );
    }

    /// A buffer with a negative `offset` must be rejected, not cast to a huge
    /// `usize`. The unchecked cast (`offset as usize`) plus a wrapping `start +
    /// len` slipped past the bounds check and panicked the slice in
    /// `buffer_to_pixels`, crashing the compositor and every window. This drives
    /// that exact path end-to-end: it must not panic and must present nothing.
    #[test]
    fn shm_buffer_negative_offset_does_not_crash() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();

        // 4096-byte pool, 16x16 buffer, but a negative offset. On the old code
        // `-4 as usize` (~1.8e19) + len overflowed to a small value, passed the
        // `> map.len()` check, then `&map[start..start+len]` panicked.
        let buf = h.shm_buffer(4096, None, -4, 16, 16);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        h.pump();

        let cmds = h.cmds();
        assert_eq!(
            count(&cmds, |c| matches!(c, WinCmd::Create { .. } | WinCmd::Frame { .. })),
            0,
            "a negative-offset buffer is rejected and never presented"
        );
    }

    // === Regression #2: single-pixel buffer BGRA byte order =============

    /// `create_u32_rgba_buffer` produces a 1x1 BGRA buffer; opaque red
    /// (r=u32::MAX, g=0, b=0, a=u32::MAX) must arrive as bytes [B,G,R,A] = [0,0,255,255].
    #[test]
    fn single_pixel_buffer_emits_bgra_frame() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();

        let buf = {
            let c = h.client();
            c.single_pixel
                .create_u32_rgba_buffer(u32::MAX, 0, 0, u32::MAX, &c.qh, ())
        };
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        h.pump();

        let cmds = h.cmds();
        let (w, hgt, pixels) = cmds
            .iter()
            .find_map(|c| match c {
                WinCmd::Frame { width, height, pixels, .. } => {
                    Some((*width, *height, pixels.clone()))
                }
                _ => None,
            })
            .expect("a Frame was emitted for the single-pixel buffer");
        assert_eq!((w, hgt), (1, 1), "single-pixel buffer is 1x1");
        assert_eq!(pixels, vec![0, 0, 255, 255], "opaque red as BGRA bytes");
    }

    // === Headless render → snapshot ====================================

    /// End-to-end proof that the "client renders → compositor → pixels handed to
    /// AppKit" path is snapshot-testable with no NSWindow, display, or GPU: a
    /// client fills a 2x2 shm buffer with four known colours and commits; the
    /// bytes that reach the AppKit seam via `WinCmd::Frame` must match the client's
    /// buffer exactly. Swap the `assert_eq!` for a golden-file / `insta` snapshot
    /// (or hash a larger buffer) to grow this into a real image-diff suite.
    #[test]
    fn shm_buffer_frame_pixels_snapshot() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();

        // XRGB8888 is little-endian, so a pixel is [B, G, R, X] in memory.
        let px = |b, g, r| [b, g, r, 0u8];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&px(0, 0, 255)); // top-left: red
        bytes.extend_from_slice(&px(0, 255, 0)); // top-right: green
        bytes.extend_from_slice(&px(255, 0, 0)); // bottom-left: blue
        bytes.extend_from_slice(&px(255, 255, 255)); // bottom-right: white

        let buf = h.shm_buffer_filled(2, 2, &bytes);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        h.pump();

        let cmds = h.cmds();
        let (w, hgt, stride, pixels) = cmds
            .iter()
            .find_map(|c| match c {
                WinCmd::Frame { width, height, stride, pixels, .. } => {
                    Some((*width, *height, *stride, pixels.clone()))
                }
                _ => None,
            })
            .expect("a Frame was emitted");

        assert_eq!((w, hgt, stride), (2, 2, 8), "Frame carries the buffer geometry");
        assert_eq!(
            pixels, bytes,
            "output pixels match the client's rendered buffer byte-for-byte"
        );
    }

    // === Regression #3: wp_viewport.set_destination ====================

    /// A toplevel with a tiny buffer but a viewport destination of 1280x800 must
    /// report that destination as the window's logical size in Create and Frame.
    #[test]
    fn viewport_destination_sets_logical_size() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();

        {
            let c = h.client();
            let vp: WpViewport = c.viewporter.get_viewport(&surface, &c.qh, ());
            vp.set_destination(1280, 800);
        }
        let buf = h.shm_buffer(64, None, 0, 2, 2); // tiny 2x2 buffer
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        h.pump();

        let cmds = h.cmds();
        let create = cmds
            .iter()
            .find_map(|c| match c {
                WinCmd::Create { dst_w, dst_h, .. } => Some((*dst_w, *dst_h)),
                _ => None,
            })
            .expect("Create emitted");
        assert_eq!(create, (1280, 800), "Create carries the viewport destination");
        let frame = cmds
            .iter()
            .find_map(|c| match c {
                WinCmd::Frame { dst_w, dst_h, .. } => Some((*dst_w, *dst_h)),
                _ => None,
            })
            .expect("Frame emitted");
        assert_eq!(frame, (1280, 800), "Frame carries the viewport destination");
    }

    // === Regression #4: subsurface compositing =========================

    /// Committing a buffer on a subsurface emits a SubFrame routed to the parent
    /// window's id, at the subsurface's accumulated offset.
    #[test]
    fn subsurface_commit_emits_subframe_to_parent() {
        let mut h = Harness::new();
        let (parent, _xdg, _tl) = h.make_toplevel();
        let parent_window = h.only_window_id();

        let child = {
            let c = h.client();
            let child = c.compositor.create_surface(&c.qh, ());
            let sub: WlSubsurface = c.subcompositor.get_subsurface(&child, &parent, &c.qh, ());
            sub.set_position(10, 20);
            child
        };
        let buf = h.shm_buffer(4096, None, 0, 8, 8);
        child.attach(Some(&buf), 0, 0);
        child.commit();
        h.pump();

        let cmds = h.cmds();
        let sub = cmds
            .iter()
            .find_map(|c| match c {
                WinCmd::SubFrame { window_id, x, y, width, height, .. } => {
                    Some((*window_id, *x, *y, *width, *height))
                }
                _ => None,
            })
            .expect("a SubFrame was emitted");
        assert_eq!(sub.0, parent_window, "SubFrame routes to the parent window");
        assert_eq!((sub.1, sub.2), (10, 20), "SubFrame carries the subsurface offset");
        assert_eq!((sub.3, sub.4), (8, 8), "SubFrame carries the child's size");
    }

    /// Destroying the subsurface emits SubDestroy for the parent window.
    #[test]
    fn subsurface_destroy_emits_subdestroy() {
        let mut h = Harness::new();
        let (parent, _xdg, _tl) = h.make_toplevel();
        let parent_window = h.only_window_id();

        let (child, sub) = {
            let c = h.client();
            let child = c.compositor.create_surface(&c.qh, ());
            let sub: WlSubsurface = c.subcompositor.get_subsurface(&child, &parent, &c.qh, ());
            (child, sub)
        };
        let buf = h.shm_buffer(4096, None, 0, 8, 8);
        child.attach(Some(&buf), 0, 0);
        child.commit();
        h.pump();
        let _ = h.cmds(); // drain the SubFrame

        sub.destroy();
        h.pump();

        let cmds = h.cmds();
        let destroyed = cmds.iter().any(|c| {
            matches!(c, WinCmd::SubDestroy { window_id, .. } if *window_id == parent_window)
        });
        assert!(destroyed, "subsurface destroy emits SubDestroy for the parent");
    }

    /// A subsurface cycle (A's parent is B, B's parent is A — neither ever
    /// reaching a root window) must not hang `resolve_subsurface`. The spec
    /// forbids cycles, but a malformed client can still build one; without a
    /// visited-set guard the parent-chain walk loops forever, freezing the whole
    /// wayland thread (every client + all input). Committing into the cycle must
    /// return promptly and present nothing. If this test hangs, the guard is gone.
    #[test]
    fn subsurface_cycle_does_not_hang() {
        let mut h = Harness::new();
        // No toplevel: neither surface resolves to a window, so the walk can only
        // terminate via the cycle guard.
        let (a, _sub_ab, _sub_ba) = {
            let c = h.client();
            let a = c.compositor.create_surface(&c.qh, ());
            let b = c.compositor.create_surface(&c.qh, ());
            // A's parent is B, B's parent is A. Each has surface != parent, so the
            // creation-time self-parent guard does not fire — this exercises the
            // walk's own cycle detection.
            let sub_ab: WlSubsurface = c.subcompositor.get_subsurface(&a, &b, &c.qh, ());
            let sub_ba: WlSubsurface = c.subcompositor.get_subsurface(&b, &a, &c.qh, ());
            (a, sub_ab, sub_ba)
        };
        let buf = h.shm_buffer(4096, None, 0, 8, 8);
        a.attach(Some(&buf), 0, 0);
        a.commit();
        h.pump(); // must return; the bug would spin here forever

        let cmds = h.cmds();
        assert_eq!(
            count(&cmds, |c| matches!(c, WinCmd::SubFrame { .. })),
            0,
            "a cyclic subsurface resolves to no window and presents nothing"
        );
    }

    // === Regression #5: enter-before-motion (prevents a KWin crash) =====

    /// A `PointerMotion` for a window with no prior enter must first send a
    /// `wl_pointer.enter`: the client sees Enter then Motion, and focus is set.
    #[test]
    fn pointer_motion_without_enter_sends_enter_first() {
        let mut h = Harness::new();
        let (_surface, _xdg, _tl) = h.make_toplevel();
        {
            let c = h.client();
            let _pointer: WlPointer = c.seat.get_pointer(&c.qh, ());
        }
        h.pump(); // register the pointer server-side

        let window_id = h.only_window_id();
        assert_eq!(h.state.pointer_focus, None, "no focus before motion");

        let dh = h.dh.clone();
        h.state
            .process_input(&dh, InputEvent::PointerMotion { window_id, x: 5.0, y: 6.0 });
        h.pump();

        assert_eq!(
            h.state.pointer_focus,
            Some(window_id),
            "motion establishes pointer focus (i.e. an enter was sent)"
        );
        assert_eq!(
            h.client().cstate.pointer_events,
            vec![PtrEv::Enter, PtrEv::Motion],
            "client receives enter before motion"
        );
    }

    // === Regression #6: destroy / client-disconnect reaping ============

    /// An explicit `xdg_toplevel.destroy` emits exactly one Destroy (the request
    /// handler and the `destroyed` hook must not double-fire it).
    #[test]
    fn toplevel_destroy_emits_single_destroy() {
        let mut h = Harness::new();
        let (surface, _xdg, toplevel) = h.make_toplevel();

        // Present so the native window is actually created (Destroy is only posted
        // for created windows).
        let buf = h.shm_buffer(4096, None, 0, 8, 8);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        h.pump();
        let _ = h.cmds(); // drain Create + Frame

        toplevel.destroy();
        h.pump();

        let cmds = h.cmds();
        assert_eq!(
            count(&cmds, |c| matches!(c, WinCmd::Destroy { .. })),
            1,
            "exactly one Destroy for an explicit toplevel destroy"
        );
    }

    /// Dropping the client connection reaps its window: exactly one Destroy.
    #[test]
    fn client_disconnect_reaps_window() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();
        let buf = h.shm_buffer(4096, None, 0, 8, 8);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        h.pump();
        let _ = h.cmds(); // drain Create + Frame

        h.disconnect();
        h.drive_server();

        let cmds = h.cmds();
        assert_eq!(
            count(&cmds, |c| matches!(c, WinCmd::Destroy { .. })),
            1,
            "client disconnect reaps its window exactly once"
        );
    }

    // === Regression #8: wl_pointer.set_cursor ==========================

    /// Designating a surface as the cursor and committing its buffer emits
    /// SetCursorImage carrying the hotspot.
    #[test]
    fn set_cursor_surface_emits_set_cursor_image() {
        let mut h = Harness::new();
        let (_surface, _xdg, _tl) = h.make_toplevel();

        let (pointer, cursor_surface) = {
            let c = h.client();
            let pointer: WlPointer = c.seat.get_pointer(&c.qh, ());
            let cursor_surface = c.compositor.create_surface(&c.qh, ());
            (pointer, cursor_surface)
        };
        h.pump();

        // set_cursor first, then commit the buffer (the handle_commit path).
        pointer.set_cursor(0, Some(&cursor_surface), 4, 5);
        let buf = h.shm_buffer(1024, None, 0, 8, 8);
        cursor_surface.attach(Some(&buf), 0, 0);
        cursor_surface.commit();
        h.pump();

        let cmds = h.cmds();
        let img = cmds
            .iter()
            .find_map(|c| match c {
                WinCmd::SetCursorImage { width, height, hotspot_x, hotspot_y, .. } => {
                    Some((*width, *height, *hotspot_x, *hotspot_y))
                }
                _ => None,
            })
            .expect("SetCursorImage emitted");
        assert_eq!((img.0, img.1), (8, 8), "cursor image carries the buffer size");
        assert_eq!((img.2, img.3), (4, 5), "cursor image carries the hotspot");
    }

    /// `set_cursor` with a null surface hides the cursor.
    #[test]
    fn set_cursor_null_hides_cursor() {
        let mut h = Harness::new();
        let (_surface, _xdg, _tl) = h.make_toplevel();

        let pointer = {
            let c = h.client();
            c.seat.get_pointer(&c.qh, ())
        };
        h.pump();

        pointer.set_cursor(0, None, 0, 0);
        h.pump();

        let cmds = h.cmds();
        assert!(
            cmds.iter().any(|c| matches!(c, WinCmd::HideCursor)),
            "null cursor surface hides the cursor"
        );
    }

    // === Regression #9: seat capabilities & shm formats =================

    /// wl_seat advertises Pointer|Keyboard and the seat name.
    #[test]
    fn seat_advertises_pointer_keyboard_and_name() {
        let mut h = Harness::new();
        h.pump();

        let caps = h.client().cstate.seat_caps;
        let pointer = wl_seat::Capability::Pointer.bits();
        let keyboard = wl_seat::Capability::Keyboard.bits();
        assert!(caps & pointer != 0, "seat advertises Pointer");
        assert!(caps & keyboard != 0, "seat advertises Keyboard");
        assert_eq!(
            h.client().cstate.seat_name.as_deref(),
            Some("seat0"),
            "seat advertises its name"
        );
    }

    /// wl_shm advertises both Argb8888 and Xrgb8888.
    #[test]
    fn shm_advertises_argb_and_xrgb_formats() {
        let mut h = Harness::new();
        h.pump();

        let formats = &h.client().cstate.shm_formats;
        let argb = wl_shm::Format::Argb8888 as u32;
        let xrgb = wl_shm::Format::Xrgb8888 as u32;
        assert!(formats.contains(&argb), "shm advertises Argb8888");
        assert!(formats.contains(&xrgb), "shm advertises Xrgb8888");
    }

    // === Resize never outruns the buffer (no black regions) =============

    /// The headless equivalent of "resize a nested KWin window to double the
    /// height and expect no black areas". A resize (from a frame drag or a nested
    /// KWin session) must ask the *client* to repaint at the new size and must NOT
    /// grow the presented surface until the matching buffer arrives — if the
    /// window grew ahead of its buffer, the uncovered region would paint black.
    /// This drives the same `WinCmd`/input seam KWin would, so it catches the
    /// black-area regression deterministically without Docker, a GUI session, or
    /// a screenshot (see scripts/e2e-resize-kwin.sh for the real-KWin smoke test).
    #[test]
    fn resize_asks_client_to_repaint_before_growing_window() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();

        // Map at 400x300.
        let (w0, h0) = (400i32, 300i32);
        let buf0 = h.shm_buffer(w0 * 4 * h0, None, 0, w0, h0);
        surface.attach(Some(&buf0), 0, 0);
        surface.commit();
        h.pump();
        assert!(
            h.cmds().iter().any(|c| {
                matches!(c, WinCmd::Frame { width, height, .. } if *width == w0 && *height == h0)
            }),
            "window maps at the initial buffer size"
        );
        let window_id = h.only_window_id();

        // Double the height, exactly as mac.rs does on a frame drag / KWin resize.
        let (w1, h1) = (400i32, 600i32);
        let dh = h.dh.clone();
        h.state
            .process_input(&dh, InputEvent::Resize { window_id, width: w1, height: h1 });
        h.pump();

        // (a) The client was asked to repaint at the doubled size.
        assert!(
            h.client().cstate.toplevel_configures.contains(&(w1, h1)),
            "client received a configure at the doubled size; got {:?}",
            h.client().cstate.toplevel_configures
        );
        // (b) Nothing is presented yet: the window must not grow ahead of a real
        //     buffer. That uncovered gap is exactly what renders black.
        assert_eq!(
            count(&h.cmds(), |c| matches!(
                c,
                WinCmd::Frame { .. } | WinCmd::Create { .. }
            )),
            0,
            "no frame presented until the client repaints — the window never \
             outruns its buffer, so there is no black region"
        );

        // (c) The client repaints at the new size; the frame follows the buffer
        //     exactly (no stretch, no black margin).
        let buf1 = h.shm_buffer(w1 * 4 * h1, None, 0, w1, h1);
        surface.attach(Some(&buf1), 0, 0);
        surface.commit();
        h.pump();
        let frame = h
            .cmds()
            .into_iter()
            .find_map(|c| match c {
                WinCmd::Frame { width, height, .. } => Some((width, height)),
                _ => None,
            })
            .expect("a Frame after the client repaints");
        assert_eq!(
            frame,
            (w1, h1),
            "the presented frame follows the client's new taller buffer exactly"
        );
    }

    // --- client-side Dispatch impls --------------------------------------

    impl Dispatch<WlRegistry, ()> for CState {
        fn event(
            state: &mut Self,
            _: &WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { name, interface, version } = event {
                state.globals.push((name, interface, version));
            }
        }
    }

    impl Dispatch<WlShm, ()> for CState {
        fn event(
            state: &mut Self,
            _: &WlShm,
            event: wl_shm::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_shm::Event::Format { format } = event {
                let f = match format {
                    WEnum::Value(v) => v as u32,
                    WEnum::Unknown(u) => u,
                };
                state.shm_formats.push(f);
            }
        }
    }

    impl Dispatch<WlSeat, ()> for CState {
        fn event(
            state: &mut Self,
            _: &WlSeat,
            event: wl_seat::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                wl_seat::Event::Capabilities { capabilities } => {
                    state.seat_caps = match capabilities {
                        WEnum::Value(c) => c.bits(),
                        WEnum::Unknown(u) => u,
                    };
                }
                wl_seat::Event::Name { name } => state.seat_name = Some(name),
                _ => {}
            }
        }
    }

    impl Dispatch<WlPointer, ()> for CState {
        fn event(
            state: &mut Self,
            _: &WlPointer,
            event: wl_pointer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let ev = match event {
                wl_pointer::Event::Enter { .. } => Some(PtrEv::Enter),
                wl_pointer::Event::Motion { .. } => Some(PtrEv::Motion),
                wl_pointer::Event::Leave { .. } => Some(PtrEv::Leave),
                wl_pointer::Event::Button { .. } => Some(PtrEv::Button),
                _ => None,
            };
            if let Some(ev) = ev {
                state.pointer_events.push(ev);
            }
        }
    }

    impl Dispatch<XdgWmBase, ()> for CState {
        fn event(
            _: &mut Self,
            wm_base: &XdgWmBase,
            event: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_wm_base::Event::Ping { serial } = event {
                wm_base.pong(serial);
            }
        }
    }

    impl Dispatch<XdgSurface, ()> for CState {
        fn event(
            _: &mut Self,
            xdg_surface: &XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = event {
                xdg_surface.ack_configure(serial);
            }
        }
    }

    /// Records the size the compositor asks the toplevel to adopt, so a resize
    /// test can assert the client was told to repaint at the new dimensions.
    impl Dispatch<XdgToplevel, ()> for CState {
        fn event(
            state: &mut Self,
            _: &XdgToplevel,
            event: wayland_protocols::xdg::shell::client::xdg_toplevel::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wayland_protocols::xdg::shell::client::xdg_toplevel::Event::Configure {
                width,
                height,
                ..
            } = event
            {
                state.toplevel_configures.push((width, height));
            }
        }
    }

    // Interfaces whose events the tests don't care about.
    wayland_client::delegate_noop!(CState: ignore WlCompositor);
    wayland_client::delegate_noop!(CState: ignore WlSurface);
    wayland_client::delegate_noop!(CState: ignore WlShmPool);
    wayland_client::delegate_noop!(CState: ignore WlBuffer);
    wayland_client::delegate_noop!(CState: ignore WlSubcompositor);
    wayland_client::delegate_noop!(CState: ignore WlSubsurface);
    wayland_client::delegate_noop!(CState: ignore WpViewporter);
    wayland_client::delegate_noop!(CState: ignore WpViewport);
    wayland_client::delegate_noop!(CState: ignore WpSinglePixelBufferManagerV1);
    wayland_client::delegate_noop!(CState: ignore XdgActivationV1);
    wayland_client::delegate_noop!(CState: ignore WpColorManagerV1);
    wayland_client::delegate_noop!(CState: ignore WpImageDescriptionCreatorParamsV1);
    wayland_client::delegate_noop!(CState: ignore WpColorManagementSurfaceV1);

    /// Records that the compositor marked the client's image description ready.
    impl Dispatch<WpImageDescriptionV1, ()> for CState {
        fn event(
            state: &mut Self,
            _: &WpImageDescriptionV1,
            event: wp_image_description_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wp_image_description_v1::Event::Ready { .. } = event {
                state.image_desc_ready = true;
            }
        }
    }

    /// Records the token string the compositor echoes back on `commit`.
    impl Dispatch<XdgActivationTokenV1, ()> for CState {
        fn event(
            state: &mut Self,
            _: &XdgActivationTokenV1,
            event: xdg_activation_token_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_activation_token_v1::Event::Done { token } = event {
                state.activation_token = Some(token);
            }
        }
    }

    // === Tier 1/2 protocols =============================================

    /// All five newly added globals are advertised to clients (covers each
    /// `GlobalDispatch::bind` wiring in `register_test_globals`).
    #[test]
    fn advertises_tier1_and_tier2_globals() {
        let mut h = Harness::new();
        let advertised: Vec<String> = h
            .client()
            .cstate
            .globals
            .iter()
            .map(|g| g.1.clone())
            .collect();
        for iface in [
            "xdg_activation_v1",
            "wp_fractional_scale_manager_v1",
            "xdg_wm_dialog_v1",
            "zwp_keyboard_shortcuts_inhibit_manager_v1",
            "zwp_primary_selection_device_manager_v1",
        ] {
            assert!(
                advertised.iter().any(|g| g == iface),
                "{iface} was not advertised; got {advertised:?}"
            );
        }
    }

    /// `xdg_activation_v1.activate` on a toplevel's surface emits `WinCmd::Activate`
    /// for that window. The token is obtained via the real get_token/commit/done
    /// handshake first.
    #[test]
    fn xdg_activation_activate_emits_activate_cmd() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();
        let window_id = h.only_window_id();

        // Bind xdg_activation_v1 from a fresh registry on the live connection
        // (the harness struct only stores the globals the other tests use).
        let manager: XdgActivationV1 = {
            let c = h.client();
            let (name, ver) = c
                .cstate
                .globals
                .iter()
                .find(|g| g.1 == "xdg_activation_v1")
                .map(|g| (g.0, g.2))
                .expect("activation advertised");
            let registry = c._conn.display().get_registry(&c.qh, ());
            registry.bind(name, ver, &c.qh, ())
        };

        // Token handshake: get_token -> commit -> done(token).
        let qh = h.client().qh.clone();
        let token = manager.get_activation_token(&qh, ());
        token.commit();
        h.pump();
        let token_str = h
            .client()
            .cstate
            .activation_token
            .clone()
            .expect("compositor sent a token");

        // Activate the toplevel's surface.
        manager.activate(token_str, &surface);
        h.pump();

        let activated = h
            .cmds()
            .iter()
            .any(|c| matches!(c, WinCmd::Activate { id } if *id == window_id));
        assert!(activated, "expected WinCmd::Activate for window {window_id}");
    }

    // === HDR / color management ========================================

    /// The color-management global is advertised, and the wl_shm formats now
    /// include the HDR / wide-gamut layouts.
    #[test]
    fn advertises_color_management_and_hdr_shm_formats() {
        let mut h = Harness::new();
        h.pump();
        let advertised: Vec<String> =
            h.client().cstate.globals.iter().map(|g| g.1.clone()).collect();
        assert!(
            advertised.iter().any(|g| g == "wp_color_manager_v1"),
            "wp_color_manager_v1 not advertised; got {advertised:?}"
        );
        let formats = &h.client().cstate.shm_formats;
        for f in [
            wl_shm::Format::Xrgb2101010 as u32,
            wl_shm::Format::Argb2101010 as u32,
            wl_shm::Format::Abgr16161616f as u32,
        ] {
            assert!(formats.contains(&f), "shm format {f:#x} not advertised");
        }
    }

    /// End-to-end HDR path: build a PQ + BT.2020 parametric image description, set
    /// it on a toplevel's surface, commit a 10-bit `argb2101010` buffer, and assert
    /// the `WinCmd::Frame` handed to AppKit carries the 10-bit format and the HDR
    /// color description (so `make_cgimage` tags a PQ color space and EDR is armed).
    #[test]
    fn hdr_image_description_reaches_frame() {
        let mut h = Harness::new();
        let (surface, _xdg, _tl) = h.make_toplevel();

        // Bind the color manager from a fresh registry on the live connection.
        let manager: WpColorManagerV1 = {
            let c = h.client();
            let (name, ver) = c
                .cstate
                .globals
                .iter()
                .find(|g| g.1 == "wp_color_manager_v1")
                .map(|g| (g.0, g.2))
                .expect("color manager advertised");
            let registry = c._conn.display().get_registry(&c.qh, ());
            registry.bind(name, ver, &c.qh, ())
        };

        // Build a PQ + BT.2020 image description via the parametric creator.
        let qh = h.client().qh.clone();
        let img: WpImageDescriptionV1 = {
            let creator: WpImageDescriptionCreatorParamsV1 =
                manager.create_parametric_creator(&qh, ());
            creator.set_tf_named(TransferFunction::St2084Pq);
            creator.set_primaries_named(CmPrimaries::Bt2020);
            creator.create(&qh, ())
        };
        h.pump();
        assert!(
            h.client().cstate.image_desc_ready,
            "compositor marked the PQ/BT.2020 description ready"
        );

        // Attach it to the surface and commit a 10-bit buffer.
        let cm_surface: WpColorManagementSurfaceV1 = manager.get_surface(&surface, &qh, ());
        cm_surface.set_image_description(&img, RenderIntent::Perceptual);
        let buf = h.shm_buffer_10bit(16, 16);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        h.pump();

        let cmds = h.cmds();
        let (format, color) = cmds
            .iter()
            .find_map(|c| match c {
                WinCmd::Frame { format, color, .. } => Some((*format, *color)),
                _ => None,
            })
            .expect("a Frame was emitted for the HDR buffer");
        assert_eq!(
            format,
            crate::mac::PixelFormat::Rgb2101010,
            "the 10-bit buffer is forwarded as a 10-bit frame (not truncated)"
        );
        let color = color.expect("the frame carries a color description");
        assert_eq!(color.tf, crate::mac::TransferFn::Pq, "PQ transfer preserved");
        assert_eq!(
            color.primaries,
            crate::mac::Primaries::Bt2020,
            "BT.2020 primaries preserved"
        );
    }
}
