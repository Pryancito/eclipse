//! The wait family through the syscall: the four `idtype`s of `waitid(2)`,
//! the three `wait4(2)` targets that are not a plain pid, and the two
//! out-pointers both calls hand back.
//!
//! `sys_waitid` had never been called. Its pure helpers were all tested --
//! `waitid_options`, `waitid_report`, `waitid_id`, `pidfd_waitid_empty` --
//! but the function that wires them to `do_wait` was 62 of 62 lines at
//! zero, so nothing proved that `P_PGID` reads a group rather than a pid,
//! that `P_PIDFD` refuses a descriptor on somebody else's process, that
//! `WNOWAIT` leaves the child waitable, or that `infop` is written whether
//! or not a child was found. `sys_wait4` had only ever been called with a
//! positive pid, so its `-1`, `0` and `-pgid` targets were at zero too, and
//! so was its `WNOHANG` tail.
//!
//! A live child may only be waited for with `WNOHANG` or through an
//! `O_NONBLOCK` pidfd: everything here runs on one thread, so a blocking
//! wait would never be woken and would hang the suite rather than fail.
//! Fixed pids live in 48_3xx.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{set_process_pgid, ChildCpu, ProcessExt};
use linux_object::signal::Signal as LinuxSignal;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const EAGAIN: isize = -(LxError::EAGAIN as isize);
const ECHILD: isize = -(LxError::ECHILD as isize);
const EINVAL: isize = -(LxError::EINVAL as isize);
const ESRCH: isize = -(LxError::ESRCH as isize);

/// `<linux/wait.h>`'s `idtype_t`, written out here rather than read back
/// through the kernel's own constants: a number that drifted would still
/// match itself, every test would stay green, and the only thing that would
/// break is the libc on the other side, which sends the number. `P_PIDFD`
/// was once 5 in this kernel, so `waitid(P_PIDFD, ..)` -- what glib's
/// `g_child_watch_source_new()` calls -- matched no arm at all.
const P_ALL: i32 = 0;
/// See [`P_ALL`].
const P_PID: i32 = 1;
/// See [`P_ALL`].
const P_PGID: i32 = 2;
/// See [`P_ALL`].
const P_PIDFD: i32 = 3;

/// The option bits a caller sends, also written out by hand.
const WNOHANG: usize = 1;
/// See [`WNOHANG`].
const WSTOPPED: usize = 2;
/// See [`WNOHANG`].
const WEXITED: usize = 4;
/// See [`WNOHANG`].
const WCONTINUED: usize = 8;
/// See [`WNOHANG`].
const WNOWAIT: usize = 0x0100_0000;
/// `O_NONBLOCK` as `pidfd_open(2)` takes it.
const PIDFD_NONBLOCK: usize = 1 << 11;

/// Where glibc reads `siginfo_t`'s `_sifields._sigchld` out of the struct.
const SI_PID: usize = 16;
/// See [`SI_PID`].
const SI_UID: usize = 20;
/// See [`SI_PID`].
const SI_STATUS: usize = 24;

fn word(info: &SigInfo, at: usize) -> i32 {
    let b = info.as_bytes();
    i32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// A report from an earlier call, sitting in the caller's buffer. Every
/// `waitid` must leave its own six fields there, and a `WNOHANG` that found
/// nothing must zero them rather than hand this back as if it were fresh.
fn a_stale_report() -> SigInfo {
    SigInfo::child_state_change(4242, 1000, 0x7f00)
}

/// An rusage from an earlier call. `waitid` writes it only when it reports a
/// child, so a call that found nothing must leave these 99 seconds alone.
fn a_stale_rusage() -> RUsage {
    RUsage::from(ChildCpu {
        utime_ns: 99_000_000_000,
        ..Default::default()
    })
}

fn a_process(pid: KoID) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "waits", lp).unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

/// A child whose one thread never runs, so it stays alive and nothing a
/// `WNOHANG` wait does can block.
fn a_live_child(parent: &Arc<Process>) -> Arc<Process> {
    let child = Process::fork_from(parent).unwrap();
    Thread::create_linux(&child).unwrap();
    child
}

/// A child that has already exited with `code`, so a wait finds it at once.
fn a_zombie_child(parent: &Arc<Process>, code: i64) -> KoID {
    let child = a_live_child(parent);
    let pid = child.id();
    child.exit(code);
    pid
}

async fn by_number(sys: &Syscall<'_>, num: Sys, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(num as u32, args).await
}

async fn waitid(
    sys: &Syscall<'_>,
    idtype: i32,
    id: usize,
    info: usize,
    options: usize,
    rusage: usize,
) -> isize {
    by_number(
        sys,
        Sys::WAITID,
        [idtype as usize, id, info, options, rusage, 0],
    )
    .await
}

async fn wait4(sys: &Syscall<'_>, pid: i32, wstatus: usize, options: usize) -> isize {
    by_number(sys, Sys::WAIT4, [pid as usize, wstatus, options, 0, 0, 0]).await
}

async fn pidfd_open(sys: &Syscall<'_>, pid: KoID, flags: usize) -> isize {
    by_number(sys, Sys::PIDFD_OPEN, [pid as usize, flags, 0, 0, 0, 0]).await
}

/// `waitid(P_PID)` on a child that has exited: the six fields of the report
/// and the child's rusage, and the child is gone afterwards.
///
/// The call answers 0, not the pid: that is the whole difference between
/// `waitid` and `wait4` for a caller, and the pid it found is only in
/// `infop`.
#[test]
fn waitid_by_pid_reports_the_child_in_infop_and_collects_it() {
    let parent = a_process(48_300);
    let pid = a_zombie_child(&parent, 7);
    let (first, info, rusage, second) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let mut info = a_stale_report();
            let mut rusage = a_stale_rusage();
            let ip = &mut info as *mut SigInfo as usize;
            let rp = &mut rusage as *mut RUsage as usize;
            let first = waitid(sys, P_PID, pid as usize, ip, WEXITED, rp).await;
            let second = waitid(sys, P_PID, pid as usize, ip, WEXITED, rp).await;
            (first, info, rusage, second)
        })
    });
    assert_eq!(first, 0, "waitid answers 0, and the pid goes in infop");
    assert_eq!(info.signo, LinuxSignal::SIGCHLD as i32, "si_signo");
    assert_eq!(word(&info, SI_PID), pid as i32, "si_pid");
    assert_eq!(word(&info, SI_UID), 0, "si_uid of a root child");
    assert_eq!(
        word(&info, SI_STATUS),
        7,
        "si_status is the code it exited with, not the whole status word"
    );
    assert_eq!(
        rusage.utime.sec, 0,
        "the child's own CPU time replaced the stale 99 s"
    );
    assert_eq!(second, ECHILD, "the first call collected it");
    parent.exit(0);
}

/// `waitid(P_ALL)` takes the children one at a time, whatever group they are
/// in -- one of these two left for a group of its own, and ALL means all --
/// and whatever it finds it writes `infop`: a `WNOHANG` that found nothing
/// zeroes the struct rather than leave the previous call's child in it, and
/// leaves the rusage alone.
#[test]
fn waitid_for_any_child_takes_one_at_a_time_and_writes_infop_either_way() {
    let parent = a_process(48_301);
    let first_pid = a_zombie_child(&parent, 1);
    let elsewhere = a_live_child(&parent);
    let second_pid = elsewhere.id();
    set_process_pgid(&parent, second_pid, second_pid).expect("a group of its own");
    elsewhere.exit(2);
    let alive = a_live_child(&parent);
    let (mut found, mut statuses, empty, stale_info, stale_rusage) =
        on_a_thread(&parent, move |sys| {
            Box::pin(async move {
                let mut info = a_stale_report();
                let mut rusage = a_stale_rusage();
                let ip = &mut info as *mut SigInfo as usize;
                let rp = &mut rusage as *mut RUsage as usize;
                let mut found = Vec::new();
                let mut statuses = Vec::new();
                for _ in 0..2 {
                    assert_eq!(waitid(sys, P_ALL, 0, ip, WEXITED, rp).await, 0);
                    found.push(word(&info, SI_PID) as KoID);
                    statuses.push(word(&info, SI_STATUS));
                }
                // Only the live child is left, so WNOHANG finds nothing. The
                // two calls above each wrote the rusage, so the stale value
                // goes back in: what is being asked is whether THIS call
                // leaves it alone.
                rusage = a_stale_rusage();
                let empty = waitid(sys, P_ALL, 0, ip, WEXITED | WNOHANG, rp).await;
                (found, statuses, empty, info, rusage)
            })
        });
    found.sort_unstable();
    let mut want = [first_pid, second_pid];
    want.sort_unstable();
    assert_eq!(found, want, "both children, one per call");
    statuses.sort_unstable();
    assert_eq!(statuses, [1, 2], "each with its own exit code");
    assert_eq!(
        empty, 0,
        "WNOHANG with nothing ready is success, not EAGAIN"
    );
    assert!(
        stale_info.as_bytes().iter().all(|&b| b == 0),
        "infop is zeroed when nothing was found, si_pid included: {:?}",
        stale_info.as_bytes()
    );
    assert_eq!(
        stale_rusage.utime.sec, 99,
        "the rusage is written only when a child is reported"
    );
    alive.exit(0);
    parent.exit(0);
}

/// `waitid(P_PGID)` reads its `id` as a GROUP. Two children in one group of
/// their own are both found by that group's number, while the number of one
/// of them -- a real child, a real pid -- names no group and finds nothing.
///
/// Zero is the caller's own group, which is the group the children were born
/// into and the one they left.
#[test]
fn waitid_for_a_group_reads_a_group_and_not_a_pid() {
    let parent = a_process(48_302);
    let leader = a_live_child(&parent);
    let joiner = a_live_child(&parent);
    let (lead_pid, join_pid) = (leader.id(), joiner.id());
    let stays = a_zombie_child(&parent, 5);
    set_process_pgid(&parent, lead_pid, lead_pid).expect("a child may start a group of its own");
    set_process_pgid(&parent, join_pid, lead_pid).expect("and another may join it");
    leader.exit(3);
    joiner.exit(4);

    let (by_group, by_a_childs_pid, own_group, after) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let mut info = a_stale_report();
            let ip = &mut info as *mut SigInfo as usize;
            let mut by_group = Vec::new();
            for _ in 0..2 {
                let r = waitid(sys, P_PGID, lead_pid as usize, ip, WEXITED, 0).await;
                assert_eq!(r, 0, "the group has two children in it");
                by_group.push(word(&info, SI_PID) as KoID);
            }
            let by_a_childs_pid =
                waitid(sys, P_PGID, join_pid as usize, ip, WEXITED | WNOHANG, 0).await;
            let own_group = waitid(sys, P_PGID, 0, ip, WEXITED, 0).await;
            let found = word(&info, SI_PID) as KoID;
            let after = waitid(sys, P_PGID, 0, ip, WEXITED | WNOHANG, 0).await;
            (by_group, by_a_childs_pid, (own_group, found), after)
        })
    });
    let mut got = by_group;
    got.sort_unstable();
    let mut want = [lead_pid, join_pid];
    want.sort_unstable();
    assert_eq!(got, want, "the group holds both of them");
    assert_eq!(
        by_a_childs_pid, ECHILD,
        "a child's pid is not a group, so there is no child in it"
    );
    assert_eq!(own_group, (0, stays), "zero is the caller's own group");
    assert_eq!(after, ECHILD, "and it held only that one");
    parent.exit(0);
}

/// `waitid(P_PIDFD)` on a descriptor for a process that is not the caller's
/// child is `ECHILD`, whatever the wait would otherwise have found: a pidfd
/// is not a permission.
///
/// And the pidfd's own `O_NONBLOCK` decides whether an empty wait blocks:
/// with the flag and no `WNOHANG` it is `EAGAIN`, with both it is success
/// and no child.
#[test]
fn waitid_through_a_pidfd_refuses_one_that_is_not_my_child() {
    let parent = a_process(48_303);
    let stranger = a_process(48_304);
    let stranger_pid = stranger.id();
    let zombie = a_zombie_child(&parent, 9);
    let alive = a_live_child(&parent);
    let alive_pid = alive.id();

    let (not_mine, blocking_empty, nohang_empty, mine) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let mut info = a_stale_report();
            let ip = &mut info as *mut SigInfo as usize;
            let foreign = pidfd_open(sys, stranger_pid, 0).await;
            assert!(foreign >= 0, "pidfd_open takes any pid: {}", foreign);
            let not_mine = waitid(sys, P_PIDFD, foreign as usize, ip, WEXITED, 0).await;

            let nb = pidfd_open(sys, alive_pid, PIDFD_NONBLOCK).await;
            assert!(nb >= 0, "pidfd_open with O_NONBLOCK: {}", nb);
            let blocking_empty = waitid(sys, P_PIDFD, nb as usize, ip, WEXITED, 0).await;
            let nohang_empty = waitid(sys, P_PIDFD, nb as usize, ip, WEXITED | WNOHANG, 0).await;

            let plain = pidfd_open(sys, zombie, 0).await;
            assert!(plain >= 0, "pidfd_open on the zombie: {}", plain);
            let mine = waitid(sys, P_PIDFD, plain as usize, ip, WEXITED, 0).await;
            (
                not_mine,
                blocking_empty,
                nohang_empty,
                (mine, word(&info, SI_PID)),
            )
        })
    });
    assert_eq!(
        not_mine, ECHILD,
        "a pidfd on somebody else's process waits for nothing"
    );
    assert_eq!(
        blocking_empty, EAGAIN,
        "the pidfd's O_NONBLOCK forced the wait, and the caller did not ask for WNOHANG"
    );
    assert_eq!(
        nohang_empty, 0,
        "a caller that asked for WNOHANG gets success and no child"
    );
    assert_eq!(
        mine,
        (0, zombie as i32),
        "a plain pidfd on a child of mine reports it"
    );
    alive.exit(0);
    stranger.exit(0);
    parent.exit(0);
}

/// An `idtype` this kernel does not know is `EINVAL`, and so is an options
/// word that names none of the three things a `waitid` can report: a wait
/// for nothing at all, which Linux refuses rather than block on forever.
///
/// `WNOWAIT` leaves the child waitable, which is only visible from the
/// syscall: the same call made twice reports the same child twice, and it
/// takes one without the bit to collect it.
#[test]
fn waitid_refuses_an_unknown_idtype_and_wnowait_leaves_the_child_waitable() {
    let parent = a_process(48_305);
    let pid = a_zombie_child(&parent, 11);
    let alive = a_live_child(&parent);
    let alive_pid = alive.id();
    let (unknown, for_nothing, not_interested, not_ready, peeked, reaped, after) =
        on_a_thread(&parent, move |sys| {
            Box::pin(async move {
                let mut info = a_stale_report();
                let ip = &mut info as *mut SigInfo as usize;
                let mut unknown = Vec::new();
                for idtype in [4, 5, 99, -1] {
                    unknown.push(waitid(sys, idtype, pid as usize, ip, WEXITED, 0).await);
                }
                // Every bit is in waitid's mask, and none of them is one of
                // WEXITED, WSTOPPED or WCONTINUED.
                let for_nothing = waitid(sys, P_PID, pid as usize, ip, WNOHANG | WNOWAIT, 0).await;
                // Accepted, and not WEXITED: the same child, the same pid, and a
                // wait that is not interested in an exit does not find it.
                let not_interested = waitid(
                    sys,
                    P_PID,
                    pid as usize,
                    ip,
                    WSTOPPED | WCONTINUED | WNOHANG,
                    0,
                )
                .await;
                // A child of mine that has not changed state: WNOHANG is
                // success with no child, and si_pid says which it was.
                let not_ready = (
                    waitid(sys, P_PID, alive_pid as usize, ip, WEXITED | WNOHANG, 0).await,
                    word(&info, SI_PID),
                );
                let mut peeked = Vec::new();
                for _ in 0..2 {
                    let r = waitid(sys, P_PID, pid as usize, ip, WEXITED | WNOWAIT, 0).await;
                    peeked.push((r, word(&info, SI_STATUS)));
                }
                let reaped = waitid(sys, P_PID, pid as usize, ip, WEXITED, 0).await;
                let after = waitid(sys, P_PID, pid as usize, ip, WEXITED | WNOHANG, 0).await;
                (
                    unknown,
                    for_nothing,
                    not_interested,
                    not_ready,
                    peeked,
                    reaped,
                    after,
                )
            })
        });
    assert_eq!(unknown, [EINVAL; 4], "4, 5, 99 and -1 are no idtype");
    assert_eq!(
        for_nothing, EINVAL,
        "a word with no WEXITED, WSTOPPED or WCONTINUED is a wait for nothing"
    );
    assert_eq!(
        not_interested, ECHILD,
        "a wait for stops and continues alone does not see a child that exited"
    );
    assert_eq!(
        not_ready,
        (0, 0),
        "a live child of mine is success with no child, and infop is zeroed"
    );
    assert_eq!(
        peeked,
        [(0, 11), (0, 11)],
        "WNOWAIT reported the same child twice without collecting it"
    );
    assert_eq!(reaped, 0, "and a call without the bit collects it");
    assert_eq!(after, ECHILD, "once, and only once");
    alive.exit(0);
    parent.exit(0);
}

/// `wait4`'s three targets that are not a plain pid: `-1` is any child at
/// all, `0` is the caller's own process group, and `-n` is group `n`. A
/// child moved out of the caller's group answers the first and the third and
/// not the second.
///
/// `INT_MIN` is `ESRCH` before anything is negated: "-INT_MIN is not
/// defined", and negating it here used to be an overflow panic in a debug
/// kernel for any process calling `waitpid(INT_MIN, ..)`.
#[test]
fn wait4_reads_minus_one_zero_and_a_negative_group_as_three_sets() {
    let parent = a_process(48_306);
    let moved = a_live_child(&parent);
    let moved_pid = moved.id();
    set_process_pgid(&parent, moved_pid, moved_pid).expect("a group of its own");
    moved.exit(13);
    let kept = a_live_child(&parent);
    let kept_pid = kept.id();

    let (overflow, not_in_my_group, by_its_group, mine, any) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let mut status = -1i32;
            let sp = &mut status as *mut i32 as usize;
            let overflow = wait4(sys, i32::MIN, sp, WNOHANG).await;
            // The one child that has exited left for a group of its own, and
            // the one still in my group is alive.
            let not_in_my_group = wait4(sys, 0, sp, WNOHANG).await;
            let by_its_group = (
                wait4(sys, -(moved_pid as i32), sp, 0).await,
                (status >> 8) & 0xff,
            );
            // Now my own group has one ready as well.
            kept.exit(14);
            let mine = (wait4(sys, 0, sp, 0).await, (status >> 8) & 0xff);
            let any = wait4(sys, -1, sp, WNOHANG).await;
            (overflow, not_in_my_group, by_its_group, mine, any)
        })
    });
    assert_eq!(overflow, ESRCH, "INT_MIN is refused before it is negated");
    assert_eq!(
        not_in_my_group, 0,
        "the only child ready is in a group of its own, so my group has none"
    );
    assert_eq!(
        by_its_group,
        (moved_pid as isize, 13),
        "named by its own group it is found"
    );
    assert_eq!(
        mine,
        (kept_pid as isize, 14),
        "and zero is my group, which holds the other one"
    );
    assert_eq!(any, ECHILD, "-1 is any child, and there are none left");
    parent.exit(0);
}

/// `wait4` with `WNOHANG` and nothing ready answers 0 and leaves the status
/// word ALONE: a `while (waitpid(-1, &status, WNOHANG) > 0)` loop reads
/// `status` after the call that ended it, and zeroing it there turned every
/// such loop's last child into a clean exit.
///
/// An error is not that case: a pid that is no child of mine is `ECHILD`
/// even with `WNOHANG`.
#[test]
fn wait4_with_wnohang_and_nothing_ready_leaves_the_status_word_alone() {
    let parent = a_process(48_307);
    let alive = a_live_child(&parent);
    let stranger = a_process(48_308);
    let stranger_pid = stranger.id();
    let alive_pid = alive.id();

    let (nothing, kept, not_a_child, by_pid) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let mut status = 0x5a5a_5a5au32 as i32;
            let sp = &mut status as *mut i32 as usize;
            let nothing = wait4(sys, -1, sp, WNOHANG).await;
            let kept = status;
            let not_a_child = wait4(sys, stranger_pid as i32, sp, WNOHANG).await;
            let by_pid = wait4(sys, alive_pid as i32, sp, WNOHANG).await;
            (nothing, kept, not_a_child, by_pid)
        })
    });
    assert_eq!(nothing, 0, "WNOHANG with a live child only is 0");
    assert_eq!(
        kept, 0x5a5a_5a5au32 as i32,
        "and the caller's status word is untouched"
    );
    assert_eq!(not_a_child, ECHILD, "a stranger's pid is no child of mine");
    assert_eq!(
        by_pid, 0,
        "a live child of mine by its own pid is 0 as well"
    );
    alive.exit(0);
    stranger.exit(0);
    parent.exit(0);
}
