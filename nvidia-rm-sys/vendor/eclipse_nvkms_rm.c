/*
 * The RM half of nvidia-modeset's way into the RM.
 *
 * In Linux nvkms reaches the RM through nvidia_modeset_rm_ops_t
 * (kernel-open/nvidia/nv-modeset-interface.c): `op` is rm_kernel_rmapi_op in
 * arch/nvalloc/unix/src/osapi.c, and `enumerate_gpus` / `open_gpu` walk the
 * Linux device list. That whole directory is NVIDIA's Linux platform layer,
 * which build.rs leaves out, so the pieces nvkms needs are here instead. The
 * Rust hooks in src/nvkms_os.rs call them with the RM gate held.
 */

#include <nv.h>
#include <os/os.h>
#include "gpu/gpu.h"
#include <gpu_mgr/gpu_mgr.h>
#include "rmapi/exports.h"
#include <core/locks.h>
#include <nv-kernel-rmapi-ops.h>
#include <nv-gpu-info.h>
#include "tls/tls.h"
#include <ctrl/ctrl0080/ctrl0080unix.h>
#include <ctrl/ctrl2080/ctrl2080unix.h>
#include <gpu/device/device.h>
#include <gpu/subdevice/subdevice.h>
#include <gpu/mem_mgr/mem_mgr.h>
#include <gpu/mem_mgr/mem_desc.h>
#include <rmapi/rmapi.h>
#include <core/thread_state.h>
#include <platform/sli/sli.h>

/*
 * rm_kernel_rmapi_op, verbatim apart from NV_ENTER/EXIT_RM_RUNTIME (the
 * stack switch Linux needs and this kernel does not). Every case is an
 * RM-core *Kernel entry point that build.rs already compiles.
 */
void eclipse_rm_kernel_rmapi_op(void *ops_cmd)
{
    nvidia_kernel_rmapi_ops_t *ops = ops_cmd;

    switch (ops->op)
    {
        case NV01_FREE:
            Nv01FreeKernel(&ops->params.free);
            break;

        case NV01_ALLOC_MEMORY:
            Nv01AllocMemoryKernel(&ops->params.allocMemory64);
            break;

        case NV04_ALLOC:
            Nv04AllocWithAccessKernel(&ops->params.alloc);
            break;

        case NV04_VID_HEAP_CONTROL:
            Nv04VidHeapControlKernel(ops->params.pVidHeapControl);
            break;

        case NV04_MAP_MEMORY:
        {
            //
            // We need to free NVUAP for MEM_SPACE_USER mappings,
            // since we're not going through the regular path
            //
            nv_usermap_access_params_t *pNvuap = NULL;
            Nv04MapMemoryKernel(&ops->params.mapMemory);
            pNvuap = (nv_usermap_access_params_t *) tlsEntryGet(TLS_ENTRY_ID_MAPPING_CONTEXT);
            if (pNvuap != NULL)
            {
                NV_ASSERT(tlsEntryRelease(TLS_ENTRY_ID_MAPPING_CONTEXT) == 0);
                os_free_mem(pNvuap->memArea.pRanges);
                os_free_mem(pNvuap);
            }
            break;
        }

        case NV04_UNMAP_MEMORY:
            Nv04UnmapMemoryKernel(&ops->params.unmapMemory);
            break;

        case NV04_ALLOC_CONTEXT_DMA:
            Nv04AllocContextDmaKernel(&ops->params.allocContextDma2);
            break;

        case NV04_MAP_MEMORY_DMA:
            Nv04MapMemoryDmaKernel(&ops->params.mapMemoryDma);
            break;

        case NV04_UNMAP_MEMORY_DMA:
            Nv04UnmapMemoryDmaKernel(&ops->params.unmapMemoryDma);
            break;

        case NV04_BIND_CONTEXT_DMA:
            Nv04BindContextDmaKernel(&ops->params.bindContextDma);
            break;

        case NV04_CONTROL:
            Nv04ControlKernel(&ops->params.control);
            break;

        case NV04_DUP_OBJECT:
            Nv04DupObjectKernel(&ops->params.dupObject);
            break;

        case NV04_SHARE:
            Nv04ShareKernel(&ops->params.share);
            break;

        case NV04_ADD_VBLANK_CALLBACK:
            Nv04AddVblankCallbackKernel(&ops->params.addVblankCallback);
            break;
    }

}

/*
 * nvidia_modeset_enumerate_gpus: every GPU the RM has attached, with the gpu
 * id the RM gave it (gpuGenerate32BitId of its domain/bus/device, the same
 * value Linux keeps in nv->gpu_id). `gpu_info` has NV_MAX_GPUS entries.
 */
NvU32 eclipse_rm_enumerate_gpus(nv_gpu_info_t *gpu_info)
{
    NvU32 gpuCount = 0, gpuMask = 0, index = 0, count = 0;
    OBJGPU *pGpu;

    if (gpumgrGetGpuAttachInfo(&gpuCount, &gpuMask) != NV_OK)
        return 0;
    while ((pGpu = gpumgrGetNextGpu(gpuMask, &index)) != NULL) {
        if (count >= NV_MAX_GPUS)
            break;
        portMemSet(&gpu_info[count], 0, sizeof(gpu_info[count]));
        gpu_info[count].gpu_id = pGpu->gpuId;
        gpu_info[count].pci_info.domain = gpuGetDomain(pGpu);
        gpu_info[count].pci_info.bus = gpuGetBus(pGpu);
        gpu_info[count].pci_info.slot = gpuGetDevice(pGpu);
        gpu_info[count].pci_info.function = 0;
        count++;
    }
    return count;
}

/* nvidia_modeset_open_gpu's question, minus the bring-up: is this GPU attached? */
NvBool eclipse_rm_gpu_id_attached(NvU32 gpuId)
{
    return gpumgrGetGpuFromId(gpuId) != NULL;
}

/*
 * The three OS_UNIX controls nvkms cannot allocate a device without
 * (nvAllocDevEvo fails on any of them). Linux answers them in
 * arch/nvalloc/unix/src/unix_console.c and dynamic-power.c, which build.rs
 * leaves out with the rest of the platform layer.
 */

/* The boot console, as Eclipse's driver registered it (src/nvkms_os.rs). */
struct EclipseBootConsole {
    NvU64 phys;
    NvU32 width;
    NvU32 height;
    NvU32 depth;
    NvU32 pitch;
};
NvBool eclipse_boot_console(struct EclipseBootConsole *out);

/*
 * nv_get_screen_info: the firmware framebuffer, if it is this GPU's. Linux
 * takes it from screen_info and keeps it only when it starts exactly at the
 * GPU's BAR1 (NV_IS_CONSOLE_MAPPED), which is where the reserved console
 * region the RM hands out as NV01_MEMORY_FRAMEBUFFER_CONSOLE lives.
 */
static void eclipse_get_screen_info(OBJGPU *pGpu, NvU64 *base, NvU32 *width,
                                    NvU32 *height, NvU32 *depth, NvU32 *pitch,
                                    NvU64 *size)
{
    struct EclipseBootConsole con;

    *base = 0;
    *width = *height = *depth = *pitch = 0;
    *size = 0;
    if (!eclipse_boot_console(&con) || con.phys != gpumgrGetGpuPhysFbAddr(pGpu))
        return;
    *base = con.phys;
    *width = con.width;
    *height = con.height;
    *depth = con.depth;
    *pitch = con.pitch;
    *size = (NvU64)con.height * (NvU64)con.pitch;
}

/* deviceCtrlCmdOsUnixVTGetFBInfo_IMPL of unix_console.c. */
NV_STATUS deviceCtrlCmdOsUnixVTGetFBInfo_IMPL(Device *pDevice,
                                              NV0080_CTRL_OS_UNIX_VT_GET_FB_INFO_PARAMS *pParams)
{
    OBJGPU *pGpu = GPU_RES_GET_GPU(pDevice);
    NvBool bContinue = NV_TRUE;

    if (rmGpuLocksAcquire(GPUS_LOCK_FLAGS_NONE, RM_LOCK_MODULES_FB) == NV_OK)
    {
        portMemSet(pParams, 0, sizeof(*pParams));

        SLI_LOOP_START(SLI_LOOP_FLAGS_NONE)

            MemoryManager *pMemoryManager = GPU_GET_MEMORY_MANAGER(pGpu);

            if ((memmgrGetReservedConsoleMemDesc(pGpu, pMemoryManager) != NULL) && bContinue)
            {
                NvU64 baseAddr, size;
                NvU32 width, height, depth, pitch;

                NV_ASSERT(pParams->width == 0);

                pParams->subDeviceInstance = gpumgrGetSubDeviceInstanceFromGpu(pGpu);

                eclipse_get_screen_info(pGpu, &baseAddr, &width, &height, &depth,
                                        &pitch, &size);

                pParams->baseAddress = baseAddr;
                pParams->size = size;
                pParams->width = (NvU16)width;
                pParams->height = (NvU16)height;
                pParams->depth = (NvU16)depth;
                pParams->pitch = (NvU16)pitch;

                if (baseAddr != 0)
                {
                    bContinue = NV_FALSE;
                }
            }

        SLI_LOOP_END

        rmGpuLocksRelease(GPUS_LOCK_FLAGS_NONE, NULL);
    }

    return NV_OK;
}

/*
 * The console region, described after the GSP boot instead of reserved
 * before it.
 *
 * Linux reserves the firmware framebuffer before kgspInitRm
 * (osinit.c RmSetConsolePreservationParams): ReservedConsoleDispMemSize makes
 * memmgrAllocateConsoleRegion describe FB region 0 as the console, and
 * bPreserveBar1ConsoleEnabled keeps a BAR1 mapping on it. That is what
 * nvidia.console_preserve does, and on a dual RTX 2060 SUPER it costs the
 * picture: the BAR1 window the console is drawn through stops reaching the
 * memory the firmware head scans, and that head freezes on its last frame.
 *
 * Without it the RM leaves BAR1 a linear window onto VRAM, so the firmware
 * framebuffer, which starts at BAR1's base, is VRAM offset 0 (the same
 * relationship eclipse_rm_ce_blit relies on), and every CPU and CE write
 * lands where the head scans. Describing that range as the reserved console
 * memdesc, exactly as memmgrAllocateConsoleRegion_GM107 does, is all
 * NV01_MEMORY_FRAMEBUFFER_CONSOLE and VT_GET_FB_INFO need: nvkms then imports
 * and scans out the memory being drawn, and nothing about the FB layout or
 * BAR1 changes. A GPU that already has a console memdesc (the flag above) is
 * left alone.
 */
NV_STATUS eclipse_nvkms_describe_console(NvU32 gpuId)
{
    struct EclipseBootConsole con;
    THREAD_STATE_NODE threadState;
    MemoryManager *pMemoryManager;
    OBJGPU *pGpu;
    NvU64 size;
    NV_STATUS status;

    if (!eclipse_boot_console(&con) || con.height == 0 || con.pitch == 0)
        return NV_ERR_INVALID_STATE;

    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);
    status = rmapiLockAcquire(API_LOCK_FLAGS_NONE, RM_LOCK_MODULES_FB);
    if (status != NV_OK)
    {
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return status;
    }
    status = rmGpuLocksAcquire(GPUS_LOCK_FLAGS_NONE, RM_LOCK_MODULES_FB);
    if (status != NV_OK)
    {
        rmapiLockRelease();
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return status;
    }

    pGpu = gpumgrGetGpuFromId(gpuId);
    pMemoryManager = (pGpu != NULL) ? GPU_GET_MEMORY_MANAGER(pGpu) : NULL;
    if (pMemoryManager == NULL)
        status = NV_ERR_INVALID_ARGUMENT;
    else if (con.phys != gpumgrGetGpuPhysFbAddr(pGpu))
        status = NV_ERR_NOT_SUPPORTED;
    else if (memmgrGetReservedConsoleMemDesc(pGpu, pMemoryManager) == NULL)
    {
        /* osinit.c's NV_ALIGN_UP(fbConsoleSize, 0x10000). */
        size = NV_ALIGN_UP((NvU64)con.height * (NvU64)con.pitch, 0x10000);
        status = memdescCreate(&pMemoryManager->pReservedConsoleMemDesc, pGpu,
                               size, RM_PAGE_SIZE_64K, NV_TRUE, ADDR_FBMEM,
                               NV_MEMORY_UNCACHED,
                               MEMDESC_FLAGS_SKIP_RESOURCE_COMPUTE);
        if (status == NV_OK)
        {
            memdescDescribe(pMemoryManager->pReservedConsoleMemDesc, ADDR_FBMEM,
                            0, size);
            memdescSetPageSize(pMemoryManager->pReservedConsoleMemDesc,
                               AT_GPU, RM_PAGE_SIZE);
        }
    }

    rmGpuLocksRelease(GPUS_LOCK_FLAGS_NONE, NULL);
    rmapiLockRelease();
    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
    return status;
}

/*
 * deviceCtrlCmdOsUnixVTSwitch_IMPL of unix_console.c without its two side
 * effects: the GC6 console refcount (Eclipse never enters GC6) and
 * RmChangeResMode, which saves and restores the VGA text-mode state of a
 * Linux VT switch. Eclipse has no VGA console to give back: the firmware
 * framebuffer stays the console, and nvkms keeps scanning it out.
 */
NV_STATUS deviceCtrlCmdOsUnixVTSwitch_IMPL(Device *pDevice,
                                           NV0080_CTRL_OS_UNIX_VT_SWITCH_PARAMS *pParams)
{
    (void)pDevice;

    switch (pParams->cmd)
    {
        case NV0080_CTRL_OS_UNIX_VT_SWITCH_CMD_SAVE_VT_STATE:
        case NV0080_CTRL_OS_UNIX_VT_SWITCH_CMD_RESTORE_VT_STATE:
        case NV0080_CTRL_OS_UNIX_VT_SWITCH_CMD_CONSOLE_RESTORED:
            return NV_OK;
        default:
            return NV_ERR_INVALID_ARGUMENT;
    }
}

/*
 * The GC6 blocker refcount nvkms raises before it touches the display. In
 * dynamic-power.c it is osRefGpuAccessNeeded / osUnrefGpuAccessNeeded, which
 * keep the GPU out of runtime power-down (RTD3, GC6). Eclipse never powers a
 * GPU down, so there is nothing to hold off: check the action and succeed.
 */
NV_STATUS subdeviceCtrlCmdOsUnixGc6BlockerRefCnt_IMPL(Subdevice *pSubdevice,
                                                      NV2080_CTRL_OS_UNIX_GC6_BLOCKER_REFCNT_PARAMS *pParams)
{
    (void)pSubdevice;

    if (pParams->action != NV2080_CTRL_OS_UNIX_GC6_BLOCKER_REFCNT_INC &&
        pParams->action != NV2080_CTRL_OS_UNIX_GC6_BLOCKER_REFCNT_DEC)
        return NV_ERR_INVALID_ARGUMENT;
    return NV_OK;
}
