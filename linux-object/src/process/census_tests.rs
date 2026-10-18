//! Who the census says is alive.
//!
//! Four of its properties are load-bearing and none of them had a test, for
//! the reason the module doc gives: the only census there was
//! (`all_live_processes`) answers about the root job, and a test cannot hold
//! the root job still -- everything else the binary is running is in it. So
//! the walk itself was never asserted, only its callers' conclusions, which
//! are all of the form "this pid is in the list" and pass just as well if
//! the walk returns everything in sight.
//!
//! The four: every live member is listed, an exited one is not, the walk goes
//! DOWN the job tree (a process in a job nobody has a reference to by name is
//! still a process on the machine, and a signal to its group has to reach
//! it), and the depth is not one level.
//!
//! Fixed pids live in 94_1xx here; they are a global table, so every file
//! that pins one has its own range.

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

/// A live Linux process with a thread, in `job`.
fn a_process_in(job: &Arc<Job>, pid: KoID) -> Arc<Process> {
    let proc =
        Process::create_with_fixed_id_ext(job, pid, "census", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

/// A job of its own under the root, so the census of it is exactly what this
/// test put in it.
fn a_job() -> Arc<Job> {
    ROOT_JOB.create_child().unwrap()
}

fn census_ids(job: &Arc<Job>) -> Vec<KoID> {
    let mut ids: Vec<KoID> = all_live_processes_in(job).iter().map(|p| p.id()).collect();
    ids.sort_unstable();
    ids
}

#[test]
fn an_empty_job_has_nobody_in_it() {
    assert!(census_ids(&a_job()).is_empty());
}

#[test]
fn every_live_member_is_listed() {
    let job = a_job();
    for pid in 94_100..94_103 {
        a_process_in(&job, pid);
    }
    assert_eq!(census_ids(&job), vec![94_100, 94_101, 94_102]);
}

#[test]
fn an_exited_member_is_not_listed() {
    let job = a_job();
    let live = a_process_in(&job, 94_110);
    let dead = a_process_in(&job, 94_111);
    dead.exit(0);
    assert_eq!(census_ids(&job), vec![live.id()]);
}

/// The recursion, which is what the walk is FOR: a process in a child job is
/// a process on the machine, and a group signal or a `ps` has to see it.
#[test]
fn a_member_of_a_child_job_is_listed() {
    let job = a_job();
    let child_job = job.create_child().unwrap();
    a_process_in(&job, 94_120);
    a_process_in(&child_job, 94_121);
    assert_eq!(census_ids(&job), vec![94_120, 94_121]);
}

/// And the recursion does not stop at one level down.
#[test]
fn a_member_three_jobs_down_is_listed() {
    let job = a_job();
    let mid = job.create_child().unwrap();
    let deep = mid.create_child().unwrap();
    a_process_in(&deep, 94_130);
    assert_eq!(census_ids(&job), vec![94_130]);
}

/// A job holds its children by `Weak`, so the walk has to cope with a child
/// that is gone -- and with one that is NOT gone although nobody named it.
///
/// A job holds its members by `Arc` and a process holds its job by `Arc`, so
/// a job with a live member keeps itself alive through that member: dropping
/// the last named reference to it changes nothing, and its processes stay in
/// the census, which is what a signal to their group needs. A job with no
/// members has nothing holding it and drops, and the walk simply does not
/// visit it.
#[test]
fn a_child_job_is_kept_alive_by_its_members_and_dropped_without_them() {
    let job = a_job();
    a_process_in(&job, 94_140);
    {
        let peopled = job.create_child().unwrap();
        a_process_in(&peopled, 94_141);
        let empty = job.create_child().unwrap();
        assert_eq!(job.children().len(), 2, "both are there while named");
        let _ = (peopled, empty);
    }
    assert_eq!(
        census_ids(&job),
        vec![94_140, 94_141],
        "the member keeps its job in the tree"
    );
    assert_eq!(job.children().len(), 1, "the empty one is gone");
}

/// The census is only ever read as a set of processes, so the two ways of
/// asking a job for its members have to agree -- the fast one is what the
/// walk uses now and the slow one is what it used to use.
#[test]
fn asking_a_job_directly_is_the_same_set_as_looking_each_id_up() {
    let job = a_job();
    for pid in 94_150..94_155 {
        a_process_in(&job, pid);
    }
    let mut direct: Vec<KoID> = job.processes().iter().map(|p| p.id()).collect();
    let mut by_id: Vec<KoID> = job
        .process_ids()
        .into_iter()
        .filter_map(|id| job.find_process(id))
        .map(|p| p.id())
        .collect();
    direct.sort_unstable();
    by_id.sort_unstable();
    assert_eq!(direct, by_id);

    let mut jobs_direct: Vec<KoID> = {
        let _ = job.create_child().unwrap();
        job.children().iter().map(|j| j.id()).collect()
    };
    let mut jobs_by_id = job.children_ids();
    jobs_direct.sort_unstable();
    jobs_by_id.sort_unstable();
    assert_eq!(jobs_direct, jobs_by_id);
}

/// `getpgid`/`getsid` ask the job tree for the one process now instead of
/// building the census, and the `Exited` filter they had has to survive the
/// change: a process that has exited is gone as far as they are concerned,
/// even while the lookup still answers with it.
mod the_one_process_a_pid_names {
    use super::*;

    #[test]
    fn a_pid_that_names_nobody_is_esrch() {
        assert_eq!(get_process_pgid(94_199), Err(LxError::ESRCH));
        assert_eq!(get_process_sid(94_199), Err(LxError::ESRCH));
    }

    #[test]
    fn a_live_process_answers_with_its_group_and_session() {
        let proc = a_process_in(&ROOT_JOB, 94_160);
        proc.linux().become_session_leader(94_160);
        proc.linux().set_pgid_raw(94_161);
        assert_eq!(get_process_pgid(proc.id()), Ok(94_161));
        assert_eq!(get_process_sid(proc.id()), Ok(94_160));
    }

    /// Asked from INSIDE the death, which is the only moment the filter can
    /// be seen: the process is still in its job, so the lookup succeeds and
    /// the status is the only thing saying it is gone. After the exit returns
    /// the lookup fails on its own and the filter is doing nothing.
    #[test]
    fn an_exited_process_is_esrch_while_it_is_still_in_its_job() {
        let proc = a_process_in(&ROOT_JOB, 94_170);
        let pid = proc.id();
        let seen: Arc<spin::Mutex<Option<(LxResult<KoID>, LxResult<KoID>)>>> =
            Arc::new(spin::Mutex::new(None));
        let writer = seen.clone();
        proc.add_signal_callback(Box::new(move |signal| {
            if signal.contains(Signal::PROCESS_TERMINATED) {
                *writer.lock() = Some((get_process_pgid(pid), get_process_sid(pid)));
            }
            false
        }));
        proc.exit(0);
        assert_eq!(
            *seen.lock(),
            Some((Err(LxError::ESRCH), Err(LxError::ESRCH))),
            "still in the job, already gone"
        );
    }
}

mod the_process_a_thread_id_names {
    use super::*;

    #[test]
    fn a_secondary_thread_names_its_process() {
        let job = a_job();
        let proc = a_process_in(&job, 94_180);
        let second = Thread::create_linux(&proc).unwrap();
        let found = process_of_thread_in(&job, second.id()).expect("no process for the thread");
        assert_eq!(found.id(), proc.id());
    }

    /// The leader already shares the pid, so `find_process` answers for it
    /// and this lookup never gets asked; what it must not do is claim the
    /// pid belongs to somebody else.
    #[test]
    fn the_leader_is_found_by_its_pid_and_names_its_own_process() {
        let job = a_job();
        let proc = a_process_in(&job, 94_181);
        assert_eq!(proc.thread_ids()[0], proc.id());
        assert_eq!(
            process_of_thread_in(&job, proc.id()).map(|p| p.id()),
            Some(proc.id())
        );
    }

    #[test]
    fn a_thread_of_another_process_names_that_process() {
        let job = a_job();
        let mine = a_process_in(&job, 94_182);
        let theirs = a_process_in(&job, 94_185);
        let worker = Thread::create_linux(&theirs).unwrap();
        assert_eq!(
            process_of_thread_in(&job, worker.id()).map(|p| p.id()),
            Some(theirs.id())
        );
        assert_ne!(theirs.id(), mine.id());
    }

    #[test]
    fn an_id_nobody_has_names_no_process() {
        let job = a_job();
        a_process_in(&job, 94_183);
        assert!(process_of_thread_in(&job, 94_189).is_none());
    }

    #[test]
    fn a_thread_of_an_exited_process_names_nothing() {
        let job = a_job();
        let proc = a_process_in(&job, 94_184);
        let first = proc.thread_ids()[0];
        proc.exit(0);
        assert!(process_of_thread_in(&job, first).is_none());
    }
}

mod the_thread_a_tid_names {
    use super::*;

    /// What `tkill` at another process's thread needs: the thread itself,
    /// and the process its permission is judged against.
    #[test]
    fn a_thread_of_another_process_is_found_with_its_process() {
        let job = a_job();
        let theirs = a_process_in(&job, 94_190);
        let worker = Thread::create_linux(&theirs).unwrap();
        let (proc, thread) = find_thread_in(&job, worker.id()).expect("thread not found");
        assert_eq!(proc.id(), theirs.id());
        assert_eq!(thread.id(), worker.id());
    }

    #[test]
    fn an_id_no_thread_has_finds_nothing() {
        let job = a_job();
        a_process_in(&job, 94_191);
        assert!(find_thread_in(&job, 94_199).is_none());
    }

    #[test]
    fn a_thread_of_an_exited_process_is_not_found() {
        let job = a_job();
        let proc = a_process_in(&job, 94_192);
        let worker = Thread::create_linux(&proc).unwrap();
        let tid = worker.id();
        proc.exit(0);
        assert!(find_thread_in(&job, tid).is_none());
    }
}
