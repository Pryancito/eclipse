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
        // These runtime/task locks are taken from timer-IRQ context (IRQs off),
        // so a CPU parked here is deaf to TLB-shootdown IPIs. A peer unmapping
        // memory spin-waits for our ack while holding the VMAR lock; without
        // this drain that peer wedges and every CPU behind the VMAR lock wedges
        // with it (observed as a multi-core vmar.rs DEADLOCK banner). Pump our
        // own shootdown queue at the same cadence the kernel's ticket lock does.
        if spins & 511 == 0 {
            lock::pump();
        }
        if spins == DEADLOCK_SPINS {
            lock::report_stuck(caller.file(), caller.line());
        }
    }
}

#[cfg(test)]
mod diag_lock_tests {
    use super::diag_lock;
    use spin::Mutex;

    /// Sin disputa es un `try_lock` y se va: es el caso de siempre, y el que
    /// no puede pagar nada por el informe que esto existe para dar.
    #[test]
    fn an_uncontended_lock_is_taken_on_the_first_try() {
        let m = Mutex::new(7u32);
        {
            let mut g = diag_lock(&m);
            assert_eq!(*g, 7);
            *g = 9;
            assert!(m.try_lock().is_none(), "el cerrojo esta tomado de verdad");
        }
        assert_eq!(*diag_lock(&m), 9);
    }

    /// Y con disputa gira hasta que el otro suelta, en vez de volver con las
    /// manos vacias: los cerrojos de este modulo se toman desde el callback
    /// del timer, donde un `None` no tiene a quien devolverselo.
    #[test]
    fn a_contended_lock_is_waited_for_and_not_refused() {
        use alloc::sync::Arc;
        let m = Arc::new(Mutex::new(0u32));
        let held = Arc::new(std::sync::Barrier::new(2));
        let peer_m = m.clone();
        let peer_held = held.clone();
        let peer = std::thread::spawn(move || {
            let mut g = peer_m.lock();
            peer_held.wait();
            std::thread::sleep(std::time::Duration::from_millis(20));
            *g = 1;
        });
        held.wait();
        // El otro hilo lo tiene; esto no vuelve hasta que lo suelta, y
        // entonces ve su escritura.
        assert_eq!(*diag_lock(&m), 1);
        peer.join().unwrap();
    }
}
