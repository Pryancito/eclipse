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

/// `getpgid`: the effective process-group id of process `pid`, a zombie's
/// included -- it keeps the group it died in until it is reaped.
pub fn get_process_pgid(pid: KoID) -> LxResult<KoID> {
    Ok(effective_pgid(&find_task(pid).ok_or(LxError::ESRCH)?))
}

/// `getsid`: the effective session id of process `pid`, as [`get_process_pgid`].
pub fn get_process_sid(pid: KoID) -> LxResult<KoID> {
    Ok(effective_sid(&find_task(pid).ok_or(LxError::ESRCH)?))
}

/// The process `pid` names as Linux's pid table answers: one that is alive,
/// or a zombie nobody has reaped yet (`find_task_by_vpid()`).
///
/// A zombie answered `ESRCH` to everybody but its parent's `kill`: it leaves
/// the job when its last thread is gone, so `getpgid`, `getsid`,
/// `pidfd_open` and `/proc/<pid>` could not find it, and `ps` never showed a
/// `<defunct>`. Such a zombie is where its reaper keeps it. And the other
/// way round, a process a `wait*` has already collected can still be in the
/// job finishing its teardown; its pid names nothing, as after Linux's
/// `release_task()`, and `kill(pid, 0)` right after the `waitpid` said it
/// was there.
pub fn find_task(pid: KoID) -> Option<Arc<Process>> {
    find_task_in(&ROOT_JOB, pid)
}

/// [`find_task`] over one job's subtree, for the tests.
pub fn find_task_in(job: &Arc<Job>, pid: KoID) -> Option<Arc<Process>> {
    let found = job.find_process(pid).or_else(|| {
        all_live_processes_in(job)
            .iter()
            .find_map(|parent| parent.try_linux()?.dead_child(pid))
    })?;
    let reaped = found.try_linux().is_some_and(|linux| linux.is_reaped());
    (!reaped).then_some(found)
}

/// The zombies nobody has reaped yet, in or out of the job: the processes
/// `/proc` lists besides the live ones.
pub fn unreaped_zombies() -> Vec<Arc<Process>> {
    unreaped_zombies_in(&ROOT_JOB)
}

/// [`unreaped_zombies`] over one job's subtree, for the tests.
pub fn unreaped_zombies_in(job: &Arc<Job>) -> Vec<Arc<Process>> {
    all_live_processes_in(job)
        .iter()
        .filter_map(|parent| parent.try_linux().map(|linux| linux.dead_children()))
        .flatten()
        .filter(|zombie| zombie.try_linux().is_none_or(|linux| !linux.is_reaped()))
        .collect()
}

/// Every process a pid still names: the live ones and the zombies nobody
/// has reaped. A walk over a process group or over a user's tasks visits a
/// zombie on Linux (`do_each_pid_thread`, `for_each_process_thread`) until
/// `release_task()`, and `getpriority(PRIO_PGRP)` counts it.
pub fn live_and_zombie_processes() -> Vec<Arc<Process>> {
    let mut procs = all_live_processes();
    procs.extend(unreaped_zombies());
    procs
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

    /// The dead child `pid` names, as long as this process keeps it: on file
    /// as a zombie, or exited and not on file yet. Only asked about a pid the
    /// job no longer has, so a child found in the live table is a dead one.
    fn dead_child(&self, pid: KoID) -> Option<Arc<Process>> {
        let inner = self.inner.lock();
        match inner.reaped_children.get(&pid) {
            Some(zombie) => zombie._wears_pid.clone(),
            None => inner.children.get(&pid).cloned(),
        }
    }

    /// Every dead child this process keeps, as [`Self::dead_child`]. The
    /// status of a live-table one is read after the lock is let go.
    fn dead_children(&self) -> Vec<Arc<Process>> {
        let (mut zombies, children): (Vec<_>, Vec<_>) = {
            let inner = self.inner.lock();
            (
                inner
                    .reaped_children
                    .values()
                    .filter_map(|zombie| zombie._wears_pid.clone())
                    .collect(),
                inner.children.values().cloned().collect(),
            )
        };
        zombies.extend(
            children
                .into_iter()
                .filter(|child| matches!(child.status(), Status::Exited(_))),
        );
        zombies
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

#[cfg(test)]
mod zombie_lookup_tests;
