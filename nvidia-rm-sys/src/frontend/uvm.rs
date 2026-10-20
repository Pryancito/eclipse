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

#[cfg(target_os = "none")]
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
}

// Hosted builds compile no RM C (build.rs): no client exists, so there is
// none to find a VA space in, map in or free from.
#[cfg(not(target_os = "none"))]
unsafe fn eclipse_rm_frontend_uvm_va_space(
    _os_info: *mut c_void,
    _h_client: NvU32,
    _h_va_space: NvU32,
    _gpu_instance: NvU32,
    _h_device: *mut NvU32,
) -> NV_STATUS {
    NV_ERR_INVALID_CLIENT
}
#[cfg(not(target_os = "none"))]
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
#[cfg(not(target_os = "none"))]
unsafe fn eclipse_rm_frontend_uvm_free(
    _os_info: *mut c_void,
    _admin: NvU32,
    _h_client: NvU32,
    _h_object: NvU32,
) -> NV_STATUS {
    NV_ERR_INVALID_CLIENT
}
#[cfg(not(target_os = "none"))]
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
    let _scope = CallScope::enter(caller.gpu_mask, caller.admin);
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

/// Map `m`: the handle of the NV50_MEMORY_VIRTUAL holding the mapping, which
/// [`free`] unmaps. Nothing is left behind on failure.
pub fn map_external(caller: &Caller, m: &ExternalMapping) -> Result<u32, NV_STATUS> {
    if caller.os_info == 0 {
        return Err(NV_ERR_INVALID_ARGUMENT);
    }
    let mut h_virtual = 0;
    let _scope = CallScope::enter(caller.gpu_mask, caller.admin);
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
        Err(status)
    }
}

/// Free `h_object` of `h_client`, an object [`map_external`] made.
pub fn free(caller: &Caller, h_client: u32, h_object: u32) -> NV_STATUS {
    if caller.os_info == 0 {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let _scope = CallScope::enter(caller.gpu_mask, caller.admin);
    unsafe {
        eclipse_rm_frontend_uvm_free(
            caller.os_info as *mut c_void,
            caller.admin as NvU32,
            h_client,
            h_object,
        )
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
    let _scope = CallScope::enter(caller.gpu_mask, caller.admin);
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
