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
    /// `siginfo_t` for a child that exited normally (`CLD_EXITED`).
    pub fn child_exited(pid: i32, status: i32) -> Self {
        let mut info = Self {
            signo: Signal::SIGCHLD as i32,
            errno: 0,
            code: SignalCode::CLD_EXITED,
            ..Self::default()
        };
        info.field.write_sigchld(pid, status);
        info
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
