#!/usr/bin/env bash
# macOS side of the pipeline. Starts (if needed):
#   1. the native Wayland compositor  (Wayland surface -> NSWindow)
#   2. waypipe client                 (connects to the compositor, listens on a unix socket)
#   3. a socat TCP<->unix bridge       (so the container can reach waypipe client)
#
# All three run in the background; PIDs are recorded in /tmp/wlmac-*.pid.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WAYPIPE="${WAYPIPE:-$ROOT/bin/waypipe-macos}"
PORT="${BRIDGE_PORT:-7777}"
CLIENT_SOCK="/tmp/waypipe-client.sock"
COMP_LOG="/tmp/wlmac-compositor.log"

echo "[mac] building compositor..."
( cd "$ROOT" && cargo build --quiet )

# Build the waypipe client from pinned upstream + patch if it's not present.
if [ ! -x "$WAYPIPE" ]; then
    echo "[mac] waypipe client missing; building it"
    "$ROOT/scripts/build-waypipe.sh"
fi

# 1. compositor
if ! pgrep -f "target/debug/wayland-macos" >/dev/null 2>&1; then
    echo "[mac] starting compositor"
    nohup "$ROOT/target/debug/wayland-macos" >"$COMP_LOG" 2>&1 &
    echo $! >/tmp/wlmac-compositor.pid
    sleep 1
else
    echo "[mac] compositor already running"
fi

# Discover the socket the compositor bound.
RUNTIME="$(grep -m1 'export XDG_RUNTIME_DIR' "$COMP_LOG" | sed 's/.*=//' | tr -d ' ')"
DISPLAY_NAME="$(grep -m1 'export WAYLAND_DISPLAY' "$COMP_LOG" | sed 's/.*=//' | tr -d ' ')"
export XDG_RUNTIME_DIR="$RUNTIME"
export WAYLAND_DISPLAY="$DISPLAY_NAME"
echo "[mac] compositor display: $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"

# 2. waypipe client (connects to the compositor via WAYLAND_DISPLAY above)
pkill -f "waypipe-macos.*client" 2>/dev/null || true
rm -f "$CLIENT_SOCK"
echo "[mac] starting waypipe client on $CLIENT_SOCK"
nohup "$WAYPIPE" -c none --no-gpu -s "$CLIENT_SOCK" client >/tmp/wlmac-waypipe.log 2>&1 &
echo $! >/tmp/wlmac-waypipe.pid
for _ in $(seq 1 50); do [ -S "$CLIENT_SOCK" ] && break; sleep 0.1; done

# 3. TCP <-> unix bridge so the container can connect
pkill -f "socat TCP-LISTEN:${PORT}" 2>/dev/null || true
echo "[mac] starting TCP bridge on :${PORT} -> $CLIENT_SOCK"
nohup socat "TCP-LISTEN:${PORT},reuseaddr,fork" "UNIX-CONNECT:${CLIENT_SOCK}" \
    >/tmp/wlmac-socat.log 2>&1 &
echo $! >/tmp/wlmac-socat.pid

echo "[mac] ready. Container should connect to host.docker.internal:${PORT}"
