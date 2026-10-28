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

/// How often the worker looks. Linux's `kswapd` sleeps on a waitqueue with a
/// 100 ms watchdog; this is the watchdog without the waitqueue.
const PERIOD_MS: u64 = 200;

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
