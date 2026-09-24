//! `signalfd(2)` — accept signals through a readable file descriptor instead of
//! a handler. libwayland's `wl_event_loop_add_signal` blocks a signal and reads
//! it here, so a Wayland compositor (labwc) handles SIGINT/SIGTERM/SIGCHLD from
//! its event loop. Without it, the blocked signal sits pending forever and
//! Ctrl-C does nothing.

use super::*;
// `crate::signal::Signal` (the Linux signal enum) would shadow
// `zircon_object::object::Signal` (the KObject signal bits used by
// `impl_kobject!`), so alias it.
use crate::signal::{Signal as LinuxSignal, Sigset};
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

    /// The calling thread's pending signals that this fd accepts.
    fn pending_matched(&self) -> Sigset {
        let mask = self.mask.load(SeqCst);
        if let Some(arc) = kernel_hal::thread::get_current_thread() {
            if let Ok(thread) = arc.downcast::<Thread>() {
                let tl = thread.lock_linux();
                return Sigset::new(tl.signals.val() & mask);
            }
        }
        Sigset::empty()
    }

    /// Consume and return the lowest-numbered accepted pending signal, removing
    /// it from the calling thread's pending set.
    fn consume_one(&self) -> Option<LinuxSignal> {
        let mask = self.mask.load(SeqCst);
        let arc = kernel_hal::thread::get_current_thread()?;
        let thread = arc.downcast::<Thread>().ok()?;
        let mut tl = thread.lock_linux();
        let sig = Sigset::new(tl.signals.val() & mask).find_first_signal()?;
        tl.signals.remove(sig);
        Some(sig)
    }
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
        loop {
            if let Some(sig) = self.consume_one() {
                // struct signalfd_siginfo: ssi_signo is the first u32; the rest
                // (errno/code/pid/uid/…) we leave zero, which is all an event
                // loop reading Ctrl-C / SIGTERM looks at.
                buf[..SIGINFO_SIZE].fill(0);
                buf[..4].copy_from_slice(&(sig as u32).to_ne_bytes());
                return Ok(SIGINFO_SIZE);
            }
            if self.flags().non_block() {
                return Err(LxError::EAGAIN);
            }
            // Block until a matching signal is pending. Signals don't fire a
            // per-fd waker, so re-check on a short timer rather than spinning.
            // The realistic user (libwayland) polls via epoll and never reaches
            // this path; epoll's own re-poll tick bounds its latency.
            let deadline = kernel_hal::timer::timer_now() + core::time::Duration::from_millis(20);
            kernel_hal::thread::sleep_until(deadline).await;
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
