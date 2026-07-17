#!/usr/bin/env bash
#
# e2e-resize-kwin.sh — real nested-KWin smoke test for "no black areas on resize".
#
# Boots the compositor + a nested kwin_wayland session, resizes its window to
# double the height, screenshots the window, and fails if a significant fraction
# of it is black (the classic "resize leaves a black region because the window
# grew ahead of the client's repaint").
#
# THIS IS NOT A HEADLESS/CI TEST. It requires:
#   * Docker running (the KWin container: docker/Dockerfile.kde-debian).
#   * A logged-in macOS GUI (Aqua) session — the compositor creates real
#     NSWindows on the main thread, which fails over SSH / in CI.
#   * Accessibility permission for the terminal app that runs this script
#     (System Events → resize/raise the window). Grant it under
#     System Settings → Privacy & Security → Accessibility.
#   * Screen Recording permission (screencapture of another app's window).
#
# The deterministic, CI-friendly equivalent of this check is the cargo test
# `resize_asks_client_to_repaint_before_growing_window` in src/wayland/mod.rs,
# which asserts the same invariant at the WinCmd/input seam without Docker,
# a GUI session, or a screenshot.
#
# Usage:
#   bash scripts/e2e-resize-kwin.sh
# Tunables (env):
#   WLMAC_PROCESS   process name of the compositor            (default: wayland-macos)
#   BLACK_THRESH    per-channel value below which a px is black (default: 16)
#   BLACK_MAX_FRAC  fail if more than this fraction is black    (default: 0.02)
#   KEEP_UP         "1" to leave the container running on exit  (default: unset)

set -euo pipefail
cd "$(dirname "$0")/.."

PROC="${WLMAC_PROCESS:-wayland-macos}"
BLACK_THRESH="${BLACK_THRESH:-16}"
BLACK_MAX_FRAC="${BLACK_MAX_FRAC:-0.02}"
OUT_DIR="$(mktemp -d)"
STARTED_COMPOSITOR=0

log()  { printf '\033[1;34m[e2e]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[e2e] FAIL:\033[0m %s\n' "$*" >&2; exit 1; }

cleanup() {
  if [ "${KEEP_UP:-}" != "1" ]; then
    log "tearing down the KWin container…"
    docker compose --profile kde down >/dev/null 2>&1 || true
    if [ "$STARTED_COMPOSITOR" = "1" ]; then
      log "stopping the compositor we started…"
      pkill -x "$PROC" 2>/dev/null || true
      bash scripts/stop.sh >/dev/null 2>&1 || true
    fi
  fi
}
trap cleanup EXIT

command -v docker >/dev/null    || fail "docker not found (Docker Desktop must be running)."
command -v osascript >/dev/null || fail "osascript not found (macOS only)."
command -v screencapture >/dev/null || fail "screencapture not found (macOS only)."
command -v python3 >/dev/null   || fail "python3 not found (install the Command Line Tools)."

# 1. Compositor + waypipe + TCP bridge.
if ! pgrep -x "$PROC" >/dev/null 2>&1; then
  log "starting the compositor (scripts/mac-side.sh) in the background…"
  bash scripts/mac-side.sh >"$OUT_DIR/compositor.log" 2>&1 &
  STARTED_COMPOSITOR=1
  sleep 4
else
  log "compositor already running; reusing it."
fi

# 2. Nested KWin session.
log "starting the nested KWin container…"
docker compose --profile kde up -d wayland-kde

# 3. Wait for the KWin window to map.
log "waiting for a '$PROC' window (up to 90s)…"
for i in $(seq 1 90); do
  if osascript -e "tell application \"System Events\" to tell process \"$PROC\" to exists window 1" 2>/dev/null | grep -q true; then
    break
  fi
  [ "$i" = 90 ] && fail "no window appeared (GUI session active? Docker up? check $OUT_DIR/compositor.log)"
  sleep 1
done
sleep 3  # let KWin finish its first paint

# Bring the window forward so the screenshot isn't of something on top of it.
osascript -e "tell application \"System Events\" to tell process \"$PROC\" to set frontmost to true" 2>/dev/null || true
sleep 1

read_geom() {
  # AX position/size are screen points, top-left origin — same as screencapture -R.
  local g
  g="$(osascript -e "tell application \"System Events\" to tell process \"$PROC\" to get {position, size} of window 1" | tr -d ' ')"
  IFS=, read -r WX WY WW WH <<<"$g"
  [ -n "${WH:-}" ] || fail "could not read window geometry (Accessibility permission granted?)"
}

read_geom
log "window at ${WX},${WY}, size ${WW}x${WH}"

# 4. Double the height.
NEWH=$((WH * 2))
log "resizing to ${WW}x${NEWH} (double height)…"
osascript -e "tell application \"System Events\" to tell process \"$PROC\" to set size of window 1 to {$WW, $NEWH}"
sleep 3  # let KWin reconfigure + repaint the newly exposed region

read_geom  # re-read: the window may have been clamped to the screen
SHOT="$OUT_DIR/after-resize.png"

# 5. Screenshot the window region, then downsample so the pure-python scan is fast.
log "capturing ${WW}x${WH} at ${WX},${WY}…"
screencapture -x -o -R"${WX},${WY},${WW},${WH}" "$SHOT"
[ -s "$SHOT" ] || fail "screencapture produced no image (Screen Recording permission granted?)"
sips --resampleHeightWidthMax 500 "$SHOT" --out "$SHOT" >/dev/null 2>&1 || true

# 6. Scan for black. Fails (exit 1) if the black fraction exceeds BLACK_MAX_FRAC.
log "scanning for black regions (threshold=$BLACK_THRESH, max_frac=$BLACK_MAX_FRAC)…"
python3 - "$SHOT" "$BLACK_THRESH" "$BLACK_MAX_FRAC" <<'PY' || fail "black regions detected after resize — the window outran its buffer."
import sys, zlib, struct

path, thr, maxfrac = sys.argv[1], int(sys.argv[2]), float(sys.argv[3])
data = open(path, "rb").read()
if data[:8] != b"\x89PNG\r\n\x1a\n":
    sys.stderr.write("not a PNG\n"); sys.exit(2)

pos, width, height, depth, ctype, idat = 8, None, None, None, None, bytearray()
while pos < len(data):
    ln = struct.unpack(">I", data[pos:pos+4])[0]; pos += 4
    kind = data[pos:pos+4]; pos += 4
    chunk = data[pos:pos+ln]; pos += ln + 4  # skip CRC
    if kind == b"IHDR":
        width, height, depth, ctype = struct.unpack(">IIBB", chunk[:10])
    elif kind == b"IDAT":
        idat += chunk
    elif kind == b"IEND":
        break

if depth != 8 or ctype not in (2, 6):
    sys.stderr.write(f"unsupported PNG (depth={depth}, colortype={ctype})\n"); sys.exit(2)
ch = 4 if ctype == 6 else 3
raw = zlib.decompress(bytes(idat))
stride = width * ch

def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p-a), abs(p-b), abs(p-c)
    return a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)

out = bytearray(); prev = bytearray(stride); i = 0
for _ in range(height):
    f = raw[i]; i += 1
    line = bytearray(raw[i:i+stride]); i += stride
    if f:
        for x in range(stride):
            a = line[x-ch] if x >= ch else 0
            b = prev[x]
            c = prev[x-ch] if x >= ch else 0
            if   f == 1: line[x] = (line[x] + a) & 255
            elif f == 2: line[x] = (line[x] + b) & 255
            elif f == 3: line[x] = (line[x] + ((a + b) >> 1)) & 255
            elif f == 4: line[x] = (line[x] + paeth(a, b, c)) & 255
    out += line; prev = line

black = total = 0
for p in range(0, len(out), ch):
    total += 1
    if out[p] < thr and out[p+1] < thr and out[p+2] < thr:
        black += 1

frac = black / total if total else 0.0
sys.stderr.write(f"black fraction: {frac:.4f} ({black}/{total})\n")
sys.exit(1 if frac > maxfrac else 0)
PY

log "PASS: no significant black region after doubling the window height."
log "artifacts in $OUT_DIR"
