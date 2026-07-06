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
#include "gpu/gpu_timeout.h"
#include "ctrl/ctrl2080/ctrl2080gpu.h"
#include "ctrl/ctrl2080/ctrl2080fb.h"
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
    if (status != NV_OK)
    {
        return status;
    }

    /*
     * Eclipse diagnostic (not NVIDIA's): make the RM narrate the attach
     * chain. In a release build nvlog_printf.c's nvDbg_PrintMsg filters
     * every NV_PRINTF below LEVEL_ERROR (debuglevel_min = LEVEL_ERROR),
     * and the RmMsg override string that would lower that threshold is
     * only ever populated by nvDbgInitRmMsg() -- which lives in the Linux
     * platform layer (osReadRegistryString of NV_REG_STR_RM_MSG) that
     * Eclipse does not vendor, so RmMsg stays empty (bss) and all of
     * NVIDIA's own INFO/NOTICE step tracing is silently dropped. That's
     * why a graceful failure like gpumgrAttachGpu returning 0x40
     * (NV_ERR_INVALID_STATE) reaches us with no RM-side explanation on
     * the console at all.
     *
     * Populate RmMsg directly with a *targeted* rule. Per the RmMsg
     * grammar (nvlog_printf.c: `"dmanv50.c" - enable all printfs in
     * dmanv50.c`), a bare filename forces every printf level in that file
     * to print. Scope it to just the two files the attach walks --
     * gpu.c (gpuPostConstruct / gpuDetermineVirtualMode / child-engine
     * construction) and gpu_mgr.c (gpumgrAttachGpu /
     * _gpumgrDetermineConfComputeCapabilities) -- so the last step before
     * the failure is visible without flooding the console (and scrolling
     * the critical line off a monitor-only bring-up box) with every
     * subsystem's INFO spam. Written here, once, after DBG_INIT (inside
     * coreInitializeRm) has run and before eclipse_rm_attach_gpu.
     */
    {
        extern char RmMsg[];
        /*
         * RmMsg grammar (nvDbgRmMsgCheck, nvlog_printf.c): a comma-separated
         * list of nouns; each noun is substring-matched (nv_strnstr) against
         * the source filename, and a hit forces every printf level in that
         * file to print regardless of the release build's LEVEL_ERROR
         * threshold. So the bare substring "kernel_gsp" enables all six
         * kernel_gsp*.c files (kernel_gsp.c orchestration plus the per-arch
         * kernel_gsp_tu102/ga102/gh100/booter_tu102.c that run the actual
         * GSP bootstrap), and "kernel_falcon" enables the FALCON reset/RISC-V
         * bring-up files. This makes the GSP boot narrate itself live: each
         * NV_PRINTF reaches log::warn! -> the graphic console synchronously
         * (drivers/.../nvidia_hooks.rs), so on a hard hang inside
         * kgspBootstrap the LAST line left on the monitor pinpoints which
         * FALCON/Booter/FWSEC step wedged -- the captured /proc/gpustep6
         * buffer is useless for a hang because it is only flushed when the
         * read returns. The chatty post-boot log-polling (kgspStartLogPolling)
         * runs only after _kgspBootGspRm succeeds, so enabling kernel_gsp.c
         * does not flood a boot that hangs before then. gpu.c/gpu_mgr.c stay
         * for the attach (gpustep5) narration.
         */
        /*
         * eng_state.c added for step 9: gpuStatePreInit failed with a silent
         * NV_ERR_STATE_IN_USE (0x63) from some engine's StatePreInit (no
         * assert printed). engstateLogStateTransitionPost prints
         * "Engine %s state change: ..." at LEVEL_INFO for every engine the
         * state machine touches, so with this rule the LAST engine line
         * before the 0x63 names the culprit.
         */
        static const char rule[] = "gpu.c,gpu_mgr.c,kernel_gsp,kernel_falcon,eng_state.c";
        unsigned int i;
        for (i = 0; rule[i] != '\0'; i++)
        {
            RmMsg[i] = rule[i];
        }
        RmMsg[i] = '\0';
    }

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
static NV_STATUS _eclipse_rm_attach_gpu_body(
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
     * Run as a GSP firmware client, NOT monolithic RM. This is the single
     * most important attach flag for a Turing GPU on the open kernel
     * modules: the real Linux driver sets nv->request_fw_client_rm =
     * NV_TRUE whenever the GSP firmware image loads successfully
     * (osinit.c:1946, RmInitGspFirmware) -- which for our TU106 it does,
     * gsp.bin is present -- and then copies it straight into the attach
     * arg (osinit.c:884: `gpuAttachArg->bRequestFwClientRm =
     * nv->request_fw_client_rm`). From there gpumgrGetGpuHalFactor ->
     * gpumgrCheckRmFirmwarePolicy turns it into isFwClient=TRUE (TU106 is
     * >= TU100 so _gpumgrIsRmFirmwareCapableChip passes), which selects
     * RM_RUNTIME_VARIANT_PF_KERNEL_ONLY and makes IS_GSP_CLIENT(pGpu) true
     * for the whole engine topology.
     *
     * Leaving this NV_FALSE (the portMemSet default) selected
     * RM_RUNTIME_VARIANT_PF_MONOLITHIC -- a full-fat monolithic RM that
     * the open kernel modules do not support on Turing+ (GSP is enabled by
     * default for every TU100+ chip per gpumgrIsDeviceRmFirmwareCapable),
     * so gpuPostConstruct built the wrong, unsupported engine topology.
     * That mismatch is the prime suspect for the graceful
     * NV_ERR_INVALID_STATE (0x40) gpumgrAttachGpu returns on real
     * hardware. GSP is only *constructed* during attach here; it is not
     * booted until eclipse_rm_init_gsp (gpustep6, kgspInitRm), so this
     * does not itself touch GSP hardware.
     */
    gpuAttachArg.bRequestFwClientRm    = NV_TRUE;

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
 * Public attach entry: registers a THREAD_STATE_NODE around the body, like
 * every real RM entry point (see the identical bracket and rationale in
 * eclipse_rm_init_gsp -- without a node, threadStateGetCurrent() returns
 * NV_ERR_OBJECT_NOT_FOUND and explicit-timeout RM waits abort on their first
 * gpuCheckTimeout instead of waiting).
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
    THREAD_STATE_NODE threadState;
    NV_STATUS status;

    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);
    status = _eclipse_rm_attach_gpu_body(domain, bus, device,
                                         bar0_phys, bar0_virt, bar0_len,
                                         bar1_phys, bar1_len, pDeviceInstance);
    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
    return status;
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
    THREAD_STATE_NODE threadState;

    /*
     * Register a THREAD_STATE_NODE for this bring-up call, exactly like every
     * real RM entry point does (rmapi/*.c: threadStateInit(&threadState,
     * THREAD_STATE_FLAGS_NONE) around the work). Without it,
     * threadStateGetCurrent() fails with NV_ERR_OBJECT_NOT_FOUND (0x57), and
     * any RM wait that uses an EXPLICIT timeout (not GPU_TIMEOUT_DEFAULT)
     * aborts on its FIRST gpuCheckTimeout call instead of waiting:
     * timeoutCheck always consults threadStateCheckTimeout in addition to the
     * local timer, and only falls back to the local timer when
     * USE_THREAD_STATE was set (i.e. only for GPU_TIMEOUT_DEFAULT waits).
     * Real-hardware consequence: GspStatusQueueInit's 4-second wait for
     * GSP-RM to link the status queue died at retries=0 with 0x57 whenever
     * GSP-RM was not ALREADY ready on the first poll -- a boot-to-boot race
     * (one boot hit it, the previous one did not).
     */
    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);

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
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return status;
    }

    pGpu = gpumgrGetGpu(gpuInstance);
    if (pGpu == NULL)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return NV_ERR_INVALID_ARGUMENT;
    }

    pKernelGsp = GPU_GET_KERNEL_GSP(pGpu);
    if (pKernelGsp == NULL)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return NV_ERR_NOT_SUPPORTED;
    }

    portMemSet(&gspFw, 0, sizeof(gspFw));
    gspFw.pBuf = pBuf;
    gspFw.size = size;

    /*
     * Guaranteed-visible bracket around the vendored kgspInitRm. ECLIPSE_TRACE
     * goes straight to nv_printf -> log::warn! -> the graphic console, bypassing
     * the RM's LEVEL_ERROR release filter, so this line lands on the monitor
     * synchronously the instant it runs. If the box hard-hangs inside GSP boot
     * (kgspBootstrap polling a FALCON/Booter register that never settles, IRQs
     * off, no timeout possible), the "before kgspInitRm" line is the proof the
     * live console path works and the freeze is inside kgspInitRm; the
     * finer-grained "NVRM: ..." lines that follow (enabled via the kernel_gsp/
     * kernel_falcon RmMsg rule in eclipse_rm_init_core) then pinpoint the exact
     * step. The captured /proc/gpustep6 buffer cannot show any of this on a
     * hang because it is only returned once the read completes.
     */
    ECLIPSE_TRACE("init_gsp: before kgspInitRm (GSP bootstrap begins)");

    /*
     * The GSP bootstrap hard-hangs the whole box in kgspExecuteSequencerCommand
     * _TU102's SEC2 GSP-RM resume: gpuTimeoutCondWait polls a scratch register
     * that never handshakes, and the loop is only bounded if pGpu->timeoutData
     * carries a real timer-source flag (GPU_TIMEOUT_FLAGS_OSTIMER) -- otherwise
     * _checkTimeout returns NV_OK forever and the CPU spins with interrupts off
     * (verified: the box stays frozen indefinitely, never printing the SEC2
     * timeout error). timeoutData is normally armed by timeoutInitializeGpuDefault
     * inside gpuPostConstruct, which Eclipse's minimal attach may not run -- so
     * arm it explicitly here, right before the boot, and narrate the before/after
     * so we can see whether it was uninitialised (defaultFlags == 0) on this box.
     * timeoutInitializeGpuDefault just recomputes from osGetTimeoutParams (which
     * now returns OSTIMER + 4s), so calling it again is harmless if it already ran.
     */
    nv_printf(0, "[eclipse-rm-trace] init_gsp: timeoutData BEFORE arm: defaultFlags=0x%x defaultus=%u\n",
              pGpu->timeoutData.defaultFlags, pGpu->timeoutData.defaultus);
    timeoutInitializeGpuDefault(&pGpu->timeoutData, pGpu);
    nv_printf(0, "[eclipse-rm-trace] init_gsp: timeoutData AFTER  arm: defaultFlags=0x%x defaultus=%u\n",
              pGpu->timeoutData.defaultFlags, pGpu->timeoutData.defaultus);

    /*
     * kgspInitRm's lock contract (matching Linux's RmInitAdapter, which always
     * calls it with the RM API lock held): _kgspBootGspRm's relaxed-init path
     * (default-ON for Unix, _kgspShouldRelaxGspInitLocking) unconditionally
     * does rmapiLockRelease() before kgspBootstrap and rmapiLockAcquire()
     * afterwards (_kgspBootReacquireLocks). Calling kgspInitRm WITHOUT owning
     * the API lock made that release corrupt the rwlock state, and the
     * post-boot reacquire then spun forever inside portSync -- silently: no
     * MMIO (no GSPF probe lines) and no osSpinLoop (no heartbeats). That was
     * the machine freeze right after "GSP FW RM ready." on real hardware,
     * three boots in a row. The GPU locks need no action here: kgspInitRm
     * acquires them itself into gpusLockedMask and releases them at its own
     * exit.
     */
    ECLIPSE_TRACE("init_gsp: acquiring RM API lock (kgspInitRm contract)");
    status = rmapiLockAcquire(API_LOCK_FLAGS_NONE, RM_LOCK_MODULES_INIT);
    if (status != NV_OK)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return status;
    }

    status = kgspInitRm(pGpu, pKernelGsp, &gspFw);
    ECLIPSE_TRACE("init_gsp: after kgspInitRm (GSP bootstrap returned)");
    rmapiLockRelease();
    gpumgrThreadDisableExpandedGpuVisibility();
    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
    return status;
}

/*
 * osPackageRegistry -- OUR implementation (the real one lives in the excluded
 * arch/nvalloc/unix/src/registry.c and depends on the Linux nv_state_t
 * registry chain Eclipse does not have). The GSP SET_REGISTRY RPC
 * (rpcSetRegistry_v17_00, kernel_gsp.c:3376 via kgspQueueAsyncInitRpcs) hands
 * GSP-RM the set of driver registry-key overrides. Eclipse sets none, so we
 * emit a well-formed *empty* PACKED_REGISTRY_TABLE (header only, zero
 * entries): GSP-RM reads numEntries == 0 and simply applies its own built-in
 * defaults. We must return NV_OK here rather than the NV_WARN_NOTHING_TO_DO
 * the Linux implementation returns for an empty table, because the RPC caller
 * treats any status != NV_OK as failure and would abort GSP boot.
 */
NV_STATUS osPackageRegistry(
    OBJGPU                *pGpu,
    PACKED_REGISTRY_TABLE *pRegTable,
    NvU32                 *pSize
)
{
    const NvU32 headerSize = (NvU32)NV_OFFSETOF(PACKED_REGISTRY_TABLE, entries);

    (void)pGpu;

    if (pSize == NULL)
        return NV_ERR_INVALID_ARGUMENT;

    /* Sizing pass: report the size of the (empty) table. */
    if (pRegTable == NULL)
    {
        *pSize = headerSize;
        return NV_OK;
    }

    /* Fill pass: caller-provided buffer must hold at least the header. */
    if (*pSize < headerSize)
        return NV_ERR_BUFFER_TOO_SMALL;

    pRegTable->size       = headerSize;
    pRegTable->numEntries = 0;
    *pSize                = headerSize;

    return NV_OK;
}

/*
 * Step-7 readback: copy out the GspStaticConfigInfo the live GSP-RM returned
 * during kgspInitRm's GET_GSP_STATIC_INFO RPC. This is data generated BY the
 * firmware (GPU marketing name, VRAM geometry, VBIOS IDs), so displaying it
 * proves the CPU<->GSP RPC channel end-to-end beyond boot. Plain fixed-layout
 * out-struct (mirrored exactly in Rust, rm_init.rs) instead of formatting
 * here, so all string building stays on the Rust side.
 */
typedef struct EclipseGspInfo
{
    NvU8  gpuNameString[64];
    NvU8  gpuShortNameString[64];
    NvU64 fbLength;
    NvU32 fbBusWidth;
    NvU32 fbRamType;
    NvU32 l2CacheSize;
    NvU8  bVbiosValid;
    NvU32 vbiosSubVendor;
    NvU32 vbiosSubDevice;
} EclipseGspInfo;

NV_STATUS eclipse_rm_get_gsp_info(NvU32 gpuInstance, EclipseGspInfo *pInfo)
{
    OBJGPU *pGpu;
    GspStaticConfigInfo *pGSCI;
    NV_STATUS status;

    if (pInfo == NULL)
    {
        return NV_ERR_INVALID_ARGUMENT;
    }

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

    pGSCI = GPU_GET_GSP_STATIC_INFO(pGpu);
    if (pGSCI == NULL)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        return NV_ERR_INVALID_STATE;
    }

    portMemSet(pInfo, 0, sizeof(*pInfo));
    portMemCopy(pInfo->gpuNameString, sizeof(pInfo->gpuNameString) - 1,
                pGSCI->gpuNameString, sizeof(pInfo->gpuNameString) - 1);
    portMemCopy(pInfo->gpuShortNameString, sizeof(pInfo->gpuShortNameString) - 1,
                pGSCI->gpuShortNameString, sizeof(pInfo->gpuShortNameString) - 1);
    pInfo->fbLength       = pGSCI->fb_length;
    pInfo->fbBusWidth     = pGSCI->fb_bus_width;
    pInfo->fbRamType      = pGSCI->fb_ram_type;
    pInfo->l2CacheSize    = pGSCI->l2_cache_size;
    pInfo->bVbiosValid    = pGSCI->bVbiosValid ? 1 : 0;
    pInfo->vbiosSubVendor = pGSCI->vbiosSubVendor;
    pInfo->vbiosSubDevice = pGSCI->vbiosSubDevice;

    gpumgrThreadDisableExpandedGpuVisibility();
    return NV_OK;
}

/*
 * Step-8: real RM API controls served by the live GSP-RM's own resource
 * server. On a GSP client, GPU_GET_PHYSICAL_RMAPI(pGpu)->Control was
 * replaced at attach time (initRpcObject -> rpcRmApiSetup, rpc_common.c)
 * with rpcRmApiControl_GSP -- a direct NV_VGPU_MSG_FUNCTION_GSP_RM_CONTROL
 * RPC that GSP-RM executes against ITS resource-server objects. The client/
 * subdevice handles for that are the GSP-side internal handles GSP-RM
 * advertised in GspStaticConfigInfo -- normally adopted into pGpu by
 * _gpuAllocateInternalObjects during gpuStatePreInit (gpu.c), which
 * Eclipse's minimal bring-up has not run, so adopt them here the same way.
 *
 * Three read-only controls, chosen to be served entirely by the firmware:
 * GPU name string, GPU GID/UUID (GSP computes it), and FB heap total/free
 * (dynamic data straight out of GSP-RM's live heap bookkeeping).
 */
typedef struct EclipseRmApiDemo
{
    NvU32 nameStatus;
    NvU8  name[64];
    NvU32 gidStatus;
    NvU32 gidLength;
    NvU8  gid[136];
    NvU32 fbStatus;
    NvU32 heapSizeKb;
    NvU32 heapFreeKb;
    NvU32 busWidth;
} EclipseRmApiDemo;

NV_STATUS eclipse_rm_step8(NvU32 gpuInstance, EclipseRmApiDemo *pOut)
{
    OBJGPU *pGpu;
    RM_API *pRmApi;
    NV_STATUS status;
    THREAD_STATE_NODE threadState;
    GPU_MASK gpusLockedMask = 0;

    if (pOut == NULL)
    {
        return NV_ERR_INVALID_ARGUMENT;
    }
    portMemSet(pOut, 0, sizeof(*pOut));
    pOut->nameStatus = NV_ERR_NOT_READY;
    pOut->gidStatus  = NV_ERR_NOT_READY;
    pOut->fbStatus   = NV_ERR_NOT_READY;

    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);

    status = gpumgrThreadEnableExpandedGpuVisibility();
    if (status != NV_OK)
    {
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return status;
    }

    pGpu = gpumgrGetGpu(gpuInstance);
    if (pGpu == NULL || !pGpu->gspRmInitialized)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return (pGpu == NULL) ? NV_ERR_INVALID_ARGUMENT : NV_ERR_INVALID_STATE;
    }

    /*
     * Hold the API lock and this GPU's group lock across the RPC controls:
     * rpcWriteCommonHeader asserts rmDeviceGpuLockIsOwner, and
     * rpcRmApiControl_GSP only self-acquires (with a warning) as a fallback.
     */
    status = rmapiLockAcquire(API_LOCK_FLAGS_NONE, RM_LOCK_MODULES_INIT);
    if (status != NV_OK)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return status;
    }
    status = rmGpuGroupLockAcquire(pGpu->gpuInstance, GPU_LOCK_GRP_SUBDEVICE,
                                   GPUS_LOCK_FLAGS_NONE, RM_LOCK_MODULES_INIT,
                                   &gpusLockedMask);
    if (status != NV_OK)
    {
        rmapiLockRelease();
        gpumgrThreadDisableExpandedGpuVisibility();
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return status;
    }

    /* Adopt the GSP-side internal handles (gpuStatePreInit's GSP-client
     * branch of _gpuAllocateInternalObjects, which we never ran). */
    if (pGpu->hInternalClient == 0)
    {
        GspStaticConfigInfo *pGSCI = GPU_GET_GSP_STATIC_INFO(pGpu);
        if (pGSCI == NULL || pGSCI->hInternalClient == 0)
        {
            status = NV_ERR_INVALID_STATE;
            goto unlock;
        }
        pGpu->hInternalClient = pGSCI->hInternalClient;
        pGpu->hInternalDevice = pGSCI->hInternalDevice;
        pGpu->hInternalSubdevice = pGSCI->hInternalSubdevice;
        rmapiControlCacheSetGpuAttrForObject(pGpu->hInternalClient,
                                             pGpu->hInternalSubdevice, pGpu);
        rmapiControlCacheSetGpuAttrForObject(pGpu->hInternalClient,
                                             pGpu->hInternalDevice, pGpu);
        nv_printf(0, "[eclipse-rm-trace] step8: adopted GSP internal handles client=0x%x subdevice=0x%x\n",
                  pGpu->hInternalClient, pGpu->hInternalSubdevice);
    }

    pRmApi = GPU_GET_PHYSICAL_RMAPI(pGpu);

    {
        NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS nameParams;
        portMemSet(&nameParams, 0, sizeof(nameParams));
        nameParams.gpuNameStringFlags = NV2080_CTRL_GPU_GET_NAME_STRING_FLAGS_TYPE_ASCII;
        pOut->nameStatus = pRmApi->Control(pRmApi, pGpu->hInternalClient,
                                           pGpu->hInternalSubdevice,
                                           NV2080_CTRL_CMD_GPU_GET_NAME_STRING,
                                           &nameParams, sizeof(nameParams));
        nv_printf(0, "[eclipse-rm-trace] step8: GET_NAME_STRING -> 0x%x\n", pOut->nameStatus);
        if (pOut->nameStatus == NV_OK)
        {
            portMemCopy(pOut->name, sizeof(pOut->name) - 1,
                        nameParams.gpuNameString.ascii, sizeof(pOut->name) - 1);
        }
    }

    {
        NV2080_CTRL_GPU_GET_GID_INFO_PARAMS gidParams;
        portMemSet(&gidParams, 0, sizeof(gidParams));
        gidParams.index = 0;
        gidParams.flags = NV2080_GPU_CMD_GPU_GET_GID_FLAGS_FORMAT_ASCII;
        pOut->gidStatus = pRmApi->Control(pRmApi, pGpu->hInternalClient,
                                          pGpu->hInternalSubdevice,
                                          NV2080_CTRL_CMD_GPU_GET_GID_INFO,
                                          &gidParams, sizeof(gidParams));
        nv_printf(0, "[eclipse-rm-trace] step8: GET_GID_INFO -> 0x%x (len %u)\n",
                  pOut->gidStatus, gidParams.length);
        if (pOut->gidStatus == NV_OK)
        {
            NvU32 n = gidParams.length;
            if (n > sizeof(pOut->gid) - 1)
            {
                n = sizeof(pOut->gid) - 1;
            }
            portMemCopy(pOut->gid, n, gidParams.data, n);
            pOut->gidLength = n;
        }
    }

    {
        NV2080_CTRL_FB_GET_INFO_V2_PARAMS fbParams;
        portMemSet(&fbParams, 0, sizeof(fbParams));
        fbParams.fbInfoListSize = 3;
        fbParams.fbInfoList[0].index = NV2080_CTRL_FB_INFO_INDEX_HEAP_SIZE;
        fbParams.fbInfoList[1].index = NV2080_CTRL_FB_INFO_INDEX_HEAP_FREE;
        fbParams.fbInfoList[2].index = NV2080_CTRL_FB_INFO_INDEX_BUS_WIDTH;
        pOut->fbStatus = pRmApi->Control(pRmApi, pGpu->hInternalClient,
                                         pGpu->hInternalSubdevice,
                                         NV2080_CTRL_CMD_FB_GET_INFO_V2,
                                         &fbParams, sizeof(fbParams));
        nv_printf(0, "[eclipse-rm-trace] step8: FB_GET_INFO_V2 -> 0x%x\n", pOut->fbStatus);
        if (pOut->fbStatus == NV_OK)
        {
            pOut->heapSizeKb = fbParams.fbInfoList[0].data;
            pOut->heapFreeKb = fbParams.fbInfoList[1].data;
            pOut->busWidth   = fbParams.fbInfoList[2].data;
        }
    }

    status = NV_OK;

unlock:
    rmGpuGroupLockRelease(gpusLockedMask, GPUS_LOCK_FLAGS_NONE);
    rmapiLockRelease();
    gpumgrThreadDisableExpandedGpuVisibility();
    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
    return status;
}

/*
 * Step-9: the rest of the real RmInitAdapter device bring-up
 * (arch/nvalloc/unix/src/osinit.c, RmInitNvDevice), run after kgspInitRm
 * exactly like the Linux driver does:
 *
 *   gpumgrStatePreInitGpu -> gpumgrStateInitGpu ->
 *   intrSetIntrEn(DISABLED) -> gpumgrStateLoadGpu(GPU_STATE_DEFAULT)
 *
 * This runs every kernel-side engine's StatePreInit/StateInit/StateLoad
 * against the live GSP (FIFO, GMMU, bus/BAR mappings, memory manager, CE,
 * ...) -- the gateway to actually using the GPU. Interrupts are explicitly
 * left DISABLED (same call the real driver makes before StateLoad), which
 * also matches Eclipse's fully-polled operation. Each phase's status is
 * reported separately so a failure names its phase; the RM's own
 * LEVEL_ERROR prints (and our RmMsg-enabled files) land in the /proc
 * capture for the exact failing engine.
 */
#include "gpu/intr/intr.h"

typedef struct EclipseStateInitResult
{
    NvU32 preInitStatus;
    NvU32 initStatus;
    NvU32 loadStatus;
} EclipseStateInitResult;

NV_STATUS eclipse_rm_state_init(NvU32 gpuInstance, EclipseStateInitResult *pOut)
{
    OBJGPU *pGpu;
    Intr *pIntr;
    NV_STATUS status;
    THREAD_STATE_NODE threadState;
    const NvU32 not_run = 0xFFFFFFFFu;

    if (pOut == NULL)
    {
        return NV_ERR_INVALID_ARGUMENT;
    }
    pOut->preInitStatus = not_run;
    pOut->initStatus    = not_run;
    pOut->loadStatus    = not_run;

    threadStateInit(&threadState, THREAD_STATE_FLAGS_NONE);

    status = gpumgrThreadEnableExpandedGpuVisibility();
    if (status != NV_OK)
    {
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return status;
    }

    pGpu = gpumgrGetGpu(gpuInstance);
    if (pGpu == NULL || !pGpu->gspRmInitialized)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return (pGpu == NULL) ? NV_ERR_INVALID_ARGUMENT : NV_ERR_INVALID_STATE;
    }

    /*
     * Hold ONLY the API lock here -- NOT the GPU locks. This was the silent
     * 0x63: the gpumgrState*Gpu wrappers each open with
     * rmGpuLocksAcquire(GPUS_LOCK_FLAGS_NONE, ...) (gpu_mgr.c) and acquire
     * the GPU locks themselves around gpuStatePreInit/Init/Load (that is who
     * satisfies gpuStatePreInit's rmGpuLockIsOwner assert). With our wrapper
     * pre-holding the group lock, that acquire found the lock already
     * running and returned NV_ERR_STATE_IN_USE (locks.c) without printing
     * anything, before a single engine ran. RmInitNvDevice (osinit.c) calls
     * these wrappers with only the API lock held -- match it exactly.
     */
    status = rmapiLockAcquire(API_LOCK_FLAGS_NONE, RM_LOCK_MODULES_INIT);
    if (status != NV_OK)
    {
        gpumgrThreadDisableExpandedGpuVisibility();
        threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
        return status;
    }

    /*
     * Version stamp: the previous debugging round burned a reboot because the
     * on-disk kernel was stale (output byte-identical to the prior build, new
     * narration absent) and there was no way to tell from the photo. Bump
     * this string whenever step-9 diagnostics change so the output
     * self-identifies which build produced it.
     */
    ECLIPSE_TRACE("state_init: narration v2 (eng_state.c engine trace enabled)");
    ECLIPSE_TRACE("state_init: before gpumgrStatePreInitGpu");
    pOut->preInitStatus = gpumgrStatePreInitGpu(pGpu);
    nv_printf(0, "[eclipse-rm-trace] state_init: gpumgrStatePreInitGpu -> 0x%x\n",
              pOut->preInitStatus);
    if (pOut->preInitStatus != NV_OK)
    {
        goto unlock;
    }

    ECLIPSE_TRACE("state_init: before gpumgrStateInitGpu");
    pOut->initStatus = gpumgrStateInitGpu(pGpu);
    nv_printf(0, "[eclipse-rm-trace] state_init: gpumgrStateInitGpu -> 0x%x\n",
              pOut->initStatus);
    if (pOut->initStatus != NV_OK)
    {
        goto unlock;
    }

    /* Same as RmInitNvDevice: keep RM's interrupt enable state at zero so
     * nothing gets enabled during StateLoad (Eclipse is fully polled). */
    pIntr = GPU_GET_INTR(pGpu);
    if (pIntr != NULL)
    {
        intrSetIntrEn(pIntr, INTERRUPT_TYPE_DISABLED);
    }

    ECLIPSE_TRACE("state_init: before gpumgrStateLoadGpu");
    pOut->loadStatus = gpumgrStateLoadGpu(pGpu, GPU_STATE_DEFAULT);
    nv_printf(0, "[eclipse-rm-trace] state_init: gpumgrStateLoadGpu -> 0x%x\n",
              pOut->loadStatus);

unlock:
    rmapiLockRelease();
    gpumgrThreadDisableExpandedGpuVisibility();
    threadStateFree(&threadState, THREAD_STATE_FLAGS_NONE);
    return NV_OK;
}
