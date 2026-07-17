//! Minimal native Wayland client to exercise the macOS compositor.
//!
//! Connects via `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`, creates an xdg toplevel, and
//! presents a single shm frame: a gradient with a white square in the top-left
//! corner (so orientation is obvious in the resulting NSWindow).
//!
//! Set `WLMAC_HDR=1` to instead present a 10-bit BT.2100 PQ frame with a
//! color-management image description: a left→right PQ luminance ramp (peaking at
//! ~10000 nits on the right) with an SDR-white reference block in the top-left.
//! On an EDR-capable display the right side should glow far brighter than the
//! reference block; on an SDR display it tone-maps without error.

use std::fs::OpenOptions;
use std::os::fd::AsFd;

use log::info;
use memmap2::MmapMut;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{Primaries, RenderIntent, TransferFunction, WpColorManagerV1},
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_v1::WpImageDescriptionV1,
};
use wayland_protocols::xdg::toplevel_icon::v1::client::{
    xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1,
};

const WIDTH: i32 = 400;
const HEIGHT: i32 = 300;

struct State {
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    configured_drawn: bool,
    running: bool,
    /// HDR mode (`WLMAC_HDR=1`): present a 10-bit PQ frame + image description.
    hdr: bool,
    /// The surface's color-management object (HDR mode only).
    cm_surface: Option<WpColorManagementSurfaceV1>,
    /// The PQ/BT.2020 image description bound to the surface (HDR mode only).
    image_desc: Option<WpImageDescriptionV1>,
    // Keep the mapping and buffer alive for the lifetime of the window.
    _keep: Option<(std::fs::File, MmapMut, wl_buffer::WlBuffer)>,
    // Keep the icon manager, icon object, and its backing buffer alive.
    _icon_keep: Option<(
        xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1,
        xdg_toplevel_icon_v1::XdgToplevelIconV1,
        std::fs::File,
        MmapMut,
        wl_buffer::WlBuffer,
    )>,
}

/// Create an `w`x`h` wl_shm buffer filled with a solid BGRA color.
fn make_solid_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    w: i32,
    h: i32,
    bgra: [u8; 4],
) -> (std::fs::File, MmapMut, wl_buffer::WlBuffer) {
    let stride = w * 4;
    let size = stride * h;
    let path = format!(
        "{}/wlmac-icon-{}",
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()),
        std::process::id()
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("open icon shm file");
    let _ = std::fs::remove_file(&path);
    file.set_len(size as u64).expect("set_len");
    let mut mmap = unsafe { MmapMut::map_mut(&file).expect("mmap") };
    for px in mmap.chunks_exact_mut(4) {
        px.copy_from_slice(&bgra);
    }
    let pool = shm.create_pool(file.as_fd(), size, qh, ());
    let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Argb8888, qh, ());
    (file, mmap, buffer)
}

fn main() {
    use std::io::Write;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            let ts = buf.timestamp();
            let thread = std::thread::current().name().unwrap_or("?").to_owned();
            writeln!(
                buf,
                "[{ts} {:5} {}/{thread}] {}",
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();

    let conn = Connection::connect_to_env().expect("connect to WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry init");
    let qh = queue.handle();

    let compositor: wl_compositor::WlCompositor =
        globals.bind(&qh, 1..=4, ()).expect("bind wl_compositor");
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
    let wm_base: xdg_wm_base::XdgWmBase =
        globals.bind(&qh, 1..=1, ()).expect("bind xdg_wm_base");

    let hdr = std::env::var("WLMAC_HDR").map(|v| v != "0").unwrap_or(false);

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title(if hdr { "Rust test client (HDR/PQ)" } else { "Rust test client" }.to_string());
    // app_id drives the --multiplex Dock/Cmd-Tab name → "TestApp".
    toplevel.set_app_id("org.example.TestApp".to_string());

    // Set a real toplevel icon (a 64x64 solid tile) via xdg_toplevel_icon, to
    // exercise the compositor's real-artwork path (→ WinCmd::SetIcon).
    let icon_keep = globals
        .bind::<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1, _, _>(&qh, 1..=1, ())
        .ok()
        .map(|mgr| {
            let (file, mmap, buf) = make_solid_buffer(&shm, &qh, 64, 64, [0, 0, 255, 255]);
            let icon = mgr.create_icon(&qh, ());
            icon.add_buffer(&buf, 1);
            mgr.set_icon(&toplevel, Some(&icon));
            (mgr, icon, file, mmap, buf)
        });

    surface.commit();

    // In HDR mode, build a BT.2100 PQ image description and a color-managed
    // surface up front; `draw_hdr` binds the description on commit.
    let (cm_surface, image_desc) = if hdr {
        let manager: WpColorManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .expect("bind wp_color_manager_v1 (compositor must offer HDR)");
        let creator: WpImageDescriptionCreatorParamsV1 = manager.create_parametric_creator(&qh, ());
        creator.set_tf_named(TransferFunction::St2084Pq);
        creator.set_primaries_named(Primaries::Bt2020);
        // Signal HDR intent: ~10000 nits peak, 203 nits reference (BT.2408) white.
        creator.set_luminances(0, 10000, 203);
        let img = creator.create(&qh, ());
        let cm = manager.get_surface(&surface, &qh, ());
        (Some(cm), Some(img))
    } else {
        (None, None)
    };

    let mut state = State {
        shm,
        surface,
        configured_drawn: false,
        running: true,
        hdr,
        cm_surface,
        image_desc,
        _keep: None,
        _icon_keep: icon_keep,
    };

    info!(target: "client", "connected ({}); waiting for configure...", if hdr { "HDR/PQ" } else { "SDR" });
    while state.running {
        queue.blocking_dispatch(&mut state).expect("dispatch");
    }
}

impl State {
    fn draw(&mut self, qh: &QueueHandle<State>) {
        let stride = WIDTH * 4;
        let size = stride * HEIGHT;

        let path = format!(
            "{}/wlmac-client-{}",
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()),
            std::process::id()
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open shm file");
        let _ = std::fs::remove_file(&path); // unlink; fd stays valid
        file.set_len(size as u64).expect("set_len");
        let mut mmap = unsafe { MmapMut::map_mut(&file).expect("mmap") };

        // Gradient: B increases right, G increases down, R constant. Bytes B,G,R,X.
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let i = (y * stride + x * 4) as usize;
                mmap[i] = (x * 255 / WIDTH) as u8;
                mmap[i + 1] = (y * 255 / HEIGHT) as u8;
                mmap[i + 2] = 128;
                mmap[i + 3] = 255;
            }
        }
        // White 24x24 marker in the top-left corner.
        for y in 0..24 {
            for x in 0..24 {
                let i = (y * stride + x * 4) as usize;
                mmap[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }

        let pool = self
            .shm
            .create_pool(file.as_fd(), size, qh, ());
        let buffer = pool.create_buffer(
            0,
            WIDTH,
            HEIGHT,
            stride,
            wl_shm::Format::Xrgb8888,
            qh,
            (),
        );

        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage(0, 0, WIDTH, HEIGHT);
        self.surface.commit();
        info!(target: "client", "presented {WIDTH}x{HEIGHT} frame");

        self._keep = Some((file, mmap, buffer));
    }

    /// Present a 10-bit `argb2101010` BT.2100 PQ frame: a left→right PQ luminance
    /// ramp (0 → code 1023 ≈ 10000 nits) with an SDR-white reference block. Packs
    /// each pixel as little-endian `A:R:G:B` 2:10:10:10.
    fn draw_hdr(&mut self, qh: &QueueHandle<State>) {
        let stride = WIDTH * 4;
        let size = stride * HEIGHT;

        let path = format!(
            "{}/wlmac-client-hdr-{}",
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()),
            std::process::id()
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open shm file");
        let _ = std::fs::remove_file(&path); // unlink; fd stays valid
        file.set_len(size as u64).expect("set_len");
        let mut mmap = unsafe { MmapMut::map_mut(&file).expect("mmap") };

        // Pack a 10-bit grayscale value (a=opaque). argb2101010 little-endian:
        // bits 31-30 A, 29-20 R, 19-10 G, 9-0 B.
        let pack = |code: u32| -> [u8; 4] {
            let a = 3u32; // 2-bit alpha, fully opaque
            ((a << 30) | (code << 20) | (code << 10) | code).to_le_bytes()
        };
        // Horizontal PQ ramp: left black, right = peak (10000 nits).
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let i = (y * stride + x * 4) as usize;
                let code = (x as u32 * 1023 / (WIDTH as u32 - 1)).min(1023);
                mmap[i..i + 4].copy_from_slice(&pack(code));
            }
        }
        // SDR-white reference (PQ code ~520 ≈ 100 nits) in the top-left, so on an
        // EDR display the ramp's right side is visibly brighter than "white".
        let white = pack(520);
        for y in 0..48 {
            for x in 0..48 {
                let i = (y * stride + x * 4) as usize;
                mmap[i..i + 4].copy_from_slice(&white);
            }
        }

        let pool = self.shm.create_pool(file.as_fd(), size, qh, ());
        let buffer = pool.create_buffer(0, WIDTH, HEIGHT, stride, wl_shm::Format::Argb2101010, qh, ());

        // Tag the surface with the PQ image description before committing.
        if let (Some(cm), Some(img)) = (&self.cm_surface, &self.image_desc) {
            cm.set_image_description(img, RenderIntent::Perceptual);
        }
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage(0, 0, WIDTH, HEIGHT);
        self.surface.commit();
        info!(target: "client", "presented {WIDTH}x{HEIGHT} HDR/PQ frame (10-bit BT.2100)");

        self._keep = Some((file, mmap, buffer));
    }
}

// --- Event plumbing --------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
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

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            if !state.configured_drawn {
                state.configured_drawn = true;
                if state.hdr {
                    state.draw_hdr(qh);
                } else {
                    state.draw(qh);
                }
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.running = false;
        }
    }
}

// No-op event handlers for the remaining objects.
macro_rules! noop_dispatch {
    ($iface:ty) => {
        impl Dispatch<$iface, ()> for State {
            fn event(
                _: &mut Self,
                _: &$iface,
                _: <$iface as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

noop_dispatch!(wl_compositor::WlCompositor);
noop_dispatch!(wl_shm::WlShm);
noop_dispatch!(wl_shm_pool::WlShmPool);
noop_dispatch!(wl_surface::WlSurface);
noop_dispatch!(wl_buffer::WlBuffer);
noop_dispatch!(WpColorManagerV1);
noop_dispatch!(WpImageDescriptionCreatorParamsV1);
noop_dispatch!(WpImageDescriptionV1);
noop_dispatch!(WpColorManagementSurfaceV1);
noop_dispatch!(xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1);
noop_dispatch!(xdg_toplevel_icon_v1::XdgToplevelIconV1);
