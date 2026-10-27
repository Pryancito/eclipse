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
#include <gpu/bus/kern_bus.h>
#include <mem_mgr/mem.h>
#include <resserv/rs_server.h>
#include <resserv/rs_client.h>

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
 * The firmware framebuffer, which starts at BAR1's base while BAR1 is still
 * the firmware's linear window, is VRAM offset 0 (the same relationship
 * eclipse_rm_ce_blit relies on). Describing that range as the reserved
 * console memdesc, exactly as memmgrAllocateConsoleRegion_GM107 does, is all
 * NV01_MEMORY_FRAMEBUFFER_CONSOLE and VT_GET_FB_INFO need: nvkms then imports
 * and scans out the memory being drawn. The state load puts BAR1 into virtual
 * mode, so the console card now gets this memdesc before it, from
 * eclipse_rm_preserve_bar1_console, which also has kbusInitBar1 map it at
 * BAR1 0; this is the fallback for a card that went without. A GPU that
 * already has a console memdesc is left alone.
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

/* NV_VIRTUAL_FUNCTION_PRIV_BAR1_BLOCK (published/turing/tu102/dev_vm.h). */
#define ECLIPSE_PRIV_BAR1_BLOCK 0x00000F40
/* NV_VIRTUAL_FUNCTION_PRIV_MMU_FAULT_* (same header). */
#define ECLIPSE_PRIV_MMU_FAULT_ADDR_LO 0x00003080
#define ECLIPSE_PRIV_MMU_FAULT_ADDR_HI 0x00003084
#define ECLIPSE_PRIV_MMU_FAULT_INST_LO 0x00003088
#define ECLIPSE_PRIV_MMU_FAULT_INFO    0x00003090
#define ECLIPSE_PRIV_MMU_FAULT_STATUS  0x00003094
/* NV_PFB_PRI_MMU_FAULT_STATUS_VALID, 31:31, write 1 to clear. */
#define ECLIPSE_MMU_FAULT_STATUS_VALID 0x80000000U
/* The GPU's PRI-error sentinel, what a faulted BAR1 reads back. */
#define ECLIPSE_PRI_ERROR(v) (((v) & 0xFFFFFF00U) == 0xBAD0AC00U)

/* Mirrored by `VidmemCheck` in src/nvkms_os.rs. */
struct EclipseVidmemCheck {
    NvU64 vramOffset;      /* where the object starts in VRAM */
    NvU32 bar1Block;       /* the BAR1 bind register, as the GPU has it */
    NvU32 words;           /* 32-bit words compared */
    NvU32 badWords;        /* words whose VRAM differs from the reference */
    NvU32 firstBad;        /* byte offset of the first of them, ~0 for none */
    NvU32 refNonZero;      /* words of the reference that are not zero */
    NvU32 badAfterRepair;  /* bad words left after the repair, ~0 if none ran */
    NvU32 consoleWords;    /* console words sampled, 0 when there is none */
    NvU32 consoleSame;     /* of them, equal through BAR1 and in VRAM */
    NvU32 cpuSame;         /* words the CPU mapping reads equal to the reference */
    NvU32 cpuOnes;         /* words the CPU mapping reads as 0xffffffff */
    NvU8  bar1Physical;    /* the RM maps BAR1 as a linear window */
    NvU8  bar1Rebound;     /* BAR1 was found faulted and rebound here */
    NvU8  pad[2];
    NvU32 cpuFirst;        /* the first word as the CPU mapping reads it */
    NvU32 faultStatus;     /* MMU_FAULT_STATUS when it was found faulted */
    NvU32 faultAddrLo;     /* MMU_FAULT_ADDR_LO: the faulting VA, low half */
    NvU32 faultAddrHi;     /* MMU_FAULT_ADDR_HI */
    NvU32 faultInstLo;     /* MMU_FAULT_INST_LO: whose instance block */
    NvU32 faultInfo;       /* MMU_FAULT_INFO: engine, client, access type */
    NvU32 cpuAfterRebind;  /* the first word through the CPU mapping again */
};

_Static_assert(sizeof(struct EclipseVidmemCheck) == 80,
               "EclipseVidmemCheck layout is shared with src/nvkms_os.rs");

static NV_STATUS eclipse_vram_word(OBJGPU *pGpu, KernelBus *pKernelBus, NvU64 phys,
                                   NvU32 *pData, NvBool bRead)
{
    return kbusMemAccessBar0Window_HAL(pGpu, pKernelBus, phys, pData, 4, bRead, ADDR_FBMEM);
}

/*
 * Whether the CPU's writes into an RM video-memory object reach the VRAM
 * behind it.
 *
 * nvkms writes its LUTs through the CPU mapping nvRmApiMapMemory gave it,
 * which goes through BAR1, and the display engine reads them from VRAM. A
 * BAR1 that does not translate to where the RM put the object leaves the
 * display reading something else, and on Turing an all-zero input or output
 * LUT turns every pixel black while the signal stays up. Here the object is
 * read word by word through the BAR0 PRAMIN window, which reaches VRAM by
 * physical address without going through BAR1, and compared with `want`,
 * what the object must hold, built in kernel memory. With `repair`, words
 * that differ are written into the VRAM through the same window, and read
 * back. Never from `cpu`: a BAR1 mapping that does not reach VRAM reads back
 * all ones, and a LUT of all ones is a white screen. `cpu` (may be NULL) is
 * only compared, to say whether the CPU mapping works.
 *
 * Also sampled, when this GPU drives the boot console: whether the firmware
 * framebuffer at the base of BAR1 is VRAM offset 0, which is what
 * eclipse_nvkms_describe_console and the nvkms console surface assume.
 */
NV_STATUS eclipse_nvkms_check_vidmem(NvU32 gpuId, NvHandle hClient, NvHandle hMemory,
                                     const NvU32 *want, const volatile NvU32 *cpu,
                                     NvU32 size, NvBool repair,
                                     struct EclipseVidmemCheck *pOut)
{
    struct EclipseBootConsole con;
    THREAD_STATE_NODE threadState;
    RsClient *pRsClient;
    Memory *pMemory;
    MEMORY_DESCRIPTOR *pMemDesc;
    KernelBus *pKernelBus;
    OBJGPU *pGpu;
    NV_STATUS status;
    NvU32 i, words, value;

    portMemSet(pOut, 0, sizeof(*pOut));
    pOut->firstBad = ~0U;
    pOut->badAfterRepair = ~0U;
    if (want == NULL || size < 4)
        return NV_ERR_INVALID_ARGUMENT;
    words = size / 4;

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
    if (pGpu == NULL)
    {
        status = NV_ERR_INVALID_ARGUMENT;
        goto done;
    }
    pKernelBus = GPU_GET_KERNEL_BUS(pGpu);
    pOut->bar1Block = GPU_VREG_RD32(pGpu, ECLIPSE_PRIV_BAR1_BLOCK);
    pOut->bar1Physical = kbusIsBar1PhysicalModeEnabled(pKernelBus) ? 1 : 0;

    status = serverGetClientUnderLock(&g_resServ, hClient, &pRsClient);
    if (status == NV_OK)
        status = memGetByHandle(pRsClient, hMemory, &pMemory);
    if (status != NV_OK)
        goto done;
    pMemDesc = memdescGetMemDescFromGpu(pMemory->pMemDesc, pGpu);
    if (memdescGetAddressSpace(pMemDesc) != ADDR_FBMEM ||
        memdescGetSize(pMemDesc) < (NvU64)words * 4)
    {
        status = NV_ERR_INVALID_ARGUMENT;
        goto done;
    }
    pOut->vramOffset = memdescGetPhysAddr(pMemDesc, AT_GPU, 0);
    pOut->words = words;

    for (i = 0; i < words; i++)
    {
        if (want[i] != 0)
            pOut->refNonZero++;
        if (cpu != NULL)
        {
            NvU32 seen = cpu[i];

            if (seen == want[i])
                pOut->cpuSame++;
            if (seen == ~0U)
                pOut->cpuOnes++;
            if (i == 0)
                pOut->cpuFirst = seen;
        }
        status = eclipse_vram_word(pGpu, pKernelBus,
                                   memdescGetPhysAddr(pMemDesc, AT_GPU, (NvU64)i * 4),
                                   &value, NV_TRUE);
        if (status != NV_OK)
            goto done;
        if (value != want[i])
        {
            if (pOut->badWords == 0)
                pOut->firstBad = i * 4;
            pOut->badWords++;
        }
    }

    /*
     * A CPU mapping that reads the PRI-error sentinel is a BAR1 that took an
     * MMU fault nobody serviced. Record what the MMU latched -- the faulting
     * address says which BAR1 offset was touched without a mapping -- and do
     * what the RM's fault handler does on Linux (kern_gmmu_gv100.c): rebind
     * BAR1 and clear the latched fault. Then read the first word again to
     * say whether that brought BAR1 back.
     */
    if (cpu != NULL && ECLIPSE_PRI_ERROR(pOut->cpuFirst))
    {
        pOut->faultStatus = GPU_VREG_RD32(pGpu, ECLIPSE_PRIV_MMU_FAULT_STATUS);
        pOut->faultAddrLo = GPU_VREG_RD32(pGpu, ECLIPSE_PRIV_MMU_FAULT_ADDR_LO);
        pOut->faultAddrHi = GPU_VREG_RD32(pGpu, ECLIPSE_PRIV_MMU_FAULT_ADDR_HI);
        pOut->faultInstLo = GPU_VREG_RD32(pGpu, ECLIPSE_PRIV_MMU_FAULT_INST_LO);
        pOut->faultInfo = GPU_VREG_RD32(pGpu, ECLIPSE_PRIV_MMU_FAULT_INFO);
        if (kbusBar1InstBlkBind_HAL(pGpu, pKernelBus) == NV_OK)
        {
            pOut->bar1Rebound = 1;
            if (pOut->faultStatus & ECLIPSE_MMU_FAULT_STATUS_VALID)
                GPU_VREG_WR32(pGpu, ECLIPSE_PRIV_MMU_FAULT_STATUS,
                              pOut->faultStatus);
        }
        pOut->cpuAfterRebind = cpu[0];
    }

    if (repair && pOut->badWords != 0)
    {
        pOut->badAfterRepair = 0;
        for (i = 0; i < words; i++)
        {
            NvU64 phys = memdescGetPhysAddr(pMemDesc, AT_GPU, (NvU64)i * 4);

            status = eclipse_vram_word(pGpu, pKernelBus, phys, &value, NV_TRUE);
            if (status == NV_OK && value != want[i])
            {
                value = want[i];
                status = eclipse_vram_word(pGpu, pKernelBus, phys, &value, NV_FALSE);
                if (status == NV_OK)
                    status = eclipse_vram_word(pGpu, pKernelBus, phys, &value, NV_TRUE);
                if (status == NV_OK && value != want[i])
                    pOut->badAfterRepair++;
            }
            if (status != NV_OK)
                goto done;
        }
    }

    /*
     * The console sample: a quarter of the way down the screen, where a
     * desktop has something drawn, read through the base of BAR1 and from
     * VRAM. Informational only; the desktop may change between the two.
     */
    if (eclipse_boot_console(&con) && con.phys == gpumgrGetGpuPhysFbAddr(pGpu) &&
        con.height != 0 && con.pitch != 0)
    {
        const NvU32 sample = 256;
        NvU64 off = (NvU64)(con.height / 4) * con.pitch;
        NvP64 va = NvP64_NULL;

        if (off + (NvU64)sample * 4 <= (NvU64)con.height * con.pitch &&
            osMapPciMemoryKernel64(pGpu, con.phys + off, (NvU64)sample * 4,
                                   NV_PROTECT_READABLE, &va,
                                   NV_MEMORY_WRITECOMBINED) == NV_OK && va != NvP64_NULL)
        {
            const volatile NvU32 *bar1 = KERNEL_POINTER_FROM_NvP64(const volatile NvU32 *, va);

            for (i = 0; i < sample; i++)
            {
                if (eclipse_vram_word(pGpu, pKernelBus, off + (NvU64)i * 4, &value,
                                      NV_TRUE) != NV_OK)
                    break;
                pOut->consoleWords++;
                if (bar1[i] == value)
                    pOut->consoleSame++;
            }
            osUnmapPciMemoryKernel64(pGpu, va);
        }
    }
    status = NV_OK;

done:
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
