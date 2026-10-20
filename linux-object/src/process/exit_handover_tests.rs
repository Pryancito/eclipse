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

/// The three tests above pin the `None` half: a dead child is not waitable
/// until its teardown says so. This is the other half, and it is the
/// sentence the module exists for -- once the handover has happened the
/// child IS waitable, while it is still a live child and before anybody has
/// reaped it.
///
/// Asked of `waitable_exit_code` directly, because a wait cannot reach it
/// from out here: `wait_child_interest` looks in `reaped_children` first,
/// and by the time an exit has returned the zombie is on file there. The
/// live path is only ever taken inside this window, so the window is where
/// the question has to be asked.
#[test]
fn a_child_that_has_handed_over_is_waitable_while_it_is_still_a_live_child() {
    let [_reaper, parent, _orphan] = a_subreaper_its_child_and_grandchild(45_104);
    let child = parent.clone();
    let seen = before_the_teardown_of(&parent, move || {
        let before = exit_handover::waitable_exit_code(&child);
        child.linux().mark_exit_handed_over();
        (before, exit_handover::waitable_exit_code(&child))
    });

    parent.exit(3);

    assert_eq!(
        seen.lock().unwrap().take(),
        Some((None, Some(3))),
        "the handover is what makes a dead child waitable, and nothing else"
    );
}

/// `fetch_max` and not a store, which is the whole of what the comment on it
/// says. A reaper that has let a zombie go put the stage at REAPED --
/// `release_task()` freed the pid in the same breath, so from then on the pid
/// names nothing -- and a teardown still running behind it must not take that
/// pid back to a zombie a second `wait*` could collect.
#[test]
fn a_pid_a_reaper_has_let_go_of_does_not_come_back_as_a_zombie() {
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 45_105).unwrap();
    exit_handover::released(Some(proc.clone()));
    assert!(proc.linux().is_reaped(), "`released` is `release_task()`");

    proc.linux().mark_exit_handed_over();

    assert!(
        proc.linux().is_reaped(),
        "the handover took a reaped pid back to being a collectable zombie"
    );
}

/// A process with no Linux side has no teardown to wait for, so its exit is
/// collectable the moment its status says so. The stage only ever speaks for
/// a process that has one, which is what `is_none_or` says.
#[test]
fn a_process_with_no_linux_side_is_waitable_as_soon_as_it_has_exited() {
    let zircon = Process::create(&ROOT_JOB, "zircon").unwrap();
    zircon.exit(7);

    assert_eq!(
        exit_handover::waitable_exit_code(&zircon),
        Some(7),
        "a process with no handover of its own was made to wait for one"
    );
}
