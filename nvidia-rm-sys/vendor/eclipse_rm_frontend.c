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
 * What is NOT here yet, and answers NV_ERR_NOT_SUPPORTED (which the kernel
 * side turns into EINVAL, like an unknown escape on Linux): RM_MAP_MEMORY /
 * RM_UNMAP_MEMORY (they need mmap on the fd), RM_ALLOC_MEMORY and
 * RM_VID_HEAP_CONTROL (OS descriptors: pinning user pages), the OS events
 * (ALLOC_OS_EVENT, GET_EVENT_DATA), and the two controls that carry a device
 * fd inside their parameters (NV00FD_CTRL_CMD_ATTACH_GPU,
 * NV00E0_CTRL_CMD_EXPORT_MEM). The nv.c-level escapes (CARD_INFO,
 * CHECK_VERSION_STR, ATTACH_GPUS_TO_FD, REGISTER_FD, ...) are not RM calls on
 * Linux either and live on the Rust side (linux-object/src/fs/devfs/
 * nvidia.rs).
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
#include "ctrl/ctrl0000/ctrl0000gpu.h" /* NV0000_CTRL_GPU_INVALID_ID */
#include "ctrl/ctrl00fd.h"     /* NV00FD_CTRL_CMD_ATTACH_GPU */
#include "ctrl/ctrl00e0.h"     /* NV00E0_CTRL_CMD_EXPORT_MEM */

extern RsServer g_resServ;

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
    API_SECURITY_INFO secInfo)
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
 * One RM escape from a user-mode client. `pData` is the kernel copy of the
 * ioctl payload (the kernel side copied it in and copies it back out, like
 * nvidia_ioctl); pointers INSIDE it are still user pointers, which the RM
 * reaches through os_memcpy_from_user. `pOsInfo` identifies the open
 * /dev/nvidiactl: every client allocated through it is filed under it, and
 * eclipse_rm_frontend_release frees them when it closes.
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
    NvU32 bAdmin)
{
    NV_STATUS rmStatus;
    THREAD_STATE_NODE threadState;
    API_SECURITY_INFO secInfo;

    if ((pData == NULL) || (pOsInfo == NULL))
    {
        return NV_ERR_INVALID_ARGUMENT;
    }

    portMemSet(&secInfo, 0, sizeof(secInfo));
    secInfo.privLevel = bAdmin ? RS_PRIV_LEVEL_USER_ROOT : RS_PRIV_LEVEL_USER;
    secInfo.paramLocation = PARAM_LOCATION_USER;
    secInfo.pProcessToken = NULL;
    secInfo.gpuOsInfo = NULL;
    secInfo.clientOSInfo = pOsInfo;

    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);

    rmStatus = gpumgrThreadEnableExpandedGpuVisibility();
    if (rmStatus == NV_OK)
    {
        rmStatus = _eclipse_rm_frontend_dispatch(cmd, pData, dataSize,
                                                 bCtl ? NV_TRUE : NV_FALSE,
                                                 secInfo);
        gpumgrThreadDisableExpandedGpuVisibility();
    }

    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
    return rmStatus;
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
 * sizeof() of the nv-ioctl.h payloads the Rust side lays out by hand, in
 * the order of `LAYOUT` (frontend.rs). The kernel side compares them
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
        default: return 0;
    }
}
