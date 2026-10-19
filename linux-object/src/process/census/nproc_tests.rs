//! `RLIMIT_NPROC`: who counts toward a user's tasks, and when one more is
//! refused.
//!
//! Each test runs under a uid of its own, so the census of the whole machine
//! -- which is what `may_start_task` takes -- holds only what that test made.
//! Fixed pids live in 45_30x here; they are a global table, so every file
//! that pins one has its own range.

extern crate std;

use super::*;
use crate::thread::ThreadExt;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;

fn a_job() -> Arc<Job> {
    ROOT_JOB.create_child().unwrap()
}

/// A process of real uid `ruid` and effective uid `euid`, with `threads`
/// threads.
fn a_process(job: &Arc<Job>, pid: KoID, (ruid, euid): (u32, u32), threads: usize) -> Arc<Process> {
    let proc =
        Process::create_with_fixed_id_ext(job, pid, "nproc", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    for _ in 0..threads {
        Thread::create_linux(&proc).unwrap();
    }
    proc.linux().set_resuid(ruid, euid, euid).unwrap();
    proc
}

/// A soft limit of `tasks`, under a hard one above it, so that reading the
/// wrong one of the two shows.
fn limit(proc: &Arc<Process>, tasks: u64) {
    let new = RLimit {
        cur: tasks,
        max: tasks + 10,
    };
    proc.linux().rlimit(RLIMIT_NPROC, Some(new), true).unwrap();
}

#[test]
fn every_thread_of_the_user_counts_and_nobody_else_s() {
    let job = a_job();
    a_process(&job, 45_301, (45_301, 45_301), 3);
    a_process(&job, 45_302, (45_301, 45_301), 1);
    a_process(&job, 45_303, (45_302, 45_302), 2);

    assert_eq!(tasks_of_user_in(&job, 45_301), 4);
    assert_eq!(tasks_of_user_in(&job, 45_302), 2);
    assert_eq!(tasks_of_user_in(&job, 45_303), 0);
    assert_eq!(
        tasks_of_user(45_301),
        4,
        "the machine's census is the job's: nobody else runs as this user"
    );
}

/// The real uid is the one charged, as in Linux (`real_cred->user`).
#[test]
fn the_real_uid_is_the_one_charged() {
    let job = a_job();
    a_process(&job, 45_304, (45_304, 45_305), 2);

    assert_eq!(tasks_of_user_in(&job, 45_304), 2);
    assert_eq!(tasks_of_user_in(&job, 45_305), 0);
}

/// A zombie has left the job, and still counts until it is reaped
/// (`release_task`).
#[test]
fn a_zombie_counts_until_it_is_reaped() {
    let job = a_job();
    let parent = a_process(&job, 45_306, (0, 0), 1);
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    child.linux().set_resuid(45_306, 45_306, 45_306).unwrap();
    let other = Process::fork_from(&parent).unwrap();
    other.linux().set_resuid(45_307, 45_307, 45_307).unwrap();
    child.exit(0);
    other.exit(0);
    drop((child, other));

    assert_eq!(tasks_of_user_in(&job, 45_306), 1, "the zombie");
    assert_eq!(tasks_of_user_in(&job, 45_307), 1, "the other user's zombie");
    assert_eq!(
        block_on(wait_child(&parent, pid, true, true)).map(|(status, _)| status),
        Ok(wait_status_exited(0))
    );
    assert_eq!(tasks_of_user_in(&job, 45_306), 0, "reaped, it is gone");
    assert_eq!(tasks_of_user_in(&job, 45_307), 1, "and only that one");
}

#[test]
fn one_more_task_is_refused_once_the_user_has_as_many_as_the_limit() {
    let proc = a_process(&a_job(), 45_308, (45_308, 45_308), 2);

    limit(&proc, 3);
    assert_eq!(proc.linux().may_start_task(), Ok(()), "2 of 3");
    Thread::create_linux(&proc).unwrap();
    assert_eq!(
        proc.linux().may_start_task(),
        Err(LxError::EAGAIN),
        "3 of 3"
    );
    limit(&proc, 0);
    assert_eq!(proc.linux().may_start_task(), Err(LxError::EAGAIN));
}

#[test]
fn unlimited_lets_everything_through() {
    let proc = a_process(&a_job(), 45_309, (45_309, 45_309), 3);

    assert_eq!(proc.linux().may_start_task(), Ok(()));
}

/// `p->real_cred->user != INIT_USER && !capable(CAP_SYS_RESOURCE) &&
/// !capable(CAP_SYS_ADMIN)`: a real root, or an effective one, is let past.
#[test]
fn root_real_or_effective_is_not_held_to_it() {
    let job = a_job();
    for (pid, ids) in [
        (45_310, (0, 0)),
        (45_311, (0, 45_311)),
        (45_312, (45_312, 0)),
    ] {
        let proc = a_process(&job, pid, ids, 2);
        limit(&proc, 0);
        assert_eq!(proc.linux().may_start_task(), Ok(()), "{:?}", ids);
    }
}

/// The user's other processes count against this one's limit, and only the
/// user's.
#[test]
fn the_limit_is_held_against_the_whole_user() {
    let job = a_job();
    a_process(&job, 45_313, (45_313, 45_313), 2);
    a_process(&job, 45_314, (45_314, 45_314), 5);
    let proc = a_process(&job, 45_315, (45_313, 45_313), 1);

    limit(&proc, 4);
    assert_eq!(proc.linux().may_start_task(), Ok(()), "3 of 4");
    limit(&proc, 3);
    assert_eq!(
        proc.linux().may_start_task(),
        Err(LxError::EAGAIN),
        "3 of 3"
    );
}
