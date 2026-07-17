#!/usr/bin/env bash
# Tear down the macOS-side processes started by mac-side.sh.
set -u
for name in socat waypipe compositor; do
    pidfile="/tmp/wlmac-${name}.pid"
    [ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null && echo "[stop] killed $name"
    rm -f "$pidfile"
done
pkill -f "target/debug/wayland-macos" 2>/dev/null || true
pkill -f "waypipe-macos" 2>/dev/null || true
pkill -f "socat TCP-LISTEN:7777" 2>/dev/null || true
rm -f /tmp/waypipe-client.sock
# Audio bridge (started by mac-side.sh). --check exits 0 if a daemon is running.
if command -v pulseaudio >/dev/null 2>&1 && pulseaudio --check 2>/dev/null; then
    pulseaudio --kill 2>/dev/null && echo "[stop] killed pulseaudio"
fi
rm -f /tmp/wlmac-pulseaudio.pid
echo "[stop] done"
