//! A signal sent to a process: where it is made pending, and on whose
//! account (`__send_signal_locked` for a process-directed signal, and the
//! process half of `prepare_signal`).

use super::*;
use crate::signal::queue::{QueueFull, Receiver};

#[cfg(test)]
mod tests;

/// What a signal sent to a process brings: what the sender knows about it,
/// to be counted against the receiver, or the expiry of a POSIX timer, in
/// the timer's own entry.
#[derive(Clone, Copy)]
pub(super) enum SharedSend {
    Counted(Option<SigInfo>),
    Timer(SigInfo),
}

/// The `RLIMIT_SIGPENDING` `fork_init()` gives `init`, which every process
/// inherits, for a machine of `memory` bytes: half of `max_threads`, which
/// `set_max_threads()` sizes so that the kernel stacks of that many threads
/// (`THREAD_SIZE`, 16 KiB on all three architectures here) take an eighth
/// of memory, kept between `MIN_THREADS` and `FUTEX_TID_MASK`.
///
/// It was unlimited, which cost nothing while a signal was pending at most
/// once. Queued, an unlimited user could fill the kernel's fixed heap with
/// `sigqueue` entries nobody takes, and the heap running out ends the
/// machine. A 16 GiB machine gets 65536, as Linux gives it.
pub fn default_sigpending_limit(memory: usize) -> u64 {
    const THREAD_SIZE: u64 = 16 * 1024;
    const MIN_THREADS: u64 = 20;
    const MAX_THREADS: u64 = 0x3fff_ffff;
    let threads = (memory as u64 / (THREAD_SIZE * 8)).clamp(MIN_THREADS, MAX_THREADS);
    threads / 2
}

impl LinuxProcessInner {
    /// Whom a signal sent to this process is counted against: its real user,
    /// up to its `RLIMIT_SIGPENDING`.
    fn signal_receiver(&self) -> Receiver {
        let limit = self.limits.get(RLIMIT_SIGPENDING);
        Receiver {
            uid: self.credentials.ruid,
            limit: limit.map_or(RLIM_INFINITY, |l| l.cur),
        }
    }
}

impl LinuxProcess {
    /// [`LinuxProcessInner::signal_receiver`], for a signal aimed at one of
    /// this process's threads.
    pub fn signal_receiver(&self) -> Receiver {
        self.inner.lock().signal_receiver()
    }

    /// Queue a process-directed signal that every thread currently has
    /// blocked, counted against this process's real user: see
    /// [`SigQueue::send`].
    pub fn queue_shared_signal(
        &self,
        signal: LinuxSignal,
        info: Option<SigInfo>,
    ) -> Result<(), QueueFull> {
        self.queue_shared(signal, SharedSend::Counted(info))
    }

    fn queue_shared(&self, signal: LinuxSignal, how: SharedSend) -> Result<(), QueueFull> {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let sent = match how {
            SharedSend::Counted(info) => {
                let to = inner.signal_receiver();
                (inner.shared_queue).send(&mut inner.shared_pending, signal, info, Some(to))
            }
            SharedSend::Timer(info) => {
                (inner.shared_queue).send_timer(&mut inner.shared_pending, signal, info);
                Ok(())
            }
        };
        self.refresh_exit_path(&guard);
        sent
    }
}

/// `prepare_signal()`: a stop and a continue cancel each other where they are
/// waiting to be received, so a process never carries both and the last one
/// sent is the one that decides.
///
/// Process-wide whoever the signal was aimed at, because the state it touches
/// is: Linux's `prepare_signal` reaches the shared `task->signal`, so a
/// `SIGCONT` aimed at one thread flushes the pending stop of the whole group.
/// The thread-directed path did not call this at all -- see
/// [`complete_signal_for_job_control`].
///
/// The process's own pending set too, where a signal every thread blocks
/// waits: only the threads' were flushed, so a `SIGTSTP` blocked when a
/// `SIGCONT` came still stopped the process once it was unblocked. And each
/// signal goes with its `siginfo_t`, which stayed behind for the next one
/// of that number to be reported with.
pub fn prepare_signal(process: &Arc<Process>, signal: LinuxSignal) {
    use crate::thread::ThreadExt;
    let all = Sigset::new(u64::MAX);
    let cancelled = Sigset::new(!pending_after_send(all, signal).val());
    if cancelled.is_empty() {
        return;
    }
    if let Some(lp) = process.try_linux() {
        lp.discard_shared_signals(cancelled);
    }
    for thread in process.threads() {
        if let Some(mut lt) = thread.try_lock_linux() {
            lt.discard_signals(cancelled);
        }
    }
}

pub fn send_signal_to_process(pid: usize, signal: LinuxSignal) -> LxResult<()> {
    send_signal_to_process_with_info(pid, signal, None)
}

/// [`send_signal_to_process`], with the `siginfo_t` the handler will be
/// handed: who sent a `kill`, which child a `SIGCHLD` is about. `None` leaves
/// [`SigInfo::bare`].
///
/// Counted against the receiving process's real user: `EAGAIN` for a
/// real-time signal past its `RLIMIT_SIGPENDING` that `kill(2)` did not send
/// (see [`SigQueue::send`]).
pub fn send_signal_to_process_with_info(
    pid: usize,
    signal: LinuxSignal,
    info: Option<SigInfo>,
) -> LxResult<()> {
    send_to_process(pid, signal, SharedSend::Counted(info))
}

/// The expiry of a POSIX timer aimed at process `pid`, in the timer's own
/// entry ([`SigQueue::send_timer`]): never refused, since the timer was
/// counted when it was created.
pub fn send_timer_signal_to_process(
    pid: usize,
    signal: LinuxSignal,
    info: SigInfo,
) -> LxResult<()> {
    send_to_process(pid, signal, SharedSend::Timer(info))
}

fn send_to_process(pid: usize, signal: LinuxSignal, how: SharedSend) -> LxResult<()> {
    use crate::thread::ThreadExt;
    if let Some(process) = ROOT_JOB.find_process(pid as KoID) {
        if signal_trace_worthy(signal) {
            // Who signals whom: a process that exits on a handled SIGTERM/
            // SIGINT/SIGHUP leaves no other trace (the `[exit]` line only
            // covers default-disposition deaths), and "labwc/foot vanished
            // 25 ms after a job started" was otherwise unexplainable.
            let (spid, sname) = current_process_pid_name();
            zcore_drivers::klog_warn!(
                "[signal] {:?} -> pid {} ({}) from pid {} ({})",
                signal,
                process.id(),
                trace_name(&process),
                spid,
                sname
            );
        }
        let threads = process.threads();
        prepare_signal(&process, signal);
        let to = process.try_linux().map(LinuxProcess::signal_receiver);
        // Prefer a thread that has the signal *unblocked* — it can act on it
        // right away — and deliver there.
        let mut first: Option<Arc<Thread>> = None;
        for thread in threads {
            // `wants_signal()` in Linux: never a thread on its way out
            // (`PF_EXITING`). A dying thread takes no signal again, so one
            // queued to it is lost -- the SIGTERM that should have ended the
            // process, sent while a worker was in `pthread_exit`.
            if is_on_its_way_out(&thread) {
                continue;
            }
            // Peek without holding the guard across a move of `thread`.
            let delivered = if let Some(mut lt) = thread.try_lock_linux() {
                // `wants_signal()`: unblocked, or parked in
                // `rt_sigtimedwait` for exactly this signal.
                if lt.wants_signal(signal) {
                    match how {
                        SharedSend::Counted(info) => lt
                            .send_signal(signal, info, to)
                            .map_err(|_| LxError::EAGAIN)?,
                        SharedSend::Timer(info) => lt.send_timer_signal(signal, info),
                    }
                    true
                } else {
                    false
                }
            } else {
                // Lock held (e.g. PID 1 in waitpid): queue below rather
                // than dropping the signal — that drop is why lunarbar's
                // `kill 1` did nothing.
                false
            };
            if delivered {
                wake_signal_sleeper(&thread);
                // Wake waitpid(-1): PID 1 blocks on this zircon bit.
                process.signal_set(Signal::SIGCHLD);
                wake_for_job_control(&process, signal);
                return Ok(());
            }
            if first.is_none() {
                first = Some(thread);
            }
        }
        // Every thread has the signal blocked: it must still become *pending*
        // (POSIX — masking only delays delivery, it does not discard the
        // signal). Queue it once on `shared_pending` — never also on the first
        // tid — so a later unblock / sigwait / signalfd pull cannot deliver
        // the same process-directed signal twice.
        if first.is_some() {
            if let Some(lp) = process.try_linux() {
                lp.queue_shared(signal, how).map_err(|_| LxError::EAGAIN)?;
            }
            // Wake every sleeper: any thread may be the one that pulls next.
            for t in process.threads() {
                wake_signal_sleeper(&t);
            }
        }
        // Pulse even when every thread had the Linux signal blocked: waitpid
        // still needs to return so the waiter can notice the pending set.
        process.signal_set(Signal::SIGCHLD);
        wake_for_job_control(&process, signal);
        Ok(())
    } else {
        Err(LxError::ESRCH)
    }
}
