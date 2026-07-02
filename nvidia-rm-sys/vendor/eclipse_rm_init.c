/*
 * OUR code, not NVIDIA's -- the Eclipse-native equivalent of what
 * NVIDIA's real Linux platform layer does in
 * arch/nvalloc/unix/src/osinit.c (osRmInitRm / osInitNvMapping /
 * RmInitAdapter), which Eclipse doesn't vendor since it's Linux-specific.
 * Every RM function called from here (__nvoc_objCreate_OBJSYS,
 * rmapiInitialize, gpumgrAllocGpuInstance, rmGpuLockAlloc,
 * gpumgrCreateDevice, gpumgrAttachGpu, gpuEncodeDomainBusDevice) is real,
 * unmodified NVIDIA code already vendored via build.rs -- this file only
 * sequences the calls and packages Eclipse's own PCI/BAR info into the
 * real GPUATTACHARG struct, the same way osInitNvMapping does from
 * nv_state_t.
 *
 * NOT called yet: REGISTER_ALL_HALS(), the rmconfig-generated HAL
 * registration macro osRmInitRm calls before any of this. It isn't
 * defined anywhere in the portable RM core we vendor (confirmed: our
 * 1038-file build already links with zero undefined symbols without
 * it) -- it appears to be Linux-build-specific glue superseded by the
 * newer per-chip chips2halspec/NVOC halspec construction path that runs
 * automatically inside gpuPostConstruct_IMPL/gpuBindHalLegacy_IMPL
 * during attach itself. Left out for now; revisit if GPU attach fails
 * specifically on missing HAL bindings.
 */
#include "gpu_mgr/gpu_mgr.h"
#include "gpu/gpu.h"
#include "core/system.h"
#include "core/locks.h"
#include "rmapi/rmapi.h"
#include "core/thread_state.h"

/*
 * Constructs the real OBJSYS singleton and the RM resource server.
 * Call exactly once, before eclipse_rm_attach_gpu.
 */
NV_STATUS eclipse_rm_init_core(void)
{
    OBJSYS *pSys = NULL;
    NV_STATUS status;

    status = __nvoc_objCreate_OBJSYS(&pSys, NULL, 0);
    if (status != NV_OK)
    {
        return status;
    }

    status = rmapiInitialize();
    if (status != NV_OK)
    {
        return status;
    }

    threadStateInitSetupFlags(THREAD_STATE_SETUP_FLAGS_ENABLED |
                              THREAD_STATE_SETUP_FLAGS_TIMEOUT_ENABLED |
                              THREAD_STATE_SETUP_FLAGS_SLI_LOGIC_ENABLED |
                              THREAD_STATE_SETUP_FLAGS_DO_NOT_INCLUDE_SLEEP_TIME_ENABLED);

    return NV_OK;
}

/*
 * Real equivalent of osInitNvMapping's GPU-attach sequence, packaging
 * Eclipse's own PCI/BAR discovery (from drivers/src/display/nvidia.rs)
 * into the real GPUATTACHARG struct instead of nv_state_t.
 *
 * bar0_phys/bar0_virt/bar0_len: BAR0, the MMIO register aperture.
 * bar1_phys/bar1_len: BAR1, the framebuffer/VRAM aperture (not mapped
 * by Eclipse ahead of time the way BAR0 is, matching
 * osInitNvMapping's own `fbBaseAddr = (GPUHWREG*) 0 // not mapped`).
 */
NV_STATUS eclipse_rm_attach_gpu(
    NvU32 domain,
    NvU8  bus,
    NvU8  device,
    NvU64 bar0_phys,
    void *bar0_virt,
    NvU64 bar0_len,
    NvU64 bar1_phys,
    NvU64 bar1_len,
    NvU32 *pDeviceInstance)
{
    NvU32 gpuInstance = 0;
    NvU32 deviceInstance = 0;
    GPUATTACHARG gpuAttachArg;
    NV_STATUS status;

    status = gpumgrAllocGpuInstance(&gpuInstance);
    if (status != NV_OK)
    {
        return status;
    }

    status = rmGpuLockAlloc(gpuInstance);
    if (status != NV_OK)
    {
        return status;
    }

    status = gpumgrCreateDevice(&deviceInstance, NVBIT(gpuInstance), NULL);
    if (status != NV_OK)
    {
        rmGpuLockFree(gpuInstance);
        return status;
    }

    portMemSet(&gpuAttachArg, 0, sizeof(gpuAttachArg));
    gpuAttachArg.devPhysAddr           = bar0_phys;
    gpuAttachArg.regBaseAddr           = (GPUHWREG *)bar0_virt;
    gpuAttachArg.regLength             = bar0_len;
    gpuAttachArg.fbPhysAddr            = bar1_phys;
    gpuAttachArg.fbBaseAddr            = (GPUHWREG *)0; /* not mapped, same as real driver */
    gpuAttachArg.fbLength              = bar1_len;
    gpuAttachArg.instPhysAddr          = 0;
    gpuAttachArg.instBaseAddr          = (GPUHWREG *)0;
    gpuAttachArg.instLength            = 0;
    gpuAttachArg.intLine               = 0;
    gpuAttachArg.nvDomainBusDeviceFunc = gpuEncodeDomainBusDevice(domain, bus, device);
    // NV_IOVA_DOMAIN_NONE/NV0000_CTRL_NO_NUMA_NODE live in headers this
    // build doesn't otherwise need to pull in (nv.h is Linux-specific;
    // the ctrl0000 NUMA sentinel wasn't worth a whole extra header for
    // two fields) -- both are just "not applicable" sentinels for a
    // plain PCIe GPU with no SMMU/NUMA, using this codebase's universal
    // NvU32/NvS32 "none" conventions (all-ones / -1) instead.
    gpuAttachArg.iovaspaceId           = 0xFFFFFFFFu;
    gpuAttachArg.cpuNumaNodeId         = -1;
    gpuAttachArg.pOsAttachArg          = NULL;

    status = gpumgrAttachGpu(deviceInstance, &gpuAttachArg);
    if (status != NV_OK)
    {
        gpumgrDestroyDevice(deviceInstance);
        rmGpuLockFree(gpuInstance);
        return status;
    }

    if (pDeviceInstance != NULL)
    {
        *pDeviceInstance = deviceInstance;
    }

    return NV_OK;
}
