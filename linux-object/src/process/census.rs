//! The process census: who is alive, and the lookups that go through it.
//!
//! Every syscall that takes a pid, a process group or a user and has to find
//! out who that names comes through here: `kill(-pgid)`, `getpgid`,
//! `getsid`, `setpriority`, `sched_setaffinity` on somebody else, `setsid`'s
//! own veto, and the `/proc` listing. Linux keeps a hash table per pid type
//! (`pid->tasks[PIDTYPE_PGID]`) so each of those walks the group and nothing
//! else; here the only index is the job tree, so the census is built first
//! and filtered afterwards.
//!
//! That makes the cost of the census the cost of all of them, which is why
//! it lives in one file with its own bench family
//! (`linux-object/benches/process_census.rs`) and
//! [`all_live_processes_in`] as the seam those rows and the tests measure a
//! KNOWN tree through -- `all_live_processes` can only ever be asked about
//! the one root job, whose contents are whatever the rest of the system
//! happens to have running.

use super::*;

/// The effective process group of every live process, for [`setsid_verdict`].
pub fn live_effective_pgids() -> Vec<KoID> {
    all_live_processes().iter().map(effective_pgid).collect()
}

/// One live process by pid, which is what a syscall that takes a pid wants.
///
/// The census is the wrong tool for this and was what both callers used: it
/// clones an `Arc` and reads a status for every process on the machine to
/// then throw all but one away. The job tree can be asked directly.
///
/// The `Exited` filter is kept deliberately, so this answers exactly what
/// the census walk answered: a process that has exited is gone as far as
/// `getpgid`/`getsid` are concerned.
fn find_live_process(pid: KoID) -> LxResult<Arc<Process>> {
    let proc = find_process(pid).ok_or(LxError::ESRCH)?;
    if matches!(proc.status(), Status::Exited(_)) {
        return Err(LxError::ESRCH);
    }
    Ok(proc)
}

/// `getpgid`: the effective process-group id of process `pid`.
pub fn get_process_pgid(pid: KoID) -> LxResult<KoID> {
    Ok(effective_pgid(&find_live_process(pid)?))
}

/// `getsid`: the effective session id of process `pid`.
pub fn get_process_sid(pid: KoID) -> LxResult<KoID> {
    Ok(effective_sid(&find_live_process(pid)?))
}

/// The live process thread `tid` belongs to.
///
/// `kill(2)` takes a pid, and on Linux a pid is a TID: `kill_pid_info()`
/// finds the task with `pid_task(pid, PIDTYPE_PID)` and signals its whole
/// thread group, so `kill(gettid(), sig)` from any thread reaches its own
/// process. Here only the leader's id is its process's id (`create_linux`
/// hands it the pid); every other thread's id is one no process has, so a
/// lookup that only knew pids answered `ESRCH` when a worker thread named
/// itself -- or when anybody probed a worker's tid with `kill(tid, 0)`.
///
/// It walks the census, so it belongs on the path where the pid named no
/// process, not in front of it.
pub fn process_of_thread(tid: KoID) -> Option<Arc<Process>> {
    process_of_thread_in(&ROOT_JOB, tid)
}

/// [`process_of_thread`] over one job's subtree, for the tests.
pub fn process_of_thread_in(job: &Arc<Job>, tid: KoID) -> Option<Arc<Process>> {
    all_live_processes_in(job)
        .into_iter()
        .find(|proc| proc.thread_ids().contains(&tid))
}

/// The live thread `tid` names, anywhere on the machine, with its process.
///
/// `tkill(2)` is `find_task_by_vpid(tid)`: a tid is looked up across the
/// whole system, not inside the caller's process, and only then judged by
/// `check_kill_permission()`. Like [`process_of_thread`] it walks the census,
/// so it is for the call that did not find the tid at home.
pub fn find_thread(tid: KoID) -> Option<(Arc<Process>, Arc<Thread>)> {
    find_thread_in(&ROOT_JOB, tid)
}

/// [`find_thread`] over one job's subtree, for the tests.
pub fn find_thread_in(job: &Arc<Job>, tid: KoID) -> Option<(Arc<Process>, Arc<Thread>)> {
    let proc = process_of_thread_in(job, tid)?;
    let thread = proc.get_child(tid).ok()?.downcast_arc::<Thread>().ok()?;
    Some((proc, thread))
}

/// The live members of `job` and of every job under it, appended to `out`.
///
/// It asks each job for its members and its children DIRECTLY
/// ([`Job::processes`], [`Job::children`]). Asking for the ids and then
/// looking each one up -- which is what this did -- is the same answer for a
/// quadratic price, because the lookup is a linear scan of the very list the
/// ids came from. See the module doc for why that price is everybody's.
fn collect_live_processes(job: &Arc<Job>, out: &mut Vec<Arc<Process>>) {
    out.extend(
        job.processes()
            .into_iter()
            .filter(|proc| !matches!(proc.status(), Status::Exited(_))),
    );
    for child_job in job.children() {
        collect_live_processes(&child_job, out);
    }
}

/// All non-exited processes in the root job tree.
pub fn all_live_processes() -> Vec<Arc<Process>> {
    all_live_processes_in(&ROOT_JOB)
}

/// The same census of any one job's subtree, which is what the benches and
/// the tests can hold still: the root job holds whatever else is running.
pub fn all_live_processes_in(job: &Arc<Job>) -> Vec<Arc<Process>> {
    let mut processes = Vec::new();
    collect_live_processes(job, &mut processes);
    processes
}

/// The process `pid` names, exited or not.
///
/// `find_task_by_vpid()`: the lookup a syscall that takes a pid does before
/// it can ask anything else about it.
pub fn find_process(pid: KoID) -> Option<Arc<Process>> {
    ROOT_JOB.find_process(pid)
}

/// How many tasks user `uid` has on the machine: what `RLIMIT_NPROC` is held
/// against.
///
/// Linux counts TASKS, not processes -- `copy_creds` charges every new one to
/// its real uid, a thread like any other -- and lets one go only in
/// `release_task`, so a zombie still counts until somebody reaps it. A zombie
/// has left the job here, so it is looked for where its parent keeps it.
pub fn tasks_of_user(uid: u32) -> usize {
    tasks_of_user_in(&ROOT_JOB, uid)
}

/// [`tasks_of_user`] over one job's subtree, for the tests.
pub fn tasks_of_user_in(job: &Arc<Job>, uid: u32) -> usize {
    all_live_processes_in(job)
        .iter()
        .filter_map(|proc| {
            let linux = proc.try_linux()?;
            let own = if linux.uid() == uid {
                proc.thread_count()
            } else {
                0
            };
            Some(own + linux.zombies_of_user(uid))
        })
        .sum()
}

impl LinuxProcess {
    /// `copy_process`'s `RLIMIT_NPROC` test, for one more task started by
    /// this process: `EAGAIN` once its real user has as many as its soft
    /// limit allows. Root, and a caller with `CAP_SYS_RESOURCE` or
    /// `CAP_SYS_ADMIN`, are not held to it, as in Linux.
    ///
    /// Unlimited, the usual case, costs one read of the limit; a limit set
    /// costs a census per `fork`, `clone` and `pthread_create`. The census is
    /// taken before the task exists, so two of them racing can both get the
    /// last one (Linux charges first and tests after, atomically).
    pub fn may_start_task(&self) -> LxResult {
        let limit = self
            .rlimit(RLIMIT_NPROC, None, false)
            .map_or(RLIM_INFINITY, |limit| limit.cur);
        if limit == RLIM_INFINITY {
            return Ok(());
        }
        let ruid = self.uid();
        if ruid == ROOT_UID || self.capable(CAP_SYS_RESOURCE) || self.capable(CAP_SYS_ADMIN) {
            return Ok(());
        }
        if tasks_of_user(ruid) as u64 >= limit {
            info!(
                "RLIMIT_NPROC: uid {} is at its limit of {} tasks",
                ruid, limit
            );
            return Err(LxError::EAGAIN);
        }
        Ok(())
    }

    /// The children of this process that are zombies of user `uid`.
    ///
    /// The zombies are taken out under the lock and asked their uid after it
    /// is let go: their lock is not to be taken under their parent's.
    fn zombies_of_user(&self, uid: u32) -> usize {
        let zombies: Vec<Arc<Process>> = self
            .inner
            .lock()
            .reaped_children
            .values()
            .filter_map(|zombie| zombie._wears_pid.clone())
            .collect();
        zombies
            .iter()
            .filter(|zombie| zombie.try_linux().is_some_and(|linux| linux.uid() == uid))
            .count()
    }
}

#[cfg(test)]
mod nproc_tests;
