/*
 * Eclipse's side of nvkms-kapi: what kernel-open/nvidia-drm does with it in
 * Linux, reduced to what the Rust driver needs (src/nvkms.rs). Linux's
 * nvidia-drm calls the kapi function table from C, through structs too big
 * and too bitfield-heavy to mirror in Rust, so the calls are made here and
 * Rust gets back small flat structs.
 *
 * PR 3 of docs/README-nvkms-port.md: allocate an nvkms device per GPU and
 * report what nvkms sees on it -- heads, connectors, displays, EDID and
 * modes. That much changes nothing on the screen: nvkms keeps scanning out
 * whatever the firmware left until a modeset is applied.
 *
 * PR 4: the first modeset, the one nvidia-drm's first atomic commit makes,
 * with the firmware console as the primary surface (eclipse_kms_show_console).
 */

#include "nvUnixVersion.h"
#include "nvidia-modeset-os-interface.h"
#include "nvkms-rmapi.h"
#include "nvkms-kapi.h"
#include "nvkms-kapi-private.h"
#include "nvkms-kapi-internal.h"
#include "nvkms-types.h"          /* nvEvoGlobal, NVDevEvoRec: the default LUT */
#include <class/cl0076.h> /* NV01_MEMORY_FRAMEBUFFER_CONSOLE */

#define ECLIPSE_KMS_MAX_DISPLAYS 8
#define ECLIPSE_KMS_EDID_BYTES   128
#define ECLIPSE_KMS_MAX_MODES    256

/* Mirrored by `KmsDisplay` in src/nvkms.rs. */
struct EclipseKmsDisplay {
    NvU32 handle;
    NvU32 connector;
    NvU32 connectorType;   /* NvKmsConnectorType */
    NvU32 signalFormat;    /* NvKmsConnectorSignalFormat */
    NvU32 physicalIndex;
    NvU32 headMask;
    NvU32 numModes;        /* modes nvkms validated for this display */
    NvU32 preferredWidth;
    NvU32 preferredHeight;
    NvU32 preferredRefreshMilliHz;
    NvU32 preferredPixelClockKHz;
    NvU32 edidSize;        /* the whole EDID nvkms read, all extensions */
    NvU8  connected;
    NvU8  internal;
    NvU8  isDpMST;
    NvU8  vrrSupported;
    NvU8  edid[ECLIPSE_KMS_EDID_BYTES]; /* its base block */
};

/* Mirrored by `KmsInfo` in src/nvkms.rs. */
struct EclipseKmsInfo {
    NvU32 numHeads;
    NvU32 numConnectors;
    NvU32 numDisplays;     /* as nvkms counted them; at most 8 described */
    NvU32 maxWidth;
    NvU32 maxHeight;
    NvU32 maxCursor;
    NvU32 pitchAlignment;
    NvU32 hasVideoMemory;
    NvU64 vtFbBase;        /* the console nvkms imported, 0 for none */
    NvU64 vtFbSize;
    struct EclipseKmsDisplay displays[ECLIPSE_KMS_MAX_DISPLAYS];
};

_Static_assert(sizeof(struct EclipseKmsDisplay) == 4 * 12 + 4 + ECLIPSE_KMS_EDID_BYTES,
               "EclipseKmsDisplay layout is shared with src/nvkms.rs");
_Static_assert(sizeof(struct EclipseKmsInfo) ==
                   8 * 4 + 2 * 8 + ECLIPSE_KMS_MAX_DISPLAYS * sizeof(struct EclipseKmsDisplay),
               "EclipseKmsInfo layout is shared with src/nvkms.rs");

/* nvidia-drm's nvKms: the table, fetched once (nv_drm_init). */
static struct NvKmsKapiFunctionsTable g_nvKms;
static NvBool g_nvKmsValid;

static void eclipse_kms_logf(int level, const char *fmt, ...)
{
    char buf[256];
    va_list ap;

    va_start(ap, fmt);
    nvkms_vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    nvkms_log(level, "[nvkms-drv] ", buf);
}

static NvBool eclipse_kms_table(void)
{
    if (!g_nvKmsValid) {
        g_nvKms.versionString = NV_VERSION_STRING;
        g_nvKmsValid = nvKmsKapiGetFunctionsTable(&g_nvKms);
    }
    return g_nvKmsValid;
}

/*
 * nv_drm_event_callback, minus the DRM side that has nothing to tell yet:
 * every event kapi delivers is logged, so a hotplug or a flip shows up.
 */
static void eclipse_kms_event(const struct NvKmsKapiEvent *event)
{
    switch (event->type) {
    case NVKMS_EVENT_TYPE_DPY_CHANGED:
        eclipse_kms_logf(NVKMS_LOG_LEVEL_INFO, "display 0x%x changed",
                         event->u.displayChanged.display);
        break;
    case NVKMS_EVENT_TYPE_DYNAMIC_DPY_CONNECTED:
        eclipse_kms_logf(NVKMS_LOG_LEVEL_INFO, "dynamic display 0x%x connected",
                         event->u.dynamicDisplayConnected.display);
        break;
    case NVKMS_EVENT_TYPE_FLIP_OCCURRED:
        break;
    default:
        eclipse_kms_logf(NVKMS_LOG_LEVEL_INFO, "event %d", (int)event->type);
        break;
    }
}

/* nv_drm_dev_load's allocateDevice. NULL when kapi refused. */
struct NvKmsKapiDevice *eclipse_kms_alloc_device(NvU32 gpuId)
{
    struct NvKmsKapiAllocateDeviceParams params = { 0 };

    if (!eclipse_kms_table()) {
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR,
                         "kapi function table refused (version %s)",
                         g_nvKms.versionString ? g_nvKms.versionString : "?");
        return NULL;
    }
    params.gpuId = gpuId;
    params.privateData = NULL;
    params.eventCallback = eclipse_kms_event;
    return g_nvKms.allocateDevice(&params);
}

void eclipse_kms_free_device(struct NvKmsKapiDevice *device)
{
    if (device != NULL && g_nvKmsValid)
        g_nvKms.freeDevice(device);
}

static void eclipse_kms_probe_modes(struct NvKmsKapiDevice *device,
                                    struct EclipseKmsDisplay *out)
{
    struct NvKmsKapiDisplayMode mode;
    NvBool valid, preferred, havePreferred = NV_FALSE;
    NvU32 i;

    for (i = 0; i < ECLIPSE_KMS_MAX_MODES; i++) {
        int more;

        valid = preferred = NV_FALSE;
        more = g_nvKms.getDisplayMode(device, out->handle, i, &mode, &valid,
                                      &preferred);
        if (more < 0)
            break;
        if (valid) {
            out->numModes++;
            /* The first preferred mode, else the first valid one: what
             * nvidia-drm marks DRM_MODE_TYPE_PREFERRED. */
            if (!havePreferred && (preferred || out->numModes == 1)) {
                out->preferredWidth = mode.timings.hVisible;
                out->preferredHeight = mode.timings.vVisible;
                out->preferredRefreshMilliHz = mode.timings.refreshRate;
                out->preferredPixelClockKHz = mode.timings.pixelClockHz / 1000;
                havePreferred = preferred;
            }
        }
        if (more == 0)
            break;
    }
}

/*
 * What nv_drm_enumerate_encoders_and_connectors and a connector's
 * detect/get_modes ask nvkms, in one pass. NV_FALSE only when the device
 * itself could not be queried; a display that fails a query is reported
 * with what was learned.
 */
NvBool eclipse_kms_probe(struct NvKmsKapiDevice *device, struct EclipseKmsInfo *info)
{
    struct NvKmsKapiDeviceResourcesInfo *res;
    struct NvKmsKapiDynamicDisplayParams *dyn;
    NvKmsKapiDisplay handles[32];
    NvU32 numDisplays = 0, i;
    NvBool ok = NV_FALSE;

    nvkms_memset(info, 0, sizeof(*info));
    if (device == NULL || !g_nvKmsValid)
        return NV_FALSE;

    /* Both are kilobytes: nvidia-drm puts them on the heap too. */
    res = nvkms_alloc(sizeof(*res), NV_TRUE);
    dyn = nvkms_alloc(sizeof(*dyn), NV_TRUE);
    if (res == NULL || dyn == NULL)
        goto done;

    if (!g_nvKms.getDeviceResourcesInfo(device, res)) {
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR, "getDeviceResourcesInfo failed");
        goto done;
    }
    info->numHeads = res->numHeads;
    info->numConnectors = res->numConnectors;
    info->maxWidth = res->caps.maxWidthInPixels;
    info->maxHeight = res->caps.maxHeightInPixels;
    info->maxCursor = res->caps.maxCursorSizeInPixels;
    info->pitchAlignment = res->caps.pitchAlignment;
    info->hasVideoMemory = res->caps.hasVideoMemory;
    info->vtFbBase = res->vtFbBaseAddress;
    info->vtFbSize = res->vtFbSize;

    /* getDisplays answers the count when handed no array. */
    if (!g_nvKms.getDisplays(device, &numDisplays, NULL)) {
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR, "getDisplays (count) failed");
        goto done;
    }
    /* A dpy id list is a 32-bit mask, so this never trips; kapi would leave
     * the array unwritten if it did. */
    if (numDisplays > sizeof(handles) / sizeof(handles[0])) {
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR, "%u displays", numDisplays);
        goto done;
    }
    if (numDisplays != 0 && !g_nvKms.getDisplays(device, &numDisplays, handles)) {
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR, "getDisplays failed");
        goto done;
    }
    info->numDisplays = numDisplays;

    for (i = 0; i < numDisplays && i < ECLIPSE_KMS_MAX_DISPLAYS; i++) {
        struct EclipseKmsDisplay *out = &info->displays[i];
        struct NvKmsKapiStaticDisplayInfo st;
        struct NvKmsKapiConnectorInfo conn;

        out->handle = handles[i];

        nvkms_memset(&st, 0, sizeof(st));
        if (g_nvKms.getStaticDisplayInfo(device, handles[i], &st)) {
            out->connector = st.connectorHandle;
            out->headMask = st.headMask;
            out->internal = st.internal;
            out->isDpMST = st.isDpMST;

            nvkms_memset(&conn, 0, sizeof(conn));
            if (g_nvKms.getConnectorInfo(device, st.connectorHandle, &conn)) {
                out->connectorType = conn.type;
                out->signalFormat = conn.signalFormat;
                out->physicalIndex = conn.physicalIndex;
            } else {
                out->connectorType = NVKMS_CONNECTOR_TYPE_UNKNOWN;
                out->signalFormat = NVKMS_CONNECTOR_SIGNAL_FORMAT_UNKNOWN;
            }
        }

        nvkms_memset(dyn, 0, sizeof(*dyn));
        dyn->handle = handles[i];
        if (g_nvKms.getDynamicDisplayInfo(device, dyn)) {
            out->connected = dyn->connected ? 1 : 0;
            out->vrrSupported = dyn->vrrSupported;
            out->edidSize = dyn->edid.bufferSize;
            nvkms_memcpy(out->edid, dyn->edid.buffer,
                         dyn->edid.bufferSize < ECLIPSE_KMS_EDID_BYTES
                             ? dyn->edid.bufferSize
                             : ECLIPSE_KMS_EDID_BYTES);
        }

        if (out->connected)
            eclipse_kms_probe_modes(device, out);
    }
    ok = NV_TRUE;

done:
    if (dyn != NULL)
        nvkms_free(dyn, sizeof(*dyn));
    if (res != NULL)
        nvkms_free(res, sizeof(*res));
    return ok;
}

/* Mirrored by `KmsMode` in src/nvkms.rs. */
struct EclipseKmsMode {
    NvU32 width;
    NvU32 height;
    NvU32 refreshMilliHz;
    NvU32 pixelClockKHz;
    NvU8  valid;
    NvU8  preferred;
    NvU8  interlaced;
    NvU8  pad;
};

_Static_assert(sizeof(struct EclipseKmsMode) == 4 * 4 + 4,
               "EclipseKmsMode layout is shared with src/nvkms.rs");

/*
 * Mode `index` of a display, the way nv_drm_connector_get_modes walks them.
 * kapi's own answer: >0 more modes follow, 0 this was the last, <0 failure.
 */
int eclipse_kms_mode(struct NvKmsKapiDevice *device, NvU32 display, NvU32 index,
                     struct EclipseKmsMode *out)
{
    struct NvKmsKapiDisplayMode mode;
    NvBool valid = NV_FALSE, preferred = NV_FALSE;
    int more;

    nvkms_memset(out, 0, sizeof(*out));
    if (device == NULL || !g_nvKmsValid)
        return -1;
    more = g_nvKms.getDisplayMode(device, display, index, &mode, &valid, &preferred);
    if (more < 0)
        return more;
    out->width = mode.timings.hVisible;
    out->height = mode.timings.vVisible;
    out->refreshMilliHz = mode.timings.refreshRate;
    out->pixelClockKHz = mode.timings.pixelClockHz / 1000;
    out->valid = valid ? 1 : 0;
    out->preferred = preferred ? 1 : 0;
    out->interlaced = mode.timings.flags.interlaced ? 1 : 0;
    return more;
}

/*
 * A kapi surface and the memory under it: the firmware console, or a GEM
 * the compositor flips to (nvidia-drm's nv_drm_framebuffer, which holds the
 * same pair).
 */
struct EclipseKmsSurface {
    struct NvKmsKapiMemory *memory;
    struct NvKmsKapiSurface *surface;
    NvU32 width;
    NvU32 height;
};

/*
 * A surface on `memory`, pitch-linear. Takes `memory`: on failure it is
 * freed.
 */
static struct EclipseKmsSurface *eclipse_kms_wrap(struct NvKmsKapiDevice *device,
                                                  struct NvKmsKapiMemory *memory,
                                                  NvU32 width, NvU32 height,
                                                  NvU32 pitch,
                                                  enum NvKmsSurfaceMemoryFormat format)
{
    struct NvKmsKapiCreateSurfaceParams params = { };
    struct EclipseKmsSurface *s = nvKmsKapiCalloc(1, sizeof(*s));

    if (s == NULL) {
        g_nvKms.freeMemory(device, memory);
        return NULL;
    }
    params.planes[0].memory = memory;
    params.planes[0].offset = 0;
    params.planes[0].pitch = pitch;
    params.width = width;
    params.height = height;
    params.format = format;
    s->surface = g_nvKms.createSurface(device, &params);
    if (s->surface == NULL) {
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR,
                         "createSurface refused %ux%u pitch %u", width, height, pitch);
        g_nvKms.freeMemory(device, memory);
        nvKmsKapiFree(s);
        return NULL;
    }
    s->memory = memory;
    s->width = width;
    s->height = height;
    return s;
}

/*
 * The console memory the firmware left scanning out, as a kapi memory object
 * and a surface on it, so a modeset can keep showing exactly those pixels.
 *
 * nvkms does the same for its own console restore (nvRmImportFbConsoleMemory):
 * NV01_MEMORY_FRAMEBUFFER_CONSOLE names the VRAM the RM reserved for the
 * console, found through NV0080_CTRL_CMD_OS_UNIX_VT_GET_FB_INFO. kapi has no
 * entry point for it -- nvidia-drm never scans out the firmware's memory, it
 * has fbdev allocate new memory -- so the object is allocated in kapi's own
 * RM client and wrapped the way ImportMemory wraps an imported one. kapi's
 * freeMemory frees it like any other.
 */
struct EclipseKmsSurface *eclipse_kms_console_surface(struct NvKmsKapiDevice *device,
                                                      NvU32 width, NvU32 height,
                                                      NvU32 pitch, NvU32 depth)
{
    struct NvKmsKapiMemory *memory;
    enum NvKmsSurfaceMemoryFormat format;
    NvU32 hMemory, ret;

    if (device == NULL || !g_nvKmsValid || width == 0 || height == 0)
        return NULL;

    /* nvRmImportFbConsoleMemory's depth switch. */
    switch (depth) {
    case 15:
        format = NvKmsSurfaceMemoryFormatX1R5G5B5;
        break;
    case 16:
        format = NvKmsSurfaceMemoryFormatR5G6B5;
        break;
    case 24:
    case 32:
        format = NvKmsSurfaceMemoryFormatX8R8G8B8;
        break;
    default:
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR, "console depth %u not supported", depth);
        return NULL;
    }

    memory = nvKmsKapiCalloc(1, sizeof(*memory));
    if (memory == NULL)
        return NULL;
    hMemory = nvKmsKapiGenerateRmHandle(device);
    if (hMemory == 0) {
        nvKmsKapiFree(memory);
        return NULL;
    }
    ret = nvRmApiAlloc(device->hRmClient, device->hRmDevice, hMemory,
                       NV01_MEMORY_FRAMEBUFFER_CONSOLE, NULL);
    if (ret != NVOS_STATUS_SUCCESS) {
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR,
                         "NV01_MEMORY_FRAMEBUFFER_CONSOLE refused (0x%x)", ret);
        nvKmsKapiFreeRmHandle(device, hMemory);
        nvKmsKapiFree(memory);
        return NULL;
    }
    memory->hRmHandle = hMemory;
    memory->size = (NvU64)pitch * height;
    memory->surfaceParams.layout = NvKmsSurfaceMemoryLayoutPitch;
    memory->isVidmem = NV_TRUE;
    /* The CPU keeps drawing into it while it is on screen. */
    memory->noDisplayCaching = NV_TRUE;
    return eclipse_kms_wrap(device, memory, width, height, pitch, format);
}

/*
 * A compositor's VRAM GEM as a surface nvkms can flip to: XRGB8888,
 * pitch-linear, the only kind the driver's ADDFB makes.
 *
 * nvidia-drm gets here through importMemory, which takes the GEM from the
 * RM client of nvidia.ko's fd (NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD).
 * Eclipse has no such fd: the GEM is a handle in the RM client of the
 * context that allocated it, so it is duped into kapi's client instead --
 * the same NvRmDupObject the fd import ends in -- and wrapped the way
 * ImportMemory wraps what it imported.
 */
struct EclipseKmsSurface *eclipse_kms_import_surface(struct NvKmsKapiDevice *device,
                                                     NvU32 hSrcClient, NvU32 hSrcMemory,
                                                     NvU64 size, NvU32 width,
                                                     NvU32 height, NvU32 pitch)
{
    struct NvKmsKapiMemory *memory;
    NvU32 hMemory, ret;

    if (device == NULL || !g_nvKmsValid || hSrcClient == 0 || hSrcMemory == 0 ||
        width == 0 || height == 0 || (NvU64)pitch * height > size)
        return NULL;

    memory = nvKmsKapiCalloc(1, sizeof(*memory));
    if (memory == NULL)
        return NULL;
    hMemory = nvKmsKapiGenerateRmHandle(device);
    if (hMemory == 0) {
        nvKmsKapiFree(memory);
        return NULL;
    }
    ret = nvRmApiDupObject(device->hRmClient, device->hRmDevice, hMemory,
                           hSrcClient, hSrcMemory, 0);
    if (ret != NVOS_STATUS_SUCCESS) {
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR,
                         "dup of GEM 0x%x (client 0x%x) refused (0x%x)",
                         hSrcMemory, hSrcClient, ret);
        nvKmsKapiFreeRmHandle(device, hMemory);
        nvKmsKapiFree(memory);
        return NULL;
    }
    memory->hRmHandle = hMemory;
    memory->size = size;
    memory->surfaceParams.layout = NvKmsSurfaceMemoryLayoutPitch;
    memory->isVidmem = NV_TRUE;
    return eclipse_kms_wrap(device, memory, width, height, pitch,
                            NvKmsSurfaceMemoryFormatX8R8G8B8);
}

void eclipse_kms_surface_free(struct NvKmsKapiDevice *device, struct EclipseKmsSurface *s)
{
    if (device == NULL || s == NULL || !g_nvKmsValid)
        return;
    g_nvKms.destroySurface(device, s->surface);
    g_nvKms.freeMemory(device, s->memory);
    nvKmsKapiFree(s);
}

/* Mirrored by `KmsLut` in src/nvkms.rs. */
struct EclipseKmsLut {
    NvU64 cpu;      /* the CPU mapping nvkms writes it through */
    NvU32 hClient;  /* the RM client and memory that hold it */
    NvU32 hMemory;
    NvU32 size;     /* bytes nvkms writes: one NVEvoLutDataRec */
    NvU32 pad;
};

void eclipse_nvkms_lock(void);
void eclipse_nvkms_unlock(void);

/*
 * The default LUT surface of the device: on Turing and later
 * (needDefaultLutSurface) the ILUT that converts a window's pixels to FP16
 * and the OLUT of every head come from it whenever the client sets none,
 * which nvidia-drm's first commit and eclipse_kms_show_console never do.
 * nvAllocLutSurfacesEvo fills it with identity ramps when the device is
 * allocated, through the CPU, and then only rewrites it if a head changes
 * transfer function. Its memory is in the RM client of nvkms itself, not
 * kapi's. NV_FALSE when the device has none.
 */
NvBool eclipse_kms_default_lut(struct NvKmsKapiDevice *device, struct EclipseKmsLut *out)
{
    NVDevEvoPtr pDevEvo;
    NvBool found = NV_FALSE;

    nvkms_memset(out, 0, sizeof(*out));
    if (device == NULL)
        return NV_FALSE;
    eclipse_nvkms_lock();
    FOR_ALL_EVO_DEVS(pDevEvo) {
        NVSurfaceEvoPtr pLut = pDevEvo->lut.defaultLut;

        if (pDevEvo->numSubDevices != 1 || pDevEvo->pSubDevices[0] == NULL ||
            pDevEvo->pSubDevices[0]->gpuId != device->gpuId)
            continue;
        if (pLut != NULL && pLut->cpuAddress[0] != NULL) {
            out->cpu = (NvU64)(NvUPtr)pLut->cpuAddress[0];
            out->hClient = nvEvoGlobal.clientHandle;
            out->hMemory = pLut->planes[0].rmHandle;
            out->size = sizeof(NVEvoLutDataRec);
            found = NV_TRUE;
        }
        break;
    }
    eclipse_nvkms_unlock();
    return found;
}

/* Mirrored by `KmsHeadSet` in src/nvkms.rs: one head lit by the modeset. */
struct EclipseKmsHeadSet {
    NvU32 head;
    NvU32 display;
    NvU32 modeIndex;
};

/*
 * The device configuration nvkms is scanning out since the console modeset:
 * nvidia-drm keeps it as the CRTC and plane states, and every flip is that
 * configuration with one thing changed.
 */
struct EclipseKmsScanout {
    struct NvKmsKapiRequestedModeSetConfig req;
    NvU32 litMask;
};

/* The answers of eclipse_kms_show_console and eclipse_kms_flip, mirrored by
 * `ShowError`. */
enum {
    ECLIPSE_KMS_SHOWN = 0,
    ECLIPSE_KMS_BAD_REQUEST = 1,
    ECLIPSE_KMS_NO_OWNERSHIP = 2,
    ECLIPSE_KMS_MODE_GONE = 3,
    ECLIPSE_KMS_REFUSED = 4,
    ECLIPSE_KMS_COMMIT_FAILED = 5,
    ECLIPSE_KMS_FLIP_PENDING = 6,
};

/* nvidia-drm's plane_config_clear. */
static void eclipse_kms_layer_clear(struct NvKmsKapiLayerConfig *config)
{
    nvkms_memset(config, 0, sizeof(*config));
    config->csc = NVKMS_IDENTITY_CSC_MATRIX;
}

/* plane_req_config_update for a primary plane showing `s` at the top left,
 * unscaled and vsynced, cropped to the mode. */
static void eclipse_kms_primary(struct NvKmsKapiHeadRequestedConfig *h,
                                const struct EclipseKmsSurface *s)
{
    struct NvKmsKapiLayerRequestedConfig *primary =
        &h->layerRequestedConfig[NVKMS_KAPI_LAYER_PRIMARY_IDX];
    NvU32 w = s->width, hgt = s->height;

    if (w > h->modeSetConfig.mode.timings.hVisible)
        w = h->modeSetConfig.mode.timings.hVisible;
    if (hgt > h->modeSetConfig.mode.timings.vVisible)
        hgt = h->modeSetConfig.mode.timings.vVisible;
    primary->config.surface = s->surface;
    primary->config.compParams.compMode = NVKMS_COMPOSITION_BLENDING_MODE_OPAQUE;
    primary->flags.srcWHChanged = primary->config.srcWidth != w ||
                                  primary->config.srcHeight != hgt;
    primary->flags.dstWHChanged = primary->flags.srcWHChanged;
    primary->config.srcWidth = w;
    primary->config.srcHeight = hgt;
    primary->config.dstWidth = w;
    primary->config.dstHeight = hgt;
    primary->config.minPresentInterval = 1;
    primary->flags.surfaceChanged = NV_TRUE;
}

/* After a commit: nothing has changed since. */
static void eclipse_kms_settled(struct NvKmsKapiRequestedModeSetConfig *req)
{
    NvU32 head, layer;

    for (head = 0; head < NVKMS_KAPI_MAX_HEADS; head++) {
        struct NvKmsKapiHeadRequestedConfig *h = &req->headRequestedConfig[head];

        nvkms_memset(&h->flags, 0, sizeof(h->flags));
        nvkms_memset(&h->cursorRequestedConfig.flags, 0,
                     sizeof(h->cursorRequestedConfig.flags));
        for (layer = 0; layer < NVKMS_KAPI_LAYER_MAX; layer++)
            nvkms_memset(&h->layerRequestedConfig[layer].flags, 0,
                         sizeof(h->layerRequestedConfig[layer].flags));
    }
}

/*
 * The first modeset: each head in `sets` lights its display at its mode with
 * the console as the primary layer, unscaled, and every other head of the
 * device goes dark -- nvidia-drm's first atomic commit, which names every
 * CRTC. Validated first (commit = false, as atomic_check does), then
 * committed. Takes modeset ownership, as nvidia-drm does when a DRM master
 * appears, and gives it back if the modeset does not happen.
 *
 * On success *out is the configuration now on screen, for the flips.
 */
int eclipse_kms_show_console(struct NvKmsKapiDevice *device,
                             const struct EclipseKmsSurface *console,
                             const struct EclipseKmsHeadSet *sets, NvU32 numSets,
                             NvU32 numHeads, struct EclipseKmsScanout **out)
{
    struct EclipseKmsScanout *scanout = NULL;
    struct NvKmsKapiRequestedModeSetConfig *req;
    struct NvKmsKapiModeSetReplyConfig *reply = NULL;
    int result = ECLIPSE_KMS_BAD_REQUEST;
    NvBool owned = NV_FALSE;
    NvU32 i, layer;

    if (out != NULL)
        *out = NULL;
    if (device == NULL || console == NULL || sets == NULL || numSets == 0 ||
        out == NULL || !g_nvKmsValid || numHeads == 0 ||
        numHeads > NVKMS_KAPI_MAX_HEADS)
        return ECLIPSE_KMS_BAD_REQUEST;

    /* Both are tens of kilobytes. */
    scanout = nvkms_alloc(sizeof(*scanout), NV_TRUE);
    reply = nvkms_alloc(sizeof(*reply), NV_TRUE);
    if (scanout == NULL || reply == NULL)
        goto done;
    req = &scanout->req;

    /* nv_drm_crtc_state_alloc, for every head. */
    req->headsMask = (numHeads == 32) ? 0xffffffffu : ((1u << numHeads) - 1);
    for (i = 0; i < numHeads; i++) {
        struct NvKmsKapiHeadRequestedConfig *h = &req->headRequestedConfig[i];

        h->modeSetConfig.olutFpNormScale = NVKMS_OLUT_FP_NORM_SCALE_DEFAULT;
        for (layer = 0; layer < NVKMS_KAPI_LAYER_MAX; layer++)
            eclipse_kms_layer_clear(&h->layerRequestedConfig[layer].config);
        /* Dark unless a set below lights it. */
        h->flags.activeChanged = NV_TRUE;
        h->flags.displaysChanged = NV_TRUE;
    }

    for (i = 0; i < numSets; i++) {
        const struct EclipseKmsHeadSet *set = &sets[i];
        struct NvKmsKapiHeadRequestedConfig *h;
        struct NvKmsKapiLayerRequestedConfig *primary;
        NvBool valid = NV_FALSE, preferred = NV_FALSE;

        if (set->head >= numHeads)
            goto done;
        h = &req->headRequestedConfig[set->head];
        if (h->modeSetConfig.numDisplays != 0)
            goto done; /* two sets for one head */

        if (g_nvKms.getDisplayMode(device, set->display, set->modeIndex,
                                   &h->modeSetConfig.mode, &valid, &preferred) < 0 ||
            !valid) {
            result = ECLIPSE_KMS_MODE_GONE;
            goto done;
        }
        h->modeSetConfig.bActive = NV_TRUE;
        h->modeSetConfig.numDisplays = 1;
        h->modeSetConfig.displays[0] = set->display;
        h->flags.modeChanged = NV_TRUE;

        eclipse_kms_primary(h, console);
        primary = &h->layerRequestedConfig[NVKMS_KAPI_LAYER_PRIMARY_IDX];
        primary->flags.srcXYChanged = NV_TRUE;
        primary->flags.srcWHChanged = NV_TRUE;
        primary->flags.dstXYChanged = NV_TRUE;
        primary->flags.dstWHChanged = NV_TRUE;
        primary->flags.cscChanged = NV_TRUE;
        scanout->litMask |= 1u << set->head;
    }

    if (!g_nvKms.grabOwnership(device)) {
        result = ECLIPSE_KMS_NO_OWNERSHIP;
        goto done;
    }
    owned = NV_TRUE;

    if (!g_nvKms.applyModeSetConfig(device, req, reply, NV_FALSE)) {
        result = ECLIPSE_KMS_REFUSED;
        goto done;
    }
    nvkms_memset(reply, 0, sizeof(*reply));
    if (!g_nvKms.applyModeSetConfig(device, req, reply, NV_TRUE)) {
        result = ECLIPSE_KMS_COMMIT_FAILED;
        goto done;
    }
    eclipse_kms_settled(req);
    *out = scanout;
    scanout = NULL;
    result = ECLIPSE_KMS_SHOWN;

done:
    if (owned && result != ECLIPSE_KMS_SHOWN)
        g_nvKms.releaseOwnership(device);
    if (reply != NULL)
        nvkms_free(reply, sizeof(*reply));
    if (scanout != NULL)
        nvkms_free(scanout, sizeof(*scanout));
    return result;
}

/*
 * Flip every lit head to `s`: the configuration on screen with only the
 * primary layer's surface changed, which kapi turns into NVKMS_IOCTL_FLIP.
 * The flip lands at the next vblank and this returns once it is queued.
 *
 * Like nvidia-drm, which waits for a CRTC's previous flip before committing
 * the next one, a flip never goes in while one is still pending:
 * ECLIPSE_KMS_FLIP_PENDING says so, and nothing was changed.
 */
int eclipse_kms_flip(struct NvKmsKapiDevice *device, struct EclipseKmsScanout *scanout,
                     const struct EclipseKmsSurface *s)
{
    struct NvKmsKapiModeSetReplyConfig *reply;
    struct NvKmsKapiRequestedModeSetConfig *req;
    struct NvKmsKapiLayerConfig saved[NVKMS_KAPI_MAX_HEADS];
    NvU32 head;
    int result = ECLIPSE_KMS_SHOWN;

    if (device == NULL || scanout == NULL || s == NULL || !g_nvKmsValid ||
        scanout->litMask == 0)
        return ECLIPSE_KMS_BAD_REQUEST;
    req = &scanout->req;

    for (head = 0; head < NVKMS_KAPI_MAX_HEADS; head++) {
        NvBool pending = NV_FALSE;

        if ((scanout->litMask & (1u << head)) == 0)
            continue;
        if (!g_nvKms.getFlipPendingStatus(device, head, NVKMS_KAPI_LAYER_PRIMARY_IDX,
                                          &pending))
            return ECLIPSE_KMS_REFUSED;
        if (pending)
            return ECLIPSE_KMS_FLIP_PENDING;
    }

    reply = nvkms_alloc(sizeof(*reply), NV_TRUE);
    if (reply == NULL)
        return ECLIPSE_KMS_BAD_REQUEST;
    for (head = 0; head < NVKMS_KAPI_MAX_HEADS; head++) {
        struct NvKmsKapiHeadRequestedConfig *h = &req->headRequestedConfig[head];

        saved[head] = h->layerRequestedConfig[NVKMS_KAPI_LAYER_PRIMARY_IDX].config;
        if (scanout->litMask & (1u << head))
            eclipse_kms_primary(h, s);
    }
    if (!g_nvKms.applyModeSetConfig(device, req, reply, NV_TRUE)) {
        /* What is on screen is still the old configuration. */
        for (head = 0; head < NVKMS_KAPI_MAX_HEADS; head++)
            req->headRequestedConfig[head]
                .layerRequestedConfig[NVKMS_KAPI_LAYER_PRIMARY_IDX].config = saved[head];
        result = ECLIPSE_KMS_COMMIT_FAILED;
    }
    eclipse_kms_settled(req);
    nvkms_free(reply, sizeof(*reply));
    return result;
}

/* Whether a flip on any lit head has yet to land: 1 yes, 0 no, -1 unknown. */
int eclipse_kms_flip_pending(struct NvKmsKapiDevice *device,
                             const struct EclipseKmsScanout *scanout)
{
    NvU32 head;

    if (device == NULL || scanout == NULL || !g_nvKmsValid)
        return -1;
    for (head = 0; head < NVKMS_KAPI_MAX_HEADS; head++) {
        NvBool pending = NV_FALSE;

        if ((scanout->litMask & (1u << head)) == 0)
            continue;
        if (!g_nvKms.getFlipPendingStatus(device, head, NVKMS_KAPI_LAYER_PRIMARY_IDX,
                                          &pending))
            return -1;
        if (pending)
            return 1;
    }
    return 0;
}

/* The cursor image is ECLIPSE_KMS_CURSOR x ECLIPSE_KMS_CURSOR, the size the
 * driver advertises (DRM_CAP_CURSOR_WIDTH/HEIGHT); nvkms takes square
 * power-of-two cursors. */
#define ECLIPSE_KMS_CURSOR 64

/*
 * The cursor plane's surface: A8R8G8B8 in video memory, from kapi's own
 * allocateMemory, as nvidia-drm's cursor framebuffers are. The image is
 * written through the RM (eclipse_rm_write_memory), so the handles it lives
 * under come back in *phClient / *phMemory.
 */
struct EclipseKmsSurface *eclipse_kms_cursor_surface(struct NvKmsKapiDevice *device,
                                                     NvU32 *phClient, NvU32 *phMemory)
{
    struct NvKmsKapiAllocateMemoryParams params = { };
    struct EclipseKmsSurface *s;
    struct NvKmsKapiMemory *memory;
    NvU8 compressible = 0;

    if (device == NULL || phClient == NULL || phMemory == NULL || !g_nvKmsValid)
        return NULL;
    params.layout = NvKmsSurfaceMemoryLayoutPitch;
    params.type = NVKMS_KAPI_ALLOCATION_TYPE_SCANOUT;
    params.size = ECLIPSE_KMS_CURSOR * ECLIPSE_KMS_CURSOR * 4;
    params.useVideoMemory = NV_TRUE;
    params.compressible = &compressible;
    memory = g_nvKms.allocateMemory(device, &params);
    if (memory == NULL) {
        eclipse_kms_logf(NVKMS_LOG_LEVEL_ERROR, "cursor allocateMemory refused");
        return NULL;
    }
    *phClient = device->hRmClient;
    *phMemory = memory->hRmHandle;
    s = eclipse_kms_wrap(device, memory, ECLIPSE_KMS_CURSOR, ECLIPSE_KMS_CURSOR,
                         ECLIPSE_KMS_CURSOR * 4, NvKmsSurfaceMemoryFormatA8R8G8B8);
    if (s == NULL) {
        *phClient = 0;
        *phMemory = 0;
    }
    return s;
}

/*
 * The cursor plane on every lit head: `s` at (x, y), premultiplied (DRM's
 * cursor plane, nvidia-drm's cursor default composition), or off for a NULL
 * `s`. A cursor-only flip: nvkms moves the cursor through its immediate
 * channel and never waits on the primary layer's pending flip.
 */
int eclipse_kms_cursor(struct NvKmsKapiDevice *device, struct EclipseKmsScanout *scanout,
                       const struct EclipseKmsSurface *s, NvS32 x, NvS32 y,
                       NvBool surfaceChanged)
{
    struct NvKmsKapiModeSetReplyConfig *reply;
    struct NvKmsKapiRequestedModeSetConfig *req;
    NvU32 head;
    int result = ECLIPSE_KMS_SHOWN;

    if (device == NULL || scanout == NULL || !g_nvKmsValid || scanout->litMask == 0)
        return ECLIPSE_KMS_BAD_REQUEST;
    req = &scanout->req;

    /* The plane is 16-bit signed. */
    if (x < -32768)
        x = -32768;
    if (x > 32767)
        x = 32767;
    if (y < -32768)
        y = -32768;
    if (y > 32767)
        y = 32767;

    reply = nvkms_alloc(sizeof(*reply), NV_TRUE);
    if (reply == NULL)
        return ECLIPSE_KMS_BAD_REQUEST;
    for (head = 0; head < NVKMS_KAPI_MAX_HEADS; head++) {
        struct NvKmsKapiCursorRequestedConfig *c =
            &req->headRequestedConfig[head].cursorRequestedConfig;

        if ((scanout->litMask & (1u << head)) == 0)
            continue;
        if (surfaceChanged || c->surface != (s != NULL ? s->surface : NULL)) {
            c->surface = s != NULL ? s->surface : NULL;
            c->compParams.compMode = NVKMS_COMPOSITION_BLENDING_MODE_PREMULT_ALPHA;
            c->flags.surfaceChanged = NV_TRUE;
        }
        c->dstX = (NvS16)x;
        c->dstY = (NvS16)y;
        c->flags.dstXYChanged = NV_TRUE;
    }
    if (!g_nvKms.applyModeSetConfig(device, req, reply, NV_TRUE))
        result = ECLIPSE_KMS_COMMIT_FAILED;
    eclipse_kms_settled(req);
    nvkms_free(reply, sizeof(*reply));
    return result;
}
