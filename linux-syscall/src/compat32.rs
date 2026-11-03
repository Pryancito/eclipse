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
//! name listed in one of two allowlists. This is Linux's
//! `ia32_sys_call_table`, where each entry is either the plain `sys_*` or a
//! `compat_sys_*` wrapper that converts something -- except that here the
//! conversion lives in the handler rather than in a wrapper, so the two
//! cases are two files rather than two kinds of entry:
//!
//! - [`i386_abi_same.in`](../i386_abi_same.in): the calls whose ABI is
//!   byte-for-byte the native one. Scalars, paths, byte buffers.
//! - [`i386_abi_narrow.in`](../i386_abi_narrow.in): the calls whose
//!   registers are already native but whose **structures** the handler
//!   converts, because it asks the calling process how wide a word is
//!   ([`crate::narrow`], Linux's `in_compat_syscall()`). `struct timespec`,
//!   `timeval`, `itimerval`, `itimerspec`, `tms`, `rusage` and the entries
//!   of a `struct iovec` are all the native layout with every word halved,
//!   and once a call passes nothing else, translating the number is again
//!   all there is to do.
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
//! - `socketcall` (102) and `ipc` (117) multiplex a whole family through one
//!   number, with the real arguments in a block of memory;
//! - `sigreturn` (119) and `rt_sigreturn` (173), whose signal frames are
//!   32-bit structures;
//! - the `*_time64` calls (403 and up), the 32-bit world's answer to 2038.
//!
//! The second is the calls that share a name but not a shape and whose shape
//! nothing converts yet: anything passing a `msghdr`, an `rlimit`, a
//! `sigaction`, a `siginfo_t`, a `sigevent`, or an **array** of the
//! structures [`crate::narrow`] converts one at a time (`utimensat`,
//! `utimes`); `pselect6`, whose timeout is converted but whose sixth
//! argument points at two words; `lseek`, whose offset is signed where these
//! registers are read zero-extended; `clone`, whose last two arguments are
//! swapped on i386; and `execve`, whose `argv` and `envp` are arrays of
//! 4-byte pointers.
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
//! The next are the ones whose native twin exists but reads the arguments
//! differently: a 64-bit offset arrives in two registers (`_llseek`,
//! `pread64`, `pwrite64`, `truncate64`, `ftruncate64`) or in 4 KiB units
//! (`mmap2`). Each is two halves put back together, and each is in a
//! different order. See [`wide`](wide).
//!
//! And then the ones whose answer is a **structure** of a shape this ABI does
//! not share, and which is not simply the native one halved --- so
//! [`crate::narrow`] does not reach them: `stat64` (195), `lstat64` (196),
//! `fstat64` (197) and
//! `fstatat64` (300), whose 96 bytes keep the real inode last and every
//! 64-bit field at 4-byte alignment (see [`stat`](stat)); and `fcntl64` (221)
//! with `fcntl` (55), six of whose commands point at one of i386's two
//! `struct flock` --- which one being decided by the command number (see
//! [`fcntl`](fcntl)); and `setrlimit` (75), `getrlimit` (76) and
//! `ugetrlimit` (191), whose two words are **clamped** rather than
//! truncated, at a different number for each of the two getters, and whose
//! `0xffffffff` means "no limit" rather than the number it spells (see
//! [`rlimit`](rlimit)).
//!
//! ## The renames
//!
//! A third group needs neither: the call is the same call under another name.
//! i386 grew up with 16-bit uids, so each credential call has a `*32` number
//! for the real thing, and x86_64 --- which never had a 16-bit anything ---
//! has only the plain name. Those are in the allowlist as
//! `getuid32 = getuid`, and the names they replaced must stay out of it: the
//! ABI of the pair is identical, the ABI of the ancestor is not.
//!
//! Every remaining one answers `ENOSYS` **with its own name** in the log, which
//! is a far better report than a number:
//! `sys_compat32: i386 syscall 192 (mmap2) is not implemented` says exactly
//! what to write next. [`I386_NAMES`] carries all 451 numbers for that, mapped
//! or not.

use super::*;
use linux_object::error::LxError;

mod fcntl;
mod ioctl;
mod iovec;
mod rlimit;
mod socketcall;
mod stat;
mod tls;
mod wide;

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

/// And the ones whose native twin exists but takes its 64-bit offset in one
/// register where i386 passes it in two. See [`wide`].
const I386_LLSEEK: u32 = 140;
const I386_PREAD64: u32 = 180;
const I386_PWRITE64: u32 = 181;
const I386_MMAP2: u32 = 192;
const I386_TRUNCATE64: u32 = 193;
const I386_FTRUNCATE64: u32 = 194;

/// And the ones whose answer is a structure this ABI lays out differently.
/// See [`stat`] and [`fcntl`].
const I386_FCNTL: u32 = 55;
const I386_STAT64: u32 = 195;
const I386_LSTAT64: u32 = 196;
const I386_FSTAT64: u32 = 197;
const I386_FCNTL64: u32 = 221;
const I386_FSTATAT64: u32 = 300;

/// And the `struct rlimit` calls, whose narrow form is not a truncation but
/// a clamp --- and whose two getters clamp at different numbers. See
/// [`rlimit`].
const I386_SETRLIMIT: u32 = 75;
const I386_OLD_GETRLIMIT: u32 = 76;
const I386_UGETRLIMIT: u32 = 191;

/// `socketcall`, the one number behind which i386 keeps all twenty socket
/// calls. See [`socketcall`].
const I386_SOCKETCALL: u32 = 102;

/// And `ioctl`, which is in neither allowlist because the thing to vouch for
/// is not the call but the **request**. See [`ioctl`].
const I386_IOCTL: u32 = 54;

/// And the scatter/gather calls whose offset arrives split across two
/// registers. `readv` (145) and `writev` (146) are not here: their only
/// difference is the width of a `struct iovec` entry, which
/// [`Syscall::read_iovecs`](crate::Syscall::read_iovecs) asks the process
/// about, so they are in `i386_abi_narrow.in` with the rest of the calls the
/// handler converts itself. See [`iovec`].
const I386_PREADV: u32 = 333;
const I386_PWRITEV: u32 = 334;

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
        let [a0, a1, a2, a3, a4, a5] = args;
        match num {
            I386_SET_THREAD_AREA => return syscall_ret(self.sys_set_thread_area(a0)),
            I386_GET_THREAD_AREA => return syscall_ret(self.sys_get_thread_area(a0)),
            I386_LLSEEK => return syscall_ret(self.sys_llseek(a0, a1, a2, a3, a4)),
            I386_PREAD64 => return syscall_ret(self.sys_pread64(a0, a1, a2, a3, a4).await),
            I386_PWRITE64 => return syscall_ret(self.sys_pwrite64(a0, a1, a2, a3, a4)),
            I386_MMAP2 => {
                return syscall_ret(self.sys_mmap2(a0, a1, a2, a3, a4, a5).await);
            }
            I386_TRUNCATE64 => return syscall_ret(self.sys_truncate64(a0, a1, a2)),
            I386_FTRUNCATE64 => return syscall_ret(self.sys_ftruncate64(a0, a1, a2)),
            I386_STAT64 => return syscall_ret(self.sys_stat64(a0, a1)),
            I386_LSTAT64 => return syscall_ret(self.sys_lstat64(a0, a1)),
            I386_FSTAT64 => return syscall_ret(self.sys_fstat64(a0, a1)),
            I386_FSTATAT64 => return syscall_ret(self.sys_fstatat64(a0, a1, a2, a3)),
            // The same handler for both numbers, because measuring Linux
            // says they are the same call: see [`fcntl`].
            I386_FCNTL64 | I386_FCNTL => return syscall_ret(self.sys_fcntl64(a0, a1, a2).await),
            I386_SETRLIMIT => return syscall_ret(self.sys_setrlimit32(a0, a1)),
            I386_OLD_GETRLIMIT => return syscall_ret(self.sys_old_getrlimit32(a0, a1)),
            I386_UGETRLIMIT => return syscall_ret(self.sys_ugetrlimit(a0, a1)),
            // `readv` (145) and `writev` (146) are not here: see
            // `i386_abi_narrow.in`. These two are, because the offset
            // arrives in two registers. See [`iovec`].
            // `socketcall` multiplexes twenty calls through one number,
            // with the real arguments in a block of memory. See
            // [`socketcall`].
            I386_SOCKETCALL => return syscall_ret(self.sys_socketcall(a0, a1).await),
            // One number, a thousand requests: vouched one request at a
            // time. See [`ioctl`].
            I386_IOCTL => {
                return syscall_ret(self.sys_ioctl32(a0, a1 as u32, a2, a3, a4).await);
            }
            I386_PREADV => return syscall_ret(self.sys_preadv32(a0, a1, a2, a3, a4).await),
            I386_PWRITEV => return syscall_ret(self.sys_pwritev32(a0, a1, a2, a3, a4)),
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

#[cfg(test)]
mod unaligned_tests;
