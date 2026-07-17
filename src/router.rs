//! Compositor-side multiplexer for `--multiplex` mode.
//!
//! When enabled, the compositor process itself owns no `NSWindow`s. Instead it
//! spawns one *window-host* child per Wayland app (keyed by a stable `app_key`
//! derived from the client), and routes each `WinCmd` to the host that owns the
//! target window. Input flows back over the same socket and is pushed into the
//! compositor's `InputBus`, so the Wayland thread is none the wiser.
//!
//! Window ids are global (allocated by the Wayland `State`), so they are unique
//! across hosts; `win_to_app` maps each id to its owning host.
//!
//! Simplifications in this first cut (documented, not silent):
//!   - Cursor commands (`SetCursor`/`SetCursorImage`/`HideCursor`) and grab-clear
//!     are *broadcast* to every host rather than routed to the focused one. A
//!     host only shows its cursor while its window owns the pointer, so this is
//!     correct if wasteful; it avoids tracking pointer focus across the boundary.
//!   - `assign_window` blocks briefly waiting for the child to connect. A child
//!     that never starts times out (the window is then dropped) rather than
//!     hanging forever.
//!   - Frame pixels cross the socket as a plain byte copy. Zero-copy via
//!     `IOSurface` is a later optimization; the wire type already carries `Vec<u8>`.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::input::InputBus;
use crate::ipc::{encode_frame, read_frame, write_frame, Downlink, Uplink};
use crate::mac::WinCmd;

struct Helper {
    /// Write half of the socket to this host (compositor → host).
    tx: UnixStream,
    /// Held so the child is reaped when the helper is dropped.
    child: Child,
    /// Per-app temp dir holding the name-carrying symlink we exec'd; removed on
    /// teardown so it doesn't accumulate.
    app_dir: PathBuf,
}

struct Router {
    /// Environment forwarded to every host in its `Hello`.
    scale: i32,
    out_w: i32,
    out_h: i32,
    insets: (i32, i32, i32, i32),
    helpers: HashMap<u32, Helper>,
    /// window id → owning app_key.
    win_to_app: HashMap<u32, u32>,
    bus: Arc<InputBus>,
}

static ROUTER: OnceLock<Mutex<Router>> = OnceLock::new();

fn with_router<R>(f: impl FnOnce(&mut Router) -> R) -> Option<R> {
    ROUTER
        .get()
        .map(|m| f(&mut m.lock().unwrap_or_else(|e| e.into_inner())))
}

pub fn is_enabled() -> bool {
    ROUTER.get().is_some()
}

/// Turn on multiplex routing. Called once from `main()` on the compositor side.
pub fn enable(bus: Arc<InputBus>, scale: i32, out_w: i32, out_h: i32) {
    let _ = ROUTER.set(Mutex::new(Router {
        scale,
        out_w,
        out_h,
        insets: crate::input::reserved_insets(),
        helpers: HashMap::new(),
        win_to_app: HashMap::new(),
        bus,
    }));
}

/// Ensure a host exists for `app_key` and record that `win_id` belongs to it.
/// Called from the Wayland thread just before posting `Create`/`CreateLayer`.
pub fn assign_window(win_id: u32, app_key: u32, name: &str, regular: bool) {
    with_router(|r| {
        if !r.helpers.contains_key(&app_key) {
            match spawn_helper(app_key, name, regular, r) {
                Some(h) => {
                    r.helpers.insert(app_key, h);
                }
                None => {
                    eprintln!("[router] failed to spawn host for app {app_key}");
                    return;
                }
            }
        }
        r.win_to_app.insert(win_id, app_key);
    });
}

/// Route a `WinCmd` to the host(s) that should run it. Called by `mac::post`.
pub fn route(cmd: WinCmd) {
    with_router(|r| {
        match target(&cmd) {
            Target::Broadcast => {
                let Ok(frame) = encode_frame(&Downlink::Cmd(cmd)) else {
                    return;
                };
                for h in r.helpers.values_mut() {
                    let _ = h.tx.write_all(&frame);
                }
            }
            Target::Window(id) => {
                // A popup shares its parent's client/host; register the popup's
                // own id so its later frames route to the same host.
                if let WinCmd::CreatePopup { id: popup, parent_id, .. } = &cmd {
                    if let Some(&app) = r.win_to_app.get(parent_id) {
                        let (p, a) = (*popup, app);
                        r.win_to_app.insert(p, a);
                    }
                }
                let is_destroy = matches!(cmd, WinCmd::Destroy { .. });
                let app = r.win_to_app.get(&id).copied();
                let Ok(frame) = encode_frame(&Downlink::Cmd(cmd)) else {
                    return;
                };
                if let Some(app) = app {
                    if let Some(h) = r.helpers.get_mut(&app) {
                        let _ = h.tx.write_all(&frame);
                    }
                    // On the last window of an app, tear its host down.
                    if is_destroy {
                        r.win_to_app.remove(&id);
                        if !r.win_to_app.values().any(|&a| a == app) {
                            if let Some(mut h) = r.helpers.remove(&app) {
                                if let Ok(sd) = encode_frame(&Downlink::Shutdown) {
                                    let _ = h.tx.write_all(&sd);
                                }
                                let _ = h.child.kill();
                                let _ = h.child.wait();
                                let _ = std::fs::remove_dir_all(&h.app_dir);
                            }
                        }
                    }
                }
            }
        }
    });
}

/// Broadcast updated docked-bar insets to every host.
pub fn broadcast_insets(top: i32, right: i32, bottom: i32, left: i32) {
    with_router(|r| {
        r.insets = (top, right, bottom, left);
        let Ok(frame) = encode_frame(&Downlink::Insets(top, right, bottom, left)) else {
            return;
        };
        for h in r.helpers.values_mut() {
            let _ = h.tx.write_all(&frame);
        }
    });
}

/// Which host(s) a command targets.
enum Target {
    Window(u32),
    Broadcast,
}

fn target(cmd: &WinCmd) -> Target {
    match cmd {
        WinCmd::SetCursor { .. }
        | WinCmd::SetCursorImage { .. }
        | WinCmd::HideCursor
        | WinCmd::SetGrab { window: None } => Target::Broadcast,
        WinCmd::SetGrab { window: Some(w) } => Target::Window(*w),
        WinCmd::Create { id, .. }
        | WinCmd::Frame { id, .. }
        | WinCmd::Title { id, .. }
        | WinCmd::StartMove { id }
        | WinCmd::StartResize { id, .. }
        | WinCmd::Maximize { id, .. }
        | WinCmd::Fullscreen { id, .. }
        | WinCmd::Minimize { id }
        | WinCmd::SetMinSize { id, .. }
        | WinCmd::SetMaxSize { id, .. }
        | WinCmd::Activate { id }
        | WinCmd::SetModal { id, .. }
        | WinCmd::SetIcon { id, .. }
        | WinCmd::Destroy { id }
        | WinCmd::CreateLayer { id, .. } => Target::Window(*id),
        WinCmd::CreatePopup { parent_id, .. } => Target::Window(*parent_id),
        WinCmd::SubFrame { window_id, .. } | WinCmd::SubDestroy { window_id, .. } => {
            Target::Window(*window_id)
        }
    }
}

/// Spawn `self --window-host <sock>` and wait (briefly) for it to connect.
///
/// macOS derives a non-bundled app's Dock/Cmd-Tab name from its executable file
/// name (not argv[0]), so we exec the child through a symlink whose basename is
/// the app's name — that is what makes each app show up as e.g. "Firefox" rather
/// than "wayland-macos". (Verified against LaunchServices' `LSDisplayName`.)
fn spawn_helper(app_key: u32, name: &str, regular: bool, r: &Router) -> Option<Helper> {
    let exe = std::env::current_exe().ok()?;
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("wl-macos-host-{pid}-{app_key}.sock"));
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path).ok()?;
    listener.set_nonblocking(true).ok()?;

    // A per-app dir so the symlink's basename is exactly `name` (and two apps that
    // share a name don't collide). exec the symlink → LSDisplayName = `name`.
    let app_dir = std::env::temp_dir().join(format!("wl-macos-apps-{pid}/{app_key}"));
    let _ = std::fs::create_dir_all(&app_dir);
    let launcher = app_dir.join(name);
    let _ = std::fs::remove_file(&launcher);
    let exec_path = match std::os::unix::fs::symlink(&exe, &launcher) {
        Ok(()) => launcher.clone(),
        Err(_) => exe.clone(), // fall back to the real binary (name = "wayland-macos")
    };

    let child = Command::new(&exec_path)
        .arg("--window-host")
        .arg(&path)
        .spawn()
        .ok()?;

    // Poll-accept with a timeout so a child that never starts doesn't hang us.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut tx = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return None;
            }
        }
    };
    // The accepted socket inherits the listener's non-blocking flag on macOS;
    // put it back to blocking so large writes (frames, icons) don't fail with
    // EWOULDBLOCK and get silently dropped. Writes now back-pressure instead.
    tx.set_nonblocking(false).ok()?;
    let _ = std::fs::remove_file(&path); // socket is connected; unlink the name

    // Send the environment the host needs before any window.
    if write_frame(
        &mut tx,
        &Downlink::Hello {
            scale: r.scale,
            out_w: r.out_w,
            out_h: r.out_h,
            insets: r.insets,
            regular,
            name: name.to_string(),
        },
    )
    .is_err()
    {
        return None;
    }

    // Uplink reader: host InputEvents -> compositor InputBus.
    let mut rx = tx.try_clone().ok()?;
    let bus = r.bus.clone();
    std::thread::spawn(move || loop {
        match read_frame::<_, Uplink>(&mut rx) {
            Ok(Some(Uplink::Input(ev))) => bus.push(ev),
            Ok(Some(Uplink::Ready)) => {}
            Ok(None) | Err(_) => break,
        }
    });

    eprintln!("[router] spawned host app={app_key} name={name:?} regular={regular}");
    Some(Helper { tx, child, app_dir })
}
