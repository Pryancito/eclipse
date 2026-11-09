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
//!
//! An fd holding a VMO also maps: `mmap(MAP_SHARED)` on it maps that VMO,
//! the way `zx_vmar_map` would, with the Linux memory map in charge of the
//! address space. This is how a userspace server or driver reaches memory
//! that a VMO names -- a buffer shared with a client, a device's registers,
//! memory pinned for DMA. The handle's rights are the ceiling: it needs
//! `MAP` and `READ` to map at all, `WRITE` for a writable mapping and
//! `EXECUTE` for an executable one, and `mprotect` cannot raise a mapping
//! past them ([`HandleFd::map_ceiling`]).

use super::*;
use alloc::sync::Arc;
use kernel_hal::MMUFlags;
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

    /// The VMO the fd holds, if it holds one and its handle may be mapped
    /// (`MAP` and `READ`). `ENODEV` for an fd holding anything else, as
    /// Linux answers for a file that cannot be mapped; `EACCES` for a VMO
    /// handle without those rights; `EBADF` once the handle is taken.
    fn mappable_vmo(&self) -> LxResult<Arc<VmObject>> {
        let guard = self.handle.lock();
        let handle = guard.as_ref().ok_or(LxError::EBADF)?;
        let vmo = handle
            .object
            .clone()
            .downcast_arc::<VmObject>()
            .map_err(|_| LxError::ENODEV)?;
        if !handle.rights.contains(Rights::MAP | Rights::READ) {
            return Err(LxError::EACCES);
        }
        Ok(vmo)
    }

    /// The most a mapping of this fd may ever allow, `None` for an fd that
    /// does not hold a VMO. `mmap` refuses a `prot` past it and installs it
    /// as the mapping's ceiling, so `mprotect` cannot go past it later
    /// either: a read-only VMO handle stays read-only however it is mapped.
    pub fn map_ceiling(&self) -> Option<MMUFlags> {
        let guard = self.handle.lock();
        let handle = guard.as_ref()?;
        if handle.object.clone().downcast_arc::<VmObject>().is_err() {
            return None;
        }
        let mut flags = MMUFlags::USER;
        if handle.rights.contains(Rights::MAP | Rights::READ) {
            flags |= MMUFlags::READ;
            if handle.rights.contains(Rights::WRITE) {
                flags |= MMUFlags::WRITE;
            }
            if handle.rights.contains(Rights::EXECUTE) {
                flags |= MMUFlags::EXECUTE;
            }
        }
        Some(flags)
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

    /// `MAP_PRIVATE` is refused (`EINVAL`): the mapping would be the VMO
    /// itself, so a "private" store would reach every other holder, and a
    /// copy-on-write child does not exist for every VMO (a physical one has
    /// none). A server that wants a private copy makes the child itself.
    fn get_vmo(&self, _offset: usize, _len: usize) -> LxResult<Arc<VmObject>> {
        self.mappable_vmo()?;
        Err(LxError::EINVAL)
    }

    /// `MAP_SHARED`: the VMO itself, at `offset`. The window has to lie
    /// inside it (`ENXIO`, as for a device mapped past its end): past the
    /// end the mapping would otherwise be padded with anonymous zero pages
    /// that nobody else sees, which for a device's registers or a DMA
    /// buffer is a silent wrong answer.
    fn get_vmo_shared(&self, offset: usize, len: usize) -> LxResult<(Arc<VmObject>, usize)> {
        let vmo = self.mappable_vmo()?;
        match offset.checked_add(len) {
            Some(end) if end <= vmo.len() => Ok((vmo, offset)),
            _ => Err(LxError::ENXIO),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::task::block_on;
    use kernel_hal::PAGE_SIZE;
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

    fn vmo_fd(pages: usize, rights: Rights) -> (Arc<HandleFd>, Arc<VmObject>) {
        let vmo = VmObject::new_paged(pages);
        let fd = HandleFd::new(Handle::new(vmo.clone(), rights), OpenFlags::empty());
        (fd, vmo)
    }

    #[test]
    fn a_vmo_fd_maps_shared_as_the_vmo_itself_inside_its_bounds() {
        let (fd, vmo) = vmo_fd(2, Rights::DEFAULT_VMO);
        let (mapped, offset) = fd.get_vmo_shared(PAGE_SIZE, PAGE_SIZE).unwrap();
        assert!(Arc::ptr_eq(&mapped, &vmo));
        assert_eq!(offset, PAGE_SIZE);
        assert!(fd.get_vmo_shared(0, 2 * PAGE_SIZE).is_ok());
        assert_eq!(
            fd.get_vmo_shared(PAGE_SIZE, 2 * PAGE_SIZE).err(),
            Some(LxError::ENXIO)
        );
        assert_eq!(
            fd.get_vmo_shared(usize::MAX, PAGE_SIZE).err(),
            Some(LxError::ENXIO)
        );
    }

    #[test]
    fn a_vmo_fd_refuses_a_private_mapping() {
        let (fd, _vmo) = vmo_fd(1, Rights::DEFAULT_VMO);
        assert_eq!(fd.get_vmo(0, PAGE_SIZE).err(), Some(LxError::EINVAL));
    }

    #[test]
    fn the_handle_rights_are_the_mapping_ceiling() {
        let (fd, _vmo) = vmo_fd(1, Rights::DEFAULT_VMO);
        assert_eq!(
            fd.map_ceiling(),
            Some(MMUFlags::USER | MMUFlags::READ | MMUFlags::WRITE)
        );
        let (fd, _vmo) = vmo_fd(1, Rights::MAP | Rights::READ);
        assert_eq!(fd.map_ceiling(), Some(MMUFlags::USER | MMUFlags::READ));
        let (fd, _vmo) = vmo_fd(1, Rights::DEFAULT_VMO | Rights::EXECUTE);
        assert!(fd.map_ceiling().unwrap().contains(MMUFlags::EXECUTE));
        // Without MAP nothing at all, and the mapping itself is refused.
        let (fd, _vmo) = vmo_fd(1, Rights::READ | Rights::WRITE);
        assert_eq!(fd.map_ceiling(), Some(MMUFlags::USER));
        assert_eq!(fd.get_vmo_shared(0, PAGE_SIZE).err(), Some(LxError::EACCES));
    }

    #[test]
    fn only_a_held_vmo_maps() {
        let (fd, _peer) = channel_fd();
        assert_eq!(fd.map_ceiling(), None);
        assert_eq!(fd.get_vmo_shared(0, PAGE_SIZE).err(), Some(LxError::ENODEV));
        let (fd, _vmo) = vmo_fd(1, Rights::DEFAULT_VMO);
        fd.take().unwrap();
        assert_eq!(fd.map_ceiling(), None);
        assert_eq!(fd.get_vmo_shared(0, PAGE_SIZE).err(), Some(LxError::EBADF));
    }

    #[test]
    fn closing_the_fd_closes_the_handle() {
        let (fd, peer) = channel_fd();
        drop(fd);
        assert!(peer.signal().contains(Signal::PEER_CLOSED));
    }
}
