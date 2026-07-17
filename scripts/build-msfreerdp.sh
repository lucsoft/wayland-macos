#!/usr/bin/env bash
# Build Microsoft's FreeRDP fork (FreeRDP 2.x, with the RAIL/VAIL extensions that
# WSLg's weston-rdprail server speaks) on macOS and install it to
# ~/.local/msfreerdp. Required for the compositor's `--use-microsoft-rail-protocol`
# mode (`cargo build --features rail` links against it — see build.rs, MSFREERDP_PREFIX).
#
# Upstream FreeRDP 3 (Homebrew) does NOT interoperate with WSLg's RAIL stream, so
# the matched 2.x fork is required on the client side too.
set -euo pipefail

PREFIX="${MSFREERDP_PREFIX:-$HOME/.local/msfreerdp}"
SRC="${MSFREERDP_SRC:-/tmp/FreeRDP-mirror}"
OPENSSL_ROOT="$(brew --prefix openssl@3 2>/dev/null || brew --prefix openssl)"

echo "== cloning microsoft/FreeRDP-mirror -> $SRC =="
[ -d "$SRC/.git" ] || git clone --depth 1 https://github.com/microsoft/FreeRDP-mirror.git "$SRC"

# macOS arm64 linker rejects unaligned pointers in the static RPC_FAULT_CODES[]
# table because _RPC_FAULT_CODE sits inside a #pragma pack(push,1) region. It's a
# lookup table, not a wire struct — exclude it from the packed region.
RPC_H="$SRC/libfreerdp/core/gateway/rpc.h"
if ! grep -q 'exclude it from the surrounding' "$RPC_H"; then
  echo "== patching rpc.h (unpack _RPC_FAULT_CODE) =="
  perl -0pi -e 's/(struct _RPC_FAULT_CODE\s*\{[^}]*\};\s*typedef struct _RPC_FAULT_CODE RPC_FAULT_CODE;)/#pragma pack(pop) \/* exclude it from the surrounding pack(1) *\/\n$1\n#pragma pack(push, 1)/s' "$RPC_H"
fi

echo "== configuring (cmake) =="
# WITH_CLIENT=OFF + WITH_CLIENT_COMMON=ON: build libfreerdp-client (+ rail channel)
#   but skip the Cocoa GUI app (its CMake errors on modern CMake).
# WITH_NEON=OFF: the RemoteFX NEON codec uses ARM32 -mfpu=neon, invalid on arm64.
# CMAKE_POLICY_VERSION_MINIMUM=3.5: the fork's cmake_minimum_required predates modern CMake.
cmake -GNinja -B "$SRC/build" -S "$SRC" \
  -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -DCMAKE_INSTALL_PREFIX="$PREFIX" -DCMAKE_BUILD_TYPE=Release \
  -DWITH_NEON=OFF -DWITH_SSE2=OFF \
  -DWITH_CLIENT=OFF -DWITH_CLIENT_COMMON=ON -DWITH_SERVER=OFF \
  -DWITH_SAMPLE=OFF -DWITH_MANPAGES=OFF -DBUILD_TESTING=OFF \
  -DWITH_X11=OFF -DWITH_WAYLAND=OFF -DWITH_PULSE=OFF -DWITH_ALSA=OFF \
  -DWITH_CUPS=OFF -DWITH_FFMPEG=OFF -DWITH_SWSCALE=OFF -DCHANNEL_URBDRC=OFF \
  -DOPENSSL_ROOT_DIR="$OPENSSL_ROOT" -Wno-dev

echo "== building + installing to $PREFIX =="
ninja -C "$SRC/build"
ninja -C "$SRC/build" install
echo "== done: $PREFIX/lib/libfreerdp2.2.dylib =="
