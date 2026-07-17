/* C bridge between FreeRDP's RAIL (RemoteApp) client and the Rust side.
 *
 * The Rust `rail` back-end provides a set of callbacks; this bridge runs the
 * FreeRDP event loop, negotiates a RemoteApp/RAIL session with the Weston-RDP
 * container, and turns RAIL window orders + surface updates into those
 * callbacks. Rust then maps them onto WinCmd / NSWindow.
 *
 * Everything here is the counterpart to `src/wayland/` for the
 * `--use-microsoft-rail-protocol` mode; see `src/rail.rs`.
 */
#ifndef RAIL_BRIDGE_H
#define RAIL_BRIDGE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Callbacks invoked from the FreeRDP event loop thread. `user` is passed back
 * verbatim (an opaque pointer Rust hands in at start). Pixel buffers are 32-bit
 * BGRA (byte order B,G,R,A) and are only valid for the duration of the call —
 * copy if you need to keep them. */
typedef struct {
    void *user;
    /* A RAIL window appeared. Geometry is in RDP desktop pixels. */
    void (*window_create)(void *user, uint32_t id, int32_t x, int32_t y,
                          uint32_t w, uint32_t h, const char *title);
    /* A RAIL window's geometry changed. */
    void (*window_update)(void *user, uint32_t id, int32_t x, int32_t y,
                          uint32_t w, uint32_t h);
    /* A RAIL window's title changed. */
    void (*window_title)(void *user, uint32_t id, const char *title);
    /* A RAIL window was destroyed. */
    void (*window_delete)(void *user, uint32_t id);
    /* New pixels for a window (its region blitted from the GDI primary surface).
     * `stride` is bytes per row. */
    void (*window_surface)(void *user, uint32_t id, uint32_t w, uint32_t h,
                           uint32_t stride, const uint8_t *pixels);
    /* The session ended (disconnected or failed to connect). */
    void (*disconnected)(void *user);
    /* The server asked the client to move the window locally — the user grabbed
     * the app's own titlebar (RAIL Server Local Move/Size, type MOVE). The Rust
     * side turns this into a native NSWindow drag. */
    void (*window_move_start)(void *user, uint32_t id);
    /* Emit a bridge log line through Rust's `log` facade so it shares the
     * compositor's format/filtering. `level` is a RAIL_LOG_* severity (see
     * rail_bridge.c); `msg` is a NUL-terminated, already-formatted string valid
     * only for the duration of the call. May be NULL if Rust wired no logger. */
    void (*log)(void *user, int level, const char *msg);
} rail_callbacks;

/* Severities for rail_callbacks.log — mirror Rust's log::Level ordering. */
#define RAIL_LOG_ERROR 0
#define RAIL_LOG_WARN  1
#define RAIL_LOG_INFO  2
#define RAIL_LOG_DEBUG 3

/* Connect to host:port, launch/attach the RemoteApp `app`, and run the FreeRDP
 * event loop until disconnect or rail_stop(). Blocking — call on a dedicated
 * thread. Returns 0 on a clean session, non-zero on connect failure. */
int rail_run(const char *host, int port, const char *app, uint32_t desktop_w,
             uint32_t desktop_h, uint32_t scale, const rail_callbacks *cb);

/* Forward a pointer event for a window. `local_x`/`local_y` are surface-local
 * pixels (top-left origin); the bridge adds the window's desktop offset.
 * `flags` is a FreeRDP PTR_FLAGS_* bitmask. Safe to call from another thread. */
void rail_send_pointer(uint32_t window_id, int32_t local_x, int32_t local_y,
                       uint16_t flags);

/* Forward a keyboard event. `scancode` is an RDP scancode; `down` is 1 for
 * press, 0 for release. Safe to call from another thread. */
void rail_send_key(uint16_t scancode, int down);

/* Ask the running session to disconnect (unblocks rail_run). */
void rail_stop(void);

#ifdef __cplusplus
}
#endif

#endif /* RAIL_BRIDGE_H */
