//! A killable wait: what `vfork`'s parent waits in until the child execs.
//!
//! Linux's `wait_for_vfork_done()` is `TASK_KILLABLE`. A signal that runs a
//! handler waits until the child has exec'd; a kill ends the wait. Here the
//! wait was on the child's process object alone, so a parent whose child
//! never reached `exec` -- a `posix_spawn` whose file action opens a FIFO
//! nobody writes to -- could not be killed, and an `exit_group` in another of
//! its threads left the process reaped but never torn down.
//!
//! The waits run on a host thread of their own with a watchdog, so a wait the
//! kill does not end fails one test instead of hanging the suite.
extern crate std;

use super::*;
use crate::signal::SignalActionFlags;
use crate::thread::ThreadExt;
use core::time::Duration;
use rcore_fs_ramfs::RamFS;
use std::sync::mpsc;
use zircon_object::task::Task;

fn linux_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A parent with one thread, and the child it vforked.
fn parent_and_child(pid: KoID) -> (Arc<Process>, Arc<Thread>, Arc<Process>) {
    let parent = linux_process(pid, "parent");
    let thread = Thread::create_linux(&parent).unwrap();
    let child = linux_process(pid + 1, "child");
    (parent, thread, child)
}

/// The vfork wait, as `thread`, on a host thread of its own: `Some` once the
/// child has exec'd or ended, `None` when the kill won.
fn vfork_wait(thread: &Arc<Thread>, child: &Arc<Process>) -> mpsc::Receiver<Option<()>> {
    let thread = thread.clone();
    let child: Arc<dyn KernelObject> = child.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = async_std::task::block_on(async move {
            kernel_hal::thread::set_current_thread(Some(thread.clone()));
            killable_on(
                &thread,
                child.wait_signal(Signal::USER_SIGNAL_0 | Signal::PROCESS_TERMINATED),
            )
            .await
            .map(|_| ())
        });
        let _ = tx.send(r);
    });
    // Long enough for the wait to be parked, not merely started.
    std::thread::sleep(Duration::from_millis(50));
    rx
}

/// Generous: a wake ends the wait in microseconds.
const WATCHDOG: Duration = Duration::from_secs(3);

/// How long a wait that must go on is watched for.
const STILL: Duration = Duration::from_millis(150);

fn catch(proc: &Arc<Process>, sig: LinuxSignal) {
    proc.linux().set_signal_action(
        sig,
        SignalAction {
            handler: 0x1000,
            flags: SignalActionFlags::empty(),
            restorer: 0,
            mask: Sigset::default(),
        },
    );
}

#[test]
fn the_child_exec_ends_the_wait() {
    let (_parent, thread, child) = parent_and_child(48_301);
    let wait = vfork_wait(&thread, &child);
    assert!(wait.recv_timeout(STILL).is_err(), "over before the exec");

    child.signal_set(Signal::USER_SIGNAL_0);

    assert_eq!(wait.recv_timeout(WATCHDOG), Ok(Some(())));
}

/// An `exit_group` in another thread of the parent, or `execve`'s
/// `de_thread`: the thread in the wait is killed.
#[test]
fn a_kill_ends_it() {
    let (_parent, thread, child) = parent_and_child(48_303);
    let wait = vfork_wait(&thread, &child);

    thread.kill();

    assert_eq!(
        wait.recv_timeout(WATCHDOG),
        Ok(None),
        "the killed parent waited on for a child that never execs"
    );
}

#[test]
fn so_does_its_process_exiting() {
    let (parent, thread, child) = parent_and_child(48_305);
    let wait = vfork_wait(&thread, &child);

    parent.exit(0);

    assert_eq!(wait.recv_timeout(WATCHDOG), Ok(None));
}

/// ^C, or a `kill` from a shell: a SIGTERM or SIGINT that nobody catches
/// ends the process, so it ends the wait too.
#[test]
fn so_does_a_signal_that_nobody_catches() {
    let (parent, thread, child) = parent_and_child(48_307);
    let wait = vfork_wait(&thread, &child);

    send_signal_to_process(parent.id() as usize, LinuxSignal::SIGTERM).unwrap();

    assert_eq!(wait.recv_timeout(WATCHDOG), Ok(None));
}

/// A caught signal waits for the child -- its handler runs once the wait is
/// over -- and neither does a signal whose default is to be ignored end it.
#[test]
fn a_caught_signal_does_not() {
    let (parent, thread, child) = parent_and_child(48_309);
    catch(&parent, LinuxSignal::SIGUSR1);
    let wait = vfork_wait(&thread, &child);

    send_signal_to_process(parent.id() as usize, LinuxSignal::SIGUSR1).unwrap();
    send_signal_to_process(parent.id() as usize, LinuxSignal::SIGCHLD).unwrap();

    assert!(
        wait.recv_timeout(STILL).is_err(),
        "a signal with a handler ended a killable wait"
    );
    child.signal_set(Signal::USER_SIGNAL_0);
    assert_eq!(wait.recv_timeout(WATCHDOG), Ok(Some(())));
    assert!(
        thread.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "the handler is still owed its signal"
    );
}

/// A blocked SIGTERM is no death yet: it waits for the mask to let it in.
/// Queued on the thread itself, the way `tgkill` does.
#[test]
fn nor_does_a_blocked_one() {
    let (_parent, thread, child) = parent_and_child(48_311);
    thread
        .lock_linux()
        .set_signal_mask(Sigset::new(1 << (LinuxSignal::SIGTERM as u64 - 1)));
    let wait = vfork_wait(&thread, &child);

    thread.lock_linux().queue_signal(LinuxSignal::SIGTERM, None);
    wake_signal_sleeper(&thread);

    assert!(wait.recv_timeout(STILL).is_err());
    child.signal_set(Signal::USER_SIGNAL_0);
    assert_eq!(wait.recv_timeout(WATCHDOG), Ok(Some(())));
}
