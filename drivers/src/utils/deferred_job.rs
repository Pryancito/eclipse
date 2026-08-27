//! Deferred job queue.
//!
//! Allows interrupt / IRQ handlers to schedule work that should run outside of
//! an atomic context (e.g. in the next scheduler tick or poll loop).
//!
//! Jobs are closures pushed onto a global queue via [`push_deferred_job`] and
//! drained with [`drain_deferred_jobs`].

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use lock::Mutex;

type Job = Box<dyn FnOnce() + Send + 'static>;

static JOBS: Mutex<VecDeque<Job>> = Mutex::new(VecDeque::new());

/// Cap queued IRQ work — unbounded growth looks like a kernel leak.
const MAX_DEFERRED_JOBS: usize = 256;
/// Run at most this many deferred jobs per drain.
///
/// Was 2: under HID/USB/watchdog pressure that budget silently aged out NIC
/// bottom-halves (e1000e `poll_pending` stuck → interrupt-deaf). 8 matches the
/// adaptive net drain floor in `linux-object` and still bounds long jobs so
/// PS/2/USB keep getting turns across drains.
const MAX_JOBS_PER_DRAIN: usize = 8;

/// Drop the oldest queued job without running it (IRQ must not execute arbitrary work).
#[allow(unused_must_use)]
fn evict_oldest_job(q: &mut VecDeque<Job>) {
    let _ = q.pop_front();
}

/// Drop the newest queued job (used when inserting an urgent front job).
#[allow(unused_must_use)]
fn evict_newest_job(q: &mut VecDeque<Job>) {
    let _ = q.pop_back();
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
        evict_oldest_job(&mut q);
    }
    q.push_back(Box::new(f));
}

/// Enqueue at the front so the next drain runs this job first.
///
/// For NIC IRQ bottom-halves that must not sit behind a backlog of lower-urgency
/// work (and risk eviction from the 256-cap FIFO). When the queue is full, drop
/// the newest back entry instead of the oldest — the oldest may itself be an
/// urgent job already at the front.
pub fn push_deferred_job_front<F: FnOnce() + Send + 'static>(f: F) {
    let mut q = JOBS.lock();
    if q.len() >= MAX_DEFERRED_JOBS {
        evict_newest_job(&mut q);
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

/// Run at most `max` deferred jobs (requeue the rest). Use before NIC poll when
/// stdin/HID must stay responsive.
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
