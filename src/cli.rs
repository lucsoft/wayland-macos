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
//! shell's `nohup ... &`. PIDs are recorded in /tmp/wlmac-*.pid; `wayland-macos
//! stop` tears them down.
//!
//! The compositor ("core") is a *separate* binary so this orchestrator shares no
//! code with it and can't drag AppKit/Wayland deps into a plain process launcher.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use log::{error, info, warn};

const CLIENT_SOCK: &str = "/tmp/waypipe-client.sock";
const COMP_LOG: &str = "/tmp/wlmac-compositor.log";
const WAYPIPE_LOG: &str = "/tmp/wlmac-waypipe.log";
const SOCAT_LOG: &str = "/tmp/wlmac-socat.log";

fn main() {
    // Log via the `log` facade to stderr, matching the compositor's format
    // (timestamp, level, target/thread). Targets use `cli`; override with
    // e.g. `RUST_LOG=cli=debug`. `--help` writes plain stdout, not a log line.
    init_logger();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("stop") => stop(),
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
         \x20 wayland-macos [up] [--multiplex]\n\
         \x20 wayland-macos stop\n\n\
         COMMANDS:\n\
         \x20 up     (default) start pulseaudio + the compositor (+ waypipe & TCP bridge for the waypipe back-end)\n\
         \x20 stop   tear down everything `up` started\n\n\
         FLAGS (forwarded to the compositor):\n\
         \x20 --multiplex                        surface each app as its own native macOS app\n\n\
         Back-end is a build-time choice: a plain build is the waypipe back-end; a\n\
         `--features rail` build is the RAIL back-end (no waypipe/bridge).\n\n\
         Env: WLMAC_MULTIPLEX=1 implies --multiplex; BRIDGE_PORT overrides the TCP\n\
         bridge port (default 7777); WAYPIPE overrides the waypipe client path;\n\
         WAYPIPE_COMPRESS sets wire compression (lz4|zstd|none, default lz4)."
    );
}

// ---------------------------------------------------------------------------
// up
// ---------------------------------------------------------------------------

fn up(flags: &[String]) {
    let multiplex = flags.iter().any(|f| f == "--multiplex")
        || std::env::var("WLMAC_MULTIPLEX").as_deref() == Ok("1");
    // The back-end is a build-time choice: a `--features rail` build is the
    // RAIL back-end, a plain build is the waypipe back-end.
    let rail = cfg!(feature = "rail");
    let root = project_root();

    // audio bridge — shared by both back-ends (optional; degrades cleanly).
    start_pulseaudio(root.as_deref());

    // compositor (built with the same feature set as this CLI).
    let core = ensure_core(root.as_deref());
    start_compositor(&core, multiplex);

    if rail {
        // RAIL: the compositor is an RDP *client* dialing into the container's
        // Weston RDP server — there's no local Wayland socket, waypipe, or socat
        // bridge (that's the waypipe pipeline). Start the container separately.
        info!(target: "cli", "RAIL back-end started; now start the container: docker compose --profile rail up wayland-rail");
        return;
    }

    // waypipe pipeline: connect a waypipe client to the compositor's socket and
    // expose it to the container over a TCP<->unix bridge.
    let (runtime, display) = discover_socket();
    let waypipe = ensure_waypipe(root.as_deref());
    start_waypipe_client(&waypipe, &runtime, &display);
    let port = std::env::var("BRIDGE_PORT").unwrap_or_else(|_| "7777".to_string());
    start_socat(&port);
    info!(target: "cli", "ready. Container should connect to host.docker.internal:{port}");
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

/// Start the compositor if it isn't already running.
fn start_compositor(core: &Path, multiplex: bool) {
    if compositor_running() {
        info!(target: "cli", "compositor already running");
        return;
    }
    let mut args: Vec<&str> = Vec::new();
    if multiplex {
        info!(target: "cli", "multiplex = on (per-app window hosts)");
        args.push("--multiplex");
    }
    info!(target: "cli", "starting compositor {}", args.join(" "));
    // Fresh log so socket discovery reads THIS run's announcement.
    let _ = fs::write(COMP_LOG, b"");
    match spawn_detached(&core.to_string_lossy(), &args, COMP_LOG, &[]) {
        Ok(pid) => write_pid("compositor", pid),
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
fn discover_socket() -> (String, String) {
    let mut runtime = String::new();
    let mut display = String::new();
    let found = wait_until(Duration::from_secs(5), || {
        let log = fs::read_to_string(COMP_LOG).unwrap_or_default();
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
        error!(target: "cli", "compositor didn't announce its socket (see {COMP_LOG})");
        std::process::exit(1);
    }
    info!(target: "cli", "compositor display: {runtime}/{display}");
    (runtime, display)
}

fn start_waypipe_client(waypipe: &Path, runtime: &str, display: &str) {
    // Replace any stale client from a previous run.
    let _ = Command::new("pkill")
        .args(["-f", "waypipe-macos.*client"])
        .status();
    let _ = fs::remove_file(CLIENT_SOCK);
    // -c lz4: compress the wire (waypipe's default low-latency codec). Must match
    // the container server's `-c` (docker/entrypoint.sh) — waypipe rejects the
    // connection on a compression-type mismatch, and a binary built without the
    // codec silently downgrades to none. WAYPIPE_COMPRESS overrides it (e.g. zstd
    // for a better ratio, none to disable); keep both ends in sync. Both binaries
    // are built with lz4+zstd support (scripts/build-waypipe.sh).
    let compress = std::env::var("WAYPIPE_COMPRESS").unwrap_or_else(|_| "lz4".to_string());
    info!(target: "cli", "starting waypipe client on {CLIENT_SOCK} (compression: {compress})");
    let child = spawn_detached(
        &waypipe.to_string_lossy(),
        &["-c", &compress, "--no-gpu", "-s", CLIENT_SOCK, "client"],
        WAYPIPE_LOG,
        &[
            ("XDG_RUNTIME_DIR", runtime),
            ("WAYLAND_DISPLAY", display),
        ],
    );
    match child {
        Ok(pid) => write_pid("waypipe", pid),
        Err(e) => {
            error!(target: "cli", "failed to start waypipe client ({e})");
            std::process::exit(1);
        }
    }
    wait_until(Duration::from_secs(5), || Path::new(CLIENT_SOCK).exists());
}

fn start_socat(port: &str) {
    let _ = Command::new("pkill")
        .args(["-f", &format!("socat TCP-LISTEN:{port}")])
        .status();
    info!(target: "cli", "starting TCP bridge on :{port} -> {CLIENT_SOCK}");
    // nodelay: disable Nagle on the TCP side (see docker/entrypoint.sh) so small
    // Wayland replies aren't stalled ~hundreds of ms.
    let child = spawn_detached(
        "socat",
        &[
            &format!("TCP-LISTEN:{port},reuseaddr,fork,nodelay"),
            &format!("UNIX-CONNECT:{CLIENT_SOCK}"),
        ],
        SOCAT_LOG,
        &[],
    );
    match child {
        Ok(pid) => write_pid("socat", pid),
        Err(e) => error!(target: "cli", "failed to start socat bridge ({e})"),
    }
}

/// True if the compositor this CLI started is still alive.
///
/// We check the pidfile rather than scanning process names: a substring match on
/// "wayland-macos-core" false-positives on any unrelated process carrying that
/// string in its argv (an editor, a `tail` on the log, or the launching shell),
/// which would wrongly skip startup and then fail on socket discovery. The
/// trade-off is that a compositor started *outside* this CLI (e.g. `cargo run
/// --bin wayland-macos-core` by hand) isn't detected — `up` would then start a
/// second one on its own socket, which is harmless.
fn compositor_running() -> bool {
    let Ok(contents) = fs::read_to_string("/tmp/wlmac-compositor.pid") else {
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

fn stop() {
    for name in ["socat", "waypipe", "compositor"] {
        let pidfile = format!("/tmp/wlmac-{name}.pid");
        if let Ok(pid) = fs::read_to_string(&pidfile) {
            let pid = pid.trim();
            if Command::new("kill")
                .arg(pid)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                info!(target: "cli", "killed {name}");
            }
        }
        let _ = fs::remove_file(&pidfile);
    }
    // Belt-and-braces: also match by command line (covers window-host children).
    for pat in ["wayland-macos-core", "waypipe-macos", "socat TCP-LISTEN:7777"] {
        let _ = Command::new("pkill").args(["-f", pat]).status();
    }
    let _ = fs::remove_file(CLIENT_SOCK);
    // Audio bridge. --check exits 0 if a daemon is running.
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
    info!(target: "cli", "done");
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

fn write_pid(name: &str, pid: u32) {
    let _ = fs::write(format!("/tmp/wlmac-{name}.pid"), pid.to_string());
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
