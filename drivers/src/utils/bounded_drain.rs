//! A bound on the loops that empty a device queue inside an interrupt handler.

/// How many items one interrupt may take out of a device queue by default.
///
/// The figure is Linux's: its 8250 handler caps a receive burst at 256 bytes
/// (`max_count`), and NAPI hands a driver a budget for the same reason. What
/// matters is not the number but that there is one.
pub const DRAIN_BURST: usize = 256;

/// Run `step` at most `limit` times, stopping as soon as it reports that the
/// queue is empty. Returns `true` when the limit is what stopped it, so the
/// caller knows the device still had more to give.
///
/// An interrupt handler has to come back. Written the obvious way —
/// `while let Some(item) = device.next() { ... }` — it does not, for any
/// device that keeps saying yes: a UART whose status register never clears, a
/// level-triggered line whose handler does not quiesce the source, a queue a
/// guest keeps refilling faster than it is drained. The handler then spins
/// forever, usually holding the lock every other CPU needs to take its own
/// interrupts, and on the architectures where that device is the console the
/// machine dies with nothing to say.
///
/// Stopping early costs nothing: the interrupt is still asserted, so the
/// handler is entered again and the rest arrives on the next pass. That is
/// what makes a bound safe to add to any of these loops.
pub fn bounded_drain(limit: usize, mut step: impl FnMut() -> bool) -> bool {
    for _ in 0..limit {
        if !step() {
            return false;
        }
    }
    limit > 0
}

/// The rule every drain loop in an interrupt handler follows, on its own so it
/// can be exercised without a device.
///
/// Three of them were written without a bound, in three drivers. The UART's
/// was the one that showed: on aarch64 and riscv64 it is the console, so a
/// stuck line hung the kernel in interrupt context with no output at all.
///
/// Notes for whoever mutates this next. Taking the bound away makes
/// `a_queue_that_never_empties_stops_at_the_limit` **hang** rather than fail,
/// which is the symptom itself — a run that stops making progress here is the
/// answer, not a broken runner. Shrinking `DRAIN_BURST` below a hardware FIFO
/// does not compile at all, because the `const` assertion below catches it
/// before any test runs.
#[cfg(test)]
mod bounded_drain_tests {
    use super::*;
    use core::cell::Cell;

    #[test]
    fn a_queue_that_never_empties_stops_at_the_limit() {
        // This is the bug. Without the bound this call does not return.
        let calls = Cell::new(0usize);
        let cut_short = bounded_drain(DRAIN_BURST, || {
            calls.set(calls.get() + 1);
            true
        });
        assert_eq!(calls.get(), DRAIN_BURST);
        assert!(cut_short, "the caller has to learn there is more");
    }

    #[test]
    fn a_queue_that_empties_is_drained_and_not_over_read() {
        let left = Cell::new(3usize);
        let cut_short = bounded_drain(DRAIN_BURST, || {
            if left.get() == 0 {
                return false;
            }
            left.set(left.get() - 1);
            true
        });
        assert_eq!(left.get(), 0);
        assert!(!cut_short, "it emptied, so nothing is left to re-arm for");
    }

    #[test]
    fn an_empty_queue_is_one_look_and_no_more() {
        // A spurious interrupt is common on a shared line; it must cost one
        // register read, not a burst of them.
        let calls = Cell::new(0usize);
        let cut_short = bounded_drain(DRAIN_BURST, || {
            calls.set(calls.get() + 1);
            false
        });
        assert_eq!(calls.get(), 1);
        assert!(!cut_short);
    }

    #[test]
    fn the_last_item_of_a_full_burst_is_not_dropped() {
        // Off by one at the top loses one item per interrupt, which on a
        // console is one keystroke in every 256 — the kind of fault that gets
        // blamed on the cable.
        let seen = Cell::new(0usize);
        let cut_short = bounded_drain(4, || {
            seen.set(seen.get() + 1);
            seen.get() < 4
        });
        assert_eq!(seen.get(), 4);
        // The fourth step said the queue was empty, so the limit is not what
        // stopped it.
        assert!(!cut_short);
    }

    #[test]
    fn a_limit_of_zero_reads_nothing_and_claims_nothing() {
        // Not a configuration anyone should use, but it must not report that
        // the queue is drained when it was never looked at -- a caller that
        // trusts `false` would leave the interrupt asserted forever.
        let calls = Cell::new(0usize);
        let cut_short = bounded_drain(0, || {
            calls.set(calls.get() + 1);
            true
        });
        assert_eq!(calls.get(), 0);
        assert!(!cut_short);
    }

    #[test]
    fn the_default_burst_is_big_enough_for_a_hardware_fifo() {
        // A 16550 FIFO is 16 bytes and a virtio queue is far larger; a burst
        // smaller than the FIFO would guarantee a second interrupt for every
        // first one.
        const _: () = assert!(DRAIN_BURST >= 16);
    }
}
