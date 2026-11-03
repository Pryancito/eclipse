//! `setsockopt`/`getsockopt`: an option value at the caller's width.
//!
//! Every other member of this family is a structure, and a structure has one
//! shape. An option value does not: `optval` is a byte buffer whose meaning
//! --- and so whose width --- depends on `optname`. `SO_REUSEADDR` is an
//! `int`, which is four bytes in both ABIs; `SO_RCVTIMEO` is a
//! `struct timeval`, which is sixteen here and **eight** to a 32-bit caller.
//! One number, two shapes, and nothing in the call says which.
//!
//! That is why these two were the last of the twenty `socketcall` numbers
//! left unwired. The conversion cannot be a property of the call, so it is a
//! property of the **option**, and this module is the table that says which.
//!
//! ## What goes wrong without it
//!
//! A 32-bit program's `setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, 8)`:
//!
//! ```text
//! the caller's eight bytes:   | tv_sec (4) | tv_usec (4) |
//! read as a native timeval:   | tv_sec ........ (8)      | tv_usec: PAST THE END
//! ```
//!
//! and the families store a sixteen-byte `timeval`, so they refuse anything
//! shorter (`data.len() < 16` is `EINVAL`, which is Linux's own check at the
//! native width). So the honest version of the bug is not a wrong timeout:
//! it is that **every** `SO_RCVTIMEO` from a 32-bit program is `EINVAL`, and
//! a program that sets a socket timeout before using the socket --- which is
//! what a timeout is for --- fails at the `setsockopt` and never gets to the
//! read. Passing sixteen instead would make the call succeed with
//! `tv_usec` read out of whatever the caller has after its eight bytes, which
//! is worse, because then it *looks* like it worked.
//!
//! The way back out is the mirror: `getsockopt(SO_RCVTIMEO)` must write
//! **eight** bytes and report eight, into a buffer the caller sized as eight.
//! A sixteen-byte answer clamped to the caller's `optlen` writes the
//! `tv_sec` of a 64-bit `timeval` --- the low half of it, little-endian, so
//! the seconds survive by luck --- and reports a `tv_usec` of whatever the
//! seconds' high half was, which is zero for any sane timeout. **So this
//! direction passes a round trip by accident whenever the timeout is under
//! four billion seconds**, which is every timeout. What it does not survive
//! is the poison test below.
//!
//! ## One table, both directions
//!
//! [`opt_shape`] is the only place that says which options are structures,
//! and both directions ask it. The numbers in it are not written here: they
//! are [`Level`] and [`SolOptname`] from `linux-object`, where the option
//! names already live, so the table cannot drift from the enum the handlers
//! match on.
//!
//! Adding an option whose value is a halved structure is a row here, and
//! both directions follow. `SO_TIMESTAMP`, `SO_TIMESTAMPNS` and
//! `SO_PEERCRED`'s `struct ucred` are the ones that would come next --- the
//! first two are a `timeval`/`timespec` and would be a row, and `ucred` is
//! three `int`s and so would not.

use super::{from_narrow_bytes, from_native_bytes, to_narrow_bytes, to_native_bytes, Narrow};
use crate::Syscall;
use alloc::borrow::Cow;
use kernel_hal::user::UserInPtr;
use linux_object::error::{LxError, LxResult};
use linux_object::net::{Level, SolOptname};
use linux_object::time::TimeVal;

/// What an option's `optval` is, as far as its width is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptShape {
    /// Bytes, an `int`, a `socklen_t`, two `int`s (`SO_LINGER`) or a string
    /// (`TCP_CONGESTION`): the same bytes to either ABI, so there is nothing
    /// to convert and the buffer is passed through untouched.
    SameWidth,
    /// A `struct timeval`: two words, sixteen bytes natively and eight to a
    /// 32-bit caller.
    Timeval,
}

/// Which options carry a structure whose width the caller's ABI decides.
///
/// Everything not named here is [`OptShape::SameWidth`], which is the right
/// default twice over: it is what the great majority of options are, and an
/// option this kernel does not implement answers `ENOPROTOOPT` further in
/// rather than being converted on a guess about its shape.
pub(crate) fn opt_shape(level: usize, optname: usize) -> OptShape {
    let timeval =
        optname == SolOptname::RCVTIMEO as usize || optname == SolOptname::SNDTIMEO as usize;
    if level == Level::SOL_SOCKET as usize && timeval {
        OptShape::Timeval
    } else {
        OptShape::SameWidth
    }
}

/// How wide a 32-bit caller's `struct timeval` is: Linux's
/// `struct old_timeval32`, two `__kernel_long_t`.
const NARROW_TIMEVAL_LEN: usize = TimeVal::WORDS * 4;

/// What `getsockopt` must do to an option value on its way out: nothing, or
/// narrow it to the caller's width.
///
/// A value rather than two arguments threaded through thirty call sites,
/// computed once from the level and the option at the top of
/// [`Syscall::sys_getsockopt`]. Every arm hands its native answer to
/// [`crate::net::write_sockopt_out`] with this alongside, so there is exactly
/// one place where the conversion happens and no arm that can forget it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OptOut {
    shape: OptShape,
    narrow: bool,
}

impl OptOut {
    /// For a value that is the same bytes in both ABIs, which is what the
    /// unit tests of the writer itself are about.
    #[cfg(test)]
    pub(crate) const SAME_WIDTH: Self = Self {
        shape: OptShape::SameWidth,
        narrow: false,
    };

    /// The native answer as the caller should see it.
    ///
    /// Borrowed when there is nothing to do, which is every option but two,
    /// so the common path copies nothing.
    pub(crate) fn apply<'a>(self, native: &'a [u8]) -> Cow<'a, [u8]> {
        match self.shape {
            OptShape::SameWidth => Cow::Borrowed(native),
            OptShape::Timeval if !self.narrow => Cow::Borrowed(native),
            OptShape::Timeval => {
                // A short native value is not this layer's to repair: hand it
                // on and let the length clamp do what it does natively. The
                // families all answer sixteen bytes.
                if native.len() < TimeVal::WORDS * size_of::<usize>() {
                    return Cow::Borrowed(native);
                }
                let tv: TimeVal = from_native_bytes(native);
                let (bytes, len) = to_narrow_bytes(tv);
                Cow::Owned(bytes[..len].to_vec())
            }
        }
    }
}

impl Syscall<'_> {
    /// The option value a `setsockopt` really means, in the native layout the
    /// socket families store, whatever width the caller wrote it at.
    ///
    /// Linux's `sock_set_timeout`, which reads an `old_timeval32` and widens
    /// it when `in_compat_syscall()`. Asked of the **process**, like every
    /// other question in this module's parent.
    pub(crate) fn sockopt_in(
        &self,
        level: usize,
        optname: usize,
        optval: UserInPtr<u8>,
        optlen: usize,
    ) -> LxResult<Cow<'static, [u8]>> {
        if !self.narrow_abi() || opt_shape(level, optname) == OptShape::SameWidth {
            return Ok(Cow::Borrowed(optval.as_slice(optlen)?));
        }
        // `sock_set_timeout` refuses a buffer shorter than the structure it
        // is about to read, at whichever width that is. Eight here, so a
        // 32-bit caller that passes a four-byte `int` by mistake gets the
        // same `EINVAL` Linux gives it and not a read of four bytes it does
        // not own.
        if optlen < NARROW_TIMEVAL_LEN {
            return Err(LxError::EINVAL);
        }
        let raw = optval.read_array(NARROW_TIMEVAL_LEN)?;
        let tv: TimeVal = from_narrow_bytes(&raw);
        let (bytes, len) = to_native_bytes(tv);
        Ok(Cow::Owned(bytes[..len].to_vec()))
    }

    /// The conversion `getsockopt` owes this option, computed once.
    pub(crate) fn sockopt_out(&self, level: usize, optname: usize) -> OptOut {
        OptOut {
            shape: opt_shape(level, optname),
            narrow: self.narrow_abi(),
        }
    }
}

#[cfg(test)]
mod tests;
