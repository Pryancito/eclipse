//! Linux signals
use crate::error::{LxError, LxResult};
use bitflags::*;
use numeric_enum_macro::numeric_enum;

mod action;
pub mod ucontext;

pub use action::*;
pub use ucontext::*;

#[repr(C)]
#[derive(Clone)]
pub struct SignalFrame {
    /// point to ret_code
    pub ret_code_addr: usize,
    /// Signal Frame info
    pub info: SigInfo,
    /// adapt interface, a little bit waste
    pub ucontext: SignalUserContext,
    /// call sys_sigreturn
    pub ret_code: [u8; 7],
}

bitflags! {
    pub struct SignalStackFlags : u32 {
        const ONSTACK = 1;
        const DISABLE = 2;
        const AUTODISARM = 0x80000000;
    }
}

/// `MINSIGSTKSZ` on the architectures this kernel runs.
pub const MIN_SIGSTACK_SIZE: usize = 2048;

/// The flags `sigaltstack(2)` accepts in `ss_flags`: `SS_AUTODISARM`, and
/// the modes `SS_DISABLE` and `SS_ONSTACK` (the latter for compatibility,
/// as `do_sigaltstack` takes it; it means "install").
pub const VALID_SIGSTACK_FLAGS: SignalStackFlags = SignalStackFlags::from_bits_truncate(
    SignalStackFlags::AUTODISARM.bits()
        | SignalStackFlags::DISABLE.bits()
        | SignalStackFlags::ONSTACK.bits(),
);

/// Linux struct stack_t
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SignalStack {
    pub sp: usize,
    pub flags: SignalStackFlags,
    pub size: usize,
}

impl Default for SignalStack {
    fn default() -> Self {
        // default to disabled
        SignalStack {
            sp: 0,
            flags: SignalStackFlags::DISABLE,
            size: 0,
        }
    }
}

impl SignalStack {
    /// Is `sp` a stack pointer inside this alternate signal stack?
    ///
    /// Linux's `on_sig_stack()`. Strict at the bottom and inclusive at the
    /// top, because a stack pointer sits one past the byte it last pushed: a
    /// thread that has just switched here has `sp == self.sp + self.size`.
    pub fn contains_sp(&self, sp: usize) -> bool {
        sp > self.sp && sp - self.sp <= self.size
    }

    /// Can a signal frame be moved onto this stack, for a thread currently
    /// using `sp`?
    ///
    /// Linux's `sas_ss_flags(sp) == 0`. Switching to the base of a stack we
    /// are already running on would overwrite the frames below us -- the exact
    /// crash `sigaltstack(2)` exists to avoid -- so being on it already
    /// disqualifies it, and so does never having been given a size.
    pub fn usable_from(&self, sp: usize) -> bool {
        self.size != 0 && !self.flags.contains(SignalStackFlags::DISABLE) && !self.contains_sp(sp)
    }

    /// A stack with nothing installed: `sas_ss_reset()`.
    pub fn disabled() -> SignalStack {
        SignalStack::default()
    }

    /// The `stack_t` a signal frame carries in `uc_stack`, taken as a signal
    /// is delivered: the stack as stored (`__save_altstack`), which
    /// [`Self::restore_from_frame`] puts back at `sigreturn`. With
    /// `SS_AUTODISARM` the delivery also disarms the stack
    /// (`signal_delivered()` -> `sas_ss_reset()`), so the handler may
    /// `swapcontext` away from it and a second signal cannot land on the
    /// frames it left there.
    ///
    /// Neither half existed: `uc_stack` went to userspace as zeros and
    /// `SS_AUTODISARM` was stored and never acted on, so a handler that
    /// switched contexts (the fibers of libco, boost.context, QEMU's
    /// coroutines) ran the next signal's frame on top of its own.
    pub fn take_for_frame(&mut self) -> SignalStack {
        let saved = *self;
        if self.flags.contains(SignalStackFlags::AUTODISARM) {
            *self = SignalStack::disabled();
        }
        saved
    }

    /// `sigreturn`'s `restore_altstack()`: install the `uc_stack` the frame
    /// came back with, as a `sigaltstack(&uc_stack, NULL)` from a thread at
    /// `sp` would, and ignore what that call would refuse (Linux drops the
    /// error too: the frame is userspace's to doctor). That is how an
    /// auto-disarmed stack comes back once its handler returns.
    pub fn restore_from_frame(&mut self, from: SignalStack, sp: usize) {
        // `on_sig_stack()`: a thread on its alternate stack may not change
        // it (`EPERM`), and an auto-disarming stack counts as never being
        // stood on. Judged on the stack as it is NOW, after any disarm.
        if !self.flags.contains(SignalStackFlags::AUTODISARM) && self.contains_sp(sp) {
            return;
        }
        if from.validate().is_err() {
            return;
        }
        *self = from.as_installed();
    }

    /// What `sigaltstack(2)` refuses, in Linux's order: unknown flags
    /// (`EINVAL`), then a mode that is not one of `0`, `SS_ONSTACK` or
    /// `SS_DISABLE` (`EINVAL` too: `do_sigaltstack` reads `ss_flags` with
    /// `SS_AUTODISARM` masked off as a small enum, and `SS_ONSTACK |
    /// SS_DISABLE` is the value 3, which names nothing), then, when the call
    /// installs rather than disables a stack, one smaller than `MINSIGSTKSZ`
    /// (`ENOMEM`). `SS_ONSTACK` is accepted as a mode for compatibility, as
    /// `do_sigaltstack` does.
    ///
    /// The mode went unchecked, so `SS_ONSTACK | SS_DISABLE` was accepted and
    /// stored as a disabled stack, where Linux refuses the call and keeps the
    /// stack the thread had.
    pub fn validate(&self) -> LxResult<()> {
        if !VALID_SIGSTACK_FLAGS.contains(self.flags) {
            return Err(LxError::EINVAL);
        }
        let mode = self.flags - SignalStackFlags::AUTODISARM;
        if mode == SignalStackFlags::ONSTACK | SignalStackFlags::DISABLE {
            return Err(LxError::EINVAL);
        }
        if !self.flags.contains(SignalStackFlags::DISABLE) && self.size < MIN_SIGSTACK_SIZE {
            return Err(LxError::ENOMEM);
        }
        Ok(())
    }

    /// This stack as the task stores it once `sigaltstack(2)` accepts it:
    /// `SS_DISABLE` forgets the address and size, Linux zeroes both, so a
    /// later `sigaltstack(NULL, &old)` does not hand back memory the program
    /// may have freed; `SS_ONSTACK` is a report, never stored.
    pub fn as_installed(&self) -> SignalStack {
        let mut out = *self;
        out.flags.remove(SignalStackFlags::ONSTACK);
        if out.flags.contains(SignalStackFlags::DISABLE) {
            out.sp = 0;
            out.size = 0;
        }
        out
    }

    /// This stack as `sigaltstack(2)` reports it to a thread using `sp`.
    ///
    /// `SS_ONSTACK` and `SS_DISABLE` are *derived*, never stored: Linux keeps
    /// only `SS_AUTODISARM` in the task and computes the other two from the
    /// caller's stack pointer (`sas_ss_flags`). Reporting them is not
    /// cosmetic -- glibc and the Rust runtime read `ss_flags` back to decide
    /// whether they may install a stack of their own, and a kernel that
    /// always answers `SS_DISABLE` tells every one of them the slot is free.
    pub fn as_reported_from(&self, sp: usize) -> SignalStack {
        let mut out = *self;
        out.flags
            .remove(SignalStackFlags::ONSTACK | SignalStackFlags::DISABLE);
        if self.size == 0 || self.flags.contains(SignalStackFlags::DISABLE) {
            out.flags.insert(SignalStackFlags::DISABLE);
        } else if self.contains_sp(sp) {
            out.flags.insert(SignalStackFlags::ONSTACK);
        }
        out
    }
}

numeric_enum! {
    #[repr(u8)]
    #[derive(Eq, PartialEq, Debug, Copy, Clone)]
    pub enum Signal {
        SIGHUP = 1,
        SIGINT = 2,
        SIGQUIT = 3,
        SIGILL = 4,
        SIGTRAP = 5,
        SIGABRT = 6,
        SIGBUS = 7,
        SIGFPE = 8,
        SIGKILL = 9,
        SIGUSR1 = 10,
        SIGSEGV = 11,
        SIGUSR2 = 12,
        SIGPIPE = 13,
        SIGALRM = 14,
        SIGTERM = 15,
        SIGSTKFLT = 16,
        SIGCHLD = 17,
        SIGCONT = 18,
        SIGSTOP = 19,
        SIGTSTP = 20,
        SIGTTIN = 21,
        SIGTTOU = 22,
        SIGURG = 23,
        SIGXCPU = 24,
        SIGXFSZ = 25,
        SIGVTALRM = 26,
        SIGPROF = 27,
        SIGWINCH = 28,
        SIGIO = 29,
        SIGPWR = 30,
        SIGSYS = 31,
        // real time signals
        SIGRT32 = 32,
        SIGRT33 = 33,
        SIGRT34 = 34,
        SIGRT35 = 35,
        SIGRT36 = 36,
        SIGRT37 = 37,
        SIGRT38 = 38,
        SIGRT39 = 39,
        SIGRT40 = 40,
        SIGRT41 = 41,
        SIGRT42 = 42,
        SIGRT43 = 43,
        SIGRT44 = 44,
        SIGRT45 = 45,
        SIGRT46 = 46,
        SIGRT47 = 47,
        SIGRT48 = 48,
        SIGRT49 = 49,
        SIGRT50 = 50,
        SIGRT51 = 51,
        SIGRT52 = 52,
        SIGRT53 = 53,
        SIGRT54 = 54,
        SIGRT55 = 55,
        SIGRT56 = 56,
        SIGRT57 = 57,
        SIGRT58 = 58,
        SIGRT59 = 59,
        SIGRT60 = 60,
        SIGRT61 = 61,
        SIGRT62 = 62,
        SIGRT63 = 63,
        SIGRT64 = 64,
    }
}

impl Signal {
    pub const RTMIN: usize = 32;
    pub const RTMAX: usize = 64;

    /// Read a signal number as it arrives in a syscall argument register.
    ///
    /// Every syscall that takes one declares it `int` — `kill(2)`, `tkill(2)`,
    /// `tgkill(2)`, `rt_sigaction(2)`, `rt_sigqueueinfo(2)`,
    /// `pidfd_send_signal(2)`, `prctl(PR_SET_PDEATHSIG)` — so Linux truncates
    /// the register to 32 bits and then hands it to `valid_signal()`, which
    /// takes it as `unsigned long`: a negative number becomes enormous and is
    /// rejected, and anything above `_NSIG` (64) is rejected too.
    ///
    /// Signal 0 is *valid* and delivers nothing: it is the existence probe
    /// behind `kill(pid, 0)`. It comes back as `Ok(None)` so each caller can
    /// answer it where Linux does — after looking the target up, so a dead
    /// process still gets `ESRCH`. A caller that has no use for it (sigaction)
    /// turns `None` into `EINVAL`.
    ///
    /// Reading the register as `signum as u8` instead, which is what every
    /// call site did, **truncated**: `kill(pid, 265)` delivered SIGKILL and
    /// `prctl(PR_SET_PDEATHSIG, 265)` latched it, where Linux answers EINVAL
    /// to both; and `kill(pid, 0)` answered EINVAL, which is how a stale lock
    /// file looks alive to the program holding it.
    pub fn from_syscall_arg(signum: usize) -> LxResult<Option<Self>> {
        // The declared `int` of the uAPI: drop the high half, keep the sign.
        let sig = signum as u32 as i32;
        if sig == 0 {
            return Ok(None);
        }
        // `valid_signal()`: 1..=_NSIG, with negatives failing as huge unsigned.
        if sig < 1 || sig > Self::RTMAX as i32 {
            return Err(LxError::EINVAL);
        }
        core::convert::TryFrom::try_from(sig as u8)
            .map(Some)
            .map_err(|_| LxError::EINVAL)
    }

    pub fn is_standard(self) -> bool {
        (self as usize) < Self::RTMIN
    }

    pub fn as_bit(&self) -> u64 {
        1 << (*self as u64 - 1)
    }
}

/// The signal number every syscall receives is an `int` in a register, and the
/// tree read it as `signum as u8` in eight places.
///
/// What that costs is not theoretical: the low byte of 265 is 9, so
/// `kill(pid, 265)` killed the target outright, and the low byte of 0 is 0,
/// which no `Signal` variant matches, so `kill(pid, 0)` — the existence probe
/// every lock file in userspace is built on — answered EINVAL. The comment in
/// `sys_kill`'s own `signal_pid` describes what a profile lock does with
/// ESRCH versus anything else, and the probe never reached it.
#[cfg(test)]
mod signal_arg_tests {
    use super::*;

    #[test]
    fn signal_zero_is_a_probe_not_an_error() {
        // `kill(pid, 0)` sends nothing and reports whether the target exists.
        // `None` is how the caller learns to skip delivery and still answer
        // ESRCH for a pid that is gone.
        assert_eq!(Signal::from_syscall_arg(0), Ok(None));
    }

    #[test]
    fn every_named_signal_survives_the_round_trip() {
        for n in 1..=Signal::RTMAX {
            let got = Signal::from_syscall_arg(n).expect("1..=64 are all valid");
            let got = got.expect("only 0 is the probe");
            assert_eq!(got as usize, n, "signal {} came back as {:?}", n, got);
        }
    }

    #[test]
    fn a_number_above_nsig_is_rejected_instead_of_truncated() {
        // 265 & 0xff == 9 == SIGKILL. This is the whole bug in one line.
        assert_eq!(Signal::from_syscall_arg(265), Err(LxError::EINVAL));
        assert_eq!(Signal::from_syscall_arg(256), Err(LxError::EINVAL));
        assert_eq!(Signal::from_syscall_arg(65), Err(LxError::EINVAL));
        assert_eq!(Signal::from_syscall_arg(usize::MAX), Err(LxError::EINVAL));
    }

    #[test]
    fn the_gap_between_nsig_and_a_full_byte_is_rejected() {
        // 65..=255 fit in a `u8` and would have reached `Signal::try_from`,
        // which rejects them; the range check has to agree, or a stricter
        // guard here would start refusing what the enum accepts.
        for n in (Signal::RTMAX + 1)..=255 {
            assert_eq!(
                Signal::from_syscall_arg(n),
                Err(LxError::EINVAL),
                "{} is not a signal",
                n
            );
        }
    }

    #[test]
    fn a_negative_signal_number_is_rejected_whatever_its_low_byte() {
        // A negative `int` arrives sign-extended in the register. -247 has low
        // byte 9, so `as u8` turned `kill(pid, -247)` into SIGKILL.
        for sig in [-1i32, -9, -247, -256, i32::MIN] {
            let arg = sig as isize as usize;
            assert_eq!(
                Signal::from_syscall_arg(arg),
                Err(LxError::EINVAL),
                "kill(_, {}) must be EINVAL",
                sig
            );
        }
    }

    #[test]
    fn the_high_half_of_the_register_is_dropped_the_way_linux_drops_it() {
        // `SYSCALL_DEFINE` truncates to the declared `int` before validating,
        // so a caller that leaves rubbish in the high 32 bits gets the same
        // answer as one that does not. Matching Linux here matters more than
        // being stricter than it: a guard harder than the kernel's rejects
        // programs the kernel accepts.
        assert_eq!(
            Signal::from_syscall_arg(0xdead_beef_0000_0009),
            Ok(Some(Signal::SIGKILL))
        );
        assert_eq!(Signal::from_syscall_arg(0xffff_ffff_0000_0000), Ok(None));
    }

    #[test]
    fn the_real_time_range_is_reachable() {
        // musl's `SIGRTMIN` is 34 after it reserves three for its own use, so
        // a threaded program that never gets here has no cancellation.
        assert_eq!(
            Signal::from_syscall_arg(Signal::RTMIN),
            Ok(Some(Signal::SIGRT32))
        );
        assert_eq!(
            Signal::from_syscall_arg(Signal::RTMAX),
            Ok(Some(Signal::SIGRT64))
        );
        assert!(!Signal::SIGRT32.is_standard());
        assert!(Signal::SIGSYS.is_standard());
    }
}

#[cfg(test)]
mod sigaltstack_tests {
    //! `sigaltstack(2)` is how a program survives its own stack running out:
    //! it hands the kernel a second, small stack and asks (with `SA_ONSTACK`)
    //! that signal handlers run there. The whole point is the handler for the
    //! signal a blown stack raises, so getting the "is it usable" answer wrong
    //! is only ever noticed at the worst possible moment. Both the kernel side
    //! (where to put the frame) and the userspace side (what `ss_flags` reads
    //! back) come out of these three functions.

    use super::*;

    const BASE: usize = 0x7000_0000;
    const SIZE: usize = 0x4000;

    fn alt() -> SignalStack {
        SignalStack {
            sp: BASE,
            flags: SignalStackFlags::empty(),
            size: SIZE,
        }
    }

    /// `ss_flags` as the syscall reads them off the user's `stack_t`: a raw
    /// word, unknown bits included (`from_bits_truncate` would drop them and
    /// test nothing).
    #[allow(unsafe_code)]
    fn raw_flags(bits: u32) -> SignalStackFlags {
        unsafe { SignalStackFlags::from_bits_unchecked(bits) }
    }

    #[test]
    fn the_stack_holds_every_pointer_that_could_have_pushed_onto_it() {
        let alt = alt();
        // A stack pointer is one past the byte it last pushed, so a thread
        // that has just switched here has sp == base + size and has pushed
        // nothing yet. Excluding the top (a `<` instead of `<=`) would make
        // `sigaltstack` report "not on it" for exactly the thread that just
        // got there, and the next signal would restart the frame at the top,
        // on top of the handler already running.
        assert!(alt.contains_sp(BASE + SIZE), "the top is on the stack");
        assert!(alt.contains_sp(BASE + 1), "one byte in is on the stack");
        assert!(alt.contains_sp(BASE + SIZE / 2));
        // The base itself is not: sp == base means the stack is completely
        // full, and Linux's `on_sig_stack` is strict here for the same reason
        // the top is inclusive.
        assert!(!alt.contains_sp(BASE), "the base is not on the stack");
        assert!(!alt.contains_sp(BASE - 1));
        assert!(!alt.contains_sp(BASE + SIZE + 1), "one past the top is off");
    }

    #[test]
    fn a_stack_pointer_below_the_base_does_not_wrap_into_range() {
        // `sp - self.sp` on two `usize`s: a stack pointer below the base
        // underflows if the `sp > self.sp` guard is dropped, and every low
        // address then reads as "on the alternate stack" -- which would stop
        // the kernel from ever switching to it.
        let alt = alt();
        assert!(!alt.contains_sp(0));
        assert!(!alt.contains_sp(0x1000));
    }

    #[test]
    fn a_stack_that_was_never_installed_is_not_usable() {
        let mut none = SignalStack::default();
        assert!(!none.usable_from(0x1000));
        // Even with a plausible address: it is the size that says whether
        // `sigaltstack` was ever called.
        none.sp = BASE;
        assert!(!none.usable_from(0x1000));
    }

    #[test]
    fn a_stack_with_no_size_is_not_usable_even_without_the_disable_flag() {
        // The syscall refuses to store a stack smaller than MINSIGSTKSZ
        // unless SS_DISABLE is set, so the two conditions always agree in
        // practice -- but the frame placement must not depend on that. A
        // size of zero means every address is off the end, so switching
        // there would put the frame in whatever follows the base address.
        let empty = SignalStack {
            sp: BASE,
            flags: SignalStackFlags::empty(),
            size: 0,
        };
        assert!(!empty.usable_from(0x1000));
        assert!(empty
            .as_reported_from(0x1000)
            .flags
            .contains(SignalStackFlags::DISABLE));
    }

    #[test]
    fn an_explicitly_disabled_stack_is_not_usable() {
        // A disabled stack that still carries a size. The syscall zeroes both
        // fields on SS_DISABLE, as Linux does, so this pairing should not
        // reach us -- but neither the placement nor the report may lean on
        // that, or a stack a program disabled would come back usable.
        let mut alt = alt();
        alt.flags.insert(SignalStackFlags::DISABLE);
        assert!(!alt.usable_from(0x1000));
        let reported = alt.as_reported_from(BASE + SIZE / 2);
        assert!(reported.flags.contains(SignalStackFlags::DISABLE));
        assert!(
            !reported.flags.contains(SignalStackFlags::ONSTACK),
            "a disabled stack is never the one we are running on"
        );
    }

    #[test]
    fn a_stack_we_are_already_running_on_is_not_usable() {
        // The nested case: a handler already running on the alternate stack
        // takes a second signal. Switching again would put the new frame at
        // the top, over the frames of the handler that is still live. Linux
        // stays on the current stack instead, which is why `sas_ss_flags`
        // returns SS_ONSTACK rather than 0 here.
        let alt = alt();
        assert!(!alt.usable_from(BASE + SIZE / 2));
        assert!(!alt.usable_from(BASE + SIZE));
        // ...but a thread on its ordinary stack may switch.
        assert!(alt.usable_from(0xffff_0000));
    }

    #[test]
    fn an_installed_stack_reads_back_as_onstack_only_while_in_use() {
        let alt = alt();
        let from_outside = alt.as_reported_from(0xffff_0000);
        assert!(
            !from_outside.flags.contains(SignalStackFlags::ONSTACK),
            "a thread on its ordinary stack is not on the alternate one"
        );
        assert!(!from_outside.flags.contains(SignalStackFlags::DISABLE));

        let from_inside = alt.as_reported_from(BASE + SIZE / 2);
        assert!(
            from_inside.flags.contains(SignalStackFlags::ONSTACK),
            "a handler running on the alternate stack must see SS_ONSTACK"
        );
        assert!(!from_inside.flags.contains(SignalStackFlags::DISABLE));
    }

    #[test]
    fn a_stack_that_was_never_installed_reads_back_as_disabled() {
        let reported = SignalStack::default().as_reported_from(0xffff_0000);
        assert!(reported.flags.contains(SignalStackFlags::DISABLE));
        assert!(!reported.flags.contains(SignalStackFlags::ONSTACK));
        assert_eq!(reported.size, 0);
    }

    #[test]
    fn the_address_and_size_survive_the_report_untouched() {
        // `sigaltstack(NULL, &old)` is how a library saves the stack it found
        // so it can put it back afterwards. Rounding or zeroing either field
        // here hands back a stack that is not the one that was installed.
        let reported = alt().as_reported_from(0xffff_0000);
        assert_eq!(reported.sp, BASE);
        assert_eq!(reported.size, SIZE);
    }

    #[test]
    fn autodisarm_is_stored_and_survives_the_report() {
        // SS_AUTODISARM is the one flag Linux really does keep in the task
        // (`current->sas_ss_flags`); the other two are computed. Dropping it
        // on the way out would make a `sigaltstack(NULL, &old)` +
        // `sigaltstack(&old, NULL)` round trip silently disarm the
        // auto-disarm, which is what makecontext/swapcontext users rely on.
        let mut alt = alt();
        alt.flags.insert(SignalStackFlags::AUTODISARM);
        let reported = alt.as_reported_from(BASE + SIZE / 2);
        assert!(reported.flags.contains(SignalStackFlags::AUTODISARM));
        assert!(reported.flags.contains(SignalStackFlags::ONSTACK));
    }

    #[test]
    fn a_delivery_disarms_an_autodisarm_stack_and_hands_the_old_one_to_the_frame() {
        let mut alt = alt();
        alt.flags.insert(SignalStackFlags::AUTODISARM);
        let mut task = alt;
        let uc_stack = task.take_for_frame();
        assert_eq!(uc_stack, alt, "uc_stack must carry the stack as it was");
        assert!(
            !task.usable_from(0xffff_0000),
            "the stack must be disarmed while the handler runs"
        );
        assert_eq!((task.sp, task.size), (0, 0));
        assert!(task.flags.contains(SignalStackFlags::DISABLE));
    }

    #[test]
    fn without_autodisarm_the_stack_stays_installed_across_a_delivery() {
        let mut task = alt();
        let uc_stack = task.take_for_frame();
        assert_eq!(uc_stack, alt());
        assert_eq!(task, alt());
        assert!(task.usable_from(0xffff_0000));
    }

    #[test]
    fn sigreturn_puts_the_autodisarmed_stack_back() {
        let mut alt = alt();
        alt.flags.insert(SignalStackFlags::AUTODISARM);
        let mut task = alt;
        let uc_stack = task.take_for_frame();
        // The handler returns from the alternate stack itself: with the
        // stack disarmed that is not "standing on it", so the restore goes
        // through, AUTODISARM included.
        task.restore_from_frame(uc_stack, BASE + SIZE / 2);
        assert_eq!(task, alt, "the stack did not come back at sigreturn");
        assert!(task.usable_from(0xffff_0000));
    }

    #[test]
    fn a_handler_on_the_stack_cannot_change_it_from_its_frame() {
        // No AUTODISARM: the handler runs on the stack, so `sigaltstack`
        // from there is EPERM and the restore is a no-op, doctored or not.
        let mut task = alt();
        let doctored = SignalStack {
            sp: 0x1000_0000,
            flags: SignalStackFlags::empty(),
            size: SIZE,
        };
        task.restore_from_frame(doctored, BASE + SIZE / 2);
        assert_eq!(task, alt(), "a frame changed the stack the handler runs on");
        // Off the stack (a handler that ran on the normal stack) it does.
        task.restore_from_frame(doctored, 0xffff_0000);
        assert_eq!(task, doctored);
    }

    #[test]
    fn sigreturn_ignores_a_frame_stack_that_sigaltstack_would_refuse() {
        let mut task = alt();
        let bad_flags = SignalStack {
            sp: 0x1000_0000,
            flags: raw_flags(0x40),
            size: SIZE,
        };
        task.restore_from_frame(bad_flags, 0xffff_0000);
        assert_eq!(task, alt(), "unknown flags were installed");
        let too_small = SignalStack {
            sp: 0x1000_0000,
            flags: SignalStackFlags::empty(),
            size: MIN_SIGSTACK_SIZE - 1,
        };
        task.restore_from_frame(too_small, 0xffff_0000);
        assert_eq!(task, alt(), "a stack under MINSIGSTKSZ was installed");
        // And a disabling one forgets the address, as the syscall does.
        let disable = SignalStack {
            sp: 0x1000_0000,
            flags: SignalStackFlags::DISABLE,
            size: SIZE,
        };
        task.restore_from_frame(disable, 0xffff_0000);
        assert_eq!(task, SignalStack::disabled());
    }

    #[test]
    fn ss_onstack_is_a_mode_sigaltstack_accepts_and_never_stores() {
        // `do_sigaltstack`: the mode may be SS_DISABLE, SS_ONSTACK or 0.
        // A program handing back the stack it read with `sigaltstack(NULL,
        // &old)` from a handler passes SS_ONSTACK, and was refused.
        let mut alt = alt();
        alt.flags.insert(SignalStackFlags::ONSTACK);
        assert_eq!(alt.validate(), Ok(()));
        assert_eq!(alt.as_installed(), self::alt());
        assert_eq!(
            SignalStack {
                flags: raw_flags(0x40),
                ..alt
            }
            .validate(),
            Err(LxError::EINVAL)
        );
    }

    /// `ss_flags & ~SS_AUTODISARM` is a mode, not a bitmask: `0`,
    /// `SS_ONSTACK` or `SS_DISABLE`. Both bits at once is the value 3, and
    /// `do_sigaltstack` answers `EINVAL` (with or without `SS_AUTODISARM` on
    /// top) rather than treating it as a disable.
    #[test]
    fn ss_onstack_and_ss_disable_together_are_not_a_mode() {
        let both = SignalStackFlags::ONSTACK | SignalStackFlags::DISABLE;
        let mut alt = alt();
        alt.flags = both;
        assert_eq!(alt.validate(), Err(LxError::EINVAL));
        alt.flags = both | SignalStackFlags::AUTODISARM;
        assert_eq!(alt.validate(), Err(LxError::EINVAL));
        // Each of them alone, with and without the auto-disarm bit, stays
        // a mode the call takes.
        for mode in [
            SignalStackFlags::empty(),
            SignalStackFlags::ONSTACK,
            SignalStackFlags::DISABLE,
        ] {
            alt.flags = mode;
            assert_eq!(alt.validate(), Ok(()), "{:?}", mode);
            alt.flags = mode | SignalStackFlags::AUTODISARM;
            assert_eq!(alt.validate(), Ok(()), "{:?}", mode);
        }
    }

    #[test]
    fn the_three_flag_values_are_the_ones_userspace_sends() {
        // These numbers are the uAPI: userspace writes them into `ss_flags`
        // and reads them back out. A test that compared the constant to
        // itself would move with it and check nothing.
        assert_eq!(SignalStackFlags::ONSTACK.bits(), 1);
        assert_eq!(SignalStackFlags::DISABLE.bits(), 2);
        assert_eq!(SignalStackFlags::AUTODISARM.bits(), 1 << 31);
    }

    #[test]
    fn the_struct_is_the_linux_stack_t_layout() {
        // `struct stack_t { void *ss_sp; int ss_flags; size_t ss_size; }`:
        // 8 + 4 (+4 padding) + 8 on 64-bit. Userspace fills this in itself,
        // so a field at the wrong offset reads the flags out of the pointer.
        use core::mem::{align_of, size_of};
        assert_eq!(size_of::<SignalStack>(), 24);
        assert_eq!(align_of::<SignalStack>(), 8);
        let s = SignalStack {
            sp: 0x1122_3344_5566_7788,
            flags: SignalStackFlags::ONSTACK,
            size: 0x99aa_bbcc_ddee_ff00,
        };
        let bytes: [u8; 24] = unsafe { core::mem::transmute(s) };
        let word = |at: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&bytes[at..at + 8]);
            usize::from_ne_bytes(w)
        };
        assert_eq!(word(0), s.sp);
        assert_eq!(
            u32::from_ne_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            1
        );
        assert_eq!(word(16), s.size);
    }
}
