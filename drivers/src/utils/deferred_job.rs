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

/// Enqueue a closure to be executed later outside of IRQ context.
pub fn push_deferred_job<F: FnOnce() + Send + 'static>(f: F) {
    let flag = intr_get();
    if flag {
        intr_off();
    }
    let mut q = JOBS.lock();
    if q.len() >= MAX_DEFERRED_JOBS {
        if flag {
            intr_on();
        }
        return;
    }
    q.push(Box::new(f));
    if flag {
        intr_on();
    }
}

/// Execute all currently queued deferred jobs.
///
/// Should be called from a non-atomic context (e.g. the kernel idle loop or a
/// timer tick handler).
pub fn drain_deferred_jobs() {
    let flag = intr_get();
    if flag {
        intr_off();
    }
    let jobs: Vec<Job> = {
        let mut q = JOBS.lock();
        core::mem::take(&mut *q)
    };
    if flag {
        intr_on();
    }
    for job in jobs {
        job();
    }
}
