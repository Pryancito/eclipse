//! ITIMER_VIRTUAL, ITIMER_PROF and `RLIMIT_CPU` count the process's CPU
//! time. The two itimers ticked on the wall clock, from the timer wheel, and
//! nothing looked at the CPU limit at all.

use super::*;
use alloc::sync::Arc;
use alloc::vec::Vec;
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

    run_cpu_timers(&proc);
    assert!(
        !took(&thread, Signal::SIGPROF),
        "no CPU used, and SIGPROF came"
    );
    thread.time_add(60_000_000);
    thread.sys_time_add(30_000_000);
    run_cpu_timers(&proc);
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
    run_cpu_timers(&proc);
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
    run_cpu_timers(&proc);
    assert!(
        !took(&thread, Signal::SIGVTALRM),
        "kernel time moved ITIMER_VIRTUAL"
    );
    thread.time_add(100_000_000);
    run_cpu_timers(&proc);
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
    run_cpu_timers(&proc);
    assert!(
        !took(&thread, Signal::SIGXCPU),
        "SIGXCPU before the soft limit"
    );
    thread.time_add(300_000_000);
    run_cpu_timers(&proc);
    assert!(took(&thread, Signal::SIGXCPU), "no SIGXCPU at 1.2 s of 1");
    assert_eq!(soft(), 2, "getrlimit does not show the next one");
    run_cpu_timers(&proc);
    assert!(
        !took(&thread, Signal::SIGXCPU),
        "a second SIGXCPU for the same second"
    );

    thread.sys_time_add(1_000_000_000);
    run_cpu_timers(&proc);
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
    run_cpu_timers(&proc);
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
