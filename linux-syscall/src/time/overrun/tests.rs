//! One signal for all the expiries it waits through, `timer_getoverrun`
//! answering for the one taken, the count started again by `timer_settime`,
//! and an absolute periodic timer counting from the expiry it was given.

use super::*;
use alloc::sync::Arc;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn a_process(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    (proc, thread)
}

/// A timer of `owner`'s every 10 ms with its next expiry at 1 s, sending
/// `notify`.
fn a_timer(owner: KoID, notify: TimerNotify) -> usize {
    let id = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
    POSIX_TIMERS.lock().insert(
        id,
        PosixTimer {
            owner,
            notify,
            clock: PosixTimerClock::Base(ClockBase::Monotonic),
            interval: ms(10),
            next: ms(1000),
            generation: 0,
            overrun: Overrun::default(),
        },
    );
    id
}

/// The wheel getting to timer `id` at each of `at` milliseconds, in order.
fn expires_at(id: usize, at: &[u64]) {
    for &now in at {
        let gen = POSIX_TIMERS.lock()[&id].generation;
        expire_posix_timer(id, gen, ms(now));
    }
}

/// The `si_overrun` of the `SIGALRM` waiting on `thread`, taking it; `None`
/// if none is.
fn take(thread: &Thread) -> Option<u32> {
    let mut lt = thread.lock_linux();
    let waiting = lt.signals.contains(Signal::SIGALRM);
    waiting.then(|| lt.take_siginfo(Signal::SIGALRM).timer_overrun())
}

fn getoverrun(proc: &Process, id: usize) -> u32 {
    posix_timer_overrun(id, proc.id()).unwrap()
}

/// `timer_settime` of `id` on the monotonic clock at `now`, arming the
/// generation it makes the way the wheel would.
fn settime(
    proc: &Process,
    id: usize,
    abs: bool,
    init: Duration,
    interval: Duration,
    now: Duration,
) {
    set_posix_timer(
        id,
        proc.id(),
        ClockBase::Monotonic,
        abs,
        init,
        interval,
        now,
        now,
    )
    .unwrap();
}

#[test]
fn the_expiries_its_signal_waits_through_are_counted_into_it() {
    let (proc, thread) = a_process(44_301);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010, 1020, 1030]);
    assert_eq!(getoverrun(&proc, id), 0, "0 until the signal is taken");
    assert_eq!(take(&thread), Some(3), "si_overrun: one signal for four");
    assert_eq!(take(&thread), None, "a second signal for the same wait");
    assert_eq!(getoverrun(&proc, id), 3, "what the signal taken carried");
    drop_posix_timers_of(proc.id());
}

#[test]
fn a_late_expiry_while_it_waits_adds_the_periods_it_was_late_by() {
    let (proc, thread) = a_process(44_302);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    // Due at 1010, got to at 1035: that one, and 1020 and 1030 gone by.
    expires_at(id, &[1000, 1035]);
    assert_eq!(take(&thread), Some(3), "floor(35 / 10) periods");
    assert_eq!(POSIX_TIMERS.lock()[&id].next, ms(1040));
    drop_posix_timers_of(proc.id());
}

#[test]
fn timer_getoverrun_answers_for_the_signal_taken_and_not_the_one_waiting() {
    let (proc, thread) = a_process(44_303);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010]);
    assert_eq!(take(&thread), Some(1));
    expires_at(id, &[1020]);
    assert_eq!(
        getoverrun(&proc, id),
        1,
        "answered for the one still waiting"
    );
    assert_eq!(take(&thread), Some(0), "a fresh signal for a fresh wait");
    assert_eq!(getoverrun(&proc, id), 0);
    drop_posix_timers_of(proc.id());
}

#[test]
fn settime_starts_the_count_again_even_in_a_signal_still_waiting() {
    let (proc, thread) = a_process(44_304);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010]);
    assert_eq!(take(&thread), Some(1));
    settime(&proc, id, false, ms(10), ms(10), ms(1015));
    assert_eq!(getoverrun(&proc, id), 0, "timer_settime resets it");

    // A signal of the old arming that waits through the re-arm: the
    // syscall throws it away first (`stale`), so only one queued while
    // the setting is made gets this far, and it counts from the new arming.
    expires_at(id, &[1025, 1035]);
    settime(&proc, id, false, ms(10), ms(10), ms(1040));
    expires_at(id, &[1050, 1060]);
    assert_eq!(
        take(&thread),
        Some(1),
        "the two expiries of the old arming were counted with the new"
    );
    drop_posix_timers_of(proc.id());
}

#[test]
fn an_absolute_periodic_timer_counts_from_the_expiry_it_was_given() {
    let (proc, thread) = a_process(44_305);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    let (_, arm) = set_posix_timer(
        id,
        proc.id(),
        ClockBase::Monotonic,
        true,
        ms(1000),
        ms(10),
        ms(2000),
        ms(2000),
    )
    .unwrap();
    assert_eq!(arm.map(|(at, _)| at), Some(ms(2000)), "not armed for now");
    assert_eq!(POSIX_TIMERS.lock()[&id].next, ms(1000));
    expires_at(id, &[2000]);
    assert_eq!(take(&thread), Some(100), "a second at 10 ms is 100 periods");
    assert_eq!(POSIX_TIMERS.lock()[&id].next, ms(2010));
    drop_posix_timers_of(proc.id());
}

#[test]
fn a_first_expiry_from_before_the_monotonic_clock_counts_the_periods_before_it() {
    let s = Duration::from_secs;
    // Wall clock at 10 s, monotonic at 5 s: an expiry at wall 1 s was -4 s
    // monotonic. Every second from there, the first after zero is at 1 s,
    // five periods on.
    assert_eq!(
        programmed_expiry(ClockBase::Wall, s(1), s(1), s(5), s(10)),
        (s(1), 5)
    );
    // Every hour from -994 s, the first after zero is still to come at
    // 2606 s, and the one gone by stands for no other.
    assert_eq!(
        programmed_expiry(ClockBase::Wall, s(1), s(3600), s(5), s(1000)),
        (s(2606), 0)
    );
    // Gone by, but after zero: where it is.
    assert_eq!(
        programmed_expiry(ClockBase::Monotonic, s(2), s(1), s(5), s(10)),
        (s(2), 0)
    );
    // Still to come: where it will be.
    assert_eq!(
        programmed_expiry(ClockBase::Wall, s(12), s(1), s(5), s(10)),
        (s(7), 0)
    );

    let (proc, thread) = a_process(44_306);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    set_posix_timer(
        id,
        proc.id(),
        ClockBase::Wall,
        true,
        s(1),
        s(1),
        s(5),
        s(10),
    )
    .unwrap();
    let gen = POSIX_TIMERS.lock()[&id].generation;
    assert_eq!(expire_posix_timer(id, gen, s(5)), Some(s(6)));
    assert_eq!(take(&thread), Some(9), "nine seconds late at one a second");
    assert_eq!(expire_posix_timer(id, gen, s(6)), Some(s(7)));
    assert_eq!(
        take(&thread),
        Some(0),
        "the periods before zero counted twice"
    );

    set_posix_timer(
        id,
        proc.id(),
        ClockBase::Wall,
        true,
        s(1),
        s(3600),
        s(5),
        s(1000),
    )
    .unwrap();
    let gen = POSIX_TIMERS.lock()[&id].generation;
    assert_eq!(
        expire_posix_timer(id, gen, s(5)),
        Some(s(2606)),
        "not next at 2606 s"
    );
    assert_eq!(take(&thread), Some(0));

    // CLOCK_REALTIME from 1 ns after 1970 at 10 ms: more than an int holds.
    set_posix_timer(
        id,
        proc.id(),
        ClockBase::Wall,
        true,
        Duration::from_nanos(1),
        ms(10),
        s(5),
        s(1_700_000_000),
    )
    .unwrap();
    let gen = POSIX_TIMERS.lock()[&id].generation;
    expire_posix_timer(id, gen, s(5));
    assert_eq!(take(&thread), Some(DELAYTIMER_MAX));
    drop_posix_timers_of(proc.id());
}

#[test]
fn a_sigev_none_timer_never_has_anything_taken() {
    let (proc, thread) = a_process(44_307);
    let none = TimerNotify {
        signo: 0,
        value: 0,
        thread: None,
    };
    let id = a_timer(proc.id(), none);
    expires_at(id, &[1500, 1600]);
    assert_eq!(getoverrun(&proc, id), 0);
    assert_eq!(take(&thread), None, "SIGEV_NONE sent a signal");
    drop_posix_timers_of(proc.id());
}

#[test]
fn a_thread_directed_timer_counts_into_the_signal_on_its_thread() {
    let (proc, first) = a_process(44_308);
    let second = Thread::create_linux(&proc).unwrap();
    let to_second = TimerNotify {
        signo: Signal::SIGALRM as usize,
        value: 0,
        thread: Some(second.id()),
    };
    let id = a_timer(proc.id(), to_second);
    expires_at(id, &[1000, 1010, 1020]);
    assert_eq!(take(&first), None, "went to the other thread");
    assert_eq!(getoverrun(&proc, id), 0);
    assert_eq!(take(&second), Some(2));
    assert_eq!(getoverrun(&proc, id), 2);
    drop_posix_timers_of(proc.id());
}

/// [`posix_timer_overrun`], with `before` run ahead of each look at the
/// signal and `after` once it has been looked at, both told which look it
/// is; and how many looks it took.
fn getoverrun_while(
    proc: &Process,
    id: usize,
    mut before: impl FnMut(usize),
    mut after: impl FnMut(usize),
) -> (u32, usize) {
    let mut looks = 0;
    let answer = posix_timer_overrun_with(id, proc.id(), |notify, signal| {
        before(looks);
        let waiting = with_pending_timer_signal(
            proc.id() as usize,
            notify.thread.map(|tid| tid as usize),
            signal,
            id as i32,
            |_| {},
        );
        after(looks);
        looks += 1;
        waiting
    })
    .unwrap();
    (answer, looks)
}

#[test]
fn an_expiry_between_the_counts_and_the_signal_is_not_answered_for() {
    // The counts read with the signal just taken, and the signal looked at
    // once the next expiry had queued a new one.
    let (proc, thread) = a_process(50_101);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010]);
    assert_eq!(take(&thread), Some(1));
    let (answer, _) = getoverrun_while(
        &proc,
        id,
        |look| {
            if look == 0 {
                expires_at(id, &[1020]);
            }
        },
        |_| {},
    );
    assert_eq!(answer, 1, "the last before the signal just taken");
    assert_eq!(take(&thread), Some(0));
    drop_posix_timers_of(proc.id());
}

#[test]
fn a_signal_counted_into_and_taken_while_it_is_looked_for_answers_what_it_carried() {
    let (proc, thread) = a_process(50_102);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010]);
    let mut took = None;
    let (answer, _) = getoverrun_while(
        &proc,
        id,
        |look| {
            if look == 0 {
                expires_at(id, &[1020]);
                took = take(&thread);
            }
        },
        |_| {},
    );
    assert_eq!(took, Some(2));
    assert_eq!(answer, 2, "what the signal carried before the last expiry");
    drop_posix_timers_of(proc.id());
}

#[test]
fn a_timer_that_went_through_expiries_back_to_the_same_counts_is_looked_at_again() {
    // Read with the 1 taken and nothing waiting; looked at with a 0
    // waiting; and before the counts are read again, the 0 taken and a 1
    // queued and taken too. The counts read as they did at first.
    let (proc, thread) = a_process(50_103);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010]);
    assert_eq!(take(&thread), Some(1));
    let mut took = vec![];
    let (answer, looks) = getoverrun_while(
        &proc,
        id,
        |look| {
            if look == 0 {
                expires_at(id, &[1020]);
            }
        },
        |look| {
            if look == 0 {
                took.push(take(&thread));
                // Due at 1030: one period late.
                expires_at(id, &[1040]);
                took.push(take(&thread));
            }
        },
    );
    assert_eq!(took, [Some(0), Some(1)]);
    assert_eq!(answer, 1, "answered from a look the timer moved under");
    assert_eq!(looks, 2);
    drop_posix_timers_of(proc.id());
}

#[test]
fn a_timer_that_moves_at_every_look_still_gets_an_answer() {
    let (proc, thread) = a_process(50_104);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010]);
    let (answer, looks) = getoverrun_while(
        &proc,
        id,
        |look| {
            // Every look finds one more expiry counted into the signal,
            // for longer than the looks it is given.
            if look < 4 * OVERRUN_LOOKS {
                expires_at(id, &[1020 + 10 * look as u64]);
            }
        },
        |_| {},
    );
    assert_eq!(looks, OVERRUN_LOOKS);
    assert_eq!(answer, 0, "nothing taken yet");
    assert_eq!(take(&thread), Some(1 + OVERRUN_LOOKS as u32));
    drop_posix_timers_of(proc.id());
}

#[test]
fn a_timer_that_holds_still_is_looked_at_once() {
    let (proc, thread) = a_process(50_105);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010, 1020]);
    assert_eq!(take(&thread), Some(2));
    assert_eq!(getoverrun_while(&proc, id, |_| {}, |_| {}), (2, 1));
    expires_at(id, &[1030]);
    assert_eq!(getoverrun_while(&proc, id, |_| {}, |_| {}), (2, 1));
    drop_posix_timers_of(proc.id());
}

#[test]
fn a_timer_set_again_and_brought_back_to_the_same_counts_is_looked_at_again() {
    // Read with a 2 waiting after a 1 was taken: 1. Before the look, set
    // again (the 2 thrown away) and run up to a 1 taken, so nothing waits;
    // after it, a 2 queued again. The counts, and how many times they
    // changed, read as they did at first: only the generation tells.
    let (proc, thread) = a_process(50_106);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010]);
    assert_eq!(take(&thread), Some(1));
    // Due at 1020: two periods late.
    expires_at(id, &[1040]);
    let mut took = None;
    let (answer, looks) = getoverrun_while(
        &proc,
        id,
        |look| {
            if look == 0 {
                // As `timer_settime` does it, the 2 thrown away first.
                stale::drop_signal_from_before(id, proc.id());
                settime(&proc, id, false, ms(10), ms(10), ms(2000));
                expires_at(id, &[2010, 2020]);
                took = take(&thread);
            }
        },
        |look| {
            if look == 0 {
                // Due at 2030.
                expires_at(id, &[2050]);
            }
        },
    );
    assert_eq!(took, Some(1));
    assert_eq!(answer, 1, "the 2 waiting, never taken");
    assert_eq!(looks, 2);
    drop_posix_timers_of(proc.id());
}

#[test]
fn between_moving_last_on_and_queuing_the_next_signal_the_answer_is_last() {
    // An expiry that finds its signal taken moves `last` on to what that
    // one carried, and only then queues the next one.
    let (proc, thread) = a_process(50_107);
    let id = a_timer(proc.id(), TimerNotify::SIGALRM_TO_PROCESS);
    expires_at(id, &[1000, 1010]);
    assert_eq!(take(&thread), Some(1));
    let gen = POSIX_TIMERS.lock()[&id].generation;
    settle(id, gen, |o| {
        if let Some(taken) = o.queued.take() {
            o.last = taken;
        }
    });
    assert_eq!(getoverrun(&proc, id), 1);
    drop_posix_timers_of(proc.id());
}
