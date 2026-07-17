# syntax=docker/dockerfile:1
# Alpine-based runtime — much newer GNOME (50) than Debian trixie. waypipe is
# built for musl from the same pinned upstream commit + macOS-portability patch as
# the host side (scripts/build-waypipe.sh), so both ends run the identical revision
# and the wire versions stay in lock-step.

FROM rust:alpine AS waypipe-build
# Keep in sync with scripts/build-waypipe.sh.
ARG WAYPIPE_REPO=https://gitlab.freedesktop.org/mstoeckl/waypipe.git
ARG WAYPIPE_COMMIT=1ac039b4d50e2658d284e750c182266cc00efe74
# build-base + the lz4/zstd wrapper build-deps: pkgconf finds the libs, lz4-dev/
# zstd-dev provide the headers, lz4-static/zstd-static the .a archives (the musl
# waypipe binary links statically, so it needs the static libs — the runtime stage
# then carries no lz4/zstd .so dependency), and clang-dev supplies libclang for the
# `bindgen` CLI (installed below) that generates the FFI bindings.
# target-feature=-crt-static: build `bindgen` dynamically linked — a static musl
# binary cannot dlopen libclang.so ("Dynamic loading not supported"), which bindgen
# does at runtime to parse the C headers.
RUN apk add --no-cache git build-base pkgconf lz4-dev zstd-dev lz4-static zstd-static clang-dev \
    && RUSTFLAGS="-C target-feature=-crt-static" cargo install bindgen-cli --locked
WORKDIR /build
RUN git clone "$WAYPIPE_REPO" . && git checkout --quiet "$WAYPIPE_COMMIT"
# The patch is macOS-cfg-gated, so it's a no-op for the Linux build's behavior;
# applying it anyway keeps "both ends build the same source" literally true.
COPY waypipe-macos.patch /tmp/waypipe-macos.patch
RUN git apply /tmp/waypipe-macos.patch
# Match the macOS build's feature set: lz4 + zstd wire compression, no
# dmabuf/video/gbmfallback (shm-only/software — no GPU). Both ends enable the same
# codecs so the compression chosen by `-c` (mac-side.sh / entrypoint.sh) agrees;
# waypipe rejects a connection whose peer advertises a different compression type.
# BuildKit cache mounts persist the registry/target.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --no-default-features --features lz4,zstd --release \
    && cp target/release/waypipe /usr/local/bin/waypipe

FROM alpine:edge
# The GNOME apps live in community/testing on edge.
RUN printf '%s\n' \
      https://dl-cdn.alpinelinux.org/alpine/edge/main \
      https://dl-cdn.alpinelinux.org/alpine/edge/community \
      https://dl-cdn.alpinelinux.org/alpine/edge/testing \
      > /etc/apk/repositories \
 && apk add --no-cache \
      bash socat \
      dbus dbus-x11 \
      pulseaudio-utils \
      gnome-console gnome \
      gtk4.0 \
      gdk-pixbuf librsvg \
      adwaita-icon-theme hicolor-icon-theme \
      gsettings-desktop-schemas dconf \
      font-dejavu \
      mesa-dri-gallium mesa-egl mesa-gl \
      xwayland \
 && dbus-uuidgen --ensure=/etc/machine-id

COPY --from=waypipe-build /usr/local/bin/waypipe /usr/local/bin/waypipe
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# GTK renders into shm buffers (no GPU) via the cairo renderer. musl is UTF-8
# natively but some apps still check LANG.
ENV GDK_BACKEND=wayland \
    GSK_RENDERER=cairo \
    LIBGL_ALWAYS_SOFTWARE=1 \
    GTK_A11Y=none \
    GTK_USE_PORTAL=0 \
    GIO_USE_PORTAL=0 \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    APP=gnome-text-editor \
    MAC_HOST=host.docker.internal \
    BRIDGE_PORT=7777

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
