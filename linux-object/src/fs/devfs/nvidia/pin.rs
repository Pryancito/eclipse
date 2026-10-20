//! The pages behind a range of the caller's memory, pinned for the RM:
//! `os_lock_user_pages`, which Linux builds on `pin_user_pages`.
//!
//! RM_ALLOC_MEMORY of an OS descriptor hands the RM memory libcuda allocated
//! itself, and the RM makes GPU memory of its pages. The GPU reads and writes
//! them by physical address for as long as that memory lives, whatever the
//! process does with its mappings meanwhile, so each page has to stay put:
//! the frame its mapping's object holds is pinned (`VmObject::pin`), which
//! refuses to decommit it, and the pin holds the object, which keeps the
//! frame allocated past an munmap or the process's exit.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use zcore_drivers::display::nvctl;
use zircon_object::vm::{MMUFlags, VmAddressRegion, VmObject, PAGE_SIZE};

use crate::error::{LxError, LxResult};

/// What keeps pinned pages pinned: a run of consecutive pages of one object
/// per entry, as `(object, offset, length)`, unpinned when this is dropped.
struct Pins(Vec<(Arc<VmObject>, usize, usize)>);

impl Pins {
    fn add(&mut self, vmo: &Arc<VmObject>, offset: usize) {
        if let Some((last, start, len)) = self.0.last_mut() {
            if Arc::ptr_eq(last, vmo) && *start + *len == offset {
                *len += PAGE_SIZE;
                return;
            }
        }
        self.0.push((vmo.clone(), offset, PAGE_SIZE));
    }
}

impl Drop for Pins {
    fn drop(&mut self) {
        for (vmo, offset, len) in self.0.drain(..) {
            let _ = vmo.unpin(offset, len);
        }
    }
}

/// `count` pages from user address `va` of `vmar`, pinned for writing as
/// well as reading when `write`, for the RM to make memory of. EINVAL for an
/// address that is not page aligned or no pages at all; EFAULT for a page
/// that is not mapped, not mapped for the access asked, or not RAM (a
/// device's aperture). Nothing stays pinned on an error.
pub fn pin_user_pages(
    vmar: &VmAddressRegion,
    va: usize,
    count: usize,
    write: bool,
) -> LxResult<nvctl::UserPages> {
    if !va.is_multiple_of(PAGE_SIZE) || count == 0 {
        return Err(LxError::EINVAL);
    }
    let mut pins = Pins(Vec::new());
    let mut phys = Vec::new();
    for i in 0..count {
        let page = i
            .checked_mul(PAGE_SIZE)
            .and_then(|offset| va.checked_add(offset))
            .ok_or(LxError::EFAULT)?;
        phys.push(pin_page(vmar, page, write, &mut pins)?);
    }
    Ok(nvctl::UserPages {
        va,
        phys,
        writable: write,
        keep: Box::new(pins),
    })
}

/// The physical address of the page at `page`, pinned into `pins`.
fn pin_page(vmar: &VmAddressRegion, page: usize, write: bool, pins: &mut Pins) -> LxResult<u64> {
    let mut access = MMUFlags::READ;
    if write {
        access |= MMUFlags::WRITE;
    }
    let mapping = vmar.find_mapping(page).ok_or(LxError::EFAULT)?;
    let flags = mapping.get_flags(page).map_err(|_| LxError::EFAULT)?;
    if !flags.contains(access) {
        return Err(LxError::EFAULT);
    }
    let (vmo, offset) = mapping.vmo_and_offset(page).ok_or(LxError::EFAULT)?;
    if !vmo.is_paged() {
        return Err(LxError::EFAULT);
    }
    // A page pinned for writing is faulted in for writing first, as
    // pin_user_pages(FOLL_WRITE) does: a copy-on-write page becomes the
    // process's own, and its mapping then points at the frame the GPU will
    // write. One pinned for reading is not faulted, which would take WRITE
    // away from a page that has it (`VmMapping::handle_page_fault`).
    if write {
        vmar.handle_page_fault(page, access)
            .map_err(|_| LxError::EFAULT)?;
    }
    vmo.commit(offset, PAGE_SIZE)?;
    vmo.pin(offset, PAGE_SIZE).map_err(|_| LxError::EFAULT)?;
    pins.add(&vmo, offset);
    let paddr = vmo
        .committed_paddr(offset / PAGE_SIZE)
        .ok_or(LxError::EFAULT)?;
    Ok(paddr as u64)
}

#[cfg(test)]
mod tests;
