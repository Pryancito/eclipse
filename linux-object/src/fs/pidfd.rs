//! Process file descriptor (`pidfd_open`).

use super::*;
use crate::sync::{Event, EventBus};
use alloc::sync::Arc;
use lock::Mutex;
use zircon_object::object::*;
use zircon_object::task::{Process, Status, Thread, ThreadState};

/// Linux `PIDFD_THREAD`: a pidfd of one thread rather than of its process.
///
/// The kernel spells it `O_EXCL`, so it is `1 << 7`, not 2. It stays in the
/// descriptor's flags, where `F_GETFL` shows it and `pidfd_send_signal` reads
/// it, as Linux keeps it in `f_flags`.
pub const PIDFD_THREAD: u32 = OpenFlags::EXCLUSIVE.bits() as u32;

/// `PIDFD_GET_INFO`, the one ioctl a pidfd answers: `_IOWR(0xff, 11, struct
/// pidfd_info)`, where the 72-byte struct makes the size field `0x48`. The
/// number encodes that size, so a call with a struct of another length is a
/// different number and falls through to `ENOTTY`, which is what Linux 6.18
/// answers for it (a 24-byte `0xc018ff0b` was `ENOTTY` when measured there),
/// and what any fd that is not a pidfd answers too.
///
/// A `u32`, matched against `request as u32`: the Linux ioctl command is a
/// 32-bit `unsigned int`, and bit 31 is set here (direction `_IOC_READ |
/// _IOC_WRITE`). musl types `ioctl`'s request `int`, so the command
/// sign-extends to `0xffff_ffff_c048_ff0b` on the way in, while glibc's
/// `unsigned long` keeps it clean; Linux truncates to 32 bits and sees both
/// as the same command, so matching the low 32 bits is what lets a musl
/// caller reach this ioctl at all (the DRM dispatch in `sys_ioctl` masks the
/// same way for the same reason).
const PIDFD_GET_INFO: u32 = 0xc048_ff0b;

/// The `mask` bits of `struct pidfd_info`, the groups of fields a reply
/// fills. Linux 6.18 ignores the mask the caller passes in and sets this to
/// what it provided. Two more bits exist and are deliberately never set
/// here: `PIDFD_INFO_CGROUPID` (`1 << 2`), because Eclipse has no cgroups,
/// and `PIDFD_INFO_EXIT` (`1 << 3`), because this kernel keeps no stash of a
/// task's exit code (a reaped pid is `ESRCH`, as on Linux for a child reaped
/// with `waitpid`, and a zombie's code is not yet available).
const PIDFD_INFO_PID: u64 = 1 << 0;
const PIDFD_INFO_CREDS: u64 = 1 << 1;

/// Linux's `struct pidfd_info` (72 bytes), the layout `PIDFD_GET_INFO`
/// fills. `#[repr(C)]` so each field lands where userspace reads it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PidfdInfo {
    mask: u64,
    cgroupid: u64,
    pid: u32,
    tgid: u32,
    ppid: u32,
    ruid: u32,
    rgid: u32,
    euid: u32,
    egid: u32,
    suid: u32,
    sgid: u32,
    fsuid: u32,
    fsgid: u32,
    exit_code: i32,
    coredump_mask: u32,
    spare1: u32,
}

/// Anonymous fd referring to a live or zombie process, or to one of its
/// threads (pollable on exit).
pub struct PidFd {
    base: KObjectBase,
    process: Arc<Process>,
    /// What a `PIDFD_THREAD` pidfd of a thread that is not its process's
    /// leader follows instead of the process: that thread, gone once it has
    /// exited, since nothing waits for it. A leader's is followed as its
    /// process is (`delay_group_leader`), so it has none here.
    thread: Option<Arc<Thread>>,
    open_flags: Mutex<OpenFlags>,
    eventbus: Arc<Mutex<EventBus>>,
}

impl_kobject!(PidFd);

impl PidFd {
    /// Create a pidfd for `process`. `open_flags` may include `O_NONBLOCK` /
    /// `O_CLOEXEC`, and `PIDFD_THREAD` for a pidfd of its leader.
    pub fn new(process: Arc<Process>, open_flags: OpenFlags) -> Arc<Self> {
        Self::of(process, None, open_flags)
    }

    /// A `PIDFD_THREAD` pidfd of `thread`, a thread of `process` that is not
    /// its leader.
    pub fn of_thread(
        process: Arc<Process>,
        thread: Arc<Thread>,
        open_flags: OpenFlags,
    ) -> Arc<Self> {
        let thread = (thread.id() != process.id()).then_some(thread);
        Self::of(process, thread, open_flags | OpenFlags::EXCLUSIVE)
    }

    fn of(process: Arc<Process>, thread: Option<Arc<Thread>>, open_flags: OpenFlags) -> Arc<Self> {
        let this = Self {
            base: KObjectBase::new(),
            process,
            thread,
            open_flags: Mutex::new(open_flags),
            eventbus: EventBus::new(),
        };
        if this.exited() {
            this.eventbus.lock().set(Event::READABLE);
        }
        Arc::new(this)
    }

    /// Resolve a pidfd from the caller's fd table. Any other descriptor is
    /// `EBADF`, as `pidfd_pid()` answers.
    pub fn from_file_like(file: Arc<dyn FileLike>) -> LxResult<Arc<Self>> {
        file.downcast_arc::<Self>().map_err(|_| LxError::EBADF)
    }

    /// Target process.
    pub fn target(&self) -> &Arc<Process> {
        &self.process
    }

    /// The thread a `PIDFD_THREAD` pidfd of a thread that is not the leader
    /// follows.
    pub fn thread(&self) -> Option<&Arc<Thread>> {
        self.thread.as_ref()
    }

    /// Whether this is a `PIDFD_THREAD` pidfd, of the leader or of another
    /// thread: what it says by itself is the thread's, not the process's.
    pub fn is_thread(&self) -> bool {
        self.flags().contains(OpenFlags::EXCLUSIVE)
    }

    /// The pid this pidfd names: its thread's, or its process's.
    pub fn task_id(&self) -> KoID {
        self.thread.as_ref().map_or(self.process.id(), |t| t.id())
    }

    /// The task its pid names, while that runs: its thread, or its process's
    /// leader (`get_pid_task(pid, PIDTYPE_PID)`). `None` once that one has
    /// exited, even with other threads of the process still running.
    pub fn task(&self) -> Option<Arc<Thread>> {
        let thread = match &self.thread {
            Some(thread) => thread.clone(),
            None => self
                .process
                .get_child(self.process.id())
                .ok()?
                .downcast_arc::<Thread>()
                .ok()?,
        };
        (thread.state() != ThreadState::Dead).then_some(thread)
    }

    /// Whether what it follows has exited: its thread, or its whole process.
    pub(crate) fn exited(&self) -> bool {
        match &self.thread {
            Some(thread) => thread.state() == ThreadState::Dead,
            None => matches!(self.process.status(), Status::Exited(_)),
        }
    }

    /// Whether the pid it names is gone: a thread that is not a leader once
    /// it has exited, a process once it is reaped. `pidfd_poll` says
    /// `POLLHUP` from then on, and `fdinfo` says -1.
    pub fn gone(&self) -> bool {
        match &self.thread {
            Some(_) => self.exited(),
            None => {
                use crate::process::ProcessExt;
                self.process.try_linux().is_some_and(|l| l.is_reaped())
            }
        }
    }

    /// `PIDFD_GET_INFO`: the pid/tgid/ppid the pidfd names and the target's
    /// credentials, written into the `struct pidfd_info` at `arg`.
    ///
    /// Measured on Linux 6.18: the `mask` the caller passes in is ignored —
    /// the kernel fills every field it can and sets the output `mask` to what
    /// it provided — so nothing is read from the struct first and the whole of
    /// it is written (a writable page is readable too, so an unreadable or
    /// unwritable one is `EFAULT` either way). A pid that is gone — a
    /// non-leader thread that has exited, a process that has been reaped — no
    /// longer names a task, so the answer is `ESRCH`.
    ///
    /// The reply here sets `PIDFD_INFO_PID | PIDFD_INFO_CREDS` and leaves the
    /// cgroup-id and exit-code fields zero, with their `mask` bits clear (see
    /// [`PIDFD_INFO_PID`]). A zombie that has not been reaped is answered from
    /// its process, which still exists: its pid and credentials, as on Linux.
    fn get_info(&self, arg: usize) -> LxResult<usize> {
        use crate::process::ProcessExt;
        if self.gone() {
            return Err(LxError::ESRCH);
        }
        let linux = self.process.try_linux().ok_or(LxError::ESRCH)?;
        let c = linux.credentials();
        let info = PidfdInfo {
            mask: PIDFD_INFO_PID | PIDFD_INFO_CREDS,
            pid: self.task_id() as u32,
            tgid: self.process.id() as u32,
            ppid: linux.parent().map_or(0, |p| p.id()) as u32,
            ruid: c.ruid,
            rgid: c.rgid,
            euid: c.euid,
            egid: c.egid,
            suid: c.suid,
            sgid: c.sgid,
            fsuid: c.fsuid,
            fsgid: c.fsgid,
            ..PidfdInfo::default()
        };
        kernel_hal::user::UserOutPtr::<PidfdInfo>::from(arg)
            .write(info)
            .map_err(|_| LxError::EFAULT)?;
        Ok(0)
    }
}

#[async_trait]
impl FileLike for PidFd {
    fn flags(&self) -> OpenFlags {
        *self.open_flags.lock()
    }

    /// With `PIDFD_THREAD`, which is `O_EXCL` and stays in a pidfd's
    /// `f_flags` (`pidfs_alloc_file`): Linux 6.18 says 0o202 to `F_GETFL`.
    fn status_flags(&self) -> OpenFlags {
        let flags = self.flags();
        flags.status() | (flags & OpenFlags::EXCLUSIVE)
    }

    /// pidfs keeps the name pidfds had as anonymous inodes (`pidfs_dname`).
    fn link_name(&self) -> String {
        String::from("anon_inode:[pidfd]")
    }

    /// `pidfd_show_fdinfo`: the pid, a zombie's included, and -1 once a
    /// `wait` has reaped it and the pid names nothing.
    fn fdinfo(&self) -> String {
        let pid = if self.gone() {
            -1
        } else {
            self.task_id() as i64
        };
        alloc::format!("Pid:\t{pid}\nNSpid:\t{pid}\n")
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        // Same trap as eventfd/epoll before `take_settable`: copying only
        // NON_BLOCK/CLOEXEC made `fcntl(F_SETFL, O_ASYNC)` "succeed" while
        // `F_GETFL` never showed the bit.
        self.open_flags.lock().take_settable(f);
        Ok(())
    }

    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        // pidfd is not seekable; pread must be ESPIPE (write stays EINVAL).
        Err(LxError::ESPIPE)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    /// `pidfd_poll`: readable once what it follows has exited, and hung up
    /// too once its pid is gone, asked for or not.
    fn poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let exited = self.exited();
        Ok(PollStatus {
            read: exited && events.wants_read(),
            write: false,
            error: false,
            hangup: self.gone(),
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        if self.exited() {
            return self.poll(events);
        }
        if self.open_flags.lock().non_block() {
            return Ok(PollStatus::default());
        }
        match &self.thread {
            Some(thread) => {
                let thread: Arc<dyn KernelObject> = thread.clone();
                thread.wait_signal(Signal::THREAD_TERMINATED).await;
            }
            None => {
                let process: Arc<dyn KernelObject> = self.process.clone();
                process.wait_signal(Signal::PROCESS_TERMINATED).await;
            }
        }
        self.eventbus.lock().set(Event::READABLE);
        self.poll(events)
    }

    /// A pidfd answers exactly one ioctl, `PIDFD_GET_INFO`. Anything else —
    /// another request, or `PIDFD_GET_INFO` with a struct of some other size,
    /// which is a different number — is unknown to a pidfd and is `ENOTTY`, as
    /// Linux answers it (`-ENOIOCTLCMD`, which the vfs turns into `-ENOTTY`).
    ///
    /// Matched on the low 32 bits (`request as u32`): the command is a 32-bit
    /// value that a musl caller sign-extends to 64 bits, so the full `request`
    /// would miss it (see [`PIDFD_GET_INFO`]).
    fn ioctl(&self, request: usize, arg1: usize, _arg2: usize, _arg3: usize) -> LxResult<usize> {
        match request as u32 {
            PIDFD_GET_INFO => self.get_info(arg1),
            _ => Err(LxError::ENOTTY),
        }
    }
}

#[cfg(test)]
mod set_flags_tests {
    use super::*;
    use zircon_object::task::Job;

    #[test]
    fn set_flags_keeps_o_async_on_a_pidfd() {
        let proc = Process::create(&Job::root(), "pidfd-flags").unwrap();
        let fd = PidFd::new(proc, OpenFlags::empty());
        assert!(!fd.flags().contains(OpenFlags::ASYNC));
        let mut f = fd.flags();
        f.set(OpenFlags::ASYNC, true);
        f.set(OpenFlags::NON_BLOCK, true);
        fd.set_flags(f).unwrap();
        assert!(fd.flags().contains(OpenFlags::ASYNC));
        assert!(fd.flags().non_block());
    }

    #[test]
    fn pread_is_espipe_and_pwrite_is_einval() {
        use async_std::task::block_on;
        let proc = Process::create(&Job::root(), "pidfd-seek").unwrap();
        let fd = PidFd::new(proc, OpenFlags::empty());
        let mut buf = [0u8; 8];
        assert_eq!(block_on(fd.read_at(0, &mut buf)), Err(LxError::ESPIPE));
        assert_eq!(fd.write_at(0, &[0u8; 8]), Err(LxError::EINVAL));
    }
}
