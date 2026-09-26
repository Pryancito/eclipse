//! A console lock that a panic can always get out of.
//!
//! The serial writer is a plain `spin::Mutex`, and the panic handler writes
//! through it with `lock()` rather than `try_lock()` on purpose: by the time it
//! runs, dropping the report is worse than waiting for it. The comment at the
//! call site says so, and it is right — but `lock()` on a non-reentrant mutex
//! has two ways of never returning, and the panic path is the one place where
//! "never returns" costs the whole report.
//!
//! **It can wait for itself.** `SerialWriter::write_str` ends in
//! `uart.write_str(s).unwrap()`, inside the critical section. If that `unwrap`
//! fires — or anything under it faults — the panic handler runs on a CPU that
//! is already holding the serial lock, and its very first act is to ask for it
//! again. Interrupts are off and the panic strategy is `abort`, so the guard is
//! never dropped: nobody will ever release it. The machine stops with the
//! report still in the formatter, and a silent freeze is indistinguishable from
//! a hang with no panic at all.
//!
//! **It can wait for a CPU that is not coming back.** The other holder may be
//! halted, triple-faulted, or stuck in the very deadlock being reported.
//!
//! Both have the same shape: the lock is being asked to provide mutual
//! exclusion at a moment when losing the output is the only real failure.
//! [`ConsoleLock`] therefore records **who** holds it, so a nested acquisition
//! is recognised instead of waited on, and bounds the wait, so a dead holder
//! costs a bounded spin and then gets the lock taken away. Interleaved output
//! is a bad panic report; no output is not a report at all.
//!
//! Everything here is policy over one word, so it is decided and tested on the
//! host; the console only supplies the CPU id and does the writing.

use core::sync::atomic::{AtomicU32, Ordering};

/// Sentinel owner for "nobody holds this lock".
///
/// `cpu_id()` is a `u8` everywhere in this kernel, so no real CPU can collide
/// with it.
pub const NOBODY: u32 = u32::MAX;

/// How many times the panic path spins before it takes the lock away.
///
/// Sized to be plainly longer than any honest console write (a full line over a
/// 115200-baud UART is on the order of a millisecond) and plainly shorter than
/// a human waiting at a frozen screen.
pub const PANIC_SPIN_BUDGET: u32 = 40_000_000;

/// What a failed attempt to take the lock means, given who actually holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// This CPU already holds it. **Waiting can never resolve this**: the
    /// holder is the caller. Write through without taking it.
    Ourselves,
    /// Another CPU holds it and there is budget left; spin.
    Wait,
    /// Another CPU holds it and the budget is spent; take it away.
    Steal,
}

/// The pure rule behind [`ConsoleLock::acquire_for_panic`].
///
/// `owner` is who the lock says holds it, `me` is the caller, `spun` is how
/// many times the caller has already tried and `budget` is how many times it is
/// willing to.
///
/// The first arm is the whole point: it does not consult the budget, because a
/// CPU waiting for itself is not slow, it is stopped.
pub fn blocked_by(owner: u32, me: u32, spun: u32, budget: u32) -> Blocked {
    if owner == me {
        Blocked::Ourselves
    } else if spun < budget {
        Blocked::Wait
    } else {
        Blocked::Steal
    }
}

/// How an acquisition ended. Hand it back to [`ConsoleLock::release`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    /// The lock was free and is now ours.
    Taken,
    /// This CPU already held it, so the write goes through on the outer
    /// holder's ticket. Releasing must **not** free the lock.
    Nested,
    /// Another CPU held it past the budget and we took it away.
    Stolen,
}

impl Acquired {
    /// Whether releasing this should actually free the lock.
    pub fn frees(self) -> bool {
        !matches!(self, Acquired::Nested)
    }
}

/// A mutual-exclusion word that remembers its owner and can be taken away.
pub struct ConsoleLock {
    owner: AtomicU32,
    steals: AtomicU32,
}

impl Default for ConsoleLock {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleLock {
    pub const fn new() -> Self {
        Self {
            owner: AtomicU32::new(NOBODY),
            steals: AtomicU32::new(0),
        }
    }

    /// Who the lock believes holds it, or [`NOBODY`].
    pub fn owner(&self) -> u32 {
        self.owner.load(Ordering::Acquire)
    }

    /// How many times a panic write has had to take the lock away from a holder
    /// that never gave it back. Any non-zero value names a real bug elsewhere.
    pub fn steals(&self) -> u32 {
        self.steals.load(Ordering::Relaxed)
    }

    /// Take the lock if it is free, and give up at once if it is not.
    ///
    /// `None` means the caller drops its output, which is what every ordinary
    /// console writer already did with `try_lock`. A nested call from the CPU
    /// that already holds it also gets `None`, so a writer that re-enters
    /// itself stops instead of recursing.
    pub fn try_acquire(&self, me: u32) -> Option<Acquired> {
        self.owner
            .compare_exchange(NOBODY, me, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Acquired::Taken)
    }

    /// Take the lock for a panic-context write. **Always returns.**
    ///
    /// `pause` is called once per spin (`core::hint::spin_loop` on hardware, a
    /// counter in tests).
    pub fn acquire_for_panic(&self, me: u32, budget: u32, mut pause: impl FnMut()) -> Acquired {
        let mut spun: u32 = 0;
        loop {
            match self
                .owner
                .compare_exchange(NOBODY, me, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return Acquired::Taken,
                Err(owner) => match blocked_by(owner, me, spun, budget) {
                    Blocked::Ourselves => return Acquired::Nested,
                    Blocked::Wait => {
                        pause();
                        spun = spun.saturating_add(1);
                    }
                    Blocked::Steal => {
                        self.owner.store(me, Ordering::Release);
                        self.steals.fetch_add(1, Ordering::Relaxed);
                        return Acquired::Stolen;
                    }
                },
            }
        }
    }

    /// Give the lock back, unless the acquisition was nested.
    pub fn release(&self, how: Acquired) {
        if how.frees() {
            self.owner.store(NOBODY, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    const ME: u32 = 3;
    const OTHER: u32 = 5;

    /// A `pause` that counts, and that refuses to wait past `cap`.
    ///
    /// The cap is not decoration. Every interesting way of getting this rule
    /// wrong ends in a loop that never exits, and a test suite that hangs names
    /// nothing: the cap turns "spins for ever" into a named failing test.
    fn bounded(cell: &Cell<u32>, cap: u32) -> impl FnMut() + '_ {
        move || {
            cell.set(cell.get() + 1);
            assert!(
                cell.get() <= cap,
                "spun {} times with a cap of {cap}: this wait does not end",
                cell.get()
            );
        }
    }

    // -- the rule ----------------------------------------------------------

    #[test]
    fn a_cpu_that_holds_the_lock_is_not_told_to_wait_for_it() {
        // The bug this module exists for: `lock()` on a non-reentrant mutex
        // waits, and the wait can never end, because the holder is the caller.
        assert_eq!(blocked_by(ME, ME, 0, u32::MAX), Blocked::Ourselves);
    }

    #[test]
    fn holding_it_ourselves_does_not_become_waitable_by_having_budget_left() {
        // Budget is about a holder that might still finish. We never will.
        for spun in [0, 1, 999, u32::MAX] {
            for budget in [0, 1, 1000, u32::MAX] {
                assert_eq!(blocked_by(ME, ME, spun, budget), Blocked::Ourselves);
            }
        }
    }

    #[test]
    fn another_cpu_with_budget_left_is_worth_waiting_for() {
        assert_eq!(blocked_by(OTHER, ME, 0, 10), Blocked::Wait);
        assert_eq!(blocked_by(OTHER, ME, 9, 10), Blocked::Wait);
    }

    #[test]
    fn the_budget_is_spent_when_it_is_reached_not_after() {
        assert_eq!(blocked_by(OTHER, ME, 9, 10), Blocked::Wait);
        assert_eq!(blocked_by(OTHER, ME, 10, 10), Blocked::Steal);
        assert_eq!(blocked_by(OTHER, ME, 11, 10), Blocked::Steal);
    }

    #[test]
    fn a_budget_of_zero_takes_the_lock_away_without_waiting_once() {
        assert_eq!(blocked_by(OTHER, ME, 0, 0), Blocked::Steal);
    }

    #[test]
    fn losing_the_race_to_a_free_lock_is_worth_retrying() {
        // The exchange can fail while reporting NOBODY: another CPU took it and
        // gave it back between our read and our write. That is not us, so it is
        // an ordinary wait, not a steal and not a nested write.
        assert_eq!(blocked_by(NOBODY, ME, 0, 10), Blocked::Wait);
    }

    #[test]
    fn no_real_cpu_can_be_mistaken_for_an_empty_lock() {
        // `cpu_id()` is a u8 everywhere in this kernel.
        assert!(NOBODY > u8::MAX as u32);
    }

    #[test]
    fn the_panic_budget_is_not_zero() {
        // A zero budget would make every contended panic write steal on the
        // first try, which is not a bounded wait, it is no lock at all.
        assert!(PANIC_SPIN_BUDGET > 0);
    }

    // -- the lock ----------------------------------------------------------

    #[test]
    fn a_fresh_lock_is_held_by_nobody() {
        let l = ConsoleLock::new();
        assert_eq!(l.owner(), NOBODY);
        assert_eq!(l.steals(), 0);
    }

    #[test]
    fn taking_a_free_lock_records_who_took_it() {
        let l = ConsoleLock::new();
        assert_eq!(l.try_acquire(ME), Some(Acquired::Taken));
        assert_eq!(l.owner(), ME);
    }

    #[test]
    fn a_best_effort_writer_gives_up_on_a_held_lock_and_changes_nothing() {
        let l = ConsoleLock::new();
        l.try_acquire(OTHER).unwrap();
        assert_eq!(l.try_acquire(ME), None);
        assert_eq!(l.owner(), OTHER);
    }

    #[test]
    fn a_best_effort_writer_that_re_enters_itself_gives_up_rather_than_recurse() {
        let l = ConsoleLock::new();
        l.try_acquire(ME).unwrap();
        assert_eq!(l.try_acquire(ME), None);
        assert_eq!(l.owner(), ME);
    }

    #[test]
    fn releasing_a_lock_we_took_hands_it_back() {
        let l = ConsoleLock::new();
        let how = l.try_acquire(ME).unwrap();
        l.release(how);
        assert_eq!(l.owner(), NOBODY);
    }

    // -- the panic path ----------------------------------------------------

    #[test]
    fn a_panic_write_on_a_free_lock_does_not_wait() {
        let l = ConsoleLock::new();
        let spins = Cell::new(0);
        assert_eq!(
            l.acquire_for_panic(ME, 10, bounded(&spins, 0)),
            Acquired::Taken
        );
        assert_eq!(spins.get(), 0);
        assert_eq!(l.owner(), ME);
    }

    #[test]
    fn a_panic_inside_our_own_console_write_gets_through_immediately() {
        // The scenario: `SerialWriter::write_str` holds the lock, its
        // `uart.write_str(s).unwrap()` panics, and the panic handler's first
        // act is a serial write. Before this it spun on itself for ever with
        // interrupts off, and the report never left the formatter.
        let l = ConsoleLock::new();
        l.try_acquire(ME).unwrap();
        let spins = Cell::new(0);
        assert_eq!(
            l.acquire_for_panic(ME, u32::MAX, bounded(&spins, 0)),
            Acquired::Nested
        );
        assert_eq!(spins.get(), 0, "waited for itself");
    }

    #[test]
    fn a_nested_panic_write_does_not_free_the_lock_the_outer_write_holds() {
        let l = ConsoleLock::new();
        let outer = l.try_acquire(ME).unwrap();
        let inner = l.acquire_for_panic(ME, 10, || {});
        l.release(inner);
        assert_eq!(l.owner(), ME, "the nested write gave away the outer ticket");
        l.release(outer);
        assert_eq!(l.owner(), NOBODY);
    }

    #[test]
    fn a_holder_that_never_comes_back_costs_the_budget_and_then_the_lock() {
        let l = ConsoleLock::new();
        l.try_acquire(OTHER).unwrap();
        let spins = Cell::new(0);
        assert_eq!(
            l.acquire_for_panic(ME, 7, bounded(&spins, 7)),
            Acquired::Stolen
        );
        assert_eq!(spins.get(), 7);
        assert_eq!(l.owner(), ME, "a stolen lock is ours");
        assert_eq!(l.steals(), 1);
    }

    #[test]
    fn a_holder_that_finishes_in_time_keeps_its_lock_honest() {
        let l = ConsoleLock::new();
        let held = l.try_acquire(OTHER).unwrap();
        let spins = Cell::new(0);
        let how = l.acquire_for_panic(ME, 1000, || {
            spins.set(spins.get() + 1);
            assert!(spins.get() <= 4, "the holder let go and we kept waiting");
            if spins.get() == 4 {
                l.release(held);
            }
        });
        assert_eq!(how, Acquired::Taken, "took it away from a live holder");
        assert_eq!(spins.get(), 4);
        assert_eq!(l.steals(), 0);
    }

    #[test]
    fn taking_a_lock_away_is_counted_so_it_can_be_reported() {
        let l = ConsoleLock::new();
        l.try_acquire(OTHER).unwrap();
        l.acquire_for_panic(ME, 0, || {});
        l.release(Acquired::Stolen);
        l.try_acquire(OTHER).unwrap();
        l.acquire_for_panic(ME, 0, || {});
        assert_eq!(l.steals(), 2);
    }

    #[test]
    fn an_ordinary_write_is_never_counted_as_a_steal() {
        let l = ConsoleLock::new();
        let how = l.acquire_for_panic(ME, 10, || {});
        l.release(how);
        let how = l.acquire_for_panic(ME, 10, || {});
        l.release(how);
        assert_eq!(l.steals(), 0);
    }

    #[test]
    fn only_a_nested_acquisition_keeps_the_lock_on_release() {
        assert!(Acquired::Taken.frees());
        assert!(Acquired::Stolen.frees());
        assert!(!Acquired::Nested.frees());
    }

    #[test]
    fn a_stolen_lock_is_released_like_any_other() {
        let l = ConsoleLock::new();
        l.try_acquire(OTHER).unwrap();
        let how = l.acquire_for_panic(ME, 0, || {});
        l.release(how);
        assert_eq!(l.owner(), NOBODY);
    }

    #[test]
    fn every_panic_write_returns_whoever_holds_the_lock() {
        // The property the panic path needs and `lock()` could not promise.
        for owner in [NOBODY, ME, OTHER, 0, u8::MAX as u32] {
            let l = ConsoleLock::new();
            if owner != NOBODY {
                l.try_acquire(owner).unwrap();
            }
            let spins = Cell::new(0);
            let how = l.acquire_for_panic(ME, 3, bounded(&spins, 3));
            assert!(spins.get() <= 3, "owner {owner} waited {}", spins.get());
            assert!(matches!(
                how,
                Acquired::Taken | Acquired::Nested | Acquired::Stolen
            ));
        }
    }
}
