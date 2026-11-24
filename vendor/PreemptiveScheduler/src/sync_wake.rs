//! Sync wake: a task woken on another CPU by a task that then blocks is
//! polled next on the waker's CPU, instead of waiting for its owner.
//!
//! The case is a pipe or socket round trip with every CPU busy. `A` writes,
//! which wakes `B` on `B`'s owner CPU, and then `A` blocks in `read`. `B`'s
//! owner is running a CPU-bound thread that has just been given a fresh slice,
//! and the RUN_TO_PARITY floor (`BASE_SLICE_NS`, 750 us in
//! `zircon_object::task::thread`) does not let a wake take the CPU from it
//! for its first 750 us. So `B` waited out that floor on every wake, and
//! the round trip, two wakes, cost the two floors: eclipse-bench on a 20-CPU
//! machine gave `pipe round trip, load` = 1508 us, Linux 7.0 on the same
//! machine 47 us. Meanwhile the CPU `A` had just left took its own next task.
//!
//! Linux never pays that wait, because a pipe wake is a *sync* wake
//! (`WF_SYNC`): the waker is about to sleep, so the woken task goes on the
//! waker's CPU. Ownership does not migrate here, so this does what a steal
//! does instead: the waker's CPU takes `B` off its owner's queue and polls it,
//! and `B` stays owned where it was.
//!
//! The hint is narrow on purpose:
//!
//! - set only by a cross-CPU wake of a task that is neither running nor
//!   forbidden on this CPU (`WakerRef::wake_by_ref`);
//! - cleared before every poll, so only a wake raised by the poll that just
//!   ended counts;
//! - used only when that poll ended BLOCKED (`Pending` and not waiting for a
//!   CPU), the "about to sleep" half of `WF_SYNC`; a preempted or yielding
//!   waker is still running and keeps its CPU's normal order;
//! - used only when the owner is not halted: a halted owner was sent the wake
//!   IPI and runs `B` itself.
//!
//! The other half of the pair: once `B` runs here, its reply wakes `A` on
//! this same CPU. Without more, the scan then picks whatever comes next in
//! this CPU's order, often the CPU-bound thread, and `A` waits out that
//! thread's whole slice: QEMU, 4 CPUs, 4 spinners, the round trip went from
//! ~200 us to ~4 ms. So while sync wakes are on, a same-CPU wake also leaves
//! the hand-off hint (`waker_page::HANDOFF_PAGE`), and it lives for the next
//! pick only: [`clear`] drops it when the next poll starts. `B` is often
//! preempted at the end of the `write` that woke `A` (the wake asked for the
//! CPU), not blocked, so the hint survives a preempted poll too.
//! `HANDOFF_LIMIT` caps the hand-offs in a row.
//!
//! `SYNCWAKE=0` on the kernel command line turns it off.

use crate::task_collection::*;
use crate::waker_page::WakerRef;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use lock::MAX_CORE_NUM;

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Per CPU: the page of the task the poll running there last woke on another
/// CPU, `0` for none. Written and read only on the CPU it belongs to.
static PAGE: [AtomicUsize; MAX_CORE_NUM] = [const { AtomicUsize::new(0) }; MAX_CORE_NUM];
/// The slot within [`PAGE`]'s page.
static IDX: [AtomicUsize; MAX_CORE_NUM] = [const { AtomicUsize::new(0) }; MAX_CORE_NUM];
/// The CPU that owns [`PAGE`]'s page.
static OWNER: [AtomicUsize; MAX_CORE_NUM] = [const { AtomicUsize::new(0) }; MAX_CORE_NUM];

/// Sync wakes taken, and hints that found the task already gone (polled by
/// its owner, taken by a thief, or a lock that was busy).
static TAKEN: AtomicU64 = AtomicU64::new(0);
static MISSED: AtomicU64 = AtomicU64::new(0);

/// Turn sync wakes on or off. Called once at boot from the command line.
pub fn set_sync_wake(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether sync wakes are on.
pub fn sync_wake_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// `(taken, missed)` since boot, for `/proc/perf`.
pub fn sync_wake_stats() -> (u64, u64) {
    (
        TAKEN.load(Ordering::Relaxed),
        MISSED.load(Ordering::Relaxed),
    )
}

/// Waker side: the code running on this CPU has just woken slot `idx` of
/// `page`, owned by `owner`, another CPU.
pub(crate) fn note(page: usize, idx: usize, owner: usize) {
    if !sync_wake_enabled() {
        return;
    }
    let cpu = crate::arch::cpu_id() as usize;
    if cpu >= MAX_CORE_NUM || owner == cpu {
        return;
    }
    IDX[cpu].store(idx, Ordering::Relaxed);
    OWNER[cpu].store(owner, Ordering::Relaxed);
    PAGE[cpu].store(page, Ordering::Relaxed);
}

/// Forget this CPU's hints: a poll is about to start, and only what it wakes
/// may count.
#[inline]
pub(crate) fn clear() {
    let cpu = crate::arch::cpu_id() as usize;
    if let Some(slot) = PAGE.get(cpu) {
        if slot.load(Ordering::Relaxed) != 0 {
            slot.store(0, Ordering::Relaxed);
        }
    }
    drop_local_hint();
}

/// Forget this CPU's same-CPU hand-off hint, unless `HANDOFF=1` keeps every
/// one: with sync wakes alone it is for the pick right after the poll that
/// left it, and a poll is about to start.
#[inline]
fn drop_local_hint() {
    if crate::runtime::handoff_enabled() {
        return;
    }
    let cpu = crate::arch::cpu_id() as usize;
    if let Some(slot) = crate::waker_page::HANDOFF_PAGE.get(cpu) {
        if slot.load(Ordering::Relaxed) != 0 {
            slot.store(0, Ordering::Relaxed);
        }
    }
}

/// The hint the poll that just ended left, taken: `(page, slot, owner)`.
fn take() -> Option<(usize, usize, usize)> {
    let cpu = crate::arch::cpu_id() as usize;
    let page = PAGE.get(cpu)?.swap(0, Ordering::Relaxed);
    if page == 0 {
        return None;
    }
    Some((
        page,
        IDX[cpu].load(Ordering::Relaxed),
        OWNER[cpu].load(Ordering::Relaxed),
    ))
}

/// Executor side, after a poll that ended with its task blocked: the task
/// that poll woke on another CPU, taken off that CPU's queue, if it is still
/// waiting there and its owner is busy.
///
/// Never waits for a lock: both are `try_lock`, for the deadlock discipline
/// `runtime::steal_task_inner` describes, and a busy lock is a miss.
pub(crate) fn take_task() -> Option<(Key, Arc<Task>, Arc<WakerRef>)> {
    let (page, idx, owner) = take()?;
    if crate::runtime::cpu_is_sleeping(owner) {
        return None;
    }
    let taken = crate::runtime::try_take_slot_from(owner, page, idx);
    if taken.is_some() {
        TAKEN.fetch_add(1, Ordering::Relaxed);
    } else {
        MISSED.fetch_add(1, Ordering::Relaxed);
    }
    taken
}
