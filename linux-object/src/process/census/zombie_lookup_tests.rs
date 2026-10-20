//! A pid names its process from the fork to the reap, zombie included, and
//! nothing after: [`find_task`].
//!
//! Fixed pids live in 45_33x here; they are a global table, so every file
//! that pins one has its own range.

extern crate std;

use super::*;
use crate::signal::{SignalAction, SignalActionFlags, Sigset};
use crate::thread::ThreadExt;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;
use std::sync::Mutex as StdMutex;

/// A live process, with a thread, in a job of its own.
fn a_parent(pid: KoID) -> (Arc<Job>, Arc<Process>) {
    let job = ROOT_JOB.create_child().unwrap();
    let parent =
        Process::create_with_fixed_id_ext(&job, pid, "zombie", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    Thread::create_linux(&parent).unwrap();
    (job, parent)
}

fn found(job: &Arc<Job>, pid: KoID) -> Option<KoID> {
    find_task_in(job, pid).map(|proc| proc.id())
}

/// Who [`find_task`] answers for `pid`, for an assertion that expected no
/// answer at all.
///
/// `get_process_pgid` walks the WHOLE root job, which every test in the crate
/// shares, and it reports a process GROUP and not a process -- so a failure
/// reads `left: Ok(45102)` and says nothing about which process that was.
/// 45102 is a pid another file pins, which is a clue and not an answer.
fn whoever_answers(pid: KoID) -> alloc::string::String {
    match find_task(pid) {
        None => alloc::string::String::from("nobody, walking the whole root job"),
        Some(proc) => alloc::format!(
            "pid {} (pgid_raw {}, parent {:?})",
            proc.id(),
            proc.try_linux().map(|lp| lp.pgid_raw()).unwrap_or(0),
            proc.try_linux()
                .and_then(|lp| lp.parent())
                .map(|parent| parent.id()),
        ),
    }
}

fn reap(parent: &Arc<Process>, pid: KoID) {
    assert_eq!(
        block_on(wait_child(parent, pid, true, true)).map(|(status, _)| status),
        Ok(wait_status_exited(0)),
        "nothing to reap"
    );
}

/// The usual zombie: its last thread is gone, so it has left the job, and
/// its parent keeps it. It answers with the group it died in.
#[test]
fn a_zombie_out_of_its_job_is_found_until_it_is_reaped() {
    let (job, parent) = a_parent(45_331);
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    child.linux().set_pgid_raw(45_339);
    child.exit(0);
    drop(child);
    assert!(
        job.find_process(pid).is_none(),
        "the case this test is about"
    );

    assert_eq!(found(&job, pid), Some(pid));
    assert_eq!(get_process_pgid(pid), Ok(45_339));
    assert_eq!(get_process_sid(pid), Ok(effective_sid(&parent)));
    let zombies: Vec<KoID> = unreaped_zombies_in(&job).iter().map(|z| z.id()).collect();
    assert_eq!(zombies, [pid]);

    reap(&parent, pid);
    assert_eq!(found(&job, pid), None);
    assert_eq!(
        get_process_pgid(pid),
        Err(LxError::ESRCH),
        "a reaped pid still names a process: {}",
        whoever_answers(pid)
    );
    assert!(unreaped_zombies_in(&job).is_empty());
}

/// A `wait*` does not wait for the last thread to finish dying, so the
/// object can still be in its job after it was reaped: its pid names
/// nothing all the same.
#[test]
fn a_child_reaped_while_still_in_its_job_is_gone_at_once() {
    let (job, parent) = a_parent(45_332);
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    // A thread that never ran keeps the dead process in its job.
    Thread::create_linux(&child).unwrap();
    child.exit(0);
    assert!(
        job.find_process(pid).is_some(),
        "the case this test is about"
    );
    assert_eq!(found(&job, pid), Some(pid), "a zombie in the job");
    assert!(!child.linux().is_reaped());

    reap(&parent, pid);
    assert!(
        job.find_process(pid).is_some(),
        "still in the job, which is the point"
    );
    assert!(child.linux().is_reaped());
    assert_eq!(found(&job, pid), None);
    assert_eq!(get_process_pgid(pid), Err(LxError::ESRCH));
}

/// A reaper that ignores `SIGCHLD` lets its children go as they die
/// (`exit_notify` releases them at once): no zombie, no pid.
#[test]
fn a_child_its_reaper_lets_go_of_leaves_no_pid() {
    let (job, parent) = a_parent(45_333);
    parent.linux().set_signal_action(
        LinuxSignal::SIGCHLD,
        SignalAction {
            handler: crate::signal::SIG_IGN,
            flags: SignalActionFlags::empty(),
            restorer: 0,
            mask: Sigset::default(),
        },
    );
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    Thread::create_linux(&child).unwrap();
    child.exit(0);

    assert!(
        job.find_process(pid).is_some(),
        "the case this test is about"
    );
    assert!(child.linux().is_reaped());
    assert_eq!(found(&job, pid), None);
}

/// Alive, the lookup is the job's; a pid nobody has is nothing.
#[test]
fn a_live_process_is_found_and_nobody_is_not() {
    let (job, parent) = a_parent(45_334);
    assert_eq!(found(&job, parent.id()), Some(parent.id()));
    assert_eq!(found(&job, 45_338), None);
    assert!(unreaped_zombies_in(&job).is_empty());
}
/// A child that has just exited is already a zombie for `/proc`, before its
/// teardown has filed it as one. `record_child_exit` moves it from the live
/// table to the zombies during the teardown, so in the gap the only place it
/// exists is `children`, with an `Exited` status -- and the zombie list has
/// to read that table too, or `ps` showed no `<defunct>` for a child that
/// had already died.
///
/// The gap is reached the way another CPU reaches it: a `PROCESS_TERMINATED`
/// callback registered after fork's runs first, with the status published
/// and the teardown not started. Note what the job says there: it still has
/// the child, which is why [`find_task`] never needs the live table for a
/// single pid -- a pid the job has lost is a pid already filed.
#[test]
fn a_child_that_has_just_exited_is_already_a_zombie_for_proc() {
    let (job, parent) = a_parent(45_335);
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();

    let seen = Arc::new(StdMutex::new(None));
    let out = seen.clone();
    let watched = job.clone();
    let reaper = parent.clone();
    child.add_signal_callback(Box::new(move |signal| {
        if signal.contains(Signal::PROCESS_TERMINATED) {
            let zombies: Vec<KoID> = unreaped_zombies_in(&watched)
                .iter()
                .map(|z| z.id())
                .collect();
            *out.lock().unwrap() = Some((
                watched.find_process(pid).is_some(),
                reaper.linux().is_zombie_child(pid),
                found(&watched, pid),
                zombies,
            ));
            return true;
        }
        false
    }));

    child.exit(0);

    let (in_its_job, on_file, names, zombies) =
        seen.lock().unwrap().take().expect("the callback never ran");
    assert_eq!(
        (in_its_job, on_file, names),
        (true, false, Some(pid)),
        "(still in its job, on file as a zombie, what its pid names)"
    );
    assert_eq!(
        zombies,
        [pid],
        "`/proc` did not list a child that had already exited"
    );

    reap(&parent, pid);
    assert_eq!(found(&job, pid), None);
}

/// A live child is not a zombie. The zombie list reads the parent's LIVE
/// table as well as its zombies, because a child that has just exited is
/// still in the live one, so it has to tell the two apart by status: taking
/// the live table whole listed every running child as a `<defunct>`.
#[test]
fn a_live_child_is_not_a_zombie() {
    let (job, parent) = a_parent(45_336);
    let alive = Process::fork_from(&parent).unwrap();
    Thread::create_linux(&alive).unwrap();
    let dead = Process::fork_from(&parent).unwrap();
    let dead_pid = dead.id();
    dead.exit(0);
    drop(dead);

    let zombies: Vec<KoID> = unreaped_zombies_in(&job).iter().map(|z| z.id()).collect();
    assert_eq!(
        zombies,
        [dead_pid],
        "the live child {} was counted among the zombies",
        alive.id()
    );

    reap(&parent, dead_pid);
    assert!(unreaped_zombies_in(&job).is_empty());
}
