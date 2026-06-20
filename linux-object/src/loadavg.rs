//! Process accounting and approximate, Linux-style system load averages.
//!
//! Linux keeps three exponentially-weighted moving averages of the
//! run-queue length, sampled every 5 seconds (the 1-, 5- and 15-minute
//! windows). We have no periodic kernel sampler wired up, so we advance the
//! averages lazily whenever they are read: each read walks forward in
//! 5-second steps from the last update, decaying toward the current number
//! of runnable processes. On an idle box this stays at 0.00; under load that
//! is polled regularly (e.g. `top`/`uptime`) it climbs as expected.

use alloc::sync::Arc;
use alloc::vec::Vec;
use kernel_hal::timer::timer_now;
use lazy_static::lazy_static;
use lock::Mutex;
use zircon_object::object::KernelObject;
use zircon_object::task::{Job, Process, Status, ROOT_JOB};

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

/// Count of `(live processes, currently-running processes)` across all jobs.
pub fn count_processes() -> (usize, usize) {
    let mut procs = Vec::new();
    collect(&ROOT_JOB, &mut procs);
    let total = procs.len();
    let running = procs
        .iter()
        .filter(|p| matches!(p.status(), Status::Running))
        .count();
    (total, running)
}

/// Linux's `calc_load`: decay `load` toward `active` by factor `exp`.
fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let mut newload = load * exp + active * (FIXED_1 - exp);
    if active >= load {
        newload += FIXED_1 - 1;
    }
    newload / FIXED_1
}

/// Advance the averages up to `now` and return them in `FSHIFT` fixed point.
fn sample() -> [u64; 3] {
    let now = timer_now().as_secs();
    let (_total, running) = count_processes();
    let active = (running as u64) * FIXED_1;

    let mut g = STATE.lock();
    if g.last_secs == 0 {
        g.last_secs = now;
    }
    let mut steps = 0;
    while now >= g.last_secs + LOAD_FREQ_SECS && steps < MAX_CATCHUP_STEPS {
        g.last_secs += LOAD_FREQ_SECS;
        let l = g.loads;
        g.loads[0] = calc_load(l[0], EXP_1, active);
        g.loads[1] = calc_load(l[1], EXP_5, active);
        g.loads[2] = calc_load(l[2], EXP_15, active);
        steps += 1;
    }
    // Skipped a large gap (e.g. the box sat unread for a long time): snap the
    // clock forward so the next read doesn't re-walk the whole interval.
    if steps == MAX_CATCHUP_STEPS {
        g.last_secs = now;
    }
    g.loads
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
