//! `socketcall(2)`, i386 102: twenty socket calls behind one number.
//!
//! i386 never got the sixteen separate socket syscalls x86_64 has. It has
//! one, whose first argument names which of them is meant and whose second
//! points at a block of **unsigned long** holding the real arguments --- four
//! bytes each, where this kernel's registers are eight.
//!
//! So there are two conversions in one call, and the second is the one with
//! teeth. The arguments are not in registers at all: they are in the
//! caller's memory, and how many of them there are depends on `call`. Read
//! one word too many and the call answers `EFAULT` for a block that ends at
//! a page boundary --- which is exactly what Linux's `nargs[]` table exists
//! to avoid.
//!
//! ## The table, measured
//!
//! Not read off a header. An i386 program maps two pages, drops the second,
//! and for `K` from 1 up puts the first `K` words of a template that would
//! make the call *succeed* at `page_end - K*4`, so that word `K+1` is
//! unmapped. The smallest `K` that does not answer `EFAULT` is the number of
//! words the kernel asked for:
//!
//! | call | name | words | what it answered at that K |
//! |---|---|---|---|
//! | 1 | `socket` | 3 | 5 (a descriptor) |
//! | 2 | `bind` | 3 | EINVAL |
//! | 3 | `connect` | 3 | EINVAL |
//! | 4 | `listen` | 2 | EINVAL |
//! | 5 | `accept` | 3 | EINVAL |
//! | 6 | `getsockname` | 3 | 0 |
//! | 7 | `getpeername` | 3 | 0 |
//! | 8 | `socketpair` | 4 | 0 |
//! | 9 | `send` | 4 | 1 (a byte sent) |
//! | 10 | `recv` | 4 | EAGAIN |
//! | 11 | `sendto` | 6 | 1 |
//! | 12 | `recvfrom` | 6 | EAGAIN |
//! | 13 | `shutdown` | 2 | 0 |
//! | 14 | `setsockopt` | 5 | 0 |
//! | 15 | `getsockopt` | 5 | 0 |
//! | 16 | `sendmsg` | 3 | 0 |
//! | 17 | `recvmsg` | 3 | 0 |
//! | 18 | `accept4` | 4 | EINVAL |
//! | 19 | `recvmmsg` | 5 | 0 |
//! | 20 | `sendmmsg` | 4 | 0 |
//!
//! The answers are in the table on purpose: they are what says the call
//! really ran at that `K` rather than failing earlier for its own reasons.
//! The first attempt at this measurement had no templates, so every call
//! with an out-parameter answered `EFAULT` at every `K` --- from the null
//! pointer in the block, not from the block --- and `recv` on a descriptor
//! that was not a socket blocked for ever.
//!
//! ## Which twelve are wired, and why the other eight are not
//!
//! Wired, because every argument is a scalar, a byte buffer, a `socklen_t`
//! (a `u32` in both ABIs) or a `struct sockaddr` (a `u16` family and bytes,
//! with no pointer and no `long` in it, so the same in both):
//!
//! `socket`, `bind`, `connect`, `listen`, `accept`, `getsockname`,
//! `getpeername`, `socketpair`, `send`, `recv`, `sendto`, `recvfrom`,
//! `shutdown`. (`send` and `recv` have no native twin --- x86_64 never had
//! those two syscalls --- so they go to `sendto` and `recvfrom` with a null
//! address, which is what Linux's `__sys_sendto` does with them too.)
//!
//! Left out, each for a reason that is about a structure and not about this
//! file:
//!
//! - **`setsockopt`(14) and `getsockopt`(15)**: their `optval` is a byte
//!   buffer for most options and a **structure** for some, and which one it
//!   is depends on `optname`. `SO_RCVTIMEO` and `SO_SNDTIMEO` take a
//!   `struct timeval`, which is half the width here (see [`crate::narrow`]),
//!   so wiring these two would quietly read sixteen bytes of a caller's
//!   eight-byte timeout. They need a per-option table, which is its own
//!   piece of work.
//!
//! And **`sendmsg`(16), `recvmsg`(17), `recvmmsg`(19) and `sendmmsg`(20)** are
//! wired, for the same reason `readv` and `writev` are: the conversion lives
//! in the handler, which asks the process how wide a word is, so the number
//! is all there is left to translate. The envelope is 28 bytes against 56,
//! the `struct cmsghdr` inside it is 12 aligned to 4 against 16 aligned to 8,
//! and an `mmsghdr` array has a stride of 32 with its count at 28 against 64
//! with its count at 56. [`crate::narrow::msghdr`] has all three layouts and
//! why none of them can be read at the wrong width without inventing a
//! pointer.
//!
//! One gap, and it is not an ABI one: `recvmmsg`'s fifth word is a
//! `struct timespec *` timeout, and [`Syscall::sys_recvmmsg`] does not honour
//! a timeout in **either** ABI --- it forces every receive after the first
//! non-blocking and returns when the queue drains. So the narrow caller is
//! no worse off than the native one, and the five words are still read (the
//! measured `nargs`), which is what keeps a block that ends at a page from
//! answering `EFAULT`. Wiring the timeout is one piece of work for both
//! ABIs, with the conversion already written ([`crate::narrow`]).
//!
//! An unwired call answers `ENOSYS`, like every other name this layer has
//! not vouched for, rather than being passed through on the guess that its
//! arguments happen to line up.

use super::*;
use kernel_hal::user::UserInPtr;

/// How many 32-bit words each call's argument block holds, indexed by the
/// call number. Measured; see the table in the module documentation.
///
/// Index 0 is unused: there is no call 0, and Linux's own table has the same
/// hole.
const NARGS: [u8; 21] = [
    0, // -
    3, // 1 socket
    3, // 2 bind
    3, // 3 connect
    2, // 4 listen
    3, // 5 accept
    3, // 6 getsockname
    3, // 7 getpeername
    4, // 8 socketpair
    4, // 9 send
    4, // 10 recv
    6, // 11 sendto
    6, // 12 recvfrom
    2, // 13 shutdown
    5, // 14 setsockopt
    5, // 15 getsockopt
    3, // 16 sendmsg
    3, // 17 recvmsg
    4, // 18 accept4
    5, // 19 recvmmsg
    4, // 20 sendmmsg
];

/// The highest call number there is.
const MAX_CALL: usize = 20;

/// The size of one word of the argument block.
const WORD: usize = 4;

/// The most words any call takes, which is `sendto`'s and `recvfrom`'s six.
const MAX_WORDS: usize = 6;

/// The names, for the log line when a call is not wired. Worth having
/// because `socketcall(17, ...)` says nothing to whoever reads the warning.
pub(crate) fn call_name(call: usize) -> &'static str {
    match call {
        1 => "socket",
        2 => "bind",
        3 => "connect",
        4 => "listen",
        5 => "accept",
        6 => "getsockname",
        7 => "getpeername",
        8 => "socketpair",
        9 => "send",
        10 => "recv",
        11 => "sendto",
        12 => "recvfrom",
        13 => "shutdown",
        14 => "setsockopt",
        15 => "getsockopt",
        16 => "sendmsg",
        17 => "recvmsg",
        18 => "accept4",
        19 => "recvmmsg",
        20 => "sendmmsg",
        _ => "unknown",
    }
}

/// How many words this call's block holds, or `EINVAL` for a call number
/// that does not exist --- which is what Linux answers, because its own
/// bounds check against the table is the thing that decides.
fn words_of(call: usize) -> LxResult<usize> {
    if call == 0 || call > MAX_CALL {
        return Err(LxError::EINVAL);
    }
    Ok(NARGS[call] as usize)
}

/// Read the argument block: `words` 32-bit words, each zero-extended.
///
/// Through bytes, not `UserInPtr<u32>::read_array`, for the reason in
/// [`crate::narrow`]: the block is 4-aligned for a 32-bit caller and a typed
/// read of a wider type would refuse an address Linux accepts. Here the
/// element is a `u32`, whose alignment is 4 anyway, but going through bytes
/// is what keeps that true if the slot type ever changes --- and it is the
/// same shape as every other narrow read in this layer.
///
/// Each word is zero-extended and not sign-extended: these are an
/// `unsigned long` each, holding a descriptor, a length, a flag word or a
/// 32-bit pointer. A pointer in the low 4 GiB is an address, never a
/// negative offset from the top of the address space. This is Linux's
/// `compat_ptr`.
fn read_args(at: usize, words: usize) -> LxResult<[usize; MAX_WORDS]> {
    if at == 0 {
        return Err(LxError::EFAULT);
    }
    let ptr: UserInPtr<u8> = at.into();
    let bytes = ptr.read_array(words * WORD)?;
    let mut out = [0usize; MAX_WORDS];
    let (slots, _) = bytes.as_chunks::<WORD>();
    for (slot, word) in slots.iter().zip(out.iter_mut()) {
        *word = u32::from_le_bytes(*slot) as usize;
    }
    Ok(out)
}

impl Syscall<'_> {
    /// `socketcall(call, args)`, i386 102.
    ///
    /// Reads the block and hands the words to the native handler, which does
    /// not know a second ABI exists. The eight calls this layer has not
    /// vouched for answer `ENOSYS`; see the module documentation for which
    /// and why.
    pub async fn sys_socketcall(&mut self, call: usize, args: usize) -> SysResult {
        let words = words_of(call)?;
        let a = read_args(args, words)?;
        info!(
            "socketcall: {} ({}), {} word(s): {:x?}",
            call,
            call_name(call),
            words,
            &a[..words]
        );
        match call {
            1 => self.sys_socket(a[0], a[1], a[2]),
            2 => self.sys_bind(a[0], a[1].into(), a[2]).await,
            3 => self.sys_connect(a[0], a[1].into(), a[2]).await,
            4 => self.sys_listen(a[0], a[1]),
            // `accept` is `accept4` with no flags, which is how Linux spells
            // it too (`__sys_accept4(..., 0)`).
            5 => self.sys_accept4(a[0], a[1].into(), a[2].into(), 0).await,
            6 => self.sys_getsockname(a[0], a[1].into(), a[2].into()),
            7 => self.sys_getpeername(a[0], a[1].into(), a[2].into()),
            8 => self.sys_socketpair(a[0], a[1], a[2], a[3].into()),
            // `send` and `recv` have no native twin: x86_64 never had those
            // two syscalls, so musl spells them with a null address through
            // `sendto` and `recvfrom`, and so does Linux internally.
            9 => {
                self.sys_sendto(a[0], a[1].into(), a[2], a[3], 0.into(), 0)
                    .await
            }
            10 => {
                self.sys_recvfrom(a[0], a[1].into(), a[2], a[3], 0.into(), 0.into())
                    .await
            }
            11 => {
                self.sys_sendto(a[0], a[1].into(), a[2], a[3], a[4].into(), a[5])
                    .await
            }
            12 => {
                self.sys_recvfrom(a[0], a[1].into(), a[2], a[3], a[4].into(), a[5].into())
                    .await
            }
            13 => self.sys_shutdown(a[0], a[1]),
            // The envelope and the control messages inside it are converted
            // by asking the process its width ([`crate::narrow::msghdr`]), so these
            // two are the native handlers unchanged, like `readv`/`writev`.
            16 => self.sys_sendmsg(a[0], a[1].into(), a[2]).await,
            17 => self.sys_recvmsg(a[0], a[1].into(), a[2]).await,
            // The batch pair, whose array has a stride of its own: 32 bytes
            // for a 32-bit caller against 64, with the count at 28 and not
            // at 56. Asked of the process, like the envelope.
            // Word 5 of `recvmmsg` is a `timespec *` timeout, which
            // `sys_recvmmsg` does not honour in either ABI; see the module
            // documentation. It is still read, so a block that ends at a
            // page does not answer EFAULT.
            19 => self.sys_recvmmsg(a[0], a[1].into(), a[2], a[3]).await,
            20 => self.sys_sendmmsg(a[0], a[1].into(), a[2], a[3]).await,
            18 => self.sys_accept4(a[0], a[1].into(), a[2].into(), a[3]).await,
            _ => {
                warn!(
                    "socketcall: {} ({}) passes a structure this layer does not convert yet",
                    call,
                    call_name(call)
                );
                Err(LxError::ENOSYS)
            }
        }
    }
}

#[cfg(test)]
mod tests;
