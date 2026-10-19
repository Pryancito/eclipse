//! A dead child is waitable once its teardown has handed over what it had,
//! not the moment its status says `Exited`: see `exit_handover`.
//!
//! The gap is reached here the way another CPU reaches it. `Process::exit`
//! publishes the status and then sets `PROCESS_TERMINATED`, whose callbacks
//! run most recent first, so one registered after `fork`'s runs BEFORE the
//! teardown with the child already `Exited`; and a process exit hook runs in
//! the middle of the teardown, after the handover of the children and
//! before the files go.

extern crate std;

use super::*;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;
use std::sync::Mutex as StdMutex;

/// A process of its own that volunteered as a subreaper, the way
/// `signal-probe`'s `orphan` section does, with a child that has a child.
fn a_subreaper_its_child_and_grandchild(pid: KoID) -> [Arc<Process>; 3] {
    let reaper = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap();
    reaper.linux().set_child_subreaper(true);
    let parent = Process::fork_from(&reaper).unwrap();
    let orphan = Process::fork_from(&parent).unwrap();
    [reaper, parent, orphan]
}

/// What a `waitpid(pid, WNOHANG)` would tell `reaper`, as the pid it reaped.
fn waitpid_nohang(reaper: &Arc<Process>, pid: KoID) -> LxResult<KoID> {
    block_on(wait_child(reaper, pid, true, true)).map(|_| pid)
}

/// What a `waitpid(-1, WNOHANG)` would tell `reaper`.
fn wait_any_nohang(reaper: &Arc<Process>) -> LxResult<KoID> {
    block_on(wait_child_any(reaper, true, true)).map(|(pid, _, _)| pid)
}

/// Run `look` once, inside the gap: `child` already `Exited`, its teardown
/// not started.
fn before_the_teardown_of<T: Send + 'static>(
    child: &Arc<Process>,
    look: impl Fn() -> T + Send + 'static,
) -> Arc<StdMutex<Option<T>>> {
    let seen = Arc::new(StdMutex::new(None));
    let out = seen.clone();
    child.add_signal_callback(Box::new(move |signal| {
        if signal.contains(Signal::PROCESS_TERMINATED) {
            *out.lock().unwrap() = Some(look());
            return true;
        }
        false
    }));
    seen
}

/// The other half of every test: once the teardown has run, the wait takes
/// the dead child, and by then the orphan is the reaper's.
fn after_the_teardown(reaper: &Arc<Process>, parent: &Arc<Process>, orphan: &Arc<Process>) {
    assert_eq!(
        block_on(wait_child(reaper, parent.id(), true, true)).map(|(status, _)| status),
        Ok(wait_status_exited(0)),
        "the dead child never became waitable"
    );
    assert!(
        reaper.linux().has_child(orphan.id()),
        "the wait returned and the orphan was not ours"
    );
    assert_eq!(orphan.linux().parent().map(|p| p.id()), Some(reaper.id()));
}

#[test]
fn a_waitpid_for_a_dying_child_does_not_take_it_before_its_orphans_are_handed_over() {
    let [reaper, parent, orphan] = a_subreaper_its_child_and_grandchild(45_101);
    let (r, pid) = (reaper.clone(), parent.id());
    let seen = before_the_teardown_of(&parent, move || waitpid_nohang(&r, pid));

    parent.exit(0);

    assert_eq!(
        seen.lock().unwrap().take(),
        Some(Err(LxError::EAGAIN)),
        "waitpid took the child while its own child was still its own"
    );
    after_the_teardown(&reaper, &parent, &orphan);
}

#[test]
fn nor_does_a_wait_for_any_child() {
    let [reaper, parent, orphan] = a_subreaper_its_child_and_grandchild(45_102);
    let r = reaper.clone();
    let seen = before_the_teardown_of(&parent, move || wait_any_nohang(&r));

    parent.exit(0);

    assert_eq!(
        seen.lock().unwrap().take(),
        Some(Err(LxError::EAGAIN)),
        "wait(-1) took the child while its own child was still its own"
    );
    after_the_teardown(&reaper, &parent, &orphan);
}

/// Who the hook below watches for, and what a `waitpid` told it.
static WATCHED: StdMutex<Option<(KoID, Arc<Process>)>> = StdMutex::new(None);
static WAITED: StdMutex<Option<LxResult<KoID>>> = StdMutex::new(None);

fn waitpid_from_the_exit_hook(pid: KoID) {
    let watched = WATCHED.lock().unwrap().clone();
    if let Some((dying, reaper)) = watched {
        if dying == pid {
            *WAITED.lock().unwrap() = Some(waitpid_nohang(&reaper, pid));
        }
    }
}

/// The teardown is under way here and the orphan already handed over, but
/// the files have not gone yet: a wait still has to come back empty.
#[test]
fn nor_while_its_teardown_is_still_running() {
    let [reaper, parent, orphan] = a_subreaper_its_child_and_grandchild(45_103);
    *WATCHED.lock().unwrap() = Some((parent.id(), reaper.clone()));
    register_process_exit_hook(waitpid_from_the_exit_hook);

    parent.exit(0);

    *WATCHED.lock().unwrap() = None;
    assert_eq!(
        WAITED.lock().unwrap().take(),
        Some(Err(LxError::EAGAIN)),
        "waitpid took the child in the middle of its teardown"
    );
    after_the_teardown(&reaper, &parent, &orphan);
}
