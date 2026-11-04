//! `compat_siginfo`: the `siginfo_t` a 32-bit handler reads.
//!
//! The three `int`s at the front are the same three in both ABIs. Everything
//! after them is a union, and a union has no single shape: `si_addr` is a
//! pointer, `si_value` is a `sigval` (also a pointer), and `si_pid`,
//! `si_uid` and `si_status` are `int`s that do not change at all. So which
//! bytes must be halved depends on **which member of the union is live**, and
//! nothing in the struct says which --- exactly the shape of trap
//! `setsockopt`'s `optval` has, where the width of the buffer depends on
//! another argument.
//!
//! What says which is the pair `(si_signo, si_code)`, and Linux has a
//! function for it: `siginfo_layout()` in `kernel/signal.c`, which
//! `copy_siginfo_to_user32()` switches on. [`siginfo_layout`] below is that
//! function, narrowed to the layouts this kernel actually produces.
//!
//! ## Two things move, not one
//!
//! The members are not merely halved; the union **starts in a different
//! place**. Natively `_sifields` is 8-byte aligned, so it begins at byte 16,
//! after the three `int`s and four bytes of padding. On i386 it is 4-byte
//! aligned and begins at byte **12**, with no padding at all. So even
//! `SI_USER`, whose every member is an `int` and so needs no conversion,
//! lands four bytes off if the native bytes are copied straight across: a
//! 32-bit program reading `si_pid` would get the padding, and `si_uid` would
//! be the sender's pid.
//!
//! The total stays 128 bytes (`SI_MAX_SIZE`) at both widths, which is why a
//! wrong conversion here is never a short write and never faults: it is 128
//! bytes of plausible rubbish.

use super::action::{SigInfo, SiginfoFields, SignalCode};
use super::Signal;

/// How wide a `siginfo_t` is at either width: `SI_MAX_SIZE`.
pub const SIGINFO_LEN: usize = 128;

/// Where `_sifields` begins in a 32-bit `siginfo_t`: straight after the three
/// `int`s, because the union is 4-byte aligned there.
const NARROW_FIELDS_AT: usize = 12;

/// Where it begins natively: after four bytes of padding the 8-byte
/// alignment of the union forces.
const NATIVE_FIELDS_AT: usize = 16;

/// Which member of `_sifields` a `siginfo_t` carries.
///
/// Linux's `enum siginfo_layout`, with the members this kernel builds. The
/// ones it never produces (`SIL_FAULT_MCEERR`, `SIL_SYS`, `SIL_POLL`'s
/// `si_band`, which is a `long`) are deliberately absent rather than guessed
/// at: a layout nothing writes is a conversion nothing can test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiginfoLayout {
    /// `_kill`: `si_pid`, `si_uid`. Two `int`s, so only the offset moves.
    Kill,
    /// `_sigchld`: `si_pid`, `si_uid`, `si_status`. Three `int`s --- and then
    /// `si_utime`/`si_stime`, which are `clock_t` and so would halve, except
    /// that nothing here writes them.
    Chld,
    /// `_sigfault`: `si_addr`, a pointer.
    Fault,
    /// `_rt` and `_timer`, which share a shape: two `int`s and then a
    /// `sigval`, which is a pointer.
    Rt,
}

/// Which member of the union `(si_signo, si_code)` says is live.
///
/// `si_code`'s positive values are reused per signal --- `SEGV_MAPERR`,
/// `CLD_EXITED`, `BUS_ADRALN` and `FPE_INTDIV` are all `1` --- so the signal
/// is half of the question. Asking `si_code` alone would read a `SIGCHLD`'s
/// `si_status` as a fault address.
pub fn siginfo_layout(signo: i32, code: SignalCode) -> SiginfoLayout {
    // The negative codes are shared by every signal and say who sent it.
    if code == SignalCode::QUEUE || code == SignalCode::TIMER {
        return SiginfoLayout::Rt;
    }
    if code.0 <= 0 || code == SignalCode::KERNEL {
        return SiginfoLayout::Kill;
    }
    // Positive, so the signal decides.
    if signo == Signal::SIGCHLD as i32 {
        return SiginfoLayout::Chld;
    }
    if signo == Signal::SIGSEGV as i32
        || signo == Signal::SIGBUS as i32
        || signo == Signal::SIGILL as i32
        || signo == Signal::SIGFPE as i32
        || signo == Signal::SIGTRAP as i32
    {
        return SiginfoLayout::Fault;
    }
    // A positive code on a signal with no union member of its own: the two
    // `int`s are what Linux's `default` copies, and are what every sender in
    // this kernel wrote.
    SiginfoLayout::Kill
}

impl SiginfoLayout {
    /// The members of this union, as `(offset in the native union, width)`.
    ///
    /// A pointer is eight bytes natively and four to the caller, and
    /// little-endian means the four that matter are the first four --- so
    /// every member is "copy the first `n` of its bytes", and the only thing
    /// that differs between the widths is how far the next member is along.
    /// The table is the one place that says which members a layout has, so
    /// both the size of the step and the number of steps come from it.
    fn members(self) -> &'static [Member] {
        const INT: Member = Member::Int;
        const PTR: Member = Member::Ptr;
        match self {
            SiginfoLayout::Kill => &[INT, INT],
            SiginfoLayout::Chld => &[INT, INT, INT],
            SiginfoLayout::Fault => &[PTR],
            SiginfoLayout::Rt => &[INT, INT, PTR],
        }
    }
}

/// One member of `_sifields`: a field that is the same at both widths, or one
/// that is a pointer and so halves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Member {
    Int,
    Ptr,
}

impl Member {
    /// How many bytes it takes natively.
    const fn native_len(self) -> usize {
        match self {
            Member::Int => 4,
            Member::Ptr => 8,
        }
    }

    /// And in the 32-bit struct.
    const fn narrow_len(self) -> usize {
        4
    }
}

impl SigInfo {
    /// This `siginfo_t` as a 32-bit process reads it.
    ///
    /// 128 bytes, zeroed and then filled: the three `int`s, then the live
    /// member of the union at the offset a 4-byte-aligned union has. Anything
    /// the layout does not name stays zero, which is what Linux's
    /// `clear_user`/`__put_user` pattern leaves there and is the only honest
    /// answer for a member this kernel never fills in.
    pub fn to_i386_bytes(&self) -> [u8; SIGINFO_LEN] {
        let mut out = [0u8; SIGINFO_LEN];
        let native = self.as_bytes();
        // `si_signo`, `si_errno`, `si_code`: three `int`s, in that order, at
        // both widths.
        out[..NARROW_FIELDS_AT].copy_from_slice(&native[..NARROW_FIELDS_AT]);
        let mut from = NATIVE_FIELDS_AT;
        let mut to = NARROW_FIELDS_AT;
        for m in siginfo_layout(self.signo, self.code).members() {
            // The low four bytes of each member, which for an `int` is the
            // whole of it and for a pointer is the half that can hold an
            // address below 4 GiB.
            out[to..to + m.narrow_len()].copy_from_slice(&native[from..from + m.narrow_len()]);
            from += m.native_len();
            to += m.narrow_len();
        }
        out
    }

    /// The `siginfo_t` a 32-bit process handed the kernel.
    ///
    /// The inverse of [`SigInfo::to_i386_bytes`], and it runs the same table:
    /// the three `int`s, then each member of the live layout widened from its
    /// four bytes to the eight a native pointer takes.
    ///
    /// **A pointer widens with ZEROS, not with sign.** `si_value` and
    /// `si_addr` are `compat_uptr_t` in Linux, and `compat_ptr()` is a plain
    /// cast of an `unsigned int`: a `sigqueue(3)` that sent `0x8000_0000`
    /// means the address two gigabytes up, not the top of the address space.
    /// Sign-extending it would hand the receiver a pointer it never sent, and
    /// this is the seventh form of the same trap --- `pselect6`'s sigset
    /// pointer was the sixth.
    ///
    /// What the layout does not name stays zero. That is not a shortcut: the
    /// caller's own bytes past the live member are a union's other members,
    /// and copying them across at native offsets would put the caller's
    /// rubbish where the receiver reads a real field. Linux does the same by
    /// construction, because `copy_siginfo_from_user32` fills a zeroed
    /// `kernel_siginfo` member by member.
    pub fn from_i386_bytes(narrow: &[u8; SIGINFO_LEN]) -> Self {
        let word = |at: usize| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&narrow[at..at + 4]);
            i32::from_ne_bytes(b)
        };
        let signo = word(0);
        let errno = word(4);
        let code = SignalCode(word(8));
        let mut field = [0u8; SiginfoFields::PAD_SIZE];
        let mut from = NARROW_FIELDS_AT;
        let mut to = 0;
        for m in siginfo_layout(signo, code).members() {
            // The high half of a pointer is left as the zero it was
            // initialised to, which IS the widening.
            field[to..to + m.narrow_len()].copy_from_slice(&narrow[from..from + m.narrow_len()]);
            from += m.narrow_len();
            to += m.native_len();
        }
        SigInfo::from_parts(signo, errno, code, SiginfoFields::from_raw(field))
    }
}

#[cfg(test)]
mod tests;
