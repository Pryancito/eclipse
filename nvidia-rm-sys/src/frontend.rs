//! The RM side of `/dev/nvidiactl`: what NVIDIA's own user-mode driver
//! (libcuda, libnvidia-ml, and through them `nvidia-smi` and every CUDA
//! program) needs from the Resource Manager, which is not the nouveau uAPI.
//!
//! On Linux a `NV_ESC_RM_*` ioctl goes `nvidia_ioctl` (nv.c) -> `rm_ioctl`
//! (osapi.c) -> `RmIoctl` (escape.c) -> `Nv04...WithSecInfo`, with the
//! parameters in user memory (`PARAM_LOCATION_USER`). Eclipse leaves the
//! whole Linux platform layer out (build.rs), so the C half of that path is
//! `vendor/eclipse_rm_frontend.c` and this is the Rust half: the gate, the
//! state the RM's OS callbacks need while a user-mode call is in flight, and
//! the version comparison.
//!
//! Two OS callbacks change meaning here, and ONLY while a front-end call is
//! in flight ([`CallScope`]):
//!
//! - `os_memcpy_from_user` / `os_memcpy_to_user` copy, after the kernel's
//!   validator ([`set_user_range_validator`]) accepts the user range. Outside
//!   a front-end call they keep answering `NV_ERR_NOT_SUPPORTED`, as they
//!   always have: nothing else in this port hands the RM a user pointer.
//! - `osIsGpuAccessible` answers from the GPUs the calling process has open,
//!   which is Linux's `nv_is_gpu_accessible` (an `iterate_fd` over the
//!   caller's files). Outside a call it keeps answering `NV_FALSE`; the RM
//!   only asks it for a caller below `RS_PRIV_LEVEL_KERNEL`, and every other
//!   RM call in this port is a kernel one.
//! - `osIsAdministrator` answers whether the caller's effective uid is 0
//!   (Linux asks `capable(CAP_SYS_ADMIN)`). Outside a call it keeps
//!   answering `NV_FALSE`.
//!
//! The C half keeps one more such state of its own: the user mappings
//! (`osMapPciMemoryUser`, and `osMapSystemMemory` for a user) answer only
//! while a call is in flight, which is what lets RM_MAP_MEMORY hand back a
//! mapping ([`rm_map_memory`]) for the kernel to record on the file the
//! client will mmap.
use crate::types::*;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// `NV_RM_API_VERSION_STRING_LENGTH` (nv-ioctl.h).
pub const VERSION_STRING_LENGTH: usize = 64;
/// `NV_RM_API_VERSION_CMD_RELAXED` (nv-ioctl.h): compare up to the first `.`.
pub const VERSION_CMD_RELAXED: u32 = b'1' as u32;
/// `NV_RM_API_VERSION_CMD_QUERY` (nv-ioctl.h): just report the RM's string.
pub const VERSION_CMD_QUERY: u32 = b'2' as u32;
/// `NV_RM_API_VERSION_REPLY_RECOGNIZED` (nv-ioctl.h).
pub const VERSION_REPLY_RECOGNIZED: u32 = 1;
/// `NV_ERR_INVALID_ADDRESS` (nvstatuscodes.h), what Linux's
/// `os_memcpy_from_user` answers when `copy_from_user` faults.
pub const NV_ERR_INVALID_ADDRESS: NV_STATUS = 0x0000_001E;

/// `NV_ERR_STATE_IN_USE` (nvstatuscodes.h): the file already holds a mapping.
pub const NV_ERR_STATE_IN_USE: NV_STATUS = 0x0000_0063;

/// `NV_ESC_RM_MAP_MEMORY` (nv_escape.h).
pub const RM_MAP_MEMORY: u32 = 0x4E;

/// The nv-ioctl.h payloads the kernel side lays out by hand, in the order
/// `eclipse_rm_frontend_layout` answers them, then the offset of the one
/// field it reads from RM_MAP_MEMORY's and the size of the mapping record
/// this file shares with the C ([`RawMmapContext`]).
pub const LAYOUT: [(&str, usize); 10] = [
    ("nv_ioctl_xfer_t", 16),
    ("nv_ioctl_card_info_t", 72),
    ("nv_ioctl_rm_api_version_t", 72),
    ("nv_ioctl_sys_params_t", 8),
    ("nv_ioctl_register_fd_t", 4),
    ("nv_ioctl_wait_open_complete_t", 8),
    ("nv_ioctl_status_code_t", 12),
    ("nv_ioctl_nvos33_parameters_with_fd", 56),
    ("nv_ioctl_nvos33_parameters_with_fd.fd", 48),
    ("EclipseRmMmapContext", 40),
];

/// `EclipseRmMmapContext` (eclipse_rm_frontend.c): one mapping RM_MAP_MEMORY
/// validated, as the C leaves it. [`MmapContext::from_raw`] reads it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawMmapContext {
    pub valid: NvU32,
    pub ctl: NvU32,
    pub gpu_instance: NvU32,
    pub memory_type: NvU32,
    pub caching: NvU32,
    pub prot: NvU32,
    pub start: NvU64,
    pub size: NvU64,
}

/// `NV_MEMORY_*` (nv_memory_type.h), the caching types a mapping can carry.
pub mod caching {
    pub const CACHED: u32 = 0;
    pub const UNCACHED: u32 = 1;
    pub const WRITECOMBINED: u32 = 2;
    pub const WRITEBACK: u32 = 5;
    pub const DEFAULT: u32 = 6;
    pub const UNCACHED_WEAK: u32 = 7;
}

/// `NV_PROTECT_WRITEABLE` (nv_memory_type.h).
const PROTECT_WRITEABLE: u32 = 2;
const PAGE: u64 = 4096;

/// What a mapping is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmapMemory {
    /// A client's system-memory allocation, recorded on `/dev/nvidiactl`.
    System,
    /// A window of a GPU's BAR0, recorded on that GPU's `/dev/nvidia<N>`.
    Registers,
    /// A window of a GPU's BAR1.
    Framebuffer,
}

/// How the CPU is to reach a mapping (`nv_encode_caching`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuCaching {
    WriteBack,
    WriteCombined,
    /// Uncached RAM.
    Uncached,
    /// Uncached device memory.
    UncachedDevice,
}

/// One validated user mapping: what `nv_add_mapping_context_to_file`
/// records on a file, and what `nvidia_mmap` then maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmapContext {
    pub memory: MmapMemory,
    /// The GPU whose node an aperture mapping is recorded on.
    pub gpu_instance: u32,
    /// `NV_MEMORY_*`: the aperture's, or the allocation's.
    pub caching: u32,
    pub writable: bool,
    /// First byte, page aligned.
    pub start: u64,
    /// Whole pages: exactly what an aperture mapping maps, and the most a
    /// system one may.
    pub size: u64,
}

impl MmapContext {
    /// The C's record, if it holds a mapping and holds it whole: page
    /// aligned, non-empty, inside the address space, and of a kind that
    /// agrees with the node it is for. Anything else is not mapped.
    pub fn from_raw(raw: &RawMmapContext) -> Option<MmapContext> {
        if raw.valid == 0 {
            return None;
        }
        let memory = match raw.memory_type {
            0 => MmapMemory::System,
            1 => MmapMemory::Registers,
            2 => MmapMemory::Framebuffer,
            _ => return None,
        };
        if (raw.ctl != 0) != (memory == MmapMemory::System) {
            return None;
        }
        if raw.size == 0 || !raw.start.is_multiple_of(PAGE) || !raw.size.is_multiple_of(PAGE) {
            return None;
        }
        raw.start.checked_add(raw.size)?;
        Some(MmapContext {
            memory,
            gpu_instance: raw.gpu_instance,
            caching: raw.caching,
            writable: raw.prot & PROTECT_WRITEABLE != 0,
            start: raw.start,
            size: raw.size,
        })
    }

    /// Whether it is recorded on `/dev/nvidiactl` rather than a GPU's node.
    pub fn on_ctl(&self) -> bool {
        self.memory == MmapMemory::System
    }

    /// `nvidia_mmap_helper`'s `nv_encode_caching`. Registers are always
    /// uncached. The framebuffer is write-combined when the RM asks for that
    /// or leaves it to the default, and uncached otherwise: what nvidia.ko
    /// falls back to when a type is not allowed there (caching is never
    /// allowed for a BAR).
    ///
    /// System memory departs from Linux. There the pages take the caching
    /// the allocation asked for, and `nv_alloc_pages` changes the kernel's
    /// own mapping of them to match. Eclipse's kernel mapping of an RM
    /// allocation is write-back unless `osAllocPagesInternal` made it
    /// uncached, so the client gets the same type the kernel has and the two
    /// never disagree about the same page: uncached for an uncached
    /// allocation, write-back for the rest. A write-combined request is
    /// therefore write-back here, which orders stores more strongly, and
    /// which the GPU sees anyway, PCIe reads of RAM being snooped.
    pub fn cpu_caching(&self) -> CpuCaching {
        match self.memory {
            MmapMemory::Registers => CpuCaching::UncachedDevice,
            MmapMemory::Framebuffer => match self.caching {
                caching::DEFAULT | caching::WRITECOMBINED => CpuCaching::WriteCombined,
                _ => CpuCaching::UncachedDevice,
            },
            MmapMemory::System => match self.caching {
                caching::UNCACHED => CpuCaching::Uncached,
                _ => CpuCaching::WriteBack,
            },
        }
    }
}

#[cfg(target_os = "none")]
extern "C" {
    fn eclipse_rm_frontend_escape(
        cmd: NvU32,
        data: *mut c_void,
        size: NvU32,
        os_info: *mut c_void,
        ctl: NvU32,
        admin: NvU32,
        mmap: *mut RawMmapContext,
    ) -> NV_STATUS;
    fn eclipse_rm_frontend_map_unrecorded(
        data: *mut c_void,
        size: NvU32,
        os_info: *mut c_void,
        admin: NvU32,
        why: NV_STATUS,
    );
    fn eclipse_rm_frontend_release(os_info: *mut c_void);
    fn eclipse_rm_frontend_version() -> *const c_char;
    fn eclipse_rm_frontend_gpu_id(gpu_instance: NvU32) -> NvU32;
    fn eclipse_rm_frontend_gpu_instance(gpu: *mut c_void) -> NvU32;
    fn eclipse_rm_frontend_layout(which: NvU32) -> NvU32;
}

// Hosted builds compile no RM C (build.rs), so the front end has no RM to
// reach: every escape is refused, nothing is ever filed under an open, and
// no GPU exists. Same shape as the `eclipse_rm_edid` stand-in in rm_init.rs.
// The tests can have RM_MAP_MEMORY answer with a record of their own, and
// see what was unmapped, to check what this file does with the C's answer.
#[cfg(not(target_os = "none"))]
unsafe fn eclipse_rm_frontend_escape(
    _cmd: NvU32,
    _data: *mut c_void,
    _size: NvU32,
    _os_info: *mut c_void,
    _ctl: NvU32,
    _admin: NvU32,
    _mmap: *mut RawMmapContext,
) -> NV_STATUS {
    #[cfg(test)]
    if let Some(status) = tests::stand_in_map(_mmap) {
        return status;
    }
    NV_ERR_NOT_SUPPORTED
}
#[cfg(not(target_os = "none"))]
unsafe fn eclipse_rm_frontend_map_unrecorded(
    _data: *mut c_void,
    _size: NvU32,
    _os_info: *mut c_void,
    _admin: NvU32,
    _why: NV_STATUS,
) {
    #[cfg(test)]
    tests::stand_in_unmapped(_why);
}
#[cfg(not(target_os = "none"))]
unsafe fn eclipse_rm_frontend_release(_os_info: *mut c_void) {}
#[cfg(not(target_os = "none"))]
unsafe fn eclipse_rm_frontend_version() -> *const c_char {
    core::ptr::null()
}
#[cfg(not(target_os = "none"))]
unsafe fn eclipse_rm_frontend_gpu_id(_gpu_instance: NvU32) -> NvU32 {
    u32::MAX
}
#[cfg(not(target_os = "none"))]
unsafe fn eclipse_rm_frontend_gpu_instance(_gpu: *mut c_void) -> NvU32 {
    u32::MAX
}
#[cfg(not(target_os = "none"))]
unsafe fn eclipse_rm_frontend_layout(which: NvU32) -> NvU32 {
    // No C to ask: answer with our own table, so a hosted build agrees with
    // itself and the comparison below is only ever a real one in the kernel.
    LAYOUT
        .get(which as usize)
        .map_or(0, |&(_, size)| size as NvU32)
}

/// Whether a front-end call is in flight. Set and cleared only by
/// [`CallScope`], which lives entirely inside the RM gate, so at most one
/// call can ever see it set.
static IN_CALL: AtomicBool = AtomicBool::new(false);
/// The RM GPU instances the caller has open, as a bitmask, for the length of
/// the call in flight.
static CALLER_GPUS: AtomicU32 = AtomicU32::new(0);
/// Whether the caller of the call in flight is an administrator.
static CALLER_ADMIN: AtomicBool = AtomicBool::new(false);
/// `fn(addr, len) -> bool`, registered by the kernel: whether `len` bytes at
/// `addr` are mapped user memory of the calling process. 0 = none yet, and
/// then every user copy is refused.
static USER_RANGE_OK: AtomicUsize = AtomicUsize::new(0);

/// Register the kernel's check for a user range. `nvidia-rm-sys` cannot
/// reach the address space itself (it sits below the kernel), so the copy
/// primitives ask through this.
pub fn set_user_range_validator(f: fn(usize, usize) -> bool) {
    USER_RANGE_OK.store(f as usize, Ordering::Release);
}

fn user_range_ok(addr: usize, len: usize) -> bool {
    let f = USER_RANGE_OK.load(Ordering::Acquire);
    if f == 0 {
        return false;
    }
    // SAFETY: only ever stored from `set_user_range_validator`.
    let f: fn(usize, usize) -> bool = unsafe { core::mem::transmute(f) };
    f(addr, len)
}

/// The state of one front-end call, in place for exactly as long as the
/// value lives. Holding one means holding the RM gate: the constructor is
/// private to [`rm_escape`] and [`release`].
struct CallScope {
    _gate: crate::rm_init::RmGate,
}

impl CallScope {
    fn enter(gpu_mask: u32, admin: bool) -> CallScope {
        let gate = crate::rm_init::RmGate::lock();
        CALLER_GPUS.store(gpu_mask, Ordering::SeqCst);
        CALLER_ADMIN.store(admin, Ordering::SeqCst);
        IN_CALL.store(true, Ordering::SeqCst);
        CallScope { _gate: gate }
    }
}

impl Drop for CallScope {
    fn drop(&mut self) {
        // Cleared before the gate opens (`_gate` drops after this body).
        IN_CALL.store(false, Ordering::SeqCst);
        CALLER_GPUS.store(0, Ordering::SeqCst);
        CALLER_ADMIN.store(false, Ordering::SeqCst);
    }
}

/// `os_memcpy_from_user`: Linux's `copy_from_user`, answering
/// `NV_ERR_INVALID_ADDRESS` for a range the caller does not own. Refused
/// with `NV_ERR_NOT_SUPPORTED` outside a front-end call.
pub(crate) fn copy_from_user(to: *mut u8, from: *const u8, n: usize) -> NV_STATUS {
    if !IN_CALL.load(Ordering::SeqCst) {
        return NV_ERR_NOT_SUPPORTED;
    }
    if to.is_null() || !user_range_ok(from as usize, n) {
        return NV_ERR_INVALID_ADDRESS;
    }
    // SAFETY: `to` is the RM's own buffer of `n` bytes, and the kernel's
    // validator just accepted `from..from + n` as mapped user memory of the
    // process whose syscall this is.
    unsafe { core::ptr::copy_nonoverlapping(from, to, n) };
    NV_OK
}

/// `os_memcpy_to_user`, the other direction of [`copy_from_user`].
pub(crate) fn copy_to_user(to: *mut u8, from: *const u8, n: usize) -> NV_STATUS {
    if !IN_CALL.load(Ordering::SeqCst) {
        return NV_ERR_NOT_SUPPORTED;
    }
    if from.is_null() || !user_range_ok(to as usize, n) {
        return NV_ERR_INVALID_ADDRESS;
    }
    // SAFETY: as in `copy_from_user`, with the roles swapped.
    unsafe { core::ptr::copy_nonoverlapping(from, to, n) };
    NV_OK
}

/// `osIsGpuAccessible`: during a front-end call, whether the caller has the
/// GPU's `/dev/nvidia<N>` open (Linux's `nv_is_gpu_accessible`). `NV_FALSE`
/// otherwise, which is what this port always answered.
pub(crate) fn gpu_accessible(gpu: *mut c_void) -> bool {
    if !IN_CALL.load(Ordering::SeqCst) || gpu.is_null() {
        return false;
    }
    let instance = unsafe { eclipse_rm_frontend_gpu_instance(gpu) };
    instance_in_mask(instance, CALLER_GPUS.load(Ordering::SeqCst))
}

/// `osIsAdministrator`: during a front-end call, whether its caller is an
/// administrator. `NV_FALSE` otherwise, which is what this port always
/// answered: only [`CallScope`] sets the flag, and it clears it on the way
/// out.
pub(crate) fn is_administrator() -> bool {
    CALLER_ADMIN.load(Ordering::SeqCst)
}

/// Whether `mask` has the bit of RM GPU instance `instance`. An instance past
/// the mask's 32 bits is never accessible, rather than wrapping onto bit 0.
pub fn instance_in_mask(instance: u32, mask: u32) -> bool {
    instance < 32 && mask & (1 << instance) != 0
}

/// Who is calling, for one escape.
#[derive(Clone, Copy, Debug)]
pub struct Caller {
    /// The open `/dev/nvidiactl` the escape came in on, as a token that is
    /// unique for as long as that open lives. Clients are filed under it.
    pub os_info: usize,
    /// Whether the escape came in on the control device. The RM escapes are
    /// control-device calls; on `/dev/nvidia<N>` they are refused, as on Linux.
    pub ctl: bool,
    /// The caller's effective uid is 0: `RS_PRIV_LEVEL_USER_ROOT` instead of
    /// `RS_PRIV_LEVEL_USER`.
    pub admin: bool,
    /// The RM GPU instances the calling process has open.
    pub gpu_mask: u32,
}

/// One `NV_ESC_RM_*` escape, on the kernel copy of its payload. The return
/// value is the escape's own status, as `RmIoctl`'s: `NV_OK` means the RM's
/// answer is in the payload's `status` field.
pub fn rm_escape(cmd: u32, payload: &mut [u8], caller: &Caller) -> NV_STATUS {
    if caller.os_info == 0 {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let _scope = CallScope::enter(caller.gpu_mask, caller.admin);
    unsafe {
        eclipse_rm_frontend_escape(
            cmd,
            payload.as_mut_ptr() as *mut c_void,
            payload.len() as NvU32,
            caller.os_info as *mut c_void,
            caller.ctl as NvU32,
            caller.admin as NvU32,
            core::ptr::null_mut(),
        )
    }
}

/// RM_MAP_MEMORY, on the kernel copy of its payload: the escape's status,
/// and the mapping to record on the file the client named, when the RM made
/// one. The caller records it, or hands the payload to [`map_unrecorded`].
///
/// A record the C left that does not hold up ([`MmapContext::from_raw`]) is
/// unmapped here, in the same call, and the client sees
/// `NV_ERR_INVALID_STATE`.
pub fn rm_map_memory(payload: &mut [u8], caller: &Caller) -> (NV_STATUS, Option<MmapContext>) {
    if caller.os_info == 0 {
        return (NV_ERR_INVALID_ARGUMENT, None);
    }
    let mut raw = RawMmapContext::default();
    let _scope = CallScope::enter(caller.gpu_mask, caller.admin);
    let status = unsafe {
        eclipse_rm_frontend_escape(
            RM_MAP_MEMORY,
            payload.as_mut_ptr() as *mut c_void,
            payload.len() as NvU32,
            caller.os_info as *mut c_void,
            caller.ctl as NvU32,
            caller.admin as NvU32,
            &mut raw,
        )
    };
    let mapping = MmapContext::from_raw(&raw);
    if raw.valid != 0 && mapping.is_none() {
        unsafe { unrecorded(payload, caller, NV_ERR_INVALID_STATE) };
    }
    (status, mapping)
}

/// The kernel could not record the mapping [`rm_map_memory`] returned
/// (`nv_add_mapping_context_to_file` failed with `why`): the RM unmaps it,
/// and `why` becomes the call's status, as escape.c does.
pub fn map_unrecorded(payload: &mut [u8], caller: &Caller, why: NV_STATUS) {
    if caller.os_info == 0 {
        return;
    }
    let _scope = CallScope::enter(caller.gpu_mask, caller.admin);
    unsafe { unrecorded(payload, caller, why) };
}

/// # Safety
/// The caller holds a [`CallScope`].
unsafe fn unrecorded(payload: &mut [u8], caller: &Caller, why: NV_STATUS) {
    eclipse_rm_frontend_map_unrecorded(
        payload.as_mut_ptr() as *mut c_void,
        payload.len() as NvU32,
        caller.os_info as *mut c_void,
        caller.admin as NvU32,
        why,
    )
}

/// The close of the `/dev/nvidiactl` open `os_info` names: every RM client
/// filed under it is freed, with everything allocated under them.
pub fn release(os_info: usize) {
    if os_info == 0 {
        return;
    }
    // Linux frees on close with `RS_PRIV_LEVEL_KERNEL`'s locks and no caller
    // to ask about: no GPU is open and nobody is an administrator.
    let _scope = CallScope::enter(0, false);
    unsafe { eclipse_rm_frontend_release(os_info as *mut c_void) };
}

/// `NV_VERSION_STRING` of the RM this kernel carries, without its NUL, or
/// empty when there is no RM (hosted builds).
pub fn rm_version() -> &'static [u8] {
    let p = unsafe { eclipse_rm_frontend_version() };
    if p.is_null() {
        return &[];
    }
    // SAFETY: a C string literal in the RM image, NUL-terminated, static.
    unsafe { core::ffi::CStr::from_ptr(p) }.to_bytes()
}

/// The RM's id for GPU instance `instance` (`pGpu->gpuId`), or `u32::MAX`
/// (`NV0000_CTRL_GPU_INVALID_ID`) when it names no GPU.
pub fn gpu_id(instance: u32) -> u32 {
    let _gate = crate::rm_init::RmGate::lock();
    unsafe { eclipse_rm_frontend_gpu_id(instance) }
}

/// The first nv-ioctl.h payload whose C `sizeof` disagrees with
/// [`LAYOUT`], as `(name, ours, C's)`.
pub fn layout_mismatch() -> Option<(&'static str, usize, usize)> {
    LAYOUT.iter().enumerate().find_map(|(i, &(name, ours))| {
        let theirs = unsafe { eclipse_rm_frontend_layout(i as NvU32) } as usize;
        (theirs != ours).then_some((name, ours, theirs))
    })
}

/// `RmPerformVersionCheck` (osapi.c) over the payload's `cmd` and
/// `versionString`, against the RM's `rm` string.
///
/// Returns whether the client may proceed. On a QUERY `version` comes back
/// holding the RM's string; on a match it is untouched; on a mismatch it is
/// the client's own string, NUL-terminated at the last byte because it
/// cannot be trusted to be. Linux then prints that string in its "API
/// mismatch" line and only afterwards overwrites it with the RM's
/// ([`copy_version`]), so the caller does the same two steps.
pub fn version_check(cmd: u32, version: &mut [u8; VERSION_STRING_LENGTH], rm: &[u8]) -> bool {
    if cmd == VERSION_CMD_QUERY {
        copy_version(version, rm);
        return true;
    }
    let relaxed = cmd == VERSION_CMD_RELAXED;
    for (i, &client) in version.iter().enumerate() {
        // `rmStr[i]`: the C string's bytes, then its NUL.
        let ours = rm.get(i).copied().unwrap_or(0);
        if client != ours {
            break;
        }
        if relaxed && client == b'.' {
            return true;
        }
        if client == 0 {
            return true;
        }
    }
    version[VERSION_STRING_LENGTH - 1] = 0;
    false
}

/// `os_string_copy(pParams->versionString, rmStr)`: the RM's string and its
/// NUL, cut so the NUL always fits.
pub fn copy_version(version: &mut [u8; VERSION_STRING_LENGTH], rm: &[u8]) {
    let n = rm.len().min(VERSION_STRING_LENGTH - 1);
    version[..n].copy_from_slice(&rm[..n]);
    version[n] = 0;
}

#[cfg(test)]
mod tests;
