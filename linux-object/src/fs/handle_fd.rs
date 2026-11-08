//! A Zircon handle held by a Linux file descriptor.
//!
//! This is how a handle gets from one Linux process to another: the fd is an
//! ordinary open file, so `fork` inherits it, `exec` keeps it unless it is
//! close-on-exec, and `SCM_RIGHTS` carries it over a Unix socket. On the
//! other side the process takes the handle back out into its own handle
//! table. A server and its clients can be set up this way with nothing but
//! Linux plumbing, and once they share a channel, further handles travel in
//! channel messages as in Zircon.
//!
//! The fd also polls: readable while the object says `READABLE`, writable
//! while it says `WRITABLE`, hung up once the peer is gone, so a Linux event
//! loop can wait on a channel with `poll` or `epoll`.

use super::*;
use alloc::sync::Arc;
use lock::Mutex;
use zircon_object::object::*;

/// A file descriptor that holds one Zircon handle.
pub struct HandleFd {
    base: KObjectBase,
    /// `None` once the handle has been taken out ([`HandleFd::take`]).
    handle: Mutex<Option<Handle>>,
    /// What the handle was when it was wrapped, for `fdinfo` and polling
    /// after it is taken.
    object: Arc<dyn KernelObject>,
    rights: Rights,
    flags: Mutex<OpenFlags>,
}

impl_kobject!(HandleFd);

impl HandleFd {
    /// Wrap `handle`. Only `O_CLOEXEC` and `O_NONBLOCK` of `flags` mean
    /// anything to it.
    pub fn new(handle: Handle, flags: OpenFlags) -> Arc<Self> {
        let flags = (flags & (OpenFlags::CLOEXEC | OpenFlags::NON_BLOCK)) | OpenFlags::RDWR;
        Arc::new(HandleFd {
            base: KObjectBase::new(),
            object: handle.object.clone(),
            rights: handle.rights,
            handle: Mutex::new(Some(handle)),
            flags: Mutex::new(flags),
        })
    }

    /// Move the handle out, leaving the fd empty. Every process sharing the
    /// open file sees it empty afterwards: this is the way to pass a handle
    /// that cannot be duplicated, such as a channel end.
    pub fn take(&self) -> ZxResult<Handle> {
        self.handle.lock().take().ok_or(ZxError::BAD_STATE)
    }

    /// A new handle to the same object with the same rights; the fd keeps
    /// its own. Needs `ZX_RIGHT_DUPLICATE`, as `zx_handle_duplicate` does.
    pub fn duplicate(&self) -> ZxResult<Handle> {
        let guard = self.handle.lock();
        let handle = guard.as_ref().ok_or(ZxError::BAD_STATE)?;
        if !handle.rights.contains(Rights::DUPLICATE) {
            return Err(ZxError::ACCESS_DENIED);
        }
        Ok(handle.clone())
    }

    fn held(&self) -> bool {
        self.handle.lock().is_some()
    }

    fn status(&self, signal: Signal) -> PollStatus {
        PollStatus {
            read: signal.contains(Signal::READABLE),
            write: signal.contains(Signal::WRITABLE),
            error: false,
            hangup: signal.contains(Signal::PEER_CLOSED) || !self.held(),
        }
    }

    /// The signals that answer a poll for `events`.
    fn wanted(events: PollEvents) -> Signal {
        let mut mask = Signal::PEER_CLOSED;
        if events.wants_read() {
            mask |= Signal::READABLE;
        }
        if events.wants_write() {
            mask |= Signal::WRITABLE;
        }
        mask
    }
}

impl Drop for HandleFd {
    fn drop(&mut self) {
        // Out of the lock first: closing the handle runs the object's own
        // code (an end's peer gets PEER_CLOSED and its observers run).
        let handle = self.handle.get_mut().take();
        drop(handle);
    }
}

#[async_trait]
impl FileLike for HandleFd {
    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn link_name(&self) -> String {
        String::from("anon_inode:[zx_handle]")
    }

    fn fdinfo(&self) -> String {
        alloc::format!(
            "zx-koid:\t{}\nzx-type:\t{}\nzx-rights:\t{:#x}\nzx-held:\t{}\n",
            self.object.id(),
            self.object.type_name(),
            self.rights.bits(),
            self.held() as u8
        )
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.flags.lock().take_settable(f);
        Ok(())
    }

    /// The handle is used through the Zircon syscalls, not `read`/`write`.
    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ESPIPE)
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        Ok(self.status(self.object.signal()))
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let wanted = Self::wanted(events);
        let now = self.object.signal();
        if now.intersects(wanted) || !self.held() || self.flags().non_block() {
            return Ok(self.status(now));
        }
        let signal = self.object.wait_signal(wanted).await;
        Ok(self.status(signal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::task::block_on;
    use zircon_object::ipc::{Channel, MessagePacket};

    fn channel_fd() -> (Arc<HandleFd>, Arc<Channel>) {
        let (a, b) = Channel::create();
        let fd = HandleFd::new(Handle::new(a, Rights::DEFAULT_CHANNEL), OpenFlags::empty());
        (fd, b)
    }

    #[test]
    fn a_channel_end_is_taken_once_and_cannot_be_duplicated() {
        let (fd, _peer) = channel_fd();
        assert_eq!(fd.duplicate().err(), Some(ZxError::ACCESS_DENIED));
        let handle = fd.take().unwrap();
        assert_eq!(handle.object.type_name(), "Channel");
        assert_eq!(fd.take().err(), Some(ZxError::BAD_STATE));
        assert!(fd.poll(PollEvents::IN).unwrap().hangup);
    }

    #[test]
    fn a_duplicable_handle_is_shared_and_the_fd_keeps_its_own() {
        let (a, _b) = Channel::create();
        let fd = HandleFd::new(
            Handle::new(a, Rights::DEFAULT_CHANNEL | Rights::DUPLICATE),
            OpenFlags::empty(),
        );
        let one = fd.duplicate().unwrap();
        let two = fd.duplicate().unwrap();
        assert_eq!(one.object.id(), two.object.id());
        assert!(fd.take().is_ok());
    }

    #[test]
    fn the_fd_polls_readable_when_a_message_arrives_and_hung_up_when_the_peer_goes() {
        let (fd, peer) = channel_fd();
        let status = fd.poll(PollEvents::IN).unwrap();
        assert!(!status.read && status.write && !status.hangup);
        peer.write(MessagePacket {
            data: alloc::vec![1, 2, 3, 4],
            handles: alloc::vec::Vec::new(),
        })
        .unwrap();
        assert!(block_on(fd.async_poll(PollEvents::IN)).unwrap().read);
        drop(peer);
        assert!(fd.poll(PollEvents::IN).unwrap().hangup);
    }

    #[test]
    fn closing_the_fd_closes_the_handle() {
        let (fd, peer) = channel_fd();
        drop(fd);
        assert!(peer.signal().contains(Signal::PEER_CLOSED));
    }
}
