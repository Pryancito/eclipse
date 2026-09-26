//! A two-channel kernel mutex.
//!
//! **What this used to be.** One `UnsafeCell<T>` behind *two* independent
//! `AtomicBool`s, one per [`LockChannel`], each handing out `&mut T`. So
//! `lock(Normal)` and `lock(Interrupt)` both succeeded at the same time, on the
//! same data, and the two guards were two live `&mut` to one place — undefined
//! behaviour by construction, not by race. It also took no `push_off`, so the
//! channel named `Interrupt` described exactly the way in: a holder on the
//! normal channel is interruptible, and the handler takes the other channel and
//! gets the second `&mut` while the first is half-way through a write.
//!
//! Nothing in this tree calls it — `lock` exports it and no crate uses it — so
//! the hole never fired. It is also not an MCS lock: there is no queue and
//! never was, so it has none of MCS's fairness; the name is inherited and left
//! alone rather than churned through a public API for nothing.
//!
//! **What it is now.** One word. A guard excludes every channel, not just its
//! own, and the channel travels on the guard as a *label* — which side of the
//! machine took it, for the `Display` and for a holder report — and not as a
//! second lock. Acquisition brackets `push_off`/`pop_off` like every other
//! mutex in this crate, so the lock counts towards `lock_depth()` (which
//! `zcore::oops` reads before it dares run recovery from inside a panic) and an
//! interrupt cannot land inside the critical section at all. And the spin drains
//! this CPU's own shootdown queue and self-reports a wedge, which it also did
//! not: a spinner with IRQs off and no pump is an ack black hole, and this was
//! the one flavour missing from the list in `deadlock::pump`'s own comment.
//!
//! If what a future caller wants is genuinely two independent critical sections,
//! one for the IRQ side and one for the normal side, that is **two locks over
//! two values** — two `MCSLock`s — and not one lock with two doors into one
//! value. There is no arrangement of flags that makes the second thing sound.

use core::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::deadlock::report_deadlock;
use crate::interrupt::{pop_off, push_off};

/// Which side of the machine holds the lock. A label on the guard; both
/// channels exclude each other, because there is one `T`.
#[repr(usize)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LockChannel {
    Normal = 0,
    Interrupt = 1,
}

/// The `holder` word when nobody holds the lock. A channel is stored as its
/// discriminant **plus one**, so the free state is a zero word and `new` stays
/// `const`.
const NOBODY: usize = 0;

impl LockChannel {
    /// What this channel stores in the `holder` word.
    const fn token(self) -> usize {
        self as usize + 1
    }

    /// The channel a `holder` word names, or `None` when it is free.
    fn of_token(token: usize) -> Option<Self> {
        match token {
            1 => Some(LockChannel::Normal),
            2 => Some(LockChannel::Interrupt),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LockChannel::Normal => "normal",
            LockChannel::Interrupt => "interrupt",
        }
    }
}

pub struct MCSLock<T: ?Sized> {
    /// [`NOBODY`], or the holder's [`LockChannel::token`].
    pub(crate) holder: AtomicUsize,
    data: UnsafeCell<T>,
}

/// An RAII implementation of a “scoped lock” of a mutex.
/// When this structure is dropped (falls out of scope),
/// the lock will be unlocked.
///
pub struct MCSLockGuard<'a, T: ?Sized + 'a> {
    mcslock: &'a MCSLock<T>,
    data: &'a mut T,
    channel: LockChannel,
}

unsafe impl<T: ?Sized + Send> Sync for MCSLock<T> {}
unsafe impl<T: ?Sized + Send> Send for MCSLock<T> {}

impl<T> MCSLock<T> {
    #[inline(always)]
    pub const fn new(data: T) -> Self {
        MCSLock {
            holder: AtomicUsize::new(NOBODY),
            data: UnsafeCell::new(data),
        }
    }

    #[inline(always)]
    pub fn into_inner(self) -> T {
        // We know statically that there are no outstanding references to
        // `self` so there's no need to lock.
        let MCSLock { data, .. } = self;
        data.into_inner()
    }

    #[inline(always)]
    pub fn as_mut_ptr(&self) -> *mut T {
        self.data.get()
    }
}

impl<T: ?Sized> MCSLock<T> {
    /// One turn of this lock's waiting: pause, count it, and act on the count.
    /// The same cadence the other flavours keep — see `spin::SpinMutex`.
    #[inline(always)]
    fn wait_turn(&self, spins: &mut u64, caller: &'static core::panic::Location<'static>) {
        core::hint::spin_loop();
        *spins += 1;
        // Spinning with IRQs off makes this CPU deaf to TLB-shootdown IPIs, and
        // a peer may be spin-waiting for our ack.
        if *spins & 511 == 0 {
            crate::deadlock::spin_pump();
        }
        if *spins == crate::deadlock::deadlock_spins() {
            report_deadlock(caller.file(), caller.line());
        }
    }

    #[track_caller]
    #[inline(always)]
    pub fn lock(&self, channel: LockChannel) -> MCSLockGuard<'_, T> {
        push_off();
        let caller = core::panic::Location::caller();
        let mut spins: u64 = 0;
        while self
            .holder
            .compare_exchange_weak(
                NOBODY,
                channel.token(),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            // Count the lost attempt itself and then wait until the lock looks
            // free, so a waiter losing the handover race still reaches the pump
            // and the wedge threshold: both loops feed one counter.
            self.wait_turn(&mut spins, caller);
            // The read-only wait is the one line here that no test pins, and on
            // purpose: deleting it leaves the lock correct. It is test-and-set
            // instead of a bare compare-exchange retry, so a waiter reads the
            // cacheline shared rather than taking it exclusive on every turn —
            // a cost, not a rule, and the outer loop still pumps and still
            // names a wedge without it. What it must not do is skip the
            // counter, which is why it calls the same `wait_turn`.
            while self.is_held() {
                self.wait_turn(&mut spins, caller);
            }
        }
        MCSLockGuard {
            mcslock: self,
            data: unsafe { &mut *self.data.get() },
            channel,
        }
    }

    #[inline(always)]
    pub fn try_lock(&self, channel: LockChannel) -> Option<MCSLockGuard<'_, T>> {
        push_off();
        if self
            .holder
            .compare_exchange(
                NOBODY,
                channel.token(),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            Some(MCSLockGuard {
                mcslock: self,
                data: unsafe { &mut *self.data.get() },
                channel,
            })
        } else {
            // Give the interrupt level back: a refused attempt holds nothing,
            // and leaving it taken is how a `pop_off` underflow is manufactured
            // several frames away from the mistake.
            pop_off();
            None
        }
    }

    #[inline(always)]
    pub fn get_mut(&mut self) -> &mut T {
        // We know statically that there are no other references to `self`, so
        // there's no need to lock the inner mutex.
        unsafe { &mut *self.data.get() }
    }

    /// Whether the lock is held **by that channel**.
    #[inline(always)]
    pub fn is_locked(&self, channel: LockChannel) -> bool {
        self.holder.load(Ordering::Relaxed) == channel.token()
    }

    /// Whether the lock is held at all.
    ///
    /// The question the waiting loop has to ask, and the one the old
    /// per-channel `is_locked` could not: a lock held by the other channel is
    /// held.
    #[inline(always)]
    pub fn is_held(&self) -> bool {
        self.holder.load(Ordering::Relaxed) != NOBODY
    }

    /// The channel holding the lock, or `None`.
    #[inline(always)]
    pub fn held_by(&self) -> Option<LockChannel> {
        LockChannel::of_token(self.holder.load(Ordering::Relaxed))
    }
}

impl<'a, T: ?Sized> MCSLockGuard<'a, T> {
    /// The channel this guard was taken on.
    pub fn channel(&self) -> LockChannel {
        self.channel
    }
}

impl<'a, T: ?Sized + fmt::Display> fmt::Display for MCSLockGuard<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<'a, T: ?Sized> Deref for MCSLockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.data
    }
}

impl<'a, T: ?Sized> DerefMut for MCSLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.data
    }
}

impl<'a, T: ?Sized> Drop for MCSLockGuard<'a, T> {
    /// The dropping of the MutexGuard will release the lock it was created from.
    fn drop(&mut self) {
        // Free the lock first and restore the interrupt level after, because
        // `pop_off` may enable interrupts and the handler is entitled to find
        // this lock takeable.
        self.mcslock.holder.store(NOBODY, Ordering::Release);
        pop_off();
    }
}

impl<T: ?Sized> fmt::Display for MCSLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.held_by() {
            Some(channel) => write!(f, "MCSLock{{held by the {} channel}}", channel.as_str()),
            None => write!(f, "MCSLock{{free}}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupt::{intr_get_for_test, intr_on_for_test, lock_depth};
    use crate::tests::on_a_cpu;

    /// The state the old two-word version could reach and this one cannot: two
    /// guards at once. Written as the question every other test rests on.
    #[test]
    fn a_lock_one_channel_holds_is_not_free_for_the_other() {
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            let held = lock.lock(LockChannel::Normal);
            assert!(
                lock.try_lock(LockChannel::Interrupt).is_none(),
                "two guards over one T is two live &mut T, whatever the channels are called"
            );
            assert!(lock.try_lock(LockChannel::Normal).is_none());
            drop(held);
            assert!(lock.try_lock(LockChannel::Interrupt).is_some());
        });
    }

    /// The blocking half of the same question. `try_lock` can ask it without
    /// waiting; `lock` cannot, so it needs a second CPU — and it is `lock` that
    /// every caller uses.
    ///
    /// The peer's channel is the whole point of having two of these, because
    /// the two ways exclusion can be got wrong are two different bugs. Letting
    /// the **other** channel in hands out two live `&mut T` over one value.
    /// Letting the **same** channel in is re-entrancy, and this lock holds
    /// interrupts off for its whole critical section, so the only way one CPU
    /// comes back round to a lock it already owns is a fault taken inside that
    /// section: not contention, a wedge. A compare-exchange that expects
    /// "free, or else my own token" passes the first test and fails this one.
    fn a_holder_keeps_a_peer_out(
        lock: &'static MCSLock<u32>,
        held_by: LockChannel,
        peer_uses: LockChannel,
    ) {
        // The peer below is a real spinner, and a spinner in this crate is
        // never only a spinner: every 512 turns it calls the process-wide
        // `spin_pump`, and at the threshold it calls the process-wide deadlock
        // hook. `rwlock.rs`'s discipline tests install counting versions of
        // both and assert their exact counts, so a spinner running beside them
        // adds turns to somebody else's ledger. This is the lock those tests
        // take for the same reason.
        let _hooks = crate::deadlock::hook_test_lock();
        on_a_cpu(|| {
            use std::sync::atomic::Ordering as O;
            GOT_IN.store(false, O::SeqCst);
            WAITING.store(false, O::SeqCst);

            let held = lock.lock(held_by);
            let peer = std::thread::spawn(move || {
                on_a_cpu(|| {
                    WAITING.store(true, O::SeqCst);
                    let g = lock.lock(peer_uses);
                    GOT_IN.store(true, O::SeqCst);
                    drop(g);
                });
            });
            // Give the peer time to be inside `lock`. A short window is
            // enough: a lock that lets this peer in lets it in at its very
            // first compare-exchange, so a longer one buys no sensitivity and
            // only spins a core for nothing.
            while !WAITING.load(O::SeqCst) {
                std::thread::yield_now();
            }
            for _ in 0..64 {
                std::thread::yield_now();
            }
            assert!(
                !GOT_IN.load(O::SeqCst),
                "the {} channel walked into a critical section the {} channel \
                 is holding: that is two live &mut T over one value",
                peer_uses.as_str(),
                held_by.as_str()
            );
            drop(held);
            // Bounded, and then assert: a release that never happened must name
            // this test rather than leave the suite waiting on a join forever.
            for _ in 0..200_000 {
                if GOT_IN.load(O::SeqCst) {
                    break;
                }
                std::thread::yield_now();
            }
            assert!(
                GOT_IN.load(O::SeqCst),
                "and once we let go it has to get in, or the lock is a leak"
            );
            peer.join().unwrap();
        });
    }

    static GOT_IN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static WAITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    #[test]
    fn lock_makes_the_other_channel_wait_and_not_walk_in() {
        static LOCK: MCSLock<u32> = MCSLock::new(0);
        a_holder_keeps_a_peer_out(&LOCK, LockChannel::Normal, LockChannel::Interrupt);
    }

    #[test]
    fn lock_makes_a_second_taker_on_the_same_channel_wait_too() {
        static LOCK: MCSLock<u32> = MCSLock::new(0);
        a_holder_keeps_a_peer_out(&LOCK, LockChannel::Normal, LockChannel::Normal);
    }

    #[test]
    fn a_lock_nobody_holds_reads_as_free_on_both_channels() {
        on_a_cpu(|| {
            let lock = MCSLock::new(7u32);
            assert!(!lock.is_held());
            assert!(!lock.is_locked(LockChannel::Normal));
            assert!(!lock.is_locked(LockChannel::Interrupt));
            assert_eq!(lock.held_by(), None);
        });
    }

    #[test]
    fn a_lock_says_which_channel_took_it() {
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            let held = lock.lock(LockChannel::Interrupt);
            assert_eq!(lock.held_by(), Some(LockChannel::Interrupt));
            assert!(lock.is_locked(LockChannel::Interrupt));
            assert!(
                !lock.is_locked(LockChannel::Normal),
                "the channel is a label, so it has to name the one that actually took it"
            );
            assert_eq!(held.channel(), LockChannel::Interrupt);
        });
    }

    #[test]
    fn a_lock_either_channel_holds_reads_as_held() {
        // The question the waiting loop asks, and the one a per-channel answer
        // gets wrong: a lock held by the *other* channel is held.
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            for channel in [LockChannel::Normal, LockChannel::Interrupt] {
                let g = lock
                    .try_lock(channel)
                    .expect("the previous guard did not release");
                assert!(lock.is_held());
                drop(g);
                assert!(!lock.is_held());
            }
        });
    }

    #[test]
    fn the_lock_says_in_words_whether_it_is_free_and_who_has_it() {
        // Printed by the deadlock banner, from a machine whose memory is
        // already suspect, so it has to distinguish the three states.
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            assert_eq!(std::format!("{}", lock), "MCSLock{free}");
            {
                let _g = lock.lock(LockChannel::Normal);
                assert_eq!(
                    std::format!("{}", lock),
                    "MCSLock{held by the normal channel}"
                );
            }
            let _g = lock
                .try_lock(LockChannel::Interrupt)
                .expect("the first guard did not release");
            assert_eq!(
                std::format!("{}", lock),
                "MCSLock{held by the interrupt channel}"
            );
        });
    }

    #[test]
    fn the_two_channels_are_two_different_labels() {
        // They are stored, so they have to be distinguishable, and neither may
        // collide with the free word.
        assert_ne!(LockChannel::Normal.token(), LockChannel::Interrupt.token());
        assert_ne!(LockChannel::Normal.token(), NOBODY);
        assert_ne!(LockChannel::Interrupt.token(), NOBODY);
        assert_eq!(
            LockChannel::of_token(LockChannel::Normal.token()),
            Some(LockChannel::Normal)
        );
        assert_eq!(
            LockChannel::of_token(LockChannel::Interrupt.token()),
            Some(LockChannel::Interrupt)
        );
    }

    #[test]
    fn a_word_that_names_no_channel_names_no_holder() {
        // The word is read by the `Display` a deadlock banner prints, from a
        // machine whose memory is already suspect.
        assert_eq!(LockChannel::of_token(0), None);
        assert_eq!(LockChannel::of_token(3), None);
        assert_eq!(LockChannel::of_token(usize::MAX), None);
    }

    #[test]
    fn the_guard_hands_the_data_back_and_writes_through() {
        on_a_cpu(|| {
            let lock = MCSLock::new(5u32);
            {
                let mut g = lock.lock(LockChannel::Normal);
                assert_eq!(*g, 5);
                *g = 9;
            }
            assert_eq!(
                *lock
                    .try_lock(LockChannel::Interrupt)
                    .expect("the first guard did not release"),
                9
            );
        });
    }

    #[test]
    fn a_released_lock_can_be_taken_by_the_other_channel_and_back() {
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            for channel in [
                LockChannel::Normal,
                LockChannel::Interrupt,
                LockChannel::Normal,
            ] {
                // `try_lock`, not `lock`: the claim is that the lock is free by
                // now, and a release that did not happen must fail this test by
                // name rather than park the suite in a spin that names nothing.
                let g = lock
                    .try_lock(channel)
                    .expect("the previous guard did not release");
                assert_eq!(lock.held_by(), Some(channel));
                drop(g);
                assert!(!lock.is_held());
            }
        });
    }

    // ── the interrupt discipline the other mutexes keep ─────────────────────

    #[test]
    fn a_guard_leaves_the_slot_as_it_found_it() {
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            let depth = lock_depth();
            {
                let _g = lock.lock(LockChannel::Normal);
                assert_eq!(
                    lock_depth(),
                    depth + 1,
                    "a lock that does not count towards the depth is invisible to \
                     `oops`, which tests it before running recovery"
                );
            }
            assert_eq!(lock_depth(), depth);
        });
    }

    #[test]
    fn nested_locks_nest_the_depth() {
        on_a_cpu(|| {
            let outer = MCSLock::new(0u32);
            let inner = MCSLock::new(0u32);
            let depth = lock_depth();
            let a = outer.lock(LockChannel::Normal);
            let b = inner.lock(LockChannel::Interrupt);
            assert_eq!(lock_depth(), depth + 2);
            drop(b);
            assert_eq!(lock_depth(), depth + 1);
            drop(a);
            assert_eq!(lock_depth(), depth);
        });
    }

    #[test]
    fn a_refused_try_lock_gives_its_level_back() {
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            let held = lock.lock(LockChannel::Normal);
            let depth = lock_depth();
            assert!(lock.try_lock(LockChannel::Interrupt).is_none());
            assert_eq!(
                lock_depth(),
                depth,
                "a refused attempt holds nothing, and a level left taken here \
                 becomes a `pop_off` underflow several frames away"
            );
            drop(held);
        });
    }

    #[test]
    fn a_taken_try_lock_keeps_one_level_until_it_drops() {
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            let depth = lock_depth();
            let g = lock.try_lock(LockChannel::Normal).unwrap();
            assert_eq!(lock_depth(), depth + 1);
            drop(g);
            assert_eq!(lock_depth(), depth);
        });
    }

    #[test]
    fn a_guard_holds_the_interrupts_off_and_gives_them_back() {
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            intr_on_for_test();
            assert!(intr_get_for_test());
            {
                let _g = lock.lock(LockChannel::Normal);
                assert!(
                    !intr_get_for_test(),
                    "a critical section an interrupt can land in is the hole the \
                     `Interrupt` channel used to be the way through"
                );
            }
            assert!(intr_get_for_test());
        });
    }

    #[test]
    fn interrupts_that_were_off_stay_off() {
        on_a_cpu(|| {
            let lock = MCSLock::new(0u32);
            crate::interrupt::intr_off_for_test();
            {
                let _g = lock.lock(LockChannel::Interrupt);
                assert!(!intr_get_for_test());
            }
            assert!(
                !intr_get_for_test(),
                "the guard restores the caller's state, it does not decide it"
            );
            intr_on_for_test();
        });
    }

    #[test]
    fn the_lock_is_free_again_before_the_interrupts_come_back() {
        // The order inside `drop`, seen from the only place it is visible: an
        // interrupt arriving the instant IRQs are re-enabled. It is entitled to
        // find this lock takeable, and would deadlock against a guard that had
        // not let go yet. `pop_off` re-enables, so the release has to come
        // first.
        static LOCK: MCSLock<u32> = MCSLock::new(0);
        static FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        static WAS_FREE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        use std::sync::atomic::Ordering as O;

        fn handler() {
            FIRED.store(true, O::SeqCst);
            WAS_FREE.store(!LOCK.is_held(), O::SeqCst);
        }

        on_a_cpu(|| {
            FIRED.store(false, O::SeqCst);
            WAS_FREE.store(false, O::SeqCst);
            let g = LOCK.lock(LockChannel::Normal);
            crate::interrupt::arm_irq_on_enable_for_test(handler);
            drop(g);
            crate::interrupt::disarm_irq_on_enable_for_test();
            assert!(
                FIRED.load(O::SeqCst),
                "the guard has to re-enable interrupts on the way out, or nothing \
                 below was observed at all"
            );
            assert!(
                WAS_FREE.load(O::SeqCst),
                "an interrupt that lands the moment the guard re-enables found the \
                 lock still held: on hardware that handler now spins on a lock \
                 whose holder has already gone"
            );
            assert!(intr_get_for_test());
        });
    }
}
