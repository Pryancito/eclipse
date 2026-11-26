//! [`ECLIPSE_SYNCOBJ`]: the kernel's table of sync objects
//! (`zcore_drivers::scheme::syncobj`) and of buffer reservations
//! (`scheme::resv`), for a GPU driver in a process.
//!
//! A client's syncobjs are the kernel's: its ioctls create, wait on and
//! export them here. So the fences a driver in a process attaches to them
//! are kept here too, and its own calls of the table come to this gateway
//! (`syncobj::remote`), unchanged for the driver.
//!
//! What the kernel does not take on the driver's word is memory: a fence's
//! landing zone is read by the kernel on every look at the table, so it
//! must be memory the driver pinned through the GPU it holds. Its channels
//! are numbered apart from every other driver's (a tag in the high bits of
//! the ctx), so that a fence of one is never taken for another's.
//!
//! When the driver is gone, every fence it attached is abandoned (its
//! waiters let go, as a dead channel's are) before its memory is released.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryFrom;
use kernel_hal::drivers::scheme::{resv, syncobj, syncobj::wire};
use zircon_object::object::{KernelObject, KoID};
use zircon_object::task::Process;
use zircon_object::ZxError;

use super::device_claim::{holds_gpu, owns_memory};

/// `eclipse_syncobj(op, in, in_len, out, out_len)`: op `op` of
/// `syncobj::wire` with the `in_len` words at `in`, its answer's words
/// written to `out` (up to `out_len`). The value is the op's word.
///
/// `ACCESS_DENIED` for a process that holds no GPU or memory not its own;
/// `INVALID_ARGS` for an unknown op, words that are not the op's, or a bad
/// pointer; `OUT_OF_RANGE` for more words than a call carries.
pub const ECLIPSE_SYNCOBJ: u32 = super::ECLIPSE_GATEWAY_EXT + 24;

/// A fence of a driver in a process: this bit in its ctx, the driver's tag
/// under it, its own ctx in the low 16 bits.
const SERVED: u32 = 1 << 31;
const TAG_SHIFT: u32 = 16;
const TAG_MAX: u32 = 0x7fff;
const CTX_MASK: u32 = 0xffff;

/// Each driver's tag, by its process.
static TAGS: lock::Mutex<Vec<(KoID, u32)>> = lock::Mutex::new(Vec::new());

/// The fences that timed out, for the driver that attached them to ask
/// for: `[ctx, phys, payload, handle, point]`.
static TIMED_OUT: lock::Mutex<Vec<(KoID, [u64; 5])>> = lock::Mutex::new(Vec::new());

/// The most timeouts kept for a driver that does not ask.
const TIMED_OUT_MAX: usize = 256;

fn tag_of(id: KoID) -> Option<u32> {
    TAGS.lock().iter().find(|(p, _)| *p == id).map(|&(_, t)| t)
}

/// `id`'s tag, a new one the first time.
fn tag_for(id: KoID) -> Option<u32> {
    let mut tags = TAGS.lock();
    if let Some(&(_, t)) = tags.iter().find(|(p, _)| *p == id) {
        return Some(t);
    }
    let t = (1..=TAG_MAX).find(|t| !tags.iter().any(|&(_, u)| u == *t))?;
    tags.push((id, t));
    Some(t)
}

fn tag_in(ctx: u32) -> Option<u32> {
    (ctx & SERVED != 0).then_some((ctx >> TAG_SHIFT) & TAG_MAX)
}

/// The kernel's fence-timeout upcall for the fences of drivers in
/// processes: kept for the driver to ask for.
fn timed_out(ctx: u32, fence_va: usize, payload: u32, handle: u32, point: u64) {
    let Some(tag) = tag_in(ctx) else {
        return;
    };
    let Some(id) = TAGS
        .lock()
        .iter()
        .find(|&&(_, t)| t == tag)
        .map(|&(p, _)| p)
    else {
        return;
    };
    let phys = kernel_hal::mem::virt_to_phys(fence_va) as u64;
    let mut q = TIMED_OUT.lock();
    if q.iter().filter(|(p, _)| *p == id).count() < TIMED_OUT_MAX {
        let ctx = (ctx & CTX_MASK) as u64;
        q.push((id, [ctx, phys, payload as u64, handle as u64, point]));
    }
}

/// The driver of process `id` is gone: the fences it attached can never
/// land, and their waiters are let go. How many there were.
pub(super) fn driver_gone(id: KoID) -> usize {
    TIMED_OUT.lock().retain(|(p, _)| *p != id);
    let Some(tag) = tag_of(id) else {
        return 0;
    };
    let n = syncobj::abandon_fences_where(|_, ctx| tag_in(ctx) == Some(tag));
    TAGS.lock().retain(|(p, _)| *p != id);
    n
}

/// The kernel's mapping of a fence of the driver in `proc` at `phys`, and
/// the ctx the table knows its channel `ctx` by: what a fence the card it
/// serves names (`DrmScheme::queued_fences` and the like) is to the kernel.
/// `None` for memory that is not its own.
pub(super) fn served_fence(proc: &Arc<Process>, phys: u64, ctx: u32) -> Option<(usize, u32)> {
    let va = zone(proc, phys).ok()?;
    if ctx > CTX_MASK {
        return None;
    }
    let tag = tag_for(proc.id())?;
    syncobj::set_fence_timeout_hook(timed_out);
    Some((va, SERVED | tag << TAG_SHIFT | ctx))
}

/// [`ECLIPSE_SYNCOBJ`].
pub(super) fn syncobj_call(proc: &Arc<Process>, args: &[usize; 6]) -> isize {
    match op(proc, args) {
        Ok(value) => value as isize,
        Err(e) => e as isize,
    }
}

/// The kernel's mapping of a landing zone at `phys`, if it is memory `proc`
/// pinned (never a BAR: the kernel does not map those).
fn zone(proc: &Arc<Process>, phys: u64) -> Result<usize, ZxError> {
    if !phys.is_multiple_of(4) || owns_memory(proc, phys, 4) != Some(false) {
        return Err(ZxError::ACCESS_DENIED);
    }
    Ok(kernel_hal::mem::phys_to_virt(phys as usize))
}

fn op(proc: &Arc<Process>, args: &[usize; 6]) -> Result<u64, ZxError> {
    if !holds_gpu(proc) {
        return Err(ZxError::ACCESS_DENIED);
    }
    let op = args[0] as u32;
    let (in_len, out_len) = (args[2], args[4]);
    if in_len > wire::WORDS_MAX || out_len > wire::WORDS_MAX {
        return Err(ZxError::OUT_OF_RANGE);
    }
    let inp: Vec<u64> = if in_len == 0 {
        Vec::new()
    } else {
        kernel_hal::user::UserInPtr::<u64>::from(args[1])
            .read_array(in_len)
            .map_err(|_| ZxError::INVALID_ARGS)?
    };
    let out = |words: &[u64]| -> Result<(), ZxError> {
        let n = words.len().min(out_len);
        if n == 0 {
            return Ok(());
        }
        let mut to = kernel_hal::user::UserOutPtr::<u64>::from(args[3]);
        to.write_array(&words[..n])
            .map_err(|_| ZxError::INVALID_ARGS)
    };
    let at = |i: usize| inp.get(i).copied().ok_or(ZxError::INVALID_ARGS);
    let waits =
        |from: usize| wire::waits_of(inp.get(from..).unwrap_or(&[])).ok_or(ZxError::INVALID_ARGS);
    let line = |s: alloc::string::String| -> Result<u64, ZxError> {
        let bytes = s.as_bytes();
        let len = bytes.len().min(out_len * 8);
        out(&wire::words_of_bytes(&bytes[..len]))?;
        Ok(len as u64)
    };
    let id = proc.id();
    let yes = |b: bool| Ok(b as u64);
    match op {
        wire::POLL_PENDING => {
            let left = syncobj::poll_pending() as u64;
            let timed_out = TIMED_OUT.lock().iter().any(|(p, _)| *p == id);
            Ok(left | if timed_out { wire::TIMED_OUT } else { 0 })
        }
        wire::HAS_PENDING => yes(syncobj::has_pending()),
        wire::ATTACH => {
            let (handle, point, phys, gpu_va) = (at(0)?, at(1)?, at(2)?, at(3)?);
            let (payload, ctx, binary) = (at(4)?, at(5)?, at(6)?);
            let va = zone(proc, phys)?;
            if ctx > CTX_MASK as u64 {
                return Err(ZxError::INVALID_ARGS);
            }
            let tag = tag_for(id).ok_or(ZxError::NO_RESOURCES)?;
            syncobj::set_fence_timeout_hook(timed_out);
            let ctx = SERVED | tag << TAG_SHIFT | ctx as u32;
            yes(syncobj::attach_hw_fence(
                handle as u32,
                point,
                va,
                gpu_va,
                payload as u32,
                ctx,
                binary != 0,
            ))
        }
        wire::ABANDON => {
            let va = zone(proc, at(0)?)?;
            let Some(tag) = tag_of(id) else {
                return Ok(0);
            };
            Ok(syncobj::abandon_fences_where(|v, ctx| v == va && tag_in(ctx) == Some(tag)) as u64)
        }
        wire::SIGNAL => yes(syncobj::timeline_signal(at(0)? as u32, at(1)?)),
        wire::PENDING_HW => {
            let tag = tag_of(id);
            let fences = syncobj::pending_hw_fences(at(0)? as u32, at(1)?);
            let words: Vec<u64> = fences
                .iter()
                .flat_map(|&(va, gpu_va, payload, ctx)| {
                    if tag.is_some() && tag_in(ctx) == tag {
                        let phys = kernel_hal::mem::virt_to_phys(va) as u64;
                        [phys, gpu_va, payload as u64, (ctx & CTX_MASK) as u64]
                    } else {
                        [0, 0, payload as u64, wire::NOT_MINE as u64]
                    }
                })
                .collect();
            out(&words)?;
            Ok(fences.len() as u64)
        }
        wire::WITHOUT_FENCE => {
            let (handles, points) = waits(0)?;
            Ok(
                syncobj::without_fence(&handles, points.as_deref())
                    .map_or(wire::NONE, |i| i as u64),
            )
        }
        wire::FIRST_UNUSABLE => {
            let pid = at(0)?;
            let handles = inp[1..]
                .iter()
                .map(|&h| u32::try_from(h).map_err(|_| ZxError::INVALID_ARGS))
                .collect::<Result<Vec<u32>, _>>()?;
            Ok(syncobj::first_unusable_by(pid, handles).map_or(wire::NONE, |h| h as u64))
        }
        wire::REACHED_BY_TIMEOUT => {
            let (handles, points) = waits(0)?;
            yes(syncobj::reached_by_timeout(&handles, points.as_deref()))
        }
        wire::DESCRIBE => {
            let (handles, points) = waits(0)?;
            line(syncobj::describe(&handles, points.as_deref()))
        }
        wire::WAIT_READY => {
            let wait_all = at(0)? != 0;
            let (handles, points) = waits(1)?;
            // The deadline is the driver's to keep: the kernel only says
            // whether the wait is over.
            let r = syncobj::wait_ready(&handles, points.as_deref(), wait_all, u64::MAX);
            Ok(wire::ready_word(r))
        }
        wire::TIMEOUTS => {
            let mine: Vec<[u64; 5]> = {
                let mut q = TIMED_OUT.lock();
                let mine = q
                    .iter()
                    .filter(|(p, _)| *p == id)
                    .map(|&(_, t)| t)
                    .collect();
                q.retain(|(p, _)| *p != id);
                mine
            };
            // What does not fit is not lost: the driver is told it is
            // there on its next poll.
            let fit = (out_len / 5).min(mine.len());
            let words: Vec<u64> = mine[..fit].iter().flatten().copied().collect();
            out(&words)?;
            if fit < mine.len() {
                TIMED_OUT
                    .lock()
                    .extend(mine[fit..].iter().map(|&t| (id, t)));
            }
            Ok(fit as u64)
        }
        wire::STATS => line(syncobj::stats_line()),
        wire::RESV_FENCES => {
            let phys = at(0)?;
            owns_memory(proc, phys, 1).ok_or(ZxError::ACCESS_DENIED)?;
            let fences = resv::fences(phys, at(1)? != 0);
            let words: Vec<u64> = fences.iter().flat_map(|&(h, p)| [h as u64, p]).collect();
            out(&words)?;
            Ok(fences.len() as u64)
        }
        wire::RESV_FORGET => {
            let phys = at(0)?;
            owns_memory(proc, phys, 1).ok_or(ZxError::ACCESS_DENIED)?;
            resv::forget(phys);
            Ok(0)
        }
        _ => Err(ZxError::INVALID_ARGS),
    }
}
