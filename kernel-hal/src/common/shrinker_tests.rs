//! What a shrinker registry has to get right is not the arithmetic, it is the
//! three ways it can make a shortage worse: recursing into itself on the
//! failure path, emptying whichever cache registered first, and answering with
//! bytes nobody freed. Each has a test here.

use super::*;
use core::sync::atomic::{AtomicBool, AtomicUsize};

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

fake_cache!(deferred_one, "deferred-one");
fake_cache!(direct_one, "direct-one");

/// The turnstile for everything below, which drives the kernel's OWN statics
/// (`DEMAND` and the deferred registry) rather than a registry of its own.
/// Those cannot be built per test -- registration is irreversible and the
/// demand is one figure for the machine -- so two of these tests in parallel
/// would read each other's demand and blame whichever lost.
static GLOBALS: lock::Mutex<()> = lock::Mutex::new(());

/// Put the fake in the kernel's deferred registry, once per test binary.
///
/// It holds nothing unless a test gives it something, and the walk skips a
/// cache holding nothing, so a test that does not care about it is unaffected.
fn with_deferred_fake<T>(held: usize, body: impl FnOnce() -> T) -> T {
    with_fakes(0, held, body)
}

/// The same, with a cache in the DIRECT registry too: `direct_held` bytes that
/// direct reclaim can give, `deferred_held` that only the worker can.
///
/// Both are registered once per test binary and reset per test, rather than
/// each test registering its own. Registration is irreversible, so a test that
/// registered on its own path would change what every later test sees -- and a
/// cache holding nothing is skipped by the walk, so a test that does not care
/// about one is unaffected by it.
fn with_fakes<T>(direct_held: usize, deferred_held: usize, body: impl FnOnce() -> T) -> T {
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    let _g = GLOBALS.lock();
    if !REGISTERED.swap(true, Ordering::Relaxed) {
        assert!(
            register_deferred(&deferred_one::SHRINKER),
            "the deferred registry refused the fake"
        );
        assert!(
            register(&direct_one::SHRINKER),
            "the direct registry refused the fake"
        );
    }
    direct_one::reset(direct_held);
    deferred_one::reset(deferred_held);
    DEMAND.store(0, Ordering::Relaxed);
    let out = body();
    direct_one::reset(0);
    deferred_one::reset(0);
    DEMAND.store(0, Ordering::Relaxed);
    out
}

#[test]
fn asking_the_worker_for_nothing_leaves_no_demand() {
    with_deferred_fake(0, || {
        request(0);
        assert_eq!(pending(), 0, "a zero request became a standing demand");
    });
}

/// The demand is the largest request since the last pass, not their sum: ten
/// allocations failing on 4 MiB each mean 4 MiB are missing ten times over,
/// and summing them sends the worker after 40 MiB that nobody wants.
#[test]
fn the_standing_demand_is_the_largest_request_and_not_their_sum() {
    with_deferred_fake(0, || {
        request(4 << 20);
        request(1 << 20);
        request(4 << 20);
        assert_eq!(pending(), 4 << 20, "the requests were summed");
    });
}

#[test]
fn a_pass_with_no_demand_asks_nobody() {
    with_deferred_fake(8 << 20, || {
        assert_eq!(run_deferred(), 0);
        assert_eq!(
            deferred_one::CALLS.load(Ordering::Relaxed),
            0,
            "a deferred cache was walked with nothing to serve"
        );
    });
}

#[test]
fn a_pass_serves_the_standing_demand_and_answers_what_was_freed() {
    with_deferred_fake(8 << 20, || {
        request(2 << 20);
        assert_eq!(
            run_deferred(),
            2 << 20,
            "the pass did not free what it said"
        );
        assert!(
            deferred_one::CALLS.load(Ordering::Relaxed) > 0,
            "the deferred cache was never asked"
        );
    });
}

/// The demand is taken before the walk, so what the walk itself requests is
/// served by the next pass instead of being served twice -- and a second pass
/// with nothing new to do asks nobody.
#[test]
fn a_pass_clears_the_demand_it_took() {
    with_deferred_fake(8 << 20, || {
        request(2 << 20);
        assert_eq!(run_deferred(), 2 << 20);
        assert_eq!(pending(), 0, "the demand outlived the pass that served it");
        let calls = deferred_one::CALLS.load(Ordering::Relaxed);
        assert_eq!(run_deferred(), 0, "the same demand was served twice");
        assert_eq!(
            deferred_one::CALLS.load(Ordering::Relaxed),
            calls,
            "the second pass walked the caches again"
        );
    });
}

/// What direct reclaim could not give is what the worker is for. Nothing is
/// registered in the direct registry here, so the whole target is the
/// shortfall.
#[test]
fn what_direct_reclaim_cannot_give_becomes_the_workers_demand() {
    with_fakes(0, 0, || {
        assert_eq!(shrink(3 << 20), 0, "a cache holding nothing gave something");
        assert_eq!(
            pending(),
            3 << 20,
            "the shortfall of a direct reclaim was dropped"
        );
    });
}

/// ... and a deferred cache is not asked by direct reclaim, however much it
/// holds. This is the separation the whole split exists for: the caller of
/// `shrink` is an allocation that has already failed, and this cache's scan
/// writes to disk.
#[test]
fn direct_reclaim_never_walks_a_deferred_cache() {
    with_deferred_fake(64 << 20, || {
        assert_eq!(
            shrink(1 << 20),
            0,
            "a deferred cache answered direct reclaim"
        );
        assert_eq!(
            deferred_one::CALLS.load(Ordering::Relaxed),
            0,
            "direct reclaim walked a deferred cache"
        );
        assert_eq!(
            reclaimable(),
            0,
            "a deferred cache was counted as directly reclaimable"
        );
        assert_eq!(
            deferred_reclaimable(),
            64 << 20,
            "the deferred registry did not count its own cache"
        );
    });
}

#[test]
fn the_worker_counts_its_passes_and_its_bytes() {
    with_deferred_fake(8 << 20, || {
        let (calls, freed, _) = deferred_stats();
        request(1 << 20);
        assert_eq!(run_deferred(), 1 << 20);
        let (calls_after, freed_after, _) = deferred_stats();
        assert_eq!(calls_after, calls + 1, "the pass was not counted");
        assert_eq!(
            freed_after - freed,
            1 << 20,
            "the bytes freed were not counted"
        );
    });
}

/// ... and only the shortfall. A direct reclaim that covered the whole target
/// leaves the worker nothing to do: raising a demand there would walk the page
/// cache -- a filesystem write per dirty page -- for memory that is already
/// free.
#[test]
fn a_direct_reclaim_that_covered_its_target_leaves_no_demand() {
    with_fakes(8 << 20, 0, || {
        assert_eq!(shrink(2 << 20), 2 << 20, "the direct cache gave nothing");
        assert_eq!(
            pending(),
            0,
            "a target direct reclaim met in full still woke the worker"
        );
    });
}

/// Half covered is half asked for.
#[test]
fn the_demand_is_what_direct_reclaim_left_short() {
    with_fakes(1 << 20, 0, || {
        assert_eq!(shrink(3 << 20), 1 << 20);
        assert_eq!(
            pending(),
            2 << 20,
            "the demand is not what direct reclaim left short"
        );
    });
}
