//! The table's calls for a driver in a process: each one is the kernel's,
//! made through `drivers_syncobj`, which the process provides
//! (`udrv-nvidia`, over `ECLIPSE_SYNCOBJ`). Same names and answers as the
//! kernel's own, so the driver is not changed by where it runs.
//!
//! What changes on the way is where a landing zone is: the driver names it
//! by its own mapping, the kernel by the physical address under it.

use super::wire::{self, *};
use super::WaitOutcome;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

extern "C" {
    /// `op` with the `in_len` words at `inp`, its answer's words in `out`
    /// (`out_len` of them): the call's value, or an `errno` negated.
    fn drivers_syncobj(
        op: u32,
        inp: *const u64,
        in_len: usize,
        out: *mut u64,
        out_len: usize,
    ) -> i64;
}

/// `op` with `inp`, its answer in `out`.
pub(crate) fn call(op: u32, inp: &[u64], out: &mut [u64]) -> Result<u64, i32> {
    // SAFETY: both slices are alive for the call, at the lengths given.
    let r = unsafe { drivers_syncobj(op, inp.as_ptr(), inp.len(), out.as_mut_ptr(), out.len()) };
    if r < 0 {
        Err((-r) as i32)
    } else {
        Ok(r as u64)
    }
}

fn word(op: u32, inp: &[u64]) -> Option<u64> {
    match call(op, inp, &mut []) {
        Ok(w) => Some(w),
        Err(e) => {
            log::warn!("[syncobj] the kernel refused op {} (errno {})", op, e);
            None
        }
    }
}

fn said(op: u32, inp: &[u64]) -> bool {
    word(op, inp) == Some(1)
}

/// A list of `each`-word entries: as many as the kernel has, up to what
/// fits in one answer.
pub(crate) fn list(op: u32, inp: &[u64], each: usize) -> Vec<u64> {
    let mut out = vec![0u64; WORDS_MAX / each * each];
    match call(op, inp, &mut out) {
        Ok(n) => {
            out.truncate((n as usize).saturating_mul(each).min(out.len()));
            out
        }
        Err(e) => {
            log::warn!("[syncobj] the kernel refused op {} (errno {})", op, e);
            Vec::new()
        }
    }
}

fn line(op: u32, inp: &[u64]) -> String {
    let mut out = vec![0u64; WORDS_MAX];
    match call(op, inp, &mut out) {
        Ok(len) => {
            let len = (len as usize).min(WORDS_MAX * 8);
            String::from_utf8_lossy(&bytes_of_words(&out, len)).into_owned()
        }
        Err(_) => String::new(),
    }
}

/// The driver's mapping of `phys`, 0 for one it does not have.
fn mine(phys: u64) -> usize {
    if phys == 0 {
        0
    } else {
        crate::bus::phys_to_virt(phys as usize)
    }
}

/// The driver's fence-timeout upcall, run here when the kernel says one
/// of its fences timed out.
static TIMEOUT_HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_fence_timeout_hook(f: fn(u32, usize, u32, u32, u64)) {
    TIMEOUT_HOOK.store(f as usize, Ordering::SeqCst);
}

/// Run the upcall for every fence of the driver's that timed out.
fn timeouts() {
    let hook = TIMEOUT_HOOK.load(Ordering::Relaxed);
    for t in list(TIMEOUTS, &[], 5).as_chunks::<5>().0.iter() {
        if hook != 0 {
            // SAFETY: stored by `set_fence_timeout_hook` from this type.
            let cb: fn(u32, usize, u32, u32, u64) = unsafe { core::mem::transmute(hook) };
            cb(t[0] as u32, mine(t[1]), t[2] as u32, t[3] as u32, t[4]);
        }
    }
}

pub fn poll_pending() -> usize {
    let w = word(POLL_PENDING, &[]).unwrap_or(0);
    if w & TIMED_OUT != 0 {
        timeouts();
    }
    (w & !TIMED_OUT) as usize
}

pub fn has_pending() -> bool {
    said(HAS_PENDING, &[])
}

pub fn attach_hw_fence(
    handle: u32,
    point: u64,
    fence_va: usize,
    fence_gpu_va: u64,
    payload: u32,
    ctx_idx: u32,
    binary: bool,
) -> bool {
    let phys = crate::bus::virt_to_phys(fence_va) as u64;
    said(
        ATTACH,
        &[
            handle as u64,
            point,
            phys,
            fence_gpu_va,
            payload as u64,
            ctx_idx as u64,
            binary as u64,
        ],
    )
}

pub fn abandon_fences(fence_va: usize) -> usize {
    let phys = crate::bus::virt_to_phys(fence_va) as u64;
    word(ABANDON, &[phys]).unwrap_or(0) as usize
}

pub fn timeline_signal(handle: u32, point: u64) -> bool {
    said(SIGNAL, &[handle as u64, point])
}

pub fn signal(handle: u32) -> bool {
    timeline_signal(handle, 1)
}

pub fn pending_hw_fences(handle: u32, point: u64) -> Vec<(usize, u64, u32, u32)> {
    list(PENDING_HW, &[handle as u64, point], 4)
        .as_chunks::<4>()
        .0
        .iter()
        .map(|f| (mine(f[0]), f[1], f[2] as u32, f[3] as u32))
        .collect()
}

pub fn without_fence(handles: &[u32], points: Option<&[u64]>) -> Option<usize> {
    match word(WITHOUT_FENCE, &waits(handles, points)) {
        Some(NONE) => None,
        Some(i) => Some(i as usize),
        // The kernel would not say: none of them is taken for one.
        None => Some(0),
    }
}

pub fn first_unusable_by(pid: u64, handles: impl IntoIterator<Item = u32>) -> Option<u32> {
    let handles: Vec<u32> = handles.into_iter().collect();
    let mut inp = Vec::with_capacity(handles.len() + 1);
    inp.push(pid);
    inp.extend(handles.iter().map(|&h| h as u64));
    match word(FIRST_UNUSABLE, &inp) {
        Some(NONE) => None,
        Some(h) => Some(h as u32),
        None => Some(handles.first().copied().unwrap_or(0)),
    }
}

pub fn reached_by_timeout(handles: &[u32], points: Option<&[u64]>) -> bool {
    said(REACHED_BY_TIMEOUT, &waits(handles, points))
}

pub fn describe(handles: &[u32], points: Option<&[u64]>) -> String {
    line(DESCRIBE, &waits(handles, points))
}

pub fn stats_line() -> String {
    line(STATS, &[])
}

#[inline]
fn now_us() -> u64 {
    // SAFETY: the process's clock, which it provides.
    unsafe { crate::bus::drivers_timer_now_as_micros() }
}

/// The deadline is the driver's clock, not the kernel's: the kernel is
/// only asked whether the wait is over, and the deadline is kept here.
pub fn wait_ready(
    handles: &[u32],
    points: Option<&[u64]>,
    wait_all: bool,
    deadline_us: u64,
) -> Option<Result<u32, WaitOutcome>> {
    let mut inp = vec![wait_all as u64];
    inp.extend(waits(handles, points));
    match word(WAIT_READY, &inp).map(wire::ready_of) {
        Some(None) if now_us() >= deadline_us => Some(Err(WaitOutcome::Timeout)),
        Some(r) => r,
        None => Some(Err(WaitOutcome::Invalid)),
    }
}

/// [`wait_ready`] until it says, backing off between looks as the kernel's
/// own wait does: the kernel never waits in the driver's call.
pub fn wait(
    handles: &[u32],
    points: Option<&[u64]>,
    wait_all: bool,
    deadline_us: u64,
) -> WaitOutcome {
    let mut probes = 0u32;
    loop {
        if probes > 0 {
            // The kernel resolves what landed when it is asked; this tells
            // the driver of its own fences that timed out meanwhile.
            poll_pending();
        }
        match wait_ready(handles, points, wait_all, deadline_us) {
            Some(Ok(i)) => {
                return WaitOutcome::Signaled {
                    first_signaled_index: i,
                }
            }
            Some(Err(e)) => return e,
            None => {}
        }
        for _ in 0..super::wait_spin_backoff(probes).max(1) {
            core::hint::spin_loop();
        }
        probes = probes.saturating_add(1);
    }
}
