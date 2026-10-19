/*
 * eclipse_rm_frontend.c -- the RM half of /dev/nvidiactl.
 *
 * OUR code, not NVIDIA's. NVIDIA's user-mode driver (libcuda, libnvidia-ml,
 * and through them nvidia-smi and every CUDA program) does not speak DRM: it
 * opens /dev/nvidiactl and /dev/nvidia<N> and drives the Resource Manager
 * through the `NV_ESC_RM_*` escapes. On Linux those land in
 * `arch/nvalloc/unix/src/escape.c` (`RmIoctl`), reached from
 * `kernel-open/nvidia/nv.c` (`nvidia_ioctl`) by way of `rm_ioctl` in
 * `osapi.c`. build.rs leaves all of `arch/nvalloc/unix/src/` out of Eclipse
 * on purpose (it is the Linux platform layer, replaced by os_interface.rs),
 * so the escapes had nowhere to land.
 *
 * This file is the subset of `RmIoctl` that the RM core needs to serve a
 * user-mode client, transliterated case by case: RM_ALLOC (both the NVOS21
 * and the NVOS64 "access" shape), RM_FREE, RM_CONTROL, RM_DUP_OBJECT and
 * RM_SHARE. Every case validates the payload size exactly as escape.c does
 * and calls the same `Nv0x...WithSecInfo` entry point with the same security
 * info, so a call that works against nvidia.ko works here and a call
 * nvidia.ko refuses is refused here, by the same RM code.
 *
 * RM_MAP_MEMORY and RM_UNMAP_MEMORY are here too, with the part of
 * osapi.c's rm_create_mmap_context that validates a mapping before the
 * kernel side attaches it to the file the client will mmap (see
 * "User mappings" below).
 *
 * And os.c's walk over an object's event notifications (osNotifyEvent and
 * the osEventNotification family), which hands each OS event that fires to
 * the registration the client made for it (see "OS events" below).
 *
 * What is NOT here yet, and answers NV_ERR_NOT_SUPPORTED (which the kernel
 * side turns into EINVAL, like an unknown escape on Linux): RM_ALLOC_MEMORY
 * and RM_VID_HEAP_CONTROL (OS descriptors: pinning user pages), and the two
 * controls that carry a device fd inside their parameters
 * (NV00FD_CTRL_CMD_ATTACH_GPU, NV00E0_CTRL_CMD_EXPORT_MEM). The nv.c-level
 * escapes (CARD_INFO, CHECK_VERSION_STR, ATTACH_GPUS_TO_FD, REGISTER_FD, ...)
 * are not RM calls on Linux either and live on the Rust side
 * (linux-object/src/fs/devfs/nvidia.rs), and so do the three OS-event
 * escapes, which rm_ioctl serves before RmIoctl without entering the RM
 * core (ALLOC_OS_EVENT, FREE_OS_EVENT, RM_GET_EVENT_DATA).
 *
 * Two departures from Linux, both forced by how Eclipse brings the RM up:
 *
 * - Every entry enables "expanded GPU visibility" for the call, like every
 *   other entry point in eclipse_rm_init.c. Eclipse attaches and state-loads
 *   GPUs from its own ladder, not from RmInitAdapter, and gpumgrGetGpu
 *   refuses a GPU whose STATE_INITIALIZED property that ladder never set.
 * - The parameters are in user memory (PARAM_LOCATION_USER, as on Linux), so
 *   the RM copies them with os_memcpy_from_user / os_memcpy_to_user. Those
 *   answer only while a front-end call is in flight (frontend.rs).
 */
#include "core/prelude.h"
#include "core/locks.h"
#include "core/thread_state.h"
#include "gpu_mgr/gpu_mgr.h"
#include "gpu/gpu.h"
#include "rmapi/rmapi.h"
#include "rmapi/exports.h"
#include "resserv/rs_server.h"
#include "nvos.h"
#include "nv_escape.h"
#include "nv-ioctl.h"
#include "nvUnixVersion.h"
#include "class/cl0000.h"      /* NV01_ROOT, NV01_ROOT_CLIENT */
#include "class/cl0001.h"      /* NV01_ROOT_NON_PRIV */
#include "class/cl0005.h"      /* NV01_EVENT */
#include "class/cl90cd.h"      /* NV_EVENT_BUFFER_BIND */
#include "ctrl/ctrl0000/ctrl0000gpu.h" /* NV0000_CTRL_GPU_INVALID_ID */
#include "ctrl/ctrl00fd.h"     /* NV00FD_CTRL_CMD_ATTACH_GPU */
#include "ctrl/ctrl00e0.h"     /* NV00E0_CTRL_CMD_EXPORT_MEM */
#include "os/os.h"
#include "os/nv_memory_type.h"
#include "gpu/bus/kern_bus.h"
#include "gpu/device/device.h"
#include "gpu/subdevice/subdevice.h"
#include "gpu/mem_mgr/mem_desc.h"
#include "gpu/mem_mgr/rm_page_size.h"
#include "mem_mgr/mem.h"
#include "rmapi/rs_utils.h"
#include "rmapi/event.h"       /* EVENTNOTIFICATION */
#include "nv-unix-nvos-params-wrappers.h" /* nv_ioctl_nvos33_parameters_with_fd */
#include "eclipse_rm_mem.h"

extern RsServer g_resServ;

/* frontend/events.rs: the OS-event registrations (see "OS events" below). */
void eclipse_rm_frontend_post_event(NvP64 event, NvHandle hEvent, NvU32 notifyIndex,
                                    NvU32 info32, NvU16 info16, NvU32 bDataValid);
void rm_client_free_os_events(NvHandle client);

/*
 * User mappings.
 *
 * On Linux a client maps RM memory in two steps. RM_MAP_MEMORY asks the RM
 * for a CPU mapping, and the RM answers with an "address" that is really a
 * physical one (the bus address of a BAR window, or of a system-memory
 * page); escape.c then has rm_create_mmap_context validate it and record it
 * on the /dev/nvidia* file the client named in the parameters
 * (nv_add_mapping_context_to_file). The client mmaps that file at offset 0,
 * and nvidia_mmap maps exactly what was recorded. One file holds one
 * mapping, so libnvidia opens a fresh file for each.
 *
 * Here the RM half is this file: the aperture windows osMapPciMemoryAreaUser
 * leaves for the call (below), the validation of a system-memory mapping
 * (RmGetAllocPrivate), and what is to be recorded (EclipseRmMmapContext).
 * The recording and the mmap are the kernel side's
 * (linux-object/src/fs/devfs/nvidia.rs), which resolved the client's fd
 * before the call.
 */

/*
 * Whether a front-end call is in flight: set by eclipse_rm_frontend_escape
 * for its whole length. The Rust half holds the RM gate around every call
 * into this file, so there is at most one.
 */
static NvBool s_bInCall = NV_FALSE;

NvBool eclipse_rm_frontend_in_call(void)
{
    return s_bInCall;
}

/*
 * The aperture window the RM mapped for the client during the call in
 * flight, waiting for RM_MAP_MEMORY to record it: the
 * nv_usermap_access_params_t os.c's osMapPciMemoryAreaUser leaves in
 * TLS_ENTRY_ID_MAPPING_CONTEXT for RmCreateMmapContextLocked to pick up.
 *
 * Unlike that TLS entry it never outlives the call. One the call does not
 * use (the map failed after the window was mapped, or an escape that is not
 * RM_MAP_MEMORY mapped one for the client) is dropped when the call ends; on
 * Linux it stays, and the next RM_MAP_MEMORY on that thread records it in
 * place of its own mapping, unvalidated.
 */
typedef struct
{
    NvBool bValid;
    NvU64  numRanges;
    NvU64  start;      /* the first range's first byte, page aligned */
    NvU64  size;       /* every range, the last one rounded up to a page */
    NvU32  caching;    /* NV_MEMORY_* */
} EclipsePendingUserMap;

static EclipsePendingUserMap s_pending;

/*
 * osMapPciMemoryAreaUser (os.c): the "address" of a user mapping of a GPU
 * aperture is its bus address, and the window is left for RM_MAP_MEMORY to
 * record. Outside a front-end call nothing can use one, and this answers
 * NV_ERR_NOT_SUPPORTED, as it always did.
 */
NV_STATUS osMapPciMemoryAreaUser(
    OS_GPU_INFO *pOsGpuInfo,
    MemoryArea   memArea,
    NvU32        protect,
    NvU32        mode,
    NvP64       *pVirtualAddress,
    NvP64       *pPriv
)
{
    NvU64 start;
    NvU64 diffStart;
    NvU64 size;
    NvU64 i;

    (void) pOsGpuInfo;
    (void) protect;

    if (memArea.numRanges == 0)
    {
        *pVirtualAddress = (NvP64) NULL;
        *pPriv = NULL;
        return NV_OK;
    }

    if (!s_bInCall)
    {
        return NV_ERR_NOT_SUPPORTED;
    }

    /* Linux aligns the first range down and the last one up, in place. */
    start = NV_ALIGN_DOWN64(memArea.pRanges[0].start, RM_PAGE_SIZE);
    diffStart = memArea.pRanges[0].start - start;
    size = diffStart;
    for (i = 0; i < memArea.numRanges; i++)
    {
        if (!portSafeAddU64(size, memArea.pRanges[i].size, &size))
        {
            return NV_ERR_INVALID_LIMIT;
        }
    }
    if (!portSafeAddU64(size, RM_PAGE_SIZE - 1, &size))
    {
        return NV_ERR_INVALID_LIMIT;
    }
    size = NV_ALIGN_DOWN64(size, RM_PAGE_SIZE);

    s_pending.bValid = NV_TRUE;
    s_pending.numRanges = memArea.numRanges;
    s_pending.start = start;
    s_pending.size = size;
    s_pending.caching = mode;

    *pVirtualAddress = (NvP64) (start + diffStart);
    return NV_OK;
}

/* osMapPciMemoryUser (os.c): one range of osMapPciMemoryAreaUser. */
NV_STATUS osMapPciMemoryUser(
    OS_GPU_INFO *pOsGpuInfo,
    RmPhysAddr   busAddress,
    NvU64        length,
    NvU32        Protect,
    NvP64       *pVirtualAddress,
    NvP64       *pPriv,
    NvU32        modeFlag
)
{
    MemoryArea memArea;
    MemoryRange memRange;

    memArea.numRanges = 1;
    memArea.pRanges = &memRange;

    memRange.start = busAddress;
    memRange.size = length;

    return osMapPciMemoryAreaUser(pOsGpuInfo, memArea, Protect, modeFlag, pVirtualAddress, pPriv);
}

/* What EclipseRmMmapContext.memoryType says the mapping is of. */
#define ECLIPSE_MMAP_SYSTEM       0
#define ECLIPSE_MMAP_REGISTERS    1
#define ECLIPSE_MMAP_FRAMEBUFFER  2

/*
 * One validated user mapping, which the kernel side records on the file the
 * client named: nv_alloc_mapping_context_t, reduced to one physically
 * contiguous range. Shared with frontend.rs (`RawMmapContext`), and its size
 * is in LAYOUT.
 */
typedef struct
{
    NvU32 bValid;       /* there is a mapping to record                    */
    NvU32 bCtl;         /* system memory: the file must be /dev/nvidiactl   */
    NvU32 gpuInstance;  /* an aperture: the file must be this GPU's node    */
    NvU32 memoryType;   /* ECLIPSE_MMAP_*                                   */
    NvU32 caching;      /* NV_MEMORY_*: the aperture's, or the allocation's */
    NvU32 prot;         /* NV_PROTECT_*                                     */
    NvU64 start;        /* first byte, page aligned                         */
    NvU64 size;         /* bytes, whole pages: the exact length of an
                         * aperture mapping, the most of a system one       */
} EclipseRmMmapContext;

/* Whether [start, start + size) lies in [base, base + barSize). */
static NvBool _eclipse_rm_in_bar(NvU64 start, NvU64 size, NvU64 base, NvU64 barSize)
{
    NvU64 end;
    NvU64 barEnd;

    return (barSize != 0) && (size != 0) &&
           portSafeAddU64(start, size, &end) &&
           portSafeAddU64(base, barSize, &barEnd) &&
           (start >= base) && (end <= barEnd);
}

/*
 * RmGetAllocPrivate (osapi.c): the system-memory allocation behind
 * `hMemory` that the user address `offset` (a physical one,
 * osMapSystemMemory) points into, and the page it names.
 *
 * Linux also maps peer-I/O, carveout and external system memory here, whose
 * memData is an nv_alloc_t that some other Linux path made. Eclipse has none
 * of those paths, so only an allocation osAllocPagesInternal made, whose
 * memData is an EclipseSysmemAlloc, can be mapped.
 */
static NV_STATUS _eclipse_rm_get_alloc_private(
    RmClient            *pRmClient,
    NvHandle             hMemory,
    NvU64                offset,
    NvU64                length,
    NvU32               *pProtection,
    EclipseSysmemAlloc **ppAlloc,
    NvU64               *pPageIndex
)
{
    NV_STATUS rmStatus;
    PMEMORY_DESCRIPTOR pMemDesc;
    NvU64 pageOffset;
    NvU64 pageCount;
    NvU64 endingOffset;
    RsResourceRef *pResourceRef;
    RmResource *pRmResource;
    void *pMemData;

    *pProtection = NV_PROTECT_READ_WRITE;
    *ppAlloc = NULL;

    pageOffset = (offset & (RM_PAGE_SIZE - 1));
    offset -= pageOffset;

    NV_ASSERT_OR_RETURN(rmapiLockIsOwner(), NV_ERR_INVALID_LOCK_STATE);

    rmStatus = clientGetResourceRef(staticCast(pRmClient, RsClient),
        hMemory, &pResourceRef);
    if (rmStatus != NV_OK)
        return rmStatus;

    pRmResource = dynamicCast(pResourceRef->pResource, RmResource);
    if (pRmResource == NULL)
        return NV_ERR_INVALID_OBJECT;

    rmStatus = rmresGetMemoryMappingDescriptor(pRmResource, &pMemDesc);
    if (rmStatus != NV_OK)
        return rmStatus;

    if (!pMemDesc->Allocated ||
        (memdescGetAddressSpace(pMemDesc) != ADDR_SYSMEM))
    {
        NV_PRINTF(LEVEL_ERROR, "Mmap is not allowed\n");
        return NV_ERR_OBJECT_NOT_FOUND;
    }

    pMemData = memdescGetMemData(pMemDesc);
    if (pMemData == NULL)
        return NV_ERR_OBJECT_NOT_FOUND;

    rmStatus = os_match_mmap_offset(pMemData, offset, pPageIndex);
    if (rmStatus != NV_OK)
        return rmStatus;

    if (!portSafeAddU64(pageOffset, length, &endingOffset))
        return NV_ERR_INVALID_ARGUMENT;

    pageCount = (endingOffset / RM_PAGE_SIZE);

    if (!portSafeAddU64(*pPageIndex + ((endingOffset % RM_PAGE_SIZE) ? 1 : 0),
                        pageCount, &pageCount))
        return NV_ERR_INVALID_ARGUMENT;

    /* NV_RM_PAGES_TO_OS_PAGES is the identity with 4 KiB pages on both sides. */
    if (pageCount > pMemDesc->PageCount)
        return NV_ERR_INVALID_ARGUMENT;

    if (memdescGetFlag(pMemDesc, MEMDESC_FLAGS_USER_READ_ONLY))
        *pProtection = NV_PROTECT_READABLE;

    *ppAlloc = (EclipseSysmemAlloc *) pMemData;
    return NV_OK;
}

/*
 * RmCreateMmapContextLocked (osapi.c), up to nv_add_mapping_context_to_file,
 * which is the kernel side's: fills `pCtx` with what to record.
 *
 * Two departures. An aperture address with no window behind it is refused
 * rather than validated with NV2080_CTRL_CMD_GPU_VALIDATE_MEM_MAP_REQUEST:
 * every aperture mapping 580.178.04 makes for a client leaves a window
 * (osMapPciMemoryAreaUser), so only an address the client made up gets
 * there, and the RM's own check would need a per-GPU internal client this
 * port does not keep. And a window of more than one range (a discontiguous
 * BAR1 mapping) is refused for now: the kernel side maps one physical range.
 */
static NV_STATUS _eclipse_rm_create_mmap_context_locked(
    RmClient             *pRmClient,
    NvHandle              hDevice,
    NvHandle              hMemory,
    NvP64                 address,
    NvU64                 size,
    EclipseRmMmapContext *pCtx
)
{
    NV_STATUS status;
    OBJGPU *pGpu = NULL;
    RsClient *pClient = staticCast(pRmClient, RsClient);
    NvBool bClientMap = (pClient->hClient == hDevice);
    EclipseSysmemAlloc *pAlloc = NULL;
    NvU64 pageIndex = 0;
    NvU32 prot = 0;

    if (!bClientMap)
    {
        if (CliSetGpuContext(pClient->hClient, hDevice, &pGpu, NULL) != NV_OK)
        {
            Subdevice *pSubdevice;

            status = subdeviceGetByHandle(pClient, hDevice, &pSubdevice);
            if (status != NV_OK)
                return status;

            pGpu = GPU_RES_GET_GPU(pSubdevice);

            GPU_RES_SET_THREAD_BC_STATE(pSubdevice);
        }
    }

    if (s_pending.bValid)
    {
        KernelBus *pKernelBus;

        s_pending.bValid = NV_FALSE;

        /* Linux reads the GPU's nv_state_t here; with no GPU it has none. */
        if (pGpu == NULL)
            return NV_ERR_INVALID_ARGUMENT;

        if (s_pending.numRanges != 1)
        {
            NV_PRINTF(LEVEL_ERROR,
                      "a user mapping of %llu discontiguous ranges is not served yet\n",
                      s_pending.numRanges);
            return NV_ERR_NOT_SUPPORTED;
        }

        /* nvidia_mmap_helper's IS_REG_OFFSET / IS_FB_OFFSET, with the BAR
         * sizes the RM itself read from the GPU. */
        pKernelBus = GPU_GET_KERNEL_BUS(pGpu);
        if (_eclipse_rm_in_bar(s_pending.start, s_pending.size,
                               pGpu->busInfo.gpuPhysAddr,
                               kbusGetPciBarSize(pKernelBus, 0)))
        {
            pCtx->memoryType = ECLIPSE_MMAP_REGISTERS;
        }
        else if (_eclipse_rm_in_bar(s_pending.start, s_pending.size,
                                    gpumgrGetGpuPhysFbAddr(pGpu),
                                    kbusGetPciBarSize(pKernelBus, 1)))
        {
            pCtx->memoryType = ECLIPSE_MMAP_FRAMEBUFFER;
        }
        else
        {
            return NV_ERR_INVALID_ADDRESS;
        }

        pCtx->bCtl = NV_FALSE;
        pCtx->gpuInstance = pGpu->gpuInstance;
        pCtx->caching = s_pending.caching;
        pCtx->prot = NV_PROTECT_READ_WRITE;
        pCtx->start = s_pending.start;
        pCtx->size = s_pending.size;
        pCtx->bValid = NV_TRUE;
        return NV_OK;
    }

    /*
     * No window: a system-memory mapping, recorded on the control device,
     * once the address is found to name a page of `hMemory`'s allocation. An
     * aperture address fails here, since it names none.
     */
    status = _eclipse_rm_get_alloc_private(pRmClient, hMemory, (NvU64) address,
                                           size, &prot, &pAlloc, &pageIndex);
    if (status != NV_OK)
        return status;

    pCtx->bCtl = NV_TRUE;
    pCtx->gpuInstance = NV_U32_MAX;
    pCtx->memoryType = ECLIPSE_MMAP_SYSTEM;
    pCtx->caching = pAlloc->cacheType;
    pCtx->prot = prot;
    pCtx->start = pAlloc->physBase + (pageIndex * RM_PAGE_SIZE);
    pCtx->size = (pAlloc->pageCount - pageIndex) * RM_PAGE_SIZE;
    pCtx->bValid = NV_TRUE;
    return NV_OK;
}

/* rm_create_mmap_context (osapi.c): the locks around the above. */
static NV_STATUS _eclipse_rm_create_mmap_context(
    NvHandle              hClient,
    NvHandle              hDevice,
    NvHandle              hMemory,
    NvP64                 address,
    NvU64                 size,
    EclipseRmMmapContext *pCtx
)
{
    NV_STATUS rmStatus = NV_OK;
    // LOCK: acquire API lock
    if ((rmStatus = rmapiLockAcquire(RMAPI_LOCK_FLAGS_READ, RM_LOCK_MODULES_OSAPI)) == NV_OK)
    {
        CLIENT_ENTRY *pClientEntry;
        RmClient *pRmClient;

        if (NV_OK != serverutilAcquireClient(hClient, LOCK_ACCESS_READ, &pClientEntry,
                &pRmClient))
        {
            // UNLOCK: release API lock
            rmapiLockRelease();
            return NV_ERR_INVALID_CLIENT;
        }

        if (pRmClient->ProcID != osGetCurrentProcess())
        {
            rmStatus = NV_ERR_INVALID_CLIENT;
        }
        // LOCK: acquire GPUs lock
        else if ((rmStatus = rmGpuLocksAcquire(GPUS_LOCK_FLAGS_NONE, RM_LOCK_MODULES_OSAPI)) == NV_OK)
        {
            rmStatus = _eclipse_rm_create_mmap_context_locked(pRmClient, hDevice,
                                                              hMemory, address, size,
                                                              pCtx);
            // UNLOCK: release GPUs lock
            rmGpuLocksRelease(GPUS_LOCK_FLAGS_NONE, NULL);
        }

        serverutilReleaseClient(LOCK_ACCESS_READ, pClientEntry);

        // UNLOCK: release API lock
        rmapiLockRelease();
    }

    return rmStatus;
}

/* escape.c's unmap of a mapping it could not record. */
static void _eclipse_rm_unmap_unrecorded(
    NVOS33_PARAMETERS *pParms,
    API_SECURITY_INFO  secInfo
)
{
    NVOS34_PARAMETERS params;

    portMemSet(&params, 0, sizeof(params));
    params.hClient        = pParms->hClient;
    params.hDevice        = pParms->hDevice;
    params.hMemory        = pParms->hMemory;
    params.pLinearAddress = pParms->pLinearAddress;
    params.flags          = pParms->flags;
    Nv04UnmapMemoryWithSecInfo(&params, secInfo);
}

/*
 * escape.c's NV_CTL_DEVICE_ONLY: every RM escape this file serves is a
 * control-device call. A client that sends one on /dev/nvidia<N> gets
 * NV_ERR_INVALID_ARGUMENT, which is what nvidia.ko answers.
 */
#define ECLIPSE_CTL_DEVICE_ONLY(bCtl)          \
{                                              \
    if (!(bCtl))                               \
    {                                          \
        rmStatus = NV_ERR_INVALID_ARGUMENT;    \
        goto done;                             \
    }                                          \
}

static NV_STATUS _eclipse_rm_frontend_dispatch(
    NvU32 cmd,
    void *pData,
    NvU32 dataSize,
    NvBool bCtl,
    API_SECURITY_INFO secInfo,
    EclipseRmMmapContext *pMmap)
{
    NV_STATUS rmStatus = NV_ERR_GENERIC;

    switch (cmd)
    {
        case NV_ESC_RM_ALLOC:
        {
            NVOS21_PARAMETERS *pApi = pData;
            NVOS64_PARAMETERS *pApiAccess = pData;
            NvBool bAccessApi = (dataSize == sizeof(NVOS64_PARAMETERS));

            if ((dataSize != sizeof(*pApi)) &&
                (dataSize != sizeof(*pApiAccess)))
            {
                rmStatus = NV_ERR_INVALID_ARGUMENT;
                goto done;
            }

            switch (pApi->hClass)
            {
                case NV01_ROOT:
                case NV01_ROOT_CLIENT:
                case NV01_ROOT_NON_PRIV:
                {
                    ECLIPSE_CTL_DEVICE_ONLY(bCtl);

                    /* Force userspace client allocations to be the _CLIENT class. */
                    pApi->hClass = NV01_ROOT_CLIENT;
                    break;
                }
                case NV01_EVENT:
                case NV01_EVENT_OS_EVENT:
                case NV01_EVENT_KERNEL_CALLBACK:
                case NV01_EVENT_KERNEL_CALLBACK_EX:
                {
                    break;
                }
                default:
                {
                    ECLIPSE_CTL_DEVICE_ONLY(bCtl);
                    break;
                }
            }

            if (!bAccessApi)
            {
                Nv04AllocWithSecInfo(pApi, secInfo);
            }
            else
            {
                Nv04AllocWithAccessSecInfo(pApiAccess, secInfo);
            }
            break;
        }

        case NV_ESC_RM_FREE:
        {
            NVOS00_PARAMETERS *pApi = pData;

            if (dataSize != sizeof(*pApi))
            {
                rmStatus = NV_ERR_INVALID_ARGUMENT;
                goto done;
            }

            ECLIPSE_CTL_DEVICE_ONLY(bCtl);

            Nv01FreeWithSecInfo(pApi, secInfo);

            if (pApi->status == NV_OK &&
                pApi->hObjectOld == pApi->hRoot)
            {
                rm_client_free_os_events(pApi->hRoot);
            }

            break;
        }

        case NV_ESC_RM_CONTROL:
        {
            NVOS54_PARAMETERS *pApi = pData;

            if (dataSize != sizeof(*pApi))
            {
                rmStatus = NV_ERR_INVALID_ARGUMENT;
                goto done;
            }

            ECLIPSE_CTL_DEVICE_ONLY(bCtl);

            /*
             * escape.c's RmGetDeviceFd: these two carry a /dev/nvidia<N> fd
             * inside their parameters, and the RM then takes a reference on
             * that file. There is no file reference to hand it here yet, so
             * they are refused by name rather than run without one.
             */
            if ((pApi->cmd == NV00FD_CTRL_CMD_ATTACH_GPU) ||
                (pApi->cmd == NV00E0_CTRL_CMD_EXPORT_MEM))
            {
                rmStatus = NV_ERR_NOT_SUPPORTED;
                goto done;
            }

            Nv04ControlWithSecInfo(pApi, secInfo);
            break;
        }

        case NV_ESC_RM_DUP_OBJECT:
        {
            NVOS55_PARAMETERS *pApi = pData;

            if (dataSize != sizeof(*pApi))
            {
                rmStatus = NV_ERR_INVALID_ARGUMENT;
                goto done;
            }

            ECLIPSE_CTL_DEVICE_ONLY(bCtl);

            Nv04DupObjectWithSecInfo(pApi, secInfo);
            break;
        }

        case NV_ESC_RM_SHARE:
        {
            NVOS57_PARAMETERS *pApi = pData;

            if (dataSize != sizeof(*pApi))
            {
                rmStatus = NV_ERR_INVALID_ARGUMENT;
                goto done;
            }

            ECLIPSE_CTL_DEVICE_ONLY(bCtl);

            Nv04ShareWithSecInfo(pApi, secInfo);
            break;
        }

        case NV_ESC_RM_MAP_MEMORY:
        {
            nv_ioctl_nvos33_parameters_with_fd *pApi = pData;
            NVOS33_PARAMETERS *pParms;

            if (dataSize != sizeof(*pApi))
            {
                rmStatus = NV_ERR_INVALID_ARGUMENT;
                goto done;
            }

            pParms = &pApi->params;

            ECLIPSE_CTL_DEVICE_ONLY(bCtl);

            /* A caller that cannot record the mapping must not have it made. */
            if (pMmap == NULL)
            {
                rmStatus = NV_ERR_NOT_SUPPORTED;
                goto done;
            }

            // Don't allow userspace to override the caching type
            pParms->flags = FLD_SET_DRF(OS33, _FLAGS, _CACHING_TYPE, _DEFAULT, pParms->flags);
            Nv04MapMemoryWithSecInfo(pParms, secInfo);

            if (pParms->status == NV_OK)
            {
                pParms->status = _eclipse_rm_create_mmap_context(pParms->hClient,
                                 pParms->hDevice, pParms->hMemory,
                                 pParms->pLinearAddress, pParms->length, pMmap);
                if (pParms->status != NV_OK)
                {
                    portMemSet(pMmap, 0, sizeof(*pMmap));
                    _eclipse_rm_unmap_unrecorded(pParms, secInfo);
                }
            }
            break;
        }

        case NV_ESC_RM_UNMAP_MEMORY:
        {
            NVOS34_PARAMETERS *pApi = pData;

            if (dataSize != sizeof(*pApi))
            {
                rmStatus = NV_ERR_INVALID_ARGUMENT;
                goto done;
            }

            ECLIPSE_CTL_DEVICE_ONLY(bCtl);

            Nv04UnmapMemoryWithSecInfo(pApi, secInfo);
            break;
        }

        default:
        {
            rmStatus = NV_ERR_NOT_SUPPORTED;
            goto done;
        }
    }

    rmStatus = NV_OK;
done:
    return rmStatus;
}

/*
 * OS events.
 *
 * A client registers an fd of its own with ALLOC_OS_EVENT and then
 * allocates an NV01_EVENT_OS_EVENT whose data is that fd number;
 * osUserHandleToKernelPtr trades the number for the registration, which the
 * RM keeps in the notification's Data. The registrations and the queue each
 * fd keeps are Rust's (frontend/events.rs and the kernel side). What is here
 * is os.c's half: the walk over an object's notifications, which decides
 * which of them an RM event reaches and with what, and hands each OS event
 * that fires to eclipse_rm_frontend_post_event. That looks the registration
 * up and queues the event, or drops it if the client has freed it.
 *
 * Linux also runs the kernel callbacks from here (NV01_EVENT_KERNEL_CALLBACK
 * and _EX). This port never has, and the RM registers some of its own on
 * every boot (mem_mgr.c, mem_utils.c, kern_disp.c), so running them now
 * would change what the RM does with the hatch off. They are still not run.
 *
 * The RM calls these with the RM gate held (frontend.rs), like everything
 * else in it.
 */

/*
 * osNotifyEvent (os.c). Answers NV_OK whatever the notification's type:
 * that is what this port always answered, and timer.c, kernel_gsp.c and
 * event.c act on it. Linux answers NV_ERR_GENERIC for a type it does not
 * know.
 */
NV_STATUS osNotifyEvent(
    OBJGPU             *pGpu,
    PEVENTNOTIFICATION  NotifyEvent,
    NvU32               Method,
    NvU32               Data,
    NV_STATUS           Status
)
{
    (void) pGpu;
    (void) Method;
    (void) Data;
    (void) Status;

    if ((NotifyEvent != NULL) && (NotifyEvent->NotifyType == NV01_EVENT_OS_EVENT))
    {
        eclipse_rm_frontend_post_event(NotifyEvent->Data,
                                       NotifyEvent->hEvent,
                                       NotifyEvent->NotifyIndex,
                                       0, 0,
                                       NotifyEvent->bEventDataRequired);
    }

    return NV_OK;
}

/* osEventNotificationWithInfo (os.c). */
NV_STATUS osEventNotificationWithInfo
(
    OBJGPU   *pGpu,
    PEVENTNOTIFICATION pNotifyEvent,
    NvU32 notifyIndex,
    NvU32 info32,
    NvU16 info16,
    void * pEventData,
    NvU32 eventDataSize
)
{
    (void) pGpu;
    (void) pEventData;
    (void) eventDataSize;

    // walk this object's event list and find any matches for this specific notify
    for (; pNotifyEvent; pNotifyEvent = pNotifyEvent->Next)
    {
        // notifyIndex must match if request isn't for all
        if ((notifyIndex != OS_EVENT_NOTIFICATION_INDEX_ALL) &&
            (pNotifyEvent->NotifyIndex != notifyIndex))
        {
            continue;
        }

        switch (pNotifyEvent->NotifyType)
        {
            case NV_EVENT_BUFFER_BIND:
            case NV01_EVENT_WIN32_EVENT:
            {
                eclipse_rm_frontend_post_event(pNotifyEvent->Data,
                                               pNotifyEvent->hEvent,
                                               pNotifyEvent->NotifyIndex,
                                               info32, info16,
                                               pNotifyEvent->bEventDataRequired);
                break;
            }

            default:
                break;
        }
    }

    return NV_OK;
}

/* osEventNotification (os.c). */
NV_STATUS osEventNotification
(
    OBJGPU   *pGpu,
    PEVENTNOTIFICATION pNotifyEvent,
    NvU32 notifyIndex,
    void * pEventData,
    NvU32 eventDataSize
)
{
    return osEventNotificationWithInfo(pGpu, pNotifyEvent, notifyIndex, 0, 0,
                                       pEventData, eventDataSize);
}

/* osObjectEventNotification (os.c). */
NV_STATUS osObjectEventNotification
(
    NvHandle            hClient,
    NvHandle            hObject,
    NvU32               hClass,
    PEVENTNOTIFICATION  pNotifyEvent,
    NvU32               notifyIndex,
    void                *pEventData,
    NvU32               eventDataSize
)
{
    (void) hClient;
    (void) hObject;
    (void) hClass;
    (void) pEventData;
    (void) eventDataSize;

    // walk this object's event list and find any matches for this specific notify
    for (; pNotifyEvent; pNotifyEvent = pNotifyEvent->Next)
    {
        // notifyIndex must match if request isn't for all
        if ((notifyIndex != OS_EVENT_NOTIFICATION_INDEX_ALL) &&
            (pNotifyEvent->NotifyIndex != notifyIndex))
        {
            continue;
        }

        if (pNotifyEvent->NotifyType == NV01_EVENT_OS_EVENT)
        {
            eclipse_rm_frontend_post_event(pNotifyEvent->Data,
                                           pNotifyEvent->hEvent,
                                           pNotifyEvent->NotifyIndex,
                                           0, 0,
                                           pNotifyEvent->bEventDataRequired);
        }
    }

    return NV_OK;
}

/*
 * Client tokens.
 *
 * os.c's four security-token functions, transliterated. When a user-mode
 * caller allocates a root client, rmclientConstruct caches two tokens on it
 * (PDB_PROP_SYS_VALIDATE_CLIENT_HANDLE, on by default): a security token,
 * the euid and pid that osValidateClientTokens later checks callers
 * against, and a uid token that _registerUserInfo files the client's user
 * under. The RM's own kernel clients skip both, and they were the only
 * clients until /dev/nvidiactl, so the stubs that stood in os_boundary.rs
 * answered NULL and nothing noticed. With /dev/nvidiactl, _registerUserInfo
 * refused that NULL uid token with NV_ERR_INVALID_ARGUMENT, every root
 * client a user-mode caller asked for failed, admin or not, and NVML
 * reported "Failed to initialize NVML: Unknown Error".
 *
 * The RM frees these tokens with portMemFree, so they are allocated here
 * with portMemAllocNonPaged and not on the Rust side.
 *
 * os_get_euid and os_get_current_process answer 0 for every caller for now,
 * so every client carries the same token and osValidateClientTokens never
 * refuses one. What keeps one open /dev/nvidiactl off another's clients is
 * the OS-info check rmclientValidate makes instead, under
 * PDB_PROP_SYS_VALIDATE_CLIENT_HANDLE_STRICT (also on by default), against
 * the clientOSInfo every call here carries.
 */
typedef struct
{
    NvU32 euid;
    NvU32 pid;
} TOKEN_USER, *PTOKEN_USER;

PSECURITY_TOKEN osGetSecurityToken(void)
{
    TOKEN_USER *pTokenUser;

    pTokenUser = portMemAllocNonPaged(sizeof(TOKEN_USER));
    if (pTokenUser == NULL)
    {
        return NULL;
    }
    if (os_get_euid(&pTokenUser->euid) != NV_OK)
    {
        portMemFree(pTokenUser);
        return NULL;
    }

    pTokenUser->pid = os_get_current_process();

    return (PSECURITY_TOKEN)pTokenUser;
}

PUID_TOKEN osGetCurrentUidToken(void)
{
    NvU32 *pUidToken;

    pUidToken = portMemAllocNonPaged(sizeof(NvU32));
    if (pUidToken == NULL)
    {
        return NULL;
    }
    if (os_get_euid(pUidToken) != NV_OK)
    {
        portMemFree(pUidToken);
        return NULL;
    }

    return (PUID_TOKEN)pUidToken;
}

/* NV_OK when either the euid or the pid matches, as on Linux. */
NV_STATUS osValidateClientTokens(
    PSECURITY_TOKEN pClientSecurityToken,
    PSECURITY_TOKEN pCurrentSecurityToken)
{
    PTOKEN_USER pClientTokenUser = (PTOKEN_USER)pClientSecurityToken;
    PTOKEN_USER pCurrentTokenUser = (PTOKEN_USER)pCurrentSecurityToken;

    if ((pClientTokenUser == NULL) || (pCurrentTokenUser == NULL))
    {
        return NV_ERR_INVALID_POINTER;
    }

    if ((pClientTokenUser->euid != pCurrentTokenUser->euid) &&
        (pClientTokenUser->pid != pCurrentTokenUser->pid))
    {
        return NV_ERR_INVALID_CLIENT;
    }

    return NV_OK;
}

NvBool osUidTokensEqual(PUID_TOKEN pUidToken1, PUID_TOKEN pUidToken2)
{
    NvU32 *pTokenUser1 = (NvU32 *)pUidToken1;
    NvU32 *pTokenUser2 = (NvU32 *)pUidToken2;

    NV_ASSERT_OR_RETURN((pTokenUser1 != NULL), NV_FALSE);
    NV_ASSERT_OR_RETURN((pTokenUser2 != NULL), NV_FALSE);

    return (*pTokenUser1 == *pTokenUser2) ? NV_TRUE : NV_FALSE;
}

/* The security info of a user-mode client's call, as RmIoctl builds it. */
static API_SECURITY_INFO _eclipse_rm_frontend_sec_info(void *pOsInfo, NvU32 bAdmin)
{
    API_SECURITY_INFO secInfo;

    portMemSet(&secInfo, 0, sizeof(secInfo));
    secInfo.privLevel = bAdmin ? RS_PRIV_LEVEL_USER_ROOT : RS_PRIV_LEVEL_USER;
    secInfo.paramLocation = PARAM_LOCATION_USER;
    secInfo.pProcessToken = NULL;
    secInfo.gpuOsInfo = NULL;
    secInfo.clientOSInfo = pOsInfo;
    return secInfo;
}

/*
 * One RM escape from a user-mode client. `pData` is the kernel copy of the
 * ioctl payload (the kernel side copied it in and copies it back out, like
 * nvidia_ioctl); pointers INSIDE it are still user pointers, which the RM
 * reaches through os_memcpy_from_user. `pOsInfo` identifies the open
 * /dev/nvidiactl: every client allocated through it is filed under it, and
 * eclipse_rm_frontend_release frees them when it closes.
 *
 * `pMmap` is where RM_MAP_MEMORY leaves the mapping it made, for the kernel
 * side to record (bValid set only then). A caller with nowhere to record one
 * passes NULL, and RM_MAP_MEMORY is then refused before anything is mapped.
 *
 * The return value is the escape's own status (NV_OK = the payload's
 * `status` field carries the RM's answer), exactly as RmIoctl's.
 */
NV_STATUS eclipse_rm_frontend_escape(
    NvU32 cmd,
    void *pData,
    NvU32 dataSize,
    void *pOsInfo,
    NvU32 bCtl,
    NvU32 bAdmin,
    EclipseRmMmapContext *pMmap)
{
    NV_STATUS rmStatus;
    THREAD_STATE_NODE threadState;
    API_SECURITY_INFO secInfo;

    if ((pData == NULL) || (pOsInfo == NULL))
    {
        return NV_ERR_INVALID_ARGUMENT;
    }

    if (pMmap != NULL)
    {
        portMemSet(pMmap, 0, sizeof(*pMmap));
    }

    secInfo = _eclipse_rm_frontend_sec_info(pOsInfo, bAdmin);

    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);

    rmStatus = gpumgrThreadEnableExpandedGpuVisibility();
    if (rmStatus == NV_OK)
    {
        s_bInCall = NV_TRUE;
        rmStatus = _eclipse_rm_frontend_dispatch(cmd, pData, dataSize,
                                                 bCtl ? NV_TRUE : NV_FALSE,
                                                 secInfo, pMmap);
        /* A window this call left unused goes with it (see s_pending). */
        s_pending.bValid = NV_FALSE;
        s_bInCall = NV_FALSE;
        gpumgrThreadDisableExpandedGpuVisibility();
    }

    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
    return rmStatus;
}

/*
 * The kernel side could not record the mapping RM_MAP_MEMORY just made on
 * the file the client named (nv_add_mapping_context_to_file failed): what
 * escape.c does when rm_create_mmap_context fails, with `why` as the call's
 * status. `pData` is that RM_MAP_MEMORY's payload, as the escape left it.
 */
void eclipse_rm_frontend_map_unrecorded(
    void *pData,
    NvU32 dataSize,
    void *pOsInfo,
    NvU32 bAdmin,
    NV_STATUS why)
{
    nv_ioctl_nvos33_parameters_with_fd *pApi = pData;
    THREAD_STATE_NODE threadState;

    if ((pData == NULL) || (pOsInfo == NULL) || (dataSize != sizeof(*pApi)))
    {
        return;
    }

    pApi->params.status = why;

    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);
    if (gpumgrThreadEnableExpandedGpuVisibility() == NV_OK)
    {
        _eclipse_rm_unmap_unrecorded(&pApi->params,
                                     _eclipse_rm_frontend_sec_info(pOsInfo, bAdmin));
        gpumgrThreadDisableExpandedGpuVisibility();
    }
    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
}

/*
 * The close of one /dev/nvidiactl: osapi.c's RmFreeUnusedClients, which
 * rm_cleanup_file_private runs under the API lock. Every client filed under
 * `pOsInfo` is disabled and then freed with the whole tree under it, so a
 * process that dies without RM_FREE leaves nothing behind in the RM.
 *
 * Linux may defer the actual free to a work item
 * (bUseDeferredClientListFree); Eclipse has no RM work queue to defer to, so
 * it frees immediately, which is the `!bUseDeferredClientListFree` branch.
 */
void eclipse_rm_frontend_release(void *pOsInfo)
{
    THREAD_STATE_NODE threadState;
    NvHandle *pClientList = NULL;
    NvU32 numClients = 0;
    RM_API *pRmApi;
    NV_STATUS status;

    if (pOsInfo == NULL)
    {
        return;
    }

    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);
    if (gpumgrThreadEnableExpandedGpuVisibility() != NV_OK)
    {
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return;
    }

    if (rmapiLockAcquire(API_LOCK_FLAGS_NONE, RM_LOCK_MODULES_OSAPI) == NV_OK)
    {
        status = rmapiGetClientHandlesFromOSInfo(pOsInfo, &pClientList, &numClients);
        if ((status == NV_OK) && (numClients != 0))
        {
            pRmApi = rmapiGetInterface(RMAPI_API_LOCK_INTERNAL);
            pRmApi->DisableClients(pRmApi, pClientList, numClients);
            serverFreeDisabledClients(&g_resServ, RM_LOCK_STATES_API_LOCK_ACQUIRED, 0);
        }
        if (pClientList != NULL)
        {
            portMemFree(pClientList);
        }
        rmapiLockRelease();
    }

    gpumgrThreadDisableExpandedGpuVisibility();
    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
}

/*
 * NV_VERSION_STRING of the RM this kernel carries. CHECK_VERSION_STR
 * compares the user-mode driver's string against it; the comparison itself
 * is in Rust (frontend.rs), where it can be tested without the RM.
 */
const char *eclipse_rm_frontend_version(void)
{
    return NV_VERSION_STRING;
}

/*
 * The RM's id for a GPU (pGpu->gpuId, gpuGenerate32BitId of its PCI
 * location), which CARD_INFO reports and ATTACH_GPUS_TO_FD and the
 * NV0000 GPU controls take back. NV0000_CTRL_GPU_INVALID_ID when the
 * instance names no GPU.
 */
NvU32 eclipse_rm_frontend_gpu_id(NvU32 gpuInstance)
{
    THREAD_STATE_NODE threadState;
    OBJGPU *pGpu;
    NvU32 gpuId = NV0000_CTRL_GPU_INVALID_ID;

    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);
    if (gpumgrThreadEnableExpandedGpuVisibility() == NV_OK)
    {
        pGpu = gpumgrGetGpu(gpuInstance);
        if (pGpu != NULL)
        {
            gpuId = pGpu->gpuId;
        }
        gpumgrThreadDisableExpandedGpuVisibility();
    }
    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
    return gpuId;
}

/*
 * The instance of an OBJGPU, for osIsGpuAccessible (os_boundary.rs), which
 * answers from the instances the calling process has open.
 */
NvU32 eclipse_rm_frontend_gpu_instance(void *pGpu)
{
    if (pGpu == NULL)
    {
        return NV_U32_MAX;
    }
    return ((OBJGPU *)pGpu)->gpuInstance;
}

/*
 * sizeof() of the nv-ioctl.h payloads the Rust side lays out by hand (and
 * the offset of the one field it reads from RM_MAP_MEMORY's, the event
 * RM_GET_EVENT_DATA copies out, and the size of the mapping record it shares
 * with this file), in the order of `LAYOUT` (frontend.rs). The kernel side compares them
 * with its own `size_of` before serving the first ioctl, so a header that
 * moves under a re-pinned submodule refuses the front end instead of
 * writing the wrong bytes back to a client.
 */
NvU32 eclipse_rm_frontend_layout(NvU32 which)
{
    switch (which)
    {
        case 0: return sizeof(nv_ioctl_xfer_t);
        case 1: return sizeof(nv_ioctl_card_info_t);
        case 2: return sizeof(nv_ioctl_rm_api_version_t);
        case 3: return sizeof(nv_ioctl_sys_params_t);
        case 4: return sizeof(nv_ioctl_register_fd_t);
        case 5: return sizeof(nv_ioctl_wait_open_complete_t);
        case 6: return sizeof(nv_ioctl_status_code_t);
        case 7: return sizeof(nv_ioctl_nvos33_parameters_with_fd);
        case 8: return (NvU32) NV_OFFSETOF(nv_ioctl_nvos33_parameters_with_fd, fd);
        case 9: return sizeof(nv_ioctl_alloc_os_event_t);
        case 10: return sizeof(nv_ioctl_free_os_event_t);
        case 11: return sizeof(NVOS41_PARAMETERS);
        case 12: return sizeof(NvUnixEvent);
        case 13: return sizeof(EclipseRmMmapContext);
        default: return 0;
    }
}
