//! The task a `sched_*`, `setpriority`/`getpriority`, `ioprio_*` or
//! `get_robust_list` call names: a thread, or the leader of a process that
//! has gone from its thread list.
//!
//! Linux finds a pid's task with `find_task_by_vpid()`, and a thread group's
//! leader is a task until `release_task()`: a zombie answers these calls
//! with what it died with, and the setters change it. Here they asked a
//! thread, and a zombie that had left its job had none: all of them were
//! `ESRCH`, where Linux 6.18 answers (`zsched-survey`, 16 checks, glibc and
//! musl). The leader's attributes are kept on its process when it leaves
//! ([`Process::leader_sched`]); this is the one type the calls ask.

use super::*;
use zircon_object::task::{SchedSnapshot, ThreadState};
use zircon_object::ZxResult;

/// Whether `thread` is on its way out, `PF_EXITING`: killed, exited, or
/// gone. `do_exit` lets such a task's `io_context` go.
fn exiting(thread: &Thread) -> bool {
    matches!(thread.state(), ThreadState::Dying | ThreadState::Dead)
}

/// Whether `thread` is one `release_task` would have taken by now: one
/// that is not the leader of a process whose exit is on file. Linux makes a
/// thread group a zombie its parent can wait for only once every other
/// thread of it has been released (`delay_group_leader`); `Process::exit`
/// publishes the exit at once, while the threads it killed are still on
/// their way out, and a thread parked in a `read` took its time. A tid
/// already finds none of them: `find_task_by_tid` looks among the
/// processes that have not exited.
fn released(thread: &Thread) -> bool {
    let proc = thread.proc();
    thread.id() != proc.id() && proc.exit_code().is_some()
}

/// A task as `find_task_by_vpid()` finds it.
pub(crate) enum SchedTask {
    /// A thread: running, or a leader kept after its own `exit(2)` while
    /// the rest of its group runs on (`exited_leader`).
    Thread(Arc<Thread>),
    /// A leader that has left its thread list, by what it left on file. Only
    /// made for a process with something on file.
    Leader(Arc<Process>),
}

impl SchedTask {
    /// The task `pid` names in `proc`, a process the pid table found: the
    /// thread wearing that id, the leader kept after its exit, or what a
    /// gone leader left on file. A process with none of those never had a
    /// leader, and is no task.
    ///
    /// It used to answer with the process's first thread, from before a
    /// leader's tid was its pid: a pid whose leader had exited before the
    /// rest of its group named whichever thread was left.
    pub(crate) fn of_process(proc: Arc<Process>, pid: KoID) -> Option<Self> {
        if let Some(thread) = linux_object::process::task_of(&proc, pid) {
            return Some(SchedTask::Thread(thread));
        }
        proc.leader_sched()
            .is_some()
            .then_some(SchedTask::Leader(proc))
    }

    /// The tasks of `proc` a walk over a group or a user visits: its
    /// threads, and its leader when that is no longer one of them. A zombie
    /// is its leader alone ([`released`]).
    pub(crate) fn all_of(proc: &Arc<Process>) -> Vec<Self> {
        let mut tasks: Vec<Self> = proc
            .threads()
            .into_iter()
            .filter(|t| !released(t))
            .map(SchedTask::Thread)
            .collect();
        if let Some(leader) = linux_object::process::exited_leader_of(proc) {
            tasks.push(SchedTask::Thread(leader));
        } else if !proc.thread_ids().contains(&proc.id()) && proc.leader_sched().is_some() {
            tasks.push(SchedTask::Leader(proc.clone()));
        }
        tasks
    }

    /// Its process.
    pub(crate) fn proc(&self) -> &Arc<Process> {
        match self {
            SchedTask::Thread(thread) => thread.proc(),
            SchedTask::Leader(proc) => proc,
        }
    }

    /// Its tid: a gone leader's is its pid.
    pub(crate) fn id(&self) -> KoID {
        match self {
            SchedTask::Thread(thread) => thread.id(),
            SchedTask::Leader(proc) => proc.id(),
        }
    }

    fn sched(&self) -> SchedSnapshot {
        match self {
            SchedTask::Thread(thread) => SchedSnapshot::of(thread),
            SchedTask::Leader(proc) => proc.leader_sched().unwrap_or_default(),
        }
    }

    pub(crate) fn sched_policy(&self) -> u8 {
        self.sched().policy
    }

    pub(crate) fn sched_nice(&self) -> i8 {
        self.sched().nice
    }

    pub(crate) fn sched_rt_priority(&self) -> u8 {
        self.sched().rt_priority
    }

    pub(crate) fn sched_reset_on_fork(&self) -> bool {
        self.sched().reset_on_fork
    }

    pub(crate) fn affinity(&self) -> u64 {
        self.sched().affinity
    }

    /// [`Thread::set_sched`], or the same on file for a gone leader.
    pub(crate) fn set_sched(&self, policy: u8, nice: i8, rt_priority: u8) {
        match self {
            SchedTask::Thread(thread) => thread.set_sched(policy, nice, rt_priority),
            SchedTask::Leader(proc) => {
                proc.update_leader_sched(|s| {
                    s.policy = policy;
                    s.nice = nice;
                    s.rt_priority = rt_priority;
                });
            }
        }
    }

    pub(crate) fn set_sched_reset_on_fork(&self, on: bool) {
        match self {
            SchedTask::Thread(thread) => thread.set_sched_reset_on_fork(on),
            SchedTask::Leader(proc) => {
                proc.update_leader_sched(|s| s.reset_on_fork = on);
            }
        }
    }

    /// [`Thread::set_affinity`], or the same on file for a gone leader,
    /// which has no CPU to be moved off. `sched_setaffinity` has refused an
    /// empty mask before it asks.
    pub(crate) fn set_affinity(&self, mask: u64) -> ZxResult {
        match self {
            SchedTask::Thread(thread) => thread.set_affinity(mask),
            SchedTask::Leader(proc) => {
                proc.update_leader_sched(|s| s.affinity = mask);
                Ok(())
            }
        }
    }

    /// The I/O priority word as set, `IOPRIO_CLASS_NONE` when never set.
    /// A task on its way out has none, whether a gone leader, a leader that
    /// exited before the rest of its group, or a thread a kill has reached:
    /// `do_exit` let its `io_context` go, and `ioprio_get` reads that as
    /// never set. A leader that exited first read the word it had set, and
    /// so did a zombie until its leader was off the thread list.
    pub(crate) fn ioprio(&self) -> u16 {
        match self {
            SchedTask::Thread(thread) if !exiting(thread) => thread.lock_linux().ioprio,
            _ => 0,
        }
    }

    /// Record an I/O priority word. On a gone leader it does nothing, and
    /// says nothing: `set_task_ioprio` finds the task `PF_EXITING` and
    /// returns 0. A thread on its way out keeps the word, which
    /// [`ioprio`](Self::ioprio) no longer reads.
    pub(crate) fn set_ioprio(&self, ioprio: u16) {
        if let SchedTask::Thread(thread) = self {
            thread.lock_linux().ioprio = ioprio;
        }
    }

    /// `get_robust_list(2)`. A gone leader's list was walked and cleared
    /// at its exit (`futex_cleanup`), so its head is null.
    pub(crate) fn get_robust_list(
        &self,
        mut head_ptr: UserOutPtr<UserOutPtr<RobustList>>,
        mut len_ptr: UserOutPtr<usize>,
    ) -> SysResult {
        match self {
            SchedTask::Thread(thread) => thread.get_robust_list(head_ptr, len_ptr),
            SchedTask::Leader(_) => {
                head_ptr.write(UserOutPtr::from(0usize))?;
                len_ptr.write(size_of::<RobustList>())?;
                Ok(0)
            }
        }
    }
}

#[cfg(test)]
mod tests;
