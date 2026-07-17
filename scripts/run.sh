#!/usr/bin/env bash
# One-shot launcher: bring up the macOS side, then run the container that starts
# a GNOME app through waypipe. The app window appears as a native NSWindow.
#
# Usage:  scripts/run.sh [app [args...]]
#   app defaults to gnome-text-editor; try gnome-calculator or weston-simple-shm.
#   Extra args are forwarded, e.g.:  scripts/run.sh gnome-shell --nested --wayland
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Capture the whole command line (app + flags), not just the first word.
APP="${*:-gnome-text-editor}"

# 1. macOS side: pulseaudio + compositor + waypipe client + TCP bridge.
# `up` starts them detached and returns, so we can go on to launch the container.
( cd "$ROOT" && cargo run --quiet --bin wayland-macos -- up )

# 2. container side: build + run, launching the app through waypipe
echo "[run] starting container app: $APP"
cd "$ROOT"
APP="$APP" docker compose run --build --rm -e "APP=$APP" wayland-app
