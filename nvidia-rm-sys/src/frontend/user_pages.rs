//! The pages behind a client's own memory, which RM_ALLOC_MEMORY of an OS
//! descriptor makes RM memory of.
//!
//! libcuda names a range of its address space. On Linux the RM's
//! `os_lock_user_pages` pins the pages behind it (`pin_user_pages`), the RM
//! makes memory of them, and that memory keeps them pinned until it is
//! destroyed (`os_unlock_user_pages`). This crate sits below the kernel and
//! cannot reach an address space, so the kernel pins the range before the
//! call and lends it ([`rm_escape_lending`]). `os_lock_user_pages` takes the
//! loan for exactly that range, and answers `NV_ERR_INVALID_ADDRESS` for any
//! other, which is what Linux answers for a range it cannot pin.
//!
//! The page array the RM gets is the loan itself: the C reads its pages'
//! physical addresses through [`eclipse_user_pages_phys`], and
//! `os_unlock_user_pages` drops it, which unpins them. The C half is "OS
//! descriptors" in eclipse_rm_frontend.c and osCreateMemFromOsDescriptor in
//! eclipse_rm_mem.c.
use super::{CallScope, Caller, IN_CALL, NV_ERR_INVALID_ADDRESS};
use crate::types::*;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::any::Any;
use core::convert::TryFrom;
use core::sync::atomic::Ordering;
use lock::Mutex;

/// `NV_LOCK_USER_PAGES_FLAGS_WRITE` (os-interface.h, bit 0 of
/// `os_lock_user_pages`'s flags): pin for writing as well as reading.
pub const LOCK_USER_PAGES_FLAGS_WRITE: NvU32 = 1;

/// Pages of the caller's, which the kernel pinned.
pub struct UserPages {
    /// The user address of the first, page aligned.
    pub va: usize,
    /// Each page's physical address, in order.
    pub phys: Vec<u64>,
    /// Whether they were pinned for writing as well as reading.
    pub writable: bool,
    /// What keeps them pinned, until it is dropped.
    pub keep: Box<dyn Any + Send>,
}

/// The pages lent for the call in flight, until the RM takes them. Only
/// touched under the RM gate.
static LENT: Mutex<Option<UserPages>> = Mutex::new(None);

/// [`super::rm_escape`], with `pages` lent to the RM for the length of the
/// call. Pages the RM did not take are unpinned once the call is over.
pub fn rm_escape_lending(
    cmd: u32,
    payload: &mut [u8],
    caller: &Caller,
    pages: Option<UserPages>,
) -> NV_STATUS {
    if caller.os_info == 0 {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let (status, untaken) = {
        let _scope = CallScope::enter(caller);
        *LENT.lock() = pages;
        let status = unsafe {
            super::eclipse_rm_frontend_escape(
                cmd,
                payload.as_mut_ptr() as *mut c_void,
                payload.len() as NvU32,
                caller.os_info as *mut c_void,
                caller.ctl as NvU32,
                caller.admin as NvU32,
                core::ptr::null_mut(),
            )
        };
        (status, LENT.lock().take())
    };
    // Unpinned with the gate open.
    drop(untaken);
    status
}

/// The pages lent, if they are `count` pages from user address `va`, pinned
/// for writing when `write` asks.
fn take_if_fits(
    lent: &mut Option<UserPages>,
    va: usize,
    count: u64,
    write: bool,
) -> Option<UserPages> {
    let fits = lent
        .as_ref()
        .is_some_and(|p| p.va == va && p.phys.len() as u64 == count && (p.writable || !write));
    if fits {
        lent.take()
    } else {
        None
    }
}

/// `os_lock_user_pages`: the pages lent for `page_count` pages from user
/// address `address`, as the page array `*page_array`. Refused with
/// `NV_ERR_NOT_SUPPORTED` outside a front-end call, as it always was.
pub(crate) fn lock(
    address: *mut c_void,
    page_count: NvU64,
    page_array: *mut *mut c_void,
    flags: NvU32,
) -> NV_STATUS {
    if !IN_CALL.load(Ordering::SeqCst) {
        return NV_ERR_NOT_SUPPORTED;
    }
    if page_array.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let write = flags & LOCK_USER_PAGES_FLAGS_WRITE != 0;
    match take_if_fits(&mut LENT.lock(), address as usize, page_count, write) {
        Some(pages) => {
            // SAFETY: checked non-null; the RM's own out-parameter.
            unsafe { *page_array = Box::into_raw(Box::new(pages)) as *mut c_void };
            NV_OK
        }
        None => NV_ERR_INVALID_ADDRESS,
    }
}

/// `os_unlock_user_pages`: the RM is done with a page array [`lock`] gave
/// out, and its pages are unpinned. Always `NV_OK`, as on Linux.
pub(crate) fn unlock(page_array: *mut c_void) -> NV_STATUS {
    if !page_array.is_null() {
        // SAFETY: only [`lock`] hands page arrays out, each one once.
        drop(unsafe { Box::from_raw(page_array as *mut UserPages) });
    }
    NV_OK
}

/// # Safety
/// `page_array` is null or a page array [`lock`] gave out and [`unlock`]
/// has not taken back.
unsafe fn pages<'a>(page_array: *mut c_void) -> Option<&'a UserPages> {
    (page_array as *const UserPages).as_ref()
}

/// How many pages a page array [`lock`] gave out holds.
#[no_mangle]
pub extern "C" fn eclipse_user_pages_count(page_array: *mut c_void) -> NvU64 {
    unsafe { pages(page_array) }.map_or(0, |p| p.phys.len() as NvU64)
}

/// The physical address of page `index` of a page array [`lock`] gave out,
/// or 0 past its end.
#[no_mangle]
pub extern "C" fn eclipse_user_pages_phys(page_array: *mut c_void, index: NvU64) -> NvU64 {
    unsafe { pages(page_array) }
        .and_then(|p| p.phys.get(usize::try_from(index).ok()?))
        .copied()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
