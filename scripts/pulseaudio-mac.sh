#!/usr/bin/env bash
# Start the Mac-side PulseAudio daemon that bridges the container's audio to
# CoreAudio (listens on TCP :4713; see scripts/pulseaudio-mac.pa).
#
# This is the out-of-band audio channel shared by BOTH back-ends — neither
# waypipe (Wayland-only) nor RDP/RAIL carries audio to a usable macOS sink, so
# Linux apps stream straight to this daemon via PULSE_SERVER=tcp:<mac>:4713.
# The `wayland-macos` CLI (`cargo run`) calls this for either back-end; you can
# also run it directly. Idempotent: a no-op if the bridge is already serving.
#
# Optional and self-healing: if `pulseaudio` isn't installed it prints a hint and
# exits 0, so callers never fail on a missing audio bridge.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PULSE_LOG="/tmp/wlmac-pulseaudio.log"

if ! command -v pulseaudio >/dev/null 2>&1; then
    echo "[mac] pulseaudio not found — audio passthrough disabled."
    echo "[mac]   enable it with:  brew install pulseaudio"
    exit 0
fi

# Is our CoreAudio bridge actually reachable on TCP 4713? We probe the port
# rather than `pulseaudio --check`, because --check is true for ANY running
# pulseaudio — including a stray default daemon with no CoreAudio module and no
# TCP listener, which would leave container audio silently dead while we assumed
# the bridge was up. A localhost connect (bash /dev/tcp) means it's serving;
# connection-refused is instant, so no timeout is needed.
pulse_bridge_up() { (exec 3<>/dev/tcp/127.0.0.1/4713) 2>/dev/null; }

if pulse_bridge_up; then
    echo "[mac] pulseaudio bridge already serving on :4713"
    exit 0
fi

echo "[mac] starting pulseaudio (CoreAudio bridge) on :4713"
# Background it with the SHELL (nohup ... &), NOT PulseAudio's own --daemonize:
# its daemonizer double-forks, and CoreAudio's HAL is not fork-safe on macOS, so
# the forked daemon dies on startup. A plain background process keeps the original
# (unforked) process, which works. -n: ignore the default system config; -F: run
# only our script (loads CoreAudio + the TCP listener). --exit-idle-time=-1: never
# self-exit.
nohup pulseaudio -n -F "$ROOT/scripts/pulseaudio-mac.pa" --exit-idle-time=-1 \
    >"$PULSE_LOG" 2>&1 &
echo $! >/tmp/wlmac-pulseaudio.pid
for _ in $(seq 1 50); do pulse_bridge_up && break; sleep 0.1; done
# Confirm via the port too — if a foreign pulseaudio already holds the per-user
# runtime, our `-n` daemon can't start and 4713 stays closed.
pulse_bridge_up \
    || echo "[mac] WARNING: pulseaudio bridge failed to start (see $PULSE_LOG); audio disabled"
