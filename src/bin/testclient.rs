//! Minimal native Wayland client to exercise the macOS compositor.
//!
//! Connects via `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`, creates an xdg toplevel, and
//! presents a single shm frame: a gradient with a white square in the top-left
//! corner (so orientation is obvious in the resulting NSWindow).

use std::fs::OpenOptions;
use std::os::fd::AsFd;

use memmap2::MmapMut;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WIDTH: i32 = 400;
const HEIGHT: i32 = 300;

struct State {
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    configured_drawn: bool,
    running: bool,
    // Keep the mapping and buffer alive for the lifetime of the window.
    _keep: Option<(std::fs::File, MmapMut, wl_buffer::WlBuffer)>,
}

fn main() {
    let conn = Connection::connect_to_env().expect("connect to WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry init");
    let qh = queue.handle();

    let compositor: wl_compositor::WlCompositor =
        globals.bind(&qh, 1..=4, ()).expect("bind wl_compositor");
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("bind wl_shm");
    let wm_base: xdg_wm_base::XdgWmBase =
        globals.bind(&qh, 1..=1, ()).expect("bind xdg_wm_base");

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("Rust test client".to_string());
    surface.commit();

    let mut state = State {
        shm,
        surface,
        configured_drawn: false,
        running: true,
        _keep: None,
    };

    println!("[client] connected; waiting for configure...");
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
        println!("[client] presented {WIDTH}x{HEIGHT} frame");

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
                state.draw(qh);
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
