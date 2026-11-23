//! The reclaim worker: Eclipse's `kswapd`, in one task.
//!
//! Direct reclaim (`kernel_hal::shrinker::shrink`) runs on the thread whose
//! allocation has just failed, so it may only ask caches that give memory back
//! without allocating, waiting or writing to disk. That rules out the biggest
//! one: evicting a page-cache entry writes its dirty pages back through the
//! filesystem. Those caches register in the DEFERRED registry, and this task
//! is the only thing that walks it.
//!
//! What it does on each pass, in Linux's order: serve whatever the failure
//! paths asked for, and reclaim ahead of them when free memory is under the
//! low watermark. The second half is the point of having a worker at all --
//! reclaiming only after an allocation has failed means every shortage is paid
//! for by whoever happened to allocate first, and a `MAP_SHARED` page-cache
//! entry cannot be reclaimed on that path at all.
//!
//! Two things are deliberately simpler than Linux. There is one zone, so there
//! is one watermark and no per-node worker; and nothing wakes this task early
//! -- it polls on a fixed period instead. Waking it from
//! `kernel_hal::shrinker::request` would mean a waker reachable from a failed
//! allocation and from the frame allocator holding its own lock, and a missed
//! wake would be a machine that never reclaims. A period costs one timer per
//! [`PERIOD_MS`] and cannot be missed.

use core::time::Duration;

use kernel_hal::shrinker;
use kernel_hal::timer::timer_now;

/// Idle coroutine stacks kept ready for the next `Executor::new`; the rest go
/// back, half the excess a period (`executor::trim_idle_stacks`).
/// Sixteen covers the stacks a quiet desktop churns through, so the common
/// case still reuses a pooled block and pays no guard install for it.
#[cfg(not(feature = "libos"))]
const IDLE_STACKS_KEPT: usize = 16;

/// Bytes of freed large heap blocks (`kernel_hal::vmalloc`) a pass leaves
/// mapped, for the next large allocation to take over without mapping or
/// flushing anything: a pipe or socket buffer that grows and shrinks.
#[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
const VMALLOC_KEPT: usize = 32 * 1024 * 1024;

/// How often the worker looks. Linux's `kswapd` sleeps on a waitqueue with a
/// 100 ms watchdog; this is the watchdog without the waitqueue.
const PERIOD_MS: u64 = 200;

/// The most spare kernel heap one pass gives back. Each page is one
/// `frame_dealloc`, so this bounds the pass at 16 384 of them; at one pass
/// every [`PERIOD_MS`] that is still 320 MiB a second.
const HEAP_RELEASE_PER_PASS: usize = 64 * 1024 * 1024;

/// Free memory under this share of the total is what the worker reclaims ahead
/// of a failure, in percent. Linux's `min_free_kbytes` works out to about 1 %
/// of RAM for a small machine and less for a large one; 5 % here because a
/// pass only runs every [`PERIOD_MS`] and a desktop can commit a great deal in
/// 200 ms.
const LOW_WATERMARK_PCT: usize = 5;

/// Under this share of the total, with nothing left to reclaim, the worker
/// kills instead. A fifth of the low watermark: at the watermark itself there
/// is still a watermark's worth of memory to allocate from, and killing there
/// would kill a process the machine did not need to lose.
const OOM_WATERMARK_DIV: usize = 5;

/// The watermark never goes below this, however small the machine: 5 % of a
/// 128 MiB box is 6 MiB, which one `fork` of a desktop client spends.
const WATERMARK_FLOOR: usize = 8 * 1024 * 1024;

/// ... nor above this, however large it is: 5 % of 32 GiB is 1.6 GiB of page
/// cache thrown away to protect memory nothing is asking for. Reclaim is for
/// the shortage that is coming, not for a free-memory target.
const WATERMARK_CEILING: usize = 64 * 1024 * 1024;

/// The low watermark for a machine of `total` bytes.
///
/// Clamped by `total / 4` as well, so that on a machine smaller than the floor
/// the worker does not declare a permanent shortage it can never satisfy and
/// walk every cache on every pass.
fn watermark(total: usize) -> usize {
    let share = total / 100 * LOW_WATERMARK_PCT;
    share
        .clamp(WATERMARK_FLOOR, WATERMARK_CEILING)
        .min(total / 4)
}

/// Bytes the worker should ask for, given what the allocator reports.
///
/// Zero means "above the watermark, leave the caches alone" -- including
/// before the allocator has any memory at all, where `total` is zero and every
/// figure derived from it is meaningless.
fn target(used: usize, total: usize) -> usize {
    let free = total.saturating_sub(used);
    watermark(total).saturating_sub(free)
}

/// One pass. Separate from the loop so that what it decides can be tested
/// without a timer: answers what the deferred caches gave back.
fn pass(used: usize, total: usize) -> usize {
    // The watermark shortfall and whatever a failed allocation asked for are
    // the same kind of demand, and `request` keeps the larger of the two.
    shrinker::request(target(used, total));
    shrinker::run_deferred()
}

/// Whether the machine is out of memory: under the OOM watermark, and the
/// caches that could have helped gave `freed` bytes.
///
/// Both halves, in Linux's order: the OOM killer runs after reclaim has
/// failed, never instead of it. A pass that freed something has made progress,
/// and the next pass 200 ms later will ask again -- reclaiming a cache costs
/// nobody their process.
fn out_of_memory(used: usize, total: usize, freed: usize) -> bool {
    if freed > 0 {
        return false;
    }
    let free = total.saturating_sub(used);
    let floor = watermark(total) / OOM_WATERMARK_DIV;
    floor > 0 && free < floor
}

/// Start the worker. Called once, during boot.
pub fn start() {
    kernel_hal::thread::spawn(async {
        // Nothing to reclaim and nothing to reclaim it for: a build whose
        // caches all register as direct shrinkers has no work here, and a
        // task that wakes 5 times a second to find that out is 5 wakeups a
        // second of nothing.
        if shrinker::registered_deferred() == 0 {
            kernel_hal::klog_info!("reclaimd: no deferred caches registered, not starting");
            return;
        }
        loop {
            kernel_hal::thread::sleep_until(timer_now() + Duration::from_millis(PERIOD_MS)).await;
            // The page-table frames that let a coroutine stack get unmapped
            // guard bands. Spent, every new stack got a soft canary instead
            // and was leaked when it retired, 2.6 MiB at a time, until the
            // heap reached its ceiling: see `refill_split_pool`.
            #[cfg(not(feature = "libos"))]
            {
                let added = kernel_hal::stack_guard::refill_split_pool();
                if added > 0 {
                    log::info!(
                        "reclaimd: {} page-table frame(s) back in the stack-guard reserve",
                        added
                    );
                }
                // Idle coroutine stacks a burst left behind, before the heap
                // is given back below, so their blocks can go with it.
                let trimmed = executor::trim_idle_stacks(IDLE_STACKS_KEPT);
                // Large heap blocks freed since the last pass are still
                // mapped (a free cannot wait for the TLB flush): give back
                // all but a few for the next large allocation to reuse.
                #[cfg(target_arch = "x86_64")]
                {
                    let purged = kernel_hal::vmalloc::purge(VMALLOC_KEPT);
                    if purged > 0 {
                        log::info!(
                            "reclaimd: gave back {} KiB of freed large heap blocks",
                            purged / 1024
                        );
                    }
                }
                if trimmed > 0 {
                    log::info!(
                        "reclaimd: gave back {} KiB of idle coroutine stacks",
                        trimmed / 1024
                    );
                }
            }
            // Before the figures are read, so a pass that gave memory back
            // sees it as free. Not tied to the watermark: memory the heap
            // grew into and no longer uses goes back to the frame allocator
            // as soon as it is spare, which is what lets a program that
            // exits give back all it took. One relaxed load when the heap
            // never grew. Bounded per pass, so a gigabyte to give back is
            // spread over a few seconds rather than held in one tick. Not
            // within a few seconds of a growth: see `release_spare_heap`.
            let released = crate::memory::release_spare_heap(HEAP_RELEASE_PER_PASS);
            if released > 0 {
                log::info!(
                    "reclaimd: gave {} KiB of spare kernel heap back to the frame allocator",
                    released / 1024
                );
            }
            // A kernel heap OOM the emergency reserve absorbed (see
            // `crate::oom`): the allocator could not kill from where it was,
            // so the kill is served here, first, before anything else gets a
            // chance to allocate. Then, once the heap has room again, the
            // reserve goes back where the next OOM can find it.
            if crate::oom::take_kill_request() {
                match linux_object::process::kill_worst("kernel heap exhausted") {
                    Some(_) => {}
                    None => log::warn!(
                        "[oom] kernel heap exhausted: no process to kill (or the last \
                         victim is still exiting)"
                    ),
                }
            }
            if crate::memory::rearm_oom_reserve() {
                let (_, spent) = crate::memory::oom_reserve_status();
                log::warn!(
                    "[oom] kernel heap recovered ({} / {} MiB): emergency reserve re-armed \
                     (spent {} time(s) this boot)",
                    crate::memory::heap_used() >> 20,
                    crate::memory::heap_total() >> 20,
                    spent,
                );
            }
            let (used, total) = crate::memory::stats();
            let freed = pass(used, total);
            if out_of_memory(used, total, freed) {
                // Nothing left to reclaim and nothing left to allocate from.
                // This is where Linux's OOM killer runs, and it is the only
                // place one can: `#[alloc_error_handler]` is `-> !` and runs
                // on a heap that is already gone, so from there the machine is
                // lost and every process dies rather than the worst one.
                linux_object::process::kill_worst("reclaim gave nothing");
            }
            if freed > 0 {
                // warn, not info: a pass that freed something means the
                // machine was under the watermark or an allocation had already
                // failed, and that line next to the failure is what tells a
                // reader whether reclaim was working.
                log::warn!(
                    "reclaimd: freed {} KiB | {} MiB used / {} MiB total | {} KiB still reclaimable",
                    freed / 1024,
                    used / (1024 * 1024),
                    total / (1024 * 1024),
                    shrinker::deferred_reclaimable() / 1024,
                );
            }
        }
    });
}

#[cfg(test)]
#[path = "reclaimd_tests.rs"]
mod reclaimd_tests;
