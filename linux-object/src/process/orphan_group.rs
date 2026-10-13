//! POSIX's orphaned process group, and the `SIGHUP` + `SIGCONT` it is owed.
//!
//! A process group is *orphaned* when no member of it has a parent that is in
//! a different group of the same session. That is the condition under which
//! nobody is left who could resume the group: the shell that would have run
//! `fg` is gone, and every surviving relative is inside the group itself.
//!
//! A stopped group in that state is wedged for good. `sleep 1000`, Ctrl-Z,
//! and then the shell dies: the job stays in its job-control stop with no
//! process in the system that will ever continue it, holding its memory, its
//! descriptors and its slot in the process table for as long as the machine
//! is up. POSIX requires the kernel to break that deadlock itself, and Linux
//! does it in `kill_orphaned_pgrp` -- `SIGHUP` so the job hears that its
//! terminal's owner is gone, then `SIGCONT` so it is running when it hears
//! it. Both, in that order: a `SIGHUP` to a stopped process is only queued.
//!
//! None of this existed here.

use super::*;

/// One member of a process group, in the facts the POSIX definition reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupMember {
    /// This member's own group.
    pub pgid: KoID,
    /// This member's session.
    pub sid: KoID,
    /// The member's parent's `(pgid, sid)`, or `None` when the parent cannot
    /// protect the group: it is gone, or it is INIT.
    ///
    /// INIT is `None` and not its real ids because Linux writes the clause
    /// out (`is_global_init(p->real_parent)`): a parent that is PID 1 is no
    /// use to a job-control shell, so it does not count as the relative that
    /// could resume the group. Here the session comparison below would reach
    /// the same answer on its own, since INIT has a session of its own -- but
    /// only by coincidence, and a coincidence is not what should decide
    /// whether a stopped job is left wedged.
    pub parent: Option<(KoID, KoID)>,
    /// Whether this member is in a job-control stop right now.
    pub stopped: bool,
}

/// Linux's `will_become_orphaned_pgrp`: is there any member whose parent is
/// outside the group but inside the same session?
///
/// One such parent is enough to make the group NOT orphaned, so this is a
/// search for a single protector and not a property of every member. Both
/// halves of that parent test are load-bearing and fail in opposite
/// directions: drop the "different group" half and a group whose members are
/// each other's parents protects itself forever, so no group is ever
/// orphaned and no stopped job is ever freed; drop the "same session" half
/// and a job whose shell has already left the session counts as protected by
/// it, which is the very case this exists for.
pub fn group_is_orphaned(members: &[GroupMember]) -> bool {
    !members.iter().any(|m| match m.parent {
        Some((parent_pgid, parent_sid)) => parent_pgid != m.pgid && parent_sid == m.sid,
        None => false,
    })
}

/// Linux's `has_stopped_jobs`. An orphaned group with nothing stopped in it is
/// ordinary -- a daemon's group is orphaned for its whole life -- so this is
/// what separates a group that is merely unreachable from one that is stuck.
pub fn group_has_stopped_member(members: &[GroupMember]) -> bool {
    members.iter().any(|m| m.stopped)
}

/// Whether this group is owed the `SIGHUP` + `SIGCONT` pair: orphaned, and
/// with something stopped in it.
///
/// An empty group is owed nothing, and says so through `group_is_orphaned`
/// answering `true` for it: a group with no members is vacuously orphaned,
/// and has nothing stopped.
pub fn orphaned_group_owes_a_wake(members: &[GroupMember]) -> bool {
    group_is_orphaned(members) && group_has_stopped_member(members)
}

/// The groups this death could leave orphaned: the dying process's own, and
/// the group of every child it still holds.
///
/// Read BEFORE the reparenting, because that is while the children are still
/// in hand, and it does not care whether the reparenting then finds anybody to
/// adopt them: a child nobody can adopt is more orphaned, not less.
pub fn groups_this_death_could_orphan(dying: &Arc<Process>) -> Vec<KoID> {
    let mut groups = alloc::vec![effective_pgid(dying)];
    if let Some(lp) = dying.try_linux() {
        let children: Vec<Arc<Process>> = lp.inner.lock().children.values().cloned().collect();
        groups.extend(children.iter().map(effective_pgid));
    }
    groups
}

/// Read the group `pgid` out of a census of live processes.
///
/// The census has already dropped every process that has exited, which is
/// what Linux's two `continue` clauses do by hand (`ignored_task`, and a
/// zombie whose thread group is empty): the process whose death started this
/// is `Status::Exited` by the time the callback runs, so it is not in here to
/// be mistaken for a protector of the group it is leaving.
fn members_of(live: &[Arc<Process>], pgid: KoID) -> Vec<GroupMember> {
    live.iter()
        .filter(|proc| effective_pgid(proc) == pgid)
        .map(|proc| GroupMember {
            pgid,
            sid: effective_sid(proc),
            parent: proc
                .try_linux()
                .and_then(|lp| lp.parent())
                // A parent that has exited protects nothing, and is the whole
                // case here: the shell whose death started this is still a
                // live `Arc` -- somebody is holding it, this very callback
                // among them -- and `parent()` hands it back exactly as
                // before. Reading it as a protector is how a group that has
                // just lost its shell looks like a group that still has one.
                .filter(|parent| !matches!(parent.status(), Status::Exited(_)))
                .filter(|parent| parent.id() != INIT_PID)
                .map(|parent| (effective_pgid(&parent), effective_sid(&parent))),
            stopped: proc
                .try_linux()
                .map(|lp| lp.is_job_stopped())
                .unwrap_or(false),
        })
        .collect()
}

/// Linux's `kill_orphaned_pgrp`, over every group this death could have
/// orphaned: the dying process's own, and the group of each child it just
/// handed to somebody else.
///
/// Called AFTER the reparenting, because the question is about the parents
/// the members have now -- a child handed to INIT has lost its protector,
/// and that is the whole point.
pub fn wake_orphaned_stopped_groups(candidates: &[KoID]) {
    let mut seen: Vec<KoID> = Vec::new();
    let live = all_live_processes();
    for &pgid in candidates {
        if pgid == 0 || seen.contains(&pgid) {
            continue;
        }
        seen.push(pgid);
        let members = members_of(&live, pgid);
        if !orphaned_group_owes_a_wake(&members) {
            continue;
        }
        warn!(
            "orphaned process group {} has {} stopped member(s) and nobody left to \
             continue it: SIGHUP + SIGCONT",
            pgid,
            members.iter().filter(|m| m.stopped).count()
        );
        // In this order, and both of them. `SIGHUP` is what tells the job its
        // terminal's owner is gone; `SIGCONT` is what gets it out of the stop
        // so it can act on it. Sent the other way round, the job would be
        // running when the hangup arrived and a `SIGHUP` handler would see a
        // continued job rather than a disconnected one.
        let _ = send_signal_to_pgrp(pgid as usize, LinuxSignal::SIGHUP);
        let _ = send_signal_to_pgrp(pgid as usize, LinuxSignal::SIGCONT);
    }
}
