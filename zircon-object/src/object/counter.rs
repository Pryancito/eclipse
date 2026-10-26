use {super::*, alloc::sync::Arc, kernel_hal::sync::Mutex};

/// A signed 64-bit kernel counter.
pub struct Counter {
    base: KObjectBase,
    value: Mutex<i64>,
}

impl_kobject!(Counter
    fn allowed_signals(&self) -> Signal {
        // `ZX_COUNTER_SIGNALED` is `__ZX_OBJECT_SIGNAL_3`, the same bit as
        // `SIGNALED`, and Zircon lets userspace assert and deassert it on a
        // counter with `zx_object_signal` -- `CounterTest.Signal` does exactly
        // that. What stays out is `COUNTER_POSITIVE` / `NON_POSITIVE`: those
        // the kernel owns, it derives them from the value in `update_signals`.
        Signal::USER_ALL | Signal::SIGNALED
    }
);

impl Counter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            base: KObjectBase::with_signal(Signal::COUNTER_NON_POSITIVE),
            value: Mutex::new(0),
        })
    }

    pub fn read(&self) -> i64 {
        *self.value.lock()
    }

    pub fn write(&self, value: i64) {
        let mut current = self.value.lock();
        let old = *current;
        *current = value;
        self.update_signals(old, value);
    }

    pub fn add(&self, amount: i64) -> ZxResult {
        let mut current = self.value.lock();
        let old = *current;
        let new = old.checked_add(amount).ok_or(ZxError::OUT_OF_RANGE)?;
        *current = new;
        self.update_signals(old, new);
        Ok(())
    }

    fn update_signals(&self, old: i64, new: i64) {
        if old <= 0 && new > 0 {
            self.base
                .signal_change(Signal::COUNTER_NON_POSITIVE, Signal::COUNTER_POSITIVE);
        } else if old > 0 && new <= 0 {
            self.base
                .signal_change(Signal::COUNTER_POSITIVE, Signal::COUNTER_NON_POSITIVE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn counter_value_and_signal_after_concurrent_writers() {
        let c = Counter::new();
        let barrier = Arc::new(Barrier::new(5));
        let workers: Vec<_> = (0..4)
            .map(|id| {
                let c = c.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    for _ in 0..10000 {
                        barrier.wait();
                        c.write(if id % 2 == 0 { 1 } else { 0 });
                        barrier.wait();
                    }
                })
            })
            .collect();
        let mut mismatch = None;
        for round in 0..10000 {
            c.write(1);
            c.write(0);
            barrier.wait();
            barrier.wait();
            let value = c.read();
            let signal = c.signal();
            if signal.contains(Signal::COUNTER_POSITIVE) != (value > 0) {
                mismatch.get_or_insert((round, value, signal));
            }
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(mismatch.is_none(), "value/signal mismatch: {:?}", mismatch);
    }

    #[test]
    fn overflow_preserves_value_and_signals() {
        let counter = Counter::new();
        counter.write(i64::MAX);
        assert_eq!(counter.add(1), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(counter.read(), i64::MAX);
        assert!(counter.signal().contains(Signal::COUNTER_POSITIVE));
        assert!(!counter
            .allowed_signals()
            .intersects(Signal::COUNTER_POSITIVE | Signal::COUNTER_NON_POSITIVE));
    }

    /// `ZX_COUNTER_SIGNALED` shares a bit with `SIGNALED`, and it is
    /// user-assertable on a counter: `CounterTest.Signal` asserts it, reads it
    /// back next to the kernel's own `COUNTER_NON_POSITIVE`, and deasserts it.
    /// Leaving it out of `allowed_signals` made every one of those four steps
    /// answer `ZX_ERR_INVALID_ARGS`.
    #[test]
    fn object_signal_can_assert_counter_signaled() {
        let allowed = Counter::new().allowed_signals();
        assert!(
            allowed.contains(Signal::SIGNALED),
            "ZX_COUNTER_SIGNALED is user-assertable on a counter"
        );
        assert!(allowed.contains(Signal::USER_SIGNAL_0));
        assert!(
            !allowed.intersects(Signal::COUNTER_POSITIVE | Signal::COUNTER_NON_POSITIVE),
            "the value-derived signals stay the kernel's"
        );
    }

    /// The whole sequence of `CounterTest.Signal`, in kernel terms: assert the
    /// two, see them beside the kernel's `NON_POSITIVE`, deassert them, and be
    /// left with `NON_POSITIVE` alone.
    #[test]
    fn asserting_and_deasserting_counter_signaled_leaves_the_kernel_signal() {
        let counter = Counter::new();
        let both = Signal::USER_SIGNAL_0 | Signal::SIGNALED;
        counter.signal_set(both);
        assert_eq!(counter.signal(), Signal::COUNTER_NON_POSITIVE | both);
        counter.signal_clear(both);
        assert_eq!(counter.signal(), Signal::COUNTER_NON_POSITIVE);
    }

    /// A counter starts at zero, zero is not positive, so a fresh counter
    /// starts with `COUNTER_NON_POSITIVE` already asserted: a waiter asking
    /// "is it still empty?" has to be told yes without anyone writing first.
    /// Every test here wrote a value before looking.
    #[test]
    fn a_fresh_counter_is_already_non_positive() {
        let counter = Counter::new();
        assert_eq!(counter.read(), 0);
        assert!(
            counter.signal().contains(Signal::COUNTER_NON_POSITIVE),
            "a waiter on an empty counter would never be woken"
        );
        assert!(!counter.signal().contains(Signal::COUNTER_POSITIVE));
    }
}
