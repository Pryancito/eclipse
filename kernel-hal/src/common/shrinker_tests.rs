//! What a shrinker registry has to get right is not the arithmetic, it is the
//! three ways it can make a shortage worse: recursing into itself on the
//! failure path, emptying whichever cache registered first, and answering with
//! bytes nobody freed. Each has a test here.

use super::*;
use core::sync::atomic::AtomicUsize;

/// A cache of `held` bytes that frees exactly what it is asked for, and
/// remembers what it was asked. Statics, because a `Shrinker` holds `fn`
/// pointers and a `fn` cannot capture.
macro_rules! fake_cache {
    ($name:ident, $label:literal) => {
        mod $name {
            use super::*;
            pub static HELD: AtomicUsize = AtomicUsize::new(0);
            pub static ASKED: AtomicUsize = AtomicUsize::new(0);
            pub static CALLS: AtomicUsize = AtomicUsize::new(0);

            pub fn reset(held: usize) {
                HELD.store(held, Ordering::Relaxed);
                ASKED.store(0, Ordering::Relaxed);
                CALLS.store(0, Ordering::Relaxed);
            }
            fn count() -> usize {
                HELD.load(Ordering::Relaxed)
            }
            fn scan(want: usize) -> usize {
                ASKED.fetch_add(want, Ordering::Relaxed);
                CALLS.fetch_add(1, Ordering::Relaxed);
                let held = HELD.load(Ordering::Relaxed);
                let give = want.min(held);
                HELD.store(held - give, Ordering::Relaxed);
                give
            }
            pub static SHRINKER: Shrinker = Shrinker {
                name: $label,
                count,
                scan,
            };
        }
    };
}

fake_cache!(big, "big");
fake_cache!(small, "small");
fake_cache!(empty, "empty");

const SLOT: usize = 0;

#[test]
fn an_empty_registry_frees_nothing_and_says_so() {
    let r = Registry::new();
    assert_eq!(r.registered(), 0);
    assert_eq!(r.reclaimable(), 0);
    assert_eq!(r.shrink_on(SLOT, 1 << 20), 0);
    // And the attempt is still counted: a reclaim that found no caches is a
    // different thing from a reclaim that was never tried, and the OOM report
    // reads this to tell them apart.
    assert_eq!(r.stats().0, 1, "the attempt was not counted");
}

#[test]
fn asking_for_nothing_asks_nobody() {
    big::reset(1 << 20);
    let r = Registry::new();
    assert!(r.register(&big::SHRINKER));
    assert_eq!(r.shrink_on(SLOT, 0), 0);
    assert_eq!(big::CALLS.load(Ordering::Relaxed), 0, "a cache was asked");
    assert_eq!(r.stats().0, 0, "a no-op counted as an attempt");
}

#[test]
fn the_registry_refuses_the_one_past_its_capacity() {
    big::reset(0);
    let r = Registry::new();
    for i in 0..MAX_SHRINKERS {
        assert!(r.register(&big::SHRINKER), "registration {} was refused", i);
    }
    assert!(
        !r.register(&big::SHRINKER),
        "the registry took one past its capacity: the array behind it is \
         fixed, and growing it on the allocation-failure path is the \
         allocation that cannot be made"
    );
    assert_eq!(r.registered(), MAX_SHRINKERS);
}

#[test]
fn reclaimable_is_what_the_caches_say_they_hold() {
    big::reset(8 << 20);
    small::reset(2 << 20);
    empty::reset(0);
    let r = Registry::new();
    assert!(r.register(&big::SHRINKER));
    assert!(r.register(&small::SHRINKER));
    assert!(r.register(&empty::SHRINKER));
    assert_eq!(r.reclaimable(), 10 << 20);
}

/// The failure this is really about: asking each cache for the whole target
/// empties whichever registered first and leaves the rest untouched, so the
/// next shortage has nothing left to reclaim from. The share is proportional.
#[test]
fn the_target_is_split_in_proportion_to_what_each_cache_holds() {
    big::reset(90 << 20);
    small::reset(10 << 20);
    let r = Registry::new();
    assert!(r.register(&big::SHRINKER));
    assert!(r.register(&small::SHRINKER));

    let want = 10 << 20;
    assert_eq!(r.shrink_on(SLOT, want), want, "the target was not met");

    let from_big = (90 << 20) - big::HELD.load(Ordering::Relaxed);
    let from_small = (10 << 20) - small::HELD.load(Ordering::Relaxed);
    assert_eq!(from_big + from_small, want);
    assert!(
        from_big > from_small * 4,
        "the cache holding nine tenths gave {} and the other gave {}: the \
         split is not proportional",
        from_big,
        from_small,
    );
    assert!(
        from_small > 0,
        "the smaller cache was not asked at all, so it will still be full at \
         the next shortage"
    );
}

/// A cache that says it holds nothing is not asked. Its `scan` may be the
/// expensive half (a walk under a lock), and a cache with nothing to give has
/// no reason to pay it on a failure path.
#[test]
fn a_cache_holding_nothing_is_not_asked() {
    big::reset(4 << 20);
    empty::reset(0);
    let r = Registry::new();
    assert!(r.register(&empty::SHRINKER));
    assert!(r.register(&big::SHRINKER));
    r.shrink_on(SLOT, 1 << 20);
    assert_eq!(
        empty::CALLS.load(Ordering::Relaxed),
        0,
        "the empty cache was walked for nothing"
    );
    assert!(big::CALLS.load(Ordering::Relaxed) > 0);
}

/// Rounding, and a cache that gives less than its share, leave the target
/// short while another cache still holds plenty. The second pass is what
/// closes the gap, and without it a reclaim reports a shortfall it did not
/// have to have.
#[test]
fn what_one_cache_cannot_give_is_asked_of_the_others() {
    big::reset(1 << 20);
    small::reset(16 << 20);
    let r = Registry::new();
    // The one that can only give a little registers FIRST, so the first pass
    // under-delivers and only the second pass can make the target.
    assert!(r.register(&big::SHRINKER));
    assert!(r.register(&small::SHRINKER));
    let want = 8 << 20;
    assert_eq!(
        r.shrink_on(SLOT, want),
        want,
        "the target was short while a cache still held memory: there is no \
         second pass"
    );
}

/// Nobody gives more than they have, and the answer is what was freed and not
/// what was asked: an allocation that retries on a lie fails again, and the
/// OOM report then quotes bytes that were never reclaimed.
#[test]
fn the_answer_is_what_was_freed_and_never_more() {
    big::reset(1 << 20);
    let r = Registry::new();
    assert!(r.register(&big::SHRINKER));
    assert_eq!(r.shrink_on(SLOT, 8 << 20), 1 << 20);
    assert_eq!(big::HELD.load(Ordering::Relaxed), 0);
    // And a second reclaim on an empty cache gives nothing rather than
    // repeating the first answer.
    assert_eq!(r.shrink_on(SLOT, 8 << 20), 0);
}

// --- the re-entrancy guard -----------------------------------------------

/// A shrinker frees through the same allocator that called it, so its own work
/// can allocate, fail, and land back in `shrink`. Without the per-CPU flag the
/// first real shortage is an infinite recursion on the failure path instead of
/// an OOM report.
///
/// Driven through a real nested call: the cache's `scan` reaches back into the
/// registry it is being scanned by.
#[test]
fn a_shrinker_that_reclaims_again_is_refused_rather_than_recursing() {
    static NESTED: Registry = Registry::new();
    static INNER_ANSWER: AtomicUsize = AtomicUsize::new(usize::MAX);
    static DEPTH: AtomicUsize = AtomicUsize::new(0);

    fn count() -> usize {
        4 << 20
    }
    fn scan(_want: usize) -> usize {
        let depth = DEPTH.fetch_add(1, Ordering::Relaxed);
        assert!(depth < 4, "the guard did not stop the recursion");
        // The allocation a real shrinker's own work would make, failing and
        // reclaiming again -- on the same CPU, inside this very scan.
        INNER_ANSWER.store(NESTED.shrink_on(SLOT, 1 << 20), Ordering::Relaxed);
        0
    }
    static RECURSIVE: Shrinker = Shrinker {
        name: "recursive",
        count,
        scan,
    };

    assert!(NESTED.register(&RECURSIVE));
    NESTED.shrink_on(SLOT, 1 << 20);
    assert_eq!(
        INNER_ANSWER.load(Ordering::Relaxed),
        0,
        "the nested reclaim ran instead of being refused"
    );
    assert_eq!(
        DEPTH.load(Ordering::Relaxed),
        2,
        "the scan ran a number of times that is neither once per pass nor a \
         refusal"
    );
    // One refusal per nested call, and the scan runs once per pass: this cache
    // always claims to hold memory and never frees any, so the second pass
    // asks it again. Derived from `DEPTH` rather than written as 2, so the two
    // numbers cannot drift apart.
    assert_eq!(
        NESTED.stats().2,
        DEPTH.load(Ordering::Relaxed) as u64,
        "the refusals were not counted one per nested reclaim"
    );
}

/// And the flag comes back down: a reclaim refused as re-entrant must not
/// leave the CPU unable to reclaim for ever after.
#[test]
fn the_guard_is_released_when_the_reclaim_ends() {
    big::reset(4 << 20);
    let r = Registry::new();
    assert!(r.register(&big::SHRINKER));
    assert!(r.shrink_on(SLOT, 1 << 20) > 0);
    big::reset(4 << 20);
    assert!(
        r.shrink_on(SLOT, 1 << 20) > 0,
        "the second reclaim on this CPU found the flag still raised"
    );
    assert_eq!(r.stats().2, 0, "a sequential reclaim was called re-entrant");
}

/// One CPU reclaiming does not stop another: the flag is per CPU because the
/// recursion it prevents is per CPU, and two cores under pressure at once is
/// the normal case, not the exception.
#[test]
fn one_cpu_reclaiming_does_not_block_another() {
    static OTHER: Registry = Registry::new();
    static OTHER_ANSWER: AtomicUsize = AtomicUsize::new(usize::MAX);

    fn count() -> usize {
        4 << 20
    }
    fn scan(_want: usize) -> usize {
        // A different slot: the peer CPU, not this one.
        OTHER_ANSWER.store(OTHER.shrink_on(1, 1 << 20), Ordering::Relaxed);
        1 << 20
    }
    static PEER: Shrinker = Shrinker {
        name: "peer",
        count,
        scan,
    };

    assert!(OTHER.register(&PEER));
    OTHER.shrink_on(0, 1 << 20);
    assert!(
        OTHER_ANSWER.load(Ordering::Relaxed) > 0,
        "slot 1 was refused while slot 0 was reclaiming: the flag is global, \
         not per CPU"
    );
}

/// A slot past the per-CPU table is not a reclaim on CPU 0's flag. `cpu_id()`
/// can answer with something that names no CPU, and `percpu_slot` sends that
/// to the quarantine slot -- which is inside the table. Anything past it is a
/// caller bug, and the answer is to do nothing rather than to share a flag.
#[test]
fn a_slot_past_the_table_reclaims_nothing() {
    big::reset(4 << 20);
    let r = Registry::new();
    assert!(r.register(&big::SHRINKER));
    assert_eq!(r.shrink_on(PERCPU_SLOTS, 1 << 20), 0);
    assert_eq!(big::CALLS.load(Ordering::Relaxed), 0);
}
