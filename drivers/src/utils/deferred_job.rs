//! Deferred job queue.
//!
//! Allows interrupt / IRQ handlers to schedule work that should run outside of
//! an atomic context (e.g. in the next scheduler tick or poll loop).
//!
//! Jobs are closures pushed onto a global queue via [`push_deferred_job`] and
//! drained with [`drain_deferred_jobs`].
//!
//! The queue is bounded, so it also *throws work away*, and both NIC drivers are
//! built around that: the guard that clears their scheduling flag is created
//! outside the closure and moved in, so an evicted job still clears
//! `watchdog_job_scheduled` / re-arms IMS when it is dropped. Every eviction is
//! counted in [`evicted_deferred_jobs`], because a bounded queue that discards
//! IRQ work with nothing to show for it is how `poll_pending` got stuck once
//! already -- see [`MAX_JOBS_PER_DRAIN`].

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicUsize, Ordering};
use lock::Mutex;

type Job = Box<dyn FnOnce() + Send + 'static>;

static JOBS: Mutex<VecDeque<Job>> = Mutex::new(VecDeque::new());

/// Jobs thrown away for want of queue space, since boot. See [`evict_tail_job`].
static EVICTED: AtomicUsize = AtomicUsize::new(0);

/// Cap queued IRQ work — unbounded growth looks like a kernel leak.
const MAX_DEFERRED_JOBS: usize = 256;
/// Run at most this many deferred jobs per drain.
///
/// Was 2: under HID/USB/watchdog pressure that budget silently aged out NIC
/// bottom-halves (e1000e `poll_pending` stuck → interrupt-deaf). 8 matches the
/// adaptive net drain floor in `linux-object` and still bounds long jobs so
/// PS/2/USB keep getting turns across drains.
const MAX_JOBS_PER_DRAIN: usize = 8;

/// Make room by throwing away the job at the **tail**, the sacrificial end.
///
/// The front is the protected end: [`push_deferred_job_front`] puts NIC
/// bottom-halves there precisely so a backlog cannot age them out, and because a
/// front push reverses arrival order, under an IRQ storm the tail holds the
/// *stalest* queued poll while the front holds the freshest. So both pushers
/// evict from the tail. [`push_deferred_job`] used to evict from the **front**,
/// which threw away the newest NIC bottom-half — the one with packets sitting in
/// the ring right now — and undid the only guarantee the front push offers.
///
/// The evicted job is **dropped**, and both NIC drivers depend on that: the
/// guard that clears `watchdog_job_scheduled` (e1000e's `Guard`) or re-arms IMS
/// (`PollPendingGuard`) is built outside the closure and moved in, so it runs
/// even when the body never does. The one exception is a job whose `dyn` fat
/// pointer is no longer live: `Drop` for a trait object dispatches through the
/// same vtable as a call, so a smashed one is leaked instead of dropped. That is
/// the rule [`drain_deferred_jobs_max`] already follows, and eviction is the
/// path that runs under exactly the memory pressure the gate exists for, so it
/// cannot be the one place that skips it.
fn evict_tail_job(q: &mut VecDeque<Job>) {
    if let Some(job) = q.pop_back() {
        EVICTED.fetch_add(1, Ordering::Relaxed);
        if !super::fat_ptr::dyn_fat_ptr_live(&job) {
            core::mem::forget(job);
        }
    }
}

/// Enqueue a closure to be executed later outside of IRQ context.
///
/// Do not wrap with manual `intr_on`/`intr_off` — `lock::Mutex` already uses
/// `push_off`/`pop_off`; re-enabling IRQs before the guard drops panics in `mycpu()`.
pub fn push_deferred_job<F: FnOnce() + Send + 'static>(f: F) {
    // Mutex::lock() uses push_off/pop_off which already handles interrupt
    // disabling. Manual intr_off/on here bypasses the noff accounting and
    // causes "RefCell already borrowed" panics under SMP.
    let mut q = JOBS.lock();
    if q.len() >= MAX_DEFERRED_JOBS {
        evict_tail_job(&mut q);
    }
    q.push_back(Box::new(f));
}

/// Enqueue at the front so the next drain runs this job first.
///
/// For NIC IRQ bottom-halves that must not sit behind a backlog of lower-urgency
/// work (and risk eviction from the 256-cap FIFO). When the queue is full the
/// tail goes, never the front: the front may itself be an urgent job that an
/// earlier IRQ put there. See [`evict_tail_job`].
pub fn push_deferred_job_front<F: FnOnce() + Send + 'static>(f: F) {
    let mut q = JOBS.lock();
    if q.len() >= MAX_DEFERRED_JOBS {
        evict_tail_job(&mut q);
    }
    q.push_front(Box::new(f));
}

/// Execute all currently queued deferred jobs.
///
/// Should be called from a non-atomic context (e.g. the kernel idle loop or a
/// timer tick handler).
pub fn drain_deferred_jobs() {
    drain_deferred_jobs_max(MAX_JOBS_PER_DRAIN);
}

/// Run at most `max` deferred jobs, leaving the rest queued for the next drain.
/// Use before NIC poll when stdin/HID must stay responsive.
///
/// The queue lock is **not** held while a job runs: a job is free to schedule
/// another one, and several of them do.
pub fn drain_deferred_jobs_max(max: usize) {
    let cap = max.max(1).min(MAX_DEFERRED_JOBS);
    for _ in 0..cap {
        let job = {
            let mut q = JOBS.lock();
            q.pop_front()
        };
        match job {
            Some(job) => {
                // Same fat-ptr gate as IRQ handlers: a smashed Box<dyn FnOnce>
                // left in the queue must not be called or Dropped.
                if !super::fat_ptr::dyn_fat_ptr_live(&job) {
                    core::mem::forget(job);
                    continue;
                }
                job();
            }
            None => break,
        }
    }
}

/// Number of queued jobs (best-effort snapshot).
pub fn pending_deferred_jobs() -> usize {
    JOBS.lock().len()
}

/// Number of jobs thrown away, unexecuted, because the queue was full.
///
/// [`pending_deferred_jobs`] reads the same on a queue that is merely busy and
/// on one that has been shedding IRQ work for hours, which is why this counter
/// exists: it is the difference between "the NIC is slow" and "the NIC lost a
/// bottom-half". Reported by `/proc/perf/kernel`; monotonic, never reset.
pub fn evicted_deferred_jobs() -> usize {
    EVICTED.load(Ordering::Relaxed)
}

/// This queue sits under every NIC bottom-half, the e1000e link watchdog and the
/// idle callback, and the only tests it ever had were in `tests/deferred_job.rs`
/// behind `required-features = ["mock"]` -- a feature that pulls in SDL2 and that
/// no `cargo test` line in this repository or in CI enables, so they had not been
/// compiled since the day they were written. They are below now, in the `--lib`
/// target that every job runs, and exact rather than `>=`: as an integration test
/// they had a private process, here they share a global queue with the e1000/
/// e1000e tests, so they go through [`tests::alone_with_the_queue`].
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicBool;

    /// Takes the queue's process-wide state for one test at a time and leaves it
    /// empty on the way in and out. Whatever another test left queued is
    /// **leaked**, not drained: running another module's bottom-half outside its
    /// own test would poke at its fake MMIO after it is done with it.
    fn alone_with_the_queue<R>(body: impl FnOnce() -> R) -> R {
        static TURNSTILE: crate::sync::Mutex<()> = crate::sync::Mutex::new(());
        let _guard = TURNSTILE.lock();
        empty_the_queue();
        EVICTED.store(0, Ordering::SeqCst);
        let out = body();
        empty_the_queue();
        EVICTED.store(0, Ordering::SeqCst);
        out
    }

    fn empty_the_queue() {
        let taken = core::mem::take(&mut *JOBS.lock());
        core::mem::forget(taken);
    }

    /// A job that records that it ran, and nothing else.
    fn marker(flag: &Arc<AtomicBool>) -> impl FnOnce() + Send + 'static {
        let flag = Arc::clone(flag);
        move || flag.store(true, Ordering::SeqCst)
    }

    /// A job that adds one to `counter` when it runs.
    fn counting(counter: &Arc<AtomicUsize>) -> impl FnOnce() + Send + 'static {
        let counter = Arc::clone(counter);
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn fill_with_back_pushes(n: usize) {
        for _ in 0..n {
            push_deferred_job(|| {});
        }
    }

    // ---------------------------------------------------------------- the basics

    #[test]
    fn a_pushed_job_is_queued_and_runs_on_the_next_drain() {
        alone_with_the_queue(|| {
            let counter = Arc::new(AtomicUsize::new(0));
            push_deferred_job(counting(&counter));
            assert_eq!(pending_deferred_jobs(), 1, "the push did not queue it");
            assert_eq!(counter.load(Ordering::SeqCst), 0, "it ran inside the push");
            drain_deferred_jobs();
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            assert_eq!(pending_deferred_jobs(), 0, "the drain left it queued");
        });
    }

    #[test]
    fn a_drain_runs_at_most_its_budget_and_leaves_the_rest_queued() {
        alone_with_the_queue(|| {
            let counter = Arc::new(AtomicUsize::new(0));
            for _ in 0..4 {
                push_deferred_job(counting(&counter));
            }
            drain_deferred_jobs_max(1);
            assert_eq!(counter.load(Ordering::SeqCst), 1, "exactly one, not zero");
            assert_eq!(pending_deferred_jobs(), 3, "the other three were lost");
            drain_deferred_jobs_max(8);
            assert_eq!(counter.load(Ordering::SeqCst), 4);
            assert_eq!(pending_deferred_jobs(), 0);
        });
    }

    #[test]
    fn the_default_drain_budget_is_the_net_drain_floor_and_not_the_old_two() {
        // This budget was 2, and at 2 a burst of HID/watchdog work aged out the
        // e1000e bottom-half until the NIC went interrupt-deaf. The number is
        // the fix, so it gets a test rather than only a comment.
        alone_with_the_queue(|| {
            let counter = Arc::new(AtomicUsize::new(0));
            for _ in 0..20 {
                push_deferred_job(counting(&counter));
            }
            drain_deferred_jobs();
            assert_eq!(counter.load(Ordering::SeqCst), MAX_JOBS_PER_DRAIN);
            assert_eq!(MAX_JOBS_PER_DRAIN, 8, "the adaptive floor in linux-object");
            assert_eq!(pending_deferred_jobs(), 20 - MAX_JOBS_PER_DRAIN);
        });
    }

    #[test]
    fn a_drain_budget_is_clamped_at_both_ends() {
        alone_with_the_queue(|| {
            let counter = Arc::new(AtomicUsize::new(0));
            push_deferred_job(counting(&counter));
            push_deferred_job(counting(&counter));
            // Zero would otherwise be a drain that cannot make progress, and the
            // adaptive budget in `linux-object` never asks for less than 4.
            drain_deferred_jobs_max(0);
            assert_eq!(counter.load(Ordering::SeqCst), 1, "zero ran nothing");
            // And an absurd budget must not overflow the loop bound.
            drain_deferred_jobs_max(usize::MAX);
            assert_eq!(counter.load(Ordering::SeqCst), 2);
        });
    }

    /// A job may schedule another, so the budget has to bound a drain even when
    /// the queue keeps refilling under it -- that is what the upper clamp is for,
    /// and without it one drain follows a self-requeuing chain for as long as the
    /// chain lasts. In the kernel that drain runs in the idle callback and in the
    /// net poll loop, so an unbounded one starves stdin and HID for its whole
    /// length. (A mutant dropping the clamp survived every other test here.)
    #[test]
    fn one_drain_runs_no_more_jobs_than_the_queue_can_hold_even_if_they_requeue() {
        fn requeue(ran: Arc<AtomicUsize>, left: usize) {
            if left == 0 {
                return;
            }
            push_deferred_job(move || {
                ran.fetch_add(1, Ordering::SeqCst);
                requeue(ran, left - 1);
            });
        }
        alone_with_the_queue(|| {
            let ran = Arc::new(AtomicUsize::new(0));
            requeue(Arc::clone(&ran), MAX_DEFERRED_JOBS + 44);
            drain_deferred_jobs_max(usize::MAX);
            assert_eq!(
                ran.load(Ordering::SeqCst),
                MAX_DEFERRED_JOBS,
                "one drain ran more jobs than the queue can even hold"
            );
            assert_eq!(pending_deferred_jobs(), 1, "the chain was not left going");
        });
    }

    #[test]
    fn a_drain_of_an_empty_queue_stops_instead_of_spinning_its_budget() {
        alone_with_the_queue(|| {
            let counter = Arc::new(AtomicUsize::new(0));
            push_deferred_job(counting(&counter));
            drain_deferred_jobs_max(MAX_DEFERRED_JOBS);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn the_queue_lock_is_not_held_while_a_job_runs() {
        // Several jobs schedule follow-up work from inside their own body (the
        // e1000e watchdog re-arms itself). Holding the lock across the call
        // would deadlock the kernel in the idle callback, and it deadlocks here.
        alone_with_the_queue(|| {
            let counter = Arc::new(AtomicUsize::new(0));
            let inner = Arc::clone(&counter);
            push_deferred_job(move || {
                push_deferred_job(counting(&inner));
                assert_eq!(pending_deferred_jobs(), 1, "the follow-up was lost");
            });
            drain_deferred_jobs();
            assert_eq!(counter.load(Ordering::SeqCst), 1, "the follow-up never ran");
        });
    }

    // ------------------------------------------------------------ front vs back

    #[test]
    fn a_front_pushed_job_runs_before_everything_already_queued() {
        alone_with_the_queue(|| {
            let order = Arc::new(AtomicUsize::new(0));
            let first = Arc::new(AtomicUsize::new(0));
            let second = Arc::new(AtomicUsize::new(0));

            let o = Arc::clone(&order);
            let s = Arc::clone(&second);
            push_deferred_job(move || {
                s.store(o.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            });
            let o = Arc::clone(&order);
            let f = Arc::clone(&first);
            push_deferred_job_front(move || {
                f.store(o.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            });

            drain_deferred_jobs_max(2);
            assert_eq!(first.load(Ordering::SeqCst), 1, "the urgent job ran second");
            assert_eq!(second.load(Ordering::SeqCst), 2);
        });
    }

    // ---------------------------------------------------------------- the cap

    #[test]
    fn the_queue_stops_growing_at_its_cap() {
        alone_with_the_queue(|| {
            fill_with_back_pushes(MAX_DEFERRED_JOBS + 44);
            assert_eq!(pending_deferred_jobs(), MAX_DEFERRED_JOBS);
            push_deferred_job_front(|| {});
            assert_eq!(pending_deferred_jobs(), MAX_DEFERRED_JOBS, "the front push");
        });
    }

    #[test]
    fn an_ordinary_push_no_longer_sacrifices_the_urgent_job_at_the_front() {
        // `push_deferred_job_front` exists so a NIC bottom-half cannot be aged
        // out of the FIFO, and its own doc says the front may hold an urgent job
        // already. A back push evicted exactly that entry, so the one ordinary
        // push in the tree -- the e1000e link watchdog -- threw away the newest
        // NIC poll, the one whose packets are in the ring.
        alone_with_the_queue(|| {
            let urgent = Arc::new(AtomicBool::new(false));
            push_deferred_job_front(marker(&urgent));
            fill_with_back_pushes(MAX_DEFERRED_JOBS - 1);
            assert_eq!(pending_deferred_jobs(), MAX_DEFERRED_JOBS, "not full yet");

            push_deferred_job(|| {});
            assert_eq!(evicted_deferred_jobs(), 1, "the cap evicted nothing");

            drain_deferred_jobs_max(1);
            assert!(
                urgent.load(Ordering::SeqCst),
                "the back push evicted the urgent front job"
            );
        });
    }

    #[test]
    fn a_front_push_at_the_cap_also_takes_the_tail_and_keeps_the_front() {
        alone_with_the_queue(|| {
            let urgent = Arc::new(AtomicBool::new(false));
            push_deferred_job_front(marker(&urgent));
            fill_with_back_pushes(MAX_DEFERRED_JOBS - 1);

            let newer = Arc::new(AtomicBool::new(false));
            push_deferred_job_front(marker(&newer));
            assert_eq!(evicted_deferred_jobs(), 1);

            // The newest urgent job first, the older one still queued behind it.
            drain_deferred_jobs_max(1);
            assert!(newer.load(Ordering::SeqCst), "the newest urgent job");
            assert!(!urgent.load(Ordering::SeqCst), "two ran on a budget of one");
            drain_deferred_jobs_max(1);
            assert!(
                urgent.load(Ordering::SeqCst),
                "the older urgent job was lost"
            );
        });
    }

    #[test]
    fn every_eviction_is_counted_so_a_shedding_queue_is_not_invisible() {
        // `pending_deferred_jobs` reads 256 whether the queue is merely busy or
        // has been discarding bottom-halves for hours, and that is the whole
        // reason this counter exists.
        alone_with_the_queue(|| {
            fill_with_back_pushes(MAX_DEFERRED_JOBS);
            assert_eq!(evicted_deferred_jobs(), 0, "the cap was not reached yet");
            for expected in 1..=7 {
                push_deferred_job(|| {});
                assert_eq!(evicted_deferred_jobs(), expected);
            }
            for expected in 8..=10 {
                push_deferred_job_front(|| {});
                assert_eq!(evicted_deferred_jobs(), expected, "the front push too");
            }
        });
    }

    #[test]
    fn an_evicted_job_is_still_dropped_so_the_nic_guards_run() {
        // Both NIC drivers build their state-clearing guard OUTSIDE the closure
        // and move it in, on the strength of this: an evicted e1000e watchdog
        // still clears `watchdog_job_scheduled`, an evicted poll still re-arms
        // IMS. Leaking an evicted job instead would leave link supervision dead
        // for the life of the kernel, which is what the comment at the guard
        // says used to happen.
        static DROPPED: AtomicUsize = AtomicUsize::new(0);
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                DROPPED.fetch_add(1, Ordering::SeqCst);
            }
        }
        alone_with_the_queue(|| {
            DROPPED.store(0, Ordering::SeqCst);
            fill_with_back_pushes(MAX_DEFERRED_JOBS - 1);
            let guard = Guard;
            push_deferred_job(move || {
                let _g = guard;
                unreachable!("an evicted job must not run");
            });
            assert_eq!(pending_deferred_jobs(), MAX_DEFERRED_JOBS);
            assert_eq!(DROPPED.load(Ordering::SeqCst), 0, "dropped before the cap");

            // The guarded job is the tail, so this push is the one that takes it.
            push_deferred_job(|| {});
            assert_eq!(evicted_deferred_jobs(), 1);
            assert_eq!(
                DROPPED.load(Ordering::SeqCst),
                1,
                "the evicted job was leaked, so its guard never cleared the flag"
            );
        });
    }

    // ------------------------------------------------- the fat-pointer gate

    /// A job whose `dyn` fat pointer cannot be live, as a use-after-free leaves
    /// it. The **vtable** word is the one corrupted, not the data word: `Box`'s
    /// only documented niche is the null data pointer, so leaving that half
    /// valid is the least this can disturb, and a misaligned vtable is a shape
    /// `dyn_fat_ptr_live` rejects outright (a vtable is an array of pointers).
    fn a_smashed_job() -> Job {
        let mut job: Job = Box::new(|| unreachable!("a dead job must never run"));
        // SAFETY: a `Box<dyn FnOnce()>` is exactly two words, `{ data, vtable }`,
        // and `job` is a live local, so the second word is in bounds and
        // naturally aligned. Nothing dereferences it afterwards: the only two
        // operations that would -- calling it and dropping it -- are exactly the
        // ones the gate is there to prevent, and this closure captures nothing,
        // so there is no allocation behind it to lose either.
        unsafe {
            let words = &mut job as *mut Job as *mut usize;
            let vtable = core::ptr::read_volatile(words.add(1));
            core::ptr::write_volatile(words.add(1), vtable | 1);
        }
        job
    }

    #[test]
    fn an_evicted_job_with_a_dead_fat_pointer_is_leaked_and_not_dropped() {
        // Dropping a trait object dispatches through the same vtable as calling
        // it, so `pop_back()` on a smashed job is the jump into garbage that
        // `fat_ptr` exists to prevent -- and eviction is the path that runs under
        // exactly the memory pressure that produces smashed jobs. Note the shape
        // of a regression here: without the gate this test **faults** instead of
        // failing, because that is the bug.
        let _gate = crate::utils::fat_ptr::GateForTest::new();
        alone_with_the_queue(|| {
            fill_with_back_pushes(MAX_DEFERRED_JOBS - 1);
            JOBS.lock().push_back(a_smashed_job());
            assert_eq!(pending_deferred_jobs(), MAX_DEFERRED_JOBS);

            push_deferred_job(|| {});
            assert_eq!(evicted_deferred_jobs(), 1, "the smashed job was not taken");
            assert_eq!(pending_deferred_jobs(), MAX_DEFERRED_JOBS);
            assert!(
                crate::utils::heap_smash_suspected(),
                "a dead pointer has to latch the tripwire"
            );
        });
    }

    #[test]
    fn a_drain_leaks_a_smashed_job_instead_of_calling_it() {
        // The gate in `drain_deferred_jobs_max` had no test either, and it is the
        // one every NIC bottom-half, timer callback and idle drain goes through.
        let _gate = crate::utils::fat_ptr::GateForTest::new();
        alone_with_the_queue(|| {
            let after = Arc::new(AtomicBool::new(false));
            JOBS.lock().push_back(a_smashed_job());
            push_deferred_job(marker(&after));

            drain_deferred_jobs();
            assert!(
                after.load(Ordering::SeqCst),
                "a smashed job stopped the drain instead of being skipped"
            );
            assert_eq!(pending_deferred_jobs(), 0, "it stayed in the queue");
            assert_eq!(evicted_deferred_jobs(), 0, "a skip is not an eviction");
        });
    }
}
