# wayland-macos

A proof-of-concept **native macOS Wayland compositor**: each Wayland
`xdg_toplevel` becomes a real Cocoa `NSWindow`. Combined with
[waypipe](https://gitlab.freedesktop.org/mstoeckl/waypipe) (ported here to
macOS), it runs **real Linux GUI apps from a Docker container as native macOS
windows**.

```
┌─ Docker container (Linux) ──────────────┐        ┌─ macOS host ──────────────────────────────┐
│  GNOME app (GTK4)                       │        │  socat  TCP:7777 ─▶ unix socket           │
│    │ wayland                            │        │     │                                     │
│  waypipe server                         │        │  waypipe client (macOS build)             │
│    │ unix socket                        │        │     │ wayland (real protocol + local fds) │
│  socat  unix ─▶ TCP host.docker.internal│◀──────▶│  wayland-macos compositor                 │
└─────────────────────────────────────────┘  TCP   │     └─ xdg_toplevel ─▶ NSWindow           │
                                                   └───────────────────────────────────────────┘
```

waypipe serializes shared-memory buffers inline, so no `SCM_RIGHTS` fd-passing
has to cross the container/VM boundary — that is the piece that makes this work
over plain TCP.

## What's here

| Path                             | What it is                                                                                                                                                                                                                    |
| -------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src/wayland.rs`                 | The compositor: `wl_compositor`, `wl_shm`, `xdg_shell`, `wl_seat` (pointer+keyboard), `wl_output`, `wl_data_device_manager`, `wl_subcompositor`, `xdg-decoration`. Pure-Rust `wayland-server` backend (no system libwayland). |
| `src/mac.rs`                     | AppKit side: shm buffer → `CGImage` → layer-backed `NSWindow`; a `WaylandView` subclass captures `NSEvent`s. Driven from the Wayland thread via the main GCD queue.                                                           |
| `src/input.rs`                   | Cross-thread input bus (AppKit main thread → Wayland thread, via a self-pipe).                                                                                                                                                |
| `src/wayland/clipboard.rs`       | Native macOS integration: connects the Wayland selection (`wl_data_device`) ⇆ `NSPasteboard`; its state lives on `State.clipboard`.                                                                                            |
| `src/keymap.xkb`                 | Embedded `us` xkb keymap sent to every `wl_keyboard`.                                                                                                                                                                         |
| `src/bin/testclient.rs`          | A native macOS Wayland client (gradient) to test the compositor without Docker.                                                                                                                                               |
| `bin/waypipe-macos`              | waypipe built for macOS — **generated** by `scripts/build-waypipe.sh` (git-ignored). See "waypipe port" below.                                                                                                                 |
| `docker/`                        | `Dockerfile` (GNOME app + waypipe + TCP bridge) and `waypipe-macos.patch` (the pinned macOS shim).                                                                                                                            |
| `docker-compose.yml`, `scripts/` | Orchestration.                                                                                                                                                                                                                |

## Requirements

- macOS with Xcode command-line tools, Rust, Docker Desktop, `socat`
  (`brew install socat`).
- Optional, for audio: `pulseaudio` (`brew install pulseaudio`). See
  [Audio](#audio).
- Run from a **graphical macOS session** (windows are drawn on the real
  display).

## Quick start

```sh
# macOS side only: pulseaudio + waypipe client + TCP bridge + the compositor.
cargo run                             # the wayland-macos orchestrator CLI
cargo run -- stop                     # tear it all back down

# Everything: macOS side + container app (defaults to gnome-text-editor)
./scripts/run.sh                      # or: ./scripts/run.sh gnome-calculator
```

`cargo run` (the default binary) launches the **`wayland-macos` orchestrator
CLI**, which brings up the audio bridge, builds/starts the compositor, and — for
the waypipe back-end — the `waypipe client` + TCP bridge (`cargo run -- stop`
tears it all down). `run.sh` calls it, then `docker compose run`s the container
which launches the app through `waypipe server`.

### Without Docker (compositor only)

The compositor proper is a separate binary, `wayland-macos-core`. Run it alone
(no waypipe/bridge) to talk to it directly with the test client:

```sh
cargo run --bin wayland-macos-core                    # terminal 1: compositor
# it prints XDG_RUNTIME_DIR + WAYLAND_DISPLAY to use
XDG_RUNTIME_DIR=/tmp/wayland-macos-$(id -u) \
  WAYLAND_DISPLAY=wayland-1 cargo run --bin testclient # terminal 2: client
```

## The waypipe macOS port

waypipe is Linux-first. Rather than vendor a full copy, we track upstream as a
**pinned commit + a small patch**:

- **`docker/waypipe-macos.patch`** — the macOS-portability shim against upstream
  waypipe `1ac039b` (post-0.11.0). It's ~360 lines: a new `src/compat.rs` plus
  cfg-gated edits to 5 files.
- **`scripts/build-waypipe.sh`** — clones that commit, applies the patch, builds
  with `--no-default-features`, and writes `bin/waypipe-macos` (git-ignored, so
  the 1.7 MB binary isn't committed). `cargo run` runs it automatically if the
  binary is missing. To track a newer upstream, bump `WAYPIPE_COMMIT` (in the
  script **and** `docker/Dockerfile`) and re-apply the patch.

The container's `Dockerfile` uses the **same** commit + patch to build waypipe
for Linux, so both ends run the identical revision (wire protocol versions stay
in lock-step). The patch is macOS-cfg-gated, so it's a no-op for the Linux build.

The shim supplies what Darwin lacks:

- `memfd_create` → a regular **unlinked temp file** (waypipe never applies file
  seals). Deliberately *not* `shm_open`: macOS POSIX shm objects can only be
  sized once, and waypipe grows shm buffers with `ftruncate` (e.g. when a client
  resizes a cursor pool), so a regular resizable file is required.
- `pipe2` → `pipe` + `fcntl`.
- `ppoll` → `poll` (the atomic signal-mask swap is dropped; waypipe also wakes
  on a self-pipe).
- `SOCK_CLOEXEC`/`SOCK_NONBLOCK` → applied via `fcntl` after
  `socket`/`socketpair`.
- `waitid` → treated as "no child" (the macOS side runs `client` mode, which
  doesn't fork the app).

Built with `--no-default-features` (no lz4/zstd/dmabuf/video), so both ends use
uncompressed frames (`-c none`).

## Input

Mouse and keyboard are forwarded. A `WaylandView` subclass captures `NSEvent`s
(motion, buttons, scroll, keys, modifiers), translates them (coords → surface
pixels, buttons → evdev, keys → evdev via a macOS→evdev table, modifiers → xkb
masks) and pushes them across a self-pipe to the Wayland thread, which emits
`wl_pointer` / `wl_keyboard` events to the focused surface. Keyboard focus
follows pointer enter/leave.

## Audio

waypipe forwards only the Wayland wire — it carries no audio. Audio therefore
takes its own channel: a **PulseAudio daemon on the Mac** exposes CoreAudio as
Pulse sinks/sources and listens on TCP (port 4713), and Linux apps in the
container connect to it via `PULSE_SERVER=tcp:host.docker.internal:4713`.

```
Linux app ──libpulse──> PULSE_SERVER (tcp) ──> PulseAudio on macOS ──> CoreAudio
```

- **Mac side:** `cargo run` starts the daemon (via `scripts/pulseaudio-mac.sh`,
  from the `scripts/pulseaudio-mac.pa` config) for **either** back-end if
  `pulseaudio` is installed (`brew install pulseaudio`) and nothing is already
  serving on TCP 4713; otherwise it prints a hint and continues without audio.
  `cargo run -- stop` tears it down. You can also run
  `scripts/pulseaudio-mac.sh` directly (idempotent).
- **Container side:** `docker/entrypoint.sh` (waypipe) and
  `docker/entrypoint-rail.sh` (RAIL) export `PULSE_SERVER` (pointing at
  `MAC_HOST`), and the images ship the PulseAudio client libraries. If the Mac
  has no daemon running, apps simply start muted — nothing else breaks.
- **Same channel in RAIL mode:** RDP/RAIL carries no audio to a usable macOS sink
  either (the Mac-side FreeRDP is built without a Pulse/ALSA backend), so RAIL
  reuses this exact out-of-band PulseAudio channel — the container is the RDP
  server but the audio still flows container → Mac.
- **Test it:** `pactl list sinks short` on the Mac shows the CoreAudio sink;
  inside a running container, `paplay /usr/share/sounds/…` (or a video in the
  `firefox` profile) should come out of the Mac's speakers.

> **Security:** the daemon listens on `0.0.0.0` with anonymous auth (like the
> waypipe TCP bridge), and CoreAudio also exposes the Mac's **microphone** as a
> Pulse source. On an untrusted network, add an `auth-ip-acl` in
> `scripts/pulseaudio-mac.pa` or block port 4713 at the firewall.

## Native macOS integrations

Some Wayland facilities have to be wired to the equivalent native macOS service.
Each such integration lives in its own module under `src/wayland/`, keeps its
state in a field on the compositor `State` (e.g. `state.clipboard`), and
implements the `Dispatch` handlers for its own protocol objects. Adding one means
dropping in a module, a `State` field, and (if needed) a global in `run`.

> **Not a "portal".** In the Linux world a _portal_
> ([xdg-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/)) is a
> specific D-Bus service for sandboxed apps — a different thing, and there's no
> D-Bus on macOS. The clipboard, for instance, is _core_ Wayland protocol, not a
> portal; we just answer it against the native macOS service.

### Implemented

- **Clipboard** — `wl_data_device` ⇆ `NSPasteboard` (`src/wayland/clipboard.rs`).
  Both directions route through the pasteboard, so Wayland-to-Wayland copy/paste
  works too. Plain UTF-8 text only.
  - **Copy (Wayland → macOS).** On `wl_data_device.set_selection` we ask the
    source client to write its bytes into a pipe (`wl_data_source.send`), read
    them on a background thread, and put the text on the pasteboard.
  - **Paste (macOS → Wayland).** A main-thread poller watches the pasteboard's
    `changeCount`; on a change it pushes the text to the Wayland thread, which
    advertises a fresh `wl_data_offer` to every client. On
    `wl_data_offer.receive` we write the snapshot back into the client's pipe.
- **Cursor shape** — `wp_cursor_shape_v1` ⇆ `NSCursor` (`src/wayland/cursor_shape.rs`).
  On `set_shape` we map the requested shape to the closest `NSCursor` via
  `WinCmd::SetCursor` (`map_cursor` in `mac.rs`); a client-drawn cursor surface
  is forwarded through `WinCmd::SetCursorImage`.
- **Primary selection** — `zwp_primary_selection_v1`, the X11-style middle-click
  paste (`src/wayland/primary_selection.rs`). Client-to-client only — macOS has
  no primary-selection pasteboard, so it never leaves the Wayland side.

### Existing glue

These are already Wayland ⇆ macOS integrations in spirit; they predate this
section and still live inline:

- **Input** — `NSEvent` → `wl_pointer` / `wl_keyboard` (`mac.rs`, `input.rs`).
- **Window lifecycle** — `xdg_toplevel` ⇆ `NSWindow` create/title/resize/close
  (`wayland.rs`, `mac.rs`).

### Missing integrations

These would all live on the Wayland socket — that's the compositor's remit.
Natural next additions:

- **Rich clipboard** — images / RTF / files across `wl_data_offer` MIMEs ⇆
  `NSPasteboardType{PNG,TIFF,RTF,FileURL}` (extends the clipboard integration).
- **Drag-and-drop** — `wl_data_device` DnD (`start_drag`) ⇆ `NSDraggingSession`.
- **Text input / IME** — `text-input-v3` ⇆ `NSTextInputClient` (marked text,
  candidate window).
- **Idle inhibit** — `idle-inhibit-v1` ⇆ `IOPMAssertion` (keep the display awake).
- **Pointer lock / relative motion** — `pointer-constraints-v1` /
  `relative-pointer-v1` ⇆ `CGAssociateMouseAndMouseCursorPosition` (for games).
  The globals are already accepted as inert stubs (enough for KWin to start
  nested); no real lock/confine or synthesized relative motion yet.
- **Tablet** — `tablet-v2` ⇆ `NSEvent` pressure/tilt.

### Out of scope: desktop portals

Actual [xdg-desktop-portal] services (file chooser, screenshot, screencast,
settings, notifications) are **D-Bus**, not Wayland. They never reach this
compositor — waypipe forwards only the Wayland socket, so a portal call stays on
the app's own session bus. Answering them would be a *separate* component (a
macOS `xdg-desktop-portal` backend + a D-Bus transport), not part of the
compositor, and is out of scope here. In practice GTK/Qt fall back to in-process
dialogs when no portal is present — e.g. an "open file" dialog renders as an
ordinary Wayland surface and already shows up as a native `NSWindow`.

[xdg-desktop-portal]: https://flatpak.github.io/xdg-desktop-portal/

## Known limitations (it's a PoC)

- **shm only.** No DMABUF/GPU path; apps run with the software renderer.
- **Single-output.** The compositor advertises one virtual `wl_output`. It
  derives scale from the max backing factor across screens and tracks the active
  display, but there is no true multi-monitor topology.
- **Clipboard is text-only.** See the clipboard integration above; rich types and
  drag-and-drop are listed under [missing integrations](#missing-integrations).

## Next steps

- True multi-output (per-monitor `wl_output` geometry and scale).
- GPU / DMABUF buffer path (drop the software-only renderer).
- Fill in the [missing integrations](#missing-integrations), starting with the
  pure-Wayland ones (rich clipboard, drag-and-drop, text input).
