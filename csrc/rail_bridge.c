/* See rail_bridge.h. FreeRDP 3 RemoteApp/RAIL client, distilled to the receive
 * path (window orders + surface content via the software GDI) plus a minimal
 * input send path. Modeled on FreeRDP's sample clients. */
#include "rail_bridge.h"

#include <string.h>
#include <stdlib.h>

#include <freerdp/freerdp.h>
#include <freerdp/client.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/gdi/gfx.h>
#include <freerdp/channels/channels.h>
#include <freerdp/channels/rdpgfx.h>
#include <freerdp/client/rdpgfx.h> /* RdpgfxClientContext + UpdateSurfaceArea */
#include <freerdp/window.h>
#include <freerdp/rail.h> /* RAIL_SVC_CHANNEL_NAME + RAIL_* order structs (2.x) */
#include <freerdp/client/rail.h>
#include <freerdp/client/cliprdr.h> /* CliprdrClientContext (set custom) */
#include <freerdp/channels/cliprdr.h> /* CLIPRDR_SVC_CHANNEL_NAME */
#include <freerdp/client/cmdline.h> /* freerdp_client_load_addins (2.x) */
#include <freerdp/codec/color.h>
#include <freerdp/input.h>

#define MAX_WINDOWS 256
#define MAX_SURFACES 256

typedef struct {
    uint32_t id;
    int32_t x, y;
    uint32_t w, h;
    int used;
} rail_window;

/* surfaceId -> windowId association (see rail_map_window_for_surface). */
typedef struct {
    uint16_t surface_id;
    uint32_t window_id;
    int used;
} surf_map;

/* Our rdpContext subclass. The base rdpContext MUST be the first member so
 * FreeRDP can cast between them. */
typedef struct {
    rdpContext context;
    rail_window windows[MAX_WINDOWS];
    surf_map surfaces[MAX_SURFACES];
} railContext;

/* Single active session. RAIL is inherently one connection here, and the input
 * send functions (called from Rust's input-drain thread) need to reach it. */
static railContext *g_ctx = NULL;
static rail_callbacks g_cb;
/* The most recent real (non-0x0) RAIL window. WSLg delivers window content as
 * rdpgfx surfaces that (in this build) arrive without a MapSurfaceToWindow, so
 * we associate unmapped content surfaces with this window. */
static uint32_t g_main_window_id = 0;

/* ---- window slot bookkeeping ------------------------------------------- */

static rail_window *win_find(railContext *rc, uint32_t id) {
    for (int i = 0; i < MAX_WINDOWS; i++)
        if (rc->windows[i].used && rc->windows[i].id == id)
            return &rc->windows[i];
    return NULL;
}

static rail_window *win_alloc(railContext *rc, uint32_t id) {
    rail_window *w = win_find(rc, id);
    if (w)
        return w;
    for (int i = 0; i < MAX_WINDOWS; i++) {
        if (!rc->windows[i].used) {
            memset(&rc->windows[i], 0, sizeof(rail_window));
            rc->windows[i].used = 1;
            rc->windows[i].id = id;
            return &rc->windows[i];
        }
    }
    return NULL;
}

/* ---- surface -> window mapping ----------------------------------------- */
//
// WSLg's weston-rdprail backend doesn't send the standard MS-RDPEGFX
// MapSurfaceToWindow PDU (so FreeRDP's gdi never sets surface->windowId).
// Instead it uses the WSLg fork's MapWindowForSurface / UnmapWindowForSurface
// callbacks to associate each rdpgfx surface with a RAIL window. We record that
// mapping here so multi-window sessions route each surface's content to the
// correct NSWindow (previously every surface fell back to the last-created
// window via g_main_window_id, so only one app ever rendered).

static void surf_map_set(railContext *rc, uint16_t surface_id, uint32_t window_id) {
    surf_map *free_slot = NULL;
    for (int i = 0; i < MAX_SURFACES; i++) {
        if (rc->surfaces[i].used && rc->surfaces[i].surface_id == surface_id) {
            rc->surfaces[i].window_id = window_id;
            return;
        }
        if (!free_slot && !rc->surfaces[i].used)
            free_slot = &rc->surfaces[i];
    }
    if (free_slot) {
        free_slot->used = 1;
        free_slot->surface_id = surface_id;
        free_slot->window_id = window_id;
    }
}

static uint32_t surf_map_lookup(railContext *rc, uint16_t surface_id) {
    for (int i = 0; i < MAX_SURFACES; i++)
        if (rc->surfaces[i].used && rc->surfaces[i].surface_id == surface_id)
            return rc->surfaces[i].window_id;
    return 0;
}

static void surf_map_clear_window(railContext *rc, uint32_t window_id) {
    for (int i = 0; i < MAX_SURFACES; i++)
        if (rc->surfaces[i].used && rc->surfaces[i].window_id == window_id)
            rc->surfaces[i].used = 0;
}

/* Convert a RAIL_UNICODE_STRING (UTF-16LE bytes) to a freshly-malloc'd UTF-8
 * string. FreeRDP 2.x has no rail_string_to_utf8_string helper; window titles
 * are almost always BMP, so a compact hand-rolled converter suffices (surrogate
 * pairs are not reconstructed). Caller frees. */
static char *rail_title_utf8(const RAIL_UNICODE_STRING *s) {
    if (!s || !s->string || s->length < 2)
        return NULL;
    size_t units = s->length / 2;
    char *out = (char *)malloc(units * 3 + 1);
    if (!out)
        return NULL;
    size_t o = 0;
    for (size_t i = 0; i < units; i++) {
        uint16_t c = (uint16_t)s->string[i * 2] |
                     ((uint16_t)s->string[i * 2 + 1] << 8);
        if (c == 0)
            break;
        if (c < 0x80) {
            out[o++] = (char)c;
        } else if (c < 0x800) {
            out[o++] = (char)(0xC0 | (c >> 6));
            out[o++] = (char)(0x80 | (c & 0x3F));
        } else {
            out[o++] = (char)(0xE0 | (c >> 12));
            out[o++] = (char)(0x80 | ((c >> 6) & 0x3F));
            out[o++] = (char)(0x80 | (c & 0x3F));
        }
    }
    out[o] = '\0';
    return out;
}

/* ---- RAIL window order callbacks --------------------------------------- */

static void apply_state(rail_window *w, const WINDOW_ORDER_INFO *oi,
                        const WINDOW_STATE_ORDER *ws) {
    if (oi->fieldFlags & WINDOW_ORDER_FIELD_WND_OFFSET) {
        w->x = ws->windowOffsetX;
        w->y = ws->windowOffsetY;
    }
    if (oi->fieldFlags & WINDOW_ORDER_FIELD_WND_SIZE) {
        w->w = ws->windowWidth;
        w->h = ws->windowHeight;
    }
}

static BOOL rail_window_create(rdpContext *context, const WINDOW_ORDER_INFO *oi,
                               const WINDOW_STATE_ORDER *ws) {
    railContext *rc = (railContext *)context;
    rail_window *w = win_alloc(rc, oi->windowId);
    if (!w)
        return TRUE;
    apply_state(w, oi, ws);
    if (w->w > 0 && w->h > 0)
        g_main_window_id = oi->windowId;

    char *title = NULL;
    if ((oi->fieldFlags & WINDOW_ORDER_FIELD_TITLE) && ws->titleInfo.length)
        title = rail_title_utf8(&ws->titleInfo);
    g_cb.window_create(g_cb.user, w->id, w->x, w->y, w->w, w->h,
                       title ? title : "");
    free(title);
    return TRUE;
}

static BOOL rail_window_update(rdpContext *context, const WINDOW_ORDER_INFO *oi,
                               const WINDOW_STATE_ORDER *ws) {
    railContext *rc = (railContext *)context;
    rail_window *w = win_find(rc, oi->windowId);
    if (!w) /* update for a window we never saw created — treat as create */
        return rail_window_create(context, oi, ws);
    apply_state(w, oi, ws);

    if (oi->fieldFlags & WINDOW_ORDER_FIELD_TITLE) {
        char *title = ws->titleInfo.length ? rail_title_utf8(&ws->titleInfo) : NULL;
        g_cb.window_title(g_cb.user, w->id, title ? title : "");
        free(title);
    }
    if (oi->fieldFlags &
        (WINDOW_ORDER_FIELD_WND_OFFSET | WINDOW_ORDER_FIELD_WND_SIZE))
        g_cb.window_update(g_cb.user, w->id, w->x, w->y, w->w, w->h);
    return TRUE;
}

static BOOL rail_window_delete(rdpContext *context,
                               const WINDOW_ORDER_INFO *oi) {
    railContext *rc = (railContext *)context;
    rail_window *w = win_find(rc, oi->windowId);
    if (w)
        w->used = 0;
    /* Drop any surface mappings for this window (defensive: the server should
     * also send UnmapWindowForSurface, but a stale mapping would misroute a
     * recycled surfaceId to a destroyed window). */
    surf_map_clear_window(rc, oi->windowId);
    g_cb.window_delete(g_cb.user, oi->windowId);
    return TRUE;
}

/* ---- painting ---------------------------------------------------------- */

/* In HiDef RAIL mode there is no desktop; each window's content is delivered as
 * an rdpgfx surface mapped to that window (MapSurfaceToWindow). FreeRDP's gdi
 * gfx renders those into per-surface buffers and, for window-mapped surfaces,
 * defers to a client UpdateSurfaceArea callback (see gdi_UpdateSurfaces). So we
 * hook that: on each surface update, hand the mapped window's surface bits to
 * Rust. (The old primary-buffer blit gave empty windows — the desktop buffer is
 * unused in RAIL.) */
static UINT rail_update_surface_area(RdpgfxClientContext *gfx, UINT16 surfaceId,
                                     UINT32 nrRects, const RECTANGLE_16 *rects) {
    (void)nrRects;
    (void)rects;
    gdiGfxSurface *surface = (gdiGfxSurface *)gfx->GetSurfaceData(gfx, surfaceId);
    if (!surface || !surface->data)
        return CHANNEL_RC_OK;
    /* Resolve the target window: the WSLg MapWindowForSurface mapping first,
     * then the surface's own windowId (standard MapSurfaceToWindow), and only
     * as a last resort the most-recent window (single-window fallback). */
    uint32_t win = g_ctx ? surf_map_lookup(g_ctx, surfaceId) : 0;
    if (win == 0)
        win = surface->windowId != 0 ? (uint32_t)surface->windowId
                                     : g_main_window_id;
    if (win == 0)
        return CHANNEL_RC_OK;
    /* Surfaces are BGRA32 (gdi initialised with PIXEL_FORMAT_BGRA32). The gfx
     * surface is the window rounded up to 16-px alignment (e.g. 806x491 -> a
     * 816x496 surface), so the extra rows/cols are padding, not content. Crop to
     * the RAIL window's logical size (top-left) so the window has no dead margin. */
    rail_window *rw = win_find(g_ctx, win);
    if (rw && rw->w > 0 && rw->h > 0 && (uint32_t)rw->w <= surface->width &&
        (uint32_t)rw->h <= surface->height &&
        ((uint32_t)rw->w != surface->width || (uint32_t)rw->h != surface->height)) {
        uint32_t cw = (uint32_t)rw->w, ch = (uint32_t)rw->h, cstride = cw * 4;
        uint8_t *buf = (uint8_t *)malloc((size_t)cstride * ch);
        if (buf) {
            for (uint32_t r = 0; r < ch; r++)
                memcpy(buf + (size_t)r * cstride,
                       surface->data + (size_t)r * surface->scanline, cstride);
            g_cb.window_surface(g_cb.user, win, cw, ch, cstride, buf);
            free(buf);
            return CHANNEL_RC_OK;
        }
    }
    g_cb.window_surface(g_cb.user, win, surface->width, surface->height,
                        surface->scanline, surface->data);
    return CHANNEL_RC_OK;
}

/* WSLg fork callbacks: the server associates an rdpgfx surface with a RAIL
 * window through these (not the standard MapSurfaceToWindow PDU). Record the
 * mapping so rail_update_surface_area routes each surface to the right window.
 * NOTE: the surface is already locked here — do no gfx calls, only bookkeeping. */
static UINT rail_map_window_for_surface(RdpgfxClientContext *gfx,
                                        UINT16 surfaceID, UINT64 windowID) {
    (void)gfx;
    fprintf(stderr, "[rail-c] map surface %u -> window %llu\n", surfaceID,
            (unsigned long long)windowID);
    if (g_ctx)
        surf_map_set(g_ctx, surfaceID, (uint32_t)windowID);
    return CHANNEL_RC_OK;
}

static UINT rail_unmap_window_for_surface(RdpgfxClientContext *gfx,
                                          UINT64 windowID) {
    (void)gfx;
    if (g_ctx)
        surf_map_clear_window(g_ctx, (uint32_t)windowID);
    return CHANNEL_RC_OK;
}

/* gdi's default EndPaint errors in RAIL mode (there is no desktop output to
 * flush); override it with a no-op so the session survives. Window content is
 * delivered via rail_update_surface_area, not this path. */
static BOOL rail_end_paint(rdpContext *context) {
    (void)context;
    return TRUE;
}

/* ---- RAIL handshake ---------------------------------------------------- */
//
// After the RDP session goes active the server sends a RAIL Handshake. The
// client must respond and then send its Client Information (status) — only then
// does the rdprail-shell start remoting window orders. FreeRDP's client-common
// clients (xfreerdp) do this; a bare context does not, so we wire it here.

/* The RemoteApp start sequence (Handshake -> Client Information -> Client
 * Execute). Driven once, from whichever fires first: the main-thread
 * ChannelConnected handler (proactive, client-initiated — the WSLg server can
 * sit waiting for the client to start the handshake) or the rail thread's
 * ServerHandshake(Ex) callback (reactive). A guard makes it idempotent so the
 * two paths don't double-send. */
static int g_rail_started = 0;
static RailClientContext *g_rail = NULL; /* set when the rail channel connects */

static UINT rail_send_client_status(RailClientContext *rail) {
    rdpContext *ctx = (rdpContext *)rail->custom;
    if (!ctx)
        return CHANNEL_RC_OK;
    if (__atomic_test_and_set(&g_rail_started, __ATOMIC_SEQ_CST))
        return CHANNEL_RC_OK; /* already started */

    /* Initiate the handshake client-first (idempotent with the server's own /
     * the channel's auto-reply). */
    RAIL_HANDSHAKE_ORDER hs;
    memset(&hs, 0, sizeof(hs));
    hs.buildNumber = 0x00001DB0;
    if (rail->ClientHandshake)
        rail->ClientHandshake(rail, &hs);

    RAIL_CLIENT_STATUS_ORDER cs;
    memset(&cs, 0, sizeof(cs));
    cs.flags = TS_RAIL_CLIENTSTATUS_ALLOWLOCALMOVESIZE |
               TS_RAIL_CLIENTSTATUS_ZORDER_SYNC |
               TS_RAIL_CLIENTSTATUS_WINDOW_RESIZE_MARGIN_SUPPORTED |
               TS_RAIL_CLIENTSTATUS_HIGH_DPI_ICONS_SUPPORTED;
    fprintf(stderr, "[rail-c] server handshake -> sending ClientInformation\n");
    UINT rc = rail->ClientInformation(rail, &cs);
    fprintf(stderr, "[rail-c] ClientInformation rc=%u\n", rc);
    if (rc != CHANNEL_RC_OK)
        return rc;

    /* Launch/associate the RemoteApp so the shell starts remoting its window.
     * Skip when the program is empty — the server returns an error ExecResult
     * that FreeRDP's rail channel fails to parse, dropping the session. */
    const char *program = freerdp_settings_get_string(
        ctx->settings, FreeRDP_RemoteApplicationProgram);
    if (!program || program[0] == '\0')
        return CHANNEL_RC_OK;
    RAIL_EXEC_ORDER exec;
    memset(&exec, 0, sizeof(exec));
    exec.RemoteApplicationProgram = (char *)program;
    rc = rail->ClientExecute(rail, &exec);
    fprintf(stderr, "[rail-c] ClientExecute program=%s rc=%u\n", program, rc);
    return rc;
}

static UINT rail_on_server_handshake(RailClientContext *rail,
                                     const RAIL_HANDSHAKE_ORDER *h) {
    (void)h;
    return rail_send_client_status(rail);
}

static UINT rail_on_server_handshake_ex(RailClientContext *rail,
                                        const RAIL_HANDSHAKE_EX_ORDER *h) {
    (void)h;
    return rail_send_client_status(rail);
}

/* ---- channel wiring (gfx -> gdi) --------------------------------------- */

static void on_channel_connected(void *context, ChannelConnectedEventArgs *e) {
    rdpContext *ctx = (rdpContext *)context;
    if (strcmp(e->name, RDPGFX_DVC_CHANNEL_NAME) == 0) {
        fprintf(stderr, "[rail-c] rdpgfx channel connected; wiring gdi pipeline\n");
        RdpgfxClientContext *gfx = (RdpgfxClientContext *)e->pInterface;
        gdi_graphics_pipeline_init(ctx->gdi, gfx);
        /* Take over per-window surface delivery (RAIL window content). Must be
         * set after gdi_graphics_pipeline_init, which installs the defaults. */
        gfx->UpdateSurfaceArea = rail_update_surface_area;
        /* WSLg surface<->window association (multi-window routing). */
        gfx->MapWindowForSurface = rail_map_window_for_surface;
        gfx->UnmapWindowForSurface = rail_unmap_window_for_surface;
    } else if (strcmp(e->name, CLIPRDR_SVC_CHANNEL_NAME) == 0) {
        /* We don't implement clipboard, but the addin is loaded (the WSLg server
         * expects it). Give it a non-NULL custom so its caps PDU doesn't error
         * ("context->custom not set" -> channel error -> session drop). */
        ((CliprdrClientContext *)e->pInterface)->custom = ctx;
    } else if (strcmp(e->name, RAIL_SVC_CHANNEL_NAME) == 0) {
        fprintf(stderr, "[rail-c] rail channel connected; starting RAIL\n");
        RailClientContext *rail = (RailClientContext *)e->pInterface;
        rail->custom = ctx;
        rail->ServerHandshake = rail_on_server_handshake;
        rail->ServerHandshakeEx = rail_on_server_handshake_ex;
        /* Publish for the main loop to proactively start RAIL after this handler
         * returns — sending PDUs from inside the channel-connected callback is
         * re-entrant within freerdp_check_event_handles and breaks the loop. */
        g_rail = rail;
    }
}

static void on_channel_disconnected(void *context, ChannelDisconnectedEventArgs *e) {
    rdpContext *ctx = (rdpContext *)context;
    if (strcmp(e->name, RDPGFX_DVC_CHANNEL_NAME) == 0)
        gdi_graphics_pipeline_uninit(ctx->gdi,
                                     (RdpgfxClientContext *)e->pInterface);
}

/* ---- instance lifecycle ------------------------------------------------ */

static DWORD verify_certificate_ex(freerdp *instance, const char *host,
                                   UINT16 port, const char *common_name,
                                   const char *subject, const char *issuer,
                                   const char *fingerprint, DWORD flags) {
    (void)instance; (void)host; (void)port; (void)common_name;
    (void)subject; (void)issuer; (void)fingerprint; (void)flags;
    return 2; /* accept permanently — this is a dev self-signed cert */
}

static BOOL rail_pre_connect(freerdp *instance) {
    rdpContext *ctx = instance->context;
    PubSub_SubscribeChannelConnected(ctx->pubSub, on_channel_connected);
    PubSub_SubscribeChannelDisconnected(ctx->pubSub, on_channel_disconnected);
    if (!freerdp_client_load_addins(ctx->channels, ctx->settings))
        return FALSE;
    return TRUE;
}

static BOOL rail_post_connect(freerdp *instance) {
    rdpContext *ctx = instance->context;
    if (!gdi_init(instance, PIXEL_FORMAT_BGRA32))
        return FALSE;

    rdpUpdate *update = ctx->update;
    update->EndPaint = rail_end_paint;
    update->window->WindowCreate = rail_window_create;
    update->window->WindowUpdate = rail_window_update;
    update->window->WindowDelete = rail_window_delete;
    return TRUE;
}

static void rail_post_disconnect(freerdp *instance) {
    gdi_free(instance);
}

static BOOL rail_client_new(freerdp *instance, rdpContext *context) {
    (void)context;
    instance->PreConnect = rail_pre_connect;
    instance->PostConnect = rail_post_connect;
    instance->PostDisconnect = rail_post_disconnect;
    instance->VerifyCertificateEx = verify_certificate_ex;
    return TRUE;
}

static void rail_client_free(freerdp *instance, rdpContext *context) {
    (void)instance; (void)context;
}

/* ---- public API -------------------------------------------------------- */

int rail_run(const char *host, int port, const char *app, uint32_t desktop_w,
             uint32_t desktop_h, uint32_t scale, const rail_callbacks *cb) {
    setvbuf(stderr, NULL, _IONBF, 0); /* unbuffered so diagnostics flush live */
    g_rail_started = 0;
    g_rail = NULL;
    g_main_window_id = 0;
    g_cb = *cb;

    RDP_CLIENT_ENTRY_POINTS ep;
    memset(&ep, 0, sizeof(ep));
    ep.Version = RDP_CLIENT_INTERFACE_VERSION;
    ep.Size = sizeof(ep);
    ep.ContextSize = sizeof(railContext);
    ep.ClientNew = rail_client_new;
    ep.ClientFree = rail_client_free;

    rdpContext *ctx = freerdp_client_context_new(&ep);
    if (!ctx)
        return -1;
    g_ctx = (railContext *)ctx;
    freerdp *instance = ctx->instance;
    rdpSettings *s = ctx->settings;

    freerdp_settings_set_string(s, FreeRDP_ServerHostname, host);
    freerdp_settings_set_uint32(s, FreeRDP_ServerPort, (UINT32)port);
    /* Weston's RDP backend uses TLS security without real authentication, but
     * FreeRDP still wants credentials present so it doesn't block on a prompt. */
    freerdp_settings_set_string(s, FreeRDP_Username, "user");
    freerdp_settings_set_string(s, FreeRDP_Password, "pass");
    /* Diagnostic: RAIL_NO_REMOTEAPP=1 connects as a plain desktop session
     * instead of RemoteApp/RAIL, to isolate whether a failure is RAIL-specific. */
    BOOL remoteapp = getenv("RAIL_NO_REMOTEAPP") == NULL;
    freerdp_settings_set_bool(s, FreeRDP_RemoteApplicationMode, remoteapp);
    if (remoteapp) {
        freerdp_settings_set_string(s, FreeRDP_RemoteApplicationProgram, app);
        /* WSLg's weston-rdprail backend REQUIRES HiDef RAIL — it rejects plain
         * ("cookie-cutter") RemoteApp at activation (rdp.c: "HiDef-RAIL is
         * required for RAIL"). This flag makes FreeRDP advertise
         * INFO_HIDEF_RAIL_SUPPORTED, which the server needs to enable window
         * remoting. (WSLg-fork-specific setting.) */
        freerdp_settings_set_bool(s, FreeRDP_HiDefRemoteApp, TRUE);
        /* Advertise the full RAIL support level in the RemoteApp capability set.
         * HANDSHAKE_EX is what makes the server send a HandshakeEx (the HiDef
         * handshake) that our ServerHandshakeEx callback drives. Without a level
         * set, the RAIL negotiation is incomplete and no handshake arrives. */
        freerdp_settings_set_uint32(
            s, FreeRDP_RemoteApplicationSupportLevel,
            RAIL_LEVEL_SUPPORTED | RAIL_LEVEL_DOCKED_LANGBAR_SUPPORTED |
                RAIL_LEVEL_SHELL_INTEGRATION_SUPPORTED |
                RAIL_LEVEL_LANGUAGE_IME_SYNC_SUPPORTED |
                RAIL_LEVEL_SERVER_TO_CLIENT_IME_SYNC_SUPPORTED |
                RAIL_LEVEL_HIDE_MINIMIZED_APPS_SUPPORTED |
                RAIL_LEVEL_WINDOW_CLOAKING_SUPPORTED |
                RAIL_LEVEL_HANDSHAKE_EX_SUPPORTED);
        /* Advertise the Window List capability level. Without this the client
         * defaults to WINDOW_LEVEL_NOT_SUPPORTED and window_order_supported()
         * REJECTS every window order carrying CLIENT_AREA_SIZE/RP_CONTENT/
         * ROOT_PARENT (which weston's do) -> "Windowing failed". EX (0x02)
         * accepts all window/notify/desktop orders. */
        freerdp_settings_set_uint32(s, FreeRDP_RemoteWndSupportLevel,
                                    0x00000002 /* WINDOW_LEVEL_SUPPORTED_EX */);
        /* The negotiated window support level ends up as 3 (SUPPORTED|EX), which
         * window_order_supported()'s switch doesn't match -> it falls through to
         * returning AllowUnanouncedOrdersFromServer. Set that TRUE so window/
         * notify/desktop orders are accepted regardless of the level value. */
        freerdp_settings_set_bool(s, FreeRDP_AllowUnanouncedOrdersFromServer, TRUE);
    }
    freerdp_settings_set_bool(s, FreeRDP_TlsSecurity, TRUE);
    freerdp_settings_set_bool(s, FreeRDP_NlaSecurity, FALSE);
    freerdp_settings_set_bool(s, FreeRDP_RdpSecurity, FALSE);
    freerdp_settings_set_bool(s, FreeRDP_UseRdpSecurityLayer, FALSE);
    freerdp_settings_set_bool(s, FreeRDP_IgnoreCertificate, TRUE);
    /* Required: WSLg's HiDef RAIL rejects activation without the graphics
     * pipeline (rdpgfx) — it delivers all window content over it. */
    freerdp_settings_set_bool(s, FreeRDP_SupportGraphicsPipeline, TRUE);
    freerdp_settings_set_bool(s, FreeRDP_SupportDynamicChannels, TRUE); /* rdpgfx is a DVC */
    freerdp_settings_set_bool(s, FreeRDP_SoftwareGdi, TRUE);
    /* Disable bulk compression: if the server compresses the fastpath order
     * stream, any decompression mismatch desyncs window-order parsing
     * ("Windowing failed"). Uncompressed is simpler and this is a local link. */
    freerdp_settings_set_bool(s, FreeRDP_CompressionEnabled, FALSE);
    /* NOTE: do NOT disable clipboard/audio/device — the WSLg server expects a
     * full-featured client and its teardown crashes (rdpaudio assertion) if the
     * client drops them. Instead the cliprdr channel is given a valid `custom`
     * pointer in on_channel_connected so its caps PDU doesn't error. */
    freerdp_settings_set_uint32(s, FreeRDP_ColorDepth, 32);
    /* Advertise the Mac's real display: physical pixel size + HiDPI scale. Weston
     * (WESTON_RDP_HI_DPI_SCALING defaults on) uses DesktopScaleFactor to render
     * apps at that scale, so a 2x display gets crisp 2x surfaces shown 1:1. */
    freerdp_settings_set_uint32(s, FreeRDP_DesktopWidth, desktop_w > 0 ? desktop_w : 1920);
    freerdp_settings_set_uint32(s, FreeRDP_DesktopHeight, desktop_h > 0 ? desktop_h : 1080);
    if (scale >= 1) {
        freerdp_settings_set_uint32(s, FreeRDP_DesktopScaleFactor, scale * 100);
        freerdp_settings_set_uint32(s, FreeRDP_DeviceScaleFactor, 100);
    }

    if (!freerdp_connect(instance)) {
        g_cb.disconnected(g_cb.user);
        freerdp_client_context_free(ctx);
        g_ctx = NULL;
        return -2;
    }

    int exit_reason = 0; /* 1=shall_disconnect 2=count0 3=wait_failed 4=check_failed */
    while (!freerdp_shall_disconnect(instance)) {
        HANDLE handles[64];
        DWORD count = freerdp_get_event_handles(ctx, handles, 64);
        if (count == 0) {
            exit_reason = 2;
            break;
        }
        /* Short timeout (not INFINITE): the RAIL start below is client-initiated,
         * so the loop must keep turning even when the server sends nothing. */
        if (WaitForMultipleObjects(count, handles, FALSE, 50) == WAIT_FAILED) {
            exit_reason = 3;
            break;
        }
        if (!freerdp_check_event_handles(ctx)) {
            exit_reason = 4;
            break;
        }
        /* Kick off the RAIL start sequence once the channel is up — from the
         * main loop (not the re-entrant channel-connected callback). */
        if (g_rail)
            rail_send_client_status(g_rail);
    }
    if (exit_reason == 0)
        exit_reason = 1;
    fprintf(stderr, "[rail-c] event loop exit: reason=%d last_error=0x%08x\n",
            exit_reason, freerdp_get_last_error(ctx));

    freerdp_disconnect(instance);
    g_cb.disconnected(g_cb.user);
    freerdp_client_context_free(ctx);
    g_ctx = NULL;
    return 0;
}

void rail_send_pointer(uint32_t window_id, int32_t local_x, int32_t local_y,
                       uint16_t flags) {
    railContext *rc = g_ctx;
    if (!rc)
        return;
    rail_window *w = win_find(rc, window_id);
    int32_t x = local_x + (w ? w->x : 0);
    int32_t y = local_y + (w ? w->y : 0);
    if (x < 0)
        x = 0;
    if (y < 0)
        y = 0;
    freerdp_input_send_mouse_event(rc->context.input, flags, (UINT16)x,
                                   (UINT16)y);
}

void rail_send_key(uint16_t scancode, int down) {
    railContext *rc = g_ctx;
    if (!rc)
        return;
    freerdp_input_send_keyboard_event_ex(rc->context.input, down ? TRUE : FALSE,
                                         scancode);
}

void rail_stop(void) {
    if (g_ctx)
        freerdp_abort_connect(g_ctx->context.instance);
}
