# syntax=docker/dockerfile:1
# Build waypipe for Linux from the same pinned upstream commit + macOS-portability
# patch as the host side (scripts/build-waypipe.sh), so both ends run the identical
# waypipe revision and the wire versions stay in lock-step. Then assemble a slim
# runtime image with a GNOME app + the transport bridge.

FROM rust:trixie AS waypipe-build
# Keep in sync with scripts/build-waypipe.sh.
ARG WAYPIPE_REPO=https://gitlab.freedesktop.org/mstoeckl/waypipe.git
ARG WAYPIPE_COMMIT=1ac039b4d50e2658d284e750c182266cc00efe74
WORKDIR /build
RUN git clone "$WAYPIPE_REPO" . && git checkout --quiet "$WAYPIPE_COMMIT"
# The patch is macOS-cfg-gated, so it's a no-op for the Linux build's behavior;
# applying it anyway keeps "both ends build the same source" literally true.
COPY waypipe-macos.patch /tmp/waypipe-macos.patch
RUN git apply /tmp/waypipe-macos.patch
# Match the macOS build's feature set (no lz4/zstd/dmabuf) so the two ends agree;
# the wire uses uncompressed frames (-c none).
# BuildKit cache mounts persist the cargo registry and target dir across builds.
# (The final binary is copied out within this same RUN, before the mount detaches.)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --no-default-features --release \
    && cp target/release/waypipe /usr/local/bin/waypipe

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        socat \
        dbus dbus-x11 \
        gnome \
        weston \
        fonts-dejavu-core \
        adwaita-icon-theme hicolor-icon-theme gnome-icon-theme \
        librsvg2-common \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# librsvg2-common's postinst registers the SVG pixbuf loader (for scalable/HD
# icons) and updates the loader cache automatically — no env override needed
# (a hardcoded path would be wrong on arm64 anyway).

COPY --from=waypipe-build /usr/local/bin/waypipe /usr/local/bin/waypipe
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# GTK renders into shm buffers (no GPU) via the cairo renderer.
# A UTF-8 locale is required by some apps (e.g. gnome-terminal refuses to start
# under the default ASCII C locale); glibc ships C.UTF-8 built-in.
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
