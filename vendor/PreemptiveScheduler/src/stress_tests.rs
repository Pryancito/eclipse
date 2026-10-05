<<<<<<< HEAD
//! Concurrency stress across the shared scheduler state.
//!
//! Every other test module in this crate drives one decision at a time from
//! one thread. That is the right shape for the pure choices — `affinity_home`,
//! `classify_parked_frame`, the key packing — but it is the wrong shape for the
//! half of this scheduler whose bugs are *all* interleavings:
//!
//! * a wake discarded because it raced the poll it was meant to interrupt,
//!   leaving the task asleep forever (`take_notified`'s deferral);
//! * one task handed to two executors at once, the second spinning on the
//!   future lock while the first sits parked — the 8-second DEADLOCK banner;
//! * two CPUs standing on one coroutine stack, which is the `[null-exec]`
//!   `ret`-to-zero;
//! * a burst of wakes for one CPU costing one IPI per wake instead of one in
//!   total.
//!
//! So these run host threads against the real structures and assert the
//! invariant rather than a count: *a slot is out at most once at a time*, *a
//! wake that was published is eventually handed to somebody*, *the latch has
//! at most one holder*. An invariant holds for every schedule, so a test of
//! one cannot pass by accident of timing — which is the whole difficulty with
//! a stress test, and the reason none of them assert on how far a thread got.
//!
//! The randomness is a seeded xorshift, never the system clock: a failure here
//! is reproducible by rerunning the same binary.
//!
//! These share this crate's globals (the reschedule masks, the recorded IPI
//! sender), so each one takes [`crate::runtime::resched_test_lock`] — the same
//! lock `resched_tests` and the `waker_page` wake tests take, for the reason
//! given there. CI runs `--test-threads=1` and would not notice; anyone
//! running `cargo test` locally would, intermittently.

use crate::executor::ResumeClaim;
use crate::runtime::{
    pick_affinity_kick_target_by, request_resched, resched_test_lock, set_cpu_sleeping,
    set_resched_ipi_sender, take_need_resched,
};
use crate::task_collection::{unpack_key, Key, TaskCollection};
use crate::waker_page::{WakerPage, WakerRef};

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc as StdArc, Barrier, Mutex};
use std::thread;

/// How many rounds each worker performs. Small enough that the whole module
/// stays well under a second, large enough that the interleavings happen:
/// every one of these loops contends on a `spin::Mutex` or a single cacheline,
/// so the threads interleave thousands of times per round.
const ROUNDS: usize = 2_000;

/// Seeded xorshift64. Deterministic on purpose — see the module docs.
=======
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
>>>>>>> ea4c3e529 (nuevo archivo.)
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
<<<<<<< HEAD
        // Any non-zero state will do; xorshift is dead at zero.
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// `TaskCollection` is deliberately neither `Send` nor `Sync`: its generator is
/// a bare `dyn Coroutine` and the kernel reaches it through
/// `ExecutorRuntime`'s own `unsafe impl`s, one runtime per CPU. The thieves
/// here are exactly what those `unsafe impl`s exist for — `try_take_task` is
/// called on a *peer's* collection on every idle pass — so the tests carry the
/// same promise the scheduler does, in one place.
struct Shared<T>(T);

// SAFETY: the same contract `ExecutorRuntime` states. Everything these threads
// touch through the wrapper goes through the collection's own `spin::Mutex`es
// (the generator lock, the per-priority queue locks) or through atomics on the
// waker pages.
unsafe impl<T> Send for Shared<T> {}
unsafe impl<T> Sync for Shared<T> {}

impl<T> core::ops::Deref for Shared<T> {
    type Target = T;
    fn deref(&self) -> &T {
=======
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
>>>>>>> ea4c3e529 (nuevo archivo.)
        &self.0
    }
}

<<<<<<< HEAD
/// Per-slot "this task is checked out right now" flags, used to assert the one
/// invariant every one of these tests shares: a task is polled by at most one
/// executor at a time.
struct OutFlags {
    out: Vec<AtomicBool>,
    /// Slots that were handed out while already out. Must stay zero.
    doubled: AtomicUsize,
    /// Slots handed out at least once, so "no wake was lost" can be checked.
    seen: Vec<AtomicBool>,
}

impl OutFlags {
    fn new(n: usize) -> Self {
        Self {
            out: (0..n).map(|_| AtomicBool::new(false)).collect(),
            doubled: AtomicUsize::new(0),
            seen: (0..n).map(|_| AtomicBool::new(false)).collect(),
        }
    }

    fn check_out(&self, idx: usize) {
        if self.out[idx].swap(true, Ordering::SeqCst) {
            self.doubled.fetch_add(1, Ordering::SeqCst);
        }
        self.seen[idx].store(true, Ordering::SeqCst);
    }

    fn check_in(&self, idx: usize) {
        self.out[idx].store(false, Ordering::SeqCst);
    }
}

// ── the waker page ───────────────────────────────────────────────────────────

/// A lane swap hands a bit to exactly one taker, and a wake published *during*
/// a poll is handed out again once the borrow is released.
///
/// The takers serialize, because that is the contract: `take_notified` is only
/// ever called from the generator, which holds the collection lock across the
/// swap *and* the `mark_borrowed` that follows it. The wakers do not
/// synchronize with anything, because real wakers do not: they are IRQ
/// handlers and peer CPUs.
///
/// Two invariants, and the second is the one with the history. `take_notified`
/// empties the lane with one swap; a wake that lands while the slot is checked
/// out to an executor is therefore in that swap's `raw` and in nobody's hands,
/// and if it is not re-published it is gone for good — the task sleeps forever.
/// So every taker publishes a wake for the slot it is holding, from inside the
/// window where it holds it (which is what an IRQ-driven future does on every
/// poll), counts it, and the final drain has to produce every slot whose count
/// says a wake is still owed.
#[test]
fn no_wake_is_lost_when_wakers_race_the_poll_they_interrupt() {
    let _g = resched_test_lock();
    const SLOTS: usize = 64;
    let page = Shared(WakerPage::new(0));
    for idx in 0..SLOTS {
        page.initialize(idx);
    }
    let page = StdArc::new(page);
    // Stands in for the collection lock the generator holds.
    let queue_lock = StdArc::new(Mutex::new(()));
    let flags = StdArc::new(OutFlags::new(SLOTS));
    // Per slot: wakes published and not yet acknowledged by a hand-out. A
    // hand-out acknowledges everything published before it, so a count left
    // standing at the end names a wake that reached the page and was dropped.
    let owed = StdArc::new((0..SLOTS).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
    let wakers_done = StdArc::new(AtomicBool::new(false));
    let barrier = StdArc::new(Barrier::new(6));
    let mut handles = Vec::new();

    for t in 0..2 {
        let (page, queue_lock, flags, owed) = (
            page.clone(),
            queue_lock.clone(),
            flags.clone(),
            owed.clone(),
        );
        let (wakers_done, barrier) = (wakers_done.clone(), barrier.clone());
        handles.push(thread::spawn(move || {
            let mut rng = Rng::new(0x5eed_0000 + t as u64);
            barrier.wait();
            // Keep draining past the last waker, so a wake that was dropped in
            // the thick of it cannot be covered up by a later one.
            let mut tail = ROUNDS / 4;
            loop {
                if wakers_done.load(Ordering::SeqCst) {
                    if tail == 0 {
                        break;
                    }
                    tail -= 1;
                }
                let taken = {
                    let _held = queue_lock.lock().unwrap();
                    let taken = page.take_notified();
                    let mut bits = taken;
                    while bits != 0 {
                        let idx = bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        flags.check_out(idx);
                        // This hand-out answers every wake published so far.
                        owed[idx].store(0, Ordering::SeqCst);
                        page.mark_borrowed(idx, true);
                    }
                    taken
                };
                // The poll itself, outside the lock, exactly as the executor
                // runs it — and the window every wake below lands in.
                let mut bits = taken;
                while bits != 0 {
                    let idx = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    if rng.next() & 1 == 0 {
                        // An IRQ-driven future woken by its own completion
                        // while this poll is still in flight. Deferred, not
                        // discarded: it has to come back after the release.
                        owed[idx].fetch_add(1, Ordering::SeqCst);
                        page.notify(idx);
                        // Stay in the poll a moment with the wake already
                        // published. This is the window the deferral exists
                        // for: the peer taker's next pass swaps the lane while
                        // this slot is still borrowed, and whether that wake
                        // survives is decided there, not here.
                        for _ in 0..256 {
                            core::hint::spin_loop();
                        }
                    }
                    flags.check_in(idx);
                    page.mark_borrowed(idx, false);
                }
            }
        }));
    }

    for t in 0..3 {
        let (page, owed) = (page.clone(), owed.clone());
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let mut rng = Rng::new(0xabcd_0000 + t as u64);
            barrier.wait();
            for _ in 0..ROUNDS {
                let idx = rng.below(SLOTS);
                owed[idx].fetch_add(1, Ordering::SeqCst);
                page.notify(idx);
            }
        }));
    }

    barrier.wait();
    // The three wakers are the last three handles; join them first so the
    // takers get their quiet tail.
    let takers: Vec<_> = handles.drain(..2).collect();
    for h in handles {
        h.join().unwrap();
    }
    wakers_done.store(true, Ordering::SeqCst);
    for h in takers {
        h.join().unwrap();
    }

    assert_eq!(
        flags.doubled.load(Ordering::SeqCst),
        0,
        "a slot was handed out while it was already checked out"
    );
    // Final drain with nothing borrowed. Every slot still owed a wake has to
    // come out of it; a slot that does not is a task that would never be
    // polled again.
    let mut drained = 0u64;
    loop {
        let taken = page.take_notified();
        if taken == 0 {
            break;
        }
        drained |= taken;
        let mut bits = taken;
        while bits != 0 {
            let idx = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            owed[idx].store(0, Ordering::SeqCst);
        }
    }
    for idx in 0..SLOTS {
        assert_eq!(
            owed[idx].load(Ordering::SeqCst),
            0,
            "slot {} was woken and the wake was dropped (drained mask {:#x})",
            idx,
            drained
        );
    }
    assert_eq!(
        page.peek().0,
        0,
        "the drain left published wakes behind, so the lane does not empty"
    );
}

/// The same page, reached the way the kernel reaches it: through the one
/// `Arc<WakerRef>` a task owns, from every thread at once.
///
/// `wake_by_ref` is more than a `fetch_or`: it samples `borrowed` *before*
/// publishing, decides between the urgent and the voluntary lane by the
/// waker's own address, and then kicks the owning CPU. All of it runs
/// concurrently on every core in the machine, for one task, whenever a futex
/// with N waiters is released.
#[test]
fn a_waker_shared_by_every_cpu_publishes_each_wake_exactly_into_a_lane() {
    let _g = resched_test_lock();
    set_cpu_sleeping(0, false);
    let page = WakerPage::new(0);
    let finished = Arc::new(AtomicBool::new(false));
    let waker = StdArc::new(Shared(Arc::new(page.make_waker(7, &finished))));
    let barrier = StdArc::new(Barrier::new(5));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let (waker, barrier) = (waker.clone(), barrier.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..ROUNDS {
                waker.wake_by_ref();
            }
        }));
    }
    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    // Nothing was borrowed and nothing was dropped, so every one of those
    // wakes coalesces onto the one urgent bit — and it has to be there.
    assert_eq!(page.peek(), (1 << 7, 0, 0));
    assert_eq!(page.take_notified(), 1 << 7);

    // And once the task has completed, a late waker still holding a clone
    // publishes nothing: that is what keeps a finished future from being
    // handed out and re-polled over its own torn-down captures.
    let w: &WakerRef = &waker.0;
    w.drop_by_ref();
    let barrier = StdArc::new(Barrier::new(5));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let (waker, barrier) = (waker.clone(), barrier.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..ROUNDS {
                waker.wake_by_ref();
            }
        }));
    }
    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        page.take_notified(),
        0,
        "a completed task was republished as runnable"
    );
}

// ── the task collection ──────────────────────────────────────────────────────

/// One owner, two thieves and a reaper on one collection.
///
/// This is the shape the >8s DEADLOCK banner came out of: the owner's
/// `take_task` blocks on the generator lock, the thieves' `try_take_task` does
/// not, and the reaper retires tasks underneath both — so a key the generator
/// published from the page bitmap can be dead by the time its holder looks it
/// up in the slab. The invariants are that no task is ever checked out twice
/// at once, and that `task_num` ends up agreeing with the retirements.
#[test]
fn a_task_is_never_checked_out_to_two_executors_at_once() {
    let _g = resched_test_lock();
    const TASKS: usize = 96; // two waker pages' worth, plus change
    let tc = StdArc::new(Shared(TaskCollection::new(0)));
    let mut keys = Vec::new();
    for _ in 0..TASKS {
        keys.push(tc.add_task(core::future::pending::<()>(), None));
    }
    assert_eq!(tc.task_num(), TASKS);
    // Index the flags by slab key, which is what `unpack_key` hands back.
    let flags = StdArc::new(OutFlags::new(TASKS * 2));
    let retired = StdArc::new(AtomicUsize::new(0));
    let barrier = StdArc::new(Barrier::new(4));
    let mut handles = Vec::new();

    for t in 0..3 {
        let (tc, flags, retired) = (tc.clone(), flags.clone(), retired.clone());
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let mut rng = Rng::new(0x7a5c_0000 + t as u64);
            barrier.wait();
            for _ in 0..ROUNDS {
                // Thread 0 is the owner (blocking take, as its own executor
                // does); the others are peers stealing with `try_take_task`.
                let got = if t == 0 {
                    tc.take_task()
                } else {
                    tc.try_take_task()
                };
                let Some((key, _task, waker)) = got else {
                    continue;
                };
                let (_, page_idx, subpage_idx) = unpack_key(key);
                let slot = page_idx * 64 + subpage_idx;
                flags.check_out(slot);
                if rng.next() & 3 == 0 {
                    core::hint::spin_loop();
                }
                flags.check_in(slot);
                // One poll in sixteen completes. On `Ready` the borrow bit is
                // deliberately left SET and `dropped` published first; the
                // generator's own dropped branch is what frees the slot.
                if rng.next() & 15 == 0 {
                    retired.fetch_add(1, Ordering::SeqCst);
                    waker.drop_by_ref();
                } else {
                    waker.mark_borrowed(false);
                    // Pending with no new wake: the task waits for one. Put it
                    // back so the queue keeps producing work for this test.
                    let (_, p, s) = unpack_key(key);
                    tc.get_mut_inner(crate::task_collection::DEFAULT_PRIORITY)
                        .pages[p]
                        .notify(s);
                }
            }
        }));
    }

    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        flags.doubled.load(Ordering::SeqCst),
        0,
        "a task was handed to two executors at once"
    );

    // Let the generator finish retiring whatever is still marked dropped, the
    // way the owning CPU's next pass does.
    while let Some((_key, _task, waker)) = tc.take_task() {
        waker.mark_borrowed(false);
    }
    let gone = retired.load(Ordering::SeqCst);
    assert!(gone > 0, "no poll ever completed, so nothing was retired");
    assert_eq!(
        tc.task_num(),
        TASKS - gone,
        "task_num does not follow the retirements ({} retired of {})",
        gone,
        TASKS
    );
}

/// A task pinned away from a CPU is never handed to that CPU, however many
/// thieves are asking.
///
/// Affinity is enforced inside the generator, which runs under the collection
/// lock on whichever CPU resumed it — so the check and the hand-out are one
/// step. On the host every thread reports `cpu_id() == 0`, so a mask that
/// excludes CPU 0 must make the queue refuse every single caller.
#[test]
fn a_pinned_task_is_refused_to_every_cpu_its_mask_forbids() {
    let _g = resched_test_lock();
    let tc = StdArc::new(Shared(TaskCollection::new(0)));
    // Allowed on CPU 1 only, and every thread here is CPU 0.
    for _ in 0..16 {
        tc.add_task(
            core::future::pending::<()>(),
            Some(Arc::new(AtomicU64::new(1 << 1))),
        );
    }
    let handed_out = StdArc::new(AtomicUsize::new(0));
    let barrier = StdArc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for t in 0..3 {
        let (tc, handed_out, barrier) = (tc.clone(), handed_out.clone(), barrier.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..ROUNDS {
                let got = if t == 0 {
                    tc.take_task()
                } else {
                    tc.try_take_task()
                };
                if let Some((_key, _task, waker)) = got {
                    handed_out.fetch_add(1, Ordering::SeqCst);
                    waker.mark_borrowed(false);
                }
            }
        }));
    }
    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        handed_out.load(Ordering::SeqCst),
        0,
        "a task pinned to CPU 1 was polled on CPU 0"
    );
    // And the figure the steal scan ranks victims by agrees: there is work
    // here, but none of it for us.
    assert!(tc.ready_num().unwrap_or(0) > 0);
    assert_eq!(tc.ready_num_for(0), Some(0));
    assert_eq!(tc.ready_num_for(1), tc.ready_num());
}

/// Four collections, four thieves: every task is polled, and never twice at
/// once.
///
/// The idle steal scan probes a peer's collection with `try_take_task` while
/// that peer's own executor may be inside `take_task` on it. Each thread here
/// owns one collection and steals from the other three, which is the real
/// four-core idle pattern.
#[test]
fn stealing_across_four_collections_hands_each_task_out_once() {
    let _g = resched_test_lock();
    const CPUS: usize = 4;
    const PER_CPU: usize = 16;
    let mut built = Vec::new();
    for cpu in 0..CPUS {
        let tc = TaskCollection::new(cpu as u8);
        for _ in 0..PER_CPU {
            tc.add_task(core::future::pending::<()>(), None);
        }
        built.push(tc);
    }
    let collections = StdArc::new(Shared(built));
    let flags = StdArc::new(
        (0..CPUS)
            .map(|_| OutFlags::new(PER_CPU * 2))
            .collect::<Vec<_>>(),
    );
    let barrier = StdArc::new(Barrier::new(CPUS + 1));
    let mut handles = Vec::new();
    for me in 0..CPUS {
        let (collections, flags, barrier) = (collections.clone(), flags.clone(), barrier.clone());
        handles.push(thread::spawn(move || {
            let mut rng = Rng::new(0xc0ffee + me as u64);
            barrier.wait();
            for _ in 0..ROUNDS {
                // Own queue first, then a peer's — the order `Executor::run`
                // uses.
                let victim = if rng.next() & 1 == 0 {
                    me
                } else {
                    (me + 1 + rng.below(CPUS - 1)) % CPUS
                };
                let got = if victim == me {
                    collections[me].take_task()
                } else {
                    collections[victim].try_take_task()
                };
                let Some((key, _task, waker)) = got else {
                    continue;
                };
                let (_, page_idx, subpage_idx) = unpack_key(key);
                let slot = page_idx * 64 + subpage_idx;
                flags[victim].check_out(slot);
                core::hint::spin_loop();
                flags[victim].check_in(slot);
                waker.mark_borrowed(false);
                let (_, p, s) = unpack_key(key);
                collections[victim]
                    .get_mut_inner(crate::task_collection::DEFAULT_PRIORITY)
                    .pages[p]
                    .notify(s);
            }
        }));
    }
    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    for (cpu, f) in flags.iter().enumerate() {
        assert_eq!(
            f.doubled.load(Ordering::SeqCst),
            0,
            "CPU {}'s collection handed one task to two thieves at once",
            cpu
        );
        let polled = f.seen.iter().filter(|s| s.load(Ordering::SeqCst)).count();
        assert_eq!(
            polled, PER_CPU,
            "only {} of CPU {}'s {} tasks were ever polled",
            polled, cpu, PER_CPU
        );
    }
}

// ── the resume latch ─────────────────────────────────────────────────────────

/// Sixteen CPUs reaching for one executor's stack leave exactly one standing
/// on it.
///
/// A second consumer of a parked frame is the `[null-exec]` `ret` to zero, so
/// the latch is not an optimisation: it is the thing that has to hold under
/// every interleaving. `resume_claim_tests` pins the rules from one thread and
/// races two; this races sixteen, with the holder doing work in between so the
/// window the others are reaching into is real.
#[test]
fn only_one_cpu_ever_stands_on_one_executor_stack() {
    const CPUS: usize = 16;
    let claim = StdArc::new(ResumeClaim::new());
    let standing = StdArc::new(AtomicUsize::new(0));
    let worst = StdArc::new(AtomicUsize::new(0));
    let claims = StdArc::new(AtomicUsize::new(0));
    let barrier = StdArc::new(Barrier::new(CPUS + 1));
    let mut handles = Vec::new();
    for cpu in 0..CPUS {
        let (claim, standing, worst, claims) = (
            claim.clone(),
            standing.clone(),
            worst.clone(),
            claims.clone(),
        );
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..ROUNDS {
                match claim.try_claim(cpu) {
                    Ok(()) => {
                        claims.fetch_add(1, Ordering::SeqCst);
                        let n = standing.fetch_add(1, Ordering::SeqCst) + 1;
                        worst.fetch_max(n, Ordering::SeqCst);
                        assert_eq!(
                            claim.holder(),
                            Some(cpu),
                            "the latch does not name the CPU it just handed the stack to"
                        );
                        core::hint::spin_loop();
                        standing.fetch_sub(1, Ordering::SeqCst);
                        claim
                            .release_by(cpu)
                            .expect("the holder's own release was reported as a slip");
                    }
                    Err(holder) => {
                        assert!(holder < CPUS, "the latch named CPU {}", holder);
                    }
                }
            }
        }));
    }
    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        worst.load(Ordering::SeqCst),
        1,
        "{} CPUs stood on one coroutine stack at the same time",
        worst.load(Ordering::SeqCst)
    );
    assert!(
        claims.load(Ordering::SeqCst) > 0,
        "nobody ever got the stack"
    );
    assert_eq!(claim.holder(), None, "the latch was left claimed");
}

// ── the reschedule request ───────────────────────────────────────────────────

/// A burst of wakes from every CPU at once costs one IPI per target, not one
/// per wake.
///
/// This is the coalescing `request_resched` publishes on the 0→1 transition.
/// Nothing consumes the bit during the burst, so the count is not a matter of
/// timing: exactly one publisher per target sees `already_pending == false`,
/// and the rest take the relaxed pre-check and send nothing.
///
/// The bits are cleared first on purpose. A `NEED_RESCHED` bit left pending by
/// an earlier test coalesces this burst into *nothing* and the test reads a
/// clean zero as a pass.
#[test]
fn a_burst_of_wakes_from_every_cpu_costs_one_ipi_per_target() {
    let _g = resched_test_lock();
    static KICKS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    fn record(cpu: usize) {
        if cpu < 8 {
            KICKS[cpu].fetch_add(1, Ordering::SeqCst);
        }
    }
    for k in KICKS.iter() {
        k.store(0, Ordering::SeqCst);
    }
    // Clear every target's pending bit, and ours, through the public door.
    for cpu in 1..8usize {
        set_cpu_sleeping(cpu, false);
    }
    let _ = take_need_resched();
    for cpu in 1..8usize {
        // `take_need_resched` only reaches this CPU's own bit, so drain the
        // others by publishing and letting the executor-side clear run.
        crate::runtime::clear_need_resched(cpu);
    }
    set_resched_ipi_sender(record);

    const TARGETS: usize = 7; // CPUs 1..=7; CPU 0 is us and skips its own IPI
    let barrier = StdArc::new(Barrier::new(5));
    let mut handles = Vec::new();
    for t in 0..4 {
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let mut rng = Rng::new(0xfeed_0000 + t as u64);
            barrier.wait();
            for _ in 0..ROUNDS {
                request_resched((1 + rng.below(TARGETS)) as u8);
            }
        }));
    }
    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    for (cpu, kicks) in KICKS.iter().enumerate().take(TARGETS + 1).skip(1) {
        let n = kicks.load(Ordering::SeqCst);
        assert_eq!(
            n, 1,
            "CPU {} was kicked {} times for one outstanding request",
            cpu, n
        );
    }
    for cpu in 1..=TARGETS {
        crate::runtime::clear_need_resched(cpu);
    }
}

// ── the affinity picker ──────────────────────────────────────────────────────

/// The picker never names a CPU outside the inputs it was given.
///
/// Pure, so this is a fuzz rather than a race — but it is the fuzz the race
/// tests rely on: `kick_for_affinity` turns this answer straight into a
/// reschedule request, and a CPU that the mask forbids, or that never entered
/// its executor loop, is a wake sent to somebody who will never look.
#[test]
fn the_picker_never_names_a_cpu_outside_its_inputs() {
    const ITERS: usize = 200_000;
    let mut rng = Rng::new(0x1234_5678_9abc_def0);
    for _ in 0..ITERS {
        let mask = rng.next();
        let ready = rng.next();
        let sleeping = rng.next();
        let skip = (rng.next() % 70) as usize; // past 63 too, which must not shift
        let loads: Vec<usize> = (0..64).map(|_| rng.below(32)).collect();
        for with_loads in [false, true] {
            let got = if with_loads {
                pick_affinity_kick_target_by(
                    mask,
                    skip,
                    ready,
                    sleeping,
                    Some(|cpu: usize| loads[cpu]),
                )
            } else {
                pick_affinity_kick_target_by(
                    mask,
                    skip,
                    ready,
                    sleeping,
                    None::<fn(usize) -> usize>,
                )
            };
            let allowed = if skip < 64 {
                mask & ready & !(1u64 << skip)
            } else {
                mask & ready
            };
            match got {
                None => assert_eq!(
                    allowed, 0,
                    "nobody was kicked although {:#x} could have been",
                    allowed
                ),
                Some(cpu) => {
                    assert!(cpu < 64, "the picker named CPU {}", cpu);
                    assert!(
                        allowed & (1u64 << cpu) != 0,
                        "picked CPU {} which mask={:#x} ready={:#x} skip={} forbid",
                        cpu,
                        mask,
                        ready,
                        skip
                    );
                    // And a sleeping candidate always wins outright: it answers
                    // in IPI time where a busy one has to reach a trap first.
                    if allowed & sleeping != 0 {
                        assert!(
                            sleeping & (1u64 << cpu) != 0,
                            "a busy CPU was preferred over a halted one"
                        );
                    }
                    if with_loads {
                        // Least loaded within the winning group, lowest id on a
                        // tie — the rule `kick_for_affinity` depends on to stop
                        // piling every affinity kick onto one core.
                        let group = if allowed & sleeping != 0 {
                            allowed & sleeping
                        } else {
                            allowed
                        };
                        let mut bits = group;
                        while bits != 0 {
                            let other = bits.trailing_zeros() as usize;
                            bits &= bits - 1;
                            assert!(
                                loads[other] > loads[cpu as usize]
                                    || (loads[other] == loads[cpu as usize]
                                        && other >= cpu as usize),
                                "CPU {} (load {}) was picked over CPU {} (load {})",
                                cpu,
                                loads[cpu as usize],
                                other,
                                loads[other]
                            );
                        }
                    }
                }
            }
        }
    }
}

/// A key's three fields survive every value this module hands around, which is
/// what lets the tests above index their flags by `page * 64 + subpage`.
#[test]
fn the_slot_a_key_names_is_the_one_the_flags_are_indexed_by() {
    let mut rng = Rng::new(0xdead_beef);
    for _ in 0..10_000 {
        let page_idx = rng.below(1 << 16);
        let subpage_idx = rng.below(64);
        let key: Key = crate::task_collection::pack_key(
            crate::task_collection::DEFAULT_PRIORITY,
            page_idx,
            subpage_idx,
        );
        let (priority, p, s) = unpack_key(key);
        assert_eq!(priority, crate::task_collection::DEFAULT_PRIORITY);
        assert_eq!((p, s), (page_idx, subpage_idx));
        assert_eq!(
            crate::task_collection::unmask_priority(key),
            page_idx * 64 + subpage_idx
        );
    }
}
=======
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
    let done = Arc::new(AtomicBool::new(false));

    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let page = page.clone();
            let wakes = wakes.clone();
            thread::spawn(move || {
                let mut rng = Rng::new(0x9E37_79B9 + p as u64);
                for _ in 0..WAKES_EACH {
                    let slot = rng.below(64);
                    wakes[slot].fetch_add(1, Ordering::SeqCst);
                    page.notify(slot);
                }
            })
        })
        .collect();

    let consumer = {
        let page = page.clone();
        let takes = takes.clone();
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
                    "a borrowed slot was handed out: taken={:#x} borrowed={:#x}", taken, borrowed
                );
                for slot in 0..64 {
                    if taken & (1u64 << slot) != 0 {
                        takes[slot].fetch_add(1, Ordering::SeqCst);
                    }
                }
                if done.load(Ordering::SeqCst) {
                    // Producers finished. Release every borrow and drain
                    // whatever was deferred behind them.
                    for slot in 0..64 {
                        if borrowed & (1u64 << slot) != 0 {
                            page.mark_borrowed(slot, false);
                        }
                    }
                    let tail = page.take_notified();
                    for slot in 0..64 {
                        if tail & (1u64 << slot) != 0 {
                            takes[slot].fetch_add(1, Ordering::SeqCst);
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

    for slot in 0..64 {
        let w = wakes[slot].load(Ordering::SeqCst);
        let t = takes[slot].load(Ordering::SeqCst);
        if w > 0 {
            assert!(t >= 1, "slot {}: {} wakes, never handed out", slot, w);
        }
        assert!(t <= w, "slot {}: handed out {} times for {} wakes", slot, t, w);
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
    let taken = page.take_notified();
    for i in 0..64 {
        if woken[i].load(Ordering::SeqCst) > 0 {
            assert!(taken & (1u64 << i) != 0, "slot {} was woken and is not pending", i);
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

    let index_of = {
        let keys = keys.clone();
        move |key: Key| keys.iter().position(|&k| k == key).expect("unknown key handed out")
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
            let index_of = index_of.clone();
            thread::spawn(move || {
                let mut idle_passes = 0u32;
                loop {
                    let got = if d == 0 { tc.take_task() } else { tc.try_take_task() };
                    match got {
                        Some((key, task, waker)) => {
                            idle_passes = 0;
                            let idx = index_of(key);
                            assert!(
                                task.allowed_on(0),
                                "task {} pinned elsewhere was handed to CPU 0", idx
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
                            if done.load(Ordering::SeqCst) {
                                idle_passes += 1;
                                if idle_passes > 8 {
                                    break;
                                }
                            }
                            thread::yield_now();
                        }
                    }
                }
            })
        })
        .collect();

    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let wakers = wakers.clone();
            thread::spawn(move || {
                let mut rng = Rng::new(101 + p as u64);
                for _ in 0..WAKES_EACH {
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

    let pinned = (0..TASKS).filter(|i| i % PINNED_EVERY == 0).count();
    for idx in 0..TASKS {
        let h = handouts[idx].load(Ordering::SeqCst);
        if idx % PINNED_EVERY == 0 {
            assert_eq!(h, 0, "pinned task {} was polled on CPU 0", idx);
        } else if dropped_at[idx].load(Ordering::SeqCst) != usize::MAX {
            assert!(
                handouts_after_drop[idx].load(Ordering::SeqCst) <= 1,
                "retired task {} kept being handed out", idx
            );
        }
        assert_eq!(in_flight[idx].load(Ordering::SeqCst), 0);
    }
    assert_eq!(
        tc.task_num(),
        TASKS - dropped,
        "task_num did not follow {} retirements", dropped
    );
    // Pinned tasks are still waiting for CPU 1 — nothing else is pending.
    let (_, notified, _, borrowed) = tc.debug_pending();
    assert_eq!(borrowed, 0, "a borrow was left set");
    assert_eq!(notified as usize, pinned, "pending wakes other than the pinned tasks");
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

    let cols: Vec<Stressed> = (0..VICTIMS).map(|_| stressed_collection(TASKS_EACH, 0)).collect();
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
                    "victim {} task {} handed out twice at once", v, idx
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
            thread::spawn(move || {
                let mut idle = 0;
                loop {
                    if !drain_one(v, tcs[v].take_task(), &keys, &in_flight, &total) {
                        if done.load(Ordering::SeqCst) {
                            idle += 1;
                            if idle > 8 {
                                break;
                            }
                        }
                        thread::yield_now();
                    } else {
                        idle = 0;
                    }
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
            thread::spawn(move || {
                let mut rng = Rng::new(0xBEEF + t as u64);
                let mut steals = 0usize;
                loop {
                    let v = rng.below(VICTIMS);
                    if drain_one(v, tcs[v].try_take_task(), &keys, &in_flight, &total) {
                        steals += 1;
                    } else if done.load(Ordering::SeqCst) {
                        break;
                    }
                }
                steals
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
    for t in thieves {
        steals += t.join().unwrap();
    }
    for o in owners {
        o.join().unwrap();
    }
    assert!(steals > 0, "no thief ever got a task; the stress did not exercise steal");
    for v in 0..VICTIMS {
        // One last owner pass per collection: everything was handed back.
        assert!(tcs[v].take_task().is_none(), "victim {} still had work after quiescence", v);
        assert_eq!(tcs[v].debug_pending(), (TASKS_EACH, 0, 0, 0), "victim {} left dirty", v);
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
                            assert_eq!(prev, usize::MAX, "cpu {} resumed a frame cpu {} stands on", cpu, prev);
                            assert_eq!(claim.holder(), Some(cpu));
                            for _ in 0..10 {
                                core::hint::spin_loop();
                            }
                            standing.store(usize::MAX, Ordering::SeqCst);
                            claim.release_by(cpu).expect("released a claim we did not hold");
                            wins.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(holder) => {
                            assert!(holder < CPUS, "holder {} is not a CPU", holder);
                            assert_ne!(holder, cpu, "cpu {} was refused by itself without holding", cpu);
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
    assert_eq!(IPIS.load(Ordering::SeqCst), 1, "a burst with one outstanding request sent more than one IPI");
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
            None => assert_eq!(candidates, 0, "nobody kicked with candidates {:#x}", candidates),
            Some(t) => {
                let t = t as usize;
                assert!(candidates & (1u64 << t) != 0, "kicked {} outside {:#x}", t, candidates);
                if sleeping & candidates != 0 {
                    assert!(sleeping & (1u64 << t) != 0, "a busy CPU beat a sleeping one");
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
                assert!(mask & (1u64 << h) != 0, "home {} outside mask {:#x}", h, mask);
                if mask & ready != 0 {
                    assert!(ready & (1u64 << h) != 0, "an unready home with ready CPUs allowed");
                }
            }
        }

        let chosen = rng.below(MAX_CORE_NUM + 1);
        let parked = park_cpu_if_cold(chosen, ready);
        if chosen < 64 && ready & (1u64 << chosen) != 0 {
            assert_eq!(parked, chosen);
        } else if ready != 0 {
            assert!(ready & (1u64 << parked) != 0, "parked on a cold CPU with a warm one available");
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
>>>>>>> ea4c3e529 (nuevo archivo.)
