/*
 * OUR code, not NVIDIA's -- the Eclipse-native equivalent of what
 * NVIDIA's real Linux platform layer does around RM bring-up
 * (kernel-open's rm_init_rm entry + arch/nvalloc/unix/src/osinit.c's
 * osRmInitRm / osInitNvMapping / RmInitAdapter), which Eclipse doesn't
 * vendor since it's Linux-specific. Every RM function called from here
 * (tlsInitialize, coreInitializeRm, REGISTER_ALL_HALS,
 * threadStateInitSetupFlags, rmapiLockAcquire/Release,
 * gpumgrAllocGpuInstance, rmGpuLockAlloc, gpumgrCreateDevice,
 * gpumgrAttachGpu, gpuEncodeDomainBusDevice) is real, unmodified NVIDIA
 * code already vendored via build.rs -- this file only sequences the
 * calls and packages Eclipse's own PCI/BAR info into the real
 * GPUATTACHARG struct, the same way osInitNvMapping does from
 * nv_state_t.
 *
 * HISTORY -- the original version of this file hand-rolled init as
 * __nvoc_objCreate_OBJSYS + rmapiInitialize + threadStateInitSetupFlags,
 * which hung real hardware solid on first use: constructing OBJSYS runs
 * portMemAllocNonPaged (and much more) on an nvport runtime that
 * portInitialize() had never set up (uninitialized global-allocator
 * state and tracking locks -- undefined behavior, in practice a wedge
 * with no output), and then called rmapiInitialize a SECOND time (the
 * real sysConstruct_IMPL already calls it internally, system.c). The
 * real, portable, already-vendored entry point that does everything in
 * the right order is coreInitializeRm() (system.c): portInitialize ->
 * NVLOG_INIT -> DBG_INIT -> objCreate(OBJSYS) -> nvAssertInit, with
 * sysConstruct_IMPL itself then running osRmInitRm, RmInitCpuInfo,
 * rmLocksAlloc, threadStateGlobalAlloc, rmapiInitialize and the rest.
 * tlsInitialize() (tls.c, also real+vendored) must run first, exactly
 * like NVIDIA's own kernel-open interface layer does, since the rmapi
 * lock allocates TLS entries.
 *
 * REGISTER_ALL_HALS mystery, RESOLVED: it's a real generated
 * `static NV_INLINE` function in src/nvidia/generated/g_hal_register.h
 * (not a Linux-only macro as previously hypothesized), and TU106's HAL
 * module is the third thing it registers. sysConstruct_IMPL calls the
 * platform hook osRmInitRm() (which Linux implements in osinit.c to run
 * REGISTER_ALL_HALS + threadStateInitSetupFlags); Eclipse's equivalent
 * implementation of that hook lives right here in this file now,
 * replacing the old os_boundary.rs stub that returned NOT_SUPPORTED
 * (which would have failed OBJSYS construction cleanly).
 */
#include "gpu_mgr/gpu_mgr.h"
#include "gpu/gpu.h"
#include "core/system.h"
#include "core/locks.h"
#include "rmapi/rmapi.h"
#include "core/thread_state.h"
#include "gpu/gsp/kernel_gsp.h"
#include "os/os.h"
#include "tls/tls.h"
#include "g_hal_register.h"

/*
 * Real, vendored (src/nvidia/src/kernel/core/system.c), but its declaring
 * header (rmosxfac.h) is not part of the vendored include surface --
 * declared here directly instead.
 */
extern NV_STATUS coreInitializeRm(void);

/*
 * TEMPORARY: checkpoint tracing to find exactly where real hardware hung
 * during the first-ever exercise of this call chain (gpustep5 froze the
 * machine solid, confirmed on real Turing hardware -- no output at all
 * reached /proc/gpustep5, so the hang is somewhere in here or deeper).
 * nv_printf (vendor/glue.c) forwards straight to Eclipse's synchronous
 * console logger, so whatever printed last before a freeze pinpoints the
 * exact call that never returns. Remove once the real hang is found.
 */
extern int nv_printf(unsigned int debuglevel, const char *printf_format, ...);
#define ECLIPSE_TRACE(msg) nv_printf(0, "[eclipse-rm-trace] " msg "\n")

/*
 * Eclipse's implementation of the osRmInitRm platform hook, called from
 * the real sysConstruct_IMPL (system.c) during OBJSYS construction --
 * the portable subset of what Linux's osinit.c version does: register
 * every generated HAL module (TU10X first -- the user's TU106 included)
 * and set up the default ThreadState flags (same four flags as the real
 * Linux implementation). The Linux-only parts (module-param registry
 * import, PCIe-Gen3 regkey plumbing, S0ix checks, nvlog re-init) have no
 * Eclipse equivalent yet and are deliberately omitted.
 */
NV_STATUS osRmInitRm(void)
{
    NV_STATUS status;

    ECLIPSE_TRACE("osRmInitRm: before REGISTER_ALL_HALS");
    status = REGISTER_ALL_HALS();
    ECLIPSE_TRACE("osRmInitRm: after REGISTER_ALL_HALS");
    if (status != NV_OK)
    {
        return status;
    }

    threadStateInitSetupFlags(THREAD_STATE_SETUP_FLAGS_ENABLED |
                              THREAD_STATE_SETUP_FLAGS_TIMEOUT_ENABLED |
                              THREAD_STATE_SETUP_FLAGS_SLI_LOGIC_ENABLED |
                              THREAD_STATE_SETUP_FLAGS_DO_NOT_INCLUDE_SLEEP_TIME_ENABLED);
    ECLIPSE_TRACE("osRmInitRm: done");

    return NV_OK;
}

/*
 * Brings up the real RM core exactly the way NVIDIA's own driver does:
 * tlsInitialize() (as kernel-open's interface layer does before touching
 * RM) followed by coreInitializeRm() (system.c) -- which runs
 * portInitialize, NVLOG_INIT, DBG_INIT, then constructs OBJSYS, whose
 * real constructor performs the entire remaining init sequence
 * (osRmInitRm above, RmInitCpuInfo, rmLocksAlloc, threadStateGlobalAlloc,
 * rmapiInitialize, ...). Call exactly once, before eclipse_rm_attach_gpu.
 */
NV_STATUS eclipse_rm_init_core(void)
{
    NV_STATUS status;

    ECLIPSE_TRACE("init_core: before tlsInitialize");
    status = tlsInitialize();
    ECLIPSE_TRACE("init_core: after tlsInitialize");
    if (status != NV_OK)
    {
        return status;
    }

    ECLIPSE_TRACE("init_core: before coreInitializeRm");
    status = coreInitializeRm();
    ECLIPSE_TRACE("init_core: after coreInitializeRm, done");
    return status;
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
    NvU32 gpuId;
    NvU64 gpuDomainBusDevice;
    GPUATTACHARG gpuAttachArg;
    NV_STATUS status;

    /*
     * gpumgrAttachGpu's first line is
     * NV_ASSERT_OR_RETURN(rmapiLockIsOwner(), NV_ERR_INVALID_LOCK_STATE)
     * (gpu_mgr.c) -- the real driver always attaches with the RM API lock
     * held, so take it the same way sysConstruct's own internal callers
     * do (API_LOCK_FLAGS_NONE = exclusive write).
     */
    ECLIPSE_TRACE("attach_gpu: before rmapiLockAcquire");
    status = rmapiLockAcquire(API_LOCK_FLAGS_NONE, RM_LOCK_MODULES_INIT);
    ECLIPSE_TRACE("attach_gpu: after rmapiLockAcquire");
    if (status != NV_OK)
    {
        return status;
    }

    /*
     * Register the GPU in OBJGPUMGR's probedGpus table BEFORE attach, the
     * same thing the real Linux probe (RmInitGpuInfoWithRmApi in
     * arch/nvalloc/unix/src/osinit.c) does. Without this, gpumgrAttachGpu's
     * very first child call (_gpumgrCreateGpu -> gpumgrGetRegisteredIds)
     * finds no probedGpus entry matching this DBDF and returns 0x57
     * (NV_ERR_OBJECT_NOT_FOUND) before OBJGPU is even constructed -- which
     * is exactly the error real hardware just reported. gpuId and
     * gpuDomainBusDevice are derived from the same (domain,bus,device) as
     * the attach arg's nvDomainBusDeviceFunc, matching osinit.c's use of
     * gpuGenerate32BitId / gpuEncodeDomainBusDevice.
     */
    gpuId              = gpuGenerate32BitId(domain, bus, device);
    gpuDomainBusDevice = gpuEncodeDomainBusDevice(domain, bus, device);
    ECLIPSE_TRACE("attach_gpu: before gpumgrRegisterGpuId");
    status = gpumgrRegisterGpuId(gpuId, gpuDomainBusDevice);
    ECLIPSE_TRACE("attach_gpu: after gpumgrRegisterGpuId");
    if (status != NV_OK)
    {
        rmapiLockRelease();
        return status;
    }

    /*
     * The whole attach path needs "expanded GPU visibility" on this thread,
     * exactly as RmInitAdapter enables it around osInitNvMapping
     * (osinit.c:2039, comment: "Initialization path requires expanded GPU
     * visibility in GPUMGR in order to access the GPU undergoing
     * initialization"). Without it, gpumgrGetGpu (gpu_mgr.c:500) refuses to
     * return a GPU whose PDB_PROP_GPU_STATE_INITIALIZED is still false --
     * which it is during attach -- so gpumgrAttachGpu's own
     * `pGpu = gpumgrGetGpu(...)` (gpu_mgr.c:1538) returns NULL and the very
     * next line dereferences it (pGpu->gpuInstance), the real
     * [KERNEL PAGE FAULT] READ @ 0x7cc (offset of gpuInstance in a NULL
     * OBJGPU) hardware just reported at gpu_mgr.c:1540.
     */
    ECLIPSE_TRACE("attach_gpu: before gpumgrThreadEnableExpandedGpuVisibility");
    status = gpumgrThreadEnableExpandedGpuVisibility();
    ECLIPSE_TRACE("attach_gpu: after gpumgrThreadEnableExpandedGpuVisibility");
    if (status != NV_OK)
    {
        gpumgrUnregisterGpuId(gpuId);
        rmapiLockRelease();
        return status;
    }

    ECLIPSE_TRACE("attach_gpu: before gpumgrAllocGpuInstance");
    status = gpumgrAllocGpuInstance(&gpuInstance);
    ECLIPSE_TRACE("attach_gpu: after gpumgrAllocGpuInstance");
    if (status != NV_OK)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        gpumgrUnregisterGpuId(gpuId);
        rmapiLockRelease();
        return status;
    }

    ECLIPSE_TRACE("attach_gpu: before rmGpuLockAlloc");
    status = rmGpuLockAlloc(gpuInstance);
    ECLIPSE_TRACE("attach_gpu: after rmGpuLockAlloc");
    if (status != NV_OK)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        gpumgrUnregisterGpuId(gpuId);
        rmapiLockRelease();
        return status;
    }

    ECLIPSE_TRACE("attach_gpu: before gpumgrCreateDevice");
    status = gpumgrCreateDevice(&deviceInstance, NVBIT(gpuInstance), NULL);
    ECLIPSE_TRACE("attach_gpu: after gpumgrCreateDevice");
    if (status != NV_OK)
    {
        rmGpuLockFree(gpuInstance);
        gpumgrThreadDisableExpandedGpuVisibility();
        gpumgrUnregisterGpuId(gpuId);
        rmapiLockRelease();
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

    /*
     * Pass gpuInstance (from gpumgrAllocGpuInstance), NOT deviceInstance
     * (from gpumgrCreateDevice) -- osinit.c:889 does exactly this:
     * `gpumgrAttachGpu(*pDeviceReference, ...)` where *pDeviceReference is
     * the gpuInstance filled by gpumgrAllocGpuInstance at osinit.c:793.
     * gpumgrAttachGpu constructs the OBJGPU at index gpuInstance and looks
     * it back up by the same gpuInstance (gpu_mgr.c:1531/1538), so the
     * argument must be the gpuInstance, not the device grouping index.
     */
    ECLIPSE_TRACE("attach_gpu: before gpumgrAttachGpu");
    status = gpumgrAttachGpu(gpuInstance, &gpuAttachArg);
    ECLIPSE_TRACE("attach_gpu: after gpumgrAttachGpu");
    if (status != NV_OK)
    {
        gpumgrDestroyDevice(deviceInstance);
        rmGpuLockFree(gpuInstance);
        gpumgrThreadDisableExpandedGpuVisibility();
        gpumgrUnregisterGpuId(gpuId);
        rmapiLockRelease();
        return status;
    }

    /*
     * Return gpuInstance (not deviceInstance): the GSP-boot path
     * (eclipse_rm_init_gsp) uses this value with gpumgrGetGpu, which is
     * keyed by gpuInstance.
     */
    if (pDeviceInstance != NULL)
    {
        *pDeviceInstance = gpuInstance;
    }

    gpumgrThreadDisableExpandedGpuVisibility();
    rmapiLockRelease();
    ECLIPSE_TRACE("attach_gpu: done, OK");
    return NV_OK;
}

/*
 * Boots GSP-RM via the vendored, unmodified kgspInitRm (kernel_gsp.c).
 * Only the raw gsp.bin bytes are supplied: RM parses the image/signature
 * sections out of pBuf itself (_kgspPrepareGspRmBinaryImage), and
 * self-allocates the Booter Load/Unload ucodes and the RISC-V bootloader
 * stub from BINDATA_ARCHIVE blobs already compiled into this vendored
 * core (kernel_gsp_booter.c's kgspGetBinArchiveBooterLoadUcode_HAL /
 * kernel_gsp.c's kgspGetGspRmBootUcodeStorage_HAL) rather than from any
 * file Eclipse provides -- confirmed those are NOT external-file reads.
 *
 * pGpu->isGspClient defaults to NV_TRUE for any non-Tegra chip under the
 * PF_KERNEL_ONLY RM variant (__nvoc_init_dataField_OBJGPU in
 * g_gpu_nvoc.c), which is what a discrete-GPU build like this one
 * selects, so Eclipse doesn't need to force it on.
 */
NV_STATUS eclipse_rm_init_gsp(NvU32 gpuInstance, const void *pBuf, NvU32 size)
{
    OBJGPU *pGpu;
    KernelGsp *pKernelGsp;
    GSP_FIRMWARE gspFw;
    NV_STATUS status;

    /*
     * Same expanded-visibility requirement as the attach path: the GPU is
     * attached but not yet PDB_PROP_GPU_STATE_INITIALIZED at this point, so
     * gpumgrGetGpu (gpu_mgr.c:500) would return NULL without it. gpuInstance
     * is the value eclipse_rm_attach_gpu returned (it is keyed by
     * gpuInstance, not the device grouping index).
     */
    status = gpumgrThreadEnableExpandedGpuVisibility();
    if (status != NV_OK)
    {
        return status;
    }

    pGpu = gpumgrGetGpu(gpuInstance);
    if (pGpu == NULL)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        return NV_ERR_INVALID_ARGUMENT;
    }

    pKernelGsp = GPU_GET_KERNEL_GSP(pGpu);
    if (pKernelGsp == NULL)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        return NV_ERR_NOT_SUPPORTED;
    }

    portMemSet(&gspFw, 0, sizeof(gspFw));
    gspFw.pBuf = pBuf;
    gspFw.size = size;

    status = kgspInitRm(pGpu, pKernelGsp, &gspFw);
    gpumgrThreadDisableExpandedGpuVisibility();
    return status;
}
