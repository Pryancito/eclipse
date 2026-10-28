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
