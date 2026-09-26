//! FreeBSD/amd64 system-call personality.
//!
//! A process whose ELF is tagged `ELFOSABI_FREEBSD` (or carries a FreeBSD ABI
//! note) enters the kernel through the same `syscall` instruction as a Linux
//! process, but with FreeBSD's *numbers*, *flag encodings*, *struct layouts*
//! and *return convention*. This module bridges that gap by translating each
//! call onto the existing Linux [`Syscall`] implementation and then re-encoding
//! the result the FreeBSD way.
//!
//! ## Return convention (amd64)
//!
//! FreeBSD's syscall return differs from Linux's "negative errno in `rax`":
//! `cpu_set_syscall_retval` (sys/amd64/amd64/vm_machdep.c) puts the primary
//! result in `rax`, a secondary result in `rdx`, and signals an error by
//! *setting the carry flag* with the positive errno in `rax`. libc's syscall
//! stubs branch on carry (`jb .cerror`). [`BsdRet`] carries those three pieces
//! back to the trap handler, which applies them to the user context.
//!
//! ## Scope
//!
//! This is a compatibility *foundation*, not a complete FreeBSD kernel. The
//! file I/O, memory, process and time calls a static binary needs to reach
//! `main` are translated; signals (sigframe/`sigreturn`), the `thr_*`
//! threading ABI, `_umtx_op` contention and the dynamic `rtld` are stubbed or
//! absent and documented in `docs/README-freebsd.md`. Anything unhandled
//! returns `ENOSYS`, exactly as a FreeBSD kernel does for a syscall its running
//! configuration lacks.

use super::*;
use consts::sys;
use kernel_hal::context::UserContextField;

pub mod consts;
pub mod errno;
pub mod fs;
pub mod sysctl;
pub mod translate;

/// The three values a FreeBSD/amd64 syscall hands back to userland.
pub struct BsdRet {
    /// Primary return value (`rax`), or the positive errno on error.
    pub rax: usize,
    /// Secondary return value (`rdx`) — used by a few calls (`fork`, `pipe`,
    /// `getpid`'s legacy form); zero for everything else.
    pub rdx: usize,
    /// Whether the call failed. When set, the trap handler raises the carry
    /// flag and `rax` holds the FreeBSD errno.
    pub error: bool,
}

impl BsdRet {
    fn ok(v: usize) -> Self {
        BsdRet {
            rax: v,
            rdx: 0,
            error: false,
        }
    }
    fn ok2(v: usize, v2: usize) -> Self {
        BsdRet {
            rax: v,
            rdx: v2,
            error: false,
        }
    }
    fn err(errno: i32) -> Self {
        BsdRet {
            rax: errno as usize,
            rdx: 0,
            error: true,
        }
    }
    fn enosys() -> Self {
        BsdRet::err(consts::errno::ENOSYS)
    }
    fn from_result(r: SysResult) -> Self {
        match r {
            Ok(v) => BsdRet::ok(v),
            Err(e) => BsdRet::err(errno::lx_to_freebsd(e)),
        }
    }
    /// A flag word this layer refused to translate, as a FreeBSD error.
    fn from_lx(e: LxError) -> Self {
        BsdRet::err(errno::lx_to_freebsd(e))
    }
}

/// Translate a FreeBSD `clockid_t` to the Linux one `sys_clock_*` expects.
///
/// The two disagree past `CLOCK_REALTIME` (both 0): FreeBSD `CLOCK_MONOTONIC`
/// is 4 where Linux uses 1, and the CPU-time clocks are renumbered too
/// (`sys/sys/_clock_id.h` vs `include/uapi/linux/time.h`).
fn clockid_to_linux(bsd: usize) -> usize {
    match bsd {
        // REALTIME and its _PRECISE(9)/_FAST(10)/SECOND(13) variants.
        0 | 9 | 10 | 13 => 0,
        // MONOTONIC(4) and the UPTIME(5,7,8) / MONOTONIC_PRECISE(11)/_FAST(12) family.
        4 | 5 | 7 | 8 | 11 | 12 => 1,
        15 => 2, // PROCESS_CPUTIME_ID
        14 => 3, // THREAD_CPUTIME_ID
        other => other,
    }
}

impl Syscall<'_> {
    /// Dispatch one FreeBSD/amd64 system call and return its FreeBSD-encoded
    /// result. Called by the trap handler when the faulting process's
    /// personality is FreeBSD.
    pub async fn bsd_syscall(&mut self, num: usize, args: [usize; 6]) -> BsdRet {
        // Terminal Ctrl-C is delivered the same way as on the Linux path.
        if self.maybe_handle_tty_intr().is_err() {
            return BsdRet::err(consts::errno::EINTR);
        }

        // syscall(2) / __syscall(2): the real number is the first argument and
        // the rest shift down. amd64 rarely needs more than five args this way,
        // so the sixth is dropped (documented limitation).
        let (num, args) = if num == sys::SYSCALL || num == sys::UNDER_SYSCALL {
            let [_, a1, a2, a3, a4, a5] = args;
            (args[0], [a1, a2, a3, a4, a5, 0])
        } else {
            (num, args)
        };

        let [a0, a1, a2, a3, a4, a5] = args;
        debug!("freebsd syscall: num={} args={:x?}", num, args);

        match num {
            // ---- file I/O ---------------------------------------------------
            sys::READ => BsdRet::from_result(self.sys_read(a0.into(), a1.into(), a2).await),
            sys::WRITE => BsdRet::from_result(self.sys_write(a0.into(), a1.into(), a2).await),
            sys::READV => BsdRet::from_result(self.sys_readv(a0.into(), a1.into(), a2).await),
            sys::WRITEV => BsdRet::from_result(self.sys_writev(a0.into(), a1.into(), a2).await),
            sys::PREAD => {
                BsdRet::from_result(self.sys_pread(a0.into(), a1.into(), a2, a3 as _).await)
            }
            sys::PWRITE => BsdRet::from_result(self.sys_pwrite(a0.into(), a1.into(), a2, a3 as _)),
            sys::OPEN => match translate::open_flags_to_linux(a1 as i32) {
                Err(e) => BsdRet::from_lx(e),
                Ok(f) => {
                    BsdRet::from_result(self.sys_openat(FileDesc::CWD, a0.into(), f as usize, a2))
                }
            },
            sys::OPENAT => match translate::open_flags_to_linux(a2 as i32) {
                Err(e) => BsdRet::from_lx(e),
                Ok(f) => BsdRet::from_result(self.sys_openat(a0.into(), a1.into(), f as usize, a3)),
            },
            sys::CLOSE => BsdRet::from_result(self.sys_close(a0.into())),
            sys::CLOSE_RANGE => BsdRet::from_result(self.sys_close_range(a0, a1, a2)),
            sys::LSEEK => BsdRet::from_result(self.sys_lseek(a0.into(), a1 as i64, a2)),
            sys::FSYNC => BsdRet::from_result(self.sys_fsync(a0.into())),
            sys::FDATASYNC => BsdRet::from_result(self.sys_fdatasync(a0.into())),
            sys::TRUNCATE => BsdRet::from_result(self.sys_truncate(a0.into(), a1)),
            sys::FTRUNCATE => BsdRet::from_result(self.sys_ftruncate(a0.into(), a1)),
            sys::DUP => BsdRet::from_result(self.sys_dup(a0.into())),
            sys::DUP2 => BsdRet::from_result(self.sys_dup2(a0.into(), a1)),
            // The command numbers and the `F_SETFL`/`F_GETFL` flag words
            // differ from Linux's past the five lowest commands; see
            // `translate::fcntl_to_linux`.
            sys::FCNTL => match translate::fcntl_to_linux(a1, a2) {
                Err(e) => BsdRet::from_lx(e),
                Ok(translate::Fcntl::Dup2 { target, cloexec }) => BsdRet::from_result(if cloexec {
                    self.sys_dup3(a0.into(), target, 0o2000000)
                } else {
                    self.sys_dup2(a0.into(), target)
                }),
                Ok(translate::Fcntl::Linux { cmd, arg }) => {
                    let r = self.sys_fcntl(a0.into(), cmd, arg).await;
                    BsdRet::from_result(if cmd == consts::lin_fcntl::F_GETFL {
                        r.map(|fl| translate::open_flags_from_linux(fl as i32) as u32 as usize)
                    } else {
                        r
                    })
                }
            },
            sys::FLOCK => BsdRet::from_result(self.sys_flock(a0.into(), a1)),
            sys::GETCWD => BsdRet::from_result(self.sys_getcwd(a0.into(), a1)),
            sys::FCHDIR => BsdRet::from_result(self.sys_fchdir(a0.into())),
            sys::CHDIR => BsdRet::from_result(self.sys_chdir(a0.into())),
            sys::FCHMOD => BsdRet::from_result(self.sys_fchmod(a0.into(), a1)),
            sys::FCHOWN => BsdRet::from_result(self.sys_fchown(a0.into(), a1, a2)),
            sys::CHMOD => BsdRet::from_result(self.sys_chmod(a0.into(), a1)),
            sys::ACCESS => BsdRet::from_result(self.sys_access(a0.into(), a1)),
            sys::FACCESSAT => match translate::at_flags_to_linux(a3 as i32) {
                Err(e) => BsdRet::from_lx(e),
                Ok(f) => {
                    BsdRet::from_result(self.sys_faccessat(a0.into(), a1.into(), a2, f as usize))
                }
            },
            sys::FCHMODAT => BsdRet::from_result(self.sys_fchmodat(a0.into(), a1.into(), a2, a3)),
            sys::FCHOWNAT => match translate::at_flags_to_linux(a4 as i32) {
                Err(e) => BsdRet::from_lx(e),
                Ok(f) => {
                    BsdRet::from_result(self.sys_fchownat(a0.into(), a1.into(), a2, a3, f as usize))
                }
            },
            sys::MKDIR => BsdRet::from_result(self.sys_mkdir(a0.into(), a1)),
            sys::MKDIRAT => BsdRet::from_result(self.sys_mkdirat(a0.into(), a1.into(), a2)),
            sys::RMDIR => BsdRet::from_result(self.sys_rmdir(a0.into())),
            sys::LINK => BsdRet::from_result(self.sys_link(a0.into(), a1.into())),
            sys::LINKAT => match translate::at_flags_to_linux(a4 as i32) {
                Err(e) => BsdRet::from_lx(e),
                Ok(f) => BsdRet::from_result(self.sys_linkat(
                    a0.into(),
                    a1.into(),
                    a2.into(),
                    a3.into(),
                    f as usize,
                )),
            },
            sys::UNLINK => BsdRet::from_result(self.sys_unlink(a0.into())),
            sys::UNLINKAT => match translate::at_flags_to_linux(a2 as i32) {
                Err(e) => BsdRet::from_lx(e),
                Ok(f) => BsdRet::from_result(self.sys_unlinkat(a0.into(), a1.into(), f as usize)),
            },
            sys::RENAME => BsdRet::from_result(self.sys_rename(a0.into(), a1.into())),
            sys::RENAMEAT => {
                BsdRet::from_result(self.sys_renameat(a0.into(), a1.into(), a2.into(), a3.into()))
            }
            sys::SYMLINKAT => {
                BsdRet::from_result(self.sys_symlinkat(a0.into(), a1.into(), a2.into()))
            }
            sys::READLINKAT => {
                BsdRet::from_result(self.sys_readlinkat(a0.into(), a1.into(), a2.into(), a3))
            }
            sys::MKNODAT => BsdRet::from_result(self.sys_mknodat(a0.into(), a1.into(), a2, a3)),
            sys::UMASK => BsdRet::from_result(self.sys_umask(a0)),

            // ---- FreeBSD-specific struct layouts ----------------------------
            sys::FSTAT => self.bsd_fstat(a0.into(), a1.into()),
            sys::FSTATAT => match translate::at_flags_to_linux(a3 as i32) {
                Err(e) => BsdRet::from_lx(e),
                Ok(f) => self.bsd_fstatat(a0.into(), a1.into(), a2.into(), f as usize),
            },
            sys::GETDIRENTRIES => self.bsd_getdirentries(a0.into(), a1.into(), a2, a3.into()),

            // ---- memory -----------------------------------------------------
            sys::MMAP => match translate::mmap_flags_to_linux(a3 as i32) {
                Err(e) => BsdRet::from_lx(e),
                Ok(f) => BsdRet::from_result(
                    self.sys_mmap(a0, a1, a2, f as usize, a4.into(), a5 as u64)
                        .await,
                ),
            },
            sys::MUNMAP => BsdRet::from_result(self.sys_munmap(a0, a1)),
            sys::MPROTECT => BsdRet::from_result(self.sys_mprotect(a0, a1, a2)),
            sys::MADVISE => BsdRet::from_result(self.sys_madvise(a0, a1, a2)),
            sys::MSYNC => BsdRet::from_result(self.sys_msync(a0, a1, a2)),
            sys::BREAK => BsdRet::from_result(self.sys_brk(a0)),

            // ---- process ----------------------------------------------------
            sys::GETPID => BsdRet::ok2(self.zircon_process().id() as usize, 0),
            sys::GETPPID => BsdRet::from_result(self.sys_getppid()),
            sys::GETUID => BsdRet::from_result(self.sys_getuid()),
            sys::GETEUID => BsdRet::from_result(self.sys_geteuid()),
            sys::GETGID => BsdRet::from_result(self.sys_getgid()),
            sys::GETEGID => BsdRet::from_result(self.sys_getegid()),
            sys::SETUID => BsdRet::from_result(self.sys_setuid(a0)),
            sys::SETGID => BsdRet::from_result(self.sys_setgid(a0)),
            sys::SETEUID => BsdRet::from_result(self.sys_setresuid(usize::MAX, a0, usize::MAX)),
            sys::SETEGID => BsdRet::from_result(self.sys_setresgid(usize::MAX, a0, usize::MAX)),
            sys::GETPGRP => BsdRet::from_result(self.sys_getpgid(0)),
            sys::GETPGID => BsdRet::from_result(self.sys_getpgid(a0 as _)),
            sys::SETPGID => BsdRet::from_result(self.sys_setpgid(a0 as _, a1 as _)),
            // getsid(2), not getpgid(2). The two answer the same number only
            // for a session leader; for every job a shell starts they differ,
            // and a FreeBSD binary asking which session it is in was told
            // which process group it is in.
            sys::GETSID => BsdRet::from_result(self.sys_getsid(a0 as _)),
            sys::SETSID => BsdRet::from_result(self.sys_setsid()),
            // `issetugid(2)`: 1 when this process's ids are not the ones
            // it was started with. A constant 0 is the answer a program
            // trusts before it decides that the environment it was handed is
            // its own -- which is exactly the decision a set-user-ID program
            // must not make. `P_SUGID` is kept on the process; see
            // `LinuxProcess::is_sugid`.
            sys::ISSETUGID => BsdRet::ok(self.linux_process().is_sugid() as usize),
            sys::KILL => BsdRet::from_result(self.sys_kill(a0 as isize, a1)),
            sys::FORK => BsdRet::from_result(self.sys_fork(0, 0)),
            sys::VFORK => BsdRet::from_result(self.sys_vfork(0, 0).await),
            // The option bits past `WNOHANG | WUNTRACED` are on different
            // positions from Linux's; see `translate::wait_options_to_linux`.
            sys::WAIT4 => match translate::wait_options_to_linux(a2 as i32) {
                Err(e) => BsdRet::from_lx(e),
                Ok(options) => BsdRet::from_result(
                    self.sys_wait4(a0 as _, a1.into(), options as u32, a3.into())
                        .await,
                ),
            },
            sys::EXECVE => BsdRet::from_result(self.sys_execve(a0.into(), a1.into(), a2.into())),
            sys::EXIT => BsdRet::from_result(self.sys_exit(a0 as _)),
            sys::THR_EXIT => BsdRet::from_result(self.sys_exit(0)),
            sys::SETPRIORITY => BsdRet::from_result(self.sys_setpriority(a0, a1, a2 as i32)),
            sys::GETPRIORITY => BsdRet::from_result(self.sys_getpriority(a0, a1)),
            sys::GETRUSAGE => BsdRet::from_result(self.sys_getrusage(a0, a1.into())),
            sys::YIELD | sys::SCHED_YIELD => {
                kernel_hal::thread::yield_now().await;
                BsdRet::ok(0)
            }
            sys::SCHED_GETCPU => BsdRet::ok(kernel_hal::cpu::cpu_id() as usize),
            sys::GETRLIMIT => BsdRet::from_result(self.sys_getrlimit(a0, a1.into())),
            sys::SETRLIMIT => BsdRet::from_result(self.sys_setrlimit(a0, a1.into())),
            sys::GETRANDOM => BsdRet::from_result(self.sys_getrandom(a0.into(), a1, a2 as u32)),

            // ---- threads (amd64 thr ABI) ------------------------------------
            sys::THR_SELF => self.bsd_thr_self(a0.into()),
            // Single-threaded programs only reach _umtx_op on lock contention,
            // which cannot happen with one thread; answer success as a no-op.
            // Real multi-threaded support needs thr_new + a umtx queue (see
            // docs/README-freebsd.md).
            sys::UMTX_OP => BsdRet::ok(0),
            sys::THR_SET_NAME => BsdRet::ok(0),

            // ---- time -------------------------------------------------------
            sys::NANOSLEEP => BsdRet::from_result(self.sys_nanosleep(a0.into(), a1.into()).await),
            sys::CLOCK_NANOSLEEP => BsdRet::from_result(
                self.sys_clock_nanosleep(clockid_to_linux(a0), a1, a2.into(), a3.into())
                    .await,
            ),
            sys::CLOCK_GETTIME => {
                BsdRet::from_result(self.sys_clock_gettime(clockid_to_linux(a0), a1.into()))
            }
            sys::CLOCK_GETRES => {
                BsdRet::from_result(self.sys_clock_getres(clockid_to_linux(a0), a1.into()))
            }
            sys::GETTIMEOFDAY => BsdRet::from_result(self.sys_gettimeofday(a0.into(), a1.into())),

            // ---- machine / sysctl -------------------------------------------
            sys::SYSARCH => self.bsd_sysarch(a0 as i32, a1),
            sys::SYSCTL => self.bsd_sysctl(a0.into(), a1, a2, a3.into()),
            sys::SYSCTLBYNAME => self.bsd_sysctlbyname(a0.into(), a1, a2, a3.into()),

            // ---- signals: accepted but not delivered (documented gap) -------
            // Installing a handler returns success so startup code proceeds;
            // actual delivery uses the default disposition because the FreeBSD
            // sigframe/sigreturn path is not implemented.
            sys::SIGACTION => BsdRet::ok(0),
            sys::SIGPROCMASK => BsdRet::ok(0),

            other => {
                warn!("freebsd: unhandled syscall {} -> ENOSYS", other);
                BsdRet::enosys()
            }
        }
    }

    /// FreeBSD `fstat(fd, struct stat *)`.
    fn bsd_fstat(&self, fd: FileDesc, buf: UserOutPtr<u8>) -> BsdRet {
        match self.linux_process().get_file(fd).and_then(|f| f.metadata()) {
            Ok(meta) => self.write_bsd_stat(buf, &meta),
            Err(e) => BsdRet::err(errno::lx_to_freebsd(e)),
        }
    }

    /// FreeBSD `fstatat(fd, path, struct stat *, flags)`.
    fn bsd_fstatat(
        &self,
        dirfd: FileDesc,
        path: UserInPtr<u8>,
        buf: UserOutPtr<u8>,
        flags: usize,
    ) -> BsdRet {
        let follow = flags as i32 & consts::lin_oflags::AT_SYMLINK_NOFOLLOW == 0;
        let meta = path
            .as_c_str()
            .map_err(LxError::from)
            .and_then(|p| self.linux_process().lookup_inode_at(dirfd, p, follow))
            .and_then(|inode| inode.metadata().map_err(LxError::from));
        match meta {
            Ok(meta) => self.write_bsd_stat(buf, &meta),
            Err(e) => BsdRet::err(errno::lx_to_freebsd(e)),
        }
    }

    fn write_bsd_stat(
        &self,
        mut buf: UserOutPtr<u8>,
        meta: &linux_object::fs::vfs::Metadata,
    ) -> BsdRet {
        let stat = fs::BsdStat::from_metadata(meta);
        match buf.write_array(&stat.to_bytes()) {
            Ok(()) => BsdRet::ok(0),
            Err(e) => BsdRet::err(errno::lx_to_freebsd(LxError::from(e))),
        }
    }

    /// FreeBSD `getdirentries(fd, buf, nbytes, *basep)`. Emits FreeBSD `dirent`
    /// records (different layout from Linux `getdents64`).
    fn bsd_getdirentries(
        &self,
        fd: FileDesc,
        mut buf: UserOutPtr<u8>,
        nbytes: usize,
        mut basep: UserOutPtr<i64>,
    ) -> BsdRet {
        use linux_object::fs::vfs::FileType;
        let proc = self.linux_process();
        let file = match proc.get_file(fd) {
            Ok(f) => f,
            Err(e) => return BsdRet::err(errno::lx_to_freebsd(e)),
        };
        match file.metadata() {
            Ok(m) if m.type_ == FileType::Dir => {}
            Ok(_) => return BsdRet::err(consts::errno::ENOTDIR),
            Err(e) => return BsdRet::err(errno::lx_to_freebsd(e)),
        }
        let mut writer = fs::BsdDirentWriter::new(nbytes.min(256 * 1024));
        let mut file = file;
        let base = match read_dirents_with_base(&mut file, &mut writer) {
            Ok(base) => base,
            Err(e) => return BsdRet::err(errno::lx_to_freebsd(e)),
        };
        if let Err(e) = buf.write_array(writer.as_slice()) {
            return BsdRet::err(errno::lx_to_freebsd(LxError::from(e)));
        }
        // Not all callers pass `basep`; libc's `readdir` does, and keeps it
        // as the position `telldir` reports for this buffer.
        let _ = basep.write_if_not_null(base);
        BsdRet::ok(writer.len())
    }

    /// FreeBSD `thr_self(long *id)` — store this thread's id and return 0.
    fn bsd_thr_self(&self, mut id: UserOutPtr<i64>) -> BsdRet {
        match id.write(self.thread.id() as i64) {
            Ok(()) => BsdRet::ok(0),
            Err(e) => BsdRet::err(errno::lx_to_freebsd(LxError::from(e))),
        }
    }

    /// FreeBSD `sysarch(op, void *parms)` for the amd64 TLS-base operations
    /// (`sys/amd64/amd64/sys_machdep.c`). `parms` is a pointer to the base
    /// value, which the kernel copies in/out.
    fn bsd_sysarch(&mut self, op: i32, parms: usize) -> BsdRet {
        use consts::sysarch::*;
        match op {
            AMD64_SET_FSBASE => {
                let p = UserInPtr::<u64>::from(parms);
                match p.read() {
                    Ok(base) => {
                        let _ = self.thread.with_context(|ctx| {
                            ctx.set_field(UserContextField::ThreadPointer, base as usize)
                        });
                        BsdRet::ok(0)
                    }
                    Err(e) => BsdRet::err(errno::lx_to_freebsd(LxError::from(e))),
                }
            }
            AMD64_GET_FSBASE => {
                let base = self
                    .thread
                    .with_context(|ctx| ctx.get_field(UserContextField::ThreadPointer))
                    .unwrap_or(0);
                let mut p = UserOutPtr::<u64>::from(parms);
                match p.write(base as u64) {
                    Ok(()) => BsdRet::ok(0),
                    Err(e) => BsdRet::err(errno::lx_to_freebsd(LxError::from(e))),
                }
            }
            // GSBASE is not used for TLS on amd64 userland; accept-and-ignore.
            AMD64_SET_GSBASE | AMD64_GET_GSBASE => BsdRet::ok(0),
            _ => BsdRet::err(consts::errno::EINVAL),
        }
    }

    /// Assemble the [`sysctl::SysctlCtx`] from live kernel/process state.
    fn sysctl_ctx(&self) -> sysctl::SysctlCtx {
        let mut arnd = [0u8; 16];
        kernel_hal::rand::fill_random(&mut arnd);
        // USER_ASPACE_BASE / USER_ASPACE_SIZE are already u64.
        let usrstack = zircon_object::vm::USER_ASPACE_BASE + zircon_object::vm::USER_ASPACE_SIZE;
        sysctl::SysctlCtx {
            ncpus: kernel_hal::vdso::vdso_constants().max_num_cpus.max(1),
            page_size: zircon_object::vm::PAGE_SIZE as u32,
            usrstack,
            ps_strings: usrstack,
            physmem: 512 * 1024 * 1024,
            hostname: linux_object::uname::hostname(),
            arnd,
        }
    }

    /// FreeBSD `__sysctl(name, namelen, oldp, oldlenp, newp, newlen)` for
    /// integer MIBs.
    fn bsd_sysctl(
        &self,
        name: UserInPtr<i32>,
        namelen: usize,
        oldp: usize,
        oldlenp: UserInOutPtr<usize>,
    ) -> BsdRet {
        if namelen == 0 || namelen > 24 {
            return BsdRet::err(consts::errno::EINVAL);
        }
        let mib = match name.read_array(namelen) {
            Ok(m) => m,
            Err(e) => return BsdRet::err(errno::lx_to_freebsd(LxError::from(e))),
        };
        let ctx = self.sysctl_ctx();
        match sysctl::query(&mib, &ctx) {
            Some(val) => self.copyout_sysctl(&val.to_bytes(), oldp, oldlenp),
            None => BsdRet::err(consts::errno::ENOENT),
        }
    }

    /// FreeBSD `__sysctlbyname(name, namelen, oldp, oldlenp, newp, newlen)`.
    fn bsd_sysctlbyname(
        &self,
        name: UserInPtr<u8>,
        _namelen: usize,
        oldp: usize,
        oldlenp: UserInOutPtr<usize>,
    ) -> BsdRet {
        let name = match name.as_c_str() {
            Ok(s) => s,
            Err(e) => return BsdRet::err(errno::lx_to_freebsd(LxError::from(e))),
        };
        let mib = match sysctl::name_to_mib(name) {
            Some(m) => m,
            None => return BsdRet::err(consts::errno::ENOENT),
        };
        let ctx = self.sysctl_ctx();
        match sysctl::query(&mib, &ctx) {
            Some(val) => self.copyout_sysctl(&val.to_bytes(), oldp, oldlenp),
            None => BsdRet::err(consts::errno::ENOENT),
        }
    }

    /// Shared `oldp`/`oldlenp` copy-out logic for both sysctl entry points,
    /// matching FreeBSD's contract: a NULL `oldp` just reports the size; a
    /// too-small buffer yields `ENOMEM` with the required size written back.
    fn copyout_sysctl(&self, data: &[u8], oldp: usize, mut oldlenp: UserInOutPtr<usize>) -> BsdRet {
        let want = data.len();
        let avail = if oldlenp.is_null() {
            0
        } else {
            oldlenp.read().unwrap_or(0)
        };
        if oldp != 0 {
            if avail < want {
                let _ = oldlenp.write(want);
                return BsdRet::err(consts::errno::ENOMEM);
            }
            let mut out = UserOutPtr::<u8>::from(oldp);
            if let Err(e) = out.write_array(data) {
                return BsdRet::err(errno::lx_to_freebsd(LxError::from(e)));
            }
        }
        if !oldlenp.is_null() {
            let _ = oldlenp.write(want);
        }
        BsdRet::ok(0)
    }
}

/// The body of `getdirentries(2)`: the records that fit in `writer`, and
/// `*basep`, the directory position BEFORE the read. `kern_getdirentries`
/// takes `loff = auio.uio_offset` ahead of `VOP_READDIR` and stores that;
/// libc's `readdir` keeps it as `dd_seek`, the value `telldir` hands back for
/// an entry of this buffer and `seekdir` gives to `lseek`. Reporting 0 made
/// every `telldir` say "the start", so `seekdir(telldir())` rewound.
///
/// The entry that does not fit goes back to the directory position instead
/// of being lost; see `collect_dirents`.
fn read_dirents_with_base(
    file: &mut Arc<linux_object::fs::File>,
    writer: &mut fs::BsdDirentWriter,
) -> linux_object::error::LxResult<i64> {
    let base = file.dir_position() as i64;
    crate::file::collect_dirents(file, |next, meta, name| {
        writer.try_push(meta.inode as u64, next, fs::dirent_type(meta.type_), name)
    })?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    use linux_object::fs::vfs::{FileSystem, FileType};
    use linux_object::fs::{File, OpenFlags};
    use rcore_fs_ramfs::RamFS;

    /// An open directory on a fresh ramfs holding `names`.
    fn dir(names: &[&str]) -> Arc<File> {
        let root = RamFS::new().root_inode();
        for name in names {
            root.create(name, FileType::File, 0o644).unwrap();
        }
        File::new(root, OpenFlags::RDONLY, alloc::string::String::from("/"))
    }

    /// `basep` is where the directory was BEFORE each read, the value
    /// `telldir` reports for an entry of that buffer. Two entries fit per
    /// call here and the ramfs lists `.` and `..` too, so the bases go
    /// 0, 2, 4, 6 with the last call short.
    #[test]
    fn getdirentries_reports_the_position_before_each_read_in_basep() {
        let mut d = dir(&["a", "b", "c", "d", "e"]);
        let one = fs::dirsiz(1);
        let mut bases = alloc::vec::Vec::new();
        let mut lens = alloc::vec::Vec::new();
        loop {
            let mut w = fs::BsdDirentWriter::new(2 * one);
            let base = read_dirents_with_base(&mut d, &mut w).unwrap();
            if w.is_empty() {
                break;
            }
            bases.push(base);
            lens.push(w.len() / one);
        }
        assert_eq!(lens, [2, 2, 2, 1]);
        assert_eq!(bases, [0, 2, 4, 6]);
    }

    #[test]
    fn bsdret_encodes_success_and_error() {
        let ok = BsdRet::ok(7);
        assert_eq!((ok.rax, ok.rdx, ok.error), (7, 0, false));
        let ok2 = BsdRet::ok2(3, 1);
        assert_eq!((ok2.rax, ok2.rdx, ok2.error), (3, 1, false));
        let err = BsdRet::err(consts::errno::EBADF);
        assert_eq!((err.rax, err.error), (9, true));
        let nosys = BsdRet::enosys();
        assert_eq!((nosys.rax, nosys.error), (78, true)); // FreeBSD ENOSYS
    }

    #[test]
    fn from_result_translates_errno_to_freebsd() {
        // Ok passes the value through with no error.
        let r = BsdRet::from_result(Ok(42));
        assert_eq!((r.rax, r.error), (42, false));
        // A Linux EAGAIN(11) must surface as FreeBSD EAGAIN(35), not EDEADLK.
        let r = BsdRet::from_result(Err(LxError::EAGAIN));
        assert_eq!((r.rax, r.error), (35, true));
        // Linux ENOSYS(38) -> FreeBSD ENOSYS(78).
        let r = BsdRet::from_result(Err(LxError::ENOSYS));
        assert_eq!((r.rax, r.error), (78, true));
    }

    #[test]
    fn clockid_translation_matches_freebsd_numbering() {
        assert_eq!(clockid_to_linux(0), 0); // REALTIME
        assert_eq!(clockid_to_linux(4), 1); // MONOTONIC (FreeBSD 4 -> Linux 1)
        assert_eq!(clockid_to_linux(11), 1); // MONOTONIC_PRECISE
        assert_eq!(clockid_to_linux(9), 0); // REALTIME_PRECISE
        assert_eq!(clockid_to_linux(15), 2); // PROCESS_CPUTIME
        assert_eq!(clockid_to_linux(14), 3); // THREAD_CPUTIME
    }
}
