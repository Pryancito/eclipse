//! Linux syscall implementations
//!
//! ## Example
//! The syscall is called like this in the [`zcore_loader`](../zcore_loader/index.html):
//! ```ignore
//! let num = regs.rax as u32;
//! let args = [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9];
//! let mut syscall = Syscall {
//!     thread,
//!     thread_fn,
//!     syscall_entry: kernel_hal::context::syscall_entry as usize,
//! };
//! let ret = syscall.syscall(num, args).await;
//! ```
//!

#![no_std]
// `#[bench]` and `test::Bencher` are libtest's, and still unstable. The
// benches live in `#[cfg(test)]` modules beside the code they measure --
// which is the only place they can live, because every submodule of this
// crate is private, so a `benches/` target would not see a single syscall
// helper. Gated on `test` so a kernel build never asks for the feature.
#![cfg_attr(test, feature(test))]
#![deny(warnings, unsafe_code, missing_docs)]
#![allow(clippy::upper_case_acronyms)]

#[cfg(test)]
extern crate test;

#[macro_use]
extern crate alloc;

#[macro_use]
extern crate log;

use alloc::sync::Arc;
use core::convert::TryFrom;

use kernel_hal::user::{IoVecIn, IoVecOut, UserInOutPtr, UserInPtr, UserOutPtr};
use linux_object::error::{LxError, LxResult, SysResult};
use linux_object::fs::FileDesc;
use linux_object::process::{LinuxProcess, ProcessExt, RLimit};
use zircon_object::object::{KernelObject, KoID};
use zircon_object::task::{CurrentThread, Process, ThreadFn};
use zircon_object::vm::VirtAddr;

use self::consts::SyscallType as Sys;
use self::file::poll_timeout_msecs;
use self::file::{XattrOp, XattrTarget};

mod consts {
    // generated from syscall.h.in
    include!(concat!(env!("OUT_DIR"), "/consts.rs"));
}

/// Glue for Eclipse's own perf accounting (`/proc/perf`, `/proc/<pid>/perf`).
///
/// Resolves a syscall number to its name via the generated [`Sys`] enum and
/// registers that resolver with `linux-object` (which owns the `/proc` files
/// but not the syscall table) the first time a syscall runs.
mod perf_accounting {
    use super::Sys;
    use alloc::string::{String, ToString};
    use core::convert::TryFrom;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn resolve(num: u32) -> Option<String> {
        // `Sys` is `#[derive(Debug)]`; its variant name is the uppercase
        // syscall name. Lower-case it to match the conventional spelling.
        Sys::try_from(num)
            .ok()
            .map(|s| alloc::format!("{:?}", s).to_lowercase())
            .or_else(|| Some(num.to_string()))
    }

    /// Register the name resolver exactly once.
    pub fn ensure_registered() {
        static DONE: AtomicBool = AtomicBool::new(false);
        // Plain-load fast path: this runs on every syscall, and after the
        // first one the `lock cmpxchg` below is pure cacheline traffic.
        if DONE.load(Ordering::Acquire) {
            return;
        }
        if DONE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            linux_object::perf::set_name_resolver(resolve);
        }
    }
}
/// Max kernel buffer for one read/write/recv/send syscall. Bare-metal zCore uses
/// a fixed kernel heap; large transient allocations (e.g. 1 MiB recv buffers)
/// exhaust or fragment it after long network sessions.
pub(crate) const SYSCALL_IO_MAX: usize = 64 * 1024;

/// Whether the bytes past what this kernel knows of an extensible struct are
/// all zero.
///
/// This is `copy_struct_from_user`'s rule, and the reason it exists: a caller
/// that sets a field this kernel has never heard of is asking for a feature
/// it will not get, so the answer is `E2BIG` and not a silent success.
///
/// Four syscalls here take such a struct with a `size` beside it saying which
/// version of it the caller built, and **none of them looked** past the
/// fields it knew. `clone3` read the first 64 bytes and dropped the rest, so
/// a `clone3` asking for `set_tid` (which is how CRIU restores a process onto
/// its old pid) or for `CLONE_INTO_CGROUP` got a child that was neither and
/// was told it worked. Each syscall keeps its own bounds check next to itself,
/// the way Linux does (`copy_clone_args_from_user`, `sched_copy_attr`,
/// `perf_copy_attr`); this is the part they share.
pub(crate) fn extensible_tail_is_empty(tail: &[u8]) -> bool {
    tail.iter().all(|&b| b == 0)
}

/// A zeroed kernel buffer of `n` bytes that answers `ENOMEM` instead of
/// panicking the machine.
///
/// `vec![0u8; n]` is an infallible allocation: when the fixed kernel heap
/// cannot satisfy it, Rust calls `alloc_error`, which panics — so one
/// process's ordinary read took the whole kernel down:
///
///     [PANIC] cpu=10 ... memory allocation of 24576 bytes failed
///       <linux_syscall::Syscall>::sys_read::{closure#0}
///
/// A user-sized allocation must never be able to do that; Linux returns
/// ENOMEM for exactly this case. `try_reserve_exact` fails instead of
/// aborting, and the `resize` that follows cannot reallocate because the
/// capacity is already there.
///
/// **Every allocation in this crate whose size comes from userspace comes
/// through here**, and for a while only `sys_read`, `sys_pread` and `splice`
/// did: `readv`, `writev`, `preadv` and `pwritev` built the same buffer with
/// `vec![0u8; n]`, `getdents64` built one four times the size of the read
/// ceiling (up to 256 KiB, its own cap), and `readlinkat` and `syslog` did
/// too. The panic above is at 24 576 bytes, well under any of those caps, so
/// clamping the length is not what keeps the machine up -- asking for the
/// memory fallibly is. `sys_readv`'s buffer even carries the comment
/// "Mirror the sys_read hybrid buffer": it mirrored the stack/heap split and
/// not the part that cannot panic.
pub(crate) fn try_zeroed_buf(n: usize) -> linux_object::error::LxResult<alloc::vec::Vec<u8>> {
    let mut buf = alloc::vec::Vec::new();
    buf.try_reserve_exact(n).map_err(|_| LxError::ENOMEM)?;
    buf.resize(n, 0);
    Ok(buf)
}

#[cfg(test)]
mod zeroed_buf_tests {
    use super::*;

    #[test]
    fn a_buffer_the_heap_cannot_hold_is_enomem_and_not_a_panic() {
        // The whole point: a length that no allocator can satisfy comes back
        // as an error the syscall can return, not as `alloc_error` taking the
        // machine with it. `vec![0u8; n]` here would abort the test process.
        assert_eq!(try_zeroed_buf(usize::MAX).err(), Some(LxError::ENOMEM));
    }

    #[test]
    fn a_buffer_that_fits_is_n_zeroed_bytes_and_no_more() {
        let buf = try_zeroed_buf(4096).expect("4 KiB fits");
        assert_eq!(buf.len(), 4096);
        assert!(buf.iter().all(|&b| b == 0));
        // `try_reserve_exact` then `resize`: the resize must not have to
        // reallocate, because a reallocation is the infallible path again.
        assert_eq!(buf.capacity(), 4096);
    }

    #[test]
    fn a_zero_length_buffer_is_empty_and_not_an_error() {
        // `read(fd, buf, 0)` and `getdents64` with a zero-sized buffer reach
        // here; both are legal calls that answer 0, not ENOMEM.
        assert_eq!(try_zeroed_buf(0).expect("zero is fine").len(), 0);
    }
}

#[cfg(test)]
mod abi;
/// FreeBSD/amd64 system-call personality (ELF `ELFOSABI_FREEBSD` binaries).
///
/// Translates FreeBSD syscalls onto the Linux implementation in this crate and
/// re-encodes results the FreeBSD way (carry-flag errors, `rdx` secondary
/// return). amd64-only: the ABI it implements is FreeBSD/amd64.
#[cfg(target_arch = "x86_64")]
pub mod bsd;
mod file;
mod intarg;
mod ipc;
mod misc;
mod net;
mod outparams;
mod signal;
mod task;
mod time;
mod vm;

/// The struct of Syscall which stores the information about making a syscall
pub struct Syscall<'a> {
    /// the thread making a syscall
    pub thread: &'a CurrentThread,
    /// new thread function
    pub thread_fn: ThreadFn,
    /// the entry of current syscall
    pub syscall_entry: VirtAddr,
}

impl Syscall<'_> {
    /// Handle terminal-generated Ctrl+C (SIGINT) in a uniform way.
    ///
    /// This is intentionally centralized so we don't sprinkle per-program hacks.
    fn maybe_handle_tty_intr(&mut self) -> SysResult {
        // Cheap relaxed-style peek first: the latch is empty on the
        // overwhelming majority of syscalls, and a plain load avoids the
        // `lock xchg` on x86 / locked CAS on aarch64/riscv that the swap in
        // `ctrl_c_pending_take` would otherwise issue per syscall.
        if !linux_object::fs::stdio::ctrl_c_pending_peek() {
            return Ok(0);
        }
        // Race-safe: another syscall may have claimed the latch between the
        // peek and the swap.
        let Some(intr) = linux_object::fs::stdio::ctrl_c_pending_take() else {
            return Ok(0);
        };
        // Only when the keystroke handler could not signal the group itself:
        // it already did whenever the VT had one, and delivering again is how
        // a single Ctrl-C came to raise two `SIGINT`s. The VT comes from the
        // latch, because the keystroke may not have arrived on the one on
        // screen.
        // Do not use sys_kill(-pgid): pgid==1 becomes kill(-1) ("every process") on Linux.
        if intr.signal_owed {
            linux_object::process::deliver_sigint_for_vt(Some(intr.vt));
        }
        Err(LxError::EINTR)
    }

    /// [ext-watch] Verify that this thread's and its process's `ext` fat
    /// pointers still read as they did at construction.
    ///
    /// `Process::ext` / `Thread::ext` keep turning up holding a valid-but-wrong
    /// trait object, with the guard words either side untouched — a precise
    /// write, not an overrun. The panics that report it fire whenever the
    /// victim next takes a Linux path, which can be long after the damage and
    /// on an unrelated CPU, so they name the victim but never the writer.
    ///
    /// Sampling here does. Both fields are written once at construction and
    /// never again, so any divergence is the bug; checking on entry and exit of
    /// every syscall bounds the damage to a single dispatch and names it. The
    /// cost is four relaxed loads and two comparisons per syscall.
    ///
    /// Diagnostic only — remove once the writer is found.
    fn check_ext_intact(&self, when: &str, num: u32) {
        let proc = self.thread.proc();
        let born = proc.ext_born();
        // Zero means the snapshot was never taken (only possible for a process
        // built before this instrumentation existed); skip rather than lie.
        if born != (0, 0) && proc.ext_fat() != born {
            let (data, vtable) = proc.ext_fat();
            panic!(
                "[ext-watch] {} syscall#{}: PROCESS ext changed under us -- pid={} name={:?} \
                 now data={:#x} vtable={:#x} -> {:x?} (drop, size, align), \
                 at birth data={:#x} vtable={:#x} -> {:x?}, \
                 canaries lo={:#x} hi={:#x}",
                when,
                num,
                proc.id(),
                proc.name(),
                data,
                vtable,
                zircon_object::task::vtable_info(vtable),
                born.0,
                born.1,
                zircon_object::task::vtable_info(born.1),
                proc.ext_canary_values().0,
                proc.ext_canary_values().1,
            );
        }
        let tborn = self.thread.ext_born();
        if tborn != (0, 0) && self.thread.ext_fat() != tborn {
            let (data, vtable) = self.thread.ext_fat();
            panic!(
                "[ext-watch] {} syscall#{}: THREAD ext changed under us -- tid={} pid={} name={:?} \
                 now data={:#x} vtable={:#x} -> {:x?} (drop, size, align), \
                 at birth data={:#x} vtable={:#x} -> {:x?}",
                when,
                num,
                self.thread.id(),
                proc.id(),
                proc.name(),
                data,
                vtable,
                zircon_object::task::vtable_info(vtable),
                tborn.0,
                tborn.1,
                zircon_object::task::vtable_info(tborn.1),
            );
        }
    }

    /// syscall entry function
    pub async fn syscall(&mut self, num: u32, args: [usize; 6]) -> isize {
        if let Err(err) = self.maybe_handle_tty_intr() {
            return -(err as isize);
        }
        let pid = self.zircon_process().id();
        if let Err(_err) = hunter::check_syscall(pid, num, &args) {
            return -(linux_object::error::LxError::EPERM as isize);
        }
        linux_object::syscall_stats::record(pid, num);
        let sys_type = Sys::try_from(num);
        debug!(
            "pid: {} syscall: num={} ({:?}), args={:x?}",
            self.zircon_process().id(),
            num,
            sys_type,
            args
        );
        let sys_type = match sys_type {
            Ok(t) => t,
            Err(_) => return unknown_syscall_number(num),
        };
        let [a0, a1, a2, a3, a4, a5] = args;
        // Eclipse's own perf accounting: time every syscall and attribute it to
        // both the system-wide and per-process tables (surfaced at `/proc/perf`
        // and `/proc/<pid>/perf`). The name resolver is registered lazily here
        // so `linux-object` can render numbers as names without an arch table.
        perf_accounting::ensure_registered();
        // [ext-watch] see `check_ext_intact`. Sampling on both sides of the
        // dispatch turns "some process's ext was corrupted, discovered whenever
        // that process next reached a Linux path" into "THIS syscall did it",
        // which is the difference between a hypothesis and a culprit.
        self.check_ext_intact("before", num);
        // What this thread is in, for `/proc/<pid>/threads`. Paired with the
        // clear below, and with the blocked marking the run loop does, so a
        // sleeping thread can say what it is sleeping in.
        self.thread.set_current_syscall(Some(num));
        let perf_start = kernel_hal::timer::timer_now();
        let ret = match sys_type {
            Sys::READ => self.sys_read(a0.into(), a1.into(), a2).await,
            Sys::WRITE => self.sys_write(a0.into(), a1.into(), a2).await,
            Sys::OPENAT => self.sys_openat(a0.into(), a1.into(), a2, a3),
            Sys::CLOSE => self.sys_close(a0.into()),
            Sys::FSTAT => self.sys_fstat(a0.into(), a1.into()),
            Sys::NEWFSTATAT => self.sys_fstatat(a0.into(), a1.into(), a2.into(), a3),
            Sys::LSEEK => self.sys_lseek(a0.into(), a1 as i64, a2),
            Sys::IOCTL => self.sys_ioctl(a0.into(), a1, a2, a3, a4).await,
            Sys::PREAD64 => self.sys_pread(a0.into(), a1.into(), a2, a3 as _).await,
            Sys::PWRITE64 => self.sys_pwrite(a0.into(), a1.into(), a2, a3 as _),
            Sys::READV => self.sys_readv(a0.into(), a1.into(), a2).await,
            Sys::WRITEV => self.sys_writev(a0.into(), a1.into(), a2).await,
            // Positional vectored I/O. The kernel ABI splits the offset into
            // (pos_l, pos_h) halves; on 64-bit both musl and glibc put the whole
            // offset in pos_l, and the kernel ignores pos_h — so do we.
            Sys::PREADV => self.sys_preadv(a0.into(), a1.into(), a2, a3 as u64).await,
            Sys::PWRITEV => self.sys_pwritev(a0.into(), a1.into(), a2, a3 as u64),
            Sys::PREADV2 => {
                self.sys_preadv2(a0.into(), a1.into(), a2, a3 as i64, a5)
                    .await
            }
            Sys::PWRITEV2 => {
                self.sys_pwritev2(a0.into(), a1.into(), a2, a3 as i64, a5)
                    .await
            }
            Sys::SENDFILE => self.sys_sendfile(a0.into(), a1.into(), a2.into(), a3).await,
            Sys::FCNTL => self.sys_fcntl(a0.into(), a1, a2).await,
            Sys::FLOCK => self.sys_flock(a0.into(), a1).await,
            Sys::FSYNC => self.sys_fsync(a0.into()),
            Sys::FDATASYNC => self.sys_fdatasync(a0.into()),
            Sys::TRUNCATE => self.sys_truncate(a0.into(), a1),
            Sys::FTRUNCATE => self.sys_ftruncate(a0.into(), a1),
            Sys::FADVISE64 => self.sys_fadvise64(a0.into(), a1, a2, a3),
            // `sys_readahead` has the man page's checks (EBADF for a
            // descriptor not open for reading, EINVAL for one with nothing
            // to read ahead into); this arm used to look the fd up itself
            // and answer 0, so the function it was written for never ran.
            Sys::READAHEAD => self.sys_readahead(a0.into(), a1 as u64, a2),
            Sys::FALLOCATE => self.sys_fallocate(a0.into(), a1, a2, a3),
            Sys::SYNC_FILE_RANGE => self.sys_sync_file_range(a0.into(), a1, a2, a3),
            Sys::GETDENTS64 => self.sys_getdents64(a0.into(), a1.into(), a2),
            Sys::GETCWD => self.sys_getcwd(a0.into(), a1),
            Sys::CHDIR => self.sys_chdir(a0.into()),
            Sys::FCHDIR => self.sys_fchdir(a0.into()),
            Sys::RENAMEAT => self.sys_renameat(a0.into(), a1.into(), a2.into(), a3.into()),
            Sys::RENAMEAT2 => self.sys_renameat2(a0.into(), a1.into(), a2.into(), a3.into(), a4),
            Sys::MKDIRAT => self.sys_mkdirat(a0.into(), a1.into(), a2),
            Sys::MKNODAT => self.sys_mknodat(a0.into(), a1.into(), a2, a3),
            Sys::LINKAT => self.sys_linkat(a0.into(), a1.into(), a2.into(), a3.into(), a4),
            Sys::UNLINKAT => self.sys_unlinkat(a0.into(), a1.into(), a2),
            Sys::SYMLINKAT => self.sys_symlinkat(a0.into(), a1.into(), a2.into()),
            Sys::READLINKAT => self.sys_readlinkat(a0.into(), a1.into(), a2.into(), a3),
            Sys::FCHMOD => self.sys_fchmod(a0.into(), a1),
            // `fchmodat` has no flags argument (`fchmodat2` would); neither
            // has `faccessat` (`faccessat2` does). See `at_flags_register`.
            ref s @ Sys::FCHMODAT => {
                let flags = at_flags_register(at_syscall_takes_flags(s), a3);
                self.sys_fchmodat(a0.into(), a1.into(), a2, flags)
            }
            Sys::FCHOWN => self.sys_fchown(a0.into(), a1, a2),
            Sys::FCHOWNAT => self.sys_fchownat(a0.into(), a1.into(), a2, a3, a4),
            ref s @ (Sys::FACCESSAT | Sys::FACCESSAT2) => {
                let flags = at_flags_register(at_syscall_takes_flags(s), a3);
                self.sys_faccessat(a0.into(), a1.into(), a2, flags)
            }
            Sys::DUP => self.sys_dup(a0.into()),
            Sys::DUP3 => self.sys_dup3(a0.into(), a1, a2),
            Sys::PIPE2 => self.sys_pipe2(a0.into(), a1),
            Sys::UTIMENSAT => self.sys_utimensat(a0.into(), a1.into(), a2.into(), a3),
            // `utimes` is an x86_64-only syscall number; riscv64 and aarch64
            // give glibc only `utimensat`.
            #[cfg(target_arch = "x86_64")]
            Sys::UTIMES => self.sys_utimes(a0.into(), a1.into()),
            Sys::COPY_FILE_RANGE => {
                self.sys_copy_file_range(a0.into(), a1.into(), a2.into(), a3.into(), a4, a5)
                    .await
            }
            Sys::SPLICE => {
                self.sys_splice(a0.into(), a1.into(), a2.into(), a3.into(), a4, a5)
                    .await
            }
            Sys::TEE => self.sys_tee(a0.into(), a1.into(), a2, a3).await,
            Sys::VMSPLICE => self.sys_vmsplice(a0.into(), a1, a2, a3).await,
            Sys::CLOSE_RANGE => self.sys_close_range(a0, a1, a2),

            // io multiplexing
            Sys::PSELECT6 => {
                self.sys_pselect6(a0, a1.into(), a2.into(), a3.into(), a4.into(), a5)
                    .await
            }
            Sys::PPOLL => {
                self.sys_ppoll(a0.into(), a1, a2.into(), a3.into(), a4)
                    .await
            }
            Sys::EPOLL_CREATE1 => self.sys_epoll_create1(a0),
            Sys::EPOLL_CTL => self.sys_epoll_ctl(a0.into(), a1 as i32, a2.into(), a3.into()),
            Sys::EPOLL_PWAIT => {
                self.sys_epoll_pwait(
                    a0.into(),
                    a1.into(),
                    a2,
                    poll_timeout_msecs(a3),
                    a4.into(),
                    a5,
                )
                .await
            }
            Sys::EVENTFD2 => self.sys_eventfd2(a0 as u32, a1),
            // Legacy `inotify_init` exists only in the x86_64 table; the generic
            // ABI (aarch64/riscv64) provides only `inotify_init1`.
            #[cfg(target_arch = "x86_64")]
            Sys::INOTIFY_INIT => self.sys_inotify_init1(0),
            Sys::INOTIFY_INIT1 => self.sys_inotify_init1(a0),
            Sys::INOTIFY_ADD_WATCH => self.sys_inotify_add_watch(a0, a1.into(), a2 as u32),
            Sys::INOTIFY_RM_WATCH => self.sys_inotify_rm_watch(a0, a1 as i32),
            Sys::MEMFD_CREATE => self.sys_memfd_create(a0.into(), a1),
            Sys::TIMERFD_CREATE => self.sys_timerfd_create(a0, a1),
            Sys::TIMERFD_SETTIME => self.sys_timerfd_settime(a0.into(), a1, a2.into(), a3.into()),
            Sys::TIMERFD_GETTIME => self.sys_timerfd_gettime(a0.into(), a1.into()),
            Sys::SIGNALFD4 => self.sys_signalfd4(a0.into(), a1.into(), a2, a3),
            // Legacy `signalfd` is x86_64-only; the generic ABI has `signalfd4`.
            #[cfg(target_arch = "x86_64")]
            Sys::SIGNALFD => self.sys_signalfd4(a0.into(), a1.into(), a2, 0),

            Sys::SOCKETPAIR => self.sys_socketpair(a0, a1, a2, a3.into()),
            // file system
            Sys::STATFS => self.sys_statfs(a0.into(), a1.into()),
            Sys::FSTATFS => self.sys_fstatfs(a0.into(), a1.into()),
            Sys::SYNC => self.sys_sync(),
            Sys::SYNCFS => self.sys_syncfs(a0.into()),
            Sys::MOUNT => self.sys_mount(a0.into(), a1.into(), a2.into(), a3, a4.into()),
            Sys::UMOUNT2 => self.sys_umount2(a0.into(), a1),

            // memory
            Sys::BRK => self.sys_brk(a0),
            Sys::MMAP => self.sys_mmap(a0, a1, a2, a3, a4.into(), a5 as _).await,
            Sys::MPROTECT => self.sys_mprotect(a0, a1, a2),
            Sys::MUNMAP => self.sys_munmap(a0, a1),
            Sys::MADVISE => self.sys_madvise(a0, a1, a2),
            Sys::MREMAP => self.sys_mremap(a0, a1, a2, a3, a4),
            Sys::MSYNC => self.sys_msync(a0, a1, a2),
            Sys::MINCORE => self.sys_mincore(a0, a1, a2.into()),
            Sys::MLOCK => self.sys_mlock(a0, a1),
            Sys::MLOCK2 => self.sys_mlock2(a0, a1, a2),
            Sys::MUNLOCK => self.sys_munlock(a0, a1),
            Sys::MLOCKALL => self.sys_mlockall(a0),
            Sys::MUNLOCKALL => self.sys_munlockall(),
            Sys::MBIND => self.unimplemented("mbind", Err(LxError::ENOSYS)),
            Sys::GET_MEMPOLICY => self.unimplemented("get_mempolicy", Err(LxError::ENOSYS)),
            Sys::SET_MEMPOLICY => self.unimplemented("set_mempolicy", Err(LxError::ENOSYS)),

            // signal
            Sys::RT_SIGACTION => self.sys_rt_sigaction(a0, a1.into(), a2.into(), a3),
            Sys::RT_SIGPROCMASK => self.sys_rt_sigprocmask(a0 as _, a1.into(), a2.into(), a3),
            Sys::RT_SIGRETURN => self.sys_rt_sigreturn(),
            Sys::RT_SIGSUSPEND => self.sys_rt_sigsuspend(a0.into(), a1).await,
            Sys::RT_SIGTIMEDWAIT => {
                self.sys_rt_sigtimedwait(a0.into(), a1.into(), a2.into(), a3)
                    .await
            }
            Sys::SIGALTSTACK => self.sys_sigaltstack(a0.into(), a1.into()),
            Sys::RT_SIGPENDING => self.sys_rt_sigpending(a0.into(), a1),
            Sys::RT_SIGQUEUEINFO => self.sys_rt_sigqueueinfo(a0, a1, a2.into()),
            Sys::RT_TGSIGQUEUEINFO => self.sys_rt_tgsigqueueinfo(a0, a1, a2, a3.into()),
            Sys::KILL => self.sys_kill(a0 as isize, a1),

            // schedule
            Sys::SCHED_YIELD => {
                kernel_hal::thread::yield_now().await;
                Ok(0)
            }
            Sys::SCHED_GETAFFINITY => self.sys_sched_getaffinity(a0, a1, a2.into()),
            Sys::SCHED_SETAFFINITY => self.sys_sched_setaffinity(a0, a1, a2.into()).await,
            Sys::SCHED_SETSCHEDULER => self.sys_sched_setscheduler(a0, a1, a2.into()),
            Sys::SCHED_GETSCHEDULER => self.sys_sched_getscheduler(a0),
            Sys::SCHED_SETPARAM => self.sys_sched_setparam(a0, a1.into()),
            Sys::SCHED_GETPARAM => self.sys_sched_getparam(a0, a1.into()),
            Sys::SCHED_GET_PRIORITY_MAX => self.sys_sched_get_priority_max(a0),
            Sys::SCHED_GET_PRIORITY_MIN => self.sys_sched_get_priority_min(a0),
            Sys::SCHED_RR_GET_INTERVAL => self.sys_sched_rr_get_interval(a0, a1.into()),
            Sys::SCHED_SETATTR => self.sys_sched_setattr(a0, a1.into(), a2),
            Sys::SCHED_GETATTR => self.sys_sched_getattr(a0, a1.into(), a2, a3),

            // socket
            Sys::SOCKET => self.sys_socket(a0, a1, a2),
            Sys::CONNECT => self.sys_connect(a0, a1.into(), a2).await,
            Sys::ACCEPT => self.sys_accept(a0, a1.into(), a2.into()).await,
            // accept4 == accept + flags on the NEW socket (SOCK_CLOEXEC /
            // SOCK_NONBLOCK). GLib/GDBus uses it unconditionally; falling
            // through to `unknown syscall` broke waybar's D-Bus socket path.
            Sys::ACCEPT4 => self.sys_accept4(a0, a1.into(), a2.into(), a3).await,
            Sys::SENDTO => self.sys_sendto(a0, a1.into(), a2, a3, a4.into(), a5).await,
            Sys::RECVFROM => {
                self.sys_recvfrom(a0, a1.into(), a2, a3, a4.into(), a5.into())
                    .await
            }
            Sys::SENDMSG => self.sys_sendmsg(a0, a1.into(), a2).await,
            Sys::RECVMSG => self.sys_recvmsg(a0, a1.into(), a2).await,
            Sys::SENDMMSG => self.sys_sendmmsg(a0, a1.into(), a2, a3).await,
            Sys::RECVMMSG => self.sys_recvmmsg(a0, a1.into(), a2, a3).await,
            Sys::SHUTDOWN => self.sys_shutdown(a0, a1),
            Sys::BIND => self.sys_bind(a0, a1.into(), a2),
            Sys::LISTEN => self.sys_listen(a0, a1),

            Sys::GETSOCKNAME => self.sys_getsockname(a0, a1.into(), a2.into()),
            Sys::GETPEERNAME => self.sys_getpeername(a0, a1.into(), a2.into()),
            Sys::SETSOCKOPT => self.sys_setsockopt(a0, a1, a2, a3.into(), a4),
            Sys::GETSOCKOPT => self.sys_getsockopt(a0, a1, a2, a3.into(), a4.into()),

            // process
            Sys::EXECVE => self.sys_execve(a0.into(), a1.into(), a2.into()),
            // clone3 is deliberately ENOSYS (pre-Linux-5.3 behaviour; glibc and
            // musl fall back to legacy clone cleanly). Root cause, found in the
            // QEMU desktop lab: glibc's __clone3 child stub starts with
            // `mov %r8,%rdi; call *%rdx` — it requires RDX (and R8) to survive
            // the syscall INTO THE NEW CHILD. Legacy clone's stub instead pops
            // the function/argument off the child STACK, which is robust. Our
            // new-thread first-entry path does not preserve the parent's RDX
            // into the child, so clone3-started threads jumped to garbage
            // (observed: kernel wild jump to 0x400000006, a #GP on a
            // non-canonical pointer inside epoll_pwait, and an all-idle wedge
            // when the crashed thread held a compositor lock). Until the child
            // context provably carries every caller-saved register, answering
            // ENOSYS is the correct, safe behaviour — sys_clone3 below stays
            // implemented for when that is fixed.
            Sys::CLONE3 => self.sys_clone3(a0.into(), a1).await,
            Sys::EXIT => self.sys_exit(a0 as _),
            Sys::EXIT_GROUP => self.sys_exit_group(a0 as _),
            Sys::WAIT4 => self.sys_wait4(a0 as _, a1.into(), a2 as _, a3.into()).await,
            Sys::WAITID => {
                self.sys_waitid(a0 as i32, a1, a2.into(), a3 as u32, a4.into())
                    .await
            }
            Sys::SET_TID_ADDRESS => self.sys_set_tid_address(a0.into()),
            Sys::FUTEX => self.sys_futex(a0, a1 as _, a2 as _, a3, a4, a5 as _).await,
            Sys::GET_ROBUST_LIST => self.sys_get_robust_list(a0 as _, a1.into(), a2.into()),
            Sys::SET_ROBUST_LIST => self.sys_set_robust_list(a0.into(), a1 as _),
            Sys::TKILL => self.sys_tkill(a0, a1),
            Sys::TGKILL => self.sys_tgkill(a0, a1, a2),
            Sys::PIDFD_OPEN => self.sys_pidfd_open(a0, a1 as u32),
            Sys::PIDFD_SEND_SIGNAL => {
                self.sys_pidfd_send_signal(a0.into(), a1, a2.into(), a3 as u32)
            }
            Sys::PIDFD_GETFD => self.sys_pidfd_getfd(a0.into(), a1 as i32, a2 as u32),

            // time
            Sys::NANOSLEEP => self.sys_nanosleep(a0.into(), a1.into()).await,
            Sys::CLOCK_NANOSLEEP => self.sys_clock_nanosleep(a0, a1, a2.into(), a3.into()).await,
            Sys::SETITIMER => self.sys_setitimer(a0, a1.into(), a2.into()),
            // `alarm` only exists in the x86_64 syscall table; the generic ABI
            // (aarch64/riscv64) omits it in favour of setitimer/timer_*.
            #[cfg(target_arch = "x86_64")]
            Sys::ALARM => self.sys_alarm(a0),
            Sys::TIMER_CREATE => self.sys_timer_create(a0, a1, a2),
            Sys::TIMER_SETTIME => self.sys_timer_settime(a0, a1, a2.into(), a3.into()),
            Sys::TIMER_GETTIME => self.sys_timer_gettime(a0, a1),
            Sys::TIMER_DELETE => self.sys_timer_delete(a0),
            Sys::TIMER_GETOVERRUN => self.sys_timer_getoverrun(a0),
            Sys::GETITIMER => self.sys_getitimer(a0, a1.into()),
            Sys::GETTIMEOFDAY => self.sys_gettimeofday(a0.into(), a1.into()),
            Sys::SETTIMEOFDAY => self.sys_settimeofday(a0.into(), a1.into()),
            Sys::CLOCK_GETTIME => self.sys_clock_gettime(a0, a1.into()),
            Sys::CLOCK_SETTIME => self.sys_clock_settime(a0, a1.into()),
            Sys::CLOCK_GETRES => self.sys_clock_getres(a0, a1.into()),
            Sys::ADJTIMEX => self.sys_adjtimex(a0.into()),
            Sys::CLOCK_ADJTIME => self.sys_clock_adjtime(a0, a1.into()),

            // msg
            Sys::MSGGET => self.sys_msgget(a0, a1),
            Sys::MSGSND => self.sys_msgsnd(a0, a1, a2, a3).await,
            Sys::MSGRCV => self.sys_msgrcv(a0, a1, a2, a3 as isize, a4).await,
            Sys::MSGCTL => self.sys_msgctl(a0, a1, a2),

            // sem
            #[cfg(not(target_arch = "mips"))]
            Sys::SEMGET => self.sys_semget(a0, a1, a2),
            #[cfg(not(target_arch = "mips"))]
            Sys::SEMOP => self.sys_semop(a0, a1.into(), a2).await,
            #[cfg(not(target_arch = "mips"))]
            Sys::SEMCTL => self.sys_semctl(a0, a1, a2, a3),

            // shm
            #[cfg(not(target_arch = "mips"))]
            Sys::SHMGET => self.sys_shmget(a0, a1, a2),
            #[cfg(not(target_arch = "mips"))]
            Sys::SHMAT => self.sys_shmat(a0, a1, a2),
            // `SYSCALL_DEFINE1(shmdt, char __user *, shmaddr)`: the address
            // is the first and only argument. It was read from the second
            // register, which a one-argument libc stub never sets.
            #[cfg(not(target_arch = "mips"))]
            Sys::SHMDT => self.sys_shmdt(a0),
            #[cfg(not(target_arch = "mips"))]
            Sys::SHMCTL => self.sys_shmctl(a0, a1, a2),

            // system
            Sys::GETPID => self.sys_getpid(),
            Sys::GETTID => self.sys_gettid(),
            Sys::GETCPU => self.sys_getcpu(a0.into(), a1.into(), a2),
            Sys::UNAME => self.sys_uname(a0.into()),
            Sys::SETHOSTNAME => self.sys_sethostname(a0.into(), a1),
            Sys::SETDOMAINNAME => self.sys_setdomainname(a0.into(), a1),
            Sys::CAPGET => self.sys_capget(a0.into(), a1.into()),
            Sys::CAPSET => self.sys_capset(a0.into(), a1.into()),
            Sys::IOPRIO_SET => self.sys_ioprio_set(a0, a1, a2),
            Sys::IOPRIO_GET => self.sys_ioprio_get(a0, a1),
            Sys::SYSLOG => self.sys_syslog(a0 as i32, a1.into(), a2 as i32),
            Sys::UMASK => self.sys_umask(a0),
            Sys::GETRLIMIT => self.sys_getrlimit(a0, a1.into()),
            Sys::SETRLIMIT => self.sys_setrlimit(a0, a1.into()),
            Sys::GETRUSAGE => self.sys_getrusage(a0, a1.into()),
            Sys::SYSINFO => self.sys_sysinfo(a0.into()),
            Sys::TIMES => self.sys_times(a0.into()),
            Sys::GETUID => self.sys_getuid(),
            Sys::GETGID => self.sys_getgid(),
            Sys::SETUID => self.sys_setuid(a0),
            Sys::SETGID => self.sys_setgid(a0),
            Sys::SETREUID => self.sys_setreuid(a0, a1),
            Sys::SETREGID => self.sys_setregid(a0, a1),
            Sys::SETRESUID => self.sys_setresuid(a0, a1, a2),
            Sys::SETRESGID => self.sys_setresgid(a0, a1, a2),
            Sys::GETRESUID => self.sys_getresuid(a0.into(), a1.into(), a2.into()),
            Sys::GETRESGID => self.sys_getresgid(a0.into(), a1.into(), a2.into()),
            Sys::SETFSUID => self.sys_setfsuid(a0),
            Sys::SETFSGID => self.sys_setfsgid(a0),
            Sys::GETEUID => self.sys_geteuid(),
            Sys::GETEGID => self.sys_getegid(),
            Sys::SETPGID => self.sys_setpgid(a0 as _, a1 as _),
            Sys::GETPPID => self.sys_getppid(),
            Sys::SETSID => self.sys_setsid(),
            Sys::GETPGID => self.sys_getpgid(a0 as _),
            Sys::GETSID => self.sys_getsid(a0 as _),
            // getpgrp() is the legacy no-argument form of getpgid(0). Without it
            // an interactive busybox `sh` cannot determine its own process group
            // during job-control setup, takes the "I am a background job" branch
            // and `kill(0, SIGTTIN)`s itself — which then terminated the shell.
            // Legacy `getpgrp` is x86_64-only; the generic ABI uses getpgid(0).
            #[cfg(target_arch = "x86_64")]
            Sys::GETPGRP => self.sys_getpgid(0),
            Sys::GETGROUPS => self.sys_getgroups(a0, a1.into()),
            Sys::SETGROUPS => self.sys_setgroups(a0, a1.into()),
            // Scheduling priority (nice). Backed by the thread's stored nice
            // value, which also biases its timeslice (see
            // `Thread::tick_should_preempt`). getpriority returns `20 - nice`
            // so that valid values stay non-negative.
            Sys::SETPRIORITY => self.sys_setpriority(a0, a1, a2 as i32),
            Sys::GETPRIORITY => self.sys_getpriority(a0, a1),
            Sys::PRCTL => self.sys_prctl(a0 as i32, a1, a2, a3, a4),
            Sys::PERSONALITY => self.sys_personality(a0),
            // `rseq` (restartable sequences) is optional: glibc probes it on
            // every thread start and silently falls back when it is missing.
            // Return ENOSYS quietly so we don't (a) advertise a feature we don't
            // implement, nor (b) flood the log with "unknown syscall: RSEQ" on
            // every process spawn (very visible under `perf`/exec-heavy loads).
            Sys::RSEQ => Err(LxError::ENOSYS),
            // Same treatment: glibc/util-linux/wlroots stacks probe
            // name_to_handle_at on hotplug/device paths and handle ENOSYS
            // fine; the loud per-call ERROR line was pure log noise.
            Sys::NAME_TO_HANDLE_AT => Err(LxError::ENOSYS),
            // No namespaces in this kernel. ENOSYS here is LOAD-BEARING, not a
            // placeholder: bubblewrap turns it into
            //   bwrap: Creating new namespace failed: Function not implemented
            // and glycin — which is how Alpine's gdk-pixbuf decodes every image
            // format, out of process, one loader per format — matches exactly
            // that string to decide the sandbox is unavailable and run its
            // loaders directly instead. glycin 2.1.5 offers no environment
            // override for that choice, so this errno is the only lever.
            //
            // Implementing `unshare` as a no-op that returns 0 would be WORSE
            // than not having it: bwrap would then proceed into a sandbox that
            // isolates nothing, fail later with a message glycin does not
            // recognise, and every image decode in the desktop would fail (a
            // NULL pixbuf, which libwnck `g_assert`s on, taking the session
            // down with it). If namespaces are ever added, they must actually
            // work before this arm goes away.
            Sys::UNSHARE => Err(LxError::ENOSYS),
            Sys::MEMBARRIER => self.sys_membarrier(a0 as i32, a1 as u32, a2 as i32),
            Sys::PRLIMIT64 => self.sys_prlimit64(a0, a1, a2.into(), a3.into()),
            Sys::REBOOT => self.sys_reboot(a0 as u32, a1 as u32, a2 as u32, a3.into()),
            Sys::GETRANDOM => self.sys_getrandom(a0.into(), a1, a2 as u32),
            Sys::STATX => self.sys_statx(a0.into(), a1.into(), a2, a3 as u32, a4.into()),

            // Extended attributes: this kernel's filesystems do not keep
            // them, and answering from here, by syscall name, kept busybox
            // init's probing off the console. But `fs/xattr.c` reads the
            // name and the flags and looks the file up BEFORE the filesystem
            // answers, and these arms did none of it: a missing path was
            // ENODATA, a closed fd was 0. `sys_xattr` makes those checks and
            // then gives the same four answers.
            Sys::GETXATTR => {
                self.sys_xattr(XattrOp::Get, XattrTarget::Path(a0.into(), true), a1.into())
            }
            Sys::LGETXATTR => {
                self.sys_xattr(XattrOp::Get, XattrTarget::Path(a0.into(), false), a1.into())
            }
            Sys::FGETXATTR => self.sys_xattr(XattrOp::Get, XattrTarget::Fd(a0.into()), a1.into()),
            Sys::SETXATTR => self.sys_xattr(
                XattrOp::Set {
                    size: a3,
                    flags: a4,
                },
                XattrTarget::Path(a0.into(), true),
                a1.into(),
            ),
            Sys::LSETXATTR => self.sys_xattr(
                XattrOp::Set {
                    size: a3,
                    flags: a4,
                },
                XattrTarget::Path(a0.into(), false),
                a1.into(),
            ),
            Sys::FSETXATTR => self.sys_xattr(
                XattrOp::Set {
                    size: a3,
                    flags: a4,
                },
                XattrTarget::Fd(a0.into()),
                a1.into(),
            ),
            Sys::LISTXATTR => {
                self.sys_xattr(XattrOp::List, XattrTarget::Path(a0.into(), true), a1.into())
            }
            Sys::LLISTXATTR => self.sys_xattr(
                XattrOp::List,
                XattrTarget::Path(a0.into(), false),
                a1.into(),
            ),
            Sys::FLISTXATTR => self.sys_xattr(XattrOp::List, XattrTarget::Fd(a0.into()), a1.into()),
            Sys::REMOVEXATTR => self.sys_xattr(
                XattrOp::Remove,
                XattrTarget::Path(a0.into(), true),
                a1.into(),
            ),
            Sys::LREMOVEXATTR => self.sys_xattr(
                XattrOp::Remove,
                XattrTarget::Path(a0.into(), false),
                a1.into(),
            ),
            Sys::FREMOVEXATTR => {
                self.sys_xattr(XattrOp::Remove, XattrTarget::Fd(a0.into()), a1.into())
            }

            // kernel module
            //            Sys::INIT_MODULE => self.sys_init_module(a0.into(), a1 as usize, a2.into()),
            Sys::FINIT_MODULE => self.unimplemented("finit_module", Err(LxError::ENOSYS)),
            //            Sys::DELETE_MODULE => self.sys_delete_module(a0.into(), a1 as u32),
            #[cfg(not(target_arch = "aarch64"))]
            Sys::BLOCK_IN_KERNEL => self.sys_block_in_kernel(),
            // Custom `eclipse_dns_query` is only in the x86_64 syscall table.
            #[cfg(target_arch = "x86_64")]
            Sys::ECLIPSE_DNS_QUERY => self.sys_eclipse_dns_query(a0.into(), a1, a2, a3.into(), a4),
            Sys::PERF_EVENT_OPEN => {
                self.sys_perf_event_open(a0, a1 as i32, a2 as i32, a3 as i32, a4)
            }

            #[cfg(target_arch = "x86_64")]
            _ => self.x86_64_syscall(sys_type, args).await,
            #[cfg(target_arch = "riscv64")]
            _ => self.riscv64_syscall(sys_type, args).await,
            #[cfg(target_arch = "aarch64")]
            _ => self.aarch64_syscall(sys_type, args).await,
        };
        // `checked_sub` (not `-`): an async syscall can migrate CPUs across an
        // await, and with unsynchronised TSCs the end can read before the start,
        // which would panic on a plain `Duration` subtraction.
        self.thread.set_current_syscall(None);
        self.check_ext_intact("after", num);
        let elapsed_ns = kernel_hal::timer::timer_now()
            .checked_sub(perf_start)
            .unwrap_or_default()
            .as_nanos() as u64;
        linux_object::perf::record(self.linux_process(), num, elapsed_ns);
        // Boot-trace: record this syscall in the /proc/bootprofile timeline if it
        // was slow enough to be one of the desktop-startup stalls the open trace
        // cannot see into. Gated on the same relaxed atomic as record_open. For
        // the memory-map calls, decode len/prot/flags/fd so a slow mmap can be
        // told apart (MAP_FIXED there forces an unmap + cross-CPU TLB shootdown,
        // the prime suspect for the uniform ~350 ms mmap stalls).
        if linux_object::boot_trace::enabled() {
            linux_object::boot_trace::record_syscall(pid, num, elapsed_ns, || match Sys::try_from(
                num,
            ) {
                Ok(Sys::MMAP) => alloc::format!(
                    "len={:#x} prot={:#x} flags={:#x} fd={}",
                    args[1],
                    args[2],
                    args[3],
                    args[4] as isize
                ),
                Ok(Sys::MPROTECT) => {
                    alloc::format!("addr={:#x} len={:#x} prot={:#x}", args[0], args[1], args[2])
                }
                Ok(Sys::MUNMAP) => alloc::format!("addr={:#x} len={:#x}", args[0], args[1]),
                _ => alloc::string::String::new(),
            });
        }
        info!("<= {:?}", ret);
        // [einval-hunt] glxgears against the finally-alive Xwayland dies with
        // "XIO: fatal IO error 22 (Invalid argument)" in the GLX/DRI3 window
        // (both HW and LIBGL_ALWAYS_SOFTWARE=1), while pure-X clients run
        // clean and the server survives -- some syscall on that path returns
        // EINVAL and the client treats it as fatal (or leaves the stale errno
        // an XIO then reports). Name the syscall instead of guessing: one
        // budgeted error! per hit for the syscall families on that path, any
        // process (the failing call may be Xwayland's own sendmsg).
        if let Err(LxError::EINVAL) = ret {
            einval_hunt(pid, num, &args);
        }
        // [alsa-hunt] PulseAudio keeps dying on an abort inside libasound:
        //   [exit] pid=N (pulseaudio) killed by signal SIGABRT
        //   [crash-bt] ... libasound.so.2+0x323f3 ... libalsa-util.so+0x26bc9
        // i.e. an assert() in alsa-lib, not a pa_assert. alsa-lib asserts on
        // kernel answers it considers impossible, so the failing ioctl and its
        // errno are the whole question -- and the EINVAL hunter above cannot
        // see it, because the abort survives with no EINVAL in the log at all.
        // Name every failing sound ioctl, with the command decoded.
        if let Err(err) = ret {
            alsa_hunt(pid, num, &args, err);
        }
        syscall_ret(ret)
    }

    #[cfg(target_arch = "aarch64")]
    /// syscall specified for aarch64
    async fn aarch64_syscall(&mut self, sys_type: Sys, args: [usize; 6]) -> SysResult {
        let [a0, a1, a2, a3, a4, _a5] = args;
        debug!("aarch6464_syscall: {:?}, args: {:?}", sys_type, args);
        match sys_type {
            Sys::CLONE => self.sys_clone(a0, a1, a2.into(), a3, a4.into()).await,
            _ => self.unknown_syscall(sys_type),
        }
    }

    #[cfg(target_arch = "x86_64")]
    /// syscall specified for x86_64
    async fn x86_64_syscall(&mut self, sys_type: Sys, args: [usize; 6]) -> SysResult {
        let [a0, a1, a2, a3, a4, _a5] = args;
        match sys_type {
            Sys::OPEN => self.sys_open(a0.into(), a1, a2),
            Sys::STAT => self.sys_stat(a0.into(), a1.into()),
            Sys::LSTAT => self.sys_lstat(a0.into(), a1.into()),
            Sys::POLL => self.sys_poll(a0.into(), a1, poll_timeout_msecs(a2)).await,
            Sys::ACCESS => self.sys_access(a0.into(), a1),
            Sys::PIPE => self.sys_pipe(a0.into()),
            Sys::SELECT => {
                self.sys_select(a0, a1.into(), a2.into(), a3.into(), a4.into())
                    .await
            }
            Sys::DUP2 => self.sys_dup2(a0.into(), a1),
            //            Sys::ALARM => self.unimplemented("alarm", Ok(0)),
            Sys::PAUSE => self.sys_pause().await,
            Sys::FORK => self.sys_fork(0, 0),
            Sys::VFORK => self.sys_vfork(0, 0).await,
            Sys::RENAME => self.sys_rename(a0.into(), a1.into()),
            Sys::MKDIR => self.sys_mkdir(a0.into(), a1),
            Sys::MKNOD => self.sys_mknod(a0.into(), a1, a2),
            Sys::RMDIR => self.sys_rmdir(a0.into()),
            Sys::LINK => self.sys_link(a0.into(), a1.into()),
            Sys::UNLINK => self.sys_unlink(a0.into()),
            Sys::SYMLINK => self.sys_symlink(a0.into(), a1.into()),
            Sys::READLINK => self.sys_readlink(a0.into(), a1.into(), a2),
            Sys::CHMOD => self.sys_chmod(a0.into(), a1),
            Sys::CHOWN => self.sys_fchownat(FileDesc::CWD, a0.into(), a1, a2, 0),
            Sys::ARCH_PRCTL => self.sys_arch_prctl(a0 as _, a1),
            Sys::TIME => self.sys_time(a0.into()),
            Sys::CLONE => self.sys_clone(a0, a1, a2.into(), a4, a3.into()).await,
            Sys::EPOLL_CREATE => self.sys_epoll_create(a0),
            Sys::EPOLL_WAIT => {
                self.sys_epoll_wait(a0.into(), a1.into(), a2, poll_timeout_msecs(a3))
                    .await
            }
            _ => self.unknown_syscall(sys_type),
        }
    }

    #[cfg(target_arch = "riscv64")]
    async fn riscv64_syscall(&mut self, sys_type: Sys, args: [usize; 6]) -> SysResult {
        let [a0, a1, a2, a3, a4, _a5] = args;
        match sys_type {
            //Sys::OPEN => self.sys_open(a0.into(), a1, a2),
            Sys::CLONE => self.sys_clone(a0, a1, a2.into(), a3, a4.into()).await,
            _ => self.unknown_syscall(sys_type),
        }
    }

    /// unkown syscalls, currently is similar to unimplemented syscalls but emit an error
    fn unknown_syscall(&mut self, sys_type: Sys) -> SysResult {
        error!("unknown syscall: {:?}.", sys_type);
        Err(LxError::ENOSYS)
    }

    /// unimplemented syscalls
    fn unimplemented(&self, name: &str, ret: SysResult) -> SysResult {
        warn!("{}: unimplemented", name);
        ret
    }

    /// get zircon process
    fn zircon_process(&self) -> &Arc<Process> {
        self.thread.proc()
    }

    /// get linux process
    fn linux_process(&self) -> &LinuxProcess {
        self.zircon_process().linux()
    }
}

/// Decode an ALSA ioctl command into a readable name, or `None` when it is not
/// one. `_IO*('A'|'U'|'T', nr, ...)`: PCM, control and timer respectively.
fn alsa_ioctl_name(cmd: u32) -> Option<&'static str> {
    // Require a real `_IOC` encoding first. The TTY family uses flat legacy
    // numbers (TCGETS 0x5401 .. TIOCGWINSZ 0x5413) whose second byte is also
    // 0x54 = b'T', so a plain type match reads every `isatty()` probe on a
    // non-tty fd as an ALSA timer call and reports its (entirely normal)
    // ENOTTY. That flooded the boot console. A real ALSA ioctl always carries
    // a direction and a payload size; the legacy TTY numbers carry neither.
    let dir = (cmd >> 30) & 3;
    let size = (cmd >> 16) & 0x3fff;
    if dir == 0 || size == 0 {
        return None;
    }
    let ty = ((cmd >> 8) & 0xff) as u8;
    let nr = (cmd & 0xff) as u8;
    Some(match (ty, nr) {
        (b'A', 0x00) => "PCM_PVERSION",
        (b'A', 0x01) => "PCM_INFO",
        (b'A', 0x10) => "PCM_HW_REFINE",
        (b'A', 0x11) => "PCM_HW_PARAMS",
        (b'A', 0x12) => "PCM_HW_FREE",
        (b'A', 0x13) => "PCM_SW_PARAMS",
        (b'A', 0x20) => "PCM_STATUS",
        (b'A', 0x21) => "PCM_DELAY",
        (b'A', 0x22) => "PCM_HWSYNC",
        (b'A', 0x23) => "PCM_SYNC_PTR",
        (b'A', 0x24) => "PCM_STATUS_EXT",
        (b'A', 0x40) => "PCM_PREPARE",
        (b'A', 0x41) => "PCM_RESET",
        (b'A', 0x42) => "PCM_START",
        (b'A', 0x43) => "PCM_DROP",
        (b'A', 0x44) => "PCM_DRAIN",
        (b'A', 0x45) => "PCM_PAUSE",
        (b'A', 0x46) => "PCM_REWIND",
        (b'A', 0x47) => "PCM_RESUME",
        (b'A', 0x48) => "PCM_XRUN",
        (b'A', 0x49) => "PCM_FORWARD",
        (b'A', 0x50) => "PCM_WRITEI_FRAMES",
        (b'A', 0x51) => "PCM_READI_FRAMES",
        (b'A', 0x52) => "PCM_WRITEN_FRAMES",
        (b'A', 0x53) => "PCM_READN_FRAMES",
        (b'A', 0x60) => "PCM_LINK",
        (b'A', 0x61) => "PCM_UNLINK",
        (b'A', _) => "PCM_?",
        (b'U', 0x00) => "CTL_PVERSION",
        (b'U', 0x01) => "CTL_CARD_INFO",
        (b'U', 0x10) => "CTL_ELEM_LIST",
        (b'U', 0x11) => "CTL_ELEM_INFO",
        (b'U', 0x12) => "CTL_ELEM_READ",
        (b'U', 0x13) => "CTL_ELEM_WRITE",
        (b'U', 0x16) => "CTL_SUBSCRIBE",
        (b'U', 0x1a) => "CTL_TLV_READ",
        (b'U', 0x30) => "CTL_PCM_NEXT_DEVICE",
        (b'U', 0x31) => "CTL_PCM_INFO",
        (b'U', 0x32) => "CTL_PCM_PREFER_SUBDEVICE",
        (b'U', 0x40) => "CTL_RAWMIDI_NEXT_DEVICE",
        (b'U', 0xd0) => "CTL_POWER",
        (b'U', 0xd1) => "CTL_POWER_STATE",
        (b'U', _) => "CTL_?",
        (b'T', _) => "TIMER_?",
        _ => return None,
    })
}

/// [alsa-hunt] One budgeted `error!` line per FAILING sound ioctl, naming the
/// command and the errno.
///
/// alsa-lib asserts (and aborts the process) on kernel answers it considers
/// impossible, so when PulseAudio dies inside libasound the question is always
/// "which ioctl returned what". `HW_REFINE` returning `EINVAL` is excluded: the
/// `*_near` helpers find a supported rate/period BY refining until the kernel
/// says no, so that one is a search, not a fault. `ENOTTY` (wrong device) and
/// `EAGAIN` (a full ring under a nonblocking write, which alsa-lib retries via
/// poll) are excluded for the same reason: expected answers, not aborts.
fn alsa_hunt(pid: KoID, num: u32, args: &[usize; 6], err: LxError) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static BUDGET: AtomicU32 = AtomicU32::new(0);
    if !matches!(Sys::try_from(num), Ok(Sys::IOCTL)) {
        return;
    }
    let cmd = args[1] as u32;
    let Some(name) = alsa_ioctl_name(cmd) else {
        return;
    };
    if name == "PCM_HW_REFINE" && matches!(err, LxError::EINVAL) {
        return;
    }
    // ENOTTY is "this fd is not that kind of device" -- a probe, not a fault.
    if matches!(err, LxError::ENOTTY) {
        return;
    }
    // CTL_PCM_INFO → ENOENT is alsa-lib / Pulse enumerating capture (or a
    // device number we do not have). We only ship playback (`pcmC*D0p`); Linux
    // answers the same ENOENT for a missing stream, and the caller just skips
    // it. Logging it as ERROR made a healthy probe look like a broken card.
    if name == "CTL_PCM_INFO" && matches!(err, LxError::ENOENT) {
        return;
    }
    // EAGAIN is "no room right now, poll and retry": the normal answer to a
    // nonblocking WRITEI against a full ring, which alsa-lib handles by going
    // back to poll(), never by asserting. It is not what this hunter is for
    // (an abort inside libasound), and logging every one at error! floods the
    // console with a line that reads like a fault -- the exact false alarm
    // that kept getting reported. A genuine stall still surfaces: the driver
    // escalates a ring that stopped draining to EPIPE, not EAGAIN.
    if matches!(err, LxError::EAGAIN) {
        return;
    }
    if BUDGET.fetch_add(1, Ordering::Relaxed) < 64 {
        log::error!(
            "[alsa-hunt] pid={} ioctl {} ({:#x}) fd={} -> {:?} ({})",
            pid,
            name,
            cmd,
            args[0],
            err,
            err as isize,
        );
    }
}

/// Encodes a [`SysResult`] the way the caller's register expects it.
///
/// Linux's convention is that a syscall return in `[-4095, -1]` is an error
/// and anything else is a value, so the sign is not decoration -- it is the
/// whole signal. Every answer this kernel gives userspace goes through here.
fn syscall_ret(ret: SysResult) -> isize {
    match ret {
        Ok(value) => value as isize,
        Err(err) => -(err as isize),
    }
}

/// The answer to a syscall number this kernel does not have in its table.
///
/// This used to be `return LxError::EINVAL as _`, which is **+22**: not an
/// error at all, but a successful call that returned the number 22. Deciding
/// at runtime whether a syscall exists is how both glibc and musl handle
/// kernels older than themselves -- they issue the call and read `-ENOSYS` --
/// so `epoll_pwait2`, `fchmodat2`, `futex_waitv`, `process_madvise`,
/// `memfd_secret`, `cachestat` and the rest of what this table does not list
/// all answered "yes, and it worked", after which the libc believes whatever
/// it thinks the call produced. The right answer is Linux's `-ENOSYS`, which
/// is what [`Syscall::unknown_syscall`] already gives for a number that IS in
/// the table with no implementation behind it. Half of "we do not do that"
/// was already correct, which is why the other half went unnoticed.
fn unknown_syscall_number(num: u32) -> isize {
    error!("invalid syscall number: {}", num);
    syscall_ret(Err(LxError::ENOSYS))
}

/// The `flags` argument for a `*at` syscall whose handler takes one, from the
/// register it would be in.
///
/// `faccessat` and `fchmodat` are `SYSCALL_DEFINE3` in Linux: `(dirfd, path,
/// mode)`, and the kernel never reads a fourth register for them. The
/// four-argument ones that added the flags are `faccessat2` and `fchmodat2`.
/// Both threes were dispatched here as if they were the fours, so a plain
/// `access()` or `chmod()` -- which musl and glibc issue as a three-argument
/// `faccessat` / `fchmodat`, leaving the fourth register holding whatever
/// their syscall stub last had in it -- handed us that as `AT_` flags. Two of
/// the bits `AtFlags` keeps change the answer: `AT_SYMLINK_NOFOLLOW` applies
/// the call to a symlink instead of to its target (`chmod("link", m)` setting
/// the mode of the link and leaving the file alone), and `AT_EACCESS` swaps
/// the real uid/gid for the effective ones. Intermittently, differing per
/// call site, on syscalls every shell, installer and dynamic loader make.
fn at_flags_register(takes_flags: bool, a3: usize) -> usize {
    if takes_flags {
        a3
    } else {
        0
    }
}

/// Which of the `*at` syscalls dispatched with a flags word actually carry
/// one. The dispatch asks this rather than deciding per arm, so the answer
/// is one place and a test can read it. `fchmodat2` has no number in this
/// table yet (it answers `ENOSYS`, and libc falls back), so `FCHMODAT` is the
/// only spelling of that family here, and it is the three-argument one.
fn at_syscall_takes_flags(sys: &Sys) -> bool {
    matches!(sys, Sys::FACCESSAT2)
}

/// [einval-hunt] One budgeted `error!` line naming a syscall that returned
/// `EINVAL`, for the syscall families on the X11/GLX fd-passing path. See the
/// call site in [`Syscall::syscall`]: glxgears against the finally-alive
/// Xwayland aborts with "XIO: fatal IO error 22" during DRI3 setup while pure
/// X clients run clean -- this names the failing call (from ANY process; the
/// culprit may be Xwayland's own sendmsg) instead of guessing among six
/// candidates. Budget 32/boot so legitimate early-boot EINVALs cannot starve
/// the interesting window, and so a retry loop cannot storm the console.
fn einval_hunt(pid: KoID, num: u32, args: &[usize; 6]) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static BUDGET: AtomicU32 = AtomicU32::new(0);
    // ALSA PCM HW_REFINE (`_IOWR('A', 0x10, snd_pcm_hw_params)`). alsa-lib's
    // `*_near` helpers find a supported period/rate BY issuing refine until
    // the kernel returns EINVAL — that is the search, not a fault. Logging
    // it here drowned the console the moment QEMU grew `/usr/share/alsa`
    // and `aplay`/`mpg123` actually reached the PCM node (wavplay uses OSS
    // and never hits this ioctl).
    if matches!(Sys::try_from(num), Ok(Sys::IOCTL)) {
        let cmd = args[1] as u32;
        if ((cmd >> 8) & 0xff) == b'A' as u32 && (cmd & 0xff) == 0x10 {
            return;
        }
        // DRM AUTH_MAGIC (`_IOW('d', 0x11, u32)` = 0x40046411). libdrm's
        // `drmIsMaster()` probes with magic 0; Linux answers EINVAL when
        // the caller is master (EACCES if not). EINVAL here is the probe
        // succeeding, not a broken ioctl.
        if cmd == 0x4004_6411 {
            let magic: UserInPtr<u32> = args[2].into();
            if magic.read().ok() == Some(0) {
                return;
            }
        }
    }
    let watched = matches!(
        Sys::try_from(num),
        Ok(Sys::SENDMSG
            | Sys::RECVMSG
            | Sys::SENDTO
            | Sys::RECVFROM
            | Sys::WRITEV
            | Sys::READV
            | Sys::WRITE
            | Sys::READ
            | Sys::PPOLL
            | Sys::SETSOCKOPT
            | Sys::GETSOCKOPT
            | Sys::FCNTL
            | Sys::IOCTL)
    );
    // Legacy `poll` only exists in the x86_64 table (the generic ABI that
    // riscv64/aarch64 use has `ppoll` alone), so it cannot sit in the
    // arch-neutral pattern above without breaking those builds.
    #[cfg(target_arch = "x86_64")]
    let watched = watched || matches!(Sys::try_from(num), Ok(Sys::POLL));
    if watched && BUDGET.fetch_add(1, Ordering::Relaxed) < 32 {
        log::error!(
            "[einval-hunt] pid={} syscall={} ({:?}) a0={:#x} a1={:#x} a2={:#x} a3={:#x} -> EINVAL",
            pid,
            num,
            Sys::try_from(num).ok(),
            args[0],
            args[1],
            args[2],
            args[3]
        );
    }
}

#[cfg(test)]
mod syscall_answer_tests {
    use super::*;

    /// Linux's ABI: a return in `[-4095, -1]` is an error, anything else is a
    /// value. This is the predicate every libc's syscall stub applies.
    fn reads_as_error(ret: isize) -> bool {
        (-4095..0).contains(&ret)
    }

    #[test]
    fn an_unknown_syscall_number_is_an_error_not_a_value() {
        // The number 9999 is in no table on any architecture.
        let ret = unknown_syscall_number(9999);
        assert!(
            reads_as_error(ret),
            "an unknown syscall answered {}, which userspace reads as success",
            ret
        );
        assert_eq!(ret, -(LxError::ENOSYS as isize));
    }

    #[test]
    fn every_syscall_number_linux_has_added_since_this_table_answers_enosys() {
        // What this table does not list is exactly what a modern libc probes
        // for at runtime: it issues the call and decides from -ENOSYS whether
        // the kernel has it. Answering +22 told it yes.
        for num in [
            440u32, // process_madvise
            441,    // epoll_pwait2
            443,    // quotactl_fd
            447,    // memfd_secret
            449,    // futex_waitv
            451,    // cachestat
            452,    // fchmodat2
            457,    // statmount
            1_000_000,
            u32::MAX,
        ] {
            assert!(
                Sys::try_from(num).is_err(),
                "{} is in the table; pick another for this test",
                num
            );
            let ret = unknown_syscall_number(num);
            assert!(reads_as_error(ret), "syscall {} answered {}", num, ret);
            assert_eq!(ret, -(LxError::ENOSYS as isize), "syscall {}", num);
        }
    }

    #[test]
    fn a_number_that_is_in_the_table_still_decodes() {
        // The guard against "answer ENOSYS to everything".
        assert_eq!(Sys::try_from(0), Ok(Sys::READ));
        assert_eq!(Sys::try_from(1), Ok(Sys::WRITE));
        assert!(Sys::try_from(435).is_ok(), "clone3 is in the table");
    }

    #[test]
    fn a_failed_syscall_is_the_negated_errno() {
        for err in [
            LxError::EPERM,
            LxError::ENOENT,
            LxError::EINTR,
            LxError::EAGAIN,
            LxError::ENOSYS,
            LxError::ENOSPC,
            LxError::ETIMEDOUT,
            LxError::EINPROGRESS,
        ] {
            let ret = syscall_ret(Err(err));
            assert_eq!(ret, -(err as isize), "{:?}", err);
            assert!(reads_as_error(ret), "{:?} answered {}", err, ret);
        }
    }

    #[test]
    fn a_successful_syscall_keeps_its_value() {
        assert_eq!(syscall_ret(Ok(0)), 0);
        assert_eq!(syscall_ret(Ok(4096)), 4096);
        // An address from mmap is a plain value, however large: user addresses
        // stay well below the range Linux reserves for errors.
        assert_eq!(syscall_ret(Ok(0x7fff_ffff_f000)), 0x7fff_ffff_f000);
        assert!(!reads_as_error(syscall_ret(Ok(0x7fff_ffff_f000))));
    }

    #[test]
    fn a_three_argument_at_syscall_is_given_no_flags_and_a_four_argument_one_the_register() {
        // AT_SYMLINK_NOFOLLOW is 0x100 and AT_EACCESS is 0x200: the two bits
        // in a garbage register that change what access() and chmod() do.
        for garbage in [0x100usize, 0x200, 0x300, 0xdead_beef, usize::MAX] {
            assert_eq!(
                at_flags_register(false, garbage),
                0,
                "a SYSCALL_DEFINE3 has three arguments; the fourth register is not its own"
            );
            assert_eq!(at_flags_register(true, garbage), garbage);
        }
        assert_eq!(at_flags_register(true, 0), 0);
    }

    #[test]
    fn faccessat2_has_a_fourth_argument_and_faccessat_and_fchmodat_do_not() {
        assert!(!at_syscall_takes_flags(&Sys::FACCESSAT));
        assert!(at_syscall_takes_flags(&Sys::FACCESSAT2));
        // `SYSCALL_DEFINE3(fchmodat, int, dfd, const char __user *, filename,
        // umode_t, mode)`: glibc's chmod() and musl's are this call with
        // three arguments, and whatever the stub left in the fourth register
        // used to arrive here as AT_ flags.
        assert!(!at_syscall_takes_flags(&Sys::FCHMODAT));
    }
}

#[cfg(test)]
mod benches {
    //! The dispatch layer: what every syscall pays before its handler runs,
    //! and what it pays on the way out.
    //!
    //! `Sys::try_from` is the one that matters most, because it is not called
    //! once per syscall. The dispatch decodes the number, and on an `EINVAL`
    //! the `[einval-hunt]` logging decodes it up to **four more times** (the
    //! ALSA early-out, the watched-family match, the x86-only `POLL` arm and
    //! the `error!` line). Whether that is free depends on whether the
    //! generated `TryFrom` is a jump table or a chain of compares, which a
    //! single row cannot say -- so it is benched across the table: the first
    //! number, the middle, the far end, Eclipse's own numbers past 600, and a
    //! number that is not in the table at all.
    //!
    //! Every row black-boxes its inputs. Two cycles (~0.6 ns) is `b.iter`'s
    //! own loop; a row there with an opaque input is a real compare and
    //! branch, a row there with a constant input is a call that was folded
    //! away and never ran.

    use super::*;
    use test::{black_box, Bencher};

    /// `read`: number 0, the first arm of the generated match.
    #[bench]
    fn sys_try_from_the_first_number(b: &mut Bencher) {
        b.iter(|| black_box(Sys::try_from(black_box(0u32))));
    }

    /// `ioctl`: number 16, and the number `einval_hunt` tests for first.
    #[bench]
    fn sys_try_from_ioctl(b: &mut Bencher) {
        b.iter(|| black_box(Sys::try_from(black_box(16u32))));
    }

    /// Near the middle of the Linux range.
    #[bench]
    fn sys_try_from_the_middle(b: &mut Bencher) {
        b.iter(|| black_box(Sys::try_from(black_box(202u32))));
    }

    /// `faccessat2` (439): the far end of the Linux numbers, and the one
    /// `at_syscall_takes_flags` answers yes for.
    #[bench]
    fn sys_try_from_the_last_linux_number(b: &mut Bencher) {
        b.iter(|| black_box(Sys::try_from(black_box(439u32))));
    }

    /// Eclipse's own numbers sit at 600 and above, past a gap of a hundred
    /// and sixty: if the decode is a dense jump table, that gap is where it
    /// stops being one.
    #[bench]
    fn sys_try_from_an_eclipse_number(b: &mut Bencher) {
        b.iter(|| black_box(Sys::try_from(black_box(601u32))));
    }

    /// A number this kernel does not have, which is what glibc and musl
    /// issue on purpose to find out whether a syscall exists. It must come
    /// back as an error, and it is the only answer that walks the whole
    /// match if the decode is a chain.
    #[bench]
    fn sys_try_from_a_number_not_in_the_table(b: &mut Bencher) {
        b.iter(|| black_box(Sys::try_from(black_box(452u32))));
    }

    /// The `[einval-hunt]` decision for an `ioctl` that is ALSA's
    /// `PCM_HW_REFINE`: alsa-lib finds a supported rate BY issuing refine
    /// until the kernel says `EINVAL`, so this early-out is on a hot loop
    /// whenever anything plays sound. It is the first of the decodes.
    #[bench]
    fn einval_hunt_alsa_early_out_decision(b: &mut Bencher) {
        // The shape of the test inside `einval_hunt`, without the logging:
        // decode, then two byte compares on the ioctl command.
        let cmd = 0xc250_4110u32; // _IOWR('A', 0x10, snd_pcm_hw_params)
        b.iter(|| {
            let cmd = black_box(cmd);
            black_box(
                matches!(Sys::try_from(black_box(16u32)), Ok(Sys::IOCTL))
                    && ((cmd >> 8) & 0xff) == b'A' as u32
                    && (cmd & 0xff) == 0x10,
            )
        });
    }

    /// Turning a handler's `SysResult` into the register the caller reads.
    /// Every syscall ends here, twice over in the `bsd` personality, which
    /// re-encodes it again.
    #[bench]
    fn syscall_ret_of_a_success(b: &mut Bencher) {
        b.iter(|| black_box(syscall_ret(black_box(Ok(4096)))));
    }

    #[bench]
    fn syscall_ret_of_an_error(b: &mut Bencher) {
        b.iter(|| black_box(syscall_ret(black_box(Err(LxError::EINVAL)))));
    }

    /// Which register a `*at` syscall's flags come from, and whether it has
    /// any. This pair is what stopped a three-argument `faccessat` from
    /// reading whatever libc's stub left in the fourth register as `AT_`
    /// flags, so it runs on every `access`, `chmod` and `stat` a shell makes.
    #[bench]
    fn at_flags_register_for_a_three_argument_call(b: &mut Bencher) {
        b.iter(|| black_box(at_flags_register(black_box(false), black_box(0xdead_beef))));
    }

    #[bench]
    fn at_syscall_takes_flags_decision(b: &mut Bencher) {
        b.iter(|| black_box(at_syscall_takes_flags(black_box(&Sys::FACCESSAT2))));
    }

    /// The `/proc/perf` resolver registration, which the dispatch calls on
    /// **every** syscall. After the first one it is a plain acquire load of
    /// an already-set flag.
    ///
    /// This row reads **below** the 0.6 ns floor, and that is the answer, not
    /// a mistake: the function returns `()`, so there is no value to hold on
    /// to, and an acquire load of a static the loop never writes is something
    /// the compiler is free to hoist out of it. Read it as "the steady-state
    /// cost is one load, and not even a reliably repeated one", which is the
    /// same thing the comment at the call site claims. What it does NOT
    /// measure is the `compare_exchange` behind the load, which the very
    /// first syscall of the boot pays once.
    #[bench]
    fn perf_accounting_ensure_registered(b: &mut Bencher) {
        // Warm it, so whatever is left of the row is the steady state.
        perf_accounting::ensure_registered();
        // Handed to `iter` as a function rather than wrapped in `black_box`:
        // `ensure_registered` returns `()`, and black-boxing a unit is a
        // no-op that only reads as if it did something. `Bencher::iter`
        // black-boxes whatever the closure returns anyway.
        b.iter(perf_accounting::ensure_registered);
    }

    /// A control: a function that decides nothing, takes a register and hands
    /// back the same `LxResult<usize>` the argument helpers do.
    ///
    /// It exists because of a pattern across these rows that would otherwise
    /// be read as a cost. Dozens of unrelated one-compare helpers -- in
    /// `intarg`, `vm`, `net`, `ipc`, `misc`, and `Sys::try_from` here -- land
    /// between 6.0 and 7.0 ns, and crucially the *accepted* and *refused* row
    /// of the same helper land there together, to within noise, although they
    /// take different branches and return different values. A figure that
    /// does not move when the work does is not the work. So this harness has
    /// a **second floor** above the 0.6 ns one, and a row sitting on it means
    /// "too cheap to measure this way", not "6.5 nanoseconds of decision".
    /// Compare any such row against this one before reading a cost into it.
    #[bench]
    fn the_second_floor_control(b: &mut Bencher) {
        #[inline(never)]
        fn decides_nothing(raw: usize) -> LxResult<usize> {
            Ok(raw)
        }
        b.iter(|| black_box(decides_nothing(black_box(1234))));
    }

    /// `copy_struct_from_user`'s rule: the bytes past what this kernel knows
    /// of an extensible struct must all be zero, or the caller is asking for
    /// a feature it will not get (`E2BIG`). `clone3`, `sched_setattr` and the
    /// rest each check their own tail, and the tail's length is the caller's
    /// `size` minus ours -- so it is a scan with a length userspace picks.
    fn bench_extensible_tail(b: &mut Bencher, n: usize) {
        let tail = alloc::vec![0u8; n];
        b.iter(|| black_box(extensible_tail_is_empty(black_box(&tail))));
    }

    /// The common case: the caller built the same version we know, so there
    /// is no tail at all.
    #[bench]
    fn extensible_tail_of_0(b: &mut Bencher) {
        bench_extensible_tail(b, 0);
    }

    /// 24 bytes: what a `clone3` from a libc newer than this kernel carries
    /// past `CLONE_ARGS_SIZE_VER0`.
    #[bench]
    fn extensible_tail_of_24(b: &mut Bencher) {
        bench_extensible_tail(b, 24);
    }

    #[bench]
    fn extensible_tail_of_4096(b: &mut Bencher) {
        bench_extensible_tail(b, 4096);
    }

    /// A tail whose first byte is set, which is the refusal: it must stop
    /// there rather than read the rest of what the caller named.
    #[bench]
    fn extensible_tail_refused_at_the_first_byte(b: &mut Bencher) {
        let mut tail = alloc::vec![0u8; 4096];
        tail[0] = 1;
        b.iter(|| black_box(extensible_tail_is_empty(black_box(&tail))));
    }

    /// The fallible, zeroed kernel buffer **every** allocation in this crate
    /// whose size comes from userspace goes through. `vec![0u8; n]` here took
    /// the machine down on a 24 KiB `read`; this is what the fallible version
    /// costs instead.
    fn bench_zeroed_buf(b: &mut Bencher, n: usize) {
        b.iter(|| black_box(try_zeroed_buf(black_box(n))));
    }

    /// One page: the size of an ordinary `read`.
    #[bench]
    fn try_zeroed_buf_of_4_kib(b: &mut Bencher) {
        bench_zeroed_buf(b, 4096);
    }

    /// `SYSCALL_IO_MAX`, the ceiling one read or write may ask for.
    #[bench]
    fn try_zeroed_buf_of_64_kib(b: &mut Bencher) {
        bench_zeroed_buf(b, SYSCALL_IO_MAX);
    }

    /// The length no allocator can satisfy, which must come back as `ENOMEM`
    /// rather than abort the machine. If the refusal is cheap, the fallible
    /// path costs nothing even when a process hammers it.
    #[bench]
    fn try_zeroed_buf_refused(b: &mut Bencher) {
        bench_zeroed_buf(b, usize::MAX);
    }

    /// Naming an ALSA ioctl for the log. The guard in front of the table is
    /// the interesting part: it rejects the legacy TTY numbers, whose second
    /// byte is also `b'T'`, so every `isatty()` probe on a non-tty fd used to
    /// be read as an ALSA timer call and reported. That flooded the console.
    #[bench]
    fn alsa_ioctl_name_rejects_a_tty_number(b: &mut Bencher) {
        // TCGETS: a flat legacy number with no direction and no payload size.
        b.iter(|| black_box(alsa_ioctl_name(black_box(0x5401))));
    }

    #[bench]
    fn alsa_ioctl_name_of_a_pcm_call(b: &mut Bencher) {
        b.iter(|| black_box(alsa_ioctl_name(black_box(0xc250_4110))));
    }
}
