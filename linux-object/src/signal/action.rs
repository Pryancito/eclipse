use crate::signal::Signal;
use _core::convert::TryFrom;
use bitflags::*;

pub const SIG_ERR: usize = usize::MAX - 1;
pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;

/// Linux struct sigset_t
///
/// yet there's a bug because of mismatching bits: <https://sourceware.org/bugzilla/show_bug.cgi?id=25657>
/// just support 64bits size sigset
#[derive(Default, Clone, Copy, Debug)]
#[repr(C)]
pub struct Sigset(u64);

impl Sigset {
    pub fn new(val: u64) -> Self {
        Sigset(val)
    }
    pub fn empty() -> Self {
        Sigset(0)
    }
    pub fn val(&self) -> u64 {
        self.0
    }
    pub fn contains(&self, sig: Signal) -> bool {
        (self.0 & sig.as_bit()) != 0
    }
    pub fn insert(&mut self, sig: Signal) {
        self.0 |= sig.as_bit()
    }
    pub fn insert_set(&mut self, sigset: &Sigset) {
        self.0 |= sigset.0;
    }
    pub fn remove(&mut self, sig: Signal) {
        self.0 ^= self.0 & sig.as_bit();
    }
    pub fn remove_set(&mut self, sigset: &Sigset) {
        self.0 ^= self.0 & sigset.0;
    }
    pub fn mask_with(&self, mask: &Sigset) -> Sigset {
        Sigset(self.0 & (!mask.0))
    }
    pub fn find_first_signal(&self) -> Option<Signal> {
        let tz = self.0.trailing_zeros() as u8;
        if tz >= 64 {
            None
        } else {
            Some(Signal::try_from(tz + 1).unwrap())
        }
    }
    /// The part of this set a thread is allowed to block.
    ///
    /// `sigprocmask(2)`: "It is not possible to block SIGKILL or SIGSTOP.
    /// Attempts to do so are silently ignored." Linux enforces it by clearing
    /// those two bits from every mask that arrives from userspace, in
    /// `sigprocmask`, in `sigsuspend`, in `ppoll`/`pselect`'s temporary mask
    /// and in `sigreturn`'s restored one -- four doors into the same field,
    /// and a process that gets through any of them can no longer be stopped.
    ///
    /// Two of the four checked it here and two did not, which is why this is a
    /// method and not another pair of `remove` calls.
    pub fn blockable(&self) -> Sigset {
        let mut out = *self;
        out.remove(Signal::SIGKILL);
        out.remove(Signal::SIGSTOP);
        out
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }
    pub fn is_not_empty(&self) -> bool {
        self.0 != 0
    }
}

/// Linux struct sigaction
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SignalAction {
    pub handler: usize, // this field may be an union
    pub flags: SignalActionFlags,
    pub restorer: usize,
    pub mask: Sigset,
}

impl SignalAction {
    /// What `do_sigaction` actually stores, which is not quite what userspace
    /// handed over: `sa_mask` loses the two signals nothing may block.
    ///
    /// This is the FIFTH door into that rule -- [`Sigset::blockable`] names
    /// the other four -- and the one with the longest reach, because a
    /// `sa_mask` is not a mask for one call: it becomes the thread's blocked
    /// set every single time the handler runs. It is also read back:
    /// `sigaction(sig, NULL, &old)` is how a program learns what it actually
    /// got, so the filtering has to happen on the way IN, not on each use.
    pub fn stored(mut self) -> Self {
        self.mask = self.mask.blockable();
        self
    }

    /// The set of signals blocked while this action's handler runs
    /// (`signal_delivered`, `kernel/signal.c`): what the thread already had
    /// blocked, plus the `sa_mask` the action asked for, plus the signal
    /// itself unless `SA_NODEFER` said not to.
    ///
    /// `sa_mask` is the whole reason a handler can touch data the signal also
    /// touches: it names the signals that must be held off for the duration.
    /// It arrived from userspace, was stored, and was read by nobody.
    pub fn handler_mask(&self, blocked: Sigset, signal: Signal) -> Sigset {
        let mut out = blocked;
        out.insert_set(&self.mask);
        if !self.flags.contains(SignalActionFlags::NODEFER) {
            out.insert(signal);
        }
        out.blockable()
    }

    /// Whether arranging this delivery puts the disposition back to `SIG_DFL`
    /// first.
    ///
    /// `SA_RESETHAND` (`SA_ONESHOT`) is how a one-shot handler is asked for,
    /// and the reset happens BEFORE the handler runs, not after it returns --
    /// so a handler that wants to stay installed re-installs itself, and one
    /// that does not is gone by the time a second signal arrives. Unread, the
    /// handler stayed installed for good.
    pub fn resets_to_default(&self) -> bool {
        self.flags.contains(SignalActionFlags::RESETHAND)
    }
}

#[repr(C)]
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct SiginfoFields {
    pad: [u8; Self::PAD_SIZE],
    // TODO: fill this union
}

impl SiginfoFields {
    const PAD_SIZE: usize = 128 - 2 * core::mem::size_of::<i32>() - core::mem::size_of::<usize>();
}

impl Default for SiginfoFields {
    fn default() -> Self {
        SiginfoFields {
            pad: [0; Self::PAD_SIZE],
        }
    }
}

impl SiginfoFields {
    fn write_sigchld(&mut self, pid: i32, status: i32) {
        #[repr(C)]
        struct Fields {
            pid: i32,
            uid: u32,
            status: i32,
        }
        let fields = Fields {
            pid,
            uid: 0,
            status,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &fields as *const Fields as *const u8,
                core::mem::size_of::<Fields>(),
            )
        };
        self.pad[..bytes.len()].copy_from_slice(bytes);
    }
}

#[repr(C)]
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct SigInfo {
    pub signo: i32,
    pub errno: i32,
    pub code: SignalCode,
    pub field: SiginfoFields,
}

impl Default for SigInfo {
    fn default() -> Self {
        Self {
            signo: 0,
            errno: 0,
            code: SignalCode::USER,
            field: Default::default(),
        }
    }
}

impl SigInfo {
    /// `siginfo_t` for a child state change, from the `wait` status word.
    ///
    /// `waitid(2)` promises `si_code` and `si_status` as two SEPARATE
    /// answers -- WHAT happened and the number that goes with it -- which is
    /// the whole reason it exists next to `wait4`. Both used to be built as
    /// "exited with `status >> 8`", so every child was `CLD_EXITED` however
    /// it had finished, and the number was the second byte of a word that,
    /// for a killed child, does not keep anything there.
    pub fn child_state_change(pid: i32, status: i32) -> Self {
        let (code, si_status) = child_si_code_and_status(status);
        let mut info = SigInfo {
            signo: Signal::SIGCHLD as i32,
            errno: 0,
            code,
            ..Self::default()
        };
        info.field.write_sigchld(pid, si_status);
        info
    }
}

/// Take a `wait(2)` status word apart into the two things `waitid(2)`
/// reports separately: `si_code`, which says HOW the child's state changed,
/// and `si_status`, the number that goes with THAT answer.
///
/// The four shapes are read apart exactly as `sys/wait.h` reads them, and in
/// its order. `0xffff` is the continued marker and its low SEVEN bits are
/// `0x7f` -- which is where `WIFSIGNALED` looks -- so a continue asked about
/// last would come back as a child killed by signal 127.
pub fn child_si_code_and_status(status: i32) -> (SignalCode, i32) {
    const CONTINUED: i32 = 0xffff;
    if status == CONTINUED {
        // `WIFCONTINUED` carries no number of its own; Linux reports the
        // signal that did it, which is the only one that can.
        return (SignalCode::CLD_CONTINUED, Signal::SIGCONT as i32);
    }
    if status & 0xff == 0x7f {
        // `WIFSTOPPED` / `WSTOPSIG`.
        return (SignalCode::CLD_STOPPED, (status >> 8) & 0xff);
    }
    match status & 0x7f {
        // `WIFEXITED` / `WEXITSTATUS`.
        0 => (SignalCode::CLD_EXITED, (status >> 8) & 0xff),
        // `WIFSIGNALED` / `WTERMSIG`. The status word keeps NOTHING in its
        // second byte here, which is why reporting `status >> 8` for every
        // child made a killed one look like `exit(0)` -- a success.
        sig => (SignalCode::CLD_KILLED, sig),
    }
}

/// A code identifying the cause of the signal.
#[repr(i32)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SignalCode {
    ASYNCNL = -60,
    TKILL = -6,
    SIGIO = -5,
    ASYNCIO = -4,
    MESGQ = -3,
    TIMER = -2,
    QUEUE = -1,
    /// from user
    USER = 0,
    /// `SIGCHLD`: child called `_exit`
    #[allow(non_camel_case_types)]
    CLD_EXITED = 1,
    /// `SIGCHLD`: child killed by a signal.
    #[allow(non_camel_case_types)]
    CLD_KILLED = 2,
    /// `SIGCHLD`: child killed by a signal AND dumped core. Never produced
    /// here -- this kernel writes no cores -- but named so the numbering is
    /// the uAPI's and not a count of what happens to be implemented.
    #[allow(non_camel_case_types)]
    CLD_DUMPED = 3,
    /// `SIGCHLD`: traced child has trapped.
    #[allow(non_camel_case_types)]
    CLD_TRAPPED = 4,
    /// `SIGCHLD`: child has stopped.
    #[allow(non_camel_case_types)]
    CLD_STOPPED = 5,
    /// `SIGCHLD`: stopped child has continued.
    #[allow(non_camel_case_types)]
    CLD_CONTINUED = 6,
    /// from kernel
    KERNEL = 128,
}

bitflags! {
    #[derive(Default)]
    pub struct SignalActionFlags : usize {
        const NOCLDSTOP = 1;
        const NOCLDWAIT = 2;
        const SIGINFO = 4;
        const ONSTACK = 0x08000000;
        const RESTART = 0x10000000;
        const NODEFER = 0x40000000;
        const RESETHAND = 0x80000000;
        const RESTORER = 0x04000000;
    }
}

#[cfg(test)]
mod sigset_tests {
    //! `Sigset` is the bitmap every signal in the system passes through: the
    //! pending set of a thread, the blocked mask, the mask a `sigaction`
    //! installs, and the set `signalfd` accepts are all this type. Its bit
    //! numbering is the one userspace uses, so an off-by-one here does not
    //! fail loudly -- it delivers the wrong signal.

    use super::*;
    use crate::signal::Signal;

    #[test]
    fn a_signal_owns_the_bit_one_below_its_number() {
        // `sigset_t` bit 0 is signal 1. Userspace builds masks with
        // `sigaddset`, which does `1 << (sig - 1)`, and hands the raw word to
        // the kernel: get this wrong by one and `sigprocmask({SIGINT})` blocks
        // SIGQUIT instead, which is the kind of bug that looks like a flaky
        // Ctrl-C.
        assert_eq!(Signal::SIGHUP.as_bit(), 1 << 0);
        assert_eq!(Signal::SIGINT.as_bit(), 1 << 1);
        assert_eq!(Signal::SIGKILL.as_bit(), 1 << 8);
        assert_eq!(Signal::SIGSTOP.as_bit(), 1 << 18);
        assert_eq!(Signal::SIGRT64.as_bit(), 1 << 63);
    }

    #[test]
    fn insert_contains_and_remove_agree_for_every_signal() {
        for n in 1..=64u8 {
            let sig = Signal::try_from(n).unwrap();
            let mut set = Sigset::empty();
            assert!(!set.contains(sig), "{:?} was in an empty set", sig);
            set.insert(sig);
            assert!(set.contains(sig), "{:?} did not go in", sig);
            // And nothing else went in with it.
            for m in 1..=64u8 {
                if m != n {
                    let other = Signal::try_from(m).unwrap();
                    assert!(
                        !set.contains(other),
                        "inserting {:?} also set {:?}",
                        sig,
                        other
                    );
                }
            }
            set.remove(sig);
            assert!(set.is_empty(), "removing {:?} left something behind", sig);
        }
    }

    #[test]
    fn removing_a_signal_that_is_not_there_changes_nothing() {
        // `remove` is `^= self & bit`, which is only equal to a clear because
        // of that `& self`. Written as a plain `^=` it would SET the bit of a
        // signal that was not pending -- inventing a signal out of an attempt
        // to drop one.
        let mut set = Sigset::empty();
        set.insert(Signal::SIGTERM);
        set.remove(Signal::SIGINT);
        assert!(set.contains(Signal::SIGTERM));
        assert!(!set.contains(Signal::SIGINT));
        assert_eq!(set.val(), Signal::SIGTERM.as_bit());
    }

    #[test]
    fn the_first_signal_is_the_lowest_numbered_one() {
        // Delivery order. Linux hands out the lowest-numbered pending signal
        // first, and the code leans on it: SIGKILL (9) must win over a pending
        // SIGWINCH (28) rather than queue behind it.
        let mut set = Sigset::empty();
        set.insert(Signal::SIGWINCH);
        set.insert(Signal::SIGKILL);
        set.insert(Signal::SIGTERM);
        assert_eq!(set.find_first_signal(), Some(Signal::SIGKILL));
        set.remove(Signal::SIGKILL);
        assert_eq!(set.find_first_signal(), Some(Signal::SIGTERM));
        set.remove(Signal::SIGTERM);
        assert_eq!(set.find_first_signal(), Some(Signal::SIGWINCH));
    }

    #[test]
    fn an_empty_set_has_no_first_signal() {
        // `trailing_zeros()` of zero is 64, one past the last signal. Reading
        // that as a signal number would hand out `Signal::try_from(65)`, which
        // panics -- a kernel panic on the ordinary "nothing is pending" path.
        assert_eq!(Sigset::empty().find_first_signal(), None);
        assert!(Sigset::empty().is_empty());
        assert!(!Sigset::empty().is_not_empty());
    }

    #[test]
    fn the_highest_signal_is_still_found() {
        let mut set = Sigset::empty();
        set.insert(Signal::SIGRT64);
        assert_eq!(set.find_first_signal(), Some(Signal::SIGRT64));
        assert!(set.is_not_empty());
    }

    #[test]
    fn masking_leaves_what_is_deliverable() {
        // `mask_with` is "pending AND NOT blocked" -- the whole of delivery
        // gating. Inverted, every blocked signal is delivered and every
        // unblocked one is dropped.
        let mut pending = Sigset::empty();
        pending.insert(Signal::SIGINT);
        pending.insert(Signal::SIGTERM);
        let mut blocked = Sigset::empty();
        blocked.insert(Signal::SIGINT);

        let deliverable = pending.mask_with(&blocked);
        assert!(
            deliverable.contains(Signal::SIGTERM),
            "SIGTERM was not blocked"
        );
        assert!(!deliverable.contains(Signal::SIGINT), "SIGINT was blocked");
        // And the pending set itself is untouched: a blocked signal stays
        // pending until it is unblocked.
        assert!(pending.contains(Signal::SIGINT));
    }

    #[test]
    fn set_operations_are_unions_and_differences() {
        let mut a = Sigset::empty();
        a.insert(Signal::SIGINT);
        let mut b = Sigset::empty();
        b.insert(Signal::SIGTERM);
        b.insert(Signal::SIGINT);

        let mut union = a;
        union.insert_set(&b);
        assert!(union.contains(Signal::SIGINT) && union.contains(Signal::SIGTERM));

        let mut diff = b;
        diff.remove_set(&a);
        assert!(!diff.contains(Signal::SIGINT), "the common signal stayed");
        assert!(diff.contains(Signal::SIGTERM), "the uncommon one went too");
    }

    #[test]
    fn removing_a_set_cannot_add_what_was_not_there() {
        // `remove_set` is `^= self & other`, and the `& self` is what makes it
        // a difference rather than a symmetric one. A plain `^= other` gives
        // the same answer whenever `other` is a subset -- which is the usual
        // case, and why this needs a set that is not. `sigprocmask(SIG_UNBLOCK,
        // {SIGINT, SIGTERM})` on a thread blocking only SIGINT is exactly
        // that shape, and the plain version would come back BLOCKING SIGTERM.
        let mut blocked = Sigset::empty();
        blocked.insert(Signal::SIGINT);
        let mut unblocking = Sigset::empty();
        unblocking.insert(Signal::SIGINT);
        unblocking.insert(Signal::SIGTERM);

        blocked.remove_set(&unblocking);
        assert!(!blocked.contains(Signal::SIGINT), "the blocked one stayed");
        assert!(
            !blocked.contains(Signal::SIGTERM),
            "unblocking a signal that was not blocked blocked it"
        );
        assert!(blocked.is_empty());
    }

    #[test]
    fn sigkill_and_sigstop_cannot_be_blocked() {
        // `sigprocmask(2)`: attempts to block these two are silently ignored.
        // Every other signal in the set must survive the filter -- a version
        // that cleared too much would quietly unblock signals the process
        // deliberately masked.
        let mut wanted = Sigset::empty();
        for sig in [
            Signal::SIGKILL,
            Signal::SIGSTOP,
            Signal::SIGINT,
            Signal::SIGTERM,
            Signal::SIGCHLD,
            Signal::SIGRT64,
        ] {
            wanted.insert(sig);
        }
        let allowed = wanted.blockable();
        assert!(
            !allowed.contains(Signal::SIGKILL),
            "SIGKILL became blockable"
        );
        assert!(
            !allowed.contains(Signal::SIGSTOP),
            "SIGSTOP became blockable"
        );
        for sig in [
            Signal::SIGINT,
            Signal::SIGTERM,
            Signal::SIGCHLD,
            Signal::SIGRT64,
        ] {
            assert!(allowed.contains(sig), "{:?} was dropped by the filter", sig);
        }
        // Filtering a set that never had them is the identity.
        let mut plain = Sigset::empty();
        plain.insert(Signal::SIGUSR1);
        assert_eq!(plain.blockable().val(), plain.val());
    }

    #[test]
    fn the_real_time_signals_start_at_thirty_two() {
        // `is_standard` decides queueing behaviour, so the boundary matters.
        assert!(
            Signal::SIGSYS.is_standard(),
            "31 is the last standard signal"
        );
        assert!(
            !Signal::SIGRT32.is_standard(),
            "32 is the first real-time one"
        );
        assert_eq!(Signal::RTMIN, 32);
        assert_eq!(Signal::RTMAX, 64);
    }

    #[test]
    fn every_number_from_one_to_sixty_four_is_a_signal_and_no_others_are() {
        // `Signal::try_from` guards every signal number that arrives from
        // userspace (`kill`, `sigaction`, `tkill`). A gap in the enum would
        // make a valid signal number an EINVAL; an extra one would let a
        // number through that `find_first_signal` could never produce.
        for n in 1..=64u8 {
            let sig = Signal::try_from(n).unwrap_or_else(|_| panic!("{} is not a signal", n));
            assert_eq!(sig as u8, n, "{} round-tripped to something else", n);
        }
        assert!(Signal::try_from(0u8).is_err(), "0 is not a signal");
        assert!(
            Signal::try_from(65u8).is_err(),
            "65 is past the last signal"
        );
    }

    #[test]
    fn a_full_set_round_trips_through_its_raw_word() {
        // The raw `u64` is what crosses to userspace and what `signalfd`
        // stores, so `new`/`val` must be exact inverses.
        let raw = 0xDEAD_BEEF_1234_5678u64;
        assert_eq!(Sigset::new(raw).val(), raw);
        let mut built = Sigset::empty();
        for n in 1..=64u8 {
            built.insert(Signal::try_from(n).unwrap());
        }
        assert_eq!(
            built.val(),
            u64::MAX,
            "the 64 signals fill the word exactly"
        );
    }
}

#[cfg(test)]
mod sigaction_tests {
    //! What `sigaction(2)` stores, and what a handler runs under.
    //!
    //! `sa_mask` and the `SA_*` flags come straight from userspace and decide
    //! control flow: which signals a handler can be interrupted by, and
    //! whether it is still installed when the next one arrives. Every one of
    //! them was stored and read by nobody.

    use super::*;
    use crate::signal::Signal;

    fn act(flags: SignalActionFlags, mask: Sigset) -> SignalAction {
        SignalAction {
            handler: 0x1000,
            flags,
            restorer: 0x2000,
            mask,
        }
    }

    fn set(sigs: &[Signal]) -> Sigset {
        let mut s = Sigset::empty();
        for sig in sigs {
            s.insert(*sig);
        }
        s
    }

    /// The numbers are UABI: userspace ORs them into `sa_flags` itself, and a
    /// constant with the wrong value here is not a compile error anywhere --
    /// it is a flag that quietly means another one.
    #[test]
    fn the_action_flags_carry_the_numbers_userspace_sends() {
        for (flag, value) in [
            (SignalActionFlags::NOCLDSTOP, 0x0000_0001),
            (SignalActionFlags::NOCLDWAIT, 0x0000_0002),
            (SignalActionFlags::SIGINFO, 0x0000_0004),
            (SignalActionFlags::RESTORER, 0x0400_0000),
            (SignalActionFlags::ONSTACK, 0x0800_0000),
            (SignalActionFlags::RESTART, 0x1000_0000),
            (SignalActionFlags::NODEFER, 0x4000_0000),
            (SignalActionFlags::RESETHAND, 0x8000_0000),
        ] {
            assert_eq!(flag.bits(), value, "{flag:?}");
        }
    }

    /// The fifth door into [`Sigset::blockable`]. A `sa_mask` is not a mask
    /// for one call: it becomes a thread's blocked set on every delivery, so
    /// an unfiltered one holds off SIGKILL for as long as the handler runs.
    #[test]
    fn a_stored_sa_mask_cannot_hold_the_two_unblockable_signals() {
        let asked = set(&[Signal::SIGKILL, Signal::SIGSTOP, Signal::SIGTERM]);
        let stored = act(SignalActionFlags::empty(), asked).stored();
        assert!(!stored.mask.contains(Signal::SIGKILL));
        assert!(!stored.mask.contains(Signal::SIGSTOP));
        // And nothing else is lost on the way in.
        assert!(stored.mask.contains(Signal::SIGTERM));
    }

    /// Storing changes the mask and nothing else about the action.
    #[test]
    fn storing_an_action_leaves_the_rest_of_it_alone() {
        let a = act(SignalActionFlags::SIGINFO, set(&[Signal::SIGUSR1]));
        let stored = a.stored();
        assert_eq!(stored.handler, a.handler);
        assert_eq!(stored.restorer, a.restorer);
        assert_eq!(stored.flags, a.flags);
        assert_eq!(stored.mask.val(), a.mask.val());
    }

    #[test]
    fn a_handler_runs_with_what_was_blocked_plus_what_it_asked_for() {
        let blocked = set(&[Signal::SIGHUP]);
        let a = act(SignalActionFlags::empty(), set(&[Signal::SIGTERM]));
        let during = a.handler_mask(blocked, Signal::SIGALRM);
        // What was already blocked stays blocked.
        assert!(during.contains(Signal::SIGHUP));
        // What the action asked for is added.
        assert!(during.contains(Signal::SIGTERM));
        // And nothing else is.
        assert!(!during.contains(Signal::SIGUSR1));
    }

    /// The default is that a signal does not interrupt its own handler; that
    /// is exactly what `SA_NODEFER` turns off.
    #[test]
    fn a_handler_blocks_its_own_signal_unless_nodefer_says_otherwise() {
        let plain = act(SignalActionFlags::empty(), Sigset::empty());
        assert!(plain
            .handler_mask(Sigset::empty(), Signal::SIGALRM)
            .contains(Signal::SIGALRM));

        let nodefer = act(SignalActionFlags::NODEFER, Sigset::empty());
        assert!(!nodefer
            .handler_mask(Sigset::empty(), Signal::SIGALRM)
            .contains(Signal::SIGALRM));
    }

    /// `SA_NODEFER` unblocks only the signal being delivered; it does not
    /// throw away `sa_mask` or what was already blocked.
    #[test]
    fn nodefer_drops_one_signal_and_not_the_rest_of_the_set() {
        let a = act(SignalActionFlags::NODEFER, set(&[Signal::SIGTERM]));
        let during = a.handler_mask(set(&[Signal::SIGHUP]), Signal::SIGALRM);
        assert!(during.contains(Signal::SIGHUP));
        assert!(during.contains(Signal::SIGTERM));
        assert!(!during.contains(Signal::SIGALRM));
    }

    /// Belt and braces: even reached with a mask that was never stored, the
    /// set a handler runs under is still one a thread may hold.
    #[test]
    fn the_mask_a_handler_runs_under_is_still_one_a_thread_may_hold() {
        let a = act(
            SignalActionFlags::empty(),
            set(&[Signal::SIGKILL, Signal::SIGSTOP]),
        );
        let during = a.handler_mask(Sigset::empty(), Signal::SIGSTOP);
        assert!(!during.contains(Signal::SIGKILL));
        assert!(!during.contains(Signal::SIGSTOP));
    }

    #[test]
    fn resethand_is_the_flag_that_makes_a_handler_one_shot() {
        assert!(act(SignalActionFlags::RESETHAND, Sigset::empty()).resets_to_default());
        // And it is that one flag, not any of the others.
        for flags in [
            SignalActionFlags::empty(),
            SignalActionFlags::NODEFER,
            SignalActionFlags::RESTART,
            SignalActionFlags::SIGINFO | SignalActionFlags::ONSTACK,
        ] {
            assert!(
                !act(flags, Sigset::empty()).resets_to_default(),
                "{:?}",
                flags
            );
        }
    }

    /// The two flags are independent: a one-shot handler may also ask not to
    /// have its own signal blocked.
    #[test]
    fn a_one_shot_handler_may_also_ask_for_nodefer() {
        let a = act(
            SignalActionFlags::RESETHAND | SignalActionFlags::NODEFER,
            Sigset::empty(),
        );
        assert!(a.resets_to_default());
        assert!(!a
            .handler_mask(Sigset::empty(), Signal::SIGSEGV)
            .contains(Signal::SIGSEGV));
    }
}

#[cfg(test)]
mod child_siginfo_tests {
    //! What `waitid(2)` puts in `siginfo_t`.
    //!
    //! `wait4` hands back one packed int and leaves userspace to take it
    //! apart with the `sys/wait.h` macros. `waitid` exists because it does
    //! that work in the kernel and answers the two questions separately:
    //! `si_code` says WHICH of the four things happened, and `si_status`
    //! carries the number that belongs to that answer. This kernel built
    //! both as "exited, with `status >> 8`", so every child came back as
    //! `CLD_EXITED` -- and a killed child, whose status word keeps nothing
    //! in its second byte, came back as `CLD_EXITED` with status 0, which
    //! reads as a clean success.

    use super::*;
    use crate::signal::Signal;

    /// The status words as the kernel builds them, spelled here the way
    /// `sys/wait.h` spells them, so a change to either encoding has to
    /// disagree with this file to pass.
    fn exited(code: i32) -> i32 {
        (code & 0xff) << 8
    }
    fn killed_by(sig: Signal) -> i32 {
        sig as i32 & 0x7f
    }
    fn stopped_by(sig: Signal) -> i32 {
        ((sig as i32) << 8) | 0x7f
    }
    const CONTINUED: i32 = 0xffff;

    #[test]
    fn an_exit_reports_its_code() {
        for code in [0, 1, 42, 255] {
            assert_eq!(
                child_si_code_and_status(exited(code)),
                (SignalCode::CLD_EXITED, code)
            );
        }
    }

    #[test]
    fn a_death_by_signal_reports_the_signal_and_says_it_was_a_kill() {
        for sig in [
            Signal::SIGHUP,
            Signal::SIGINT,
            Signal::SIGKILL,
            Signal::SIGSEGV,
            Signal::SIGTERM,
        ] {
            assert_eq!(
                child_si_code_and_status(killed_by(sig)),
                (SignalCode::CLD_KILLED, sig as i32),
                "{:?}",
                sig
            );
        }
    }

    /// The shape that made a killed child unreadable: `status >> 8` of a
    /// death-by-signal word is zero, so `waitid` reported it as a child that
    /// exited successfully.
    #[test]
    fn a_killed_child_is_not_a_child_that_exited_with_zero() {
        let killed = child_si_code_and_status(killed_by(Signal::SIGKILL));
        let clean = child_si_code_and_status(exited(0));
        assert_ne!(killed, clean);
        assert_eq!(killed.0, SignalCode::CLD_KILLED);
        assert_ne!(killed.1, 0, "si_status must name the signal, not be empty");
    }

    #[test]
    fn a_stop_reports_the_signal_that_stopped_it() {
        for sig in [
            Signal::SIGSTOP,
            Signal::SIGTSTP,
            Signal::SIGTTIN,
            Signal::SIGTTOU,
        ] {
            assert_eq!(
                child_si_code_and_status(stopped_by(sig)),
                (SignalCode::CLD_STOPPED, sig as i32),
                "{:?}",
                sig
            );
        }
    }

    /// `0xffff` has `0x7f` in its low seven bits, which is exactly what
    /// `WIFSIGNALED` looks at, so the continue has to be read FIRST: asked
    /// about last, every resumed child comes back as one killed by signal
    /// 127, and a parent in `waitid(WCONTINUED)` is told the job it just
    /// resumed is dead.
    #[test]
    fn a_continue_is_read_before_the_shape_it_would_pass_for() {
        // Not a stop: `WIFSTOPPED` reads the whole low byte and finds 0xff.
        assert_ne!(CONTINUED & 0xff, 0x7f);
        // But `WIFSIGNALED` reads seven bits, and there it is a signal.
        assert_eq!(CONTINUED & 0x7f, 0x7f, "it really would pass for a kill");
        assert_eq!(
            child_si_code_and_status(CONTINUED),
            (SignalCode::CLD_CONTINUED, Signal::SIGCONT as i32)
        );
    }

    /// The numbers are the uAPI's (`include/uapi/asm-generic/siginfo.h`),
    /// not a count of what this kernel happens to produce: userspace
    /// compares against its own headers.
    #[test]
    fn the_codes_are_the_ones_userspace_has_in_its_headers() {
        assert_eq!(SignalCode::CLD_EXITED as i32, 1);
        assert_eq!(SignalCode::CLD_KILLED as i32, 2);
        assert_eq!(SignalCode::CLD_DUMPED as i32, 3);
        assert_eq!(SignalCode::CLD_TRAPPED as i32, 4);
        assert_eq!(SignalCode::CLD_STOPPED as i32, 5);
        assert_eq!(SignalCode::CLD_CONTINUED as i32, 6);
    }

    /// And `si_signo` is SIGCHLD whatever happened, because that is the
    /// signal this `siginfo_t` describes.
    #[test]
    fn every_child_state_change_is_a_sigchld() {
        for status in [
            exited(3),
            killed_by(Signal::SIGKILL),
            stopped_by(Signal::SIGTSTP),
            CONTINUED,
        ] {
            let info = SigInfo::child_state_change(4242, status);
            assert_eq!(info.signo, Signal::SIGCHLD as i32);
        }
    }
}
