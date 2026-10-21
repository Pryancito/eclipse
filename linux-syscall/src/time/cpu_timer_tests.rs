//! ITIMER_VIRTUAL, ITIMER_PROF and `RLIMIT_CPU` count the process's CPU
//! time. The two itimers ticked on the wall clock, from the timer wheel, and
//! nothing looked at the CPU limit at all.

use super::*;
use alloc::sync::Arc;
use alloc::vec::Vec;
use async_std::task::block_on;
use linux_object::process::{wait_child, LinuxProcess};
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

/// Whether `signal` is pending on `thread`, taking it if it is.
fn took(thread: &Thread, signal: Signal) -> bool {
    let mut linux = thread.lock_linux();
    let pending = linux.signals.contains(signal);
    linux.take_siginfo(signal);
    linux.signals.remove(signal);
    pending
}

#[test]
fn itimer_prof_goes_off_at_its_cpu_time_and_not_before() {
    let (proc, thread) = a_process(45_141);
    let lp = proc.linux();
    let now = itimer_now(&proc, ITIMER_PROF);
    let (_, wheel) = set_itimer(lp.itimers(), ITIMER_PROF, ms(100), Duration::ZERO, now);
    assert_eq!(wheel, None, "a CPU timer was handed to the timer wheel");
    assert!(lp.itimers().cpu_armed());

    run_cpu_timers(&thread);
    assert!(
        !took(&thread, Signal::SIGPROF),
        "no CPU used, and SIGPROF came"
    );
    thread.time_add(60_000_000);
    thread.sys_time_add(30_000_000);
    run_cpu_timers(&thread);
    assert!(!took(&thread, Signal::SIGPROF), "SIGPROF at 90 ms of 100");
    assert_eq!(
        itimerval_from_slot(
            &lp.itimers().lock()[ITIMER_PROF],
            itimer_now(&proc, ITIMER_PROF)
        )
        .value
        .usec,
        10_000,
        "getitimer: 10 ms left"
    );

    thread.sys_time_add(10_000_000);
    run_cpu_timers(&thread);
    assert!(
        took(&thread, Signal::SIGPROF),
        "no SIGPROF at 100 ms of CPU"
    );
    assert_eq!(lp.itimers().lock()[ITIMER_PROF].deadline, None);
    assert!(
        !lp.itimers().cpu_armed(),
        "nothing left, and still looked at"
    );
}

#[test]
fn itimer_virtual_counts_user_time_only() {
    let (proc, thread) = a_process(45_142);
    let lp = proc.linux();
    let now = itimer_now(&proc, ITIMER_VIRTUAL);
    set_itimer(lp.itimers(), ITIMER_VIRTUAL, ms(100), Duration::ZERO, now);
    thread.sys_time_add(500_000_000);
    run_cpu_timers(&thread);
    assert!(
        !took(&thread, Signal::SIGVTALRM),
        "kernel time moved ITIMER_VIRTUAL"
    );
    thread.time_add(100_000_000);
    run_cpu_timers(&thread);
    assert!(took(&thread, Signal::SIGVTALRM));
}

#[test]
fn itimer_real_is_still_the_wheels() {
    let itimers = Itimers::default();
    let (_, wheel) = set_itimer(&itimers, ITIMER_REAL, ms(100), Duration::ZERO, ms(5));
    assert_eq!(wheel.map(|(at, _)| at), Some(ms(105)));
    assert!(
        !itimers.cpu_armed(),
        "ITIMER_REAL made the CPU timers looked at"
    );
}

/// `check_cpu_itimer`: one interval a look, so one that fell behind goes
/// off again at the next look.
#[test]
fn an_interval_moves_one_period_a_look() {
    let mut slot = ItimerSlot {
        interval: ms(20),
        deadline: Some(ms(100)),
        generation: 1,
    };
    let looks: Vec<_> = (0..4)
        .map(|_| (expire_cpu_slot(&mut slot, ms(145)), slot.deadline))
        .collect();
    assert_eq!(
        looks,
        [
            (true, Some(ms(120))),
            (true, Some(ms(140))),
            (true, Some(ms(160))),
            (false, Some(ms(160)))
        ]
    );
    let mut one_shot = ItimerSlot {
        deadline: Some(ms(100)),
        ..Default::default()
    };
    assert!(!expire_cpu_slot(&mut one_shot, ms(99)));
    assert!(expire_cpu_slot(&mut one_shot, ms(100)));
    assert_eq!(one_shot.deadline, None);
}

#[test]
fn rlimit_cpu_is_sigxcpu_at_the_soft_limit_and_sigkill_at_the_hard_one() {
    let limit = |cur, max| RLimit { cur, max };
    let s = |n: u64| n * 1_000_000_000;
    assert_eq!(rlimit_cpu_due(limit(1, 2), s(1) - 1), RlimitCpu::Within);
    assert_eq!(rlimit_cpu_due(limit(1, 2), s(1)), RlimitCpu::Xcpu);
    assert_eq!(rlimit_cpu_due(limit(1, 2), s(2)), RlimitCpu::Kill);
    assert_eq!(
        rlimit_cpu_due(limit(1, RLIM_INFINITY), s(500)),
        RlimitCpu::Xcpu
    );
    assert_eq!(
        rlimit_cpu_due(limit(RLIM_INFINITY, RLIM_INFINITY), u64::MAX),
        RlimitCpu::Within
    );
}

#[test]
fn rlimit_cpu_sends_a_sigxcpu_a_second_and_moves_the_soft_limit() {
    let (proc, thread) = a_process(45_143);
    let lp = proc.linux();
    lp.rlimit(RLIMIT_CPU, Some(RLimit { cur: 1, max: 3 }), true)
        .unwrap();
    lp.itimers().watch_cpu_limit();
    let soft = || lp.rlimit(RLIMIT_CPU, None, false).unwrap().cur;

    thread.time_add(900_000_000);
    run_cpu_timers(&thread);
    assert!(
        !took(&thread, Signal::SIGXCPU),
        "SIGXCPU before the soft limit"
    );
    thread.time_add(300_000_000);
    run_cpu_timers(&thread);
    assert!(took(&thread, Signal::SIGXCPU), "no SIGXCPU at 1.2 s of 1");
    assert_eq!(soft(), 2, "getrlimit does not show the next one");
    run_cpu_timers(&thread);
    assert!(
        !took(&thread, Signal::SIGXCPU),
        "a second SIGXCPU for the same second"
    );

    thread.sys_time_add(1_000_000_000);
    run_cpu_timers(&thread);
    assert!(
        took(&thread, Signal::SIGXCPU),
        "kernel time is CPU time too"
    );
    assert_eq!(soft(), 3);
    assert!(lp.itimers().cpu_armed());
}

/// The hard limit ends the process, whatever it does about SIGXCPU.
#[test]
fn rlimit_cpu_kills_at_the_hard_limit() {
    let (proc, thread) = a_process(45_144);
    let lp = proc.linux();
    lp.rlimit(RLIMIT_CPU, Some(RLimit { cur: 1, max: 2 }), true)
        .unwrap();
    lp.itimers().watch_cpu_limit();
    thread.time_add(2_000_000_000);
    run_cpu_timers(&thread);
    let killed = matches!(proc.status(), Status::Exited(_)) || took(&thread, Signal::SIGKILL);
    assert!(killed, "2 s of CPU against a hard limit of 2, and alive");
    assert!(
        !took(&thread, Signal::SIGXCPU),
        "SIGXCPU where the hard limit is due"
    );
}

/// fork(2): no interval timer is inherited, the CPU limit is, and the child
/// is watched against it from the start.
#[test]
fn a_fork_watches_the_cpu_limit_it_inherits() {
    let mut limits = linux_object::process::RLimits::default();
    assert!(!Itimers::forked(&limits).cpu_armed());
    limits.set(RLIMIT_CPU, RLimit { cur: 5, max: 10 }).unwrap();
    let child = Itimers::forked(&limits);
    assert!(child.cpu_armed());
    assert!(child.lock().iter().all(|slot| slot.deadline.is_none()));
}

/// A `timer_create(2)` timer on `clock`, owned by `owner`, disarmed.
fn a_cpu_timer(owner: KoID, clock: CpuClock, notify: TimerNotify) -> usize {
    let id = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
    POSIX_TIMERS.lock().insert(
        id,
        PosixTimer {
            owner,
            notify,
            clock: PosixTimerClock::Cpu(clock),
            interval: Duration::ZERO,
            next: Duration::ZERO,
            generation: 0,
            overrun_last: 0,
        },
    );
    id
}

fn process_clock(proc: &Process) -> CpuClock {
    CpuClock {
        owner: CpuClockOwner::Process(proc.id() as usize),
        kind: CpuClockKind::Sched,
    }
}

/// `timer_settime` on CPU timer `id`, as `cpu_timer_settime` does it.
fn arm(
    proc: &Arc<Process>,
    id: usize,
    init: Duration,
    interval: Duration,
    abs: bool,
) -> ITimerSpec {
    let clock = match POSIX_TIMERS.lock()[&id].clock {
        PosixTimerClock::Cpu(clock) => clock,
        PosixTimerClock::Base(_) => unreachable!(),
    };
    let (target, now) = cpu_timer_target(proc, clock).expect("the clock's task is gone");
    set_cpu_timer(id, proc.id(), &target, now, abs, init, interval).unwrap()
}

/// `timer_gettime`'s `it_value`.
fn left(proc: &Arc<Process>, id: usize) -> Duration {
    let PosixTimerClock::Cpu(clock) = POSIX_TIMERS.lock()[&id].clock else {
        unreachable!()
    };
    let (_, now) = cpu_timer_target(proc, clock).unwrap();
    timespec_to_duration(cpu_timer_spec(&POSIX_TIMERS.lock()[&id], now).value)
}

#[test]
fn a_timer_on_the_process_clock_goes_off_at_its_cpu_time_and_not_before() {
    let (proc, thread) = a_process(45_145);
    let id = a_cpu_timer(
        proc.id(),
        process_clock(&proc),
        TimerNotify::SIGALRM_TO_PROCESS,
    );
    arm(&proc, id, ms(100), Duration::ZERO, false);
    assert!(proc.linux().itimers().posix_cpu_armed());

    run_cpu_timers(&thread);
    assert!(
        !took(&thread, Signal::SIGALRM),
        "no CPU used, and it went off"
    );
    thread.time_add(60_000_000);
    thread.sys_time_add(30_000_000);
    run_cpu_timers(&thread);
    assert!(
        !took(&thread, Signal::SIGALRM),
        "it went off at 90 ms of 100"
    );
    assert_eq!(left(&proc, id), ms(10), "timer_gettime: 10 ms left");

    thread.time_add(10_000_000);
    run_cpu_timers(&thread);
    let info = thread.lock_linux().take_siginfo(Signal::SIGALRM);
    assert!(
        info == SigInfo::timer(Signal::SIGALRM, id as i32, 0, 0),
        "no SI_TIMER SIGALRM at 100 ms of CPU"
    );
    assert_eq!(left(&proc, id), Duration::ZERO, "a one-shot left armed");
    assert!(
        !proc.linux().itimers().posix_cpu_armed(),
        "nothing left, and still looked at"
    );
    drop_posix_timers_of(proc.id());
}

/// The process clock is every thread's time together, whichever thread
/// comes back to find the timer due.
#[test]
fn a_timer_on_the_process_clock_counts_every_thread() {
    let (proc, first) = a_process(45_155);
    let second = Thread::create_linux(&proc).unwrap();
    let id = a_cpu_timer(
        proc.id(),
        process_clock(&proc),
        TimerNotify::SIGALRM_TO_PROCESS,
    );
    arm(&proc, id, ms(100), Duration::ZERO, false);
    first.time_add(60_000_000);
    second.time_add(60_000_000);
    run_cpu_timers(&second);
    assert_eq!(
        left(&proc, id),
        Duration::ZERO,
        "120 ms between two threads did not reach 100"
    );
    let pending = took(&first, Signal::SIGALRM) | took(&second, Signal::SIGALRM);
    assert!(pending, "no SIGALRM");
    drop_posix_timers_of(proc.id());
}

/// A thread's clock is its own time: another thread's running neither
/// moves it nor takes the timer off the list to look at.
#[test]
fn a_timer_on_a_thread_clock_counts_that_thread_alone() {
    let (proc, mine) = a_process(45_146);
    let other = Thread::create_linux(&proc).unwrap();
    let clock = CpuClock {
        owner: CpuClockOwner::Thread(mine.id() as usize),
        kind: CpuClockKind::Sched,
    };
    // Go's profiler: SIGPROF to the very thread whose clock it is.
    let notify = TimerNotify {
        signo: Signal::SIGPROF as usize,
        value: 7,
        thread: Some(mine.id()),
    };
    let id = a_cpu_timer(proc.id(), clock, notify);
    arm(&proc, id, ms(10), ms(10), false);

    other.time_add(1_000_000_000);
    run_cpu_timers(&other);
    assert!(
        !took(&mine, Signal::SIGPROF),
        "another thread's second of CPU"
    );
    assert!(!took(&other, Signal::SIGPROF));
    assert!(
        proc.linux().itimers().posix_cpu_armed(),
        "the other thread's look stopped the timer being looked at"
    );
    run_cpu_timers(&mine);
    assert!(
        !took(&mine, Signal::SIGPROF),
        "no CPU of its own, and it went off"
    );

    mine.time_add(10_000_000);
    run_cpu_timers(&mine);
    assert!(
        took(&mine, Signal::SIGPROF),
        "no SIGPROF at 10 ms of its CPU"
    );
    assert!(
        !took(&other, Signal::SIGPROF),
        "the signal went to the other thread"
    );
    assert_eq!(left(&proc, id), ms(10), "the period did not start again");
    drop_posix_timers_of(proc.id());
}

/// `bump_cpu_timer`: one signal for a timer that fell behind, the periods it
/// skipped in its overrun count, and the next expiry past the clock.
#[test]
fn a_periodic_cpu_timer_that_fell_behind_goes_off_once_with_an_overrun() {
    let (proc, thread) = a_process(45_147);
    let id = a_cpu_timer(
        proc.id(),
        process_clock(&proc),
        TimerNotify::SIGALRM_TO_PROCESS,
    );
    arm(&proc, id, ms(10), ms(10), false);
    thread.time_add(35_000_000);
    run_cpu_timers(&thread);
    let info = thread.lock_linux().take_siginfo(Signal::SIGALRM);
    assert!(
        info == SigInfo::timer(Signal::SIGALRM, id as i32, 2, 0),
        "not one SIGALRM with 2 overruns for the expiries at 10, 20 and 30 ms"
    );
    assert_eq!(POSIX_TIMERS.lock()[&id].overrun_last, 2, "timer_getoverrun");
    assert_eq!(left(&proc, id), ms(5), "next expiry not at 40 ms");
    run_cpu_timers(&thread);
    assert!(
        !took(&thread, Signal::SIGALRM),
        "a second signal for the same expiries"
    );
    drop_posix_timers_of(proc.id());
}

/// `TIMER_ABSTIME` on a CPU clock is a reading of that clock; one already
/// gone by goes off at once (`cpu_timer_settime` looks right after arming),
/// and `timer_gettime` says a due timer is about to go off, not disarmed.
#[test]
fn an_absolute_cpu_expiry_is_a_reading_of_the_clock() {
    let (proc, thread) = a_process(45_148);
    thread.time_add(20_000_000);
    let id = a_cpu_timer(
        proc.id(),
        process_clock(&proc),
        TimerNotify::SIGALRM_TO_PROCESS,
    );
    arm(&proc, id, ms(50), Duration::ZERO, true);
    assert_eq!(
        left(&proc, id),
        ms(30),
        "an absolute 50 ms read as a length"
    );

    let old = arm(&proc, id, ms(5), Duration::ZERO, true);
    assert_eq!(timespec_to_duration(old.value), ms(30));
    assert_eq!(
        left(&proc, id),
        Duration::from_nanos(1),
        "a due timer reads as disarmed"
    );
    run_cpu_timers(&thread);
    assert!(took(&thread, Signal::SIGALRM), "a past expiry did not fire");
    drop_posix_timers_of(proc.id());
}

/// A timer may watch another process's clock (`clock_getcpuclockid`): that
/// process running moves it, and the signal goes to the timer's owner.
#[test]
fn a_timer_on_another_process_clock_signals_its_owner() {
    let (owner, owner_thread) = a_process(45_149);
    let (watched, worker) = a_process(45_150);
    let id = a_cpu_timer(
        owner.id(),
        process_clock(&watched),
        TimerNotify::SIGALRM_TO_PROCESS,
    );
    let (target, _) = cpu_timer_target(&owner, process_clock(&watched)).unwrap();
    assert_eq!(target.id(), watched.id());
    arm(&owner, id, ms(10), Duration::ZERO, false);
    assert!(watched.linux().itimers().posix_cpu_armed());
    assert!(!owner.linux().itimers().posix_cpu_armed());

    owner_thread.time_add(50_000_000);
    run_cpu_timers(&owner_thread);
    assert!(
        !took(&owner_thread, Signal::SIGALRM),
        "the owner's own CPU moved it"
    );
    worker.time_add(10_000_000);
    run_cpu_timers(&worker);
    assert!(
        took(&owner_thread, Signal::SIGALRM),
        "the owner got nothing"
    );
    assert!(
        !took(&worker, Signal::SIGALRM),
        "the watched process got it"
    );
    drop_posix_timers_of(owner.id());
}

/// `pid_for_clock` for a timer: a thread clock names a thread of the
/// caller's own process, and a clock whose task is gone reads as nothing
/// (`timer_settime`'s ESRCH). A zombie is still a task, with its clock
/// stopped; a child its parent has collected is not, even while it is
/// still in its job finishing its teardown.
#[test]
fn a_cpu_clock_names_a_task_until_it_is_reaped() {
    let (proc, _thread) = a_process(45_151);
    let (other, other_thread) = a_process(45_152);
    let theirs = CpuClock {
        owner: CpuClockOwner::Thread(other_thread.id() as usize),
        kind: CpuClockKind::Sched,
    };
    assert!(cpu_timer_target(&other, theirs).is_some());
    assert!(
        cpu_timer_target(&proc, theirs).is_none(),
        "a thread of another process"
    );
    let nobody = CpuClock {
        owner: CpuClockOwner::Process(45_153),
        kind: CpuClockKind::Sched,
    };
    assert!(cpu_timer_target(&proc, nobody).is_none());
    let child = Process::fork_from(&proc).unwrap();
    let child_thread = Thread::create_linux(&child).unwrap();
    let clock = process_clock(&child);
    assert!(cpu_timer_target(&proc, clock).is_some());
    child.exit(0);
    assert!(cpu_timer_target(&proc, clock).is_some(), "a zombie");
    block_on(wait_child(&proc, child.id(), true, true)).unwrap();
    assert!(
        ROOT_JOB.find_process(child.id()).is_some(),
        "the case this is about: collected, still in its job"
    );
    assert!(
        cpu_timer_target(&proc, clock).is_none(),
        "a child its parent has collected"
    );
    child_thread.terminate_abandoned();
}
