//! `sigaction`, `sigaltstack` and `rt_sigreturn` for a 32-bit process.
//!
//! Three of the four things a signal needs, the fourth being the frame the
//! kernel pushes (`loader::linux::sigframe32`). They are here rather than in
//! either allowlist because each passes a structure that is not the native
//! one with its words halved:
//!
//! | | native | i386 |
//! |---|---|---|
//! | `struct sigaction` | 32 bytes | 20 |
//! | `struct sigaltstack` | 24 bytes | 12 |
//!
//! and `rt_sigreturn` has no native twin at all --- it reads a frame, and the
//! frame is a 32-bit one.
//!
//! ## What the sizes are made of
//!
//! Neither is a straight halving. A `sigaction`'s `sa_mask` is a `sigset_t`,
//! which is **eight bytes in both ABIs** (`_NSIG` is 64 either way), so three
//! of its four members halve and the fourth does not: 32 bytes become 20 and
//! not 16. A `sigaltstack`'s `ss_flags` is an `int` at both widths, so
//! natively it has four bytes of padding after it that the 32-bit struct does
//! not: 24 become 12, which happens to be half, by way of a member that did
//! not change and padding that vanished.
//!
//! | `struct old_sigaction` | --- | 16 |
//!
//! the third being the pre-POSIX `sigaction` (67), whose `sa_mask` is a
//! single `u32` and whose members are in a different ORDER: it is not the
//! twenty above with four taken off. x86_64 has no such call at all, which
//! is what the dash in that row says.
//!
//! ## Alignment
//!
//! All three are read and written with the **unaligned** pair of
//! [`UserInPtr`]/[`UserOutPtr`]. Linux's `copy_from_user` and `put_user` ask
//! `access_ok` and nothing about alignment, so a `struct sigaction` one byte
//! off a word boundary is accepted there; the typed `read` refuses it.
//! Measured on a Linux host for all three numbers (67, 174 and 186): every
//! one of them answers 0 for a structure at 1 modulo 4, where this kernel
//! answered `EFAULT`.
//!
//! ## And what is still missing
//!
//! `rt_sigqueueinfo` (178) passes a `siginfo_t` **inwards**, which needs the
//! conversion of [`linux_object::signal::siginfo32`] run backwards and is not
//! written yet. `rt_sigtimedwait` (177) is **not** here and needs no handler
//! of its own: its `siginfo_t` goes **outwards**, so the conversion that
//! already exists is the one it needs, and both of its structures are the
//! width of the process, which is what the narrow table asks. It is in
//! `i386_abi_narrow.in`.

use super::*;
use linux_object::signal::ucontext::i386::SignalStack32;
use linux_object::signal::{SignalAction, SignalActionFlags, Sigset};

/// i386's number for `rt_sigaction`.
pub(super) const I386_RT_SIGACTION: u32 = 174;
/// And for the pre-POSIX `sigaction`, which is a second call with a
/// structure of its own and not a flag on the one above. It is what a libc
/// that does not use `rt_sigaction` installs a handler with, and what
/// `signal(2)`'s own manual page calls the "original" call; x86_64 has no
/// number for it.
pub(super) const I386_SIGACTION: u32 = 67;
/// i386's number for `sigaltstack`.
pub(super) const I386_SIGALTSTACK: u32 = 186;
/// i386's number for `rt_sigreturn`, which has no native twin: the native
/// `rt_sigreturn` reads a native frame.
pub(super) const I386_RT_SIGRETURN: u32 = 173;
/// And for the pre-POSIX `sigreturn`, which returns from the **old** frame:
/// the one an action without `SA_SIGINFO` is given, and so the one a handler
/// installed with `signal(3)` comes back from. It has no native twin at all,
/// x86_64 having only the `rt_` form.
pub(super) const I386_SIGRETURN: u32 = 119;

/// `struct compat_sigaction` (`include/linux/compat.h`): twenty bytes.
///
/// `sa_mask` is last and is `compat_sigset_t`, two `u32`. Spelling it as two
/// words rather than as a [`Sigset`] is the same decision the frame's
/// `uc_sigmask` makes, and for the same reason: a `Sigset` is eight-byte
/// aligned, which would pad this struct to 24 bytes and put the mask four
/// bytes past where a 32-bit libc writes it. Linux marks the field
/// `__packed` to say exactly that.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
pub struct SignalAction32 {
    pub handler: u32,
    pub flags: u32,
    pub restorer: u32,
    pub mask: [u32; 2],
}

impl SignalAction32 {
    /// The action as the kernel keeps it.
    ///
    /// `sa_flags` is a `compat_ulong_t`, so a 32-bit program can only ever
    /// set the low 32 bits --- which is all of them: the highest flag Linux
    /// defines is `SA_RESETHAND` at bit 31.
    pub fn to_native(self) -> SignalAction {
        SignalAction {
            handler: self.handler as usize,
            flags: SignalActionFlags::from_bits_truncate(self.flags as usize),
            restorer: self.restorer as usize,
            mask: Sigset::new(self.mask[0] as u64 | ((self.mask[1] as u64) << 32)),
        }
    }

    /// And what `sigaction(sig, NULL, &old)` reads back.
    ///
    /// The handler and the restorer are truncated rather than refused: they
    /// are addresses in a process that lives below 4 GiB, so there is nothing
    /// above the low half to lose, and every one of them came in through this
    /// same structure in the first place.
    pub fn from_native(action: SignalAction) -> Self {
        let mask = action.mask.val();
        Self {
            handler: action.handler as u32,
            flags: action.flags.bits() as u32,
            restorer: action.restorer as u32,
            mask: [mask as u32, (mask >> 32) as u32],
        }
    }
}

/// `struct compat_old_sigaction` (`include/linux/compat.h`): sixteen bytes,
/// and NOT the twenty of [`SignalAction32`] with a word taken off.
///
/// Two things move at once, which is why this is its own type and not a
/// narrower spelling of the one above:
///
/// | | `compat_sigaction` | `compat_old_sigaction` |
/// |---|---|---|
/// | 0 | `sa_handler` | `sa_handler` |
/// | 4 | `sa_flags` | **`sa_mask`** |
/// | 8 | `sa_restorer` | `sa_flags` |
/// | 12 | `sa_mask` (two words) | `sa_restorer` |
///
/// The mask is `compat_old_sigset_t`, **one** `u32` where the `rt_` form has
/// the whole eight-byte `sigset_t`, so this layout can only ever name the
/// first 32 signals --- and it is the SECOND member, not the last. A kernel
/// that read these sixteen bytes in the `rt_` order would take the mask for
/// the flags and the flags for the restorer, and a handler installed by
/// `signal(3)` would come back through an address of 0.
///
/// Measured on a Linux host, not read off a header: an i386 program installs
/// an action through 67 and reads it back through 174, and the handler, the
/// flags, the restorer and the mask each come out of the offset this table
/// gives. The same measurement showed nothing written past byte sixteen.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
pub struct OldSignalAction32 {
    pub handler: u32,
    pub mask: u32,
    pub flags: u32,
    pub restorer: u32,
}

impl OldSignalAction32 {
    /// The action as the kernel keeps it, with the mask WIDENED BY ZEROS.
    ///
    /// `compat_sys_sigaction` finishes with `siginitset(&new_ka.sa.sa_mask,
    /// mask)`, which sets the low word and clears the high one: an action
    /// installed through 67 blocks no signal above 32 **even if the action
    /// it replaces blocked some**. That is not a rounding of the old value,
    /// it is a replacement, and it is measured: an action installed through
    /// 174 with a realtime signal in its mask, re-installed through 67,
    /// reads back through 174 with that half zero.
    pub fn to_native(self) -> SignalAction {
        SignalAction {
            handler: self.handler as usize,
            flags: SignalActionFlags::from_bits_truncate(self.flags as usize),
            restorer: self.restorer as usize,
            mask: Sigset::new(self.mask as u64),
        }
    }

    /// And what `sigaction(sig, NULL, &old)` reads back, with the high half
    /// of the mask DROPPED without a word.
    ///
    /// `put_user(old_ka.sa.sa_mask.sig[0], &oact->sa_mask)`: there is
    /// nowhere in sixteen bytes to report the other 32 signals, and Linux
    /// does not refuse the call over it. Measured the same way round: an
    /// action installed through 174 with a realtime signal in its mask comes
    /// back through 67 with only the low word, and the call answers 0.
    ///
    /// The handler and the restorer are truncated for the reason
    /// [`SignalAction32::from_native`] gives: the process lives below 4 GiB.
    pub fn from_native(action: SignalAction) -> Self {
        Self {
            handler: action.handler as u32,
            mask: action.mask.val() as u32,
            flags: action.flags.bits() as u32,
            restorer: action.restorer as u32,
        }
    }
}

impl Syscall<'_> {
    /// `compat_sys_rt_sigaction`: the same call as the native one, with
    /// twenty bytes in and twenty bytes out.
    ///
    /// Every check and the whole order of the call live in
    /// [`Syscall::sigaction_with`], which the native entry point uses too.
    /// This is the conversion and nothing else.
    pub fn sys_rt_sigaction32(
        &self,
        signum: usize,
        act: UserInPtr<SignalAction32>,
        mut oldact: UserOutPtr<SignalAction32>,
        sigsetsize: usize,
    ) -> SysResult {
        info!(
            "rt_sigaction32: signum={}, act={:?}, oldact={:?}, sigsetsize={}, thread={}",
            signum,
            act,
            oldact,
            sigsetsize,
            self.thread.id()
        );
        self.sigaction_with(
            signum,
            act.is_null(),
            sigsetsize,
            || Ok(act.read_unaligned()?.to_native()),
            |old| Ok(oldact.write_if_not_null_unaligned(SignalAction32::from_native(old))?),
        )
    }

    /// `compat_sys_sigaction`: the pre-POSIX call, sixteen bytes in and
    /// sixteen out.
    ///
    /// The same `do_sigaction` as the two above --- every check, and the
    /// order they are made in, is [`Syscall::sigaction_with`]'s. The
    /// `sigsetsize` it is handed is `size_of::<Sigset>()` and not a number
    /// this caller supplied, because **this call has no such argument**:
    /// `compat_sys_sigaction` takes three, and the check lives in the `rt_`
    /// entry point in Linux too (`sys_rt_sigaction` is where
    /// `sigsetsize != sizeof(sigset_t)` is refused). Passing the size that
    /// passes is how a shared `do_sigaction` says "no check here", and it
    /// is measured: `sigaction(SIGUSR1, &act, NULL)` from an i386 program
    /// answers 0, with nothing in any register that could be a size.
    pub fn sys_sigaction32(
        &self,
        signum: usize,
        act: UserInPtr<OldSignalAction32>,
        mut oldact: UserOutPtr<OldSignalAction32>,
    ) -> SysResult {
        info!(
            "sigaction32: signum={}, act={:?}, oldact={:?}, thread={}",
            signum,
            act,
            oldact,
            self.thread.id()
        );
        self.sigaction_with(
            signum,
            act.is_null(),
            core::mem::size_of::<Sigset>(),
            || Ok(act.read_unaligned()?.to_native()),
            |old| Ok(oldact.write_if_not_null_unaligned(OldSignalAction32::from_native(old))?),
        )
    }

    /// `compat_sys_sigaltstack`: twelve bytes in and twelve bytes out, and
    /// [`Syscall::sigaltstack_with`] for the rest.
    ///
    /// The structure is [`SignalStack32`], the same type the signal frame's
    /// `uc_stack` is: `compat_stack_t` is one layout, and a second spelling
    /// of it here is a second thing to get wrong.
    pub fn sys_sigaltstack32(
        &self,
        ss: UserInPtr<SignalStack32>,
        mut old_ss: UserOutPtr<SignalStack32>,
    ) -> SysResult {
        info!("sigaltstack32: ss={:?}, old_ss={:?}", ss, old_ss);
        self.sigaltstack_with(
            ss.is_null(),
            || Ok(ss.read_unaligned()?.to_native()),
            |old| Ok(old_ss.write_if_not_null_unaligned(SignalStack32::from_native(&old))?),
        )
    }
}

#[cfg(test)]
mod tests;

/// The three calls that pass a `sigset_t` and nothing else, where the only
/// thing that differs is the ALIGNMENT. Own file, own subject.
#[cfg(test)]
mod mask_tests;

/// The pre-POSIX `sigaction` (67), whose structure is the OTHER one: own
/// file, because its subject is that layout and not the `rt_` pair's.
#[cfg(test)]
mod old_sigaction_tests;

/// The two that read a `siginfo_t` INWARDS (178 and 335), whose subject is
/// the conversion run backwards: own file, so the mask's ceiling and this
/// one's stay separate.
#[cfg(test)]
mod queueinfo_tests;
