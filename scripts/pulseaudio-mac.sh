#!/usr/bin/env bash
# Start the Mac-side PulseAudio daemon that bridges the container's audio to
# CoreAudio (listens on TCP :4713; see scripts/pulseaudio-mac.pa).
#
# This is the out-of-band audio channel shared by BOTH back-ends — neither
# waypipe (Wayland-only) nor RDP/RAIL carries audio to a usable macOS sink, so
# Linux apps stream straight to this daemon via PULSE_SERVER=tcp:<mac>:4713.
# `scripts/mac-side.sh` calls this for the waypipe pipeline; RAIL users (who run
# the compositor with --use-microsoft-rail-protocol, not mac-side.sh) can run it
# directly. Idempotent: a no-op if a daemon is already running.
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

if pulseaudio --check 2>/dev/null; then
    echo "[mac] pulseaudio already running"
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
for _ in $(seq 1 50); do pulseaudio --check 2>/dev/null && break; sleep 0.1; done
pulseaudio --check 2>/dev/null \
    || echo "[mac] WARNING: pulseaudio failed to start (see $PULSE_LOG); audio disabled"
