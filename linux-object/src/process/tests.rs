use super::signal_default_action_interrupts;
use crate::signal::Signal as LinuxSignal;

/// A wait whose own future knows nothing about signals still has to end
/// when one arrives. The case in the kernel is a blocking `read` of a
/// pipe: it parks on the pipe's event bus, which only a writer or a close
/// ever fires, so the thread sat there deaf to everything -- and musl's
/// `__synccall` (`setuid` and friends) stops every other thread with
/// SIGRT34 and waits for each to check in, so one thread parked here hung
/// the whole process, past `^C`, for good.
mod interruptible {
    extern crate std;

    use crate::error::{LxError, LxResult};
    use crate::process::{interruptible_on, send_signal_to_process, LinuxProcess, ProcessExt};
    use crate::signal::{Signal as LinuxSignal, SignalAction, SignalActionFlags};
    use crate::thread::ThreadExt;
    use alloc::sync::Arc;
    use core::future::Future;
    use core::time::Duration;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::object::{KernelObject, KoID};
    use zircon_object::task::{Process, Thread, ROOT_JOB};

    /// One thread of a throwaway process, with SIGUSR1 caught so it
    /// interrupts a wait.
    fn a_waiter(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "waiter",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        proc.linux().set_signal_action(
            LinuxSignal::SIGUSR1,
            SignalAction {
                handler: 0x1000,
                flags: SignalActionFlags::empty(),
                restorer: 0,
                mask: Default::default(),
            },
        );
        let thread = Thread::create_linux(&proc).unwrap();
        (proc, thread)
    }

    fn signal_later(pid: KoID, signal: LinuxSignal, after: Duration) {
        std::thread::spawn(move || {
            std::thread::sleep(after);
            let _ = send_signal_to_process(pid as usize, signal);
        });
    }

    /// The wait, on a host thread of its own with a watchdog: a wait that
    /// stops being interruptible would otherwise hang the whole suite
    /// instead of failing one test, and `pending` never ends by itself.
    /// The margin is orders of magnitude over the ~50 ms these take.
    fn wait_on<F>(thread: &Arc<Thread>, future: F) -> (LxResult<F::Output>, Duration)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let thread = thread.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let start = std::time::Instant::now();
        std::thread::spawn(move || {
            let r = async_std::task::block_on(interruptible_on(&thread, future));
            let _ = tx.send(r);
        });
        let r = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the wait never returned");
        (r, start.elapsed())
    }

    #[test]
    fn a_signal_ends_a_wait_that_would_otherwise_never_return() {
        let (proc, thread) = a_waiter(44_101);
        signal_later(proc.id(), LinuxSignal::SIGUSR1, Duration::from_millis(50));
        let (res, took) = wait_on(&thread, core::future::pending::<()>());
        assert_eq!(res, Err(LxError::EINTR));
        assert!(took < Duration::from_secs(2), "returned after {:?}", took);
    }

    #[test]
    fn an_ignored_signal_does_not_end_the_wait() {
        // SIGCHLD's default action is ignore, so Linux does not interrupt
        // a syscall with it and neither does this. Interrupting on one
        // would give every parent of a short-lived child a spurious EINTR
        // out of a perfectly healthy read.
        let (proc, thread) = a_waiter(44_102);
        signal_later(proc.id(), LinuxSignal::SIGCHLD, Duration::from_millis(20));
        let (res, _took) = wait_on(&thread, async {
            async_std::task::sleep(Duration::from_millis(150)).await;
            7u32
        });
        assert_eq!(res, Ok(7), "an ignored signal cut the wait short");
    }

    #[test]
    fn a_future_that_finishes_first_is_not_disturbed() {
        let (_proc, thread) = a_waiter(44_103);
        let (res, _took) = wait_on(&thread, core::future::ready(3u32));
        assert_eq!(res, Ok(3));
    }

    #[test]
    fn a_signal_already_pending_never_starts_the_wait() {
        let (proc, thread) = a_waiter(44_104);
        send_signal_to_process(proc.id() as usize, LinuxSignal::SIGUSR1).unwrap();
        let (res, took) = wait_on(&thread, core::future::pending::<()>());
        assert_eq!(res, Err(LxError::EINTR));
        assert!(
            took < Duration::from_millis(500),
            "waited {:?} anyway",
            took
        );
    }
}

#[test]
fn default_ignored_and_stop_signals_do_not_interrupt_waits() {
    for sig in [
        LinuxSignal::SIGCHLD,
        LinuxSignal::SIGURG,
        LinuxSignal::SIGWINCH,
        LinuxSignal::SIGCONT,
        LinuxSignal::SIGSTOP,
        LinuxSignal::SIGTSTP,
        LinuxSignal::SIGTTIN,
        LinuxSignal::SIGTTOU,
    ] {
        assert!(
            !signal_default_action_interrupts(sig),
            "{:?} should not interrupt",
            sig
        );
    }
}

#[test]
fn default_terminating_signals_interrupt_waits() {
    for sig in [
        LinuxSignal::SIGHUP,
        LinuxSignal::SIGINT,
        LinuxSignal::SIGTERM,
    ] {
        assert!(
            signal_default_action_interrupts(sig),
            "{:?} should interrupt",
            sig
        );
    }
}
