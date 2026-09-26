//! `signalfd(2)` — accept signals through a readable file descriptor instead of
//! a handler. libwayland's `wl_event_loop_add_signal` blocks a signal and reads
//! it here, so a Wayland compositor (labwc) handles SIGINT/SIGTERM/SIGCHLD from
//! its event loop. Without it, the blocked signal sits pending forever and
//! Ctrl-C does nothing.

use super::*;
// `crate::signal::Signal` (the Linux signal enum) would shadow
// `zircon_object::object::Signal` (the KObject signal bits used by
// `impl_kobject!`), so alias it.
use crate::signal::{SigInfo, Signal as LinuxSignal, Sigset};
use crate::thread::ThreadExt;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
use lock::Mutex;
use zircon_object::object::*;
use zircon_object::task::Thread;

/// `sizeof(struct signalfd_siginfo)`.
const SIGINFO_SIZE: usize = 128;

/// signalfd implementation. The set of accepted signals is mutable
/// (`signalfd4` with an existing fd updates it).
pub struct SignalFd {
    base: KObjectBase,
    mask: Arc<AtomicU64>,
    /// Behind a lock so `fcntl(F_SETFL)` can change it after creation.
    flags: Mutex<OpenFlags>,
}

impl_kobject!(SignalFd);

impl SignalFd {
    /// The signals a signalfd is allowed to accept.
    ///
    /// `do_signalfd4` filters the set the caller handed over before it is
    /// stored:
    ///
    /// ```c
    /// sigdelsetmask(&sigmask, sigmask(SIGKILL) | sigmask(SIGSTOP));
    /// ```
    ///
    /// The same two signals every mask in this kernel loses, for a reason of
    /// its own and a worse one: a signalfd does not merely *defer* what it
    /// accepts, it **consumes** it -- [`SignalFd::consume_one`] takes the bit
    /// out of the thread's pending set. So a mask naming SIGKILL is a process
    /// eating its own death: `pthread_kill(t, SIGKILL)` and any `tgkill` make
    /// the signal pending, a `read` on the fd removes it, and the delivery
    /// path never sees it. One naming SIGSTOP swallows every `kill -STOP`.
    ///
    /// Filtered here rather than in the syscall because the mask has two
    /// doors -- a new fd and a replacement on an existing one -- and both
    /// lead here ([`Sigset::blockable`] names the rest of the family).
    fn accepted(mask: u64) -> u64 {
        Sigset::new(mask).blockable().val()
    }

    /// Create a signalfd watching the signals in `mask`.
    pub fn new(mask: u64, flags: OpenFlags) -> Arc<Self> {
        Arc::new(SignalFd {
            base: KObjectBase::new(),
            mask: Arc::new(AtomicU64::new(Self::accepted(mask))),
            flags: Mutex::new(flags),
        })
    }

    /// Replace the accepted-signal mask (`signalfd4` on an existing fd).
    pub fn set_mask(&self, mask: u64) {
        self.mask.store(Self::accepted(mask), SeqCst);
    }

    /// The calling thread, when it is a Linux thread.
    fn current_thread() -> Option<Arc<Thread>> {
        kernel_hal::thread::get_current_thread()?
            .downcast::<Thread>()
            .ok()
    }

    /// The calling thread's pending signals that this fd accepts.
    fn pending_matched(&self) -> Sigset {
        let mask = self.mask.load(SeqCst);
        if let Some(thread) = Self::current_thread() {
            let tl = thread.lock_linux();
            return Sigset::new(tl.signals.val() & mask);
        }
        Sigset::empty()
    }

    /// A blocking `read` with nothing to take yet: park the calling thread
    /// until a signal is queued to it, or until one outside the mask
    /// interrupts (`signalfd_dequeue` -> `wait_event_interruptible`, so
    /// `EINTR`). While it waits the thread announces the mask as its
    /// `sigwait` set, the way `rt_sigtimedwait` does: the signals a
    /// signalfd takes are blocked, and a process-directed one has to reach
    /// the thread reading the fd rather than whichever thread comes first
    /// (Linux reads them off the shared pending set; here, see
    /// `LinuxThread::wants_signal`). Without that, a program with a
    /// dedicated signal thread and every other thread blocking the signal
    /// never had its `read` return.
    ///
    /// This used to be a 20 ms sleep in a loop: a daemon parked in the read
    /// woke fifty times a second for nothing, and took up to 20 ms to see
    /// the signal.
    async fn wait_for_one(&self, buf: &mut [u8], thread: &Arc<Thread>) -> LxResult<usize> {
        use crate::process::{check_signals_of, SignalPark};
        thread.lock_linux().sigwait = Sigset::new(self.mask.load(SeqCst));
        let mut park = SignalPark::new(thread);
        let outcome = loop {
            park.prepare();
            if let Some(info) = self.consume_one() {
                buf[..SIGINFO_SIZE].copy_from_slice(&signalfd_record(&info));
                break Ok(SIGINFO_SIZE);
            }
            if let Err(e) = check_signals_of(thread) {
                break Err(e);
            }
            park.park(None).await;
        };
        thread.lock_linux().sigwait = Sigset::empty();
        outcome
    }

    /// Consume and return the lowest-numbered accepted pending signal, removing
    /// it from the calling thread's pending set, with the `siginfo_t` that
    /// came with it.
    fn consume_one(&self) -> Option<SigInfo> {
        let mask = self.mask.load(SeqCst);
        let thread = Self::current_thread()?;
        let mut tl = thread.lock_linux();
        let sig = Sigset::new(tl.signals.val() & mask).find_first_signal()?;
        Some(tl.take_siginfo(sig))
    }
}

/// `struct signalfd_siginfo` for one consumed signal: `signalfd_copyinfo`.
///
/// The layout is the uAPI's, not `siginfo_t`'s: `ssi_signo` (0), `ssi_errno`
/// (4), `ssi_code` (8), `ssi_pid` (12), `ssi_uid` (16), ..., `ssi_status`
/// (40). Which fields are filled depends on what the signal carries, as in
/// the kernel's `siginfo_layout`: a signal a process sent (`SI_USER`,
/// `SI_TKILL`, `SI_QUEUE`) has a sender; a `SIGCHLD` has the child, its uid
/// and its status. An event loop reading a `SIGCHLD` off a signalfd is
/// looking for exactly `ssi_pid` and `ssi_status`, and used to get zeros.
fn signalfd_record(info: &SigInfo) -> [u8; SIGINFO_SIZE] {
    let src = info.as_bytes();
    let mut out = [0u8; SIGINFO_SIZE];
    out[..4].copy_from_slice(&(info.signo as u32).to_ne_bytes());
    out[4..8].copy_from_slice(&info.errno.to_ne_bytes());
    out[8..12].copy_from_slice(&info.code.0.to_ne_bytes());
    let sent_by_a_process = info.code.from_a_process();
    let about_a_child = info.signo == LinuxSignal::SIGCHLD as i32 && !sent_by_a_process;
    if sent_by_a_process || about_a_child {
        // `_kill` and `_sigchld` both start with `si_pid`, `si_uid`.
        out[12..20].copy_from_slice(&src[16..24]);
    }
    if about_a_child {
        out[40..44].copy_from_slice(&src[24..28]);
    }
    out
}

#[async_trait]
impl FileLike for SignalFd {
    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.flags.lock().take_settable(f);
        Ok(())
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        if buf.len() < SIGINFO_SIZE {
            return Err(LxError::EINVAL);
        }
        if let Some(info) = self.consume_one() {
            buf[..SIGINFO_SIZE].copy_from_slice(&signalfd_record(&info));
            return Ok(SIGINFO_SIZE);
        }
        if self.flags().non_block() {
            return Err(LxError::EAGAIN);
        }
        match Self::current_thread() {
            Some(thread) => self.wait_for_one(buf, &thread).await,
            // No Linux thread behind the call: nothing can ever be queued
            // to it, so there is nothing to wait for.
            None => Err(LxError::EAGAIN),
        }
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        self.read(buf).await
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus {
            read: self.pending_matched().is_not_empty(),
            write: false,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        // Signals don't fire an EventBus, so this future resolves only once a
        // matching signal is already pending; callers (epoll/select) re-poll on
        // their own timer tick, which bounds the latency.
        let status = self.poll(_events)?;
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for `signalfd(2)`.
    //!
    //! Everything that *delivers* a signal here goes through the calling
    //! thread's pending set, and a host unit test has no such thread — so what
    //! is covered is the part a regression would break first and that needs no
    //! thread: the accepted-signal mask, the `dup` that shares it, and the
    //! read/write contract. With no thread context `pending_matched` is empty,
    //! which is also the state a real signalfd sits in almost all the time.

    use super::*;
    use async_std::task::block_on;

    /// A `sigset_t` the way userspace builds one, which is also the way the
    /// rest of the kernel reads one: `sigaddset` sets bit `sig - 1`, so
    /// signal 1 is bit 0. This used to shift by the signal number itself,
    /// which is a set one bit too high all the way along -- harmless while
    /// these tests only pass the value around, and a mask naming the wrong
    /// signals the moment one of them compares it against a pending set.
    fn mask_of(signals: &[LinuxSignal]) -> u64 {
        signals.iter().fold(0u64, |m, s| m | s.as_bit())
    }

    #[test]
    fn the_mask_is_in_the_same_numbering_as_every_other_signal_set() {
        // `pending_matched` ANDs this mask with the thread's pending `Sigset`,
        // so the two have to agree on which bit is which signal or a signalfd
        // watching SIGINT wakes on SIGQUIT.
        let mask = mask_of(&[LinuxSignal::SIGINT]);
        let mut pending = Sigset::empty();
        pending.insert(LinuxSignal::SIGINT);
        assert_eq!(mask, pending.val());
        assert_eq!(mask, 1 << 1, "SIGINT is signal 2, so it is bit 1");
    }

    fn sfd(mask: u64, flags: OpenFlags) -> Arc<SignalFd> {
        SignalFd::new(mask, flags)
    }

    /// `fcntl(F_SETFL, O_NONBLOCK)` on a signalfd made without `SFD_NONBLOCK`
    /// used to change nothing, so a read with no matching signal pending
    /// slept in 20 ms ticks for good instead of answering EAGAIN. If this
    /// test hangs, that is the bug back.
    #[test]
    fn set_flags_turns_a_blocking_signalfd_non_blocking() {
        let fd = sfd(mask_of(&[LinuxSignal::SIGINT]), OpenFlags::empty());
        assert!(!fd.flags().non_block());
        fd.set_flags(OpenFlags::NON_BLOCK).unwrap();
        assert!(fd.flags().non_block());
        let mut buf = [0u8; SIGINFO_SIZE];
        assert_eq!(block_on(fd.read(&mut buf)), Err(LxError::EAGAIN));
    }

    /// The one a signalfd must never be allowed to take.
    ///
    /// `SignalFd` does not defer what it accepts, it **consumes** it: a read
    /// takes the bit out of the thread's pending set, and nothing delivers it
    /// afterwards. So a mask naming SIGKILL is a process eating its own
    /// death -- `pthread_kill(t, SIGKILL)` is a `tgkill`, which makes the
    /// signal pending like any other -- and one naming SIGSTOP swallows every
    /// `kill -STOP`. `do_signalfd4` deletes both before it stores the set.
    #[test]
    fn a_signalfd_cannot_accept_the_two_signals_nothing_may_keep() {
        let fd = sfd(
            mask_of(&[
                LinuxSignal::SIGKILL,
                LinuxSignal::SIGSTOP,
                LinuxSignal::SIGINT,
            ]),
            OpenFlags::NON_BLOCK,
        );
        let got = Sigset::new(fd.mask.load(SeqCst));
        assert!(
            !got.contains(LinuxSignal::SIGKILL),
            "it could eat its own kill"
        );
        assert!(
            !got.contains(LinuxSignal::SIGSTOP),
            "it could eat its own stop"
        );
        assert!(
            got.contains(LinuxSignal::SIGINT),
            "and lost what it asked for"
        );
    }

    /// The set has two doors -- a new fd and a replacement on an existing
    /// one -- so the filter lives where both arrive rather than at the
    /// syscall.
    #[test]
    fn replacing_the_mask_goes_through_the_same_filter() {
        let fd = sfd(mask_of(&[LinuxSignal::SIGINT]), OpenFlags::NON_BLOCK);
        fd.set_mask(u64::MAX);
        let got = Sigset::new(fd.mask.load(SeqCst));
        assert!(!got.contains(LinuxSignal::SIGKILL));
        assert!(!got.contains(LinuxSignal::SIGSTOP));
        // Everything else a `sigfillset` asked for is still there.
        assert!(got.contains(LinuxSignal::SIGTERM));
        assert!(got.contains(LinuxSignal::SIGTSTP));
    }

    /// The filter takes those two and nothing else: a signalfd that quietly
    /// lost SIGTSTP would be a shell that cannot see Ctrl-Z.
    #[test]
    fn the_filter_takes_exactly_two_signals() {
        let fd = sfd(u64::MAX, OpenFlags::NON_BLOCK);
        let want = Sigset::new(u64::MAX).blockable().val();
        assert_eq!(fd.mask.load(SeqCst), want);
        assert_eq!(
            (u64::MAX ^ fd.mask.load(SeqCst)).count_ones(),
            2,
            "exactly SIGKILL and SIGSTOP"
        );
    }

    #[test]
    fn the_accepted_mask_is_what_it_was_created_with_and_can_be_replaced() {
        // Neither of these is filtered, so the stored set is the whole ask.
        let wanted = mask_of(&[LinuxSignal::SIGINT, LinuxSignal::SIGTERM]);
        let fd = sfd(wanted, OpenFlags::NON_BLOCK);
        assert_eq!(fd.mask.load(SeqCst), wanted);
        // `signalfd4` on an existing fd replaces the set rather than making a
        // new fd, so an event loop that narrows what it accepts keeps the same
        // descriptor registered with epoll.
        let narrowed = mask_of(&[LinuxSignal::SIGCHLD]);
        fd.set_mask(narrowed);
        assert_eq!(fd.mask.load(SeqCst), narrowed);
    }

    #[test]
    fn a_dup_shares_the_mask_so_updating_one_updates_both() {
        let fd = sfd(mask_of(&[LinuxSignal::SIGINT]), OpenFlags::NON_BLOCK);
        let dup = fd.clone();
        let wider = mask_of(&[LinuxSignal::SIGINT, LinuxSignal::SIGTERM]);
        fd.set_mask(wider);
        assert_eq!(dup.mask.load(SeqCst), wider);
        assert_eq!(dup.flags(), fd.flags());
    }

    #[test]
    fn a_read_must_have_room_for_a_whole_siginfo() {
        // `struct signalfd_siginfo` is 128 bytes and glibc reads exactly that
        // much; a short read has to be refused rather than half-filled.
        assert_eq!(SIGINFO_SIZE, 128);
        let fd = sfd(mask_of(&[LinuxSignal::SIGINT]), OpenFlags::NON_BLOCK);
        let mut short = [0u8; 127];
        assert_eq!(block_on(fd.read(&mut short)), Err(LxError::EINVAL));
    }

    #[test]
    fn a_signalfd_is_read_only_and_quiet_when_nothing_is_pending() {
        let fd = sfd(mask_of(&[LinuxSignal::SIGINT]), OpenFlags::NON_BLOCK);
        assert_eq!(fd.write(&[0u8; SIGINFO_SIZE]), Err(LxError::EINVAL));
        let s = fd.poll(PollEvents::IN | PollEvents::OUT).unwrap();
        // No error and no hangup: an event loop seeing either would drop the
        // fd and stop handling Ctrl-C for the rest of the session.
        assert!(!s.read && !s.write && !s.error && !s.hangup);
        let mut buf = [0u8; SIGINFO_SIZE];
        assert_eq!(block_on(fd.read(&mut buf)), Err(LxError::EAGAIN));
        // `async_poll` answers from the same state instead of parking, which
        // is what lets epoll fall back to its own re-poll tick.
        let s = block_on(fd.async_poll(PollEvents::IN)).unwrap();
        assert!(!s.read);
    }

    #[test]
    fn an_empty_mask_accepts_nothing() {
        let fd = sfd(0, OpenFlags::NON_BLOCK);
        assert!(fd.pending_matched().is_empty());
        assert!(!fd.poll(PollEvents::IN).unwrap().read);
        assert!(fd.consume_one().is_none());
    }
}

#[cfg(test)]
mod record_tests {
    //! What a `read` on a signalfd hands over besides the number. It was
    //! `ssi_signo` and 124 zero bytes, so a compositor reaping its children
    //! through a signalfd read `ssi_pid == 0` for every `SIGCHLD`.

    use super::*;
    use crate::process::wait_status_exited;
    use crate::signal::SignalCode;

    fn word(b: &[u8], at: usize) -> i32 {
        i32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn a_sigchld_read_off_a_signalfd_names_the_child_its_uid_and_its_status() {
        let info = SigInfo::child_state_change(4242, 1000, wait_status_exited(3));
        let r = signalfd_record(&info);
        assert_eq!(word(&r, 0), LinuxSignal::SIGCHLD as i32, "ssi_signo");
        assert_eq!(word(&r, 8), SignalCode::CLD_EXITED.0, "ssi_code");
        assert_eq!(word(&r, 12), 4242, "ssi_pid");
        assert_eq!(word(&r, 16), 1000, "ssi_uid");
        assert_eq!(word(&r, 40), 3, "ssi_status");
    }

    #[test]
    fn a_kill_read_off_a_signalfd_names_its_sender() {
        let info = SigInfo::from_user(LinuxSignal::SIGTERM, 77, 1000, SignalCode::USER);
        let r = signalfd_record(&info);
        assert_eq!(word(&r, 0), LinuxSignal::SIGTERM as i32);
        assert_eq!(word(&r, 8), SignalCode::USER.0);
        assert_eq!((word(&r, 12), word(&r, 16)), (77, 1000));
        assert_eq!(word(&r, 40), 0, "no status on a kill");
    }

    #[test]
    fn a_bare_signal_is_its_number_and_nothing_else() {
        let r = signalfd_record(&SigInfo::bare(LinuxSignal::SIGINT));
        assert_eq!(word(&r, 0), LinuxSignal::SIGINT as i32);
        assert!(r[4..].iter().all(|&b| b == 0), "{:?}", &r[4..48]);
    }
}

#[cfg(test)]
mod blocking_read_tests {
    //! A blocking `read` slept 20 ms at a time and looked only at the
    //! calling thread; a program whose other threads all blocked the signal
    //! never had the read return, because the signal went to the first
    //! thread of the process.
    extern crate std;

    use super::*;
    use crate::process::ProcessExt;
    use crate::process::{send_signal_to_process, LinuxProcess};
    use crate::signal::{SignalAction, SignalActionFlags};
    use core::time::Duration;
    use rcore_fs_ramfs::RamFS;
    use std::sync::mpsc;
    use zircon_object::object::KoID;
    use zircon_object::task::{Process, ROOT_JOB};

    fn bit(signal: LinuxSignal) -> u64 {
        1 << (signal as u64 - 1)
    }

    /// A process of two threads, both blocking SIGUSR1; the second one is
    /// the reader.
    fn program(pid: KoID) -> (Arc<Process>, Arc<Thread>, Arc<Thread>) {
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "sfd",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let main = Thread::create_linux(&proc).unwrap();
        let reader = Thread::create_linux(&proc).unwrap();
        main.lock_linux()
            .set_signal_mask(Sigset::new(bit(LinuxSignal::SIGUSR1)));
        reader
            .lock_linux()
            .set_signal_mask(Sigset::new(bit(LinuxSignal::SIGUSR1)));
        (proc, main, reader)
    }

    fn later(pid: KoID, signal: LinuxSignal, after: Duration) {
        std::thread::spawn(move || {
            std::thread::sleep(after);
            let _ = send_signal_to_process(pid as usize, signal);
        });
    }

    /// `read` on `fd` as `thread`, on a host thread of its own so a read
    /// that never returns fails the test instead of hanging it.
    fn read_as(
        fd: Arc<SignalFd>,
        thread: Arc<Thread>,
    ) -> (LxResult<usize>, [u8; SIGINFO_SIZE], Duration) {
        let (tx, rx) = mpsc::channel();
        let start = std::time::Instant::now();
        std::thread::spawn(move || {
            let mut buf = [0u8; SIGINFO_SIZE];
            let r = async_std::task::block_on(async {
                kernel_hal::thread::set_current_thread(Some(thread.clone()));
                fd.read(&mut buf).await
            });
            let _ = tx.send((r, buf));
        });
        let (r, buf) = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the read never returned");
        (r, buf, start.elapsed())
    }

    #[test]
    fn a_blocking_read_returns_the_signal_the_moment_it_arrives() {
        let (proc, _main, reader) = program(43_801);
        let fd = SignalFd::new(bit(LinuxSignal::SIGUSR1), OpenFlags::empty());
        later(proc.id(), LinuxSignal::SIGUSR1, Duration::from_millis(50));
        let (r, buf, took) = read_as(fd, reader.clone());
        assert_eq!(r, Ok(SIGINFO_SIZE));
        let signo = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(signo, LinuxSignal::SIGUSR1 as u32);
        assert!(took < Duration::from_secs(1), "returned after {:?}", took);
        assert_eq!(
            reader.lock_linux().sigwait.val(),
            0,
            "the wait set must be cleared once the read returns"
        );
    }

    #[test]
    fn the_thread_reading_the_fd_is_the_one_a_process_signal_reaches() {
        // Every thread blocks SIGUSR1 (as signalfd asks); the signal must go
        // to the reader, not to the first thread of the process.
        let (proc, main, reader) = program(43_802);
        let fd = SignalFd::new(bit(LinuxSignal::SIGUSR1), OpenFlags::empty());
        later(proc.id(), LinuxSignal::SIGUSR1, Duration::from_millis(50));
        let (r, _buf, _took) = read_as(fd, reader);
        assert_eq!(r, Ok(SIGINFO_SIZE), "the signal went elsewhere");
        assert!(
            !main.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
            "the signal was queued to the first thread"
        );
    }

    #[test]
    fn a_caught_signal_outside_the_mask_interrupts_the_read() {
        let (proc, main, reader) = program(43_803);
        // Aimed at the reader: the main thread blocks SIGUSR2 as well.
        let mut both = Sigset::new(bit(LinuxSignal::SIGUSR1));
        both.insert(LinuxSignal::SIGUSR2);
        main.lock_linux().set_signal_mask(both);
        proc.linux().set_signal_action(
            LinuxSignal::SIGUSR2,
            SignalAction {
                handler: 0x1000,
                flags: SignalActionFlags::empty(),
                restorer: 0,
                mask: Sigset::default(),
            },
        );
        let fd = SignalFd::new(bit(LinuxSignal::SIGUSR1), OpenFlags::empty());
        later(proc.id(), LinuxSignal::SIGUSR2, Duration::from_millis(50));
        let (r, _buf, took) = read_as(fd, reader.clone());
        assert_eq!(r, Err(LxError::EINTR));
        assert!(
            took < Duration::from_secs(1),
            "interrupted after {:?}",
            took
        );
        assert_eq!(reader.lock_linux().sigwait.val(), 0);
    }
}
