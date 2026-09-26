//! Process accounting and approximate, Linux-style system load averages.
//!
//! Linux keeps three exponentially-weighted moving averages of the
//! run-queue length, sampled every 5 seconds (the 1-, 5- and 15-minute
//! windows) from the scheduler tick — crucially, at instants *uncorrelated*
//! with whoever reads `/proc/loadavg`. [`sampler_task`] reproduces that: a
//! kernel task advances the averages every 5 s from the executor's global
//! run-queue length.
//!
//! The previous design advanced the averages lazily at read time using the
//! count of threads executing at that instant. That correlates the sample
//! with the reader's own wake-up: a status bar that polls `/proc/loadavg`
//! whenever the desktop redraws always finds exactly the compositor mid-poll
//! next to it, so an idle desktop reported a rock-steady load of 1.00. The
//! lazy path is kept only as a fallback for hosted (libos) builds, where no
//! sampler task is spawned and the executor run queue is not visible.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use kernel_hal::timer::timer_now;
use lazy_static::lazy_static;
use lock::Mutex;
use zircon_object::object::KernelObject;
use zircon_object::task::{running_thread_count, Job, Process, Status, ROOT_JOB};

/// Fixed-point shift used by the EWMA math (matches Linux `FSHIFT`).
const FSHIFT: u32 = 11;
/// 1.0 in the internal fixed-point format.
const FIXED_1: u64 = 1 << FSHIFT;
/// Shift used by `sysinfo(2)`'s `loads` field (`SI_LOAD_SHIFT`).
const SI_LOAD_SHIFT: u32 = 16;
/// Sampling period, in seconds (Linux samples every 5s).
const LOAD_FREQ_SECS: u64 = 5;
/// Decay factors for the 1/5/15-minute windows at a 5s sample period —
/// `exp(-5/60)`, `exp(-5/300)`, `exp(-5/900)` in `FSHIFT` fixed point.
const EXP_1: u64 = 1884;
const EXP_5: u64 = 2014;
const EXP_15: u64 = 2037;
/// Cap on catch-up iterations so a long gap between reads can't spin: 256
/// steps is 21 min of samples, well past the 15-minute window's settling.
const MAX_CATCHUP_STEPS: u32 = 256;

struct LoadAvg {
    /// Seconds-since-boot at which the averages were last advanced (0 = never).
    last_secs: u64,
    /// The three averages, in `FSHIFT` fixed point.
    loads: [u64; 3],
}

lazy_static! {
    static ref STATE: Mutex<LoadAvg> = Mutex::new(LoadAvg {
        last_secs: 0,
        loads: [0; 3],
    });
}

fn collect(job: &Arc<Job>, out: &mut Vec<Arc<Process>>) {
    for id in job.process_ids() {
        if let Some(proc) = job.find_process(id) {
            if !matches!(proc.status(), Status::Exited(_)) {
                out.push(proc);
            }
        }
    }
    for child_id in job.children_ids() {
        if let Ok(child) = job.get_child(child_id) {
            if let Ok(child_job) = child.downcast_arc::<Job>() {
                collect(&child_job, out);
            }
        }
    }
}

/// Count of `(live processes, currently-runnable threads)` across all jobs.
///
/// "Live" is every process that has started and not yet exited. "Runnable" is
/// *not* the same as "live": a process is alive for its whole lifetime even
/// while every one of its threads sits blocked in a syscall, so counting live
/// processes makes an idle box report a load average equal to its process
/// count. Instead we ask the executor how many threads are actually executing
/// right now ([`running_thread_count`]) and subtract one for the sampler thread
/// itself (the caller is, by definition, running while it reads this).
pub fn count_processes() -> (usize, usize) {
    let mut procs = Vec::new();
    collect(&ROOT_JOB, &mut procs);
    let total = procs.len();
    let running = runnable_count();
    (total, running)
}

/// Linux's `nr_threads`: how many live TASKS there are, which is what both
/// the denominator of `/proc/loadavg` and the `procs` field of `sysinfo(2)`
/// are (`loadavg_proc_show`, `do_sysinfo`).
///
/// Both printed a count of PROCESSES, and the two numbers differ by every
/// thread any program has started -- which is most of them: a shell with one
/// job, a JVM, or anything linked against a runtime with a reaper thread. The
/// denominator being the smaller of the two is what made `x/y` come out with
/// `x` bigger than `y` (see [`loadavg_tasks`]), and `top`, which parses that
/// field as its task total, showed fewer tasks than it was listing.
pub fn count_threads() -> usize {
    let mut procs = Vec::new();
    collect(&ROOT_JOB, &mut procs);
    procs.iter().map(|p| p.thread_count()).sum()
}

/// The `running/total` pair `/proc/loadavg` prints, from the runnable count
/// and the live thread count.
///
/// Two floors that the raw numbers do not have. `total` is at least 1,
/// because the process doing the read is itself a live task and a `0` there
/// makes every parser that divides by it fall over. `running` is at least 1
/// for the same reason and never more than `total`: the runnable count comes
/// from the executor's run queue, which counts kernel tasks the thread walk
/// above knows nothing about, so on an idle box with one process it read as
/// `2/1` -- two of one task running, which `top` renders as a task count that
/// goes backwards.
pub fn loadavg_tasks(runnable: usize, threads: usize) -> (usize, usize) {
    let total = threads.max(1);
    (runnable.saturating_add(1).min(total), total)
}

/// Number of runnable tasks system-wide, excluding the caller.
///
/// Prefers the executor's run-queue length (queued + currently-polled tasks,
/// which includes the calling thread — hence the `- 1`): unlike counting only
/// threads mid-poll, it also sees runnable-but-preempted work. On a fully idle
/// system only the caller occupies the queue — giving 0, matching a real Linux
/// idle box. Hosted (libos) builds report a queue length of 0 and fall back to
/// the old executing-thread count.
pub fn runnable_count() -> usize {
    let queued = kernel_hal::thread::runnable_task_count();
    if queued > 0 {
        queued - 1
    } else {
        running_thread_count().saturating_sub(1)
    }
}

/// Linux's `calc_load`: decay `load` toward `active` by factor `exp`.
fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let mut newload = load * exp + active * (FIXED_1 - exp);
    if active >= load {
        newload += FIXED_1 - 1;
    }
    newload / FIXED_1
}

/// Set once the periodic sampler has taken its first sample. From then on
/// reads only report state — the lazy read-time advance (which correlates the
/// sample with the reader and biases the average) stays off for good.
static SAMPLER_LIVE: AtomicBool = AtomicBool::new(false);

/// Advance the EWMA windows up to `now`, decaying toward `active` (the
/// runnable count in `FSHIFT` fixed point) for every elapsed 5 s step.
///
/// Pure, so the catch-up walk can be exercised without driving the clock:
/// takes the state and the time, returns the state it leaves behind.
fn advance_from(loads: [u64; 3], last_secs: u64, now: u64, active: u64) -> ([u64; 3], u64) {
    let mut loads = loads;
    // 0 means "never advanced": adopt `now` rather than walking from the
    // epoch, which would be 256 wasted steps on the first sample.
    let mut last_secs = if last_secs == 0 { now } else { last_secs };
    let mut steps = 0;
    while now >= last_secs + LOAD_FREQ_SECS && steps < MAX_CATCHUP_STEPS {
        last_secs += LOAD_FREQ_SECS;
        let l = loads;
        loads[0] = calc_load(l[0], EXP_1, active);
        loads[1] = calc_load(l[1], EXP_5, active);
        loads[2] = calc_load(l[2], EXP_15, active);
        steps += 1;
    }
    // Skipped a large gap (e.g. the sampler was started late): snap the clock
    // forward so the next advance doesn't re-walk the whole interval.
    if steps == MAX_CATCHUP_STEPS {
        last_secs = now;
    }
    (loads, last_secs)
}

/// See [`advance_from`]; this is the same thing against the shared state.
fn advance(active: u64) {
    let now = timer_now().as_secs();
    let mut g = STATE.lock();
    let (loads, last_secs) = advance_from(g.loads, g.last_secs, now, active);
    g.loads = loads;
    g.last_secs = last_secs;
}

/// Return the averages in `FSHIFT` fixed point. With the periodic sampler
/// live, this is a pure read; otherwise (hosted builds) it advances lazily
/// from the caller's instantaneous view, as before.
fn sample() -> [u64; 3] {
    if !SAMPLER_LIVE.load(Ordering::Relaxed) {
        let (_total, running) = count_processes();
        advance((running as u64) * FIXED_1);
    }
    STATE.lock().loads
}

/// Kernel-side load sampler: advances the averages every 5 s from the
/// executor's global run-queue length, uncorrelated with any reader. Spawned
/// once at boot (see the loader's deferred boot work); never returns.
pub async fn sampler_task() {
    loop {
        kernel_hal::thread::sleep_until(timer_now() + Duration::from_secs(LOAD_FREQ_SECS)).await;
        // This task occupies one run-queue slot while it samples; subtract it
        // so a fully idle box reads 0.00.
        let running = kernel_hal::thread::runnable_task_count().saturating_sub(1);
        advance((running as u64) * FIXED_1);
        SAMPLER_LIVE.store(true, Ordering::Relaxed);
    }
}

/// Load averages as `f64` (1, 5, 15 minutes), for textual reports like
/// `/proc/loadavg`.
pub fn loadavg_f64() -> [f64; 3] {
    let l = sample();
    [
        l[0] as f64 / FIXED_1 as f64,
        l[1] as f64 / FIXED_1 as f64,
        l[2] as f64 / FIXED_1 as f64,
    ]
}

/// Load averages in the fixed point `sysinfo(2)` expects (`<< SI_LOAD_SHIFT`).
pub fn loadavg_sysinfo() -> [u64; 3] {
    let l = sample();
    let shift = SI_LOAD_SHIFT - FSHIFT;
    [l[0] << shift, l[1] << shift, l[2] << shift]
}

#[cfg(test)]
mod loadavg_tests {
    //! The load-average arithmetic, which had no tests and is the kind of
    //! thing that drifts without anything failing: a wrong constant or a lost
    //! rounding term still produces a plausible-looking number, and nobody
    //! reads `/proc/loadavg` closely enough to notice it is wrong.

    use super::*;

    /// Linux's `calc_load` rounds **up** while the load is rising
    /// (`active >= load`), and that `FIXED_1 - 1` is not cosmetic: without it
    /// a permanently busy box converges on 0.99 and never reaches 1.00,
    /// because integer division keeps eating the last fraction. Drop the term
    /// and this test stops terminating at the right value.
    #[test]
    fn a_steady_load_is_reached_exactly_and_not_approached_forever() {
        let target = FIXED_1; // 1.00
        let mut load = 0;
        for _ in 0..4096 {
            load = calc_load(load, EXP_1, target);
            if load == target {
                break;
            }
        }
        assert_eq!(load, target, "a load of 1.00 must actually read 1.00");
    }

    /// And the other direction: an idle box must reach exactly zero, not sit
    /// at 0.01 for ever. Here the truncation of the division is what carries
    /// it home, since the rounding term only applies while rising.
    #[test]
    fn an_idle_box_decays_all_the_way_to_zero() {
        let mut load = 64 * FIXED_1; // a load of 64, then nothing to do
        for _ in 0..4096 {
            load = calc_load(load, EXP_1, 0);
            if load == 0 {
                break;
            }
        }
        assert_eq!(load, 0, "an idle system must read 0.00");
    }

    /// The fixed point of the recurrence: a load that already equals the
    /// current activity does not move. If this fails the average drifts on a
    /// perfectly steady machine, up or down, for no reason.
    #[test]
    fn a_load_equal_to_the_activity_does_not_move() {
        for exp in [EXP_1, EXP_5, EXP_15] {
            for load in [0, 1, FIXED_1 / 2, FIXED_1, 7 * FIXED_1, 1000 * FIXED_1] {
                assert_eq!(
                    calc_load(load, exp, load),
                    load,
                    "exp={} moved a steady load of {}",
                    exp,
                    load,
                );
            }
        }
    }

    /// Monotone in the right direction, for all three windows.
    #[test]
    fn the_average_always_moves_toward_the_current_activity() {
        for exp in [EXP_1, EXP_5, EXP_15] {
            let rising = calc_load(FIXED_1, exp, 4 * FIXED_1);
            assert!(rising > FIXED_1, "exp={} did not rise", exp);
            assert!(rising < 4 * FIXED_1, "exp={} overshot in one step", exp);

            let falling = calc_load(4 * FIXED_1, exp, FIXED_1);
            assert!(falling < 4 * FIXED_1, "exp={} did not fall", exp);
            assert!(falling > FIXED_1, "exp={} undershot in one step", exp);
        }
    }

    /// One step of decay with nothing runnable leaves exactly the constant:
    /// the `active` term drops out and the rounding does not apply, so
    /// `calc_load` is a plain multiply by `exp/2048`. That is the shape the
    /// window test below relies on.
    #[test]
    fn one_idle_step_from_a_full_load_leaves_the_decay_constant_itself() {
        for exp in [EXP_1, EXP_5, EXP_15] {
            assert_eq!(calc_load(FIXED_1, exp, 0), exp);
        }
    }

    /// The three constants *are* the three windows. Rather than assert the
    /// magic numbers back at themselves, measure what they do: from a full
    /// load with nothing left to run, an exponentially-weighted average with
    /// that time constant falls to 1/e after one window's worth of samples.
    /// Swapping two constants, or moving any of them by one, moves these
    /// numbers.
    ///
    /// They land a little *under* 1/e, and by more the longer the window,
    /// because every step's integer division truncates and the loss compounds.
    /// That is why the assertion is a value and a band rather than 753 with
    /// slack: the band says where the value may be, the value says where it is.
    #[test]
    fn each_decay_constant_matches_the_window_it_is_named_after() {
        // 1/e of 1.00 in FSHIFT fixed point: 2048 / 2.71828 = 753.
        const ONE_OVER_E: u64 = 753;
        for (exp, window_secs, landing, name) in [
            (EXP_1, 60u64, 749u64, "1 minute"),
            (EXP_5, 300, 731, "5 minutes"),
            (EXP_15, 900, 720, "15 minutes"),
        ] {
            let steps = window_secs / LOAD_FREQ_SECS;
            let mut load = FIXED_1;
            for _ in 0..steps {
                load = calc_load(load, exp, 0);
            }
            assert_eq!(
                load, landing,
                "the {} window no longer decays the way it did",
                name,
            );
            assert!(
                load < ONE_OVER_E && load >= ONE_OVER_E - steps,
                "after {}s the {} average is {}, outside [{}, {}) -- truncation \
                 can only lose one unit per step, so this is not rounding",
                window_secs,
                name,
                load,
                ONE_OVER_E - steps,
                ONE_OVER_E,
            );
        }
    }

    /// `sysinfo(2)` reports the same averages in its own fixed point. The
    /// shift between the two is the whole conversion, and getting it wrong
    /// scales every reading by a power of two without any other symptom.
    #[test]
    fn the_sysinfo_shift_turns_one_point_zero_into_si_load_shift() {
        assert_eq!(SI_LOAD_SHIFT - FSHIFT, 5);
        assert_eq!(FIXED_1 << (SI_LOAD_SHIFT - FSHIFT), 1 << SI_LOAD_SHIFT);
    }

    /// The catch-up walk. Nothing happens until a whole period has passed,
    /// and then exactly one step happens per period.
    #[test]
    fn the_walk_takes_one_step_per_elapsed_period() {
        let start = [FIXED_1; 3];
        let (loads, last) = advance_from(start, 1000, 1000 + LOAD_FREQ_SECS - 1, 0);
        assert_eq!((loads, last), (start, 1000), "a partial period is no step");

        let (one, last) = advance_from(start, 1000, 1000 + LOAD_FREQ_SECS, 0);
        assert_eq!(last, 1000 + LOAD_FREQ_SECS);
        assert_eq!(one, [EXP_1, EXP_5, EXP_15], "one idle step is the constant");

        let (three, last) = advance_from(start, 1000, 1000 + 3 * LOAD_FREQ_SECS, 0);
        assert_eq!(last, 1000 + 3 * LOAD_FREQ_SECS);
        // All three slots, each with its own constant: the windows sit in one
        // array and swapping two of them is a silent, plausible-looking
        // change that nothing else here would see.
        for (slot, exp) in [(0usize, EXP_1), (1, EXP_5), (2, EXP_15)] {
            let mut by_hand = FIXED_1;
            for _ in 0..3 {
                by_hand = calc_load(by_hand, exp, 0);
            }
            assert_eq!(
                three[slot], by_hand,
                "slot {} is not decaying with its own constant",
                slot,
            );
        }
    }

    /// A first sample adopts the clock instead of walking from the epoch. The
    /// difference is 256 pointless steps on the very first read, and an
    /// average that starts out already decayed to nothing.
    #[test]
    fn the_first_sample_adopts_the_clock_rather_than_walking_from_boot() {
        let (loads, last) = advance_from([FIXED_1; 3], 0, 100_000, 0);
        assert_eq!(last, 100_000);
        assert_eq!(loads, [FIXED_1; 3], "the first sample must not decay");
    }

    /// A long gap is capped, and the clock snaps forward so the *next* call
    /// does not walk the same interval again. Without the snap, every read
    /// after a long sleep pays for 256 steps, for ever.
    #[test]
    fn a_long_gap_is_capped_and_does_not_have_to_be_walked_twice() {
        let now = 10_000_000u64;
        let (loads, last) = advance_from([FIXED_1; 3], 1, now, 0);
        assert_eq!(last, now, "the clock must snap forward after a capped walk");

        let mut by_hand = FIXED_1;
        for _ in 0..MAX_CATCHUP_STEPS {
            by_hand = calc_load(by_hand, EXP_15, 0);
        }
        assert_eq!(loads[2], by_hand, "exactly the cap, no more and no fewer");

        // The second call has nothing left to do.
        assert_eq!(advance_from(loads, last, now, 0), (loads, last));
    }

    /// The cap has to cover the longest window, or a box that was quiet for a
    /// while reports a 15-minute average that never finished settling.
    #[test]
    fn the_catch_up_cap_outlasts_the_longest_window() {
        let covered = MAX_CATCHUP_STEPS as u64 * LOAD_FREQ_SECS;
        assert!(
            covered >= 15 * 60,
            "the cap covers {}s, less than the 15-minute window",
            covered,
        );
    }

    /// The pair `/proc/loadavg` prints. `2/1` was what an idle box with one
    /// process actually showed.
    #[test]
    fn the_running_count_never_passes_the_total() {
        assert_eq!(loadavg_tasks(0, 1), (1, 1));
        assert_eq!(loadavg_tasks(5, 1), (1, 1));
        assert_eq!(loadavg_tasks(3, 40), (4, 40));
    }

    /// Nothing may divide by zero on this line, and the reader itself is a
    /// live task, so neither field is ever 0.
    #[test]
    fn neither_field_is_ever_zero() {
        assert_eq!(loadavg_tasks(0, 0), (1, 1));
    }

    /// A runnable count at the top of the range must not wrap the `+ 1`.
    #[test]
    fn a_runnable_count_at_the_top_of_the_range_does_not_wrap() {
        assert_eq!(loadavg_tasks(usize::MAX, 7), (7, 7));
    }
}
