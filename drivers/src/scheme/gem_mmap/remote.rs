//! The table's calls for a driver in a process: each one is the kernel's,
//! made through `drivers_gem`, which the process provides (`udrv-nvidia`,
//! over `ECLIPSE_GEM`). Same names and answers as the kernel's own, so the
//! driver is not changed by where it runs.

use super::wire::{self, *};
use super::{DecRef, HandleSlice};
use alloc::vec::Vec;

extern "C" {
    /// `op` with `args`, its answer's words in `out` (`out_len` of them):
    /// the call's value, or an `errno` negated.
    fn drivers_gem(op: u32, args: *const u64, out: *mut u64, out_len: usize) -> i64;
}

fn call(op: u32, args: [u64; 4], out: &mut [u64]) -> Result<u64, i32> {
    // SAFETY: `args` is four words and `out` is `out.len()` words, both
    // alive for the call.
    let r = unsafe { drivers_gem(op, args.as_ptr(), out.as_mut_ptr(), out.len()) };
    if r < 0 {
        Err((-r) as i32)
    } else {
        Ok(r as u64)
    }
}

fn word(op: u32, args: [u64; 4]) -> Result<u64, i32> {
    call(op, args, &mut [])
}

fn said(op: u32, args: [u64; 4]) -> bool {
    word(op, args) == Ok(1)
}

/// Whatever the kernel says of a call whose answer the driver cannot use,
/// in the log: a refusal is a driver asking for what is not its own.
fn note(op: u32, r: Result<u64, i32>) {
    if let Err(e) = r {
        log::warn!("[gem] the kernel refused op {} (errno {})", op, e);
    }
}

pub fn alloc_handle_slice() -> HandleSlice {
    let mut out = [0u64; 2];
    match call(SLICE, [0; 4], &mut out) {
        Ok(_) => HandleSlice {
            base: out[0] as u32,
            end: out[1] as u32,
        },
        Err(e) => {
            log::warn!("[gem] no handle slice from the kernel (errno {})", e);
            HandleSlice::exhausted()
        }
    }
}

pub fn note_host_pa_handed_out(handle: u32) {
    note(NOTE_HOST_PA, word(NOTE_HOST_PA, [handle as u64, 0, 0, 0]));
}

pub fn host_pa_totals() -> (u32, u64) {
    let mut out = [0u64; 2];
    match call(HOST_PA_TOTALS, [0; 4], &mut out) {
        Ok(_) => (out[0] as u32, out[1]),
        Err(_) => (0, 0),
    }
}

pub fn register(handle: u32, phys_addr: u64, size: u64, owner_pid: u64) {
    let args = [handle as u64, phys_addr, size, owner_pid];
    note(REGISTER, word(REGISTER, args));
}

pub fn register_device_memory(handle: u32, phys_addr: u64, size: u64, owner_pid: u64) {
    let args = [handle as u64, phys_addr, size, owner_pid];
    note(REGISTER_DEVICE, word(REGISTER_DEVICE, args));
}

pub fn is_device_memory(handle: u32) -> bool {
    said(IS_DEVICE, [handle as u64, 0, 0, 0])
}

pub fn add_ref(handle: u32, pid: u64) -> Option<u32> {
    word(ADD_REF, [handle as u64, pid, 0, 0])
        .ok()
        .map(|n| n as u32)
}

/// A list answer: its room goes as the fourth word.
fn list(op: u32, mut args: [u64; 4]) -> Option<Vec<u64>> {
    let mut out = alloc::vec![0u64; LIST_MAX];
    args[3] = LIST_MAX as u64;
    let n = call(op, args, &mut out).ok()? as usize;
    out.truncate(n.min(LIST_MAX));
    Some(out)
}

pub fn holders(handle: u32) -> Option<Vec<u64>> {
    list(HOLDERS, [handle as u64, 0, 0, 0])
}

pub fn ref_count(handle: u32) -> Option<u32> {
    word(REF_COUNT, [handle as u64, 0, 0, 0])
        .ok()
        .map(|n| n as u32)
}

pub fn holds(handle: u32, pid: u64) -> bool {
    said(HOLDS, [handle as u64, pid, 0, 0])
}

pub fn dec_ref(handle: u32, pid: u64) -> DecRef {
    match word(DEC_REF, [handle as u64, pid, 0, 0]) {
        Ok(w) => dec_ref_of(w),
        Err(_) => DecRef::NotTracked,
    }
}

pub fn release_pid(pid: u64) -> Vec<(u32, bool)> {
    list(RELEASE_PID, [pid, 0, 0, 0])
        .unwrap_or_default()
        .into_iter()
        .map(wire::released_of)
        .collect()
}

pub fn mark_no_share(handle: u32, owner_pid: u64) {
    note(
        MARK_NO_SHARE,
        word(MARK_NO_SHARE, [handle as u64, owner_pid, 0, 0]),
    );
}

pub fn no_share(handle: u32, pid: u64) -> bool {
    said(NO_SHARE, [handle as u64, pid, 0, 0])
}

pub fn clear_no_share(handle: u32) -> bool {
    said(CLEAR_NO_SHARE, [handle as u64, 0, 0, 0])
}

pub fn rekey_pid(from: u64, to: u64) -> usize {
    word(REKEY_PID, [from, to, 0, 0]).unwrap_or(0) as usize
}

pub fn unregister(handle: u32) -> bool {
    said(UNREGISTER, [handle as u64, 0, 0, 0])
}

fn range(op: u32, args: [u64; 4]) -> Option<(u64, u64)> {
    let mut out = [0u64; 2];
    call(op, args, &mut out).ok()?;
    Some((out[0], out[1]))
}

pub fn lookup(handle: u32) -> Option<(u64, u64)> {
    range(LOOKUP, [handle as u64, 0, 0, 0])
}

pub fn lookup_for(handle: u32, pid: u64) -> Option<(u64, u64)> {
    range(LOOKUP_FOR, [handle as u64, pid, 0, 0])
}

pub fn lookup_by_phys(phys_addr: u64) -> Option<(u32, u64)> {
    range(LOOKUP_BY_PHYS, [phys_addr, 0, 0, 0]).map(|(h, size)| (h as u32, size))
}

pub fn lookup_by_phys_within(phys_addr: u64) -> Option<u32> {
    word(LOOKUP_BY_PHYS_WITHIN, [phys_addr, 0, 0, 0])
        .ok()
        .map(|h| h as u32)
}
