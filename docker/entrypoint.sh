#!/usr/bin/env bash
# Container side of the pipeline:
#   GNOME app  ->  waypipe server  ->  (unix socket)  ->  socat  ->  TCP  ->  macOS
#
# The macOS side runs `waypipe client` (behind a TCP<->unix socat bridge) which
# replays everything into our native Wayland compositor.
set -euo pipefail

HOST="${MAC_HOST:-host.docker.internal}"
PORT="${BRIDGE_PORT:-7777}"
APP="${APP:-gnome-text-editor}"
SOCK="/tmp/wp-server.sock"

# waypipe (and the app) need a runtime dir for their sockets.
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/xdg}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

# There's no logind session in the container, so XDG_SESSION_TYPE is unset.
# Nested mutter (gnome-shell --nested --wayland) rejects that with "Unsupported
# session type"; declaring wayland fixes it and is correct for plain apps too.
export XDG_SESSION_TYPE="${XDG_SESSION_TYPE:-wayland}"

# Audio: route libpulse clients to the PulseAudio daemon on the Mac (which
# bridges to CoreAudio — see scripts/mac-side.sh). waypipe forwards only Wayland,
# so audio takes its own TCP channel straight to the host; 4713 is PulseAudio's
# default port. If the Mac has no PulseAudio running, apps just start muted.
# Exported here so dbus-update-activation-environment (below) hands it to every
# D-Bus-activated helper too.
export PULSE_SERVER="${PULSE_SERVER:-tcp:${HOST}:4713}"
echo "[container] audio -> ${PULSE_SERVER}"

# If any child dies (the app/waypipe crashing, or the bridge dropping), tear the
# whole container down instead of lingering in a half-broken state.
pids=()
shutdown() {
    trap - EXIT TERM INT
    echo "[container] a child process exited; shutting down"
    # Kill the remaining children and take the whole process group with us.
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    exit "${1:-1}"
}
trap shutdown EXIT TERM INT

# System bus: a single app only needs the session bus (from dbus-run-session
# below), but a full gnome-session also wants the system bus. Starting it is
# harmless in the single-app case, so we always bring it up to support both.
# This is a plain dbus-daemon, NOT systemd — the systemd warnings gnome-session
# prints are an expected, harmless fallback.
echo "[container] starting system dbus"
mkdir -p /run/dbus
dbus-daemon --system --nofork --nopidfile &
pids+=($!)
# Wait for the system bus socket to appear before launching anything that needs it.
for _ in $(seq 1 50); do
    [ -S /run/dbus/system_bus_socket ] && break
    sleep 0.1
done

echo "[container] bridging unix ${SOCK}  ->  tcp ${HOST}:${PORT}"
rm -f "$SOCK"
# `fork`: waypipe may open more than one transport connection, so the bridge must
# keep accepting (a single-connection socat would ECONNREFUSED the rest). Teardown
# is driven by the app process exiting (below), not by socat.
# `nodelay`: disable Nagle's algorithm. The Wayland wire is a chatty request/reply
# protocol of small messages; with Nagle, a reply (e.g. wl_output geometry after a
# bind) can stall for hundreds of ms, which makes Qt clients give up on output
# detection during startup and fall back to a 0x0 placeholder screen — breaking
# popup/menu positioning. nodelay flushes each message immediately.
socat "UNIX-LISTEN:${SOCK},reuseaddr,fork" "TCP:${HOST}:${PORT},nodelay" &
pids+=($!)

# Wait for the bridge socket to appear.
for _ in $(seq 1 50); do
    [ -S "$SOCK" ] && break
    sleep 0.1
done

# Run waypipe as a display SERVER that exposes a NAMED socket
# ($XDG_RUNTIME_DIR/wayland-0) rather than spawning the app itself. This is the
# crucial bit: if you pass `server -- app`, waypipe hands that one process the
# display as an inherited fd (WAYLAND_SOCKET), so the shell it spawns and any
# child/D-Bus-activated program get no display ("Cannot open display"). A named
# socket lets EVERY client — the app, its shell, launched programs, and activated
# helpers — connect via WAYLAND_DISPLAY. (No --oneshot: several clients connect.)
echo "[container] starting waypipe display server (wayland-0)"
waypipe -c none --no-gpu --display wayland-0 -s "$SOCK" server &
pids+=($!)
for _ in $(seq 1 100); do
    [ -S "$XDG_RUNTIME_DIR/wayland-0" ] && break
    sleep 0.1
done
export WAYLAND_DISPLAY=wayland-0

echo "[container] launching: ${APP}"
# dbus-run-session inherits WAYLAND_DISPLAY (set above), so the session bus and
# every D-Bus-activated service get it too. The shim additionally pushes the env
# into the bus's activation set as belt-and-suspenders.
dbus-run-session -- \
    bash -c 'dbus-update-activation-environment --all >/dev/null 2>&1 || true; exec "$@"' _ ${APP} &
pids+=($!)

# Block until the first child exits (the app quitting, or the socat bridge
# dropping when the macOS side goes away), then let the EXIT trap stop everything.
wait -n
shutdown $?
