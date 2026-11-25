//! [`ECLIPSE_GEM`]: the kernel's table of mappable GEM objects
//! (`zcore_drivers::scheme::gem_mmap`), for a GPU driver in a process.
//!
//! The table is where the kernel's mmap, PRIME and KMS paths find the
//! memory of a driver's GEM object, so a driver in a process (`udrv-nvidia`)
//! keeps it here too: the driver's own calls of the table come to this
//! gateway (`gem_mmap::remote`), unchanged for the driver.
//!
//! A process is taken at its word only about what is its own:
//!
//! - it must hold a GPU (a claim of kind `nvidia`, or a PCI function);
//! - the handles it names are those of the slices the kernel gave it
//!   ([`wire::SLICE`]), never another card's;
//! - the memory it registers is memory it pinned through that GPU's
//!   initiator, or one of the GPU's memory BARs.
//!
//! When it is gone, its objects leave the table, and what it pinned stays
//! pinned for good (`device_claim`): a client may still have one mapped.

use alloc::sync::Arc;
use alloc::vec::Vec;
use kernel_hal::drivers::scheme::gem_mmap::{self, wire, HandleSlice};
use zircon_object::object::{KernelObject, KoID};
use zircon_object::task::Process;
use zircon_object::ZxError;

use super::device_claim::{holds_gpu, owns_memory};

/// `eclipse_gem(op, a, b, c, d, out)`: op `op` of `gem_mmap::wire` with
/// the words `a`..`d`, its answer's words written to `out` (as many as
/// the op has, or up to `d` of a list). The value is the op's word.
///
/// `ACCESS_DENIED` for a process that holds no GPU, a handle outside its
/// slices or memory not its own; `NOT_FOUND` for a handle the table does
/// not have where the op needs one; `INVALID_ARGS` for an unknown op or a
/// bad `out`.
pub const ECLIPSE_GEM: u32 = super::ECLIPSE_GATEWAY_EXT + 23;

/// The slices each driver was given, by its process.
static SLICES: lock::Mutex<Vec<(KoID, HandleSlice)>> = lock::Mutex::new(Vec::new());

fn slices_of(id: KoID) -> Vec<HandleSlice> {
    SLICES
        .lock()
        .iter()
        .filter(|(holder, _)| *holder == id)
        .map(|(_, s)| *s)
        .collect()
}

/// The driver of process `id` is gone: its objects leave the table.
/// Whether it had named any.
pub(super) fn driver_gone(id: KoID) -> bool {
    let mine = slices_of(id);
    if mine.is_empty() {
        return false;
    }
    SLICES.lock().retain(|(holder, _)| *holder != id);
    gem_mmap::forget_where(|h| mine.iter().any(|s| s.contains(h))) > 0
}

/// [`ECLIPSE_GEM`].
pub(super) fn gem(proc: &Arc<Process>, args: &[usize; 6]) -> isize {
    match gem_op(proc, args) {
        Ok(value) => value as isize,
        Err(e) => e as isize,
    }
}

fn gem_op(proc: &Arc<Process>, args: &[usize; 6]) -> Result<u64, ZxError> {
    if !holds_gpu(proc) {
        return Err(ZxError::ACCESS_DENIED);
    }
    let op = args[0] as u32;
    let (a, b, c, d) = (
        args[1] as u64,
        args[2] as u64,
        args[3] as u64,
        args[4] as u64,
    );
    let id = proc.id();
    let mine = slices_of(id);
    let is_mine = |h: u32| mine.iter().any(|s| s.contains(h));
    let handle = a as u32;
    let own = || {
        if is_mine(handle) {
            Ok(())
        } else {
            Err(ZxError::ACCESS_DENIED)
        }
    };
    let out = |words: &[u64]| {
        let mut to = kernel_hal::user::UserOutPtr::<u64>::from(args[5]);
        to.write_array(words).map_err(|_| ZxError::INVALID_ARGS)
    };
    let list = |words: Vec<u64>| -> Result<u64, ZxError> {
        let n = words.len().min(d as usize).min(wire::LIST_MAX);
        out(&words[..n])?;
        Ok(words.len() as u64)
    };
    let yes = |b: bool| Ok(b as u64);
    match op {
        wire::SLICE => {
            let slice = gem_mmap::alloc_handle_slice();
            if slice != HandleSlice::exhausted() {
                SLICES.lock().push((id, slice));
            }
            out(&[slice.base() as u64, slice.end() as u64])?;
            Ok(0)
        }
        wire::REGISTER | wire::REGISTER_DEVICE => {
            own()?;
            let device = owns_memory(proc, b, c).ok_or(ZxError::ACCESS_DENIED)?;
            // What the memory is decides how it is mapped, not what the
            // driver calls it: a BAR is never mapped write-back.
            if device {
                gem_mmap::register_device_memory(handle, b, c, d);
            } else if op == wire::REGISTER {
                gem_mmap::register(handle, b, c, d);
            } else {
                return Err(ZxError::ACCESS_DENIED);
            }
            Ok(0)
        }
        wire::UNREGISTER => {
            own()?;
            yes(gem_mmap::unregister(handle))
        }
        wire::ADD_REF => {
            own()?;
            gem_mmap::add_ref(handle, b)
                .map(|n| n as u64)
                .ok_or(ZxError::NOT_FOUND)
        }
        wire::DEC_REF => {
            own()?;
            Ok(wire::dec_ref_word(gem_mmap::dec_ref(handle, b)))
        }
        wire::HOLDS => {
            own()?;
            yes(gem_mmap::holds(handle, b))
        }
        wire::HOLDERS => {
            own()?;
            list(gem_mmap::holders(handle).ok_or(ZxError::NOT_FOUND)?)
        }
        wire::REF_COUNT => {
            own()?;
            gem_mmap::ref_count(handle)
                .map(|n| n as u64)
                .ok_or(ZxError::NOT_FOUND)
        }
        wire::IS_DEVICE => {
            own()?;
            yes(gem_mmap::is_device_memory(handle))
        }
        wire::LOOKUP | wire::LOOKUP_FOR => {
            own()?;
            let found = if op == wire::LOOKUP {
                gem_mmap::lookup(handle)
            } else {
                gem_mmap::lookup_for(handle, b)
            };
            let (phys, size) = found.ok_or(ZxError::NOT_FOUND)?;
            out(&[phys, size])?;
            Ok(0)
        }
        wire::RELEASE_PID => list(
            gem_mmap::release_pid_where(a, is_mine)
                .into_iter()
                .map(|(h, freed)| wire::released_word(h, freed))
                .collect(),
        ),
        wire::MARK_NO_SHARE => {
            own()?;
            gem_mmap::mark_no_share(handle, b);
            Ok(0)
        }
        wire::NO_SHARE => {
            own()?;
            yes(gem_mmap::no_share(handle, b))
        }
        wire::CLEAR_NO_SHARE => {
            own()?;
            yes(gem_mmap::clear_no_share(handle))
        }
        wire::REKEY_PID => Ok(gem_mmap::rekey_pid_where(a, b, is_mine) as u64),
        wire::LOOKUP_BY_PHYS => {
            let (h, size) = gem_mmap::lookup_by_phys(a)
                .filter(|&(h, _)| is_mine(h))
                .ok_or(ZxError::NOT_FOUND)?;
            out(&[h as u64, size])?;
            Ok(0)
        }
        wire::LOOKUP_BY_PHYS_WITHIN => gem_mmap::lookup_by_phys_within(a)
            .filter(|&h| is_mine(h))
            .map(|h| h as u64)
            .ok_or(ZxError::NOT_FOUND),
        wire::NOTE_HOST_PA => {
            own()?;
            gem_mmap::note_host_pa_handed_out(handle);
            Ok(0)
        }
        wire::HOST_PA_TOTALS => {
            let (n, bytes) = gem_mmap::host_pa_totals();
            out(&[n as u64, bytes])?;
            Ok(0)
        }
        _ => Err(ZxError::INVALID_ARGS),
    }
}
