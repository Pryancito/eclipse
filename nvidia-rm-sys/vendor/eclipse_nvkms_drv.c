/*
 * Eclipse's side of nvkms-kapi: what kernel-open/nvidia-drm does with it in
 * Linux, reduced to what the Rust driver needs (src/nvkms.rs). Linux's
 * nvidia-drm calls the kapi function table from C, through structs too big
 * and too bitfield-heavy to mirror in Rust, so the calls are made here and
 * Rust gets back small flat structs.
 *
 * PR 3 of docs/README-nvkms-port.md: allocate an nvkms device per GPU and
 * report what nvkms sees on it -- heads, connectors, displays, EDID and
 * modes. Like nvidia-drm before its first atomic commit, nothing here
 * changes what is on the screen: nvkms keeps scanning out whatever the
 * firmware left until a modeset is applied.
 */

#include "nvUnixVersion.h"
#include "nvidia-modeset-os-interface.h"
#include "nvkms-kapi.h"

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
