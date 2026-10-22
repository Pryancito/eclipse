//! A pid names its process from the fork to the reap, a zombie included, and
//! nothing after: the calls that take a pid, made by number from the parent.
//!
//! A `wait4` does not wait for the child's last thread to finish dying, so
//! the child can still be in its job after it was collected, and the calls
//! that looked there found it. In QEMU, right after the `waitpid`: its CPU
//! clock, `prlimit` and `capget` nearly every time, the `sched_*` calls,
//! `getpriority` and `ioprio_get` a third of the time, `tgkill` now and
//! then. Fixed pids live in 47_8xx here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::ProcessExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const EINVAL: isize = -(LxError::EINVAL as isize);
const EPERM: isize = -(LxError::EPERM as isize);
const ESRCH: isize = -(LxError::ESRCH as isize);
const RLIMIT_NOFILE: usize = 7;
const CAP_V3: u32 = 0x2008_0522;

fn a_process(pid: KoID) -> Arc<Process> {
    a_process_of(pid, 0)
}

fn a_process_of(pid: KoID, uid: u32) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    if uid != 0 {
        lp.set_resgid(uid, uid, uid).unwrap();
        lp.set_resuid(uid, uid, uid).unwrap();
    }
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "collected", lp).unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

/// `MAKE_PROCESS_CPUCLOCK(pid, CPUCLOCK_SCHED)`, as `clock_getcpuclockid`
/// makes it.
fn cpu_clock(pid: KoID) -> usize {
    (!(pid as i32) << 3 | 2) as usize
}

async fn by_number(sys: &Syscall<'_>, num: Sys, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(num as u32, args).await
}

/// The three calls that send a signal to one task, each a probe (signal
/// 0): `tgkill`, `tkill` and `rt_tgsigqueueinfo` with an `SI_QUEUE`.
async fn signal_probes(sys: &Syscall<'_>, pid: KoID) -> [isize; 3] {
    let p = pid as usize;
    let mut info = [0u8; 128];
    info[8..12].copy_from_slice(&(-1i32).to_ne_bytes());
    let info_ptr = info.as_mut_ptr() as usize;
    [
        by_number(sys, Sys::TGKILL, [p, p, 0, 0, 0, 0]).await,
        by_number(sys, Sys::TKILL, [p, 0, 0, 0, 0, 0]).await,
        by_number(sys, Sys::RT_TGSIGQUEUEINFO, [p, p, 0, info_ptr, 0, 0]).await,
    ]
}

/// What each call that takes a pid answers about `pid`, in the order of
/// [`NAMES`].
async fn ask(sys: &Syscall<'_>, pid: KoID) -> Vec<isize> {
    let p = pid as usize;
    let mut ts = [0u64; 2];
    let mut res = [0u64; 2];
    let mut timer = 0i32;
    let mut lim = [0u64; 2];
    let mut cap_header = [CAP_V3, pid as u32];
    let mut caps = [0u32; 6];
    let mut param = 0i32;
    let mut mask = [0u8; 128];
    let mut slice = [0u64; 2];
    let ts_ptr = ts.as_mut_ptr() as usize;
    let res_ptr = res.as_mut_ptr() as usize;
    let timer_ptr = &mut timer as *mut i32 as usize;
    let lim_ptr = lim.as_mut_ptr() as usize;
    let header_ptr = cap_header.as_mut_ptr() as usize;
    let caps_ptr = caps.as_mut_ptr() as usize;
    let param_ptr = &mut param as *mut i32 as usize;
    let mask_ptr = mask.as_mut_ptr() as usize;
    let slice_ptr = slice.as_mut_ptr() as usize;
    let clock = cpu_clock(pid);
    let made = by_number(sys, Sys::TIMER_CREATE, [clock, 0, timer_ptr, 0, 0, 0]).await;
    if made == 0 {
        let deleted = by_number(sys, Sys::TIMER_DELETE, [timer as usize, 0, 0, 0, 0, 0]).await;
        assert_eq!(deleted, 0);
    }
    let signals = signal_probes(sys, pid).await;
    vec![
        by_number(sys, Sys::CLOCK_GETTIME, [clock, ts_ptr, 0, 0, 0, 0]).await,
        by_number(sys, Sys::CLOCK_GETRES, [clock, res_ptr, 0, 0, 0, 0]).await,
        made,
        by_number(sys, Sys::PRLIMIT64, [p, RLIMIT_NOFILE, 0, lim_ptr, 0, 0]).await,
        by_number(sys, Sys::CAPGET, [header_ptr, caps_ptr, 0, 0, 0, 0]).await,
        by_number(sys, Sys::SCHED_GETSCHEDULER, [p, 0, 0, 0, 0, 0]).await,
        by_number(sys, Sys::SCHED_GETPARAM, [p, param_ptr, 0, 0, 0, 0]).await,
        by_number(
            sys,
            Sys::SCHED_GETAFFINITY,
            [p, mask.len(), mask_ptr, 0, 0, 0],
        )
        .await,
        by_number(sys, Sys::SCHED_RR_GET_INTERVAL, [p, slice_ptr, 0, 0, 0, 0]).await,
        by_number(sys, Sys::GETPRIORITY, [0, p, 0, 0, 0, 0]).await,
        by_number(sys, Sys::IOPRIO_GET, [1, p, 0, 0, 0, 0]).await,
        signals[0],
        signals[1],
        signals[2],
    ]
}

const NAMES: [&str; 14] = [
    "clock_gettime",
    "clock_getres",
    "timer_create",
    "prlimit",
    "capget",
    "sched_getscheduler",
    "sched_getparam",
    "sched_getaffinity",
    "sched_rr_get_interval",
    "getpriority",
    "ioprio_get",
    "tgkill",
    "tkill",
    "rt_tgsigqueueinfo",
];

/// The calls that found `pid`, by name.
fn found_by(got: &[isize]) -> Vec<&'static str> {
    NAMES
        .iter()
        .zip(got)
        .filter(|(_, &r)| r >= 0)
        .map(|(name, _)| *name)
        .collect()
}

async fn reap(sys: &Syscall<'_>, pid: KoID) -> isize {
    by_number(sys, Sys::WAIT4, [pid as usize, 0, 0, 0, 0, 0]).await
}

/// Every call finds a child that is alive, and what they answer is what
/// they answer about anybody: the probe itself is sound.
#[test]
fn a_live_child_is_found_by_every_call() {
    let parent = a_process(47_801);
    let child = Process::fork_from(&parent).unwrap();
    Thread::create_linux(&child).unwrap();
    let pid = child.id();
    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move { ask(sys, pid).await })
    });
    assert_eq!(found_by(&got), NAMES, "{:?}", got);
    assert_eq!(got[7], 8, "sched_getaffinity copies out one word");
    assert_eq!(got[9], 20, "getpriority of nice 0 is 20 - 0");
    child.exit(0);
    parent.exit(0);
}

/// Collected, the child names nothing -- while it is still in its job, which
/// is the case: its one thread never ran, so it never finished dying. Before
/// that, it is a zombie and every call still finds it.
#[test]
fn a_collected_child_still_in_its_job_names_nothing() {
    let parent = a_process(47_802);
    let child = Process::fork_from(&parent).unwrap();
    Thread::create_linux(&child).unwrap();
    let pid = child.id();
    child.exit(0);
    let job = parent.job();
    assert!(
        job.find_process(pid).is_some(),
        "the case this test is about"
    );

    let (zombie, reaped, collected) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            (
                ask(sys, pid).await,
                reap(sys, pid).await,
                ask(sys, pid).await,
            )
        })
    });
    assert_eq!(
        found_by(&zombie),
        NAMES,
        "a zombie in its job: {:?}",
        zombie
    );
    assert_eq!(reaped, pid as isize);
    assert!(
        job.find_process(pid).is_some(),
        "still in the job, which is the point"
    );
    assert_eq!(
        collected,
        [
            EINVAL, EINVAL, EINVAL, ESRCH, ESRCH, ESRCH, ESRCH, ESRCH, ESRCH, ESRCH, ESRCH, ESRCH,
            ESRCH, ESRCH,
        ],
        "found after the reap by {:?}",
        found_by(&collected)
    );
    drop(child);
    parent.exit(0);
}

/// A zombie that has left its job -- its last thread is gone, so its parent
/// keeps it -- is still a task to every call: its CPU clock, which has
/// stopped, a CPU timer on that clock, its limits, its capabilities, a
/// signal at its leader, which does nothing, and the `sched_*`,
/// `getpriority` and `ioprio_get`, which ask a THREAD. It has none left;
/// its leader's attributes stayed on file when it went, as Linux's leader
/// task lingers, and they answer with those. They were `ESRCH`.
#[test]
fn a_zombie_out_of_its_job_is_still_a_task() {
    let parent = a_process(47_803);
    let child = Process::fork_from(&parent).unwrap();
    let leader = Thread::create_linux(&child).unwrap();
    let pid = child.id();
    child.exit(0);
    leader.terminate_abandoned();
    drop((child, leader));
    assert!(
        parent.job().find_process(pid).is_none(),
        "the case this test is about"
    );

    let (zombie, reaped, collected) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            (
                ask(sys, pid).await,
                reap(sys, pid).await,
                ask(sys, pid).await,
            )
        })
    });
    assert_eq!(found_by(&zombie), NAMES, "{:?}", zombie);
    assert_eq!(reaped, pid as isize);
    assert_eq!(found_by(&collected), Vec::<&str>::new(), "{:?}", collected);
    parent.exit(0);
}

/// Of a process that has exited, only the leader is still a task: `tgkill`
/// at another of its threads is `ESRCH`, as is one that names the zombie as
/// the group of a tid that is not its own.
#[test]
fn of_a_zombie_only_the_leader_takes_a_tgkill() {
    let parent = a_process(47_804);
    let child = Process::fork_from(&parent).unwrap();
    Thread::create_linux(&child).unwrap();
    let other = Thread::create_linux(&child).unwrap().id();
    let pid = child.id();
    let me = parent.id();
    child.exit(0);
    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let (p, o, m) = (pid as usize, other as usize, me as usize);
            [
                by_number(sys, Sys::TGKILL, [p, p, 0, 0, 0, 0]).await,
                by_number(sys, Sys::TGKILL, [p, o, 0, 0, 0, 0]).await,
                by_number(sys, Sys::TGKILL, [p, m, 0, 0, 0, 0]).await,
            ]
        })
    });
    assert_eq!(got, [0, ESRCH, ESRCH]);
    drop(child);
    parent.exit(0);
}

/// A zombie is still a task to `check_kill_permission`: another user may
/// not even probe it, in its job or out of it.
#[test]
fn a_zombie_takes_a_signal_only_from_who_may_send_it() {
    let parent = a_process(47_805);
    let in_job = Process::fork_from(&parent).unwrap();
    Thread::create_linux(&in_job).unwrap();
    let out_of_job = Process::fork_from(&parent).unwrap();
    let (a, b) = (in_job.id(), out_of_job.id());
    in_job.exit(0);
    out_of_job.exit(0);
    drop(out_of_job);
    let stranger = a_process_of(47_806, 1000);
    let got = on_a_thread(&stranger, move |sys| {
        Box::pin(async move { (signal_probes(sys, a).await, signal_probes(sys, b).await) })
    });
    assert_eq!(got, ([EPERM; 3], [EPERM; 3]));
    let mine = on_a_thread(&parent, move |sys| {
        Box::pin(async move { (signal_probes(sys, a).await, signal_probes(sys, b).await) })
    });
    assert_eq!(mine, ([0; 3], [0; 3]));
    drop(in_job);
    stranger.exit(0);
    parent.exit(0);
}
