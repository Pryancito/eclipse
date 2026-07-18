//! Deadlock-visible locking for the scheduler's `spin::Mutex`es.
//!
//! The runtime/task locks here are taken from timer-IRQ context on every tick,
//! so a deadlock involving them freezes every CPU with no panic and no console
//! output. The kernel's own `lock::Mutex` self-reports long spins through a
//! lock-free hook (painted straight onto the framebuffer); this helper gives
//! the scheduler's external `spin::Mutex`es the same behavior: spin with
//! `try_lock`, and after ~8s of continuous spinning report the stuck call site
//! (once) through `lock::report_stuck`, then keep spinning.

use spin::{Mutex, MutexGuard};

/// ~8s of PAUSE iterations — orders of magnitude beyond legitimate contention.
const DEADLOCK_SPINS: u64 = 1_000_000_000;

#[track_caller]
pub(crate) fn diag_lock<'a, T>(m: &'a Mutex<T>) -> MutexGuard<'a, T> {
    let caller = core::panic::Location::caller();
    let mut spins: u64 = 0;
    loop {
        if let Some(g) = m.try_lock() {
            return g;
        }
        core::hint::spin_loop();
        spins += 1;
        if spins == DEADLOCK_SPINS {
            lock::report_stuck(caller.file(), caller.line());
        }
    }
}
