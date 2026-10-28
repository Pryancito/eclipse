//! The worker's policy, which is the part that can be wrong without anything
//! crashing: a watermark that is always met reclaims nothing, and one that is
//! never met walks every cache on every pass.

use super::*;
use core::sync::atomic::Ordering;

const MIB: usize = 1024 * 1024;

/// Above the watermark there is nothing to do. This is the normal state of the
/// machine, so it is the one that must not walk a cache.
#[test]
fn a_machine_with_memory_to_spare_asks_for_nothing() {
    assert_eq!(target(0, 4096 * MIB), 0);
    assert_eq!(target(1024 * MIB, 4096 * MIB), 0);
    // Exactly at the watermark is still not under it.
    let total = 4096 * MIB;
    assert_eq!(target(total - watermark(total), total), 0);
}

/// Under the watermark, the target is the shortfall and nothing more: asking
/// for the whole watermark would evict memory that is already free.
#[test]
fn the_target_is_the_shortfall_and_not_the_whole_watermark() {
    let total = 4096 * MIB;
    let wm = watermark(total);
    let free = wm / 4;
    assert_eq!(target(total - free, total), wm - free);
}

/// A machine with nothing left asks for the whole watermark, not for more.
#[test]
fn a_machine_with_nothing_free_asks_for_the_watermark() {
    let total = 1024 * MIB;
    assert_eq!(target(total, total), watermark(total));
    // And `used` past `total` (the heap and the frames are counted together,
    // and both move) is the same case, not an underflow.
    assert_eq!(target(total + MIB, total), watermark(total));
}

/// Before the allocator has any memory, every figure derived from the total is
/// meaningless -- and a watermark of zero is what keeps this task quiet during
/// the part of boot where the registry is still empty.
#[test]
fn an_allocator_with_no_memory_yet_asks_for_nothing() {
    assert_eq!(watermark(0), 0);
    assert_eq!(target(0, 0), 0);
}

/// The floor applies on a small machine...
#[test]
fn the_watermark_has_a_floor_a_small_machine_cannot_sink_below() {
    // 5 % of 512 MiB is 25 MiB, which is over the floor; 5 % of 64 MiB is
    // 3 MiB, which is under it and is raised.
    assert_eq!(
        watermark(512 * MIB),
        26_843_545,
        "not 5 % of a 512 MiB machine"
    );
    assert_eq!(watermark(64 * MIB), WATERMARK_FLOOR);
}

/// ... and `total / 4` applies under the floor, so that a machine smaller than
/// the floor does not report a shortage it can never satisfy. Without it a
/// 16 MiB machine would be permanently 8 MiB short and walk every cache on
/// every pass, for ever.
#[test]
fn a_machine_smaller_than_the_floor_is_not_permanently_short() {
    let total = 16 * MIB;
    assert_eq!(watermark(total), total / 4);
    assert!(
        target(0, total) == 0,
        "an empty machine was declared short of memory"
    );
    assert!(
        watermark(total) < total,
        "the watermark asks for more than the machine has"
    );
}

/// The ceiling applies on a large one: 5 % of 32 GiB is 1.6 GiB of page cache
/// thrown away to protect memory nothing is asking for.
#[test]
fn the_watermark_has_a_ceiling_a_large_machine_does_not_pass() {
    assert_eq!(watermark(32 * 1024 * MIB), WATERMARK_CEILING);
}

/// A cache that gives back whatever it is asked for, to prove a pass reaches
/// one at all. Statics, because a `Shrinker` holds `fn` pointers.
mod fake_cache {
    use core::sync::atomic::{AtomicUsize, Ordering};

    pub static HELD: AtomicUsize = AtomicUsize::new(0);
    pub static ASKED: AtomicUsize = AtomicUsize::new(0);

    fn count() -> usize {
        HELD.load(Ordering::Relaxed)
    }

    fn scan(want: usize) -> usize {
        ASKED.store(want, Ordering::Relaxed);
        let held = HELD.load(Ordering::Relaxed);
        let give = want.min(held);
        HELD.store(held - give, Ordering::Relaxed);
        give
    }

    pub static SHRINKER: kernel_hal::shrinker::Shrinker = kernel_hal::shrinker::Shrinker {
        name: "reclaimd-test",
        count,
        scan,
    };
}

/// A pass under the watermark turns the shortfall into a demand and serves it
/// from the deferred caches. This is the whole of what the worker does, minus
/// the sleep.
///
/// Registration is irreversible and the demand is one figure for the machine,
/// so this test drives the kernel's own statics; it is the only one here that
/// does, which is why nothing has to serialize it.
#[test]
fn a_pass_under_the_watermark_asks_the_deferred_caches_for_the_shortfall() {
    assert!(
        kernel_hal::shrinker::register_deferred(&fake_cache::SHRINKER),
        "the deferred registry refused the fake"
    );
    let total = 1024 * MIB;
    let wm = watermark(total);
    fake_cache::HELD.store(wm, Ordering::Relaxed);

    // Nothing free at all: the shortfall is the whole watermark.
    assert_eq!(pass(total, total), wm, "the pass did not serve the demand");
    assert_eq!(
        fake_cache::ASKED.load(Ordering::Relaxed),
        wm,
        "the cache was asked for something other than the shortfall"
    );

    // ... and a pass above the watermark asks for nothing, so it walks nobody.
    fake_cache::ASKED.store(0, Ordering::Relaxed);
    fake_cache::HELD.store(wm, Ordering::Relaxed);
    assert_eq!(pass(0, total), 0, "a pass with memory to spare reclaimed");
    assert_eq!(
        fake_cache::ASKED.load(Ordering::Relaxed),
        0,
        "a cache was walked with memory to spare"
    );
    fake_cache::HELD.store(0, Ordering::Relaxed);
}
