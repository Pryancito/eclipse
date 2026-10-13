//! The word the syscall return reads instead of taking the thread's lock:
//! that it is never wrong, and that no mutator can forget it.
//!
//! [`ThreadInner::state`] is an answer derived from three fields, and what
//! makes its lock-free copy safe is not that every writing site remembers to
//! refresh it, but that the refresh lives in the [`Drop`] of
//! [`ThreadInnerGuard`], the only way to reach those fields. So each test here
//! mutates through a **production** path (or writes the field the way the
//! other tests in this file do, which is the other half of what the guard
//! protects) and then compares the published word against what the derivation
//! itself says with the lock held.
//!
//! What breaks if this breaks: `run_user` asks `state() ==
//! ThreadState::Dying` three times per turn. A word that says `Running` of a
//! dead thread is a thread that never leaves its loop; one that says `Dying`
//! of a live thread kills it.

extern crate std;

use super::*;
use crate::task::*;

fn a_thread() -> Arc<Thread> {
    let root = Job::root();
    let proc = Process::create(&root, "proc").expect("a process");
    Thread::create(&proc, "a thread").expect("a thread")
}

/// What the word says, read the way the return path reads it.
fn published(t: &Arc<Thread>) -> u32 {
    t.state_published.load(Ordering::Acquire)
}

/// What is true, derived under the lock from the same three fields.
fn truth(t: &Arc<Thread>) -> ThreadState {
    t.inner.lock().state()
}

fn assert_published_agrees(t: &Arc<Thread>, after: &str) {
    let (p, v) = (published(t), truth(t));
    assert_eq!(
        ThreadState::from_raw(p),
        Some(v),
        "after {after}: the word says {p:#x} and the truth is {v:?} -- a syscall return reads \
         the first"
    );
    assert_eq!(t.state(), v, "after {after}");
}

#[test]
fn a_new_thread_publishes_what_it_is() {
    let t = a_thread();
    assert_eq!(t.state(), ThreadState::New);
    assert_published_agrees(&t, "being born");
}

#[test]
fn change_state_publishes_through_the_guard() {
    let t = a_thread();
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    assert_eq!(t.state(), ThreadState::Running);
    assert_published_agrees(&t, "change_state(Running)");
}

/// The path that pays for this: `MarkBlocked` does it on every syscall that
/// sleeps, and `run_user` asks for the state right afterwards.
#[test]
fn set_blocked_publishes_and_unblocking_undoes_it() {
    let t = a_thread();
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    assert!(t.set_blocked(true), "a running thread can be blocked");
    assert_eq!(t.state(), ThreadState::Blocked);
    assert_published_agrees(&t, "set_blocked(true)");
    assert!(t.set_blocked(false));
    assert_eq!(t.state(), ThreadState::Running);
    assert_published_agrees(&t, "set_blocked(false)");
}

/// A `set_blocked` that changes nothing leaves through its guard **before**
/// touching the field, and the word has to stay what it was.
#[test]
fn a_set_blocked_that_changes_nothing_leaves_the_word_alone() {
    let t = a_thread();
    assert!(!t.set_blocked(true), "a `New` thread does not block");
    assert_eq!(t.state(), ThreadState::New);
    assert_published_agrees(&t, "a refused set_blocked");
}

#[test]
fn a_suspend_publishes_suspended_and_a_resume_undoes_it() {
    let t = a_thread();
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    t.suspend();
    assert_eq!(
        t.state(),
        ThreadState::Suspended,
        "a thread with a suspend token over it"
    );
    assert_published_agrees(&t, "suspend");
    t.resume();
    assert_eq!(t.state(), ThreadState::Running);
    assert_published_agrees(&t, "resume");
}

/// Two suspends need two resumes, and the word has to follow the count rather
/// than the first one in.
#[test]
fn two_suspends_need_two_resumes_and_the_word_follows_the_count() {
    let t = a_thread();
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    t.suspend();
    t.suspend();
    t.resume();
    assert_eq!(
        t.state(),
        ThreadState::Suspended,
        "one suspend token is left"
    );
    assert_published_agrees(&t, "the first of two resumes");
    t.resume();
    assert_eq!(t.state(), ThreadState::Running);
    assert_published_agrees(&t, "the second resume");
}

/// The third field the answer comes from: with no saved context, a suspended
/// thread reports what it was doing and not `Suspended`. It is the field
/// `wait_for_run` takes away on EVERY entry to user mode, so the word changes
/// twice per syscall without anybody touching `state`.
#[test]
fn taking_the_context_away_republishes_too() {
    let t = a_thread();
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    t.suspend();
    assert_eq!(t.state(), ThreadState::Suspended);
    t.lock_inner().context = None;
    assert_eq!(
        t.state(),
        ThreadState::Running,
        "with no context, the suspension does not hide what it was doing"
    );
    assert_published_agrees(&t, "taking its context away");
}

/// The question `run_user` asks three times per turn, through the path that
/// really sets `Dying`.
#[test]
fn a_kill_publishes_dying() {
    let t = a_thread();
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    t.kill();
    assert_eq!(
        t.state(),
        ThreadState::Dying,
        "the return loop would never leave"
    );
    assert_published_agrees(&t, "kill");
}

/// A specific state (`BlockedFutex` and company) is not lost on the way
/// through: the word is the whole state, not an "is it alive".
#[test]
fn every_specific_blocked_state_survives_the_round_trip() {
    let t = a_thread();
    for specific in [
        ThreadState::BlockedException,
        ThreadState::BlockedSleeping,
        ThreadState::BlockedFutex,
        ThreadState::BlockedPort,
        ThreadState::BlockedChannel,
        ThreadState::BlockedWaitOne,
        ThreadState::BlockedWaitMany,
        ThreadState::BlockedInterrupt,
        ThreadState::BlockedPager,
    ] {
        t.lock_inner().state = specific;
        assert_eq!(t.state(), specific, "{specific:?}");
        assert_published_agrees(&t, "a specific state");
    }
}

/// Every state, there and back through its number. A state missing from
/// [`ThreadState::from_raw`] sends `state()` down the slow path, which is
/// correct but is not this.
#[test]
fn from_raw_is_the_inverse_of_as_u32() {
    for state in [
        ThreadState::New,
        ThreadState::Running,
        ThreadState::Suspended,
        ThreadState::Blocked,
        ThreadState::Dying,
        ThreadState::Dead,
        ThreadState::BlockedException,
        ThreadState::BlockedSleeping,
        ThreadState::BlockedFutex,
        ThreadState::BlockedPort,
        ThreadState::BlockedChannel,
        ThreadState::BlockedWaitOne,
        ThreadState::BlockedWaitMany,
        ThreadState::BlockedInterrupt,
        ThreadState::BlockedPager,
    ] {
        assert_eq!(
            ThreadState::from_raw(state as u32),
            Some(state),
            "{state:?} does not come back from its number"
        );
    }
    // And nothing that is not a state slips through as one.
    for raw in [6u32, 7, 0x102, 0x104, 0xA03, u32::MAX] {
        assert_eq!(ThreadState::from_raw(raw), None, "{raw:#x}");
    }
}

/// A word that is no state sends `state()` to take the lock, which is the
/// right answer. It cannot happen; that is why the way out had to be correct
/// and not a panic or a defaulted `New`.
#[test]
fn a_word_that_is_not_a_state_falls_back_to_the_lock() {
    let t = a_thread();
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    t.state_published.store(0xDEAD_BEEF, Ordering::Release);
    assert_eq!(
        t.state(),
        ThreadState::Running,
        "the slow path has to give the truth"
    );
    // And it leaves it mended on the way, because the slow path goes through
    // the guard.
    assert_published_agrees(&t, "the slow path");
}

/// The refresh is in `Drop`, which also runs while unwinding: a mutator that
/// changes the state and then fails cannot leave the old word behind.
#[test]
fn a_panic_inside_the_guard_still_publishes() {
    let t = a_thread();
    let t2 = t.clone();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut inner = t2.lock_inner();
        inner.state = ThreadState::Dying;
        panic!("a mutator that fails after writing");
    }));
    assert!(panicked.is_err(), "the test needs the panic to happen");
    assert_eq!(
        t.state(),
        ThreadState::Dying,
        "unwinding runs the guard's Drop"
    );
    assert_published_agrees(&t, "a panicking mutator");
}

/// Each thread publishes its own: the word is in the thread, not in the
/// process.
#[test]
fn two_threads_of_the_same_process_publish_separately() {
    let root = Job::root();
    let proc = Process::create(&root, "proc").expect("a process");
    let a = Thread::create(&proc, "a").expect("a thread");
    let b = Thread::create(&proc, "b").expect("another thread");
    a.lock_inner().change_state(ThreadState::Running, &a.base);
    b.kill();
    assert_eq!(a.state(), ThreadState::Running);
    assert_eq!(b.state(), ThreadState::Dying);
    assert_published_agrees(&a, "the live sibling");
    assert_published_agrees(&b, "the dead sibling");
}

/// `terminate` publishes `Dead` through a path nothing here covered, and it
/// took the lock on its own: it was one of the two sites that, spelled across
/// two lines, escaped the mechanical conversion of the one-line form. What
/// found them was grepping for `.inner`, not the mutation batch.
#[test]
fn terminate_publishes_dead() {
    let t = a_thread();
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    t.terminate();
    assert_eq!(
        t.state(),
        ThreadState::Dead,
        "a dead thread advertising itself as running never leaves the list"
    );
    assert_published_agrees(&t, "terminate");
}

/// And the other one: `start` publishes `Running`. The transition is called
/// here rather than `start` itself, because `start` is reached through
/// `Process::start` and an executor spawn; the real path is covered by
/// `task::thread::tests::start`, which asserts `state() == Running` after a
/// real start and is what would catch `start` going round the guard again.
#[test]
fn the_transition_start_makes_publishes_running() {
    let t = a_thread();
    assert_eq!(t.state(), ThreadState::New);
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    assert_eq!(t.state(), ThreadState::Running);
    assert_published_agrees(&t, "start's transition");
}
