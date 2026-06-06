//! Deferred job queue.
//!
//! Allows interrupt / IRQ handlers to schedule work that should run outside of
//! an atomic context (e.g. in the next scheduler tick or poll loop).
//!
//! Jobs are closures pushed onto a global queue via [`push_deferred_job`] and
//! drained with [`drain_deferred_jobs`].

use alloc::boxed::Box;
use alloc::vec::Vec;
use lock::Mutex;

type Job = Box<dyn FnOnce() + Send + 'static>;

extern "C" {
    fn drivers_intr_on();
    fn drivers_intr_off();
    fn drivers_intr_get() -> bool;
}

fn intr_get() -> bool {
    unsafe { drivers_intr_get() }
}

fn intr_off() {
    unsafe { drivers_intr_off() }
}

fn intr_on() {
    unsafe { drivers_intr_on() }
}

static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());

/// Cap queued IRQ work — unbounded growth looks like a kernel leak.
const MAX_DEFERRED_JOBS: usize = 256;
/// Run at most this many deferred jobs per drain (long NIC init must not starve PS/2/USB).
const MAX_JOBS_PER_DRAIN: usize = 2;

/// Drop the oldest queued job without running it (IRQ must not execute arbitrary work).
#[allow(unused_must_use)]
fn evict_oldest_job(q: &mut Vec<Job>) {
    if !q.is_empty() {
        drop(q.remove(0));
    }
}

/// Enqueue a closure to be executed later outside of IRQ context.
pub fn push_deferred_job<F: FnOnce() + Send + 'static>(f: F) {
    let flag = intr_get();
    if flag {
        intr_off();
    }
    {
        let mut q = JOBS.lock();
        if q.len() >= MAX_DEFERRED_JOBS {
            evict_oldest_job(&mut q);
        }
        q.push(Box::new(f));
    }
    if flag {
        intr_on();
    }
}

/// Execute all currently queued deferred jobs.
///
/// Should be called from a non-atomic context (e.g. the kernel idle loop or a
/// timer tick handler).
pub fn drain_deferred_jobs() {
    drain_deferred_jobs_max(MAX_JOBS_PER_DRAIN);
}

/// Run at most `max` deferred jobs (requeue the rest). Use before NIC poll when
/// stdin/HID must stay responsive.
pub fn drain_deferred_jobs_max(max: usize) {
    let cap = max.max(1).min(MAX_DEFERRED_JOBS);
    let flag = intr_get();
    if flag {
        intr_off();
    }
    let mut jobs: Vec<Job> = {
        let mut q = JOBS.lock();
        core::mem::take(&mut *q)
    };
    if flag {
        intr_on();
    }
    let run = jobs.len().min(cap);
    for job in jobs.drain(..run) {
        job();
    }
    if !jobs.is_empty() {
        let mut q = JOBS.lock();
        for job in jobs.into_iter().rev() {
            q.insert(0, job);
        }
    }
}
