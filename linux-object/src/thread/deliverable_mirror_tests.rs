//! The word the return to user mode reads instead of taking the thread's
//! lock: that it is never wrong, and that no mutator can forget it.
//!
//! What makes [`LinuxThreadExt::deliverable`] safe is not a promise kept at
//! each call site -- `signals` is a public field with writers in three
//! files -- but that the refresh lives in [`LinuxThreadGuard::drop`], the
//! only way to reach the state. These tests mutate through **production**
//! paths and then compare the published word against what `handle_signal`
//! would actually do, which is the only comparison that matters: a word
//! that says "nothing" while a signal is deliverable is a signal that never
//! arrives.
extern crate std;

use super::*;
use crate::process::ProcessExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

fn a_thread(pid: u64) -> Arc<Thread> {
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap();
    Thread::create_linux(&proc).unwrap()
}

fn set_of(sig: Signal) -> Sigset {
    Sigset::new(1 << (sig as u64 - 1))
}

/// The published word, read the way the return path reads it.
fn published(thread: &Arc<Thread>) -> u64 {
    thread
        .ext()
        .downcast_ref::<LinuxThreadExt>()
        .unwrap()
        .deliverable()
}

/// What is true, computed under the lock from the same two fields
/// `handle_signal` consults.
fn truth(thread: &Arc<Thread>) -> u64 {
    thread.lock_linux().deliverable_signals()
}

fn assert_published_agrees(thread: &Arc<Thread>, after: &str) {
    let (p, t) = (published(thread), truth(thread));
    assert_eq!(
        p, t,
        "after {after}: the published word is {p:#x} and the truth is {t:#x} -- a syscall \
         return reads the first and would skip the signal"
    );
}

#[test]
fn a_new_thread_publishes_nothing_to_deliver() {
    let thread = a_thread(79_001);
    assert_eq!(published(&thread), 0);
    assert!(!thread.has_deliverable_signal());
    assert_published_agrees(&thread, "creation");
}

#[test]
fn a_queued_signal_is_published_by_dropping_the_guard() {
    let thread = a_thread(79_002);
    thread.lock_linux().queue_signal(Signal::SIGINT, None);
    assert!(
        thread.has_deliverable_signal(),
        "the return path would not even look, and the Ctrl-C would be lost"
    );
    assert_published_agrees(&thread, "queue_signal");
}

/// The word is only read *outside* the guard, so what matters is the value
/// after the drop. Taking the lock again is what the return path does next,
/// and it must still find the signal.
#[test]
fn what_the_word_sends_us_to_look_for_is_there() {
    let thread = a_thread(79_003);
    thread.lock_linux().queue_signal(Signal::SIGINT, None);
    assert!(thread.has_deliverable_signal());
    let got = thread.lock_linux().handle_signal();
    assert_eq!(
        got.map(|(s, _, _)| s),
        Some(Signal::SIGINT),
        "the word promised a signal and the lock found none"
    );
    assert_eq!(
        published(&thread),
        0,
        "delivering the last one has to unpublish it"
    );
}

#[test]
fn a_blocked_signal_is_not_published_and_unblocking_publishes_it() {
    let thread = a_thread(79_004);
    thread.lock_linux().set_signal_mask(set_of(Signal::SIGUSR1));
    thread.lock_linux().queue_signal(Signal::SIGUSR1, None);
    assert!(
        !thread.has_deliverable_signal(),
        "a blocked signal is pending, not deliverable, and `handle_signal` skips it"
    );
    assert_published_agrees(&thread, "a blocked signal");
    // The mask is the other half of the answer: changing it changes what is
    // deliverable without touching `signals` at all.
    thread.lock_linux().set_signal_mask(Sigset::empty());
    assert!(
        thread.has_deliverable_signal(),
        "unblocking is what makes it deliverable, and the word has to notice"
    );
    assert_published_agrees(&thread, "set_signal_mask");
}

#[test]
fn taking_the_siginfo_of_the_last_signal_unpublishes_it() {
    let thread = a_thread(79_005);
    thread.lock_linux().queue_signal(Signal::SIGTERM, None);
    thread.lock_linux().take_siginfo(Signal::SIGTERM);
    assert_eq!(published(&thread), 0);
    assert_published_agrees(&thread, "take_siginfo");
}

/// Two signals, one delivered: the word is a set, not a flag.
#[test]
fn delivering_one_of_two_leaves_the_other_published() {
    let thread = a_thread(79_006);
    {
        let mut lt = thread.lock_linux();
        lt.queue_signal(Signal::SIGINT, None);
        lt.queue_signal(Signal::SIGTERM, None);
    }
    thread.lock_linux().handle_signal().expect("one of the two");
    assert_ne!(
        published(&thread),
        0,
        "the other signal is still deliverable and the return path has to look again"
    );
    assert_published_agrees(&thread, "delivering one of two");
}

/// A mutation through a path that is not in this file at all: the process's
/// own signal-send, which writes `signals` directly. It cannot forget the
/// refresh, because it does not do the refresh -- the guard does.
#[test]
fn a_signal_sent_through_the_process_is_published_too() {
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 79_007).unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    crate::process::send_signal_to_process(proc.id() as usize, Signal::SIGINT).unwrap();
    assert!(
        thread.has_deliverable_signal(),
        "`kill` reached the thread's set through a path that never names the word"
    );
    assert_published_agrees(&thread, "send_signal_to_process");
}

/// The guard republishes on the way out even when the body panics: the
/// refresh is in `Drop`, which unwinds. A mutator that leaves a signal
/// queued and then fails must not leave the word saying there is nothing.
#[test]
fn a_panic_inside_the_guard_still_publishes() {
    let thread = a_thread(79_008);
    let t = thread.clone();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut lt = t.lock_linux();
        lt.queue_signal(Signal::SIGINT, None);
        panic!("a mutator that fails after queueing");
    }));
    assert!(panicked.is_err(), "the test needs the panic to happen");
    assert!(
        thread.has_deliverable_signal(),
        "the signal was queued before the panic and unwinding ran Drop"
    );
    assert_published_agrees(&thread, "a panicking mutator");
}

/// A thread whose ext is not a Linux one reads as "nothing to deliver",
/// which is what the old `try_lock_linux`-shaped code did with it.
#[test]
fn a_thread_without_a_linux_ext_reads_as_nothing() {
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 79_009).unwrap();
    let bare = Thread::create(&proc, "bare").unwrap();
    assert!(
        !bare.has_deliverable_signal(),
        "a non-Linux thread must answer `nothing`, not panic"
    );
}

// ── the waker a wait leaves on the thread ───────────────────────────────────

/// A waker that counts how many times it was woken.
fn counting_waker(count: &'static core::sync::atomic::AtomicUsize) -> Waker {
    use core::sync::atomic::AtomicUsize;
    use core::task::{RawWaker, RawWakerVTable};
    fn raw(ptr: *const ()) -> RawWaker {
        unsafe fn clone(ptr: *const ()) -> RawWaker {
            raw(ptr)
        }
        unsafe fn wake(ptr: *const ()) {
            (*(ptr as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst);
        }
        unsafe fn drop(_: *const ()) {}
        RawWaker::new(ptr, &RawWakerVTable::new(clone, wake, wake, drop))
    }
    unsafe { Waker::from_raw(raw(count as *const AtomicUsize as *const ())) }
}

/// A signal that becomes deliverable wakes the wait parked on the thread,
/// from the guard's own `Drop`: whoever queued it did not have to remember.
#[test]
fn a_signal_that_becomes_deliverable_wakes_the_wait_parked_on_the_thread() {
    static WOKEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    let thread = a_thread(79_101);
    let ext = thread.linux_ext().unwrap();
    let mut wait = None;
    assert!(ext.park_waiter(&mut wait, &counting_waker(&WOKEN)));
    assert!(wait.is_some());

    thread.lock_linux().queue_signal(Signal::SIGUSR1, None);
    assert_eq!(WOKEN.load(Ordering::SeqCst), 1);

    // The same bit again is no news; another one is.
    thread.lock_linux().queue_signal(Signal::SIGUSR1, None);
    assert_eq!(WOKEN.load(Ordering::SeqCst), 1);
    thread.lock_linux().queue_signal(Signal::SIGTERM, None);
    assert_eq!(WOKEN.load(Ordering::SeqCst), 2);

    // Once the wait is over nothing is woken.
    ext.unpark_waiter(wait.unwrap());
    thread.lock_linux().queue_signal(Signal::SIGINT, None);
    assert_eq!(WOKEN.load(Ordering::SeqCst), 2);
}

/// A signal the thread blocks is not deliverable and wakes nothing by the
/// guard; who queues it says so (`wake_signal_sleeper`), for the waits that
/// want a blocked signal too.
#[test]
fn a_blocked_signal_wakes_the_parked_wait_only_when_its_sender_says_so() {
    static WOKEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    let thread = a_thread(79_102);
    let ext = thread.linux_ext().unwrap();
    thread.lock_linux().set_signal_mask(set_of(Signal::SIGUSR1));
    let mut wait = None;
    assert!(ext.park_waiter(&mut wait, &counting_waker(&WOKEN)));

    thread.lock_linux().queue_signal(Signal::SIGUSR1, None);
    assert_eq!(WOKEN.load(Ordering::SeqCst), 0);
    crate::process::wake_signal_sleeper(&thread);
    assert_eq!(WOKEN.load(Ordering::SeqCst), 1);
}

/// One wait has the place on a thread: a second is refused, and cannot
/// take the first one's waker away by ending.
#[test]
fn one_wait_has_the_place_on_a_thread_and_a_second_is_refused() {
    static FIRST: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    static SECOND: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    let thread = a_thread(79_103);
    let ext = thread.linux_ext().unwrap();
    let (mut first, mut second) = (None, None);
    assert!(ext.park_waiter(&mut first, &counting_waker(&FIRST)));
    assert!(!ext.park_waiter(&mut second, &counting_waker(&SECOND)));
    assert_eq!(second, None);
    // The first parks again, as each poll of it does.
    assert!(ext.park_waiter(&mut first, &counting_waker(&FIRST)));

    // A number that is not the one parked lets nothing go.
    ext.unpark_waiter(first.unwrap() + 1_000_000);
    ext.wake_waiter();
    assert_eq!(FIRST.load(Ordering::SeqCst), 1);
    assert_eq!(SECOND.load(Ordering::SeqCst), 0);

    ext.unpark_waiter(first.unwrap());
    assert!(ext.park_waiter(&mut second, &counting_waker(&SECOND)));
    ext.wake_waiter();
    assert_eq!(SECOND.load(Ordering::SeqCst), 1);
}
