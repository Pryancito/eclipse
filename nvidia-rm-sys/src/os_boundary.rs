//! Implementation of the remaining "OBJOS" internal-service surface
//! (src/nvidia/generated/g_os_nvoc.h) revealed once nearly the entire
//! real Resource Manager core links (see build.rs) -- distinct from
//! both `os_interface.rs` (the real os-interface.h ABI) and the smaller
//! `os_services.rs` set from an earlier pass. Signatures transcribed
//! verbatim from the real generated header.
//!
//! A handful (`osPciInitHandle`, `osUnmapKernelSpace`, `osGetRandomBytes`)
//! delegate straight to the matching os_interface.rs function since the
//! signatures happen to line up exactly. `osPciReadByte/Word/Dword`,
//! `osPciWriteWord/Dword`, and `osMapKernelSpace` have a DIFFERENT
//! arity/return convention than their os_interface.rs namesakes (this
//! surface returns the value directly rather than NV_STATUS-plus-
//! out-param, and osMapKernelSpace has an extra `Protect` argument), so
//! those are hand-written against the same `KernelHooks` facade
//! instead of naively delegating.
//!
//! Everything else defaults to the same "not supported" convention
//! already established throughout os_interface.rs: ACPI method calls,
//! Tegra SoC integration, NUMA, cgroups, vGPU, IMEX fabric memory
//! export, host filesystem access, and internal RM diagnostic/crash-log
//! bookkeeping are all out of scope for Eclipse's single discrete-GPU
//! bring-up.
#![allow(non_snake_case)]

use crate::hooks::with_hooks;
use crate::types::*;

#[no_mangle]
pub extern "C" fn osAcquireRmSema(arg0: *mut c_void) -> *mut c_void {
    let _ = arg0;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osAddRecordForCrashLog(arg0: *mut c_void, arg1: NvU32) {
    let _ = arg0;
    let _ = arg1;
}

#[no_mangle]
pub extern "C" fn osAllocAcquirePage(arg0: NvU64, arg1: NvU32) {
    let _ = arg0;
    let _ = arg1;
}

#[no_mangle]
pub extern "C" fn osAllocPagesInternal(arg0: *mut c_void) -> *mut c_void {
    let _ = arg0;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osAllocPagesNode(arg0: NvS32, arg1: NvU64, arg2: NvU32, arg3: *mut NvU64) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osAllocReleasePage(arg0: NvU64, arg1: NvU32) {
    let _ = arg0;
    let _ = arg1;
}

#[no_mangle]
pub extern "C" fn osAllocWaitQueue(ppWq: *mut *mut c_void) -> NV_STATUS {
    let _ = ppWq;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osAllocatedRmClient(pOSInfo: *mut c_void) {
    let _ = pOSInfo;
}

#[no_mangle]
pub extern "C" fn osApiLockAcquireConfigureFlags(flags: NvU32) -> NvU32 {
    let _ = flags;
    0
}

#[no_mangle]
pub extern "C" fn osAssertFailed() {

}

#[no_mangle]
pub extern "C" fn osAttachGpu(arg0: *mut c_void, arg1: *mut c_void) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osAttachToProcess(arg0: *mut *mut c_void, arg1: NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osBugCheck(bugCode: NvU32) {
    let _ = bugCode;
}

#[no_mangle]
pub extern "C" fn osCallACPI_DDC(arg0: *mut c_void, arg1: NvU32, arg2: *mut NvU8, arg3: *mut NvU32, arg4: NvBool) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCallACPI_DOD(arg0: *mut c_void, arg1: *mut NvU32, arg2: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCallACPI_DSM(pGpu: *mut c_void, acpiDSMFunction: *mut c_void, NVHGDSMSubfunction: NvU32, pInOut: *mut NvU32, size: *mut NvU16) -> NV_STATUS {
    let _ = pGpu;
    let _ = acpiDSMFunction;
    let _ = NVHGDSMSubfunction;
    let _ = pInOut;
    let _ = size;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCallACPI_MXDM(arg0: *mut c_void, arg1: NvU32, arg2: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCallACPI_MXDS(arg0: *mut c_void, arg1: NvU32, arg2: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCallACPI_NVHG_ROM(arg0: *mut c_void, arg1: *mut NvU32, arg2: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCancelNanoTimer(pArg1: *mut c_void, pArg2: *mut c_void) -> NV_STATUS {
    let _ = pArg1;
    let _ = pArg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCgroupImplementation() -> *mut c_void {
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osCgroupRegisterRegion(pGpu: *mut c_void, size: NvU64) -> *mut c_void {
    let _ = pGpu;
    let _ = size;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osCgroupUnregisterRegion(region: *mut c_void) {
    let _ = region;
}

#[no_mangle]
pub extern "C" fn osCheckAccess(accessRight: *mut c_void) -> NvBool {
    let _ = accessRight;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osClientGcoffDisallowRefcount(pArg1: *mut c_void, arg2: NvBool) {
    let _ = pArg1;
    let _ = arg2;
}

#[no_mangle]
pub extern "C" fn osClientGroupID(process: NvU64, pidInfo: *mut c_void) -> *mut c_void {
    let _ = process;
    let _ = pidInfo;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osCloseFile(pFile: *mut c_void) {
    let _ = pFile;
}

#[no_mangle]
pub extern "C" fn osCondAcquireRmSema(arg0: *mut c_void) -> *mut c_void {
    let _ = arg0;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osConfigurePcieReqAtomics(pOsGpuInfo: *mut c_void, pMask: *mut NvU32) -> NV_STATUS {
    let _ = pOsGpuInfo;
    let _ = pMask;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCountTailPages(arg0: NvU64) -> NvU32 {
    let _ = arg0;
    0
}

#[no_mangle]
pub extern "C" fn osCreateMemFromOsDescriptor(pGpu: *mut c_void, pDescriptor: *mut c_void, hClient: *mut c_void, flags: NvU32, pLimit: *mut NvU64, ppMemDesc: *mut *mut c_void, descriptorType: NvU32, privilegeLevel: *mut c_void) -> NV_STATUS {
    let _ = pGpu;
    let _ = pDescriptor;
    let _ = hClient;
    let _ = flags;
    let _ = pLimit;
    let _ = ppMemDesc;
    let _ = descriptorType;
    let _ = privilegeLevel;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCreateNanoTimer(pArg1: *mut c_void, tmrEvent: *mut c_void, tmrUserData: *mut *mut c_void) -> NV_STATUS {
    let _ = pArg1;
    let _ = tmrEvent;
    let _ = tmrUserData;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osCxlSetCaching(pGpu: *mut c_void, bEnableCache: NvBool) {
    let _ = pGpu;
    let _ = bEnableCache;
}

#[no_mangle]
pub extern "C" fn osDelay(arg0: NvU32) -> NV_STATUS {
    let _ = arg0;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osDelayNs(arg0: NvU32) -> NV_STATUS {
    let _ = arg0;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osDeleteRecordForCrashLog(arg0: *mut c_void) {
    let _ = arg0;
}

#[no_mangle]
pub extern "C" fn osDereferenceObjectCount(pEvent: *mut c_void) -> NV_STATUS {
    let _ = pEvent;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osDestroyNanoTimer(pArg1: *mut c_void, pArg2: *mut c_void) -> NV_STATUS {
    let _ = pArg1;
    let _ = pArg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osDetachFromProcess(arg0: *mut c_void) {
    let _ = arg0;
}

// osDevRead/WriteReg{008,016,032} were unconditional stubs (always read 0,
// writes dropped) -- a confirmed real bug, not a deliberate "not supported"
// stub: this is the exact function real chip-ID detection uses BEFORE an
// OBJGPU even exists (gpumgrGetGpuHalFactor in gpu_mgr.c, pGpu=NULL), so
// stubbing it made TU106 undetectable (chipId0/chipId1 always read as 0).
//
// Real DEVICE_MAPPING layout (src/nvidia/generated/g_gpu_access_nvoc.h,
// aggregated from gpu_access.h -- confirmed against the actual struct body,
// not guessed):
//   typedef struct DEVICE_MAPPING {
//       GPUHWREG  *gpuNvAddr;   // CPU Virtual Address -- first field
//       RmPhysAddr gpuNvPAddr;
//       NvU64      gpuNvLength;
//       ...
//   } DEVICE_MAPPING;
// `gpuNvAddr` is the already page-table-mapped CPU virtual base of the
// aperture (for the discrete-GPU path, Eclipse's own bar0_virt -- see
// gpu_mgr.c's `gpuDevMapping.gpuNvAddr = pAttachArg->regBaseAddr`, and
// eclipse_rm_init.c's `gpuAttachArg.regBaseAddr = (GPUHWREG*)bar0_virt`).
// Being first means reading the pointer-sized value at pMapping's own
// address gets it directly, with no need to know the rest of the struct.
// This is a plain mapped-memory read, not port I/O or PCI config space, so
// (unlike PCI/MMIO-map/timing) it needs no KernelHooks indirection.
#[no_mangle]
pub extern "C" fn osDevReadReg008(_pGpu: *mut c_void, pMapping: *mut c_void, this_address: NvU32) -> NvU8 {
    match dev_mapping_base(pMapping) {
        Some(base) => unsafe { core::ptr::read_volatile(base.add(this_address as usize)) },
        None => 0xFF,
    }
}

#[no_mangle]
pub extern "C" fn osDevReadReg016(_pGpu: *mut c_void, pMapping: *mut c_void, this_address: NvU32) -> NvU16 {
    match dev_mapping_base(pMapping) {
        Some(base) => unsafe { core::ptr::read_volatile(base.add(this_address as usize) as *const NvU16) },
        None => 0xFFFF,
    }
}

#[no_mangle]
pub extern "C" fn osDevReadReg032(_pGpu: *mut c_void, pMapping: *mut c_void, this_address: NvU32) -> NvU32 {
    match dev_mapping_base(pMapping) {
        Some(base) => unsafe { core::ptr::read_volatile(base.add(this_address as usize) as *const NvU32) },
        None => 0xFFFF_FFFF,
    }
}

#[no_mangle]
pub extern "C" fn osDevWriteReg008(_pGpu: *mut c_void, pMapping: *mut c_void, this_address: NvU32, this_value: NvU8) {
    if let Some(base) = dev_mapping_base(pMapping) {
        unsafe { core::ptr::write_volatile(base.add(this_address as usize), this_value) };
    }
}

#[no_mangle]
pub extern "C" fn osDevWriteReg016(_pGpu: *mut c_void, pMapping: *mut c_void, this_address: NvU32, this_value: NvU16) {
    if let Some(base) = dev_mapping_base(pMapping) {
        unsafe { core::ptr::write_volatile(base.add(this_address as usize) as *mut NvU16, this_value) };
    }
}

#[no_mangle]
pub extern "C" fn osDevWriteReg032(_pGpu: *mut c_void, pMapping: *mut c_void, this_address: NvU32, this_value: NvU32) {
    if let Some(base) = dev_mapping_base(pMapping) {
        unsafe { core::ptr::write_volatile(base.add(this_address as usize) as *mut NvU32, this_value) };
    }
}

/// Extracts `DEVICE_MAPPING::gpuNvAddr` (the first field) without needing
/// the rest of the struct's layout. Returns `None` for a null mapping or a
/// null/not-yet-mapped `gpuNvAddr`.
fn dev_mapping_base(p_mapping: *mut c_void) -> Option<*mut u8> {
    if p_mapping.is_null() {
        return None;
    }
    let base = unsafe { *(p_mapping as *const *mut u8) };
    if base.is_null() {
        None
    } else {
        Some(base)
    }
}

#[no_mangle]
pub extern "C" fn osDisableConsoleManagement(pGpu: *mut c_void) {
    let _ = pGpu;
}

#[no_mangle]
pub extern "C" fn osDisableInterrupts(pGpu: *mut c_void, bIsr: NvBool) {
    let _ = pGpu;
    let _ = bIsr;
}

#[no_mangle]
pub extern "C" fn osDmaSetAddressSize(pArg1: *mut c_void, bits: NvU32) {
    let _ = pArg1;
    let _ = bits;
}

#[no_mangle]
pub extern "C" fn osDmabufIsSupported() -> NvBool {
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osDpcAttachGpu(arg0: *mut c_void, arg1: *mut c_void) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osDpcDetachGpu(arg0: *mut c_void) {
    let _ = arg0;
}

#[no_mangle]
pub extern "C" fn osEnableInterrupts(pGpu: *mut c_void) {
    let _ = pGpu;
}

#[no_mangle]
pub extern "C" fn osErrorLogV(pGpu: *mut c_void, context: *mut c_void, pFormat: *mut c_char, arglist: *mut c_void) -> *mut c_void {
    let _ = pGpu;
    let _ = context;
    let _ = pFormat;
    let _ = arglist;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osEventNotification(arg0: *mut c_void, arg1: *mut c_void, arg2: NvU32, arg3: *mut c_void, arg4: NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osEventNotificationWithInfo(arg0: *mut c_void, arg1: *mut c_void, arg2: NvU32, arg3: NvU32, arg4: NvU16, arg5: *mut c_void, arg6: NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    let _ = arg5;
    let _ = arg6;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osFindNsPid(pOsPidInfo: *mut c_void, pNsPid: *mut NvU32) -> NV_STATUS {
    let _ = pOsPidInfo;
    let _ = pNsPid;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osFlushCpuCache() -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osFlushCpuWriteCombineBuffer() {

}

#[no_mangle]
pub extern "C" fn osFlushGpuCoherentCpuCacheRange(pOsGpuInfo: *mut c_void, cpuVirtual: NvU64, size: NvU64) {
    let _ = pOsGpuInfo;
    let _ = cpuVirtual;
    let _ = size;
}

#[no_mangle]
pub extern "C" fn osFreePagesInternal(arg0: *mut c_void) {
    let _ = arg0;
}

#[no_mangle]
pub extern "C" fn osFreeWaitQueue(pWq: *mut c_void) {
    let _ = pWq;
}

#[no_mangle]
pub extern "C" fn osGC6PowerControl(arg0: *mut c_void, arg1: NvU32, arg2: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetAcpiRsdpFromUefi(pRsdpAddr: *mut NvU32) -> NV_STATUS {
    let _ = pRsdpAddr;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetCpuCount() -> NvU32 {
    0
}

#[no_mangle]
pub extern "C" fn osGetCpuFrequency() -> NvU32 {
    0
}

#[no_mangle]
pub extern "C" fn osGetCurrentProcess() -> NvU32 {
    0
}

#[no_mangle]
pub extern "C" fn osGetCurrentProcessName(arg0: *mut c_char, arg1: NvU32) {
    let _ = arg0;
    let _ = arg1;
}

#[no_mangle]
pub extern "C" fn osGetCurrentUidToken() -> *mut c_void {
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osGetDriverBlock(arg0: *mut c_void, arg1: *mut c_void) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetDynamicPowerSupportMask() -> NvU32 {
    0
}

#[no_mangle]
pub extern "C" fn osGetEgmInfo(pGpu: *mut c_void, pPhysAddr: *mut NvU64, pSize: *mut NvU64, pNodeId: *mut NvS32) -> NV_STATUS {
    let _ = pGpu;
    let _ = pPhysAddr;
    let _ = pSize;
    let _ = pNodeId;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetFbNumaInfo(pGpu: *mut c_void, pAddrPhys: *mut NvU64, pSizePhys: *mut NvU64, pAddrRsvdPhys: *mut NvU64, pNodeId: *mut NvS32) -> NV_STATUS {
    let _ = pGpu;
    let _ = pAddrPhys;
    let _ = pSizePhys;
    let _ = pAddrRsvdPhys;
    let _ = pNodeId;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetForcedNVLinkConnection(pGpu: *mut c_void, maxLinks: NvU32, pLinkConnection: *mut NvU32) -> NV_STATUS {
    let _ = pGpu;
    let _ = maxLinks;
    let _ = pLinkConnection;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetGridCspSupport() -> NvU32 {
    0
}

#[no_mangle]
pub extern "C" fn osGetMaxUserVa() -> NvU64 {
    0
}

#[no_mangle]
pub extern "C" fn osGetMemoryPages(pMemDesc: *mut c_void, pPages: *mut c_void, pNumPages: *mut NvU32) -> NV_STATUS {
    let _ = pMemDesc;
    let _ = pPages;
    let _ = pNumPages;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetNumMemoryPages(pMemDesc: *mut c_void, pNumPages: *mut NvU32) -> NV_STATUS {
    let _ = pMemDesc;
    let _ = pNumPages;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn NVBIT(numaId: *mut c_void, free_memory_bytes: *mut NvU64, total_memory_bytes: *mut NvU64) -> *mut c_void {
    let _ = numaId;
    let _ = free_memory_bytes;
    let _ = total_memory_bytes;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osGetNvlinkLinkCallbacks() -> *mut c_void {
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osGetPageRefcount(arg0: NvU64) -> NvU32 {
    let _ = arg0;
    0
}

#[no_mangle]
pub extern "C" fn osGetPageShift() -> NvU8 {
    0
}

#[no_mangle]
pub extern "C" fn osGetPageSize() -> NvU64 {
    0
}

#[no_mangle]
pub extern "C" fn osGetPerformanceCounter(arg0: *mut NvU64) -> NV_STATUS {
    let _ = arg0;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetPidInfo() -> *mut c_void {
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osGetPlatformNvlinkLinerate(pGpu: *mut c_void, lineRate: *mut NvU32) -> NV_STATUS {
    let _ = pGpu;
    let _ = lineRate;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetRandomBytes(pBytes: *mut NvU8, numBytes: NvU16) -> NV_STATUS {
    crate::os_interface::os_get_random_bytes(pBytes, numBytes)
}

#[no_mangle]
pub extern "C" fn osGetSecurityToken() -> *mut c_void {
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osGetSupportedSysmemPageSizeMask() -> NvU64 {
    0
}

#[no_mangle]
pub extern "C" fn osGetSyncpointAperture(pOsGpuInfo: *mut c_void, syncpointId: NvU32, physAddr: *mut NvU64, limit: *mut NvU64, offset: *mut NvU32) -> NV_STATUS {
    let _ = pOsGpuInfo;
    let _ = syncpointId;
    let _ = physAddr;
    let _ = limit;
    let _ = offset;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGetTimestamp() -> *mut c_void {
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osGetTimestampFreq() -> NvU64 {
    0
}

#[no_mangle]
pub extern "C" fn osGetVersionDump(arg0: *mut c_void) -> NV_STATUS {
    let _ = arg0;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osGpuIsCxlDevice(pGpu: *mut c_void) -> NvBool {
    let _ = pGpu;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn DPC_RELEASE_ALL_GPU_LOCKS(pGpu: *mut c_void, dpcGpuLockRelease: NvU32) -> *mut c_void {
    let _ = pGpu;
    let _ = dpcGpuLockRelease;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osGpuSupportsAts(pGpu: *mut c_void) -> NvBool {
    let _ = pGpu;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osHandleDeferredRecovery(arg0: *mut c_void) {
    let _ = arg0;
}

#[no_mangle]
pub extern "C" fn osHandleGpuLost(arg0: *mut c_void, arg1: NvBool) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osImexChannelCount() -> NvS32 {
    -1
}

#[no_mangle]
pub extern "C" fn osImexChannelGet(descriptor: NvU64) -> NvS32 {
    let _ = descriptor;
    -1
}

#[no_mangle]
pub extern "C" fn osImexChannelIsSupported() -> NvBool {
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osInitMapping(pGpu: *mut c_void) -> NV_STATUS {
    let _ = pGpu;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osInitSystemStaticConfig(arg0: *mut c_void) {
    let _ = arg0;
}

#[no_mangle]
pub extern "C" fn osIovaMap(pIovaMapping: *mut c_void) -> NV_STATUS {
    let _ = pIovaMapping;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osIovaUnmap(pIovaMapping: *mut c_void) {
    let _ = pIovaMapping;
}

#[no_mangle]
pub extern "C" fn osIsAdministrator() -> NvBool {
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osIsGpuAccessible(pGpu: *mut c_void) -> NvBool {
    let _ = pGpu;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osIsGpuShutdown(pGpu: *mut c_void) -> NvBool {
    let _ = pGpu;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osIsGridSupported(arg0: *mut c_void) -> NvBool {
    let _ = arg0;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osIsISR() -> NvBool {
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osIsInitNs() -> NvBool {
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osIsNvswitchPresent() -> NvBool {
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osIsRaisedIRQL() -> NvBool {
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osIsSwPreInitOnly(arg0: *mut c_void) -> NvBool {
    let _ = arg0;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osIsVfioPciCorePresent() -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osIsVgpuDeviceVmPresent() -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osIsVgpuVfioPresent() -> NV_STATUS {
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osLockMem(arg0: *mut c_void) -> *mut c_void {
    let _ = arg0;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osLockShouldToggleInterrupts(arg0: *mut c_void) -> NvBool {
    let _ = arg0;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osMapGPU(arg0: *mut c_void, arg1: *mut c_void, arg2: NvU64, arg3: NvU64, arg4: NvU32, arg5: *mut c_void, arg6: *mut c_void) -> *mut c_void {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    let _ = arg5;
    let _ = arg6;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osMapPciMemoryAreaUser(arg0: *mut c_void, arg1: *mut c_void, arg2: NvU32, arg3: NvU32, arg4: *mut c_void, arg5: *mut c_void) -> *mut c_void {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    let _ = arg5;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osMapPciMemoryKernel64(arg0: *mut c_void, arg1: NvU64, arg2: NvU64, arg3: NvU32, arg4: *mut c_void, arg5: NvU32) -> *mut c_void {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    let _ = arg5;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osMapPciMemoryKernelOld(arg0: *mut c_void, arg1: NvU64, arg2: NvU64, arg3: NvU32, arg4: *mut *mut c_void, arg5: NvU32) -> *mut c_void {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    let _ = arg5;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osMapPciMemoryUser(arg0: *mut c_void, arg1: NvU64, arg2: NvU64, arg3: NvU32, arg4: *mut c_void, arg5: *mut c_void, arg6: NvU32) -> *mut c_void {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    let _ = arg5;
    let _ = arg6;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osMapSystemMemory(arg0: *mut c_void, arg1: NvU64, arg2: NvU64, arg3: NvBool, arg4: NvU32, arg5: *mut c_void, arg6: *mut c_void) -> *mut c_void {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    let _ = arg5;
    let _ = arg6;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osMatchGpuOsInfo(pGpu: *mut c_void, pOsInfo: *mut c_void) -> NvBool {
    let _ = pGpu;
    let _ = pOsInfo;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osMemacctReleaseCharge(pool: *mut c_void, size: NvU64) {
    let _ = pool;
    let _ = size;
}

#[no_mangle]
pub extern "C" fn osMemacctTryCharge(pRegion: *mut c_void, size: NvU64, ppPool: *mut *mut c_void) -> NV_STATUS {
    let _ = pRegion;
    let _ = size;
    let _ = ppPool;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osModifyGpuSwStatePersistence(arg0: *mut c_void, arg1: NvBool) {
    let _ = arg0;
    let _ = arg1;
}

#[no_mangle]
pub extern "C" fn osNotifyEvent(arg0: *mut c_void, arg1: *mut c_void, arg2: NvU32, arg3: NvU32, arg4: NV_STATUS, arg5: NvBool) -> *mut c_void {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    let _ = arg5;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osNumaAddGpuMemory(pOsGpuInfo: *mut c_void, offset: NvU64, size: NvU64, pNumaNodeId: *mut NvU32) -> NV_STATUS {
    let _ = pOsGpuInfo;
    let _ = offset;
    let _ = size;
    let _ = pNumaNodeId;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osNumaMemblockSize(arg0: *mut NvU64) -> NV_STATUS {
    let _ = arg0;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osNumaOnliningEnabled(arg0: *mut c_void) -> NvBool {
    let _ = arg0;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osNumaRemoveGpuMemory(pOsGpuInfo: *mut c_void, offset: NvU64, size: NvU64, numaNodeId: NvU32) {
    let _ = pOsGpuInfo;
    let _ = offset;
    let _ = size;
    let _ = numaNodeId;
}

#[no_mangle]
pub extern "C" fn osObjectEventNotification(arg0: *mut c_void, arg1: *mut c_void, arg2: NvU32, arg3: *mut c_void, arg4: NvU32, arg5: *mut c_void, arg6: NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
    let _ = arg5;
    let _ = arg6;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osOfflinePageAtAddress(address: NvU64) -> NV_STATUS {
    let _ = address;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osOpenTemporaryFile(ppFile: *mut *mut c_void) -> NV_STATUS {
    let _ = ppFile;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osPackageRegistry(arg0: *mut c_void, arg1: *mut c_void, arg2: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osPciInitHandle(domain: NvU32, bus: NvU8, slot: NvU8, function: NvU8, pVendor: *mut NvU16, pDevice: *mut NvU16) -> *mut c_void {
    crate::os_interface::os_pci_init_handle(domain, bus, slot, function, pVendor, pDevice)
}

#[no_mangle]
pub extern "C" fn osPutPidInfo(pOsPidInfo: *mut c_void) {
    let _ = pOsPidInfo;
}

#[no_mangle]
pub extern "C" fn osQueueDrainP2PHandler(arg0: *mut NvU8) -> NV_STATUS {
    let _ = arg0;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osQueueMMUFaultHandler(arg0: *mut c_void) {
    let _ = arg0;
}

#[no_mangle]
pub extern "C" fn osQueueResumeP2PHandler(arg0: *mut NvU8) {
    let _ = arg0;
}

#[no_mangle]
pub extern "C" fn osQueueSystemWorkItem(arg0: *mut c_void, arg1: *mut c_void) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osQueueWorkItem(pGpu: *mut c_void, pFunction: *mut c_void, pParams: *mut c_void, flags: *mut c_void) -> NV_STATUS {
    let _ = pGpu;
    let _ = pFunction;
    let _ = pParams;
    let _ = flags;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osReadFromFile(pFile: *mut c_void, buffer: *mut NvU8, size: NvU64, offset: NvU64) -> NV_STATUS {
    let _ = pFile;
    let _ = buffer;
    let _ = size;
    let _ = offset;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osReadRegistryBinary(arg0: *mut c_void, arg1: *mut c_char, arg2: *mut NvU8, arg3: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osReadRegistryDwordBase(arg0: *mut c_void, arg1: *mut c_char, arg2: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osReadRegistryStringBase(arg0: *mut c_void, arg1: *mut c_char, arg2: *mut NvU8, arg3: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osReadRegistryVolatile(arg0: *mut c_void, arg1: *mut c_char, arg2: *mut NvU8, arg3: NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osReadRegistryVolatileSize(arg0: *mut c_void, arg1: *mut c_char, arg2: *mut NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osRefGpuAccessNeeded(pOsGpuInfo: *mut c_void) -> NV_STATUS {
    let _ = pOsGpuInfo;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osReleaseGpuOsInfo(pOsInfo: *mut c_void) {
    let _ = pOsInfo;
}

#[no_mangle]
pub extern "C" fn osReleaseRmSema(arg0: *mut c_void, arg1: *mut c_void) -> NvU32 {
    let _ = arg0;
    let _ = arg1;
    0
}

#[no_mangle]
pub extern "C" fn osRemoveGpu(domain: NvU32, bus: NvU8, device: NvU8) {
    let _ = domain;
    let _ = bus;
    let _ = device;
}

#[no_mangle]
pub extern "C" fn osRemoveGpuSupported() -> NvBool {
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osRmCapAcquire(pOsRmCaps: *mut c_void, rmCap: NvU32, capDescriptor: NvU64, dupedCapDescriptor: *mut NvU64) -> NV_STATUS {
    let _ = pOsRmCaps;
    let _ = rmCap;
    let _ = capDescriptor;
    let _ = dupedCapDescriptor;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osRmCapInitDescriptor(pCapDescriptor: *mut NvU64) {
    let _ = pCapDescriptor;
}

#[no_mangle]
pub extern "C" fn osRmCapRegisterGpu(pOsGpuInfo: *mut c_void, ppOsRmCaps: *mut *mut c_void) -> NV_STATUS {
    let _ = pOsGpuInfo;
    let _ = ppOsRmCaps;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osRmCapRegisterSmcExecutionPartition(pPartitionOsRmCaps: *mut c_void, ppExecPartitionOsRmCaps: *mut *mut c_void, execPartitionId: NvU32) -> NV_STATUS {
    let _ = pPartitionOsRmCaps;
    let _ = ppExecPartitionOsRmCaps;
    let _ = execPartitionId;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osRmCapRegisterSmcPartition(pGpuOsRmCaps: *mut c_void, ppPartitionOsRmCaps: *mut *mut c_void, partitionId: NvU32) -> NV_STATUS {
    let _ = pGpuOsRmCaps;
    let _ = ppPartitionOsRmCaps;
    let _ = partitionId;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osRmCapRegisterSys(ppOsRmCaps: *mut *mut c_void) -> NV_STATUS {
    let _ = ppOsRmCaps;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osRmCapRelease(dupedCapDescriptor: NvU64) {
    let _ = dupedCapDescriptor;
}

#[no_mangle]
pub extern "C" fn osRmCapUnregister(ppOsRmCaps: *mut *mut c_void) {
    let _ = ppOsRmCaps;
}

// osRmInitRm is NOT stubbed here: sysConstruct_IMPL (system.c) calls it
// during OBJSYS construction and a NOT_SUPPORTED return would fail the
// whole construction. The real Eclipse implementation (REGISTER_ALL_HALS
// + threadStateInitSetupFlags, mirroring Linux's osinit.c version) lives
// in vendor/eclipse_rm_init.c where the generated g_hal_register.h
// inline registration functions are reachable from C.

#[no_mangle]
pub extern "C" fn osSetEvent(arg0: *mut c_void, arg1: *mut c_void) -> NvU32 {
    let _ = arg0;
    let _ = arg1;
    0
}

#[no_mangle]
pub extern "C" fn osSpinLoop() {

}

#[no_mangle]
pub extern "C" fn osStartNanoTimer(pArg1: *mut c_void, pTimer: *mut c_void, timeNs: NvU64) -> NV_STATUS {
    let _ = pArg1;
    let _ = pTimer;
    let _ = timeNs;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osSyncWithGpuDestroy(arg0: NvBool) {
    let _ = arg0;
}

#[no_mangle]
pub extern "C" fn osSyncWithRmDestroy() {

}

#[no_mangle]
pub extern "C" fn osTegraAllocateDisplayBandwidth(pOsGpuInfo: *mut c_void, averageBandwidthKBPS: NvU32, floorBandwidthKBPS: NvU32) -> NV_STATUS {
    let _ = pOsGpuInfo;
    let _ = averageBandwidthKBPS;
    let _ = floorBandwidthKBPS;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osTegraDceClientIpcSendRecv(clientId: NvU32, msg: *mut c_void, msgLength: NvU32) -> NV_STATUS {
    let _ = clientId;
    let _ = msg;
    let _ = msgLength;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osTegraDceRegisterIpcClient(interfaceType: NvU32, usrCtx: *mut c_void, clientId: *mut NvU32) -> NV_STATUS {
    let _ = interfaceType;
    let _ = usrCtx;
    let _ = clientId;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osTegraDceUnregisterIpcClient(clientId: NvU32) -> NV_STATUS {
    let _ = clientId;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osTegraSocGetDispClockRates(pOsGpuInfo: *mut c_void, pMaxDispClkRateDisppll: *mut NvU32, pMaxDispClkRateSppllClkouta: *mut NvU32, pMaxHubClkRateSppllClkoutb: *mut NvU32) -> NV_STATUS {
    let _ = pOsGpuInfo;
    let _ = pMaxDispClkRateDisppll;
    let _ = pMaxDispClkRateSppllClkouta;
    let _ = pMaxHubClkRateSppllClkoutb;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osTegraSocGetImpImportData(pGpu: *mut c_void, pTegraImpImportData: *mut c_void) -> NV_STATUS {
    let _ = pGpu;
    let _ = pTegraImpImportData;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osTegraSocGetImpUefiData(pOsGpuInfo: *mut c_void, pIsoBwKbps: *mut NvU32, pFloorBwKbps: *mut NvU32) -> NV_STATUS {
    let _ = pOsGpuInfo;
    let _ = pIsoBwKbps;
    let _ = pFloorBwKbps;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osTegraiGpuPerfBoost(pGpu: *mut c_void, enable: NvBool, duration: NvU32) -> NV_STATUS {
    let _ = pGpu;
    let _ = enable;
    let _ = duration;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osTestPcieExtendedConfigAccess(handle: *mut c_void, offset: NvU32) -> NvBool {
    let _ = handle;
    let _ = offset;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osUidTokensEqual(arg1: *mut c_void, arg2: *mut c_void) -> NvBool {
    let _ = arg1;
    let _ = arg2;
    NV_FALSE
}

#[no_mangle]
pub extern "C" fn osUnlockMem(arg0: *mut c_void) -> NV_STATUS {
    let _ = arg0;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osUnmapGPU(arg0: *mut c_void, arg1: *mut c_void, arg2: *mut c_void, arg3: NvU64, arg4: *mut c_void) {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    let _ = arg4;
}

#[no_mangle]
pub extern "C" fn osUnmapKernelSpace(addr: *mut c_void, size: NvU64) {
    crate::os_interface::os_unmap_kernel_space(addr, size)
}

#[no_mangle]
pub extern "C" fn osUnmapPciMemoryKernel64(arg0: *mut c_void, arg1: *mut c_void) {
    let _ = arg0;
    let _ = arg1;
}

#[no_mangle]
pub extern "C" fn osUnmapPciMemoryKernelOld(arg0: *mut c_void, arg1: *mut c_void) {
    let _ = arg0;
    let _ = arg1;
}

#[no_mangle]
pub extern "C" fn osUnmapPciMemoryUser(arg0: *mut c_void, arg1: *mut c_void, arg2: NvU64, arg3: *mut c_void) {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
}

#[no_mangle]
pub extern "C" fn osUnmapSystemMemory(arg0: *mut c_void, arg1: NvBool, arg2: *mut c_void, arg3: *mut c_void) {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
}

#[no_mangle]
pub extern "C" fn osUnrefGpuAccessNeeded(pOsGpuInfo: *mut c_void) {
    let _ = pOsGpuInfo;
}

#[no_mangle]
pub extern "C" fn osUserHandleToKernelPtr(hClient: NvU32, Handle: *mut c_void, pHandle: *mut c_void) -> NV_STATUS {
    let _ = hClient;
    let _ = Handle;
    let _ = pHandle;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osValidateClientTokens(arg1: *mut c_void, arg2: *mut c_void) -> NV_STATUS {
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osWaitInterruptible(pWq: *mut c_void) {
    let _ = pWq;
}

#[no_mangle]
pub extern "C" fn osWakeRemoveVgpu(arg0: NvU32, arg1: NvU32) {
    let _ = arg0;
    let _ = arg1;
}

#[no_mangle]
pub extern "C" fn osWakeUp(pWq: *mut c_void) {
    let _ = pWq;
}

#[no_mangle]
pub extern "C" fn osWriteRegistryDword(arg0: *mut c_void, arg1: *mut c_char, arg2: NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osWriteRegistryVolatile(arg0: *mut c_void, arg1: *mut c_char, arg2: *mut NvU8, arg3: NvU32) -> NV_STATUS {
    let _ = arg0;
    let _ = arg1;
    let _ = arg2;
    let _ = arg3;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn osWriteToFile(pFile: *mut c_void, buffer: *mut NvU8, size: NvU64, offset: NvU64) -> NV_STATUS {
    let _ = pFile;
    let _ = buffer;
    let _ = size;
    let _ = offset;
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub extern "C" fn os_get_system_time(arg0: *mut NvU32, arg1: *mut NvU32) -> *mut c_void {
    let _ = arg0;
    let _ = arg1;
    core::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn osPciReadByte(pHandle: *mut c_void, offset: NvU32) -> NvU8 {
    with_hooks(0xFF, |h| h.pci_config_read(pHandle as usize, offset, 1)) as NvU8
}

#[no_mangle]
pub extern "C" fn osPciReadWord(pHandle: *mut c_void, offset: NvU32) -> NvU16 {
    with_hooks(0xFFFF, |h| h.pci_config_read(pHandle as usize, offset, 2)) as NvU16
}

#[no_mangle]
pub extern "C" fn osPciReadDword(pHandle: *mut c_void, offset: NvU32) -> NvU32 {
    with_hooks(0xFFFF_FFFF, |h| h.pci_config_read(pHandle as usize, offset, 4))
}

#[no_mangle]
pub extern "C" fn osPciWriteWord(pHandle: *mut c_void, offset: NvU32, value: NvU16) {
    with_hooks((), |h| h.pci_config_write(pHandle as usize, offset, 2, value as u32));
}

#[no_mangle]
pub extern "C" fn osPciWriteDword(pHandle: *mut c_void, offset: NvU32, value: NvU32) {
    with_hooks((), |h| h.pci_config_write(pHandle as usize, offset, 4, value));
}

#[no_mangle]
pub extern "C" fn osMapKernelSpace(start: NvU64, size: NvU64, _mode: NvU32, _protect: NvU32) -> *mut c_void {
    crate::os_interface::os_map_kernel_space(start, size, _mode)
}

// The functions below surfaced as real link failures on actual hardware
// builds: they're referenced from vendored core files (os_init.c, cpu.c,
// phys_mem_allocator.c, locks.c) but aren't part of the open-sourced
// Linux platform glue (arch/nvalloc/unix/src/), so they were missed by
// the original os_boundary.rs sweep. Real declarations transcribed from
// g_os_nvoc.h.
//
// Note: osNv_rdxcr0 is NOT implemented here even though it's declared
// right alongside osNv_rdcr4/osNv_cpuid -- unlike those two, the real
// os_stubs.c (vendored, always compiled) defines it unconditionally
// (no RMCFG_FEATURE_PLATFORM_UNIX/NVCPU_IS_X86_64 guard), so providing
// it here is a duplicate-symbol link error, confirmed against a real
// build. osNv_rdcr4/osNv_cpuid ARE guarded out by os_stubs.c specifically
// for RMCFG_FEATURE_PLATFORM_UNIX && NVCPU_IS_X86_64, which is why they
// were genuinely missing.
//
// osNv_cpuid/osNv_rdcr4 are genuine host-CPU operations -- Eclipse runs
// directly on the x86_64 CPU, so these execute the real instructions
// rather than stubbing, unlike GPU-hardware-dependent calls.

#[no_mangle]
pub extern "C" fn osInitObjOS(pOS: *mut c_void) {
    // Real Linux implementations of this hook are platform-specific
    // bookkeeping (not part of the open-sourced RM core); OBJOS is
    // already zero-initialized by NVOC construction, so there is
    // nothing Eclipse needs to add here.
    let _ = pOS;
}

#[no_mangle]
pub extern "C" fn osNv_rdcr4() -> NvU32 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr4", out(reg) value);
    }
    value as NvU32
}

#[no_mangle]
pub extern "C" fn osNv_cpuid(
    leaf: i32,
    subleaf: i32,
    peax: *mut NvU32,
    pebx: *mut NvU32,
    pecx: *mut NvU32,
    pedx: *mut NvU32,
) -> i32 {
    let a: u32;
    let b: u32;
    let c: u32;
    let d: u32;
    unsafe {
        core::arch::asm!(
            "mov {tmp:e}, ebx",
            "cpuid",
            "xchg {tmp:e}, ebx",
            tmp = out(reg) b,
            inout("eax") leaf as u32 => a,
            inout("ecx") subleaf as u32 => c,
            out("edx") d,
        );
        if !peax.is_null() { *peax = a; }
        if !pebx.is_null() { *pebx = b; }
        if !pecx.is_null() { *pecx = c; }
        if !pedx.is_null() { *pedx = d; }
    }
    1
}

#[no_mangle]
pub extern "C" fn osGetNumaMemoryUsage(
    numa_id: NvS32,
    free_memory_bytes: *mut NvU64,
    total_memory_bytes: *mut NvU64,
) {
    // STUB: Eclipse has no NUMA concept (single discrete GPU, no ACPI
    // NUMA topology), matching the existing os_get_numa_node_memory_usage
    // precedent in os_interface.rs.
    let _ = numa_id;
    unsafe {
        if !free_memory_bytes.is_null() {
            *free_memory_bytes = 0;
        }
        if !total_memory_bytes.is_null() {
            *total_memory_bytes = 0;
        }
    }
}

#[no_mangle]
pub extern "C" fn osGpuLocksQueueRelease(pGpu: *mut c_void, dpc_gpu_lock_release: NvU32) -> NV_STATUS {
    // Real Linux implementation defers lock release to a deferred
    // procedure call (DPC) queue for interrupt-context safety; Eclipse's
    // lock implementation doesn't need that indirection, so this is a
    // no-op that reports success (locks are released synchronously by
    // the caller elsewhere).
    let _ = pGpu;
    let _ = dpc_gpu_lock_release;
    NV_OK
}
