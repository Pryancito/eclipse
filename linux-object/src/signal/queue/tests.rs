//! The queue as Linux 6.18 keeps it (`rtq` probe, 52 cases there): one entry
//! per send of a real-time signal, one per pending standard signal, counted
//! against the receiving user up to `RLIMIT_SIGPENDING`.
//!
//! The count is one table for the machine, so every test charges a uid of
//! its own, in 47_5xx.

use super::*;
use crate::signal::SignalCode;
use alloc::vec::Vec;
use core::convert::TryInto;

const RT: Signal = Signal::SIGRT34;

fn to(uid: u32, limit: u64) -> Option<Receiver> {
    Some(Receiver { uid, limit })
}

/// What `sigqueue(pid, signal, value)` sends.
fn queued(signal: Signal, value: usize) -> Option<SigInfo> {
    Some(SigInfo::queued(signal, 7, 1000, value))
}

/// What `kill(2)` sends.
fn killed(signal: Signal) -> Option<SigInfo> {
    Some(SigInfo::from_user(signal, 7, 1000, SignalCode::USER))
}

/// The `si_value` of a `queued` info (`_rt.si_sigval`, after pid and uid).
fn value(info: &SigInfo) -> usize {
    let b = info.as_bytes();
    usize::from_ne_bytes(b[24..32].try_into().unwrap())
}

#[test]
fn a_real_time_signal_sent_three_times_is_taken_three_times_in_order() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    for v in 1..=3 {
        q.send(&mut pending, RT, queued(RT, v), to(47_501, 100))
            .unwrap();
    }
    assert_eq!(queued_for_user(47_501), 3, "SigQ counts each");
    for v in 1..=3 {
        assert!(pending.contains(RT), "gone before its entry {}", v);
        let info = q.take(&mut pending, RT);
        assert_eq!((value(&info), info.code), (v, SignalCode::QUEUE));
    }
    assert!(!pending.contains(RT), "pending once drained");
    assert_eq!(queued_for_user(47_501), 0, "taking gives each back");
}

#[test]
fn a_standard_signal_sent_twice_is_one_entry_with_the_first_info() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    let usr1 = Signal::SIGUSR1;
    q.send(&mut pending, usr1, queued(usr1, 1), to(47_502, 100))
        .unwrap();
    q.send(&mut pending, usr1, queued(usr1, 2), to(47_502, 100))
        .unwrap();
    assert_eq!(queued_for_user(47_502), 1);
    assert_eq!(value(&q.take(&mut pending, usr1)), 1, "the first kept");
    assert!(!pending.contains(usr1), "the second was queued");
}

#[test]
fn a_kill_of_a_real_time_signal_queues_too() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    for _ in 0..2 {
        q.send(&mut pending, RT, killed(RT), to(47_503, 100))
            .unwrap();
    }
    assert_eq!(q.entries_of(RT), 2);
    assert_eq!(q.take(&mut pending, RT).code, SignalCode::USER);
    assert_eq!(q.take(&mut pending, RT).code, SignalCode::USER);
    assert!(!pending.contains(RT));
}

#[test]
fn past_the_limit_a_sigqueue_is_refused_and_a_kill_loses_its_info() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    let lim = to(47_504, 2);
    q.send(&mut pending, RT, queued(RT, 1), lim).unwrap();
    q.send(&mut pending, RT, queued(RT, 2), lim).unwrap();
    assert_eq!(q.send(&mut pending, RT, queued(RT, 3), lim), Err(QueueFull));
    assert_eq!(q.entries_of(RT), 2, "the refused one left an entry");
    assert_eq!(queued_for_user(47_504), 2);
    // A `kill` is never refused: past the limit it is the bit alone.
    let rt1 = Signal::SIGRT35;
    q.send(&mut pending, RT, killed(RT), lim).unwrap();
    q.send(&mut pending, rt1, killed(rt1), lim).unwrap();
    assert_eq!((q.entries_of(RT), q.entries_of(rt1)), (2, 0));
    assert!(pending.contains(rt1));
    // Nor is a standard signal, even from `sigqueue`.
    let usr2 = Signal::SIGUSR2;
    q.send(&mut pending, usr2, queued(usr2, 7), lim).unwrap();
    assert!(pending.contains(usr2));
    assert_eq!(queued_for_user(47_504), 2, "nothing past the limit counted");
    assert_eq!(value(&q.take(&mut pending, RT)), 1);
    assert_eq!(value(&q.take(&mut pending, RT)), 2);
    assert!(!pending.contains(RT), "the kill folded into the set bit");
    assert!(q.take(&mut pending, rt1) == SigInfo::bare(rt1), "lost info");
    assert!(
        q.take(&mut pending, usr2) == SigInfo::bare(usr2),
        "lost info"
    );
    assert_eq!(queued_for_user(47_504), 0);
    q.send(&mut pending, RT, queued(RT, 4), lim).unwrap();
    assert_eq!(q.entries_of(RT), 1, "room again once taken");
}

#[test]
fn a_standard_signal_from_the_kernel_or_kill_is_counted_past_the_limit() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    let lim = to(47_505, 0);
    let chld = Signal::SIGCHLD;
    q.send(
        &mut pending,
        chld,
        Some(SigInfo::child_state_change(9, 0, 0)),
        lim,
    )
    .unwrap();
    q.send(&mut pending, Signal::SIGTERM, killed(Signal::SIGTERM), lim)
        .unwrap();
    q.send(&mut pending, Signal::SIGHUP, None, lim).unwrap();
    assert_eq!(queued_for_user(47_505), 3, "`override_rlimit`");
    assert_eq!(q.take(&mut pending, chld).code, SignalCode::CLD_EXITED);
    // A real-time one with no information is not, and is not refused.
    assert_eq!(q.send(&mut pending, RT, None, lim), Ok(()));
    assert_eq!(q.entries_of(RT), 0);
    assert!(pending.contains(RT));
}

#[test]
fn sigkill_gets_no_entry() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    q.send(
        &mut pending,
        Signal::SIGKILL,
        killed(Signal::SIGKILL),
        to(47_506, 9),
    )
    .unwrap();
    assert!(pending.contains(Signal::SIGKILL));
    assert_eq!(q.entries_of(Signal::SIGKILL), 0);
    assert_eq!(queued_for_user(47_506), 0);
}

#[test]
fn a_discard_or_a_clear_gives_every_entry_back() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    for v in 0..3 {
        q.send(&mut pending, RT, queued(RT, v), to(47_507, 9))
            .unwrap();
    }
    q.send(&mut pending, Signal::SIGUSR1, None, to(47_507, 9))
        .unwrap();
    q.discard(&mut pending, RT);
    assert!(!pending.contains(RT));
    assert_eq!(queued_for_user(47_507), 1);
    q.clear();
    assert_eq!(queued_for_user(47_507), 0);
    q.send(&mut pending, RT, queued(RT, 0), to(47_507, 9))
        .unwrap();
    drop(q);
    assert_eq!(queued_for_user(47_507), 0, "dropped with its task");
}

#[test]
fn a_bit_with_no_entry_is_taken_as_si_user_from_nobody() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    pending.insert(Signal::SIGUSR1);
    assert!(q.take(&mut pending, Signal::SIGUSR1) == SigInfo::bare(Signal::SIGUSR1));
    assert!(!pending.contains(Signal::SIGUSR1));
}

#[test]
fn a_timer_s_entry_is_its_own_and_counts_for_nobody() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    let alrm = Signal::SIGALRM;
    // An `alarm(2)` already pending, and the timer goes in all the same.
    q.send(
        &mut pending,
        alrm,
        Some(SigInfo::from_kernel(alrm)),
        to(47_508, 9),
    )
    .unwrap();
    q.send_timer(&mut pending, alrm, SigInfo::timer(alrm, 7, 0, 0));
    assert_eq!(q.entries_of(alrm), 2, "no `legacy_queue` for a timer");
    assert_eq!(
        queued_for_user(47_508),
        1,
        "the timer was counted at its creation"
    );
    // A forged `SI_TIMER` from `rt_sigqueueinfo` is no timer's.
    q.send(
        &mut pending,
        RT,
        Some(SigInfo::timer(RT, 7, 0, 0)),
        to(47_508, 9),
    )
    .unwrap();
    assert!(!q.at_timer(&mut pending, RT, 7, |_| true));
    assert!(
        !q.at_timer(&mut pending, alrm, 8, |_| true),
        "another timer"
    );
    assert!(q.at_timer(&mut pending, alrm, 7, |info| {
        info.set_timer_overrun(3);
        false
    }));
    assert!(q.at_timer(&mut pending, alrm, 7, |info| info.timer_overrun() == 3));
    assert!(pending.contains(alrm), "the alarm is still there");
    assert!(q.take(&mut pending, alrm) == SigInfo::from_kernel(alrm));
    assert!(!pending.contains(alrm), "the timer's entry came back");
}

#[test]
fn a_timer_s_entry_is_not_found_once_its_bit_is_gone() {
    let (mut q, mut pending) = (SigQueue::default(), Sigset::empty());
    q.send_timer(&mut pending, RT, SigInfo::timer(RT, 7, 0, 0));
    pending.remove(RT);
    assert!(!q.at_timer(&mut pending, RT, 7, |_| true));
}

#[test]
fn a_move_keeps_the_entries_their_order_and_their_charges() {
    let (mut shared, mut shared_pending) = (SigQueue::default(), Sigset::empty());
    let (mut own, mut own_pending) = (SigQueue::default(), Sigset::empty());
    own.send(&mut own_pending, RT, queued(RT, 2), to(47_509, 9))
        .unwrap();
    shared
        .send(&mut shared_pending, RT, queued(RT, 1), to(47_509, 9))
        .unwrap();
    shared
        .send(&mut shared_pending, RT, queued(RT, 3), to(47_509, 9))
        .unwrap();
    shared.move_into(&mut shared_pending, RT, &mut own, &mut own_pending);
    assert!(!shared_pending.contains(RT));
    assert_eq!(shared.entries_of(RT), 0);
    assert_eq!(queued_for_user(47_509), 3, "moved, not given back");
    let order: Vec<usize> = (0..3)
        .map(|_| value(&own.take(&mut own_pending, RT)))
        .collect();
    assert_eq!(
        order,
        [2, 1, 3],
        "the thread's own first, as `dequeue_signal`"
    );
    assert_eq!(queued_for_user(47_509), 0);
}

#[test]
fn a_move_onto_a_pending_signal_keeps_both_deliveries() {
    let usr1 = Signal::SIGUSR1;
    // Both bits alone: two deliveries, as Linux makes one from each list.
    let (mut shared, mut sp) = (SigQueue::default(), Sigset::empty());
    let (mut own, mut op) = (SigQueue::default(), Sigset::empty());
    sp.insert(usr1);
    op.insert(usr1);
    shared.move_into(&mut sp, usr1, &mut own, &mut op);
    assert_eq!(own.entries_of(usr1), 2);
    // The thread's entry, then the moved bit alone.
    let (mut shared, mut sp) = (SigQueue::default(), Sigset::empty());
    let (mut own, mut op) = (SigQueue::default(), Sigset::empty());
    sp.insert(usr1);
    own.send(&mut op, usr1, killed(usr1), None).unwrap();
    shared.move_into(&mut sp, usr1, &mut own, &mut op);
    assert_eq!(own.take(&mut op, usr1).code, SignalCode::USER);
    assert!(op.contains(usr1), "the moved one went");
    assert!(own.take(&mut op, usr1) == SigInfo::bare(usr1));
    assert!(!op.contains(usr1));
    // Nothing pending on the thread: the bit moves alone, and is one.
    let (mut shared, mut sp) = (SigQueue::default(), Sigset::empty());
    let (mut own, mut op) = (SigQueue::default(), Sigset::empty());
    sp.insert(usr1);
    shared.move_into(&mut sp, usr1, &mut own, &mut op);
    assert!(op.contains(usr1));
    assert_eq!(own.entries_of(usr1), 0);
    // And a signal not pending does not move.
    shared.move_into(&mut sp, usr1, &mut own, &mut op);
    assert!(op.contains(usr1));
}
