#!/usr/bin/env bash
# Build the macOS waypipe client (bin/waypipe-macos) from a pinned upstream commit
# plus our macOS-portability patch (docker/waypipe-macos.patch).
#
# This is the host-side counterpart of the Dockerfile's waypipe-build stage: both
# use the same commit + patch, so the two ends run the identical waypipe revision.
# Idempotent — re-run to rebuild. Bump WAYPIPE_COMMIT (here and in docker/Dockerfile)
# to track a newer upstream; re-apply the patch and resolve any conflicts.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

WAYPIPE_REPO="${WAYPIPE_REPO:-https://gitlab.freedesktop.org/mstoeckl/waypipe.git}"
WAYPIPE_COMMIT="${WAYPIPE_COMMIT:-1ac039b4d50e2658d284e750c182266cc00efe74}"
PATCH="$ROOT/docker/waypipe-macos.patch"
BUILD_DIR="${WAYPIPE_BUILD_DIR:-$ROOT/.waypipe-build}"
OUT="$ROOT/bin/waypipe-macos"

echo "[waypipe] pinned commit: $WAYPIPE_COMMIT"

if [ ! -d "$BUILD_DIR/.git" ]; then
    echo "[waypipe] cloning $WAYPIPE_REPO -> $BUILD_DIR"
    git clone "$WAYPIPE_REPO" "$BUILD_DIR"
fi

# Pin to the exact commit (fetch first in case it's newer than the local clone).
git -C "$BUILD_DIR" fetch --quiet origin || true
git -C "$BUILD_DIR" reset --hard --quiet "$WAYPIPE_COMMIT"
git -C "$BUILD_DIR" clean -fdq

echo "[waypipe] applying $(basename "$PATCH")"
git -C "$BUILD_DIR" apply "$PATCH"

# Match the container build's feature set (no lz4/zstd/dmabuf); the wire uses
# uncompressed frames (-c none), so both ends agree.
echo "[waypipe] building (cargo --no-default-features --release)"
( cd "$BUILD_DIR" && cargo build --no-default-features --release )

mkdir -p "$ROOT/bin"
cp "$BUILD_DIR/target/release/waypipe" "$OUT"
echo "[waypipe] wrote $OUT"
