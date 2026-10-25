//! A zombie is its leader to the scheduling calls until it is reaped: what
//! they read is what the leader died with, the setters change it, and a
//! pid whose leader exited before the rest of its group names that leader.
//!
//! Fixed pids live in 48_1xx here.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use async_std::task::block_on;
use linux_object::process::{wait_child, LinuxProcess, PRIO_PGRP, PRIO_PROCESS, PRIO_USER};
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{ROOT_JOB, SCHED_BATCH, SCHED_FIFO, SCHED_IDLE};

const IOPRIO_WHO_PROCESS: usize = 1;
const IOPRIO_WHO_PGRP: usize = 2;
/// `IOPRIO_PRIO_VALUE(IOPRIO_CLASS_BE, level)`.
const fn best_effort(level: u16) -> u16 {
    (2 << 13) | level
}

fn a_process_of(pid: KoID, uid: u32) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    if uid != 0 {
        lp.set_resgid(uid, uid, uid).unwrap();
        lp.set_resuid(uid, uid, uid).unwrap();
        // A change of ids makes a process undumpable until it execs, as on
        // Linux; these stand for ones that started as that user.
        lp.set_dumpable(1);
    }
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "sched-task", lp).unwrap()
}

/// A child of `parent` whose leader was given what `set` gives it and then
/// died: a zombie out of its job, which only its parent keeps.
fn a_zombie(parent: &Arc<Process>, set: impl FnOnce(&Thread)) -> KoID {
    let child = Process::fork_from(parent).unwrap();
    let leader = Thread::create_linux(&child).unwrap();
    assert_eq!(
        leader.id(),
        child.id(),
        "the premise: the leader wears the pid"
    );
    set(&leader);
    child.exit(0);
    leader.terminate_abandoned();
    let pid = child.id();
    assert!(
        parent.job().find_process(pid).is_none(),
        "the case these tests are about: it has left its job"
    );
    pid
}

fn reap(parent: &Arc<Process>, pid: KoID) {
    block_on(wait_child(parent, pid, true, true)).unwrap();
}

#[test]
fn a_zombie_answers_the_sched_reads_with_what_its_leader_died_with() {
    let parent = a_process_of(48_101, 0);
    // Its leader, so that it outlives the first call's thread.
    let keep = Thread::create_linux(&parent).unwrap();
    let pid = a_zombie(&parent, |leader| {
        leader.set_sched(SCHED_BATCH, 7, 0);
        leader.set_sched_reset_on_fork(true);
        leader.set_affinity(0b11).unwrap();
    }) as usize;

    let read = move |sys: &Syscall<'_>| {
        let mut param = -1i32;
        let mut mask = [0u8; 8];
        let mut slice = TimeSpec { sec: 9, nsec: 9 };
        let mut attr = SchedAttr::default();
        (
            sys.sys_sched_getscheduler(pid),
            sys.sys_sched_getparam(pid, UserOutPtr::from(&mut param as *mut i32 as usize))
                .map(|_| param),
            sys.sys_sched_getaffinity(pid, 8, UserOutPtr::from(mask.as_mut_ptr() as usize))
                .map(|_| u64::from_le_bytes(mask)),
            sys.sys_sched_rr_get_interval(
                pid,
                UserOutPtr::from(&mut slice as *mut TimeSpec as usize),
            )
            .map(|_| (slice.sec, slice.nsec)),
            sys.sys_getpriority(PRIO_PROCESS, pid),
            sys.sys_sched_getattr(
                pid,
                UserOutPtr::from(&mut attr as *mut SchedAttr as usize),
                48,
                0,
            )
            .map(|_| (attr.sched_policy, attr.sched_nice, attr.sched_flags)),
        )
    };
    let got = on_a_thread(&parent, move |sys| Box::pin(async move { read(sys) }));
    let online = Syscall::online_cpu_mask();
    assert_eq!(
        got,
        (
            Ok(SCHED_BATCH as usize | SCHED_RESET_ON_FORK),
            Ok(0),
            Ok(0b11 & online),
            Ok((0, 0)),
            Ok(20 - 7),
            Ok((SCHED_BATCH as u32, 7, SCHED_FLAG_RESET_ON_FORK)),
        )
    );

    reap(&parent, pid as KoID);
    let gone = on_a_thread(&parent, move |sys| Box::pin(async move { read(sys) }));
    assert_eq!(
        gone,
        (
            Err(LxError::ESRCH),
            Err(LxError::ESRCH),
            Err(LxError::ESRCH),
            Err(LxError::ESRCH),
            Err(LxError::ESRCH),
            Err(LxError::ESRCH),
        ),
        "reaped, the pid names nothing"
    );
    keep.terminate_abandoned();
    parent.exit(0);
}

#[test]
fn the_setters_change_what_a_zombie_answers() {
    let parent = a_process_of(48_102, 0);
    let pid = a_zombie(&parent, |leader| {
        leader.set_sched(SCHED_BATCH, 7, 0);
        leader.set_sched_reset_on_fork(true);
        leader.set_affinity(0b10).unwrap();
    }) as usize;
    let zombie = linux_object::process::find_task(pid as KoID).unwrap();

    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let (thirty, idle) = (30i32, 0i32);
            let mut param = -1i32;
            let cpu0 = 1u64.to_le_bytes();
            (
                sys.sys_setpriority(PRIO_PROCESS, pid, 3),
                sys.sys_getpriority(PRIO_PROCESS, pid),
                sys.sys_sched_setscheduler(
                    pid,
                    SCHED_FIFO as usize,
                    UserInPtr::from(&thirty as *const i32 as usize),
                ),
                sys.sys_sched_getparam(pid, UserOutPtr::from(&mut param as *mut i32 as usize))
                    .map(|_| param),
                // Root may take `SCHED_RESET_ON_FORK` off, and a policy
                // without it does.
                sys.sys_sched_setscheduler(
                    pid,
                    SCHED_IDLE as usize,
                    UserInPtr::from(&idle as *const i32 as usize),
                ),
                sys.sys_sched_getscheduler(pid),
                sys.sys_sched_setaffinity(pid, 8, UserInPtr::from(cpu0.as_ptr() as usize))
                    .await,
            )
        })
    });
    assert_eq!(
        got,
        (
            Ok(0),
            Ok(20 - 3),
            Ok(0),
            Ok(30),
            Ok(0),
            Ok(SCHED_IDLE as usize),
            Ok(0)
        )
    );
    let filed = zombie.leader_sched().unwrap();
    assert_eq!(
        (filed.policy, filed.nice, filed.affinity),
        (SCHED_IDLE, 3, 1),
        "what the setters left on file"
    );
    drop(zombie);
    reap(&parent, pid as KoID);
    parent.exit(0);
}

#[test]
fn a_zombie_s_io_priority_is_none_and_its_group_s_the_level_its_nice_is_served_at() {
    let parent = a_process_of(48_103, 0);
    let pid = a_zombie(&parent, |leader| {
        leader.set_sched(SCHED_BATCH, 7, 0);
        leader.lock_linux().ioprio = best_effort(2);
    }) as usize;
    let zombie = linux_object::process::find_task(pid as KoID).unwrap();
    // A group of its own, so that the parent's thread is not in it.
    zombie.linux().set_pgid_raw(pid as u64);

    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            (
                sys.sys_ioprio_get(IOPRIO_WHO_PROCESS, pid),
                sys.sys_ioprio_get(IOPRIO_WHO_PGRP, pid),
                sys.sys_getpriority(PRIO_PGRP, pid),
                sys.sys_ioprio_set(IOPRIO_WHO_PROCESS, pid, best_effort(1) as usize),
                sys.sys_ioprio_get(IOPRIO_WHO_PROCESS, pid),
            )
        })
    });
    assert_eq!(
        got,
        (Ok(0), Ok(best_effort(5) as usize), Ok(20 - 7), Ok(0), Ok(0)),
        "none, the group's derived level and nice, a set that does nothing"
    );
    drop(zombie);
    reap(&parent, pid as KoID);
    parent.exit(0);
}

#[test]
fn a_zombie_counts_in_its_parent_s_group_and_in_its_user_s_tasks() {
    // A user of its own, so that nothing else in this binary is theirs.
    let uid = 48_104;
    let parent = a_process_of(48_104, uid);
    let pid = a_zombie(&parent, |leader| leader.set_sched(SCHED_BATCH, -5, 0));
    let zombie = linux_object::process::find_task(pid).unwrap();

    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            (
                sys.sys_getpriority(PRIO_PGRP, 0),
                sys.sys_getpriority(PRIO_USER, uid as usize),
                // Its owner may lower it, and the zombie is one of its tasks.
                sys.sys_setpriority(PRIO_USER, uid as usize, 10),
            )
        })
    });
    assert_eq!(
        got,
        (Ok(20 + 5), Ok(20 + 5), Ok(0)),
        "the zombie's nice -5 is the best in both, over the parent's 0"
    );
    assert_eq!(zombie.leader_sched().map(|s| s.nice), Some(10));
    // One task, and it wears the pid.
    let ids: Vec<KoID> = SchedTask::all_of(&zombie)
        .iter()
        .map(SchedTask::id)
        .collect();
    assert_eq!(ids, [pid]);
    drop(zombie);
    reap(&parent, pid);
    parent.exit(0);
}

#[test]
fn a_pid_whose_leader_exited_first_names_that_leader() {
    let parent = a_process_of(48_105, 0);
    let child = Process::fork_from(&parent).unwrap();
    let leader = Thread::create_linux(&child).unwrap();
    let worker = Thread::create_linux(&child).unwrap();
    leader.set_sched(SCHED_BATCH, 3, 0);
    worker.set_sched(SCHED_BATCH, 7, 0);
    child.linux().keep_exited_leader(leader.clone());
    leader.terminate_abandoned();
    drop(leader);
    // A group of its own, so that the parent's thread is not in it.
    child.linux().set_pgid_raw(child.id());
    let (pid, tid) = (child.id() as usize, worker.id() as usize);

    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            (
                sys.sys_getpriority(PRIO_PROCESS, pid),
                sys.sys_getpriority(PRIO_PROCESS, tid),
                sys.sys_getpriority(PRIO_PGRP, pid),
            )
        })
    });
    assert_eq!(
        got,
        (Ok(20 - 3), Ok(20 - 7), Ok(20 - 3)),
        "the pid is the leader, the tid the worker, the group the best of both"
    );
    worker.terminate_abandoned();
    reap(&parent, pid as KoID);
    parent.exit(0);
}

#[test]
fn a_zombie_s_robust_list_is_an_empty_head_of_the_struct_s_length() {
    let parent = a_process_of(48_106, 0);
    let pid = a_zombie(&parent, |leader| {
        leader.set_robust_list(UserInPtr::from(0x6000_0000usize), size_of::<RobustList>())
    });
    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let mut head = 0xdead_beefusize;
            let mut len = 0xdead_beefusize;
            let r = sys.sys_get_robust_list(
                pid as i32,
                UserOutPtr::from(&mut head as *mut usize as usize),
                UserOutPtr::from(&mut len as *mut usize as usize),
            );
            (r, head, len)
        })
    });
    // `futex_cleanup` cleared it at the exit, and the length is the
    // struct's whatever was registered.
    assert_eq!(got, (Ok(0), 0, size_of::<RobustList>()));
    reap(&parent, pid);
    parent.exit(0);
}

#[test]
fn another_user_may_not_read_where_a_task_keeps_its_robust_list() {
    let root = a_process_of(48_107, 0);
    let root_leader = Thread::create_linux(&root).unwrap();
    root_leader.set_robust_list(UserInPtr::from(0x6000_0000usize), size_of::<RobustList>());
    // The caller's own process made itself undumpable: its own tasks are
    // still its own to read (`same_thread_group`).
    let stranger = a_process_of(48_108, 1000);
    let stranger_leader = Thread::create_linux(&stranger).unwrap();
    stranger.linux().set_dumpable(0);
    let own = a_process_of(48_109, 1000);
    let own_leader = Thread::create_linux(&own).unwrap();
    // The same user's, made undumpable: out of reach like root's.
    let hardened = a_process_of(48_112, 1000);
    let hardened_leader = Thread::create_linux(&hardened).unwrap();
    hardened.linux().set_dumpable(0);
    let pids = [&root, &own, &hardened, &stranger].map(|p| p.id() as i32);

    let got = on_a_thread(&stranger, move |sys| {
        Box::pin(async move {
            pids.map(|pid| {
                let mut words = [0xdead_beefusize; 2];
                let r = sys.sys_get_robust_list(
                    pid,
                    UserOutPtr::from(&mut words[0] as *mut usize as usize),
                    UserOutPtr::from(&mut words[1] as *mut usize as usize),
                );
                (r, words)
            })
        })
    });
    let refused = (Err(LxError::EPERM), [0xdead_beef; 2]);
    let read = (Ok(0), [0, size_of::<RobustList>()]);
    assert_eq!(
        got,
        [refused, read, refused, read],
        "root's, the same user's, the same user's undumpable one, its own"
    );
    for leader in [root_leader, own_leader, hardened_leader, stranger_leader] {
        leader.terminate_abandoned();
    }
}

/// A process whose leader is there again -- an `execve` from another thread
/// gave the pid to a new one -- is its threads and nothing more, though the
/// old leader left something on file.
#[test]
fn a_new_leader_is_no_second_task_beside_what_the_old_one_left() {
    let proc = a_process_of(48_110, 0);
    let leader = Thread::create_linux(&proc).unwrap();
    let worker = Thread::create_linux(&proc).unwrap();
    leader.set_sched(SCHED_BATCH, 7, 0);
    leader.terminate_abandoned();
    assert!(proc.leader_sched().is_some(), "the premise");
    let successor = worker.successor_as_leader().unwrap();

    let tasks = SchedTask::all_of(&proc);
    let mut ids: Vec<KoID> = tasks.iter().map(SchedTask::id).collect();
    ids.sort_unstable();
    let mut want = alloc::vec![proc.id(), worker.id()];
    want.sort_unstable();
    assert_eq!(ids, want);
    assert!(tasks.iter().all(|t| matches!(t, SchedTask::Thread(_))));
    match SchedTask::of_process(proc.clone(), proc.id()) {
        Some(SchedTask::Thread(t)) => assert!(Arc::ptr_eq(&t, &successor)),
        _ => panic!("the pid named something other than the new leader"),
    }
    worker.terminate_abandoned();
    successor.terminate_abandoned();
}

/// A process that never had a thread wearing its pid has nothing on file,
/// and is no task: `ESRCH`, not an answer made up from the defaults.
#[test]
fn a_process_with_no_leader_on_file_is_no_task() {
    let parent = a_process_of(48_111, 0);
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    child.exit(0);
    drop(child);
    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move { sys.sys_getpriority(PRIO_PROCESS, pid as usize) })
    });
    assert_eq!(got, Err(LxError::ESRCH));
    reap(&parent, pid);
    parent.exit(0);
}

/// A leader that exited before the rest of its group has no I/O priority:
/// `do_exit` let its `io_context` go. It read the word it had set, and a
/// group walk served the group at that word instead of at its nice.
#[test]
fn a_leader_that_exited_first_has_no_io_priority() {
    let parent = a_process_of(48_112, 0);
    let child = Process::fork_from(&parent).unwrap();
    let leader = Thread::create_linux(&child).unwrap();
    let worker = Thread::create_linux(&child).unwrap();
    leader.set_sched(SCHED_BATCH, 7, 0);
    worker.set_sched(SCHED_BATCH, 7, 0);
    leader.lock_linux().ioprio = best_effort(2);
    child.linux().keep_exited_leader(leader.clone());
    leader.terminate_abandoned();
    drop(leader);
    // A group of its own, so that the parent's thread is not in it.
    child.linux().set_pgid_raw(child.id());
    let pid = child.id() as usize;

    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            (
                sys.sys_ioprio_get(IOPRIO_WHO_PROCESS, pid),
                sys.sys_ioprio_get(IOPRIO_WHO_PGRP, pid),
                sys.sys_ioprio_set(IOPRIO_WHO_PROCESS, pid, best_effort(1) as usize),
                sys.sys_ioprio_get(IOPRIO_WHO_PROCESS, pid),
            )
        })
    });
    assert_eq!(
        got,
        (Ok(0), Ok(best_effort(5) as usize), Ok(0), Ok(0)),
        "none, the group at nice 7's level, a set that does nothing"
    );
    worker.terminate_abandoned();
    reap(&parent, pid as KoID);
    parent.exit(0);
}

/// From the moment its exit is on file, a process is its leader alone:
/// Linux lets its parent wait for it only once every other thread has been
/// released. Here those threads were still on the list while they died,
/// and a group walk counted them, a tid found them, and the leader read the
/// I/O priority word it had set.
#[test]
fn a_zombie_is_its_leader_alone_while_its_other_threads_die() {
    let parent = a_process_of(48_113, 0);
    let child = Process::fork_from(&parent).unwrap();
    let leader = Thread::create_linux(&child).unwrap();
    let worker = Thread::create_linux(&child).unwrap();
    leader.set_sched(SCHED_BATCH, 7, 0);
    leader.lock_linux().ioprio = best_effort(2);
    worker.set_sched(SCHED_BATCH, 3, 0);
    worker.lock_linux().ioprio = best_effort(1);
    child.linux().set_pgid_raw(child.id());
    // Both threads killed and still on the list: the window.
    child.exit(0);
    let (pid, tid) = (child.id() as usize, worker.id() as usize);

    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            (
                sys.sys_getpriority(PRIO_PGRP, pid),
                sys.sys_ioprio_get(IOPRIO_WHO_PGRP, pid),
                sys.sys_ioprio_get(IOPRIO_WHO_PROCESS, pid),
                sys.sys_getpriority(PRIO_PROCESS, pid),
                sys.sys_getpriority(PRIO_PROCESS, tid),
                sys.sys_ioprio_get(IOPRIO_WHO_PROCESS, tid),
            )
        })
    });
    assert_eq!(
        got,
        (
            Ok(20 - 7),
            Ok(best_effort(5) as usize),
            Ok(0),
            Ok(20 - 7),
            Err(LxError::ESRCH),
            Err(LxError::ESRCH),
        ),
        "the leader's nice and level, no word of its own, and no worker"
    );
    let ids: Vec<KoID> = SchedTask::all_of(&child)
        .iter()
        .map(SchedTask::id)
        .collect();
    assert_eq!(ids, [pid as KoID]);
    worker.terminate_abandoned();
    leader.terminate_abandoned();
    drop(child);
    reap(&parent, pid as KoID);
    parent.exit(0);
}
