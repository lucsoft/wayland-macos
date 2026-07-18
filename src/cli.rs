//! `wayland-macos` — the end-user orchestrator CLI.
//!
//! This is the *default* binary (`cargo run` with no `--bin`). It does what the
//! old `mac-side.sh` shell script used to do by hand, so a new user can just run
//! `cargo run`. The steps depend on the back-end:
//!
//!   audio (both)   — the PulseAudio CoreAudio bridge on :4713 (scripts/pulseaudio-mac.sh)
//!   core  (both)   — build + start the compositor (the `wayland-macos-core` binary)
//!
//!   waypipe back-end (default) additionally:
//!     discover     — read the Wayland socket the compositor bound
//!     waypipe client — connect it to the compositor, listen on a unix socket
//!     socat bridge — a TCP<->unix bridge so the container can reach waypipe
//!
//!   RAIL back-end (a `--features rail` build): none of the above — the
//!     compositor is an RDP client that dials into the container's Weston RDP
//!     server, so there's no local Wayland socket / waypipe / socat.
//!
//! All long-lived children are detached (their own session via `setsid`, stdio
//! to log files under /tmp) so they survive this CLI exiting — exactly like the
//! shell's `nohup ... &`. PIDs are recorded in /tmp/wlmac-<port>-*.pid;
//! `wayland-macos stop` tears them down.
//!
//! Every instance is keyed on its bridge **port** (see `Instance`), so several
//! stacks can run concurrently on distinct ports — `--port 7778` alongside the
//! default `7777`, each with its own compositor, waypipe client, TCP bridge,
//! logs and pidfiles. `stop --port <n>` tears down one; `stop --all` tears down
//! every instance this CLI started.
//!
//! The compositor ("core") is a *separate* binary so this orchestrator shares no
//! code with it and can't drag AppKit/Wayland deps into a plain process launcher.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use log::{error, info, warn};

/// One running stack (compositor + waypipe client + TCP bridge), namespaced by
/// its bridge **port** so several can run side by side without stomping each
/// other's sockets, logs or pidfiles.
///
/// The port is the natural identity: the container dials
/// `host.docker.internal:<port>`, so the port *is* the connection string, and
/// two instances can't share a listen port anyway. Everything an `up` creates —
/// the waypipe client socket, the three log files, the three pidfiles — is
/// derived from it here, so `stop --port <n>` tears down exactly one instance
/// and leaves the others running.
struct Instance {
    port: String,
}

impl Instance {
    fn new(port: String) -> Self {
        Instance { port }
    }
    /// The waypipe client's unix socket (also the socat bridge's target).
    fn client_sock(&self) -> String {
        format!("/tmp/wlmac-{}-waypipe-client.sock", self.port)
    }
    fn comp_log(&self) -> String {
        format!("/tmp/wlmac-{}-compositor.log", self.port)
    }
    fn waypipe_log(&self) -> String {
        format!("/tmp/wlmac-{}-waypipe.log", self.port)
    }
    fn socat_log(&self) -> String {
        format!("/tmp/wlmac-{}-socat.log", self.port)
    }
    /// Pidfile for one of this instance's children (`compositor`/`waypipe`/`socat`).
    fn pidfile(&self, name: &str) -> String {
        format!("/tmp/wlmac-{}-{name}.pid", self.port)
    }
}

fn main() {
    // Log via the `log` facade to stderr, matching the compositor's format
    // (timestamp, level, target/thread). Targets use `cli`; override with
    // e.g. `RUST_LOG=cli=debug`. `--help` writes plain stdout, not a log line.
    init_logger();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("stop") => stop(&args[1..]),
        Some("up") => up(&args[1..]),
        Some("-h") | Some("--help") | Some("help") => print_help(),
        // No subcommand (or a leading flag like `--multiplex`): default to `up`
        // and treat everything as pass-through flags.
        _ => up(&args),
    }
}

/// stderr logger matching `wayland-macos-core`'s format (timestamp, level,
/// target/thread). Level defaults to `info`; filter with `RUST_LOG`.
fn init_logger() {
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
}

fn print_help() {
    println!(
        "wayland-macos — Wayland-on-macOS orchestrator\n\n\
         USAGE:\n\
         \x20 wayland-macos [up] [-d] [--multiplex] [--port <n>]\n\
         \x20 wayland-macos stop [--port <n> | --all]\n\n\
         COMMANDS:\n\
         \x20 up     (default) start pulseaudio + the compositor (+ waypipe & TCP bridge for the waypipe back-end)\n\
         \x20 stop   tear down one instance (--port, default 7777) or every instance (--all)\n\n\
         FLAGS:\n\
         \x20 -d, --detach                       start everything, then exit (stop it later with `wayland-macos stop`)\n\
         \x20 --multiplex                        surface each app as its own native macOS app (forwarded to the compositor)\n\
         \x20 -p, --port <n>                     TCP bridge port; also the instance id (default 7777)\n\
         \x20 --all                              (stop only) tear down every running instance\n\n\
         By default `up` stays in the foreground; press Ctrl-C to tear everything down.\n\n\
         MULTIPLE INSTANCES: each instance is keyed on its `--port`, so a second\n\
         `wayland-macos up --port 7778` runs a fully isolated stack alongside the\n\
         default one. Point a container at it with `BRIDGE_PORT=7778 docker compose\n\
         run ...` (it connects to host.docker.internal:7778).\n\n\
         Back-end is a build-time choice: a plain build is the waypipe back-end; a\n\
         `--features rail` build is the RAIL back-end (no waypipe/bridge).\n\n\
         Env: WLMAC_MULTIPLEX=1 implies --multiplex; BRIDGE_PORT sets the bridge\n\
         port when --port is absent (default 7777); WAYPIPE overrides the waypipe\n\
         client path; WAYPIPE_COMPRESS sets wire compression (lz4|zstd|none, default lz4)."
    );
}

// ---------------------------------------------------------------------------
// up
// ---------------------------------------------------------------------------

fn up(flags: &[String]) {
    // `-d`/`--detach`: start everything and exit (the old behavior — children are
    // detached daemons). Without it we stay in the foreground and tear everything
    // down on Ctrl-C.
    let detach = flags.iter().any(|f| f == "-d" || f == "--detach");
    let multiplex = flags.iter().any(|f| f == "--multiplex")
        || std::env::var("WLMAC_MULTIPLEX").as_deref() == Ok("1");
    // The bridge port keys the whole instance (sockets, logs, pidfiles). All
    // isolation between concurrent stacks flows from this one value.
    let inst = Instance::new(parse_port(flags));
    // The back-end is a build-time choice: a `--features rail` build is the
    // RAIL back-end, a plain build is the waypipe back-end.
    let rail = cfg!(feature = "rail");
    let root = project_root();

    // audio bridge — shared by both back-ends (optional; degrades cleanly). One
    // PulseAudio daemon on :4713 serves every instance, so it's started once and
    // only torn down when the last instance stops.
    start_pulseaudio(root.as_deref());

    // compositor (built with the same feature set as this CLI).
    let core = ensure_core(root.as_deref());
    start_compositor(&core, multiplex, &inst);

    if rail {
        // RAIL: the compositor is an RDP *client* dialing into the container's
        // Weston RDP server — there's no local Wayland socket, waypipe, or socat
        // bridge (that's the waypipe pipeline). Start the container separately.
        info!(target: "cli", "RAIL back-end started; now start the container: docker compose --profile rail up wayland-rail");
    } else {
        // waypipe pipeline: connect a waypipe client to the compositor's socket and
        // expose it to the container over a TCP<->unix bridge.
        let (runtime, display) = discover_socket(&inst);
        let waypipe = ensure_waypipe(root.as_deref());
        start_waypipe_client(&waypipe, &runtime, &display, &inst);
        start_socat(&inst);
        info!(target: "cli", "ready. Container should connect to host.docker.internal:{}", inst.port);
    }

    if detach {
        info!(target: "cli", "detached; children keep running. Tear down with `wayland-macos stop --port {}`.", inst.port);
    } else {
        run_foreground(inst);
    }
}

/// The bridge port for this invocation: `--port <n>` / `-p <n>` / `--port=<n>`,
/// else the `BRIDGE_PORT` env, else `7777`. This value is the instance identity,
/// so `up`/`stop` resolve it the same way.
fn parse_port(flags: &[String]) -> String {
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        if let Some(v) = f.strip_prefix("--port=") {
            return v.to_string();
        }
        if f == "--port" || f == "-p" {
            if let Some(v) = it.next() {
                return v.clone();
            }
        }
    }
    std::env::var("BRIDGE_PORT").unwrap_or_else(|_| "7777".to_string())
}

/// Block until SIGINT/SIGTERM, then tear everything back down.
///
/// The children are detached daemons (`spawn_detached`), so this CLI doesn't own
/// them as OS children — it just parks until interrupted and then runs the same
/// `stop()` the `stop` subcommand does. Keeps the common `cargo run` case a
/// single foreground process you can Ctrl-C, without changing how children are
/// launched (so `-d` + `stop` still works identically).
fn run_foreground(inst: Instance) {
    static STOP: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_signal(_: libc::c_int) {
        // Only async-signal-safe work here: flip a flag the main loop polls.
        STOP.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    info!(target: "cli", "running; press Ctrl-C to stop");
    while !STOP.load(Ordering::SeqCst) {
        sleep(Duration::from_millis(200));
    }
    info!(target: "cli", "interrupted; shutting down");
    stop_instance(&inst);
}

/// Start the shared PulseAudio CoreAudio bridge via `scripts/pulseaudio-mac.sh`.
///
/// Delegating to the script keeps one source of truth for the (fiddly) launch —
/// it probes TCP 4713 and no-ops if the bridge is already serving, prints a hint
/// if pulseaudio isn't installed, and writes the pidfile. See that script.
fn start_pulseaudio(root: Option<&Path>) {
    let Some(script) = root.map(|r| r.join("scripts/pulseaudio-mac.sh")) else {
        warn!(target: "cli", "no source tree; run scripts/pulseaudio-mac.sh yourself for audio");
        return;
    };
    // Inherit stdio so its `[mac]` lines show through; it returns promptly.
    let _ = Command::new(&script).status();
}

/// Locate the waypipe client, building it from pinned upstream if it's missing.
fn ensure_waypipe(root: Option<&Path>) -> PathBuf {
    let waypipe = std::env::var_os("WAYPIPE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            root.map(|r| r.join("bin/waypipe-macos"))
                .unwrap_or_else(|| PathBuf::from("waypipe-macos"))
        });
    if is_executable(&waypipe) {
        return waypipe;
    }
    if let Some(root) = root {
        info!(target: "cli", "waypipe client missing; building it");
        let status = Command::new(root.join("scripts/build-waypipe.sh"))
            .current_dir(root)
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            error!(target: "cli", "build-waypipe.sh failed");
            std::process::exit(1);
        }
    }
    waypipe
}

/// Ensure the compositor binary is up to date and return its path.
///
/// In the source tree we always `cargo build` it — incremental, so it's a fast
/// no-op when unchanged, but it means editing the compositor and re-running the
/// CLI actually picks up the new code (just as the old mac-side script did). Only
/// when run outside the tree (a plain installed binary) do we fall back to a
/// sibling of this CLI or a PATH lookup.
fn ensure_core(root: Option<&Path>) -> PathBuf {
    let (profile, sibling) = exe_profile_and_sibling("wayland-macos-core");

    if let Some(root) = root {
        info!(target: "cli", "building compositor (wayland-macos-core)...");
        let mut args = vec!["build", "--quiet", "--bin", "wayland-macos-core"];
        if profile == "release" {
            args.push("--release");
        }
        // Build the compositor with the same back-end feature as this CLI so
        // `cargo run --features rail` yields a RAIL core.
        if cfg!(feature = "rail") {
            args.push("--features");
            args.push("rail");
        }
        let status = Command::new("cargo").args(&args).current_dir(root).status();
        if !matches!(status, Ok(s) if s.success()) {
            error!(target: "cli", "`cargo build` of wayland-macos-core failed");
            std::process::exit(1);
        }
        return root.join(format!("target/{profile}/wayland-macos-core"));
    }

    // Installed (no source tree): best-effort sibling, else PATH.
    sibling
        .filter(|s| s.exists())
        .or_else(|| which("wayland-macos-core"))
        .unwrap_or_else(|| {
            error!(target: "cli", "can't find the wayland-macos-core binary");
            std::process::exit(1);
        })
}

/// Start the compositor if this instance isn't already running.
fn start_compositor(core: &Path, multiplex: bool, inst: &Instance) {
    if compositor_running(inst) {
        info!(target: "cli", "compositor for port {} already running", inst.port);
        return;
    }
    let mut args: Vec<&str> = Vec::new();
    if multiplex {
        info!(target: "cli", "multiplex = on (per-app window hosts)");
        args.push("--multiplex");
    }
    let comp_log = inst.comp_log();
    info!(target: "cli", "starting compositor {}", args.join(" "));
    // Fresh log so socket discovery reads THIS run's announcement.
    let _ = fs::write(&comp_log, b"");
    match spawn_detached(&core.to_string_lossy(), &args, &comp_log, &[]) {
        Ok(pid) => write_pid(inst, "compositor", pid),
        Err(e) => {
            error!(target: "cli", "failed to start compositor ({e})");
            std::process::exit(1);
        }
    }
}

/// Read the Wayland socket the compositor bound (`XDG_RUNTIME_DIR`,
/// `WAYLAND_DISPLAY`). Waypipe pipeline only — the compositor prints
/// `export XDG_RUNTIME_DIR=…` / `export WAYLAND_DISPLAY=…` once its socket is
/// bound (see `wayland::run`); poll the log for both.
fn discover_socket(inst: &Instance) -> (String, String) {
    let comp_log = inst.comp_log();
    let mut runtime = String::new();
    let mut display = String::new();
    let found = wait_until(Duration::from_secs(5), || {
        let log = fs::read_to_string(&comp_log).unwrap_or_default();
        for line in log.lines() {
            if let Some(v) = line.split_once("export XDG_RUNTIME_DIR=") {
                runtime = v.1.trim().to_string();
            }
            if let Some(v) = line.split_once("export WAYLAND_DISPLAY=") {
                display = v.1.trim().to_string();
            }
        }
        !runtime.is_empty() && !display.is_empty()
    });
    if !found {
        error!(target: "cli", "compositor didn't announce its socket (see {comp_log})");
        std::process::exit(1);
    }
    info!(target: "cli", "compositor display: {runtime}/{display}");
    (runtime, display)
}

fn start_waypipe_client(waypipe: &Path, runtime: &str, display: &str, inst: &Instance) {
    let client_sock = inst.client_sock();
    // Replace any stale client from a previous run of THIS instance. The socket
    // path is unique per instance, so matching on it leaves sibling instances'
    // clients untouched (a broad `waypipe-macos.*client` match would kill them).
    let _ = Command::new("pkill").args(["-f", &client_sock]).status();
    let _ = fs::remove_file(&client_sock);
    // -c lz4: compress the wire (waypipe's default low-latency codec). Must match
    // the container server's `-c` (docker/entrypoint.sh) — waypipe rejects the
    // connection on a compression-type mismatch, and a binary built without the
    // codec silently downgrades to none. WAYPIPE_COMPRESS overrides it (e.g. zstd
    // for a better ratio, none to disable); keep both ends in sync. Both binaries
    // are built with lz4+zstd support (scripts/build-waypipe.sh).
    let compress = std::env::var("WAYPIPE_COMPRESS").unwrap_or_else(|_| "lz4".to_string());
    info!(target: "cli", "starting waypipe client on {client_sock} (compression: {compress})");
    let child = spawn_detached(
        &waypipe.to_string_lossy(),
        &["-c", &compress, "--no-gpu", "-s", &client_sock, "client"],
        &inst.waypipe_log(),
        &[
            ("XDG_RUNTIME_DIR", runtime),
            ("WAYLAND_DISPLAY", display),
        ],
    );
    match child {
        Ok(pid) => write_pid(inst, "waypipe", pid),
        Err(e) => {
            error!(target: "cli", "failed to start waypipe client ({e})");
            std::process::exit(1);
        }
    }
    wait_until(Duration::from_secs(5), || Path::new(&client_sock).exists());
}

fn start_socat(inst: &Instance) {
    let port = &inst.port;
    let client_sock = inst.client_sock();
    let _ = Command::new("pkill")
        .args(["-f", &format!("socat TCP-LISTEN:{port}")])
        .status();
    info!(target: "cli", "starting TCP bridge on :{port} -> {client_sock}");
    // nodelay: disable Nagle on the TCP side (see docker/entrypoint.sh) so small
    // Wayland replies aren't stalled ~hundreds of ms.
    let child = spawn_detached(
        "socat",
        &[
            &format!("TCP-LISTEN:{port},reuseaddr,fork,nodelay"),
            &format!("UNIX-CONNECT:{client_sock}"),
        ],
        &inst.socat_log(),
        &[],
    );
    match child {
        Ok(pid) => write_pid(inst, "socat", pid),
        Err(e) => error!(target: "cli", "failed to start socat bridge ({e})"),
    }
}

/// True if the compositor for this instance is still alive.
///
/// We check the pidfile rather than scanning process names: a substring match on
/// "wayland-macos-core" false-positives on any unrelated process carrying that
/// string in its argv (an editor, a `tail` on the log, or the launching shell),
/// which would wrongly skip startup and then fail on socket discovery. It would
/// also confuse *sibling* instances, which share the process name — the
/// per-instance pidfile keeps them independent. The trade-off is that a
/// compositor started *outside* this CLI (e.g. `cargo run --bin
/// wayland-macos-core` by hand) isn't detected — `up` would then start a second
/// one on its own socket, which is harmless.
fn compositor_running(inst: &Instance) -> bool {
    let Ok(contents) = fs::read_to_string(inst.pidfile("compositor")) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<i32>() else {
        return false;
    };
    // Signal 0 probes for the process's existence without delivering anything.
    unsafe { libc::kill(pid, 0) == 0 }
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

/// `stop` dispatch: `--all` tears down every discovered instance, otherwise the
/// one named by `--port` (default 7777).
fn stop(flags: &[String]) {
    if flags.iter().any(|f| f == "--all") {
        let ports = discovered_ports();
        if ports.is_empty() {
            info!(target: "cli", "no running instances");
        }
        for port in ports {
            stop_instance(&Instance::new(port));
        }
        stop_shared_audio();
        return;
    }
    let inst = Instance::new(parse_port(flags));
    stop_instance(&inst);
    // A single-instance stop only reaps the shared audio bridge once nothing
    // else is using it (see stop_instance).
}

/// Tear down one instance's children, then the shared audio bridge if this was
/// the last instance standing.
fn stop_instance(inst: &Instance) {
    for name in ["socat", "waypipe", "compositor"] {
        let pidfile = inst.pidfile(name);
        if let Ok(pid) = fs::read_to_string(&pidfile) {
            let pid = pid.trim();
            if Command::new("kill")
                .arg(pid)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                info!(target: "cli", "killed {name} (port {})", inst.port);
            }
        }
        let _ = fs::remove_file(&pidfile);
    }
    // Belt-and-braces, scoped to THIS instance so siblings survive: the waypipe
    // client and socat bridge are matched by this instance's unique socket/port
    // (killing the compositor already reaps its window-host children, which are
    // keyed by the compositor pid — see router::spawn_helper).
    let _ = Command::new("pkill")
        .args(["-f", &inst.client_sock()])
        .status();
    let _ = Command::new("pkill")
        .args(["-f", &format!("socat TCP-LISTEN:{}", inst.port)])
        .status();
    let _ = fs::remove_file(inst.client_sock());

    // The PulseAudio bridge is shared across instances, so only reap it once the
    // last instance is gone — otherwise stopping one instance would mute the
    // others.
    if discovered_ports().is_empty() {
        stop_shared_audio();
    }
    info!(target: "cli", "done (port {})", inst.port);
}

/// Kill the shared PulseAudio CoreAudio bridge (`--check` exits 0 if a daemon is
/// running). Shared by every instance, so callers reap it only when no instance
/// remains.
fn stop_shared_audio() {
    if which("pulseaudio").is_some()
        && Command::new("pulseaudio")
            .arg("--check")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        && Command::new("pulseaudio")
            .arg("--kill")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    {
        info!(target: "cli", "killed pulseaudio");
    }
    let _ = fs::remove_file("/tmp/wlmac-pulseaudio.pid");

    // The audio-follow agent (tracks the macOS default output; started by
    // scripts/pulseaudio-mac.sh) is tied to the bridge — reap it alongside.
    if let Ok(pid) = fs::read_to_string("/tmp/wlmac-audio-follow.pid") {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                info!(target: "cli", "killed audio-follow agent");
            }
        }
    }
    let _ = fs::remove_file("/tmp/wlmac-audio-follow.pid");
}

/// The ports of every instance with a live compositor, discovered from the
/// `/tmp/wlmac-<port>-compositor.pid` files this CLI writes. A stale pidfile
/// (process gone) is cleaned up and skipped, so this doubles as the "is anyone
/// still running?" check.
fn discovered_ports() -> Vec<String> {
    let mut ports = Vec::new();
    let Ok(entries) = fs::read_dir("/tmp") else {
        return ports;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(port) = name
            .strip_prefix("wlmac-")
            .and_then(|r| r.strip_suffix("-compositor.pid"))
        else {
            continue;
        };
        let alive = fs::read_to_string(entry.path())
            .ok()
            .and_then(|c| c.trim().parse::<i32>().ok())
            .map(|pid| unsafe { libc::kill(pid, 0) == 0 })
            .unwrap_or(false);
        if alive {
            ports.push(port.to_string());
        } else {
            let _ = fs::remove_file(entry.path());
        }
    }
    ports
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The source-tree root (contains Cargo.toml + scripts/), derived from this
/// binary's location: `<root>/target/<profile>/wayland-macos`. `None` when run
/// from outside the tree (e.g. `cargo install`ed).
fn project_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|p| p.join("Cargo.toml").is_file() && p.join("scripts").is_dir())
        .map(Path::to_path_buf)
}

/// The build profile this CLI was compiled with ("debug"/"release", inferred
/// from its own path), and the same-directory path of a sibling binary.
fn exe_profile_and_sibling(sibling: &str) -> (String, Option<PathBuf>) {
    let exe = std::env::current_exe().ok();
    let profile = exe
        .as_deref()
        .and_then(|e| e.parent())
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| n == "release")
        .unwrap_or_else(|| "debug".to_string());
    let sib = exe.map(|e| e.with_file_name(sibling));
    (profile, sib)
}

/// Spawn a detached, backgrounded process (its own session via setsid, stdio to
/// `log`), returning its pid. This is the `nohup <cmd> >log 2>&1 &` equivalent:
/// the child outlives this CLI and ignores the controlling terminal's hangup.
fn spawn_detached(
    program: &str,
    args: &[&str],
    log: &str,
    envs: &[(&str, &str)],
) -> io::Result<u32> {
    let out = OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    for (k, v) in envs {
        cmd.env(k, v);
    }
    // setsid detaches from the controlling terminal so a terminal hangup (SIGHUP)
    // doesn't take the daemon down with the launching shell.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|c| c.id())
}

fn write_pid(inst: &Instance, name: &str, pid: u32) {
    let _ = fs::write(inst.pidfile(name), pid.to_string());
}

/// Poll `cond` every 100ms until it's true or `timeout` elapses. Returns whether
/// it became true.
fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(100));
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
