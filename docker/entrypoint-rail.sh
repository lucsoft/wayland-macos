#!/usr/bin/env bash
# Container side of the RAIL pipeline:
#   Linux app  ->  Weston (rdp-backend, RDP server)  ->  (TCP :3389)  ->  macOS
#
# The macOS side runs a FreeRDP RAIL client (the compositor's RAIL back-end,
# a `--features rail` build) which draws each RAIL window as an
# NSWindow. Weston does the compositing here; only pixels + window metadata cross
# the boundary (contrast with the waypipe image, which forwards the Wayland
# protocol itself).

# audio_preflight <pulse_server>
# Fail loud, not silent: two things make audio vanish without any error from the
# app itself. (1) A stale image built before audio support was added has no
# libpulse — Firefox/Qt dlopen it at runtime, so they just play muted. (2) The
# Mac-side daemon isn't up (or unreachable), so the connection is refused. Warn
# on both so the cause is obvious instead of "no sound, no idea why". Always
# returns 0 — audio is optional and degrades cleanly. Defined above the
# sourced-guard below so scripts/test-audio-preflight.sh can exercise it without
# booting Weston.
audio_preflight() {
    local pulse_server="$1"
    if ! ldconfig -p 2>/dev/null | grep -q 'libpulse\.so'; then
        echo "[rail-container] WARNING: libpulse not found in this image — apps will play MUTED."
        echo "[rail-container]   This image predates audio support; rebuild it:"
        echo "[rail-container]   docker compose --profile rail build wayland-rail && docker compose --profile rail up -d --force-recreate wayland-rail"
    fi
    # Reachability probe against the configured PULSE_SERVER (tcp:HOST:PORT).
    local pa_addr="${pulse_server#tcp:}"
    local pa_host="${pa_addr%:*}"
    local pa_port="${pa_addr##*:}"
    if [ -n "$pa_host" ] && [ -n "$pa_port" ]; then
        if timeout 2 bash -c "echo > /dev/tcp/${pa_host}/${pa_port}" 2>/dev/null; then
            echo "[rail-container] audio bridge reachable at ${pa_host}:${pa_port}"
        else
            echo "[rail-container] WARNING: audio bridge ${pa_host}:${pa_port} is UNREACHABLE — apps will play MUTED."
            echo "[rail-container]   Start it on the Mac:  cargo run --features rail   (or: scripts/pulseaudio-mac.sh)"
        fi
    fi
}

# When this file is sourced (by the test harness) rather than executed, stop
# here: we only want the functions above, not the server bring-up below.
(return 0 2>/dev/null) && return 0

set -euo pipefail

PORT="${RDP_PORT:-3389}"
WIDTH="${RDP_WIDTH:-1920}"
HEIGHT="${RDP_HEIGHT:-1080}"
APP="${APP:-weston-terminal}"
WL_SOCK="wayland-rail"

# Audio: RDP/RAIL carries no audio to a usable macOS sink, so — like the waypipe
# path (docker/entrypoint.sh) — audio takes its own TCP channel straight to the
# Mac's PulseAudio daemon on :4713 (started by scripts/pulseaudio-mac.sh, which
# bridges to CoreAudio). Here the container is the RDP *server* and the Mac dials
# in, but the audio channel still runs container->Mac, so we reach back via
# MAC_HOST (host.docker.internal on Docker Desktop). Exported so the app and its
# D-Bus-activated helpers inherit it; if the Mac has no daemon, apps start muted.
export PULSE_SERVER="${PULSE_SERVER:-tcp:${MAC_HOST:-host.docker.internal}:4713}"
echo "[rail-container] audio -> ${PULSE_SERVER}"
audio_preflight "$PULSE_SERVER"

CERT_DIR="/etc/weston-rail"
CERT="${CERT_DIR}/rdp.crt"
KEY="${CERT_DIR}/rdp.key"

# Weston needs a runtime dir (0700) for its wayland socket.
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/xdg}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
export XDG_SESSION_TYPE="${XDG_SESSION_TYPE:-wayland}"

# If any child dies (Weston crashing, the app quitting, the client dropping),
# tear the whole container down rather than lingering half-broken.
pids=()
shutdown() {
    trap - EXIT TERM INT
    echo "[rail-container] a child process exited; shutting down"
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    exit "${1:-1}"
}
trap shutdown EXIT TERM INT

# The RDP backend requires a TLS certificate + key. Generate a self-signed pair
# once (dev use only). Weston refuses a key that is group/world-readable, so lock
# it down to 0600.
if [ ! -f "$CERT" ] || [ ! -f "$KEY" ]; then
    echo "[rail-container] generating self-signed RDP TLS certificate"
    mkdir -p "$CERT_DIR"
    openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout "$KEY" -out "$CERT" -days 3650 \
        -subj "/CN=wayland-macos-rail" >/dev/null 2>&1
fi
chmod 600 "$KEY"
chmod 644 "$CERT"

# System D-Bus so GTK apps (e.g. gnome-calculator) that expect it start cleanly.
echo "[rail-container] starting system dbus"
mkdir -p /run/dbus
dbus-daemon --system --nofork --nopidfile &
pids+=($!)
for _ in $(seq 1 50); do
    [ -S /run/dbus/system_bus_socket ] && break
    sleep 0.1
done

# Start Weston with the RDP backend as a server listening on all interfaces.
#   --renderer=pixman : software compositing (no GPU in the container).
#   --socket          : the wayland socket apps connect to.
#   --port/--address  : where the RDP client (macOS) connects in.
# RemoteApp/RAIL (per-window) vs full-desktop is negotiated by the connecting
# client; nothing extra is needed server-side.
# Remove any stale wayland socket from a previous run (survives `docker restart`),
# otherwise the "wait for socket" check below passes on the dead file and the app
# connects to nothing ("failed to connect to Wayland display: Connection refused").
rm -f "$XDG_RUNTIME_DIR/$WL_SOCK" "$XDG_RUNTIME_DIR/$WL_SOCK.lock"

# Launch Microsoft's Weston fork with the RAIL shell (rdprail-shell.so) so each
# app window is remoted individually (RemoteApp/RAIL), matching WSLg's
#   weston --backend=rdp-backend.so --shell=rdprail-shell.so --socket=wayland-0 ...
# TLS cert/port for the RDP backend are set on the command line (see below); if a
# flag name differs in this fork, `weston --help` in the image lists the rdp
# backend options.
echo "[rail-container] starting weston rdprail server on 0.0.0.0:${PORT}"
weston \
    --backend=rdp-backend.so \
    --shell=rdprail-shell.so \
    --socket="$WL_SOCK" \
    --address=0.0.0.0 \
    --port="$PORT" \
    --rdp-tls-cert="$CERT" \
    --rdp-tls-key="$KEY" &
pids+=($!)

# Wait for Weston's wayland socket before launching the app.
for _ in $(seq 1 100); do
    [ -S "$XDG_RUNTIME_DIR/$WL_SOCK" ] && break
    sleep 0.1
done
if [ ! -S "$XDG_RUNTIME_DIR/$WL_SOCK" ]; then
    echo "[rail-container] weston did not create its wayland socket; aborting"
    shutdown 1
fi
export WAYLAND_DISPLAY="$WL_SOCK"

echo "[rail-container] launching: ${APP}"
dbus-run-session -- \
    bash -c 'dbus-update-activation-environment --all >/dev/null 2>&1 || true; exec "$@"' _ ${APP} &
pids+=($!)

# Block until the first child exits, then let the EXIT trap stop everything.
wait -n
shutdown $?
