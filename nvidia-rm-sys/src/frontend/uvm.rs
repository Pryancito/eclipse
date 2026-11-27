//! What `/dev/nvidia-uvm` asks of the RM.
//!
//! On Linux nvidia-uvm.ko owns the page tables of a GPU VA space a client
//! registers with it, and writes every PTE CUDA's mappings there need. This
//! port has no UVM page-table code: the RM keeps those page tables, and each
//! UVM mapping becomes an RM mapping, made as the user. The C half, and why,
//! is "UVM" in eclipse_rm_frontend.c.
//!
//! Every call is made under the RM gate ([`CallScope`]) with the caller's
//! privilege and GPUs, and in the caller's own client, which the RM checks
//! belongs to the `/dev/nvidiactl` open [`Caller::os_info`] names.
//!
//! A semaphore pool's pages are the kernel's. [`pages`] lends them to the RM
//! for the length of the one call that makes RM memory of them, and the RM
//! keeps a hold on them ([`eclipse_uvm_pages_hold`]) for as long as that
//! memory lives, so they go back to the allocator only once neither the
//! kernel nor the RM has them.
//!
//! A SKED reflected range is video memory of the user's client mapped with
//! the kind of a message to SKED ([`sked_reflected`]).
use super::{CallScope, Caller};
use crate::types::*;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};
use lock::Mutex;

/// `NV_ERR_INVALID_CLIENT` (nvstatuscodes.h).
pub const NV_ERR_INVALID_CLIENT: NV_STATUS = 0x0000_0023;

#[cfg(nvrm_real)]
extern "C" {
    fn eclipse_rm_frontend_uvm_va_space(
        os_info: *mut c_void,
        h_client: NvU32,
        h_va_space: NvU32,
        gpu_instance: NvU32,
        h_device: *mut NvU32,
    ) -> NV_STATUS;
    fn eclipse_rm_frontend_uvm_map(
        os_info: *mut c_void,
        admin: NvU32,
        h_client: NvU32,
        h_device: NvU32,
        h_va_space: NvU32,
        h_memory: NvU32,
        offset: NvU64,
        base: NvU64,
        length: NvU64,
        read_only: NvU32,
        h_virtual: *mut NvU32,
    ) -> NV_STATUS;
    fn eclipse_rm_frontend_uvm_map_failure(out: *mut MapFailure) -> NvU32;
    fn eclipse_rm_frontend_uvm_free(
        os_info: *mut c_void,
        admin: NvU32,
        h_client: NvU32,
        h_object: NvU32,
    ) -> NV_STATUS;
    fn eclipse_rm_frontend_uvm_pages(
        os_info: *mut c_void,
        h_client: NvU32,
        h_device: NvU32,
        phys: NvU64,
        size: NvU64,
        gpu_cached: NvU32,
        h_memory: *mut NvU32,
    ) -> NV_STATUS;
    fn eclipse_rm_frontend_uvm_user_pages(
        os_info: *mut c_void,
        h_client: NvU32,
        h_device: NvU32,
        va: NvU64,
        size: NvU64,
        gpu_cached: NvU32,
        h_memory: *mut NvU32,
    ) -> NV_STATUS;
    fn eclipse_rm_frontend_uvm_sked(
        os_info: *mut c_void,
        admin: NvU32,
        h_client: NvU32,
        h_device: NvU32,
        h_va_space: NvU32,
        base: NvU64,
        length: NvU64,
        h_memory: *mut NvU32,
        h_virtual: *mut NvU32,
    ) -> NV_STATUS;
    fn eclipse_rm_frontend_uvm_p2p_caps(
        gpu_instance_a: NvU32,
        gpu_instance_b: NvU32,
        read: *mut u8,
        write: *mut u8,
        atomics: *mut u8,
        connectivity: *mut NvU32,
    ) -> NV_STATUS;
}

// Hosted builds compile no RM C (build.rs): no client exists, so there is
// none to find a VA space in, map in or free from.
#[cfg(not(nvrm_real))]
unsafe fn eclipse_rm_frontend_uvm_va_space(
    _os_info: *mut c_void,
    _h_client: NvU32,
    _h_va_space: NvU32,
    _gpu_instance: NvU32,
    _h_device: *mut NvU32,
) -> NV_STATUS {
    NV_ERR_INVALID_CLIENT
}
#[cfg(not(nvrm_real))]
#[allow(clippy::too_many_arguments)]
unsafe fn eclipse_rm_frontend_uvm_map(
    _os_info: *mut c_void,
    _admin: NvU32,
    _h_client: NvU32,
    _h_device: NvU32,
    _h_va_space: NvU32,
    _h_memory: NvU32,
    _offset: NvU64,
    _base: NvU64,
    _length: NvU64,
    _read_only: NvU32,
    _h_virtual: *mut NvU32,
) -> NV_STATUS {
    NV_ERR_INVALID_CLIENT
}
#[cfg(not(nvrm_real))]
unsafe fn eclipse_rm_frontend_uvm_map_failure(_out: *mut MapFailure) -> NvU32 {
    0
}
#[cfg(not(nvrm_real))]
unsafe fn eclipse_rm_frontend_uvm_free(
    _os_info: *mut c_void,
    _admin: NvU32,
    _h_client: NvU32,
    _h_object: NvU32,
) -> NV_STATUS {
    NV_ERR_INVALID_CLIENT
}
#[cfg(not(nvrm_real))]
unsafe fn eclipse_rm_frontend_uvm_pages(
    _os_info: *mut c_void,
    _h_client: NvU32,
    _h_device: NvU32,
    _phys: NvU64,
    _size: NvU64,
    _gpu_cached: NvU32,
    _h_memory: *mut NvU32,
) -> NV_STATUS {
    NV_ERR_INVALID_CLIENT
}
#[cfg(not(nvrm_real))]
unsafe fn eclipse_rm_frontend_uvm_user_pages(
    _os_info: *mut c_void,
    _h_client: NvU32,
    _h_device: NvU32,
    _va: NvU64,
    _size: NvU64,
    _gpu_cached: NvU32,
    _h_memory: *mut NvU32,
) -> NV_STATUS {
    NV_ERR_INVALID_CLIENT
}
#[cfg(not(nvrm_real))]
#[allow(clippy::too_many_arguments)]
unsafe fn eclipse_rm_frontend_uvm_sked(
    _os_info: *mut c_void,
    _admin: NvU32,
    _h_client: NvU32,
    _h_device: NvU32,
    _h_va_space: NvU32,
    _base: NvU64,
    _length: NvU64,
    _h_memory: *mut NvU32,
    _h_virtual: *mut NvU32,
) -> NV_STATUS {
    NV_ERR_INVALID_CLIENT
}

#[cfg(not(nvrm_real))]
unsafe fn eclipse_rm_frontend_uvm_p2p_caps(
    _gpu_instance_a: NvU32,
    _gpu_instance_b: NvU32,
    _read: *mut u8,
    _write: *mut u8,
    _atomics: *mut u8,
    _connectivity: *mut NvU32,
) -> NV_STATUS {
    NV_ERR_INVALID_DEVICE
}

/// `NV_ERR_INVALID_DEVICE` (nvstatuscodes.h).
#[cfg(not(nvrm_real))]
const NV_ERR_INVALID_DEVICE: NV_STATUS = 0x0000_0026;

/// What the RM says of peer access between two GPUs: each
/// `NV0000_P2P_CAPS_STATUS_*` (0 is OK) and the `P2P_CONNECTIVITY` it goes
/// over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct P2pCaps {
    pub read: u8,
    pub write: u8,
    pub atomics: u8,
    pub connectivity: u32,
}

impl P2pCaps {
    /// Whether peer access is possible both ways, as nvidia-uvm counts it
    /// (`UVM_LINK_TYPE_PCIE` or better): reads and writes both OK.
    pub fn supported(&self) -> bool {
        self.read == 0 && self.write == 0
    }
}

/// The RM's P2P caps between RM GPU instances `a` and `b`, as
/// `NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS` answers libcuda.
pub fn p2p_caps(a: u32, b: u32) -> Result<P2pCaps, NV_STATUS> {
    let mut caps = P2pCaps::default();
    let status = {
        let _scope = CallScope::enter_kernel();
        unsafe {
            eclipse_rm_frontend_uvm_p2p_caps(
                a,
                b,
                &mut caps.read,
                &mut caps.write,
                &mut caps.atomics,
                &mut caps.connectivity,
            )
        }
    };
    if status == NV_OK {
        Ok(caps)
    } else {
        Err(status)
    }
}

/// UVM_REGISTER_GPU_VASPACE's check: `h_va_space` must be a FERMI_VASPACE_A
/// of `h_client`, a client of the caller's open, on RM GPU instance
/// `gpu_instance`. The handle of the device it hangs from, which its
/// mappings are made under.
pub fn va_space_device(
    caller: &Caller,
    h_client: u32,
    h_va_space: u32,
    gpu_instance: u32,
) -> Result<u32, NV_STATUS> {
    if caller.os_info == 0 {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    let mut h_device = 0;
    let _scope = CallScope::enter(caller);
    let status = unsafe {
        eclipse_rm_frontend_uvm_va_space(
            caller.os_info as *mut c_void,
            h_client,
            h_va_space,
            gpu_instance,
            &mut h_device,
        )
    };
    if status == NV_OK {
        Ok(h_device)
    } else {
        Err(status)
    }
}

/// One GPU's half of a UVM external mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExternalMapping {
    pub h_client: u32,
    /// The device the VA space hangs from ([`va_space_device`]).
    pub h_device: u32,
    pub h_va_space: u32,
    /// The memory mapped, of `h_client`.
    pub h_memory: u32,
    /// Where in the memory the mapping starts.
    pub offset: u64,
    /// The GPU virtual address it is mapped at.
    pub base: u64,
    pub length: u64,
    pub read_only: bool,
}

/// Mirror of `EclipseUvmMapFailure` (eclipse_rm_frontend.c): what the last
/// failed [`map_external`] was asked, where it stopped, and the GPU VA
/// space's ranges. The RM answers every reason it cannot place a mapping's
/// virtual allocation at its fixed address with one
/// NV_ERR_INSUFFICIENT_RESOURCES; these say which.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MapFailure {
    /// 1 the virtual allocation, 2 the map, 3 the RM moved the address.
    pub stage: NvU32,
    pub status: NvU32,
    pub small_pages: NvU32,
    /// The VA space was found, and the ranges below are its.
    pub ranges: NvU32,
    pub base: NvU64,
    pub length: NvU64,
    pub offset: NvU64,
    pub va_start: NvU64,
    pub va_limit: NvU64,
    pub internal_start: NvU64,
    pub internal_limit: NvU64,
    pub server_start: NvU64,
    pub server_limit: NvU64,
}

/// The last failure of [`map_external`], until [`take_map_failure`].
static MAP_FAILURE: lock::Mutex<Option<MapFailure>> = lock::Mutex::new(None);

/// What the last failed [`map_external`] recorded, once.
pub fn take_map_failure() -> Option<MapFailure> {
    MAP_FAILURE.lock().take()
}

/// Map `m`: the handle of the NV50_MEMORY_VIRTUAL holding the mapping, which
/// [`free`] unmaps. Nothing is left behind on failure, but what it was is
/// kept for [`take_map_failure`].
pub fn map_external(caller: &Caller, m: &ExternalMapping) -> Result<u32, NV_STATUS> {
    if caller.os_info == 0 {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    let mut h_virtual = 0;
    let _scope = CallScope::enter(caller);
    let status = unsafe {
        eclipse_rm_frontend_uvm_map(
            caller.os_info as *mut c_void,
            caller.admin as NvU32,
            m.h_client,
            m.h_device,
            m.h_va_space,
            m.h_memory,
            m.offset,
            m.base,
            m.length,
            m.read_only as NvU32,
            &mut h_virtual,
        )
    };
    if status == NV_OK {
        Ok(h_virtual)
    } else {
        let mut failure = MapFailure::default();
        if unsafe { eclipse_rm_frontend_uvm_map_failure(&mut failure) } != 0 {
            *MAP_FAILURE.lock() = Some(failure);
        }
        Err(status)
    }
}

/// Free `h_object` of `h_client`, an object [`map_external`], [`pages`] or
/// [`sked_reflected`] made.
pub fn free(caller: &Caller, h_client: u32, h_object: u32) -> NV_STATUS {
    if caller.os_info == 0 {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let _scope = CallScope::enter(caller);
    unsafe {
        eclipse_rm_frontend_uvm_free(
            caller.os_info as *mut c_void,
            caller.admin as NvU32,
            h_client,
            h_object,
        )
    }
}

/// One GPU's SKED reflected range (UVM_MAP_DYNAMIC_PARALLELISM_REGION):
/// `length` bytes at `base`, one PTE's worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkedRange {
    pub h_client: u32,
    /// The device the VA space hangs from ([`va_space_device`]).
    pub h_device: u32,
    pub h_va_space: u32,
    /// The GPU virtual address it is mapped at.
    pub base: u64,
    pub length: u64,
}

/// Map `r`: the handles of the video memory behind it and of the
/// NV50_MEMORY_VIRTUAL holding the mapping, which [`free`] frees in that
/// order reversed. Nothing is left behind on failure.
pub fn sked_reflected(caller: &Caller, r: &SkedRange) -> Result<(u32, u32), NV_STATUS> {
    if caller.os_info == 0 {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    let (mut h_memory, mut h_virtual) = (0, 0);
    let _scope = CallScope::enter(caller);
    let status = unsafe {
        eclipse_rm_frontend_uvm_sked(
            caller.os_info as *mut c_void,
            caller.admin as NvU32,
            r.h_client,
            r.h_device,
            r.h_va_space,
            r.base,
            r.length,
            &mut h_memory,
            &mut h_virtual,
        )
    };
    if status == NV_OK {
        Ok((h_memory, h_virtual))
    } else {
        Err(status)
    }
}

/// Pages of the kernel's: `size` bytes from physical `phys`, one contiguous
/// block, which `keep` keeps allocated.
pub struct Pages<'a> {
    pub phys: u64,
    pub size: u64,
    pub keep: &'a Arc<dyn Any + Send + Sync>,
}

/// `pages` as RM memory of `h_client` under `h_device`, cached by the GPU
/// when `gpu_cached`: the handle of an NV01_MEMORY_SYSTEM_OS_DESCRIPTOR,
/// which [`map_external`] maps and [`free`] frees. The RM keeps the pages
/// for as long as anything is made of that memory, past [`free`] if need be.
pub fn pages(
    caller: &Caller,
    h_client: u32,
    h_device: u32,
    pages: &Pages,
    gpu_cached: bool,
) -> Result<u32, NV_STATUS> {
    if caller.os_info == 0 {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    let _lent = Lent::new(pages);
    let mut h_memory = 0;
    let _scope = CallScope::enter(caller);
    let status = unsafe {
        eclipse_rm_frontend_uvm_pages(
            caller.os_info as *mut c_void,
            h_client,
            h_device,
            pages.phys,
            pages.size,
            gpu_cached as NvU32,
            &mut h_memory,
        )
    };
    if status == NV_OK {
        Ok(h_memory)
    } else {
        Err(status)
    }
}

/// `pages`, the process's own pages pinned by the kernel, as RM memory of
/// `h_client` under `h_device`, cached by the GPU when `gpu_cached`: the
/// handle of an NV01_MEMORY_SYSTEM_OS_DESCRIPTOR of their page array, which
/// [`map_external`] maps and [`free`] frees. A range of managed memory, on
/// one GPU. The RM keeps them pinned for as long as that memory lives;
/// pages it did not take are unpinned once the call is over.
pub fn user_pages(
    caller: &Caller,
    h_client: u32,
    h_device: u32,
    pages: super::user_pages::UserPages,
    gpu_cached: bool,
) -> Result<u32, NV_STATUS> {
    if caller.os_info == 0 {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    let va = pages.va as u64;
    let size = pages.phys.len() as u64 * 4096;
    let mut h_memory = 0;
    let (status, untaken) = {
        let _scope = CallScope::enter(caller);
        super::user_pages::lend(pages);
        let status = unsafe {
            eclipse_rm_frontend_uvm_user_pages(
                caller.os_info as *mut c_void,
                h_client,
                h_device,
                va,
                size,
                gpu_cached as NvU32,
                &mut h_memory,
            )
        };
        (status, super::user_pages::take_back())
    };
    // Unpinned with the gate open.
    drop(untaken);
    if status == NV_OK {
        Ok(h_memory)
    } else {
        Err(status)
    }
}

/// Pages lent to the RM.
static LENT: Mutex<Vec<Loan>> = Mutex::new(Vec::new());
/// The id of the last loan, which tells two loans of one block apart.
static LAST_LOAN: AtomicU64 = AtomicU64::new(0);

/// One block lent, with what keeps it allocated.
struct Loan {
    id: u64,
    phys: u64,
    size: u64,
    keep: Arc<dyn Any + Send + Sync>,
}

/// One loan, from [`pages`] until it returns: only then may the RM take a
/// hold on them.
struct Lent(u64);

impl Lent {
    fn new(pages: &Pages) -> Lent {
        let id = LAST_LOAN.fetch_add(1, Ordering::Relaxed) + 1;
        LENT.lock().push(Loan {
            id,
            phys: pages.phys,
            size: pages.size,
            keep: pages.keep.clone(),
        });
        Lent(id)
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        let gone: Vec<_> = {
            let mut lent = LENT.lock();
            let (gone, kept) = core::mem::take(&mut *lent)
                .into_iter()
                .partition(|loan| loan.id == self.0);
            *lent = kept;
            gone
        };
        // What keeps the pages is dropped with the lock released.
        drop(gone);
    }
}

/// The RM's hold on `size` bytes of pages from physical `phys`, while
/// exactly those are lent; null for any others (osCreateMemFromOsDescriptor,
/// eclipse_rm_mem.c, then makes no memory of them).
#[no_mangle]
pub extern "C" fn eclipse_uvm_pages_hold(phys: NvU64, size: NvU64) -> *mut c_void {
    let lent = LENT.lock();
    match lent
        .iter()
        .find(|loan| loan.phys == phys && loan.size == size)
    {
        Some(loan) => Box::into_raw(Box::new(loan.keep.clone())) as *mut c_void,
        None => core::ptr::null_mut(),
    }
}

/// A hold [`eclipse_uvm_pages_hold`] gave out, given back when the RM
/// destroys the memory made of the pages.
#[no_mangle]
pub extern "C" fn eclipse_uvm_pages_release(hold: *mut c_void) {
    if !hold.is_null() {
        drop(unsafe { Box::from_raw(hold as *mut Arc<dyn Any + Send + Sync>) });
    }
}

#[cfg(test)]
mod tests;
