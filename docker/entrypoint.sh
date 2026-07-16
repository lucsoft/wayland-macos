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
socat "UNIX-LISTEN:${SOCK},reuseaddr,fork" "TCP:${HOST}:${PORT}" &
pids+=($!)

# Wait for the bridge socket to appear.
for _ in $(seq 1 50); do
    [ -S "$SOCK" ] && break
    sleep 0.1
done

echo "[container] launching via waypipe: ${APP}"
# waypipe server connects to $SOCK, spawns the app, and forwards its Wayland
# traffic (buffers serialized inline, so no fd-passing crosses the boundary).
#
# The app is launched through a small shim that first runs
# dbus-update-activation-environment: waypipe sets WAYLAND_DISPLAY only after the
# session bus (from dbus-run-session) is already up, so D-Bus-activated services
# (e.g. gnome-terminal-server) would otherwise start without WAYLAND_DISPLAY and
# fail with "Cannot open display". Pushing the env into the bus's activation set
# fixes that; it's a harmless no-op for apps that don't use D-Bus activation.
dbus-run-session -- waypipe -c none --no-gpu -s "$SOCK" server -- \
    bash -c 'dbus-update-activation-environment --all >/dev/null 2>&1 || true; exec "$@"' _ ${APP} &
pids+=($!)

# Block until the first child exits, then let the EXIT trap stop everything.
wait -n
shutdown $?
