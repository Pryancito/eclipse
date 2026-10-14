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

/// The throttle on the `[wait]` trace, which is the whole of what keeps a
/// storm of `EINTR` from pushing the rest of the kernel log out of the ring
/// buffer. A capture from real hardware held 118461 of these lines inside a
/// 2-second window and nothing else from the preceding 17 minutes.
mod wait_trace_throttle {
    use super::super::{wait_trace_is_due, WAIT_TRACE_MIN_GAP_NS};

    /// `last == 0` is "never printed", and a one-off failure -- the case the
    /// trace was written for, a compositor dying on a single `epoll_wait`
    /// error -- has to reach the log on its first and only line.
    #[test]
    fn first_line_always_prints() {
        assert!(wait_trace_is_due(0, 0, WAIT_TRACE_MIN_GAP_NS));
        assert!(wait_trace_is_due(1, 0, WAIT_TRACE_MIN_GAP_NS));
        assert!(wait_trace_is_due(u64::MAX, 0, WAIT_TRACE_MIN_GAP_NS));
    }

    /// Inside the gap: suppressed. This is the 59000-lines-per-second case,
    /// and the only test here that fails if the throttle is removed outright.
    #[test]
    fn inside_the_gap_is_suppressed() {
        let last = 1_000_000_000;
        assert!(!wait_trace_is_due(last, last, WAIT_TRACE_MIN_GAP_NS));
        assert!(!wait_trace_is_due(last + 1, last, WAIT_TRACE_MIN_GAP_NS));
        // 17us apart, the interval the firefox capture ran at.
        assert!(!wait_trace_is_due(
            last + 17_000,
            last,
            WAIT_TRACE_MIN_GAP_NS
        ));
        assert!(!wait_trace_is_due(
            last + WAIT_TRACE_MIN_GAP_NS - 1,
            last,
            WAIT_TRACE_MIN_GAP_NS
        ));
    }

    /// Exactly at the gap prints. A `>` instead of `>=` makes the gap
    /// unreachable on a clock whose tick lands on it, which on a coarse
    /// monotonic source is every tick.
    #[test]
    fn exactly_at_the_gap_prints() {
        let last = 1_000_000_000;
        assert!(wait_trace_is_due(
            last + WAIT_TRACE_MIN_GAP_NS,
            last,
            WAIT_TRACE_MIN_GAP_NS
        ));
        assert!(wait_trace_is_due(
            last + WAIT_TRACE_MIN_GAP_NS + 1,
            last,
            WAIT_TRACE_MIN_GAP_NS
        ));
    }

    /// A clock that went backwards must print, not go quiet. With a bare
    /// `now - last` the subtraction wraps to a number no gap can exceed, so
    /// the trace would stay silent until the clock caught up -- and the one
    /// thing this kernel's monotonic clock has already been seen to do is
    /// hand out a base it had not set yet.
    #[test]
    fn clock_going_backwards_prints() {
        assert!(wait_trace_is_due(5, 1_000_000_000, WAIT_TRACE_MIN_GAP_NS));
        assert!(wait_trace_is_due(0, 1, WAIT_TRACE_MIN_GAP_NS));
    }

    /// The configured gap is the one the caller uses, and it is the value the
    /// rate of the log depends on: 100ms, i.e. at most ten lines a second.
    #[test]
    fn configured_gap_is_a_tenth_of_a_second() {
        assert_eq!(WAIT_TRACE_MIN_GAP_NS, 100_000_000);
    }
}
