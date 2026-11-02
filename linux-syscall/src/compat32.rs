//! The 32-bit (i386) system-call entry: Linux's `CONFIG_COMPAT`.
//!
//! An `ELFCLASS32` program runs on this 64-bit kernel in the CPU's
//! compatibility mode (see `linux_object::process::Abi::Linux32`). It reaches
//! the kernel through `int $0x80` rather than `syscall`, and it speaks the
//! i386 ABI on the way in:
//!
//! | | native (x86_64) | compat (i386) |
//! |---|---|---|
//! | number | `rax` | `eax` |
//! | arguments | `rdi rsi rdx r10 r8 r9` | `ebx ecx edx esi edi ebp` |
//! | numbering | `syscall.h.in` | [`i386_syscall.h.in`](../i386_syscall.h.in) |
//!
//! The numbering is the part with teeth: i386 `write` is 4 where x86_64 `write`
//! is 1, and i386 4 means `stat` natively. So the number has to be translated
//! before anything looks at it, and [`I386_TO_NATIVE`] is that translation.
//!
//! ## What is mapped, and why so little
//!
//! [`I386_TO_NATIVE`] is **default-deny**: `build.rs` emits a row only for a
//! name listed in [`i386_abi_same.in`](../i386_abi_same.in), the calls whose
//! ABI is byte-for-byte the native one. This is the allowlist half of Linux's
//! `ia32_sys_call_table`, where each entry is either the plain `sys_*` -- these
//! -- or a `compat_sys_*` wrapper that converts something.
//!
//! Matching the two tables by name alone would be unsound, and quietly so.
//! `gettimeofday` has that name in both and passes a `struct timeval` of two
//! longs: 8 bytes written by the 32-bit caller, 16 bytes read by the native
//! handler. Nothing reports that. So a name earns a row only once somebody has
//! read its arguments, and everything else answers `ENOSYS`.
//!
//! What is left out falls into two groups. The first is the calls i386 has to
//! itself, which have no native twin to match:
//!
//! - `mmap2` (192) takes its offset in pages, not bytes;
//! - `socketcall` (102) and `ipc` (117) multiplex a whole family through one
//!   number, with the real arguments in a block of memory;
//! - `fstat64`/`stat64`/`lstat64`/`fstatat64` pass a `struct stat64` whose
//!   layout is not `struct stat`'s;
//! - `_llseek`, `pread64`, `truncate64` and friends split a 64-bit offset
//!   across two registers;
//! - `fcntl64`, and the `*32` credential calls (`getuid32`, `setresuid32`,
//!   ...), which exist because the originals were 16-bit;
//! - `sigreturn` (119) and `rt_sigreturn` (173), whose signal frames are
//!   32-bit structures;
//! - the `*_time64` calls (403 and up), the 32-bit world's answer to 2038.
//!
//! The second is the calls that share a name but not a shape: anything passing
//! a `stat`, `timespec`, `timeval`, `rusage`, `iovec`, `msghdr`, `rlimit` or
//! `sigaction`; `lseek`, whose offset is signed where these registers are read
//! zero-extended; `clone`, whose last two arguments are swapped on i386; and
//! `execve`, whose `argv` and `envp` are arrays of 4-byte pointers.
//!
//! ## And the ones with no native twin at all
//!
//! A number can be missing from the allowlist for a second reason: not that
//! its ABI differs from a native call's, but that there is no native call. For
//! those the kernel does the work itself, and the dispatch below answers them
//! before it asks the table --- which would say `ENOSYS` for a call that is
//! right here.
//!
//! `set_thread_area` (243) and `get_thread_area` (244) are the first:
//! x86_64 keeps a thread pointer in an MSR and i386 keeps it in a segment
//! descriptor, so the two kernels genuinely do different work. See
//! [`tls`](tls).
//!
//! Every remaining one answers `ENOSYS` **with its own name** in the log, which
//! is a far better report than a number:
//! `sys_compat32: i386 syscall 192 (mmap2) is not implemented` says exactly
//! what to write next. [`I386_NAMES`] carries all 451 numbers for that, mapped
//! or not.

use super::*;
use linux_object::error::LxError;

mod tls;

/// The i386 numbers this kernel answers **itself**, because there is no native
/// call to translate them into.
///
/// A number here is deliberately absent from `i386_abi_same.in`: it is not
/// that its ABI differs from a native twin's, it is that it has no twin at
/// all. `set_thread_area` is the first: x86_64 keeps a thread pointer in an
/// MSR and i386 keeps it in a segment descriptor, so the two kernels do
/// genuinely different work. See [`tls`].
const I386_SET_THREAD_AREA: u32 = 243;
const I386_GET_THREAD_AREA: u32 = 244;

mod table {
    include!(concat!(env!("OUT_DIR"), "/compat32.rs"));
}

pub use table::{I386_NAMES, I386_TO_NATIVE};

/// The native system-call number an i386 one means, when the two tables agree
/// on the name.
///
/// `None` is not "no such call": it is "this number is not one a native
/// handler can be given as it stands", which covers both an i386-only call
/// (see the [module documentation](self)) and a number that is in no table at
/// all.
pub fn native_number(i386: u32) -> Option<u32> {
    I386_TO_NATIVE
        .binary_search_by_key(&i386, |&(n, _)| n)
        .ok()
        .map(|at| I386_TO_NATIVE[at].1)
}

/// What i386 calls the system call numbered `i386`, whether this kernel
/// implements it or not.
pub fn name(i386: u32) -> Option<&'static str> {
    I386_NAMES
        .binary_search_by_key(&i386, |&(n, _)| n)
        .ok()
        .map(|at| I386_NAMES[at].1)
}

impl Syscall<'_> {
    /// Run one system call made by a 32-bit process: `num` and `args` as the
    /// i386 ABI puts them, read out of the trap frame by the caller.
    ///
    /// Translating the number is all this does, and it only does it for a call
    /// the allowlist vouches for: one whose arguments are scalars, paths, byte
    /// buffers or structures that are laid out the same in both ABIs. Those
    /// cover everything a program does before it has done anything --
    /// `write`, `exit_group`, `brk`, `getpid`, `open`, `close` -- and nothing
    /// that passes a `struct stat`, a `timespec`, an `iovec` or a signal
    /// frame. See the [module documentation](self) for why the list is an
    /// allowlist.
    pub async fn compat32_syscall(&mut self, num: u32, args: [usize; 6]) -> isize {
        // The i386-only calls this kernel implements come first: they are not
        // in the translation table on purpose, so asking it would answer
        // `ENOSYS` for a call that is right here.
        match num {
            I386_SET_THREAD_AREA => return syscall_ret(self.sys_set_thread_area(args[0])),
            I386_GET_THREAD_AREA => return syscall_ret(self.sys_get_thread_area(args[0])),
            _ => {}
        }
        match native_number(num) {
            Some(native) => {
                trace!(
                    "compat32 syscall: {} ({}) -> native {} {:x?}",
                    num,
                    name(num).unwrap_or("?"),
                    native,
                    args
                );
                self.syscall(native, args).await
            }
            None => {
                warn!(
                    "sys_compat32: i386 syscall {} ({}) is not implemented",
                    num,
                    name(num).unwrap_or("unknown"),
                );
                syscall_ret(Err(LxError::ENOSYS))
            }
        }
    }
}

#[cfg(test)]
mod tests;
