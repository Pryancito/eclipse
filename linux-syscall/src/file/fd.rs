//! File descriptor operations
//!
//! - open(at)
//! - close
//! - dup2
//! - pipe

use super::*;
use crate::outparams::{commit_and_report_old, hand_out_pair};
use alloc::string::String;
use alloc::sync::Arc;
use linux_object::error::LxResult;
use linux_object::fs::{SignalFd, TimerFd};
use linux_object::time::{timerfd_clock_base, TimeSpec};
use rcore_fs::vfs::INode;

/// `struct itimerspec` for `timerfd_settime`/`timerfd_gettime`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ITimerSpec {
    it_interval: TimeSpec,
    it_value: TimeSpec,
}

impl ITimerSpec {
    /// Saturating: the fields come straight from userspace, where
    /// `sec * 1_000_000_000` overflows a `u64` well inside the range a
    /// `time_t` can hold. Callers validate first; this is the backstop.
    fn value_ns(&self) -> u64 {
        (self.it_value.sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(self.it_value.nsec as u64)
    }
    fn interval_ns(&self) -> u64 {
        (self.it_interval.sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(self.it_interval.nsec as u64)
    }
    fn from_ns(interval_ns: u64, value_ns: u64) -> Self {
        let ts = |ns: u64| TimeSpec {
            sec: (ns / 1_000_000_000) as usize,
            nsec: (ns % 1_000_000_000) as usize,
        };
        ITimerSpec {
            it_interval: ts(interval_ns),
            it_value: ts(value_ns),
        }
    }
}

fn prepare_open_inode(inode: Arc<dyn INode>) -> LxResult<Arc<dyn INode>> {
    Ok(
        if inode
            .downcast_ref::<linux_object::fs::pty::PtmxINode>()
            .is_some()
        {
            linux_object::fs::pty::alloc_ptmx()
        } else if let Some(ptmx) = inode.downcast_ref::<linux_object::fs::devfs::PtmxINode>() {
            ptmx.open_master().map_err(LxError::from)?
        } else if let Some(pcm) = inode.downcast_ref::<linux_object::fs::devfs::PcmDev>() {
            // Raw ALSA hw PCMs are single-client: while one fd owns
            // `/dev/snd/pcmC*D0p`, the next open must fail with EBUSY.
            pcm.open_client().map_err(LxError::from)?
        } else if let Some(dsp) = inode.downcast_ref::<linux_object::fs::devfs::DspDev>() {
            // `/dev/dsp<N>` shares that claim: the OSS node and the native
            // PCM are two front ends onto one unmixed hardware ring, so
            // whichever is second gets EBUSY. That is what makes a bare
            // `mpg123 file.mp3` fall through from OSS to ALSA (and the
            // daemon) instead of playing into PulseAudio's ring.
            dsp.open_client().map_err(LxError::from)?
        } else if let Some(timer) = inode.downcast_ref::<linux_object::fs::devfs::TimerDev>() {
            // `/dev/snd/timer` too: every open is its own ALSA timer
            // instance (selection, params, event queue), as on Linux.
            timer.open_client()
        } else if let Some(drm) = inode.downcast_ref::<linux_object::fs::devfs::DrmDev>() {
            // `/dev/dri/card*` / `renderD*`: per-open ATOMIC_CLIENT + event
            // queue (F-M7). GEM handles remain global for now.
            drm.open_client()
        } else {
            inode
        },
    )
}

/// The three commands `flock(2)` accepts, after `LOCK_NB` is taken off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlockCmd {
    /// `LOCK_SH`: shared, read lock.
    Shared,
    /// `LOCK_EX`: exclusive, write lock.
    Exclusive,
    /// `LOCK_UN`: drop whatever this fd held.
    Unlock,
}

/// `flock_translate_cmd()`, which is a `switch` on three exact values and not
/// a bitmask.
///
/// Reading the argument through a `bitflags` type accepted three things Linux
/// refuses, each of which came back as "lock acquired": `flock(fd, 0)`, which
/// names no command at all; `flock(fd, LOCK_SH | LOCK_EX)`, which names two;
/// and any high bit, because the argument was first truncated to a byte, so
/// `flock(fd, 0x102)` took an exclusive lock on the strength of its low byte.
fn flock_translate(operation: usize) -> LxResult<(FlockCmd, bool)> {
    const LOCK_SH: u32 = 1;
    const LOCK_EX: u32 = 2;
    const LOCK_NB: u32 = 4;
    const LOCK_UN: u32 = 8;
    // `SYSCALL_DEFINE2(flock, unsigned int, fd, unsigned int, cmd)`: 32 bits,
    // not 8.
    let cmd = operation as u32;
    let nonblock = cmd & LOCK_NB != 0;
    let cmd = match cmd & !LOCK_NB {
        LOCK_SH => FlockCmd::Shared,
        LOCK_EX => FlockCmd::Exclusive,
        LOCK_UN => FlockCmd::Unlock,
        _ => return Err(LxError::EINVAL),
    };
    Ok((cmd, nonblock))
}

/// `O_CLOEXEC`, as every anonymous-fd constructor spells it: `EFD_CLOEXEC`,
/// `TFD_CLOEXEC`, `SFD_CLOEXEC`, `IN_CLOEXEC`, `EPOLL_CLOEXEC` and
/// `SOCK_CLOEXEC` are all the same bit, and all land on `OpenFlags::CLOEXEC`.
pub(crate) const ANON_CLOEXEC: usize = 0o2_000_000;
/// `O_NONBLOCK`, likewise: `EFD_NONBLOCK`, `TFD_NONBLOCK`, `SFD_NONBLOCK`,
/// `IN_NONBLOCK`. Lands on `OpenFlags::NON_BLOCK`.
pub(crate) const ANON_NONBLOCK: usize = 0o4_000;

/// The open flags an anonymous-fd constructor was asked for, or `EINVAL` if
/// the caller set a bit that syscall does not have.
///
/// `OpenFlags::from_bits_truncate` alone answers the wrong question: it drops
/// what it does not recognise in silence, so `eventfd2(0, 0x4000_0000)` used
/// to hand back an ordinary eventfd instead of the `EINVAL` Linux answers.
/// Each of these syscalls checks its own word against its own set
/// (`EFD_FLAGS_SET`, `TFD_CREATE_FLAGS`, `SFD_FLAGS_SET`, ...), and this is
/// that check, written once. `inotify_init1` was the only one of the five
/// doing it.
pub(crate) fn anon_fd_flags(flags: usize, allowed: usize) -> Result<OpenFlags, LxError> {
    if flags & !allowed != 0 {
        return Err(LxError::EINVAL);
    }
    // Truncating is right now that the word is known to hold nothing else:
    // a bit inside `allowed` that `OpenFlags` does not name is the syscall's
    // own (`EFD_SEMAPHORE`), read back through its own name by whoever owns
    // it.
    Ok(OpenFlags::from_bits_truncate(flags))
}

impl Syscall<'_> {
    /// `timerfd_create(2)`: a timer delivered through a readable fd. The
    /// `wl_event_loop` (libwayland) arms one for all its timers.
    pub fn sys_timerfd_create(&self, clockid: usize, flags: usize) -> SysResult {
        info!("timerfd_create: clockid={}, flags={:#x}", clockid, flags);
        // The `clockid` used to be logged and then dropped, so every timerfd
        // ran on the monotonic clock whatever the caller asked for. An
        // absolute `CLOCK_REALTIME` deadline -- seconds since 1970 -- was
        // then armed as monotonic nanoseconds since boot, i.e. decades away:
        // the timer simply never fired.
        let clock = timerfd_clock_base(clockid)?;
        let open_flags = anon_fd_flags(flags, ANON_CLOEXEC | ANON_NONBLOCK)?;
        let tfd = TimerFd::new(open_flags, clock);
        let fd = self.linux_process().add_file(tfd)?;
        Ok(fd.into())
    }

    /// `timerfd_settime(2)`: arm/disarm the timer (`TFD_TIMER_ABSTIME` = bit 0).
    pub fn sys_timerfd_settime(
        &self,
        fd: FileDesc,
        flags: usize,
        new_value: UserInPtr<ITimerSpec>,
        mut old_value: UserOutPtr<ITimerSpec>,
    ) -> SysResult {
        // `TFD_SETTIME_FLAGS`. CANCEL_ON_SET is accepted and does nothing:
        // it asks to be woken with ECANCELED when the realtime clock is
        // stepped, and nothing here steps it behind a timer's back.
        const TFD_TIMER_ABSTIME: usize = 1;
        const TFD_TIMER_CANCEL_ON_SET: usize = 2;
        if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
            return Err(LxError::EINVAL);
        }
        let file_like = self.linux_process().get_file_like(fd)?;
        let tfd = file_like.downcast_ref::<TimerFd>().ok_or(LxError::EINVAL)?;
        let (iv, rem) = tfd.get_time();
        let old = ITimerSpec::from_ns(iv, rem);
        // The old value is reported LAST, after the timer is set, as
        // timerfd_settime(2) does: reading `new_value` can fault and an
        // out-of-range timespec is EINVAL, and neither of those may have
        // already written the caller's `old_value`.
        commit_and_report_old(old, &mut old_value, || {
            let v = new_value.read()?;
            // Same rule as `timer_settime`: an out-of-range `timespec` is EINVAL.
            if !v.it_interval.valid() || !v.it_value.valid() {
                return Err(LxError::EINVAL);
            }
            info!(
                "timerfd_settime: fd={:?}, flags={:#x}, value_ns={}, interval_ns={}",
                fd,
                flags,
                v.value_ns(),
                v.interval_ns()
            );
            tfd.set_time(
                v.value_ns(),
                v.interval_ns(),
                flags & TFD_TIMER_ABSTIME != 0,
            );
            Ok(())
        })?;
        Ok(0)
    }

    /// `timerfd_gettime(2)`: report the time until the next expiration.
    pub fn sys_timerfd_gettime(
        &self,
        fd: FileDesc,
        mut curr_value: UserOutPtr<ITimerSpec>,
    ) -> SysResult {
        let file_like = self.linux_process().get_file_like(fd)?;
        let tfd = file_like.downcast_ref::<TimerFd>().ok_or(LxError::EINVAL)?;
        let (iv, rem) = tfd.get_time();
        curr_value.write(ITimerSpec::from_ns(iv, rem))?;
        Ok(0)
    }

    /// `signalfd4(2)`: accept the signals in `mask` through a readable fd. With
    /// `fd == -1` a new signalfd is created; otherwise the existing fd's mask is
    /// replaced. The caller is expected to also block those signals
    /// (`sigprocmask`) so they stay pending for the fd — which libwayland does.
    ///
    /// The order below is `do_signalfd4`'s: the flag word, then `sizemask`,
    /// then the set itself.
    pub fn sys_signalfd4(
        &self,
        fd: FileDesc,
        mask: UserInPtr<u64>,
        sizemask: usize,
        flags: usize,
    ) -> SysResult {
        // Checked before anything else, as Linux does: the flag word is
        // rejected whether or not `fd` names an existing signalfd.
        let open_flags = anon_fd_flags(flags, ANON_CLOEXEC | ANON_NONBLOCK)?;
        // How wide the caller's `sigset_t` is. Every other syscall in this
        // tree that takes one asks (see [`check_sigsetsize`]); this one took
        // the word, named it `_sizemask`, and read eight bytes regardless.
        crate::signal::check_sigsetsize(sizemask)?;
        let sigmask = mask.read()?;
        info!(
            "signalfd4: fd={:?}, mask={:#x}, flags={:#x}",
            fd, sigmask, flags
        );
        let proc = self.linux_process();
        if <FileDesc as Into<i32>>::into(fd) >= 0 {
            // Update an existing signalfd's accepted-signal set.
            let file_like = proc.get_file_like(fd)?;
            let sfd = file_like
                .downcast_ref::<SignalFd>()
                .ok_or(LxError::EINVAL)?;
            sfd.set_mask(sigmask);
            return Ok(fd.into());
        }
        let sfd = SignalFd::new(sigmask, open_flags);
        let new_fd = proc.add_file(sfd)?;
        Ok(new_fd.into())
    }
    /// Opens or creates a file, depending on the flags passed to the call. Returns an integer with the file descriptor.
    pub fn sys_open(&self, path: UserInPtr<u8>, flags: usize, mode: usize) -> SysResult {
        self.sys_openat(FileDesc::CWD, path, flags, mode)
    }

    /// open file relative to directory file descriptor
    pub fn sys_openat(
        &self,
        dir_fd: FileDesc,
        path: UserInPtr<u8>,
        flags: usize,
        mode: usize,
    ) -> SysResult {
        let proc = self.linux_process();
        let path = path.as_c_str()?;
        // hard code special path
        let path = if path == "/dev/shm/testshm" {
            "/testshm"
        } else {
            path
        };
        let flags = OpenFlags::from_bits_truncate(flags);
        info!(
            "openat: dir_fd={:?}, path={:?}, flags={:?}, mode={:#o}",
            dir_fd, path, flags, mode
        );
        let follow = !flags.contains(OpenFlags::NOFOLLOW);

        // The whole resolution runs inside a closure so its many `?`/`return`
        // exit points funnel into one `ret`, which the boot-trace recorder
        // (below) sees — including the ENOENT misses that reveal how ld.so
        // probes its library search path. The closure is a zero-cost wrapper:
        // no allocation, no extra work, just structure.
        let ret: SysResult = (|| {
            // Pseudo-terminals. Opening `/dev/ptmx` mints a brand-new master (each
            // open must yield an independent PTY pair, which the generic INode open
            // path cannot express), and `/dev/pts/N` resolves to the matching slave
            // from the live PTY registry rather than a static device node.
            // Every path handled specially below hands back a character
            // device, so an `O_DIRECTORY` over one of them is answered before
            // the work of minting a PTY starts.
            if path == "/dev/ptmx" || path == "/dev/tty" || pty::pts_id_from_path(path).is_some() {
                open_resolved_type(flags, FileType::CharDevice)?;
            }
            if path == "/dev/ptmx" {
                let inode = pty::alloc_ptmx();
                let file = File::new(inode, flags, String::from("/dev/ptmx"));
                let fd = proc.add_file(file)?;
                return Ok(fd.into());
            }
            if let Some(id) = pty::pts_id_from_path(path) {
                let inode = pty::open_pts(id).ok_or(LxError::ENXIO)?;
                let file = File::new(inode, flags, String::from(path));
                let fd = proc.add_file(file)?;
                return Ok(fd.into());
            }
            // `/dev/tty` is the *controlling terminal* of the calling process, which
            // for our per-VT shells is that process's own virtual terminal. Resolve
            // it per-caller instead of through a single shared node: otherwise a
            // background-VT shell's job-control query — `tcgetpgrp("/dev/tty")` —
            // returns the *active* VT's foreground pgrp, never equals its own pgrp,
            // and busybox spins forever on `killpg(0, SIGTTIN)` (a CPU-burning busy
            // loop on every spare VT — the dominant idle heat once the signal
            // self-deadlock is fixed).
            if path == "/dev/tty" {
                // A process RUNNING ON A PTY (the shell inside foot/alacritty) must
                // get its own pts back, not the VT. busybox ash opens /dev/tty for
                // job control, and handing it the VT reads/writes ANOTHER
                // terminal's foreground pgrp: the first pty shell's tcsetpgrp()
                // stamped its pid into the VT's global fg_pgrp, and every LATER pty
                // shell then saw that stale pid from tcgetpgrp(), never matched its
                // own pgrp, and spun forever in killpg(0, SIGTTIN) without printing
                // a prompt — foot worked exactly once per boot, then never again.
                // There is no session/ctty tracking to consult (setsid is a stub),
                // so use the fds: stdin/stdout/stderr on a pts means the caller's
                // controlling terminal is that pty.
                let pts = (0i32..3).find_map(|n| {
                    let f = proc.get_file_like(FileDesc::from(n)).ok()?;
                    let file = f.downcast_ref::<File>()?;
                    let inode = file.inode();
                    let slave = inode.as_any_ref().downcast_ref::<pty::PtySlave>()?;
                    pty::open_pts(slave.pty_id())
                });
                if let Some(inode) = pts {
                    let file = File::new(inode, flags, String::from("/dev/tty"));
                    let fd = proc.add_file(file)?;
                    return Ok(fd.into());
                }
                let inode = linux_object::fs::stdio::vt_stdin(proc.vt());
                let file = File::new(inode, flags, String::from("/dev/tty"));
                let fd = proc.add_file(file)?;
                return Ok(fd.into());
            }

            let inode = if flags.contains(OpenFlags::CREATE) {
                let (dir_path, file_name) = split_path(path);
                // relative to cwd
                let dir_inode = proc.lookup_inode_at(dir_fd, dir_path, true)?;
                let dir_metadata = dir_inode.metadata()?;
                proc.check_access(&dir_metadata, 0o3, true)?;
                match dir_inode.find(file_name) {
                    Ok(file_inode) => {
                        if flags.contains(OpenFlags::EXCLUSIVE) {
                            return Err(LxError::EEXIST);
                        }
                        // `O_CREAT` over a name that is already a symbolic
                        // link still opens what the link points AT:
                        // `open_last_lookups` resolves the final component
                        // like any other open, and only `O_EXCL` (the EEXIST
                        // above) or `O_NOFOLLOW` stop it. `find` hands back
                        // the link's own inode, so without this the write
                        // went into the link and not into the file.
                        let file_inode = if file_inode.metadata()?.type_ == FileType::SymLink {
                            if !follow {
                                return Err(LxError::ELOOP);
                            }
                            proc.lookup_inode_at(dir_fd, path, true)?
                        } else {
                            file_inode
                        };
                        let metadata = file_inode.metadata()?;
                        if flags.writable() || flags.contains(OpenFlags::TRUNCATE) {
                            proc.check_access(&metadata, 0o2, true)?;
                        }
                        if flags.readable() {
                            proc.check_access(&metadata, 0o4, true)?;
                        }
                        file_inode
                    }
                    Err(FsError::EntryNotFound) => {
                        let create_mode = proc.apply_umask(mode as u16);
                        let inode =
                            dir_inode.create(file_name, FileType::File, create_mode as u32)?;
                        linux_object::fs::dcache_invalidate();
                        proc.initialize_created_metadata(
                            &inode,
                            Some(&dir_metadata),
                            create_mode,
                            false,
                        )?;
                        inode
                    }
                    Err(e) => return Err(LxError::from(e)),
                }
            } else {
                // `O_NOFOLLOW` refuses a symbolic link as the LAST component
                // and nothing else: a path through `/var/log` where that is
                // itself a link resolves as usual. Asking the resolution not
                // to follow anything would answer the wrong question, because
                // its budget covers every hop, so the last component is asked
                // about on its own -- and only when the flag is there, so the
                // ordinary open pays nothing for it.
                if !follow {
                    let (dir_path, file_name) = split_path(path);
                    let dir_inode = proc.lookup_inode_at(dir_fd, dir_path, true)?;
                    if matches!(
                        dir_inode.find(file_name).map(|i| i.metadata()),
                        Ok(Ok(m)) if m.type_ == FileType::SymLink
                    ) {
                        return Err(LxError::ELOOP);
                    }
                }
                let inode = proc.lookup_inode_at(dir_fd, path, true)?;
                let metadata = inode.metadata()?;
                if flags.readable() {
                    proc.check_access(&metadata, 0o4, true)?;
                }
                if flags.writable() {
                    proc.check_access(&metadata, 0o2, true)?;
                }
                inode
            };
            let metadata = inode.metadata()?;
            open_resolved_type(flags, metadata.type_)?;
            if flags.contains(OpenFlags::TRUNCATE) && metadata.type_ == FileType::File {
                proc.check_access(&metadata, 0o2, true)?;
                inode.resize(0)?;
                linux_object::fs::cache_truncate(&inode, 0);
            }
            // `/dev/ptmx` is a cloning device: each open allocates a fresh PTY
            // master (and publishes its slave at `/dev/pts/N`). Prefer the
            // `fs/pty` registry (absolute opens already special-cased above); the
            // legacy `devfs::PtmxINode` path remains for any leftover node.
            // `/dev/dsp` opened without write access asks for a capture
            // stream, and there is none: Linux's OSS emulation answers
            // EINVAL when neither direction the open asked for exists.
            if inode
                .downcast_ref::<linux_object::fs::devfs::DspDev>()
                .is_some()
                && !flags.writable()
            {
                return Err(LxError::EINVAL);
            }
            let inode = prepare_open_inode(inode)?;
            let abs_path = proc.get_absolute_path(dir_fd, path)?;
            let file = File::new(inode, flags, abs_path);
            let fd = proc.add_file(file)?;
            Ok(fd.into())
        })();

        // Boot-time file-access recorder. Gated on a single relaxed atomic that
        // is false unless `BOOTTRACE=<comm>` was on the kernel command line, so
        // this is free on every open when tracing is off. When on, it records
        // this open (path + result + timestamp) for the one process whose
        // `comm` matches — the raw material for /proc/bootprofile and the
        // desktop preload list. `comm` is computed lazily inside record_open.
        if linux_object::boot_trace::enabled() {
            let pid = self.zircon_process().id();
            let code = match &ret {
                Ok(v) => (*v).min(i32::MAX as usize) as i32,
                Err(e) => -(*e as i32),
            };
            linux_object::boot_trace::record_open(
                pid,
                || {
                    let p = self.linux_process().execute_path();
                    String::from(p.rsplit('/').next().unwrap_or(p.as_str()))
                },
                path,
                code,
            );
        }
        ret
    }

    /// Closes a file descriptor, so that it no longer refers to any file and may be reused.
    pub fn sys_close(&self, fd: FileDesc) -> SysResult {
        info!("close: fd={:?}", fd);
        let proc = self.linux_process();
        // DRM diagnostics: removal of a DRM/dmabuf fd, with the pid, so a
        // stale-fd DRM ioctl can be traced to whoever closed it. debug level:
        // closing card0 is normal application behavior (X and every DRM client
        // probe-and-close at startup); available under LOG=debug when hunting.
        if let Ok(f) = proc.get_file_like(fd) {
            if let Some(desc) = linux_object::fs::drm_fd_desc(&f) {
                debug!(
                    "[drm] pid={} close(fd={:?}) of {}",
                    self.zircon_process().id(),
                    fd,
                    desc
                );
            }
        }

        proc.close_file(fd)?;
        Ok(0)
    }

    /// `close_range(2)`: act on every open descriptor in `[first, last]`.
    ///
    /// `flags` is load-bearing and must NOT be dropped:
    /// - `CLOSE_RANGE_CLOEXEC` means "MARK this range close-on-exec"; the
    ///   descriptors stay open and usable. Treating it as "close" turns a
    ///   routine hardening call (glibc, dbus, systemd, GLib all make one)
    ///   into a mass close of live fds.
    /// - `CLOSE_RANGE_UNSHARE` asks for a private fd table first; this kernel
    ///   never shares one between processes, so it is a no-op rather than an
    ///   error.
    /// - Any other bit must be `EINVAL`, which is how callers detect an old
    ///   kernel and fall back.
    pub fn sys_close_range(&self, first: usize, last: usize, flags: usize) -> SysResult {
        const CLOSE_RANGE_UNSHARE: usize = 1 << 1;
        const CLOSE_RANGE_CLOEXEC: usize = 1 << 2;
        let proc = self.linux_process();
        // Diagnostic at klog level: a mass close of a live fd table is
        // invisible at the default log level otherwise.
        kernel_hal::klog_info!(
            "[close-range] proc={:?} first={} last={} flags={:#x}",
            proc.execute_path(),
            first,
            last,
            flags
        );
        if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 || first > last {
            return Err(LxError::EINVAL);
        }
        // `FileDesc` is an i32, so the canonical `close_range(3, ~0U, 0)`
        // idiom would wrap `last` to -1 and silently match nothing. Clamp.
        let first = FileDesc::from(first.min(i32::MAX as usize));
        let last = FileDesc::from(last.min(i32::MAX as usize));
        if flags & CLOSE_RANGE_CLOEXEC != 0 {
            proc.set_range_cloexec(first, last);
        } else {
            proc.close_range(first, last);
        }
        Ok(0)
    }

    /// `dup3(2)`: like dup2, but equal descriptors are an error and
    /// `O_CLOEXEC` can be applied atomically to the new descriptor — the whole
    /// reason the syscall exists, and what the old alias-to-dup2 dropped.
    pub fn sys_dup3(&self, fd1: FileDesc, fd2: usize, flags: usize) -> SysResult {
        info!("dup3: from {:?} to {} flags {:#o}", fd1, fd2, flags);
        const O_CLOEXEC: usize = 0o2000000;
        if usize::from(fd1) == fd2 || flags & !O_CLOEXEC != 0 {
            return Err(LxError::EINVAL);
        }
        self.sys_dup2(fd1, fd2)?;
        let fd2 = FileDesc::from(fd2);
        if flags & O_CLOEXEC != 0 {
            // Per-descriptor CLOEXEC on the new fd — set in the fd table, not
            // in the File object (whose flags are only a creation-time record).
            self.linux_process().set_fd_cloexec(fd2, true)?;
        }
        Ok(fd2.into())
    }

    /// create a copy of the file descriptor oldfd.
    ///
    /// `fd2` arrives as the raw register: Linux (`ksys_dup3`) answers `EBADF`
    /// to a target at or past `RLIMIT_NOFILE` before it looks at anything
    /// else, and the check has to see the whole word, since narrowing first
    /// turned `1 << 32 | 1` into fd 1 and `-1` into a table entry at -1.
    pub fn sys_dup2(&self, fd1: FileDesc, fd2: usize) -> SysResult {
        info!("dup2: from {:?} to {}", fd1, fd2);
        let proc = self.linux_process();
        let fd2 = dup_target(fd2, proc.file_limit().cur)?;
        if fd1 == fd2 {
            let _ = proc.get_file_like(fd1)?;
            return Ok(fd2.into());
        }
        // `dup2(2)` installs the SAME open file description under `fd2`, not a
        // copy of it: `fs/file.c`'s `do_dup2` does `get_file(file)` and stores
        // that pointer. So the two descriptors share the file offset and the
        // status flags -- which is what makes `prog >log 2>&1` write one log
        // instead of two streams overwriting each other from offset 0 -- and
        // the only thing the new descriptor gets of its own is `FD_CLOEXEC`,
        // which `dup2` clears (POSIX; `dup3(2)` sets it back when asked).
        let file_like = proc.get_file_like(fd1)?;
        // Atomic replace (Linux dup2 semantics). The previous close-then-insert
        // pair took the fd-table lock twice, leaving a window where fd2 was
        // absent — a concurrent syscall on fd2 in that window got a spurious
        // EBADF.
        let old = proc.replace_file(fd2, file_like, false)?;
        if let Some(old) = old {
            if let Some(desc) = linux_object::fs::drm_fd_desc(&old) {
                error!(
                    "[drm] pid={} dup2 clobbered fd={:?} ({})",
                    self.zircon_process().id(),
                    fd2,
                    desc
                );
            }
        }
        Ok(fd2.into())
    }

    /// `fcntl(F_DUPFD)`: the copy goes to the lowest free descriptor at or
    /// above `start`, which is also checked against `RLIMIT_NOFILE` the way
    /// `f_dupfd` does. Before the check, a start of `-1` (every "no minimum"
    /// mistake spells it that way) reached the allocator as `usize::MAX`: the
    /// first call installed the copy at descriptor -1 and the second walked
    /// the open range off the end of `usize`, which is a kernel panic from a
    /// process with no privileges.
    pub fn sys_dupfd(&self, fd1: FileDesc, start: usize) -> SysResult {
        let proc = self.linux_process();
        let nofile = proc.file_limit().cur;
        let start = dupfd_start(start, nofile)?;
        let new_fd = dupfd_picked(proc.get_free_fd_from(start).into(), nofile)?;
        // sys_dup2 registers the new fd with CLOEXEC off (POSIX dup
        // semantics); F_DUPFD_CLOEXEC re-tags it afterwards.
        self.sys_dup2(fd1, new_fd.into())
    }

    /// create a copy of the file descriptor fd, and uses the lowest-numbered unused descriptor for the new descriptor.
    pub fn sys_dup(&self, fd1: FileDesc) -> SysResult {
        info!("dup: from {:?}", fd1);
        let proc = self.linux_process();
        // Same open file description, lowest free descriptor, `FD_CLOEXEC`
        // clear (see `sys_dup2`).
        let file_like = proc.get_file_like(fd1)?;
        let fd2 = proc.add_file_cloexec(file_like, false)?;
        Ok(fd2.into())
    }

    /// Creates a pipe, a unidirectional data channel that can be used for interprocess communication.
    pub fn sys_pipe(&self, fds: UserOutPtr<[i32; 2]>) -> SysResult {
        self.sys_pipe2(fds, 0)
    }

    /// Creates a pipe, a unidirectional data channel that can be used for interprocess communication.
    pub fn sys_pipe2(&self, mut fds: UserOutPtr<[i32; 2]>, flags: usize) -> SysResult {
        info!("pipe2: fds={:?}, flags: {:#x}", fds, flags);

        let proc = self.linux_process();
        let (read, write) = Pipe::create_pair();

        let base_flags =
            OpenFlags::from_bits_truncate(flags) & (OpenFlags::NON_BLOCK | OpenFlags::CLOEXEC);
        let read_fd = proc.add_file(File::new(
            Arc::new(read),
            base_flags | OpenFlags::RDONLY,
            String::from("pipe_r:[]"),
        ))?;

        // The descriptors only belong to the caller once it has their numbers:
        // until then a failure has to take them back, or they sit in the fd
        // table with nothing left that can close them. See `hand_out_pair`.
        hand_out_pair(
            read_fd,
            || {
                proc.add_file(File::new(
                    Arc::new(write),
                    base_flags | OpenFlags::WRONLY,
                    String::from("pipe_w:[]"),
                ))
            },
            |read_fd, write_fd| {
                fds.write([read_fd.into(), write_fd.into()])?;
                info!("pipe2: created rfd={:?} wfd={:?}", read_fd, write_fd);
                Ok(())
            },
            |fd| {
                let _ = proc.close_file(fd);
            },
        )?;

        Ok(0)
    }

    /// apply or remove an advisory lock on an open file
    /// (see [linux man flock(2)](https://man7.org/linux/man-pages/man2/flock.2.html)).
    ///
    /// The lock is held by the open file description, which here is the
    /// `Arc<File>` behind the fd: `dup`ed and inherited descriptors share it,
    /// and the drop of the last of them releases it (`fs::flock`). A
    /// conflicting lock makes the call wait, unless `LOCK_NB` asks for
    /// `EWOULDBLOCK` instead; a signal ends the wait with `EINTR`.
    ///
    /// It used to validate the request and answer 0 without taking anything,
    /// so two `LOCK_EX` on the same file both succeeded: `flock(1)`, dpkg's
    /// and apt's frontend locks and every `LOCK_EX | LOCK_NB` "is another
    /// instance running?" probe found the file free every time.
    pub async fn sys_flock(&self, fd: FileDesc, operation: usize) -> SysResult {
        use linux_object::fs::flock;
        let (cmd, nonblock) = flock_translate(operation)?;
        info!(
            "flock: fd: {:?}, cmd: {:?}, nonblock: {}",
            fd, cmd, nonblock
        );
        let file = self.linux_process().get_file(fd)?;
        let meta = file.metadata()?;
        let key = (meta.dev, meta.inode);
        let owner = Arc::as_ptr(&file) as usize;
        let exclusive = match cmd {
            FlockCmd::Unlock => {
                flock::unlock(key, owner);
                return Ok(0);
            }
            FlockCmd::Shared => false,
            FlockCmd::Exclusive => true,
        };
        loop {
            match flock::try_lock(key, owner, exclusive) {
                Ok(()) => return Ok(0),
                Err(since) => {
                    if nonblock {
                        return Err(LxError::EAGAIN);
                    }
                    flock::wait_for_change(since).await?;
                }
            }
        }
    }

    /// `memfd_create(2)`: create an anonymous in-RAM file referred to by the
    /// returned fd. Supports `ftruncate`, `mmap` and `read`/`write`; seals
    /// (`fcntl` `F_ADD_SEALS`) are accepted as no-ops. Wayland/wlroots/Mesa use
    /// it to share xkb keymaps and shm pools.
    pub fn sys_memfd_create(&self, name: UserInPtr<u8>, flags: usize) -> SysResult {
        // A name that cannot be read is EFAULT, as it is for every other
        // syscall that takes a string. Substituting a default for it -- which
        // is what `unwrap_or("memfd")` did -- handed back a working fd for a
        // pointer the caller got wrong, and the name is the one thing a memfd
        // carries for the rest of its life.
        let name = name.as_c_str()?;
        info!("memfd_create: name={:?}, flags={:#x}", name, flags);
        let file = linux_object::fs::new_memfd(name, flags)?;
        let fd = self.linux_process().add_file(file)?;
        Ok(fd.into())
    }

    /// creates an eventfd object that can be used as an event notification mechanism by user-space applications,
    /// and by the kernel to notify user-space applications of events.
    pub fn sys_eventfd2(&self, initval: u32, flags: usize) -> SysResult {
        info!("eventfd2: initval={}, flags={:#x}", initval, flags);
        // `EFD_FLAGS_SET`. EFD_SEMAPHORE is bit 0, which `OpenFlags` spells
        // `WRONLY`; `EventFd` reads it back under its own name.
        const EFD_SEMAPHORE: usize = 1;
        let flags = anon_fd_flags(flags, EFD_SEMAPHORE | ANON_CLOEXEC | ANON_NONBLOCK)?;
        let proc = self.linux_process();
        let eventfd = EventFd::new(initval, flags);
        let fd = proc.add_file(eventfd)?;
        Ok(fd.into())
    }

    /// `inotify_init1(2)`: create an inotify instance. `flags` may carry
    /// `IN_NONBLOCK` (0o4000) / `IN_CLOEXEC` (0o2000000), sharing the
    /// `O_NONBLOCK` / `O_CLOEXEC` bit values. `inotify_init(2)` is this with
    /// flags = 0. labwc and GTK apps call this to watch their config dirs.
    pub fn sys_inotify_init1(&self, flags: usize) -> SysResult {
        info!("inotify_init1: flags={:#x}", flags);
        let flags = anon_fd_flags(flags, ANON_CLOEXEC | ANON_NONBLOCK)?;
        let inotify = linux_object::fs::Inotify::new(flags);
        let fd = self.linux_process().add_file(inotify)?;
        Ok(fd.into())
    }

    /// `inotify_add_watch(2)`: add `pathname` to the watch list of the inotify
    /// instance `fd`, returning a watch descriptor.
    pub fn sys_inotify_add_watch(
        &self,
        fd: usize,
        pathname: UserInPtr<u8>,
        mask: u32,
    ) -> SysResult {
        let path = pathname.as_c_str()?;
        info!(
            "inotify_add_watch: fd={}, path={:?}, mask={:#x}",
            fd, path, mask
        );
        let file = self.linux_process().get_file_like(fd.into())?;
        let inotify = file
            .downcast_arc::<linux_object::fs::Inotify>()
            .map_err(|_| LxError::EINVAL)?;
        inotify.add_watch(path, mask)
    }

    /// `inotify_rm_watch(2)`: remove watch descriptor `wd` from inotify `fd`.
    pub fn sys_inotify_rm_watch(&self, fd: usize, wd: i32) -> SysResult {
        info!("inotify_rm_watch: fd={}, wd={}", fd, wd);
        let file = self.linux_process().get_file_like(fd.into())?;
        let inotify = file
            .downcast_arc::<linux_object::fs::Inotify>()
            .map_err(|_| LxError::EINVAL)?;
        inotify.rm_watch(wd)
    }

    /// `perf_event_open(2)`: open a performance-monitoring file descriptor.
    ///
    /// Implements software CPU-clock sampling (no hardware PMU). The returned fd
    /// supports `mmap` (ring buffer), `ioctl(ENABLE/DISABLE/...)`, `poll` and
    /// `read`; the timer tick feeds `PERF_RECORD_SAMPLE` records into the ring.
    pub fn sys_perf_event_open(
        &self,
        attr_ptr: usize,
        pid: i32,
        cpu: i32,
        group_fd: i32,
        flags: usize,
    ) -> SysResult {
        info!(
            "perf_event_open: attr={:#x} pid={} cpu={} group_fd={} flags={:#x}",
            attr_ptr, pid, cpu, group_fd, flags
        );
        if attr_ptr == 0 {
            return Err(LxError::EFAULT);
        }
        // `attr.size` is the u32 at byte offset 4. It used to be clamped to
        // a window, which answers a request this kernel cannot honour by
        // quietly reading a different struct than the one the caller built.
        let attr_size = perf_attr_size(UserInPtr::<u32>::from(attr_ptr + 4).read()?)?;
        let attr_bytes = UserInPtr::<u8>::from(attr_ptr).read_array(attr_size)?;
        if !crate::extensible_tail_is_empty(&attr_bytes[PERF_ATTR_SIZE_VER0..]) {
            return Err(LxError::E2BIG);
        }
        // A `pid` of 0 means "the calling process", not "the process whose id
        // is zero" — which is how `perf record ./prog` and every program that
        // profiles itself opens the event. Passing the literal 0 through made
        // the sampler compare it against the real pid of whoever was running,
        // so it matched nothing and the profile came out empty.
        let pid = if pid == 0 {
            self.zircon_process().id() as i32
        } else {
            pid
        };
        let event = PerfEvent::new(&attr_bytes, pid, cpu, OpenFlags::from_bits_truncate(flags));
        let fd = self.linux_process().add_file(event)?;
        Ok(fd.into())
    }
}

use kernel_hal::PAGE_SIZE;

/// Size of `struct perf_event_attr` version 0 (Linux `PERF_ATTR_SIZE_VER0`),
/// and the whole of what this kernel reads of it.
pub(crate) const PERF_ATTR_SIZE_VER0: usize = 64;

/// How many bytes of `struct perf_event_attr` `perf_event_open` will read, or
/// the errno Linux answers for that `size`.
///
/// `perf_copy_attr` and `sched_copy_attr` give the same two answers -- a zero
/// `size` means version 0, and anything else out of range is `E2BIG` -- which
/// is not what `clone3` does (`EINVAL` below the minimum) and not what
/// `sched_getattr` does (`EINVAL` at both ends). Four syscalls, four answers,
/// all of them deliberate; the one this code used to give was a fifth, and it
/// was to clamp, which is not an answer at all.
pub(crate) fn perf_attr_size(size: u32) -> Result<usize, LxError> {
    let size = if size == 0 {
        PERF_ATTR_SIZE_VER0
    } else {
        size as usize
    };
    if !(PERF_ATTR_SIZE_VER0..=PAGE_SIZE).contains(&size) {
        return Err(LxError::E2BIG);
    }
    Ok(size)
}

/// `fcntl(F_DUPFD, start)`: `f_dupfd` answers `EINVAL` to a start at or past
/// the soft `RLIMIT_NOFILE`, before looking for a free descriptor.
fn dupfd_start(start: usize, nofile: u64) -> Result<usize, LxError> {
    if start as u64 >= nofile || start > i32::MAX as usize {
        return Err(LxError::EINVAL);
    }
    Ok(start)
}

/// The descriptor `F_DUPFD`'s search settled on, which `alloc_fd` refuses
/// with `EMFILE` when it is past the limit: the search starts below it but
/// every number from there up may be taken.
fn dupfd_picked(fd: usize, nofile: u64) -> Result<FileDesc, LxError> {
    if fd as u64 >= nofile {
        return Err(LxError::EMFILE);
    }
    Ok(FileDesc::from(fd))
}

/// `dup2`/`dup3`'s target, which `ksys_dup3` answers `EBADF` to at or past
/// the soft `RLIMIT_NOFILE`. Checked on the raw word: a negative `int` from
/// the caller is a huge `usize` here and must not become descriptor -1, and a
/// value past 32 bits must not fold onto a small descriptor that is open.
fn dup_target(newfd: usize, nofile: u64) -> Result<FileDesc, LxError> {
    if newfd as u64 >= nofile || newfd > i32::MAX as usize {
        return Err(LxError::EBADF);
    }
    Ok(FileDesc::from(newfd))
}

#[cfg(test)]
mod dup_limit_tests {
    //! The three `RLIMIT_NOFILE` checks the dup family did not have. The
    //! limit is the process's soft one, 1024 by default; every value here is
    //! what a C caller's `int` becomes in the syscall register.

    use super::{dup_target, dupfd_picked, dupfd_start, FileDesc, LxError};

    const NOFILE: u64 = 1024;
    /// `(int)-1`, sign-extended into the register.
    const MINUS_ONE: usize = usize::MAX;

    #[test]
    fn a_dupfd_start_at_or_past_the_limit_is_einval() {
        // `fcntl(fd, F_DUPFD, -1)` used to hand the allocator `usize::MAX`;
        // the copy landed at descriptor -1 and the second call panicked the
        // kernel walking off the end of the open range.
        assert_eq!(dupfd_start(MINUS_ONE, NOFILE), Err(LxError::EINVAL));
        assert_eq!(dupfd_start(1024, NOFILE), Err(LxError::EINVAL));
        assert_eq!(dupfd_start(1023, NOFILE), Ok(1023));
        assert_eq!(dupfd_start(0, NOFILE), Ok(0));
        // A raised limit widens the window; a huge one still cannot admit a
        // start that is not a descriptor number.
        assert_eq!(dupfd_start(1024, 4096), Ok(1024));
        assert_eq!(dupfd_start(MINUS_ONE, u64::MAX), Err(LxError::EINVAL));
        assert_eq!(dupfd_start(1 << 32, u64::MAX), Err(LxError::EINVAL));
    }

    #[test]
    fn a_dupfd_that_lands_past_the_limit_is_emfile_not_a_descriptor() {
        // Start 1000 with 1000..1023 all open: the lowest free number is
        // 1024, which is not a descriptor this process may hold.
        assert_eq!(dupfd_picked(1024, NOFILE), Err(LxError::EMFILE));
        assert_eq!(dupfd_picked(1023, NOFILE), Ok(FileDesc::from(1023usize)));
        assert_eq!(dupfd_picked(3, NOFILE), Ok(FileDesc::from(3usize)));
    }

    #[test]
    fn a_dup2_target_at_or_past_the_limit_is_ebadf() {
        // `dup2(fd, -1)` installed the copy at descriptor -1; `dup2(fd,
        // 1 << 32 | 1)` narrowed to 1 and clobbered stdout.
        assert_eq!(dup_target(MINUS_ONE, NOFILE), Err(LxError::EBADF));
        assert_eq!(dup_target((1 << 32) | 1, NOFILE), Err(LxError::EBADF));
        assert_eq!(dup_target(1024, NOFILE), Err(LxError::EBADF));
        assert_eq!(dup_target(1023, NOFILE), Ok(FileDesc::from(1023usize)));
        assert_eq!(dup_target(1, NOFILE), Ok(FileDesc::from(1usize)));
        assert_eq!(dup_target(2048, 4096), Ok(FileDesc::from(2048usize)));
        // The narrowing guard stands on its own: with a limit that would
        // admit the number, `1 << 32 | 1` still cannot become descriptor 1.
        assert_eq!(dup_target((1 << 32) | 1, u64::MAX), Err(LxError::EBADF));
        assert_eq!(
            dup_target(i32::MAX as usize + 1, u64::MAX),
            Err(LxError::EBADF)
        );
        // Not EINVAL: `dup2` and `dup3` say EBADF here, and `F_DUPFD` says
        // EINVAL, and callers tell the two apart.
        assert_ne!(dup_target(MINUS_ONE, NOFILE), Err(LxError::EINVAL));
    }
}

#[cfg(test)]
mod open_inode_tests {
    use super::*;
    use kernel_hal::sync::Mutex;
    use linux_object::fs::devfs::PcmDev;
    use zcore_drivers::{scheme::AudioScheme, DeviceResult};

    struct FakeAudio {
        queued: Mutex<usize>,
        cap: usize,
    }

    impl FakeAudio {
        fn new(cap: usize) -> Self {
            Self {
                queued: Mutex::new(0),
                cap,
            }
        }
    }

    impl zcore_drivers::scheme::Scheme for FakeAudio {
        fn name(&self) -> &str {
            "fake-audio"
        }
    }

    impl AudioScheme for FakeAudio {
        fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
            Ok((rate, channels))
        }

        fn params(&self) -> (u32, u8) {
            (48_000, 2)
        }

        fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
            let mut queued = self.queued.lock();
            let n = self.cap.saturating_sub(*queued).min(pcm.len());
            *queued += n;
            Ok(n)
        }

        fn free_bytes(&self) -> usize {
            self.cap.saturating_sub(*self.queued.lock())
        }

        fn buffer_bytes(&self) -> usize {
            self.cap
        }

        fn queued_bytes(&self) -> usize {
            *self.queued.lock()
        }

        fn reset(&self) -> DeviceResult {
            *self.queued.lock() = 0;
            Ok(())
        }
    }

    #[test]
    fn pcm_inode_open_and_close_enforce_exclusivity() {
        let inode: Arc<dyn INode> = Arc::new(PcmDev::new(Arc::new(FakeAudio::new(4096)), 0));
        let first = prepare_open_inode(inode.clone()).unwrap();
        let file = File::new(first, OpenFlags::RDONLY, String::from("/dev/snd/pcmC0D0p"));
        assert!(matches!(
            prepare_open_inode(inode.clone()),
            Err(LxError::EBUSY)
        ));
        drop(file);
        assert!(prepare_open_inode(inode).is_ok());
    }
}

/// `flock(2)` is a `switch` on three exact values in Linux, and here it was a
/// bitmask over a truncated byte — so three requests the kernel refuses all
/// came back as "lock acquired".
///
/// The lock still is not taken (see `sys_flock`); what these pin down is that
/// a request Linux rejects is rejected here too, because reporting success is
/// how a caller decides the feature works.
#[cfg(test)]
mod flock_translate_tests {
    use super::*;

    #[test]
    fn the_three_commands_translate() {
        assert_eq!(flock_translate(1), Ok((FlockCmd::Shared, false)));
        assert_eq!(flock_translate(2), Ok((FlockCmd::Exclusive, false)));
        assert_eq!(flock_translate(8), Ok((FlockCmd::Unlock, false)));
    }

    #[test]
    fn lock_nb_rides_along_with_each_of_them() {
        // `cmd & ~LOCK_NB` is what the switch sees, and LOCK_NB itself is the
        // difference between blocking and EWOULDBLOCK. Dropping it here would
        // make every lock blocking, which is a hang, not an error.
        assert_eq!(flock_translate(1 | 4), Ok((FlockCmd::Shared, true)));
        assert_eq!(flock_translate(2 | 4), Ok((FlockCmd::Exclusive, true)));
        assert_eq!(flock_translate(8 | 4), Ok((FlockCmd::Unlock, true)));
    }

    #[test]
    fn naming_no_command_is_einval() {
        // `flock(fd, 0)` and `flock(fd, LOCK_NB)` name nothing to do. Through a
        // `bitflags` both parsed as the empty set and reported success.
        assert_eq!(flock_translate(0), Err(LxError::EINVAL));
        assert_eq!(flock_translate(4), Err(LxError::EINVAL));
    }

    #[test]
    fn naming_two_commands_at_once_is_einval() {
        // Linux switches on the exact value, so no pair of them is a command.
        for pair in [1 | 2, 1 | 8, 2 | 8, 1 | 2 | 8] {
            assert_eq!(flock_translate(pair), Err(LxError::EINVAL), "{:#x}", pair);
        }
    }

    #[test]
    fn a_high_bit_is_not_dropped_on_the_way_in() {
        // This is the truncation: 0x102 has low byte 2, so it took an
        // exclusive lock. `unsigned int` keeps 32 bits, so it is EINVAL.
        assert_eq!(flock_translate(0x102), Err(LxError::EINVAL));
        assert_eq!(flock_translate(0x100), Err(LxError::EINVAL));
        assert_eq!(flock_translate(usize::MAX), Err(LxError::EINVAL));
    }

    #[test]
    fn the_high_half_of_the_register_is_dropped_the_way_linux_drops_it() {
        // `cmd` is declared `unsigned int`, so the top 32 bits never reach the
        // switch. Being stricter than the kernel here would reject a caller
        // the kernel accepts.
        assert_eq!(
            flock_translate(0xdead_beef_0000_0002),
            Ok((FlockCmd::Exclusive, false))
        );
    }
}

#[cfg(test)]
mod anon_fd_flag_tests {
    //! `eventfd2`, `timerfd_create`, `signalfd4`, `inotify_init1` and
    //! `epoll_create1` are each handed a flag word by userspace and each has
    //! its own short list of bits. Four of the five used to run the word
    //! through `OpenFlags::from_bits_truncate` and keep whatever was left,
    //! so a flag this kernel had never heard of was not an error — it was
    //! nothing at all. `inotify_init1` was the one that checked.

    use super::*;

    /// The sets each caller passes, spelled as the uapi headers spell them.
    const EFD_SEMAPHORE: usize = 1;
    const EFD_FLAGS: usize = EFD_SEMAPHORE | ANON_CLOEXEC | ANON_NONBLOCK;
    const TFD_FLAGS: usize = ANON_CLOEXEC | ANON_NONBLOCK;
    const EPOLL_FLAGS: usize = ANON_CLOEXEC;

    /// `EFD_CLOEXEC`, `TFD_CLOEXEC`, `SFD_CLOEXEC`, `IN_CLOEXEC` and
    /// `EPOLL_CLOEXEC` are all `O_CLOEXEC`, and the NONBLOCK ones are all
    /// `O_NONBLOCK`. If these two constants drift from `OpenFlags`, every
    /// one of those syscalls silently stops honouring its flag: the fd would
    /// survive an `execve` it was asked to close on.
    #[test]
    fn the_shared_bits_are_the_open_flags_they_claim_to_be() {
        assert_eq!(ANON_CLOEXEC, 0o2_000_000);
        assert_eq!(ANON_NONBLOCK, 0o4_000);
        assert_eq!(OpenFlags::CLOEXEC.bits(), ANON_CLOEXEC);
        assert_eq!(OpenFlags::NON_BLOCK.bits(), ANON_NONBLOCK);
    }

    #[test]
    fn the_two_shared_flags_come_through_as_open_flags() {
        let f = anon_fd_flags(ANON_CLOEXEC | ANON_NONBLOCK, TFD_FLAGS).unwrap();
        assert!(f.close_on_exec());
        assert!(f.non_block());
        let none = anon_fd_flags(0, TFD_FLAGS).unwrap();
        assert!(!none.close_on_exec());
        assert!(!none.non_block());
    }

    /// The bug: a bit the syscall does not have was dropped in silence.
    #[test]
    fn a_bit_the_syscall_does_not_have_is_einval_and_not_ignored() {
        for stray in [1usize << 30, 1 << 21, 0o10, 1 << 63] {
            assert_eq!(
                anon_fd_flags(stray, TFD_FLAGS),
                Err(LxError::EINVAL),
                "flag {:#x}",
                stray
            );
            // ... including alongside a flag that IS valid, which is how a
            // truncating read hides it.
            assert_eq!(
                anon_fd_flags(stray | ANON_CLOEXEC, TFD_FLAGS),
                Err(LxError::EINVAL),
                "flag {:#x}",
                stray
            );
        }
    }

    /// Each caller's list is its own. `EFD_SEMAPHORE` is bit 0 and belongs
    /// to `eventfd2` alone; `epoll_create1` has no NONBLOCK at all.
    #[test]
    fn each_syscall_is_held_to_its_own_list() {
        assert!(anon_fd_flags(EFD_SEMAPHORE, EFD_FLAGS).is_ok());
        assert_eq!(
            anon_fd_flags(EFD_SEMAPHORE, TFD_FLAGS),
            Err(LxError::EINVAL)
        );
        assert!(anon_fd_flags(ANON_CLOEXEC, EPOLL_FLAGS).is_ok());
        assert_eq!(
            anon_fd_flags(ANON_NONBLOCK, EPOLL_FLAGS),
            Err(LxError::EINVAL)
        );
    }

    /// `EFD_SEMAPHORE` shares bit 0 with `OpenFlags::WRONLY`, and `EventFd`
    /// reads it back by that bit. Masking it away here would turn every
    /// semaphore eventfd into an ordinary counting one, which is a silent
    /// behaviour change in a synchronisation primitive.
    #[test]
    fn the_eventfd_semaphore_bit_survives_the_check() {
        let f = anon_fd_flags(EFD_SEMAPHORE | ANON_NONBLOCK, EFD_FLAGS).unwrap();
        assert_eq!(f.bits() & EFD_SEMAPHORE, EFD_SEMAPHORE);
        assert!(f.non_block());
    }
}

#[cfg(test)]
mod perf_attr_size_tests {
    //! `perf_event_attr` is the fourth extensible struct this kernel takes
    //! from userspace with a `size` beside it. It used to `clamp` that size,
    //! which is not one of the answers Linux gives: it reads a different
    //! struct than the one the caller built and reports success.

    use super::*;

    /// The window the clamp used to impose, as errors.
    #[test]
    fn a_size_outside_the_window_is_e2big_and_not_a_clamp() {
        // Too small: the clamp read 64 bytes of a struct the caller said was
        // 8 bytes long, so 56 bytes of whatever followed became the attr.
        assert_eq!(perf_attr_size(8), Err(LxError::E2BIG));
        assert_eq!(
            perf_attr_size(PERF_ATTR_SIZE_VER0 as u32 - 1),
            Err(LxError::E2BIG)
        );
        // Too big: the clamp read a page of a struct the caller said was
        // bigger, and then acted on it as if nothing had been left out.
        assert_eq!(perf_attr_size(PAGE_SIZE as u32 + 1), Err(LxError::E2BIG));
        assert_eq!(perf_attr_size(u32::MAX), Err(LxError::E2BIG));
    }

    /// `perf_copy_attr` has `sched_copy_attr`'s zero quirk, and for the same
    /// reason: the field postdates the syscall's first users.
    #[test]
    fn a_perf_attr_with_no_size_at_all_is_version_zero() {
        assert_eq!(perf_attr_size(0), Ok(PERF_ATTR_SIZE_VER0));
        assert_eq!(PERF_ATTR_SIZE_VER0, 64);
    }

    /// Everything from version 0 up to a page is read, and vetted.
    #[test]
    fn a_perf_attr_may_be_anything_from_version_zero_up_to_a_page() {
        for size in [PERF_ATTR_SIZE_VER0, 72, 96, 112, 128, PAGE_SIZE] {
            assert_eq!(perf_attr_size(size as u32), Ok(size), "{size}");
        }
    }
}

/// What the type of the inode an `open(2)` resolved to means for the flags it
/// was given.
///
/// Three of the refusals `may_open` and `do_open` make come down to the type
/// alone, and they come in this order upstream: `O_DIRECTORY` over a
/// non-directory, then a symbolic link the resolution stopped at, then a
/// directory opened for writing. Only the last of the three was made here,
/// because the first two flags never reached this far: `OpenFlags` did not
/// name `O_DIRECTORY` or `O_NOFOLLOW`, and `from_bits_truncate` drops what it
/// cannot name.
///
/// That `O_DIRECTORY` goes first matters for the one case where two of them
/// apply at once: a symbolic link opened with `O_DIRECTORY` is `ENOTDIR`, not
/// `ELOOP`, because it is not a directory whatever it points at.
pub(crate) fn open_resolved_type(flags: OpenFlags, type_: FileType) -> Result<(), LxError> {
    if flags.contains(OpenFlags::DIRECTORY) && type_ != FileType::Dir {
        return Err(LxError::ENOTDIR);
    }
    // Resolution only ever stops at a symbolic link when `O_NOFOLLOW` told it
    // to, so reaching one here is that flag's answer.
    if type_ == FileType::SymLink {
        return Err(LxError::ELOOP);
    }
    if type_ == FileType::Dir && flags.writable() {
        return Err(LxError::EISDIR);
    }
    Ok(())
}

#[cfg(test)]
mod open_flag_tests {
    use super::*;

    /// The numbers are UABI and every one of them is a bit userspace sends.
    /// A constant with the wrong value here is not a compile error anywhere:
    /// it is a flag that quietly means something else.
    #[test]
    fn the_flags_carry_the_numbers_userspace_sends() {
        for (flag, value) in [
            (OpenFlags::WRONLY, 0o1),
            (OpenFlags::RDWR, 0o2),
            (OpenFlags::CREATE, 0o100),
            (OpenFlags::EXCLUSIVE, 0o200),
            (OpenFlags::NOCTTY, 0o400),
            (OpenFlags::TRUNCATE, 0o1000),
            (OpenFlags::APPEND, 0o2000),
            (OpenFlags::NON_BLOCK, 0o4000),
            (OpenFlags::DSYNC, 0o10000),
            (OpenFlags::ASYNC, 0o20000),
            (OpenFlags::DIRECT, 0o40000),
            (OpenFlags::LARGEFILE, 0o100000),
            (OpenFlags::DIRECTORY, 0o200000),
            (OpenFlags::NOFOLLOW, 0o400000),
            (OpenFlags::NOATIME, 0o1000000),
            (OpenFlags::CLOEXEC, 0o2000000),
            (OpenFlags::SYNC, 0o4010000),
        ] {
            assert_eq!(flag.bits(), value, "{flag:?}");
        }
    }

    /// The two that used to fall out of `from_bits_truncate` are the two that
    /// change what the call does.
    #[test]
    fn the_flags_that_decide_survive_being_parsed() {
        let parsed = OpenFlags::from_bits_truncate(0o200000 | 0o400000 | 0o2);
        assert!(parsed.contains(OpenFlags::DIRECTORY));
        assert!(parsed.contains(OpenFlags::NOFOLLOW));
        assert!(parsed.writable());
    }

    /// `O_PATH` and `O_TMPFILE` stay unnamed on purpose: each changes what the
    /// descriptor IS, and naming a flag this kernel does not honour is the
    /// same silent lie as dropping one it should.
    #[test]
    fn the_flags_this_kernel_does_not_honour_stay_out() {
        for bit in [0o10000000usize, 0o20000000] {
            assert_eq!(OpenFlags::from_bits_truncate(bit), OpenFlags::RDONLY);
        }
    }

    #[test]
    fn a_directory_open_wants_a_directory() {
        let dir_only = OpenFlags::DIRECTORY;
        assert_eq!(open_resolved_type(dir_only, FileType::Dir), Ok(()));
        for other in [
            FileType::File,
            FileType::CharDevice,
            FileType::BlockDevice,
            FileType::NamedPipe,
            FileType::Socket,
        ] {
            assert_eq!(
                open_resolved_type(dir_only, other),
                Err(LxError::ENOTDIR),
                "{other:?}"
            );
        }
    }

    /// A symbolic link is not a directory whatever it points at, so the two
    /// flags together answer ENOTDIR and not ELOOP.
    #[test]
    fn a_link_asked_for_as_a_directory_is_not_a_directory() {
        assert_eq!(
            open_resolved_type(OpenFlags::DIRECTORY, FileType::SymLink),
            Err(LxError::ENOTDIR)
        );
        assert_eq!(
            open_resolved_type(OpenFlags::NOFOLLOW, FileType::SymLink),
            Err(LxError::ELOOP)
        );
    }

    /// Reaching a symbolic link at all means `O_NOFOLLOW` stopped there.
    #[test]
    fn a_link_reached_by_the_open_is_the_end_of_it() {
        assert_eq!(
            open_resolved_type(OpenFlags::RDONLY, FileType::SymLink),
            Err(LxError::ELOOP)
        );
    }

    /// The one check that was already here keeps its answer, and its place
    /// after the other two.
    #[test]
    fn a_directory_still_cannot_be_opened_for_writing() {
        assert_eq!(open_resolved_type(OpenFlags::RDONLY, FileType::Dir), Ok(()));
        assert_eq!(
            open_resolved_type(OpenFlags::WRONLY, FileType::Dir),
            Err(LxError::EISDIR)
        );
        assert_eq!(
            open_resolved_type(OpenFlags::RDWR, FileType::Dir),
            Err(LxError::EISDIR)
        );
        // And with O_DIRECTORY beside it the directory is still the thing it
        // asked for; only the write is wrong.
        assert_eq!(
            open_resolved_type(OpenFlags::DIRECTORY | OpenFlags::WRONLY, FileType::Dir),
            Err(LxError::EISDIR)
        );
    }

    #[test]
    fn an_ordinary_open_of_an_ordinary_file_is_none_of_their_business() {
        for flags in [
            OpenFlags::RDONLY,
            OpenFlags::RDWR,
            OpenFlags::NOFOLLOW,
            OpenFlags::NOATIME | OpenFlags::DIRECT | OpenFlags::LARGEFILE,
        ] {
            assert_eq!(
                open_resolved_type(flags, FileType::File),
                Ok(()),
                "{flags:?}"
            );
        }
    }
}
