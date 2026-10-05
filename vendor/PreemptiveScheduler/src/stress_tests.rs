//! Stress tests: the scheduler's shared state under real concurrency.
//!
//! The unit tests in each module pin one interleaving at a time. These run
//! many threads against the same bitmaps, run queues and latches for tens of
//! thousands of operations and check the invariants that hold regardless of
//! interleaving:
//!
//! * a published wake is never lost (every woken slot is handed out again);
//! * a task is never handed out twice at once (owner and thieves included);
//! * a task pinned elsewhere is never handed to this CPU;
//! * a dropped task is handed out at most once more and then reclaimed;
//! * a parked frame has at most one claimant;
//! * a burst of reschedule requests for one CPU costs one IPI;
//! * the placement/kick pickers never name a CPU outside their inputs.
//!
//! Host `cpu_id()` is 0 on every thread, so "this CPU" is CPU 0 throughout and
//! an affinity mask without bit 0 is how "pinned elsewhere" is spelled. The
//! executors themselves (2.6 MiB stacks, `switch.S`) are not driven here.
//!
//! Sizes are chosen to finish in a few seconds in a debug build. Anything that
//! touches `runtime`'s reschedule globals takes `resched_test_lock`, like the
//! unit tests that share them.

use crate::executor::ResumeClaim;
use crate::runtime::{
    affinity_home, park_cpu_if_cold, pick_affinity_kick_target_with_loads, request_resched,
    resched_test_lock, set_executor_ready_mask_for_test, set_resched_ipi_sender,
    should_pull_for_balance,
};
use crate::task_collection::{key::*, Task, TaskCollection};
use crate::waker_page::{WakerPage, WakerRef};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;

const MAX_CORE_NUM: usize = lock::MAX_CORE_NUM;

/// xorshift64*: deterministic per seed, no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

fn pending() -> impl core::future::Future<Output = ()> + Send + 'static {
    core::future::pending::<()>()
}

static IPIS: AtomicU64 = AtomicU64::new(0);
fn count_ipi(_cpu: usize) {
    IPIS.fetch_add(1, Ordering::SeqCst);
}

/// A `TaskCollection` shared between test threads.
///
/// The kernel shares one across CPUs through `ExecutorRuntime`, which carries
/// the `unsafe impl Sync`; the coroutine inside is only ever resumed under its
/// `spin::Mutex` (`take_task` / `try_take_task`), which is the property that
/// makes the sharing sound there and here alike. Thieves on other CPUs are
/// exactly what these tests model.
#[derive(Clone)]
struct Shared(Arc<TaskCollection>);
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl core::ops::Deref for Shared {
    type Target = TaskCollection;
    fn deref(&self) -> &TaskCollection {
        &self.0
    }
}

// ── WakerPage ───────────────────────────────────────────────────────────────

/// Four producers hammer one page with wakes while a consumer drains it and
/// randomly checks slots in and out. No wake may be lost, and no borrowed
/// slot may ever be handed out.
#[test]
fn stress_waker_page_never_loses_a_wake_nor_hands_out_a_borrowed_slot() {
    const PRODUCERS: usize = 4;
    const WAKES_EACH: usize = 20_000;

    let page = WakerPage::new(0);
    let wakes: Arc<Vec<AtomicUsize>> = Arc::new((0..64).map(|_| AtomicUsize::new(0)).collect());
    let takes: Arc<Vec<AtomicUsize>> = Arc::new((0..64).map(|_| AtomicUsize::new(0)).collect());
    // Per slot: wakes published and not yet answered by a hand-out. A hand-out
    // answers everything published before it, so over-zeroing is possible and
    // only ever hides a fault. A count left standing at the end names a wake
    // that reached the page and was dropped -- which "handed out at least
    // once" cannot see, because a slot woken a thousand times is still handed
    // out even when a wake that raced a borrow is thrown away.
    let owed: Arc<Vec<AtomicUsize>> = Arc::new((0..64).map(|_| AtomicUsize::new(0)).collect());
    let done = Arc::new(AtomicBool::new(false));

    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let page = page.clone();
            let wakes = wakes.clone();
            let owed = owed.clone();
            thread::spawn(move || {
                let mut rng = Rng::new(0x9E37_79B9 + p as u64);
                for _ in 0..WAKES_EACH {
                    let slot = rng.below(64);
                    wakes[slot].fetch_add(1, Ordering::SeqCst);
                    owed[slot].fetch_add(1, Ordering::SeqCst);
                    page.notify(slot);
                }
            })
        })
        .collect();

    let consumer = {
        let page = page.clone();
        let takes = takes.clone();
        let owed = owed.clone();
        let done = done.clone();
        thread::spawn(move || {
            let mut rng = Rng::new(0xC0FF_EE);
            let mut borrowed: u64 = 0;
            loop {
                // Check a few slots in or out, as polls starting and ending.
                for _ in 0..3 {
                    let slot = rng.below(64);
                    let bit = 1u64 << slot;
                    if borrowed & bit != 0 {
                        page.mark_borrowed(slot, false);
                        borrowed &= !bit;
                    } else if rng.chance(50) {
                        page.mark_borrowed(slot, true);
                        borrowed |= bit;
                    }
                }
                let taken = page.take_notified();
                assert_eq!(
                    taken & borrowed,
                    0,
                    "a borrowed slot was handed out: taken={:#x} borrowed={:#x}",
                    taken,
                    borrowed
                );
                for slot in 0..64 {
                    if taken & (1u64 << slot) != 0 {
                        takes[slot].fetch_add(1, Ordering::SeqCst);
                        owed[slot].store(0, Ordering::SeqCst);
                    }
                }
                if done.load(Ordering::SeqCst) {
                    // Producers finished, so from here nothing races this
                    // thread. Borrow a fixed set of slots, wake each one while
                    // it is borrowed, and only then release: the wake has to
                    // be held and come back. During the concurrent phase above
                    // a swallowed wake can be covered up by the next of the
                    // ~1250 wakes that slot gets, which is why this window
                    // exists and is quiet.
                    for slot in 0..8 {
                        if borrowed & (1u64 << slot) == 0 {
                            page.mark_borrowed(slot, true);
                            borrowed |= 1u64 << slot;
                        }
                        owed[slot].fetch_add(1, Ordering::SeqCst);
                        page.notify(slot);
                    }
                    let quiet = page.take_notified();
                    assert_eq!(
                        quiet & 0xff,
                        0,
                        "a borrowed slot was handed out in the quiet window"
                    );
                    // Whatever else came out here is a hand-out like any
                    // other; not booking it would leave its slot owed a wake
                    // that was in fact delivered.
                    for slot in 0..64 {
                        if quiet & (1u64 << slot) != 0 {
                            takes[slot].fetch_add(1, Ordering::SeqCst);
                            owed[slot].store(0, Ordering::SeqCst);
                        }
                    }
                    // Release every borrow and drain whatever was deferred
                    // behind them.
                    for slot in 0..64 {
                        if borrowed & (1u64 << slot) != 0 {
                            page.mark_borrowed(slot, false);
                        }
                    }
                    let tail = page.take_notified();
                    assert_eq!(
                        tail & 0xff,
                        0xff,
                        "the wakes published behind a borrow did not come back \
                         once it was released: tail={:#x}",
                        tail
                    );
                    for slot in 0..64 {
                        if tail & (1u64 << slot) != 0 {
                            takes[slot].fetch_add(1, Ordering::SeqCst);
                            owed[slot].store(0, Ordering::SeqCst);
                        }
                    }
                    assert_eq!(page.take_notified(), 0, "a second drain found more");
                    break;
                }
            }
        })
    };

    for p in producers {
        p.join().unwrap();
    }
    done.store(true, Ordering::SeqCst);
    consumer.join().unwrap();

    // The producers ran: without this every check below is satisfied by a run
    // in which nothing was ever woken.
    let published: usize = wakes.iter().map(|w| w.load(Ordering::SeqCst)).sum();
    assert_eq!(
        published,
        PRODUCERS * WAKES_EACH,
        "a producer never published its wakes"
    );
    let drained: usize = takes.iter().map(|t| t.load(Ordering::SeqCst)).sum();
    assert!(drained > 0, "the consumer never drained a single wake");
    for slot in 0..64 {
        let w = wakes[slot].load(Ordering::SeqCst);
        let t = takes[slot].load(Ordering::SeqCst);
        assert_eq!(
            owed[slot].load(Ordering::SeqCst),
            0,
            "slot {}: a wake reached the page and was never handed out again",
            slot
        );
        if w > 0 {
            assert!(t >= 1, "slot {}: {} wakes, never handed out", slot, w);
        }
        assert!(
            t <= w,
            "slot {}: handed out {} times for {} wakes",
            slot,
            t,
            w
        );
    }
    let (n, d, b) = page.peek();
    assert_eq!((n, d, b), (0, 0, 0), "lanes left dirty");
}

/// Waking through `WakerRef` — the real entry point — from many threads
/// while slots are borrowed and released. Covers the deferral + IPI path.
#[test]
fn stress_waker_ref_wakes_from_many_threads_are_all_delivered() {
    let _g = resched_test_lock();
    set_resched_ipi_sender(count_ipi);
    const THREADS: usize = 6;
    const ROUNDS: usize = 5_000;

    let page = WakerPage::new(0);
    let flags: Vec<Arc<AtomicBool>> = (0..64).map(|_| Arc::new(AtomicBool::new(false))).collect();
    let wakers: Arc<Vec<Arc<WakerRef>>> = Arc::new(
        (0..64)
            .map(|i| {
                page.initialize(i);
                Arc::new(page.make_waker(i, &flags[i]))
            })
            .collect(),
    );
    page.take_notified();
    let woken: Arc<Vec<AtomicUsize>> = Arc::new((0..64).map(|_| AtomicUsize::new(0)).collect());

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let wakers = wakers.clone();
            let woken = woken.clone();
            let page = page.clone();
            thread::spawn(move || {
                let mut rng = Rng::new(7 + t as u64);
                for _ in 0..ROUNDS {
                    let i = rng.below(64);
                    if rng.chance(20) {
                        // Simulate a poll in flight on this slot elsewhere.
                        page.mark_borrowed(i, true);
                        wakers[i].wake_by_ref();
                        woken[i].fetch_add(1, Ordering::SeqCst);
                        wakers[i].mark_borrowed(false);
                    } else {
                        wakers[i].wake_by_ref();
                        woken[i].fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let published: usize = woken.iter().map(|w| w.load(Ordering::SeqCst)).sum();
    assert_eq!(
        published,
        THREADS * ROUNDS,
        "a waker thread never ran, so nothing below was tested"
    );
    let taken = page.take_notified();
    assert_ne!(taken, 0, "every wake from every thread left the lane empty");
    for i in 0..64 {
        if woken[i].load(Ordering::SeqCst) > 0 {
            assert!(
                taken & (1u64 << i) != 0,
                "slot {} was woken and is not pending",
                i
            );
        }
    }
    assert_eq!(page.take_notified(), 0);
}

// ── TaskCollection ──────────────────────────────────────────────────────────

struct Stressed {
    tc: Shared,
    wakers: Vec<Arc<WakerRef>>,
    keys: Vec<Key>,
}

/// Build a collection with `n` tasks, `pinned_every`th one pinned to CPU 1,
/// drain it once to collect every task's waker, and hand everything back.
fn stressed_collection(n: usize, pinned_every: usize) -> Stressed {
    let tc = TaskCollection::new(0);
    let mut keys = Vec::with_capacity(n);
    for i in 0..n {
        let affinity = if pinned_every != 0 && i % pinned_every == 0 {
            Some(Arc::new(AtomicU64::new(1 << 1)))
        } else {
            None
        };
        keys.push(tc.add_task(pending(), affinity));
    }
    // Pinned tasks are never handed out here, so their wakers come from the
    // slab directly.
    let mut wakers: Vec<Option<Arc<WakerRef>>> = (0..n).map(|_| None).collect();
    while let Some((key, _task, waker)) = tc.take_task() {
        let idx = keys.iter().position(|&k| k == key).unwrap();
        wakers[idx] = Some(waker);
    }
    for idx in 0..n {
        if wakers[idx].is_none() {
            let mut inner = tc.get_mut_inner(DEFAULT_PRIORITY);
            let task: &Task = inner.slab.get(unmask_priority(keys[idx])).unwrap();
            wakers[idx] = Some(task.waker().clone());
        }
    }
    let wakers: Vec<Arc<WakerRef>> = wakers.into_iter().map(|w| w.unwrap()).collect();
    for (idx, w) in wakers.iter().enumerate() {
        if pinned_every == 0 || idx % pinned_every != 0 {
            w.mark_borrowed(false);
        }
    }
    Stressed {
        tc: Shared(tc),
        wakers,
        keys,
    }
}

/// Owner (`take_task`) and thieves (`try_take_task`) drain one collection
/// while producers wake tasks at random and one thread retires some.
#[test]
fn stress_task_collection_hands_each_task_out_exactly_once_at_a_time() {
    let _g = resched_test_lock();
    set_resched_ipi_sender(count_ipi);
    let saved_ready = set_executor_ready_mask_for_test(0b11);

    const TASKS: usize = 256; // four waker pages
    const PINNED_EVERY: usize = 10;
    const PRODUCERS: usize = 3;
    const WAKES_EACH: usize = 15_000;
    const THIEVES: usize = 2;

    let s = stressed_collection(TASKS, PINNED_EVERY);
    let tc = s.tc.clone();
    let wakers = Arc::new(s.wakers);
    let keys = Arc::new(s.keys);
    let in_flight: Arc<Vec<AtomicUsize>> =
        Arc::new((0..TASKS).map(|_| AtomicUsize::new(0)).collect());
    let handouts: Arc<Vec<AtomicUsize>> =
        Arc::new((0..TASKS).map(|_| AtomicUsize::new(0)).collect());
    let dropped_at: Arc<Vec<AtomicUsize>> =
        Arc::new((0..TASKS).map(|_| AtomicUsize::new(usize::MAX)).collect());
    let handouts_after_drop: Arc<Vec<AtomicUsize>> =
        Arc::new((0..TASKS).map(|_| AtomicUsize::new(0)).collect());
    let done = Arc::new(AtomicBool::new(false));
    // Thieves still running. An owner may not conclude its collection is empty
    // while a thief can still be holding one of its tasks: releasing that
    // borrow re-publishes whatever wake was deferred behind it, so a task can
    // reappear *after* a pass that found nothing. Counting idle passes is a
    // guess at that and loses the race on a loaded box; this is the fact.
    let thieves_left = Arc::new(AtomicUsize::new(THIEVES));

    let index_of = {
        let keys = keys.clone();
        move |key: Key| {
            keys.iter()
                .position(|&k| k == key)
                .expect("unknown key handed out")
        }
    };

    // Drainers: one owner, several thieves.
    let drainers: Vec<_> = (0..1 + THIEVES)
        .map(|d| {
            let tc = tc.clone();
            let in_flight = in_flight.clone();
            let handouts = handouts.clone();
            let dropped_at = dropped_at.clone();
            let handouts_after_drop = handouts_after_drop.clone();
            let done = done.clone();
            let thieves_left = thieves_left.clone();
            let index_of = index_of.clone();
            thread::spawn(move || {
                // Set once the owner has read "no thief is holding anything":
                // the *next* empty pass is then conclusive. Routing that pass
                // through the loop, rather than taking a task on the side,
                // keeps every hand-out booked and every borrow released.
                let mut settling = false;
                loop {
                    let got = if d == 0 {
                        tc.take_task()
                    } else {
                        tc.try_take_task()
                    };
                    match got {
                        Some((key, task, waker)) => {
                            settling = false;
                            let idx = index_of(key);
                            assert!(
                                task.allowed_on(0),
                                "task {} pinned elsewhere was handed to CPU 0",
                                idx
                            );
                            let prev = in_flight[idx].fetch_add(1, Ordering::SeqCst);
                            assert_eq!(prev, 0, "task {} handed out twice at once", idx);
                            handouts[idx].fetch_add(1, Ordering::SeqCst);
                            if dropped_at[idx].load(Ordering::SeqCst) != usize::MAX {
                                handouts_after_drop[idx].fetch_add(1, Ordering::SeqCst);
                            }
                            // A poll of a few hundred ns.
                            for _ in 0..50 {
                                core::hint::spin_loop();
                            }
                            in_flight[idx].fetch_sub(1, Ordering::SeqCst);
                            waker.mark_borrowed(false);
                        }
                        None => {
                            if !done.load(Ordering::SeqCst) {
                                thread::yield_now();
                                continue;
                            }
                            if d != 0 {
                                // A thief stops at the first empty pass after
                                // the producers are done, and says so.
                                thieves_left.fetch_sub(1, Ordering::SeqCst);
                                break;
                            }
                            if thieves_left.load(Ordering::SeqCst) != 0 {
                                thread::yield_now();
                                continue;
                            }
                            // No thief can hold a task any more, so one more
                            // empty pass *after* that read settles it.
                            if settling {
                                break;
                            }
                            settling = true;
                        }
                    }
                }
            })
        })
        .collect();

    let published = Arc::new(AtomicUsize::new(0));
    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let wakers = wakers.clone();
            let published = published.clone();
            thread::spawn(move || {
                let mut rng = Rng::new(101 + p as u64);
                for _ in 0..WAKES_EACH {
                    published.fetch_add(1, Ordering::SeqCst);
                    wakers[rng.below(TASKS)].wake_by_ref();
                }
            })
        })
        .collect();

    // Retire ~5% of the unpinned tasks part-way through.
    let reaper = {
        let wakers = wakers.clone();
        let dropped_at = dropped_at.clone();
        thread::spawn(move || {
            let mut rng = Rng::new(0xDEAD);
            let mut n = 0;
            for idx in 0..TASKS {
                if idx % PINNED_EVERY != 0 && rng.chance(5) {
                    dropped_at[idx].store(n, Ordering::SeqCst);
                    wakers[idx].drop_by_ref();
                    n += 1;
                    thread::yield_now();
                }
            }
            n
        })
    };

    for p in producers {
        p.join().unwrap();
    }
    let dropped = reaper.join().unwrap();
    done.store(true, Ordering::SeqCst);
    for d in drainers {
        d.join().unwrap();
    }
    // Final owner pass reaps every `dropped` bit.
    assert!(tc.take_task().is_none());
    set_executor_ready_mask_for_test(saved_ready);
    // The pinned tasks kicked CPU 1; don't leave that request pending for
    // whichever test counts requests next.
    crate::runtime::clear_need_resched(1);

    // The producers ran and the drainers saw their wakes: a run in which
    // nothing was woken satisfies every count below on its own.
    assert_eq!(
        published.load(Ordering::SeqCst),
        PRODUCERS * WAKES_EACH,
        "a producer never published its wakes"
    );
    let drained: usize = handouts.iter().map(|h| h.load(Ordering::SeqCst)).sum();
    assert!(drained > 0, "no drainer was ever handed a task");

    let pinned = (0..TASKS).filter(|i| i % PINNED_EVERY == 0).count();
    for idx in 0..TASKS {
        let h = handouts[idx].load(Ordering::SeqCst);
        if idx % PINNED_EVERY == 0 {
            assert_eq!(h, 0, "pinned task {} was polled on CPU 0", idx);
        } else if dropped_at[idx].load(Ordering::SeqCst) != usize::MAX {
            assert!(
                handouts_after_drop[idx].load(Ordering::SeqCst) <= 1,
                "retired task {} kept being handed out",
                idx
            );
        }
        assert_eq!(in_flight[idx].load(Ordering::SeqCst), 0);
    }
    assert_eq!(
        tc.task_num(),
        TASKS - dropped,
        "task_num did not follow {} retirements",
        dropped
    );
    // Pinned tasks are still waiting for CPU 1 — nothing else is pending.
    let (_, notified, _, borrowed) = tc.debug_pending();
    assert_eq!(borrowed, 0, "a borrow was left set");
    assert_eq!(
        notified as usize, pinned,
        "pending wakes other than the pinned tasks"
    );
    assert_eq!(tc.ready_num_for(0), Some(0));
    assert_eq!(tc.ready_num_for(1), Some(pinned));
}

/// Several collections (one per victim CPU) raided by several thieves at
/// once, while their owners drain them too. Exclusivity must hold across
/// collections and `try_take_task` must never block a thief.
#[test]
fn stress_steal_across_collections_keeps_handouts_exclusive() {
    let _g = resched_test_lock();
    set_resched_ipi_sender(count_ipi);
    const VICTIMS: usize = 4;
    const TASKS_EACH: usize = 128;
    const THIEVES: usize = 4;
    const WAKES_EACH: usize = 20_000;

    let cols: Vec<Stressed> = (0..VICTIMS)
        .map(|_| stressed_collection(TASKS_EACH, 0))
        .collect();
    let tcs: Arc<Vec<Shared>> = Arc::new(cols.iter().map(|s| s.tc.clone()).collect());
    let wakers: Arc<Vec<Vec<Arc<WakerRef>>>> =
        Arc::new(cols.iter().map(|s| s.wakers.clone()).collect());
    let keys: Arc<Vec<Vec<Key>>> = Arc::new(cols.iter().map(|s| s.keys.clone()).collect());
    let in_flight: Arc<Vec<Vec<AtomicUsize>>> = Arc::new(
        (0..VICTIMS)
            .map(|_| (0..TASKS_EACH).map(|_| AtomicUsize::new(0)).collect())
            .collect(),
    );
    let total_handouts = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    // See the note in the single-collection stress above: an owner cannot call
    // its collection empty while a thief may still be holding one of its
    // tasks.
    let thieves_left = Arc::new(AtomicUsize::new(THIEVES));

    let drain_one = |v: usize,
                     got: Option<(Key, Arc<Task>, Arc<WakerRef>)>,
                     keys: &Vec<Vec<Key>>,
                     in_flight: &Vec<Vec<AtomicUsize>>,
                     total: &AtomicUsize|
     -> bool {
        match got {
            Some((key, _task, waker)) => {
                let idx = keys[v].iter().position(|&k| k == key).unwrap();
                assert_eq!(
                    in_flight[v][idx].fetch_add(1, Ordering::SeqCst),
                    0,
                    "victim {} task {} handed out twice at once",
                    v,
                    idx
                );
                total.fetch_add(1, Ordering::SeqCst);
                for _ in 0..20 {
                    core::hint::spin_loop();
                }
                in_flight[v][idx].fetch_sub(1, Ordering::SeqCst);
                waker.mark_borrowed(false);
                true
            }
            None => false,
        }
    };

    let owners: Vec<_> = (0..VICTIMS)
        .map(|v| {
            let tcs = tcs.clone();
            let keys = keys.clone();
            let in_flight = in_flight.clone();
            let total = total_handouts.clone();
            let done = done.clone();
            let thieves_left = thieves_left.clone();
            thread::spawn(move || loop {
                if drain_one(v, tcs[v].take_task(), &keys, &in_flight, &total) {
                    continue;
                }
                if !done.load(Ordering::SeqCst) || thieves_left.load(Ordering::SeqCst) != 0 {
                    thread::yield_now();
                    continue;
                }
                if !drain_one(v, tcs[v].take_task(), &keys, &in_flight, &total) {
                    break;
                }
            })
        })
        .collect();

    let thieves: Vec<_> = (0..THIEVES)
        .map(|t| {
            let tcs = tcs.clone();
            let keys = keys.clone();
            let in_flight = in_flight.clone();
            let total = total_handouts.clone();
            let done = done.clone();
            let thieves_left = thieves_left.clone();
            thread::spawn(move || {
                let mut rng = Rng::new(0xBEEF + t as u64);
                let mut steals = 0usize;
                let mut probes = 0usize;
                loop {
                    let v = rng.below(VICTIMS);
                    probes += 1;
                    if drain_one(v, tcs[v].try_take_task(), &keys, &in_flight, &total) {
                        steals += 1;
                    } else if done.load(Ordering::SeqCst) {
                        thieves_left.fetch_sub(1, Ordering::SeqCst);
                        break;
                    }
                    // No yield while the producers are running: a thief that
                    // steps aside here loses every race to the four owners and
                    // the steal path goes untested.
                }
                (steals, probes)
            })
        })
        .collect();

    let producers: Vec<_> = (0..VICTIMS)
        .map(|v| {
            let wakers = wakers.clone();
            thread::spawn(move || {
                let mut rng = Rng::new(0xA11CE + v as u64);
                for _ in 0..WAKES_EACH {
                    let victim = rng.below(VICTIMS);
                    wakers[victim][rng.below(TASKS_EACH)].wake_by_ref();
                }
            })
        })
        .collect();

    for p in producers {
        p.join().unwrap();
    }
    done.store(true, Ordering::SeqCst);
    let mut steals = 0;
    let mut probes = 0;
    for t in thieves {
        let (s, p) = t.join().unwrap();
        steals += s;
        probes += p;
    }
    for o in owners {
        o.join().unwrap();
    }
    // The thieves ran. Whether any of them *won* against four owners draining
    // their own queues is a race and not an invariant, so it is not asserted
    // here; the deterministic steal below is what covers the path when they
    // all lose.
    assert!(probes > 0, "no thief ever probed a victim");
    // Quiescent, so this part is not a race: a woken task is handed to a thief
    // on another thread by `try_take_task`, which is the path the concurrent
    // phase above exercises when it wins. Without it a run in which every
    // thief lost every probe would test nothing about stealing at all.
    for v in 0..VICTIMS {
        wakers[v][0].wake_by_ref();
        let (tcs, keys) = (tcs.clone(), keys.clone());
        let stolen = thread::spawn(move || {
            tcs[v]
                .try_take_task()
                .map(|(key, _task, waker)| {
                    waker.mark_borrowed(false);
                    keys[v].iter().position(|&k| k == key).unwrap()
                })
                .ok_or(v)
        })
        .join()
        .unwrap();
        assert_eq!(
            stolen,
            Ok(0),
            "a thief was refused victim {}'s one woken task on a quiet queue",
            v
        );
    }
    let _ = steals;
    for v in 0..VICTIMS {
        // One last owner pass per collection: everything was handed back.
        assert!(
            tcs[v].take_task().is_none(),
            "victim {} still had work after quiescence",
            v
        );
        assert_eq!(
            tcs[v].debug_pending(),
            (TASKS_EACH, 0, 0, 0),
            "victim {} left dirty",
            v
        );
    }
}

// ── ResumeClaim ─────────────────────────────────────────────────────────────

/// Many CPUs racing to resume the same parked frame. At most one may stand
/// on it at any instant; a refused claimant must learn who holds it.
#[test]
fn stress_resume_claim_has_at_most_one_holder() {
    const CPUS: usize = 16;
    const ROUNDS: usize = 20_000;
    let claim = Arc::new(ResumeClaim::new());
    let standing = Arc::new(AtomicUsize::new(usize::MAX));
    let wins = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..CPUS)
        .map(|cpu| {
            let claim = claim.clone();
            let standing = standing.clone();
            let wins = wins.clone();
            thread::spawn(move || {
                for _ in 0..ROUNDS {
                    match claim.try_claim(cpu) {
                        Ok(()) => {
                            let prev = standing.swap(cpu, Ordering::SeqCst);
                            assert_eq!(
                                prev,
                                usize::MAX,
                                "cpu {} resumed a frame cpu {} stands on",
                                cpu,
                                prev
                            );
                            assert_eq!(claim.holder(), Some(cpu));
                            for _ in 0..10 {
                                core::hint::spin_loop();
                            }
                            standing.store(usize::MAX, Ordering::SeqCst);
                            claim
                                .release_by(cpu)
                                .expect("released a claim we did not hold");
                            wins.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(holder) => {
                            assert!(holder < CPUS, "holder {} is not a CPU", holder);
                            assert_ne!(
                                holder, cpu,
                                "cpu {} was refused by itself without holding",
                                cpu
                            );
                        }
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert!(claim.holder().is_none(), "a claim was left standing");
    assert!(wins.load(Ordering::SeqCst) > 0);
}

// ── Reschedule requests ─────────────────────────────────────────────────────

/// Eight threads flood one CPU with reschedule requests. The 0→1 transition
/// is what sends the IPI, so a burst with nobody consuming costs exactly one.
#[test]
fn stress_resched_burst_for_one_cpu_costs_one_ipi() {
    let _g = resched_test_lock();
    IPIS.store(0, Ordering::SeqCst);
    set_resched_ipi_sender(count_ipi);
    crate::runtime::set_wakeup_preempt(true);
    crate::runtime::set_cpu_sleeping(5, false);
    // Start clean: consume anything pending for CPU 5 by recording a request
    // and clearing it through the executor-side path.
    crate::runtime::clear_need_resched(5);
    IPIS.store(0, Ordering::SeqCst);

    const THREADS: usize = 8;
    const EACH: usize = 10_000;
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            thread::spawn(move || {
                for _ in 0..EACH {
                    request_resched(5);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        IPIS.load(Ordering::SeqCst),
        1,
        "a burst with one outstanding request sent more than one IPI"
    );
    crate::runtime::clear_need_resched(5);
}

// ── Placement / kick pickers ────────────────────────────────────────────────

/// Random masks, ready sets, sleeping sets and loads: every picker answers
/// inside its inputs and respects its stated preferences.
#[test]
fn stress_pickers_never_name_a_cpu_outside_their_inputs() {
    let mut rng = Rng::new(0x5EED);
    for _ in 0..200_000 {
        let mask = rng.next();
        let ready = rng.next() & rng.next(); // sparser
        let sleeping = rng.next() & ready;
        let skip = rng.below(MAX_CORE_NUM + 2);
        let mut loads = [0usize; MAX_CORE_NUM];
        for l in loads.iter_mut() {
            *l = rng.below(8);
        }
        let skip_bit = if skip < 64 { 1u64 << skip } else { 0 };
        let candidates = mask & ready & !skip_bit;

        let pick = pick_affinity_kick_target_with_loads(mask, skip, ready, sleeping, Some(&loads));
        match pick {
            None => assert_eq!(
                candidates, 0,
                "nobody kicked with candidates {:#x}",
                candidates
            ),
            Some(t) => {
                let t = t as usize;
                assert!(
                    candidates & (1u64 << t) != 0,
                    "kicked {} outside {:#x}",
                    t,
                    candidates
                );
                if sleeping & candidates != 0 {
                    assert!(
                        sleeping & (1u64 << t) != 0,
                        "a busy CPU beat a sleeping one"
                    );
                    let best = (0..64)
                        .filter(|c| sleeping & candidates & (1u64 << c) != 0)
                        .map(|c| loads[c])
                        .min()
                        .unwrap();
                    assert_eq!(loads[t], best, "not the least-loaded sleeping candidate");
                } else {
                    let best = (0..64)
                        .filter(|c| candidates & (1u64 << c) != 0)
                        .map(|c| loads[c])
                        .min()
                        .unwrap();
                    assert_eq!(loads[t], best, "not the least-loaded candidate");
                }
            }
        }

        match affinity_home(mask, ready) {
            None => assert_eq!(mask, 0),
            Some(h) => {
                assert!(
                    mask & (1u64 << h) != 0,
                    "home {} outside mask {:#x}",
                    h,
                    mask
                );
                if mask & ready != 0 {
                    assert!(
                        ready & (1u64 << h) != 0,
                        "an unready home with ready CPUs allowed"
                    );
                }
            }
        }

        let chosen = rng.below(MAX_CORE_NUM + 1);
        let parked = park_cpu_if_cold(chosen, ready);
        if chosen < 64 && ready & (1u64 << chosen) != 0 {
            assert_eq!(parked, chosen);
        } else if ready != 0 {
            assert!(
                ready & (1u64 << parked) != 0,
                "parked on a cold CPU with a warm one available"
            );
        } else {
            assert_eq!(parked, chosen);
        }

        let local = rng.below(4);
        let richest = rng.below(8);
        let pull = should_pull_for_balance(local, richest);
        if pull {
            assert_eq!(local, 1);
            assert!(richest >= 3);
        }
        if local == 0 {
            assert!(!pull, "an idle CPU took the rebalance path");
        }
    }
}
