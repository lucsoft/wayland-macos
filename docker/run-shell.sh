#!/usr/bin/env bash
# A minimal desktop "shell" for the wayland-macos compositor: a docked bar
# (waybar) plus an app launcher (fuzzel). Both are wlr-layer-shell clients that
# connect to the same waypipe display, so each becomes a native macOS window.
#
# The bar carries a launcher button (its "custom/launch" module runs fuzzel on
# click — see waybar/config.jsonc), so the launcher is re-summonable without a
# focus-stealing respawn loop. We also pop fuzzel once at startup as a demo.
# Apps launched from fuzzel run as their own clients and appear as native windows.
set -u

# fuzzel stores its recent/most-used cache here; without it, it logs a warning.
mkdir -p "${XDG_CACHE_HOME:-$HOME/.cache}"

# Docked bar (top edge, reserves an exclusive zone; has a launcher button).
waybar &

# Show the launcher once at startup; dismiss with Esc or pick an app. fuzzel
# detaches the programs it launches, so they survive it exiting.
fuzzel || true

# Keep the bar (and the container) alive after the launcher closes.
wait
