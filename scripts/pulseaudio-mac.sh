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

FOLLOW_PID="/tmp/wlmac-audio-follow.pid"
FOLLOW_LOG="/tmp/wlmac-audio-follow.log"
FOLLOW_SRC="$ROOT/scripts/audio-follow-default.swift"
FOLLOW_BIN="$ROOT/bin/audio-follow-default"

# Make the bridge follow the macOS default output device (and switch with it).
# module-coreaudio-detect pins ONE static sink that may not be your selected
# output and never follows changes; this small Swift agent repoints the default
# sink + moves live streams via a CoreAudio listener (see the .swift file).
# Optional: needs `pactl` + the Swift toolchain (Xcode CLT); degrades cleanly if
# either is missing. Always returns 0 so it can't abort the caller under set -e.
ensure_follower() {
    # Already running against this bridge? Nothing to do.
    if [ -f "$FOLLOW_PID" ] && kill -0 "$(cat "$FOLLOW_PID" 2>/dev/null)" 2>/dev/null; then
        return 0
    fi
    local pactl_bin
    pactl_bin="$(command -v pactl || true)"
    if [ -z "$pactl_bin" ] || ! command -v swiftc >/dev/null 2>&1; then
        echo "[mac] audio won't track the macOS default output (needs pactl + the Swift toolchain)"
        return 0
    fi
    # Build once (like bin/waypipe-macos); rebuild only when the source changes.
    if [ ! -x "$FOLLOW_BIN" ] || [ "$FOLLOW_SRC" -nt "$FOLLOW_BIN" ]; then
        mkdir -p "$ROOT/bin"
        if ! swiftc -O "$FOLLOW_SRC" -o "$FOLLOW_BIN" 2>"$FOLLOW_LOG.build"; then
            echo "[mac] failed to build the audio-follow agent (see $FOLLOW_LOG.build); default-output tracking disabled"
            return 0
        fi
    fi
    echo "[mac] starting audio-follow agent (tracks the macOS default output device)"
    nohup "$FOLLOW_BIN" "$pactl_bin" "tcp:127.0.0.1:4713" >"$FOLLOW_LOG" 2>&1 &
    echo $! >"$FOLLOW_PID"
    return 0
}

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
    ensure_follower
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
if pulse_bridge_up; then
    ensure_follower
else
    echo "[mac] WARNING: pulseaudio bridge failed to start (see $PULSE_LOG); audio disabled"
fi
