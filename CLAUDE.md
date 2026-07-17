# wayland-macos

A native macOS Wayland compositor written in Rust. Each Wayland `xdg_toplevel`
becomes a real `NSWindow`; Linux GUI apps run inside a Docker container and are
forwarded across the container boundary by **waypipe**, so they appear as native
macOS windows.

```
Linux app ──> waypipe (server, in container) ──TCP──> waypipe (client, macOS)
                                                            │ unix socket
                                                            ▼
                                              this compositor (wayland thread)
                                                            │ WinCmd via mac::post
                                                            ▼
                                                     AppKit main thread (NSWindow)
```

## Threading model (the one thing to internalise)

- The **Wayland protocol** runs on a background thread (`src/wayland/`).
- **AppKit/Cocoa** owns the **main thread** (`src/mac.rs`) — all `NSWindow` work
  must happen there.
- The two communicate over a single seam:
  - wayland → AppKit: `mac::post(WinCmd)` (marshals onto the GCD main queue).
  - AppKit → wayland: `InputBus` self-pipe in `src/input.rs` (`InputEvent`).
- `WinCmd` (`src/mac.rs`) is the complete vocabulary of window operations
  (`Create`, `Frame`, `Title`, `Destroy`, `CreatePopup`, `Maximize`, `SetCursor`,
  `StartMove`, `StartResize`, …). It is `Send`, which also makes it the natural
  test seam (see Testing).

## Multiplex mode (`--multiplex`)

By default one `NSApplication` (this process) hosts every window, so the Dock/
Cmd-Tab shows a single "wayland-macos". macOS ties Dock tiles and Cmd-Tab entries
to *processes*, not windows, so the only way to show each app separately is one
process per app. `--multiplex` does exactly that:

- The compositor process becomes an `Accessory` app (no Dock tile, no Cmd-Tab)
  and owns **no** `NSWindow`s. It still runs the Wayland engine, the clipboard
  bridge, and an AppKit run loop.
- It spawns one **window-host** child (`--window-host <sock>`) per Wayland client,
  keyed by a stable `app_key` (`State::client_app_ids`). A host is a normal
  `NSApplication` — `Regular` for toplevels (its own Dock tile / Cmd-Tab entry),
  `Accessory` for a layer-shell bar.
- Each host shows the **app's real name** (not "wayland-macos") in the Dock/Cmd-Tab.
  macOS derives a non-bundled app's name from its *executable file name* (not
  argv[0]), so `router::spawn_helper` execs the child through a symlink whose
  basename is the app name — derived from `xdg_toplevel.set_app_id` (fallback:
  title) by `app_display_name`. Verified via LaunchServices' `LSDisplayName`.
- A host's **Dock icon** is set only when the client provides buffer artwork via
  `xdg_toplevel_icon_v1` (→ `WinCmd::SetIcon` → `NSApplication.setApplicationIcon
  Image:`). Otherwise no icon is set (the default icon stands). Themed-name icons
  can't be resolved to artwork on macOS, so those get no icon either.
- The seam is unchanged: `mac::post(WinCmd)` normally hits the local GCD main
  queue; with the router enabled it serializes the `WinCmd` and sends it to the
  owning host (`src/router.rs` → `src/ipc.rs`), whose `handle()` runs the *same*
  AppKit code as in-process. Input flows back as `InputEvent`s over the same
  socket into the compositor's `InputBus`. Window ids are global, so they key
  hosts unambiguously.

Enable it with `WLMAC_MULTIPLEX=1 cargo run` or `cargo run -- --multiplex` (the
CLI forwards the flag to the compositor). First-cut limitations, all documented in `router.rs`:
cursor commands are broadcast to every host rather than routed to the focused
one; frame pixels cross the socket as a plain copy (IOSurface zero-copy is a
later optimization — the wire type already carries the `Vec<u8>`); and a host
that stalls its socket read applies back-pressure to the Wayland thread.

## Source map

There are two binaries: **`wayland-macos`** (`src/cli.rs`, the *default* `cargo
run` target) is the end-user orchestrator; **`wayland-macos-core`** (`src/main.rs`)
is the compositor proper. The CLI builds + launches the core alongside the audio
bridge (both back-ends) and, for the waypipe back-end, the waypipe client + socat
bridge.

| Path | Responsibility |
|------|----------------|
| `src/cli.rs` | **`wayland-macos`** binary (default `cargo run`): orchestrator that starts the pulseaudio bridge + the compositor, plus (waypipe back-end only) the waypipe client + socat bridge; tears them down with `cargo run -- stop`. Replaces the old `mac-side.sh`/`stop.sh`. Shares no modules with the compositor. |
| `src/main.rs` | **`wayland-macos-core`** binary: compositor entry point — detects display scale + output size, starts AppKit, spawns the wayland thread. |
| `src/wayland/mod.rs` | Compositor **engine**: `State` + its data model, buffer `present`, `handle_commit`, `process_input`, client-disconnect reaping, globals registration, and `run()` (poll loop). |
| `src/wayland/*.rs` | One file per protocol family — the `Dispatch`/`GlobalDispatch` impls (see below). |
| `src/mac.rs` | AppKit side: `WaylandView`/`WaylandWindow`/`WinDelegate`, `WinCmd` handling, drag/resize, cursors, keyboard-layout detection. |
| `src/input.rs` | Cross-thread `InputBus`, `InputEvent`, and the shared `scale` / `output_size` atomics. |
| `src/wayland/clipboard.rs` | Native macOS integration (the clipboard ↔ `wl_data_device`; its state lives on `State.clipboard`). |
| `src/router.rs` | `--multiplex` mode (compositor side): spawns one *window-host* child per Wayland app and routes each `WinCmd` to the host owning the target window; feeds hosts' input back into the local `InputBus`. |
| `src/host.rs` | `--multiplex` mode (child side): `--window-host <sock>` entry — its own `NSApplication` (its own Dock tile / Cmd-Tab entry) running the normal AppKit `handle()` path over a socket. |
| `src/ipc.rs` | Wire format for the two above: length-prefixed `bincode` frames (`Downlink`/`Uplink`) over a `UnixStream`. |
| `src/bin/testclient.rs` | Standalone `wayland-client` test client. |
| `build.rs` | Points the linker at Homebrew's libxkbcommon. |
| `docker/` | Container images + `entrypoint.sh` (see Containers). |
| `scripts/` | `run.sh` (bring up the Mac side + launch a container app), `pulseaudio-mac.sh`/`.pa` (audio bridge), `build-waypipe.sh`, `build-msfreerdp.sh` (rail), `e2e-resize-kwin.sh`. Bring-up/teardown itself is `cargo run` / `cargo run -- stop`. |

### `src/wayland/` module tree

`mod.rs` holds `State` and the engine; every protocol lives in its own file and
is wired in via `mod <name>;` (top of `mod.rs`) + a `create_global` line in
`run()`. Submodules start with `use super::*;`, which pulls in `State`, the shared
records, and all protocol/`wayland_server` imports.

| Module | Protocols |
|--------|-----------|
| `compositor.rs` | `wl_compositor`, `wl_surface`, `wl_region`, `wl_callback`, `wl_subcompositor`, `wl_subsurface` (composited as CALayer sublayers — KWin renders its output into a subsurface) |
| `shm.rs` | `wl_shm`, `wl_shm_pool`, `wl_buffer` (+ `PoolMem`/`BufferKind` live in `mod.rs`) |
| `xdg_shell.rs` | `xdg_wm_base`, `xdg_surface`, `xdg_toplevel`, `xdg_popup`, `xdg_positioner` |
| `output.rs` | `wl_output` |
| `seat.rs` | `wl_seat`, `wl_pointer`, `wl_keyboard`, `wl_touch` |
| `decoration.rs` | `zxdg_decoration_manager_v1` (forces server-side decorations) |
| `cursor_shape.rs` | `wp_cursor_shape_v1` |
| `single_pixel.rs` | `wp_single_pixel_buffer_v1` |
| `viewporter.rs` | `wp_viewporter` |
| `pointer_constraints.rs` | `zwp_pointer_constraints_v1` |
| `relative_pointer.rs` | `zwp_relative_pointer_v1` |
| `presentation.rs` | `wp_presentation` (present-timing feedback) |
| `xdg_output.rs` | `zxdg_output_manager_v1` (logical output geometry) |
| `xdg_activation.rs` | `xdg_activation_v1` (focus/raise a surface → `WinCmd::Activate`) |
| `fractional_scale.rs` | `wp_fractional_scale_v1` (reports `scale*120` for crisp non-integer scaling) |
| `xdg_dialog.rs` | `xdg_wm_dialog_v1` (modal dialogs → raised NSWindow level via `WinCmd::SetModal`) |
| `keyboard_shortcuts_inhibit.rs` | `zwp_keyboard_shortcuts_inhibit_v1` (grants the inhibitor; see file note on scope) |
| `primary_selection.rs` | `zwp_primary_selection_v1` (X11-style middle-click paste, client-to-client only) |
| `layer_shell.rs` | `zwlr_layer_shell_v1` (docked bars/panels + launchers → borderless floating NSWindow anchored to a screen edge; a keyboard-interactive surface like fuzzel is made key; see `create_layer_window`) |
| `color_management.rs` | `wp_color_manager_v1` (HDR: PQ/HLG/sRGB transfer + BT.2020/Display-P3/sRGB primaries → a `ColorDesc` staged on the surface; see HDR below) |
| `xdg_toplevel_icon.rs` | `xdg_toplevel_icon_v1` (per-toplevel icons; a buffer icon → `WinCmd::SetIcon` → the app's Dock icon in --multiplex mode; name-only icons are ignored — no icon set) |

## Adding a new Wayland protocol

1. Create `src/wayland/<name>.rs`, starting with `use super::*;`.
2. Add `impl GlobalDispatch<TheManager, ()> for State { fn bind … }` and a
   `Dispatch` impl for each object the protocol creates. Keep unused requests as
   a `_ => {}` arm; emit a `WinCmd` (or mutate `State`) for the ones you handle.
3. In `src/wayland/mod.rs`: add `mod <name>;` near the other `mod` lines, a
   `const <NAME>_VERSION: u32 = …;`, and a
   `dh.create_global::<State, TheManager, _>(<NAME>_VERSION, ());` inside `run()`.
4. Add the protocol's `use wayland_protocols::…` import at the top of `mod.rs`
   (submodules inherit it via `use super::*;`).
5. `cargo build`. Discovering a client's required protocols is often iterative —
   e.g. KWin logs `"<name> isn't supported by the host compositor"` and exits;
   add that global and retry.

Buffers that aren't plain shm (e.g. single-pixel) extend the `BufferKind` enum in
`mod.rs`; `present()` matches on it to produce BGRA pixels.

## Build & run

```bash
# macOS side: builds + starts pulseaudio, the compositor, waypipe client, and TCP bridge.
cargo run                      # the wayland-macos orchestrator CLI
cargo run -- stop              # tear it down
# Compositor alone (no waypipe/bridge, e.g. for the test client):
cargo run --bin wayland-macos-core

# Container side (GNOME app, default):
docker compose up wayland-app            # APP env picks the program (default: kgx)

# Container side (KDE Plasma / KWin nested, gated behind the "kde" profile):
docker compose --profile kde up wayland-kde

# Standalone docked bar (waybar) via wlr-layer-shell:
docker compose --profile bar up wayland-bar

# Minimal "shell": docked bar (waybar) + app launcher (fuzzel). The bar has an
# "☰ Apps" button that re-summons fuzzel; type an app name to launch it as its
# own native window.
docker compose --profile shell up wayland-shell

# Firefox (GTK/Wayland) as a native macOS window:
docker compose --profile firefox up wayland-firefox
```

Firefox notes (`wayland-firefox` service): `MOZ_ENABLE_WAYLAND=1` (native Wayland,
not XWayland); `MOZ_DISABLE_CONTENT_SANDBOX=1` (Docker's default seccomp blocks the
content-sandbox syscalls, else tabs crash); software WebRender via the image's
`LIBGL_ALWAYS_SOFTWARE=1` (compositor is shm-only, no GPU); and `shm_size: 2gb`
(Firefox crashes with Docker's default 64MB `/dev/shm`). The `Sandbox … EPERM` and
`glxtest: libpci missing` log lines are expected and non-fatal.

Restarting the compositor drops existing client connections; relaunch the
container afterwards.

## Audio

waypipe forwards only Wayland — **not** audio — so audio takes a separate,
out-of-band channel that never touches the compositor: a PulseAudio daemon on the
Mac (`scripts/pulseaudio-mac.pa`, started by `scripts/pulseaudio-mac.sh` which the
`wayland-macos` CLI invokes) exposes CoreAudio as Pulse sinks/sources and listens
on TCP 4713; the container's `entrypoint.sh` sets
`PULSE_SERVER=tcp:${MAC_HOST}:4713` so libpulse clients stream straight to it. The
Dockerfiles ship the PulseAudio client libs (`pulseaudio-utils`; the Debian and
RAIL images add `gstreamer1.0-pulseaudio` for Qt/GTK media). It's optional and
degrades cleanly: no `pulseaudio` on the Mac (`brew install pulseaudio`) →
`scripts/pulseaudio-mac.sh` prints a hint and apps start muted; nothing else
breaks. This is not a native macOS integration (those live on the Wayland socket)
— it bypasses waypipe entirely.

The **RAIL back-end reuses this same channel** — RDP/RAIL carries no audio to a
usable macOS sink (the Mac-side FreeRDP is built `-DWITH_PULSE=OFF
-DWITH_ALSA=OFF`), so `docker/entrypoint-rail.sh` exports the same `PULSE_SERVER`
and the RAIL container streams container→Mac even though it's the RDP *server*.
The daemon-start logic lives in `scripts/pulseaudio-mac.sh` — the CLI calls it for
both back-ends (`cargo run` and `cargo run -- --use-microsoft-rail-protocol`), and
you can run it directly. It probes TCP 4713 to decide whether to start (not
`pulseaudio --check`, which is true for any daemon even one without the CoreAudio
module / TCP listener).

## Containers

- `docker/Dockerfile` — Alpine + GNOME (newest GTK). Default for `wayland-app`.
- `docker/Dockerfile.kde-debian` — **Debian sid** + KDE Plasma 6 / Qt 6, a
  **working nested KWin session** (`docker compose --profile kde up wayland-kde`).
  What it took to make Plasma 6 render:
  - Debian's glibc provides `renameat2` (Alpine's musl does not — that blocked Qt6).
  - KWin runs nested with the QPainter software compositor (`KWIN_COMPOSE=Q`, no GPU)
    at `--scale 2` for a crisp Retina result.
  - The compositor implements the protocols KWin 6 requires (`wp_single_pixel_buffer`,
    `wp_viewporter` incl. **destination sizing**, `zwp_pointer_constraints`,
    `zwp_relative_pointer`, `wp_presentation`) and **composites KWin's output
    subsurface** (KWin renders its whole output into a subsurface of a 1×1 toplevel).
  - The compositor **paces frame callbacks + presentation feedback to a ~16ms
    vblank** (see `vblank_tick`); KWin's nested render loop needs a real cadence or
    its output never stabilises (black window). This is what made rendering reliable.
  - `zxdg_output_v1` reports logical output geometry (plasmashell needs it, else
    "requesting unexisting screen geometry -1").
  - `KWIN_FORCE_SW_CURSOR` is deliberately **not** set — with it KWin draws the
    cursor into its buffer but only repaints on demand, so the cursor vanishes;
    left unset, KWin's cursor is a subsurface we composite as a tracking sublayer.
  - A bundled KWin script (`kwin-showwindows`, enabled via `/etc/xdg/kwinrc`) +
    focus-follows-mouse activate windows so KWin composites them.
  - The full session (`… plasmashell`) renders the desktop + panel; a single app
    (`… konsole`) works too. `startplasma-wayland` does NOT (needs systemd --user).
  - Compositor-side, `wl_pointer.motion` must be preceded by `wl_pointer.enter`
    (see `ensure_pointer_focus`) or KWin's nested backend segfaults.

  `docker/Dockerfile.kde` is the older Alpine attempt, kept for reference (musl
  blocks Qt6).
- `docker/entrypoint.sh` — starts system D-Bus, the `socat` TCP↔unix bridge, and
  `waypipe` as a **named-socket display server** (so the app *and* its D-Bus-
  activated helpers can all connect), then launches `$APP`. Tears the container
  down when any child exits.

## Conventions

- **Code docs, comments, and commit messages are in English.**
- Logging: use the `log` facade (`info!`/`warn!`/`error!`/`debug!`), never
  `println!`/`eprintln!`. Pass `target:` to name the subsystem — the targets mirror
  the old `[wl]`/`[mac]`/`[rail]`/`[router]`/`[host]`/`[clipboard]`/`[primary]`/
  `[client]` prefixes, e.g. `info!(target: "wl", "…")`. Both binaries init
  `env_logger` in `main()` (default level `info`); filter at runtime with
  `RUST_LOG`, e.g. `RUST_LOG=wl=debug,mac=info`. Levels: `error!` for failures,
  `warn!` for recoverable/unexpected conditions, `info!` for lifecycle/status,
  `debug!` for chatty per-event traces.
  The line format is `[timestamp LEVEL target/thread] message` — the **thread
  name** is included because the thread is the key axis here (main/AppKit ↔
  `wayland`/`rail` ↔ per-app host threads). Spawn threads via
  `thread::Builder::new().name(…)` so they show up named rather than as `?`
  (`wayland`, `rail`, `rail-rdp`, `host-reader`, `host-uplink`, `router-uplink`,
  `clip-read`, `clip-write`).
- HiDPI: `src/main.rs` detects the backing scale and screen size once; the
  compositor advertises them via `wl_output`, and `present_frame`/`create_window`
  in `mac.rs` convert physical buffer pixels ↔ logical points. Clients must render
  at the advertised scale (KWin needs `--scale 2` passed explicitly).
- HDR: a client declares its content's color via `wp_color_manager_v1`
  (`color_management.rs`) — a parametric image description (PQ/HLG/sRGB transfer +
  BT.2020/Display-P3/sRGB primaries) is resolved to a `ColorDesc` (in `mac.rs`,
  double-buffered onto the surface and applied on commit), then carried through
  `WinCmd::Frame`/`SubFrame` alongside the pixel `PixelFormat`. `shm.rs` advertises
  and `buffer_to_pixels` decodes the high-bit-depth formats without truncating:
  10-bit `xrgb2101010`/`argb2101010` and float16 `abgr16161616f` (the common RGB
  HDR shm layouts; other byte orders are not advertised). `make_cgimage` tags the
  frame with the matching `CGColorSpace` (`kCGColorSpaceITUR_2100_PQ`,
  extended-linear P3/2020, Display P3, …) and `present_frame` opts the layer into
  Extended Dynamic Range (`setWantsExtendedDynamicRangeContent`) for PQ/HLG
  content — so highlights use the display's EDR headroom. Two constraints: Core
  Graphics can't ingest a 10-bit RGB `CGImage`, so 10-bit is expanded to 16-bit
  RGBA first (`expand_2101010_to_rgba16`); and being **shm-only/software** (no
  dmabuf/GPU import), HDR only arrives from clients that render high-bit-depth into
  shm — no tone-mapping is done, the OS composites via the tagged color space.
  Exercise it end-to-end with `WLMAC_HDR=1 cargo run --bin testclient` (a 10-bit PQ
  ramp); the log shows `[mac] EDR enabled; display headroom …` on an EDR display.
- Popups/menus (`xdg_popup`): the client positions them client-side from the
  screen geometry, so a Qt client with **no valid `QScreen`** anchors every menu
  at `(0,0)`. Two things guarantee a valid screen: `zxdg_output_v1` at v3 applies
  its logical geometry only on the *next* `wl_output.done()`, so `xdg_output.rs`
  sends a fresh `wl_output.done()` after the xdg-output events; and the socat
  bridges use `nodelay` (below) so the output events aren't Nagle-stalled past the
  client's startup roundtrip. A popup that requests a grab but never paints must
  still clear the mac-side pointer grab (`reap_popup` always sends `Destroy`;
  `grabbed()` self-heals a stale grab) — otherwise input is swallowed forever.
  Button releases honor the **implicit pointer grab** (`IMPLICIT_GRAB` in `mac.rs`):
  a release goes to the surface that received the *press*, not the popup grab.
  Without it, the click that opens a menu also selects whatever item the popup maps
  under (newly visible since popups map in ~2ms after the nodelay fix).
  Popups are **clamped onto the screen** (`create_popup` — the `xdg_positioner`
  "slide" adjustment): a menu that would fall off an edge slides back into the
  visible frame, so it isn't clipped and its click-outside dismiss hit-test still
  works. (Full flip/resize adjustment isn't implemented.) Note: many app menus are
  NOT Wayland popups — e.g. Firefox's hamburger "AppMenu" is drawn in-content in the
  toplevel's own buffer, so it has no `xdg_popup`/grab and closes via a click the
  client receives, or on focus-out (`windowDidResignKey` → `Focus{false}`).
- Transport: the `socat` TCP bridges (the `wayland-macos` CLI, `docker/entrypoint.sh`)
  set `nodelay` (TCP_NODELAY). The Wayland wire is chatty request/reply; without it
  Nagle stalls small replies by hundreds of ms.
- Interactive resize: the drag asks the *client* to repaint at the new size
  (`xdg_toplevel.configure`); the `NSWindow` follows the buffer that comes back so
  content never stretches (`try_resize_window` + `present_frame` in `mac.rs`).
- CSD shadow margins: a client that draws its own decorations (GTK, Firefox — they
  never call `get_toplevel_decoration`) pads its buffer with a transparent shadow
  and reports the real content bounds via `xdg_surface.set_window_geometry`. We
  keep the `NSWindow` sized to the whole buffer (so nothing is clipped), but a
  resize `configure` asks for `window size − margin` (`csd_margin` on `WinEntry`),
  where `margin = buffer − geometry`. Without this the client returns a buffer that
  is `window + margin`, which `present_frame` would grow the window to — a runaway
  resize loop (the window balloons a little more each round-trip).
- Docked-bar reserved space: a layer-shell bar's exclusive zone becomes
  work-area insets (`input::reserved_insets`). Our *own* windows honor it —
  maximize/fullscreen-restore (`set_window_fill`), initial placement
  (`place_in_work_area`), and drag/zoom via a `constrainFrameRect:toScreen:`
  override on `WaylandWindow` (the same hook macOS uses for the menu bar;
  fullscreen windows are exempt). macOS gives no public API to reserve space
  globally for *other* apps — only the menu bar/Dock (WindowServer) can.
- Multi-monitor: the compositor is single-output. `main.rs` derives scale from
  the max backing factor across screens and output size + the bar's screen from
  `mainScreen` (the active display), so on a mixed setup the bar tracks the
  monitor you're using. `wl_output` is advertised at v4 (name/description) —
  fuzzel and other layer-shell clients require ≥ v3.

## Testing

Tests live in-crate as `#[cfg(test)] mod tests` (there is no `[lib]` target, so
`tests/` can't reach internals). Run:

```bash
cargo test --bin wayland-macos
```

The intended integration pattern drives a real in-process client↔server pair over
`UnixStream::pair()` and asserts on the emitted `WinCmd`s — capture them by routing
`mac::post` to a channel instead of the AppKit main queue. This keeps tests
headless (no `NSApplication`, no GPU, no display), which CI on macOS requires.
