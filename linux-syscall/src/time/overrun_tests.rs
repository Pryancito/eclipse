//! A periodic timer that fell behind fired once per missed period, back
//! to back, and `timer_getoverrun` always said 0.

use super::*;
use linux_object::process::LinuxProcess;
use linux_object::signal::SignalCode;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::Process;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The boundary of "has the next expiry already gone by": one that lands
/// exactly on `now` is due now, so the answer has to step past it. Every
/// other case here sits comfortably on one side or the other, and
/// returning `now` itself is a timer that fires again at the same instant,
/// for ever.
#[test]
fn the_next_expiry_is_always_strictly_after_now() {
    // 1000 + 100 lands exactly on 1100: that expiry fires now, so the one
    // after it is 1200, and the period in between is the overrun.
    let (next, overrun) = forward_periodic(ms(1000), ms(100), ms(1100));
    assert_eq!((next, overrun), (ms(1200), 1));
    assert!(next > ms(1100), "un vencimiento en el mismo instante");

    // One nanosecond earlier and nothing has been missed at all.
    let just_before = ms(1100) - Duration::from_nanos(1);
    assert_eq!(
        forward_periodic(ms(1000), ms(100), just_before),
        (ms(1100), 0)
    );
}

/// The conversion `timer_settime(2)` puts both of its `timespec` fields
/// through. Nothing named it: they reach it only from inside
/// `sys_timer_settime`, where a wrong unit still produces "some duration"
/// and the timer just fires at the wrong time.
#[test]
fn a_timespec_becomes_its_own_seconds_and_its_own_nanoseconds() {
    assert_eq!(
        timespec_to_duration(TimeSpec {
            sec: 2,
            nsec: 500_000_000
        }),
        ms(2_500)
    );
    assert_eq!(
        timespec_to_duration(TimeSpec { sec: 1, nsec: 0 }),
        Duration::from_secs(1),
        "los segundos son segundos"
    );
    assert_eq!(
        timespec_to_duration(TimeSpec { sec: 0, nsec: 1 }),
        Duration::from_nanos(1),
        "y los nanosegundos, nanosegundos"
    );
}

/// Anything outside `TIMER_ABSTIME` is EINVAL — masking the one known bit
/// used to arm a relative timer and answer success for a typo.
#[test]
fn timer_settime_rejects_unknown_flags() {
    assert_eq!(timer_settime_abs(0), Ok(false));
    assert_eq!(timer_settime_abs(TIMER_ABSTIME), Ok(true));
    assert_eq!(timer_settime_abs(2), Err(LxError::EINVAL));
    assert_eq!(
        timer_settime_abs(TIMER_ABSTIME | 2),
        Err(LxError::EINVAL),
        "a valid bit must not hide a stray one"
    );
    assert_eq!(timer_settime_abs(1usize << 30), Err(LxError::EINVAL));
}

/// What `getitimer(2)` and `alarm(2)` both report, in the one place it is
/// worked out now: the time still to run, not the time already run.
#[test]
fn a_slot_reports_the_time_it_has_left_and_not_the_time_it_has_run() {
    let armed = ItimerSlot {
        interval: ms(0),
        deadline: Some(ms(1_000)),
        generation: 0,
    };
    assert_eq!(slot_remaining(&armed, ms(400)), ms(600));
    assert_eq!(slot_remaining(&armed, ms(999)), ms(1));
    // A deadline already gone by is zero, not a wrapped duration.
    assert_eq!(slot_remaining(&armed, ms(1_400)), Duration::ZERO);

    let disarmed = ItimerSlot {
        interval: ms(50),
        deadline: None,
        generation: 0,
    };
    assert_eq!(slot_remaining(&disarmed, ms(400)), Duration::ZERO);
}

fn a_process(pid: KoID) -> (alloc::sync::Arc<Process>, alloc::sync::Arc<Thread>) {
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    (proc, thread)
}

/// A periodic POSIX timer of `owner` whose expiry was programmed at
/// 1 s, every 10 ms.
fn a_periodic_timer(owner: KoID) -> usize {
    let id = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
    POSIX_TIMERS.lock().insert(
        id,
        PosixTimer {
            _sigq: None,
            owner,
            notify: TimerNotify::SIGALRM_TO_PROCESS,
            clock: PosixTimerClock::Base(ClockBase::Monotonic),
            interval: ms(10),
            next: ms(1000),
            generation: 0,
            overrun: Overrun::default(),
        },
    );
    id
}

/// `si_overrun` where glibc reads it.
fn si_overrun(info: &SigInfo) -> i32 {
    let b = info.as_bytes();
    i32::from_ne_bytes([b[20], b[21], b[22], b[23]])
}

#[test]
fn a_timer_on_time_steps_one_period_with_no_overrun() {
    assert_eq!(forward_periodic(ms(1000), ms(10), ms(1000)), (ms(1010), 0));
    assert_eq!(forward_periodic(ms(1000), ms(10), ms(1009)), (ms(1010), 0));
}

#[test]
fn a_late_timer_skips_to_the_first_period_after_now_and_counts_the_rest() {
    // 3.5 periods late: this expiry, three skipped, next at 1040.
    assert_eq!(forward_periodic(ms(1000), ms(10), ms(1035)), (ms(1040), 3));
    // Exactly two periods late: the next expiry has to be AFTER now.
    assert_eq!(forward_periodic(ms(1000), ms(10), ms(1020)), (ms(1030), 2));
    // Ten seconds late on a 1 ms timer: one expiry, not ten thousand.
    let (next, overrun) = forward_periodic(ms(1000), ms(1), ms(11000));
    assert!(next > ms(11000), "next expiry {:?} already gone", next);
    assert_eq!(overrun, 10_000);
    // Nothing to overflow on: a 1 ns period a year behind.
    let (next, overrun) = forward_periodic(
        Duration::from_secs(1),
        Duration::from_nanos(1),
        Duration::from_secs(365 * 24 * 3600),
    );
    assert!(next > Duration::from_secs(365 * 24 * 3600));
    assert_eq!(overrun, DELAYTIMER_MAX);
}

#[test]
fn a_late_expiry_delivers_one_signal_with_the_overrun_and_keeps_it_for_getoverrun() {
    let (proc, thread) = a_process(43_701);
    let id = a_periodic_timer(proc.id());
    // The wheel got to the 1 s expiry at 1.5 s: the expiries at 1010,
    // 1020, ... 1500 went by, fifty of them, and only this one fires.
    let next = expire_posix_timer(id, 0, ms(1500));
    assert_eq!(next, Some(ms(1510)), "the next expiry must be after now");
    let info = thread.lock_linux().take_siginfo(Signal::SIGALRM);
    assert_eq!(
        info.code,
        SignalCode::TIMER,
        "not SI_TIMER: a POSIX timer expiry"
    );
    assert_eq!(si_overrun(&info), 50, "si_overrun");
    assert_eq!(
        posix_timer_overrun(id, proc.id()),
        Ok(50),
        "timer_getoverrun would answer this"
    );
    let timers = POSIX_TIMERS.lock();
    let t = timers.get(&id).unwrap();
    assert_eq!(t.next, ms(1510));
    assert!(
        !thread.lock_linux().signals.contains(Signal::SIGALRM),
        "a second signal was delivered for the same expiry"
    );
}

#[test]
fn a_one_shot_expiry_disarms_and_a_stale_generation_fires_nothing() {
    let (proc, thread) = a_process(43_702);
    let id = a_periodic_timer(proc.id());
    POSIX_TIMERS.lock().get_mut(&id).unwrap().interval = Duration::ZERO;
    assert_eq!(expire_posix_timer(id, 0, ms(1500)), None);
    assert!(thread.lock_linux().signals.contains(Signal::SIGALRM));
    assert_eq!(POSIX_TIMERS.lock().get(&id).unwrap().next, Duration::ZERO);

    let id = a_periodic_timer(proc.id());
    POSIX_TIMERS.lock().get_mut(&id).unwrap().generation = 3;
    thread.lock_linux().take_siginfo(Signal::SIGALRM);
    assert_eq!(expire_posix_timer(id, 0, ms(1500)), None);
    assert!(!thread.lock_linux().signals.contains(Signal::SIGALRM));
}

#[test]
fn a_late_interval_timer_fires_once_and_lands_after_now() {
    let (proc, thread) = a_process(43_703);
    {
        let lp = proc.linux();
        let mut slots = lp.itimers().lock();
        slots[ITIMER_REAL] = ItimerSlot {
            interval: ms(10),
            deadline: Some(ms(1000)),
            generation: 5,
        };
    }
    let next = expire_itimer(proc.id(), ITIMER_REAL, 5, ms(1500));
    assert_eq!(next, Some(ms(1510)), "the next expiry must be after now");
    assert_eq!(
        proc.linux().itimers().lock()[ITIMER_REAL].deadline,
        Some(ms(1510))
    );
    assert!(thread.lock_linux().signals.contains(Signal::SIGALRM));
    // A stale generation is a slot that was re-armed since: nothing.
    assert_eq!(expire_itimer(proc.id(), ITIMER_REAL, 4, ms(1600)), None);
}

/// POSIX: a zero `it_value` stops the timer, whatever `it_interval` says,
/// and `getitimer` reports it stopped.
#[test]
fn a_disarmed_slot_keeps_no_interval() {
    let mut slot = ItimerSlot::default();
    rearm_itimer_slot(&mut slot, Some(ms(1000)), ms(50));
    assert_eq!(slot.interval, ms(50));
    rearm_itimer_slot(&mut slot, None, ms(50));
    assert_eq!((slot.deadline, slot.interval), (None, Duration::ZERO));
}

/// The alarm a process left pending when it died expires against its
/// pid, which by then can be another process's, one that armed its own
/// alarm the same way.
#[test]
fn a_dead_process_alarm_does_not_fire_at_whoever_has_its_pid_now() {
    let alarm_of = |proc: &Process, at| {
        let lp = proc.linux();
        let mut slots = lp.itimers().lock();
        rearm_itimer_slot(&mut slots[ITIMER_REAL], Some(at), Duration::ZERO)
    };
    let dead = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        43_704,
        "t",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let pending = alarm_of(&dead, ms(1000));
    dead.exit(0);
    drop(dead);

    let (heir, thread) = a_process(43_704);
    alarm_of(&heir, ms(20_000));

    assert_eq!(expire_itimer(43_704, ITIMER_REAL, pending, ms(1000)), None);
    assert!(
        !thread.lock_linux().signals.contains(Signal::SIGALRM),
        "the dead process's alarm went off in the one with its pid"
    );
    assert_eq!(
        heir.linux().itimers().lock()[ITIMER_REAL].deadline,
        Some(ms(20_000)),
        "and took the alarm of its own with it"
    );
}
