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
//! ## And what is still missing
//!
//! `sigaction` (67) and `sigreturn` (119), the pre-POSIX pair, are not here.
//! Their `struct old_sigaction` keeps a 32-bit `sa_mask` and their frame is a
//! `struct sigframe_ia32` with no `siginfo` --- a second layout, reached only
//! by a libc that does not use `rt_sigaction`, which musl and glibc both do.
//! `rt_sigqueueinfo` (178) and `rt_sigtimedwait` (177) pass a `siginfo_t`
//! **inwards**, which needs the conversion of
//! [`linux_object::signal::siginfo32`] run backwards and is not written yet.

use super::*;
use linux_object::signal::ucontext::i386::SignalStack32;
use linux_object::signal::{SignalAction, SignalActionFlags, Sigset};

/// i386's number for `rt_sigaction`.
pub(super) const I386_RT_SIGACTION: u32 = 174;
/// i386's number for `sigaltstack`.
pub(super) const I386_SIGALTSTACK: u32 = 186;
/// i386's number for `rt_sigreturn`, which has no native twin: the native
/// `rt_sigreturn` reads a native frame.
pub(super) const I386_RT_SIGRETURN: u32 = 173;

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
            || Ok(act.read()?.to_native()),
            |old| Ok(oldact.write_if_not_null(SignalAction32::from_native(old))?),
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
            || Ok(ss.read()?.to_native()),
            |old| Ok(old_ss.write_if_not_null(SignalStack32::from_native(&old))?),
        )
    }
}

#[cfg(test)]
mod tests;
