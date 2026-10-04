//! Process file descriptor (`pidfd_open`).

use super::*;
use crate::sync::{Event, EventBus};
use alloc::sync::Arc;
use lock::Mutex;
use zircon_object::object::*;
use zircon_object::task::{Process, Status};

/// Linux `PIDFD_THREAD` (unsupported here — pid must name a process).
///
/// The kernel spells it `O_EXCL`, so it is `1 << 7`, not 2. Both values end in
/// EINVAL from `pidfd_open` today — 2 because it is not a flag we know, 128
/// because it is one we refuse — so this is the number being right rather than
/// a behaviour change; it stops being harmless the day thread pidfds work.
pub const PIDFD_THREAD: u32 = OpenFlags::EXCLUSIVE.bits() as u32;

/// Anonymous fd referring to a live or zombie process (pollable on exit).
pub struct PidFd {
    base: KObjectBase,
    process: Arc<Process>,
    open_flags: Mutex<OpenFlags>,
    eventbus: Arc<Mutex<EventBus>>,
}

impl_kobject!(PidFd);

impl PidFd {
    /// Create a pidfd for `process`. `open_flags` may include `O_NONBLOCK` / `O_CLOEXEC`.
    pub fn new(process: Arc<Process>, open_flags: OpenFlags) -> Arc<Self> {
        let eventbus = EventBus::new();
        if matches!(process.status(), Status::Exited(_)) {
            eventbus.lock().set(Event::READABLE);
        }
        Arc::new(Self {
            base: KObjectBase::new(),
            process,
            open_flags: Mutex::new(open_flags),
            eventbus,
        })
    }

    /// Resolve a pidfd from the caller's fd table.
    pub fn from_file_like(file: Arc<dyn FileLike>) -> LxResult<Arc<Self>> {
        file.downcast_arc::<Self>().map_err(|_| LxError::EINVAL)
    }

    /// Target process.
    pub fn target(&self) -> &Arc<Process> {
        &self.process
    }

    pub(crate) fn exited(&self) -> bool {
        matches!(self.process.status(), Status::Exited(_))
    }
}

#[async_trait]
impl FileLike for PidFd {
    fn flags(&self) -> OpenFlags {
        *self.open_flags.lock()
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

    fn poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let exited = self.exited();
        Ok(PollStatus {
            read: exited && events.wants_read(),
            write: false,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        if self.exited() {
            return self.poll(events);
        }
        if self.open_flags.lock().non_block() {
            return Ok(PollStatus::default());
        }
        let proc_obj: Arc<dyn KernelObject> = self.process.clone();
        proc_obj.wait_signal(Signal::PROCESS_TERMINATED).await;
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
