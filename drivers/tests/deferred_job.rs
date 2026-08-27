use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use zcore_drivers::utils::deferred_job::{
    drain_deferred_jobs, drain_deferred_jobs_max, pending_deferred_jobs, push_deferred_job,
    push_deferred_job_front,
};

#[test]
fn push_and_drain() {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    push_deferred_job(move || {
        c.fetch_add(1, Ordering::SeqCst);
    });
    assert!(pending_deferred_jobs() >= 1);
    drain_deferred_jobs();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn drain_max_limits_jobs() {
    let counter = Arc::new(AtomicUsize::new(0));
    for _ in 0..4 {
        let c = counter.clone();
        push_deferred_job(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
    }
    // drain_deferred_jobs_max(1) should run at most 1 job
    drain_deferred_jobs_max(1);
    let after_one = counter.load(Ordering::SeqCst);
    assert!(
        after_one <= 1,
        "expected at most 1 job drained, got {after_one}"
    );
    // drain remaining
    drain_deferred_jobs_max(8);
    drain_deferred_jobs_max(8);
}

#[test]
fn front_job_runs_before_back() {
    let order = Arc::new(AtomicUsize::new(0));
    let first = Arc::new(AtomicUsize::new(0));
    let second = Arc::new(AtomicUsize::new(0));

    let o = order.clone();
    let s = second.clone();
    push_deferred_job(move || {
        s.store(o.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
    });
    let o = order.clone();
    let f = first.clone();
    push_deferred_job_front(move || {
        f.store(o.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
    });

    drain_deferred_jobs_max(2);
    assert_eq!(first.load(Ordering::SeqCst), 1, "front job should run first");
    assert_eq!(second.load(Ordering::SeqCst), 2, "back job should run second");
}
