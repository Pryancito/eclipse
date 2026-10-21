//! The leader of a thread group that has called `exit(2)` while other
//! threads of its process still run.
//!
//! Linux keeps that task (`delay_group_leader`): it is a zombie until the
//! last thread of the group goes, and its pid still names it. `/proc/<pid>`
//! reports its state, `Z`, and counts and lists it among the tasks, and a
//! signal sent to it as a thread finds it and does nothing. Here the leader
//! left the thread list with its `exit`, so `/proc/<pid>` showed whichever
//! thread came next, asleep, one thread short, and `tgkill(pid, pid, 0)` was
//! `ESRCH`.
//!
//! The thread object is kept until the process dies, or until an `execve`
//! from another thread gives its pid to a new leader. It holds its process
//! strongly, so both of those let it go.

use super::*;

/// The leader kept; see the module docs.
#[derive(Default)]
pub(super) struct ExitedLeader(Mutex<Option<Arc<Thread>>>);

impl LinuxProcess {
    /// Said by the leader's own `exit(2)` while other threads remain.
    pub fn keep_exited_leader(&self, leader: Arc<Thread>) {
        *self.exited_leader.0.lock() = Some(leader);
    }

    /// The leader, if it has exited while its process runs on.
    pub fn exited_leader(&self) -> Option<Arc<Thread>> {
        self.exited_leader.0.lock().clone()
    }

    /// Let it go: the process has died, or an exec from another thread
    /// made a new leader (`de_thread` releases the old one).
    pub fn release_exited_leader(&self) -> Option<Arc<Thread>> {
        self.exited_leader.0.lock().take()
    }
}

/// The leader of `proc` that has exited while other threads run, if any.
pub fn exited_leader_of(proc: &Process) -> Option<Arc<Thread>> {
    proc.try_linux()?.exited_leader()
}

/// How many tasks `proc` has, as `/proc` counts them: its threads and a
/// leader that has exited before them. Never 0: a process that has not
/// started a thread yet is one task.
pub fn task_count(proc: &Process) -> usize {
    let leader = exited_leader_of(proc).is_some() as usize;
    (proc.thread_ids().len() + leader).max(1)
}

/// The ids of `proc`'s tasks, as `/proc/<pid>/task` lists them: a leader
/// that has exited first, where Linux's thread list keeps it, then the
/// threads.
pub fn task_ids(proc: &Process) -> Vec<KoID> {
    let mut ids: Vec<KoID> = exited_leader_of(proc).map(|t| t.id()).into_iter().collect();
    ids.extend(proc.thread_ids());
    ids
}

/// Task `tid` of `proc`: one of its threads, or its leader that has exited.
pub fn task_of(proc: &Process, tid: KoID) -> Option<Arc<Thread>> {
    if let Some(leader) = exited_leader_of(proc).filter(|t| t.id() == tid) {
        return Some(leader);
    }
    proc.get_child(tid).ok()?.downcast_arc::<Thread>().ok()
}

#[cfg(test)]
mod tests;
