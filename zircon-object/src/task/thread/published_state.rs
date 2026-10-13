//! The thread's state, published outside its lock.
//!
//! # What was wrong
//!
//! [`Thread::state`] is not the read of a field: it is the answer derived from
//! three ([`ThreadInner::state`] looks at `suspend_count`, at whether there is
//! a saved user context, and at the `state` field), and asking it meant taking
//! [`Thread::inner`] — the lock that also guards the user context, the
//! exception in flight, the waker and the killer.
//!
//! The loop in `loader/src/linux.rs::run_user`, which is the exit path of
//! EVERY syscall of EVERY Linux process, asks that question **three times per
//! turn**: on return from `wait_for_run`, after looking at signals, and after
//! handling the trap. Measured on the host
//! (`task::thread::return_path_benches`):
//!
//! | row | ns |
//! |---|---|
//! | one question | 8.96 |
//! | the lock alone | 8.63 |
//! | **the answer alone, with the lock already held** | **0.64** |
//! | the shaped control | 0.35 |
//! | **the three questions of one turn** | **25.68** |
//!
//! That is: of the 25.7 ns a syscall paid for the three questions, less than
//! one was answering them. And with other threads looking at the same thread
//! (`/proc/<pid>/task`, signal delivery, `zx_object_get_info`), one question
//! went from 8.96 to 110 / 731 / 1603 ns with one / two / three of them.
//! Those contended figures are saturation points of a shared four-core
//! container, not predictions; what they establish is that the cost **was the
//! lock**, since nothing else on this path grows with the number of threads
//! looking.
//!
//! Afterwards: 1.24 ns for one question, 1.82 for the three, and 2.7 / 2.8 /
//! 1.4 with one / two / three others — a curve against a flat line, which is
//! what says the cost was the lock and not something that merely correlated
//! with it.
//!
//! # The fix, and where the invariant lives
//!
//! The derived answer is published in an `AtomicU32` that [`Thread::state`]
//! reads without the lock.
//!
//! What makes such a mirror safe is not a promise kept at each writing site,
//! but that there is no way to write without keeping it. And writing here is
//! very easy: the three fields the answer comes from are mutated at eleven
//! production sites inside `thread.rs` (`change_state` at six of them,
//! `set_blocked`, `suspend`/`resume`, and the `context.take()` /
//! `context = Some(..)` of every entry to and exit from user mode) **and
//! directly by the tests**, which do `inner.state = ...` to build the case
//! they want to prove.
//!
//! So the refresh is not in the mutators: it is in the [`Drop`] of
//! [`ThreadInnerGuard`], the only way to reach those fields. A new mutator —
//! or a test — cannot forget something it does not do. It is the same lesson
//! as this errand's seventh batch (`linux_object::thread::LinuxThreadExt`):
//! **when you cannot encapsulate the field, encapsulate the access.**
//!
//! Two of the eleven sites spelled the lock across two lines
//! (`self.inner\n.lock()`), which is how `start` and `terminate` — the
//! transitions to `Running` and to `Dead` — survived a mechanical conversion
//! of the one-line form. What found them was grepping for `.inner`, not the
//! mutation batch: a mutation of a site nothing reaches is not a mutant that
//! survives, it is a site with no test.
//!
//! A struct's fields are dropped AFTER the body of its `Drop::drop`, so the
//! publication happens with the lock still held, and a panic inside the
//! critical section publishes too, on the way out.
//!
//! # What a reader without the lock can see
//!
//! A value one store old; never one that was never true. That is the same
//! window Linux has with `TIF_SIGPENDING`, and the one the old code had:
//! `run_user` asks on the syscall return either way, so the only thing that
//! changed is where it reads from.
//!
//! # And if the word is not a state
//!
//! [`ThreadState::from_raw`] answers `None` for a number that is no state at
//! all, and then [`Thread::state`] **takes the lock and asks**. It cannot
//! happen (the word is only ever written from an `as u32` of a real state),
//! and that is exactly why the way out had to be a correct answer rather than
//! a panic or a defaulted `New`: if it ever does happen, the memory is
//! corrupt, and the slow path gives the right answer. A new state that
//! somebody adds and forgets to list in `from_raw` falls through there too: it
//! ends up slow, not wrong.
//!
//! # The two mutants that survive, and why they are equivalent
//!
//! Of fifteen deliberate breakages, twelve fall to a named test. The two that
//! do not change no answer any test can ask for:
//!
//! * relaxing the load's `Acquire` to `Relaxed`. The ordering is what pairs
//!   with the `Release` of the publication, and on x86 both compile to the
//!   same instruction; what it buys is on a weaker machine (this kernel also
//!   builds for aarch64 and riscv64), where a reader could otherwise see the
//!   word without the writes that preceded it. No host test can observe it.
//! * making `state()` take the lock even when the word IS a state. That is
//!   the old behaviour: the same answer, only paying what this module exists
//!   to stop paying. A test that caught it would be a test of the benches.

use super::*;
use core::ops::{Deref, DerefMut};
use kernel_hal::sync::MutexGuard;

impl ThreadState {
    /// The inverse of `as u32`, or `None` if the number is no state.
    ///
    /// See the module notes: the `None` is a correct path, not an error.
    pub(super) fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::New,
            1 => Self::Running,
            2 => Self::Suspended,
            3 => Self::Blocked,
            4 => Self::Dying,
            5 => Self::Dead,
            0x103 => Self::BlockedException,
            0x203 => Self::BlockedSleeping,
            0x303 => Self::BlockedFutex,
            0x403 => Self::BlockedPort,
            0x503 => Self::BlockedChannel,
            0x603 => Self::BlockedWaitOne,
            0x703 => Self::BlockedWaitMany,
            0x803 => Self::BlockedInterrupt,
            0x903 => Self::BlockedPager,
            _ => return None,
        })
    }
}

/// The thread's lock, which on release publishes the derived state.
///
/// Obtained with [`Thread::lock_inner`] and used as the `MutexGuard` it wraps:
/// everything that used to say `self.inner.lock()` still reads the same. See
/// the module notes for why the refresh lives here.
pub(super) struct ThreadInnerGuard<'a> {
    guard: MutexGuard<'a, ThreadInner>,
    published: &'a AtomicU32,
}

impl Deref for ThreadInnerGuard<'_> {
    type Target = ThreadInner;
    fn deref(&self) -> &ThreadInner {
        &self.guard
    }
}

impl DerefMut for ThreadInnerGuard<'_> {
    fn deref_mut(&mut self) -> &mut ThreadInner {
        &mut self.guard
    }
}

impl Drop for ThreadInnerGuard<'_> {
    fn drop(&mut self) {
        // Still holding the lock: `self.guard` is a field, and fields are
        // dropped after this body runs.
        self.published
            .store(self.guard.state() as u32, Ordering::Release);
    }
}

impl Thread {
    /// Takes the thread's lock. Releasing it publishes the derived state,
    /// which is what [`Thread::state`] reads.
    pub(super) fn lock_inner(&self) -> ThreadInnerGuard<'_> {
        ThreadInnerGuard {
            guard: self.inner.lock(),
            published: &self.state_published,
        }
    }

    /// Get the thread state, without taking its lock.
    ///
    /// This is on the return path of every syscall, three times. See the
    /// module notes.
    pub fn state(&self) -> ThreadState {
        match ThreadState::from_raw(self.state_published.load(Ordering::Acquire)) {
            Some(state) => state,
            // Cannot happen; if it does, the slow path gives the right answer.
            None => self.lock_inner().state(),
        }
    }
}
