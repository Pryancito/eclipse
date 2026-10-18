//! `INode` for the card node: read of the event queue, write, poll,
//! mmap and the ioctl entry. Out of `drm_scheme.rs` for its size ceiling.

use super::*;

impl INode for DrmDev {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        // Deliver queued DRM events (page-flip completions). When none are
        // pending report `Again` so a non-blocking reader gets EAGAIN and an
        // epoll/poll waiter re-checks on the next tick.
        match self.file.read_events(buf) {
            drm::EventRead::Read(n) => Ok(n),
            drm::EventRead::Empty => Err(FsError::Again),
            // A buffer the first event does not fit in: `drm_read()` puts
            // the event back and returns what it has read so far, which is
            // 0 -- not EAGAIN (a livelock: the queue is non-empty, so
            // READABLE stays set and a blocking reader's wait resolves
            // instantly, over and over) and not EINVAL, which this answered
            // and `drmHandleEvent` reports as a failed read.
            drm::EventRead::TooSmall => Ok(0),
        }
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        // The DRM file operations have no `.write`, so `vfs_write` refuses
        // with EINVAL. This swallowed the bytes and reported them written.
        Err(FsError::InvalidParam)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            // Linux `drm_poll`: POLLIN when the event queue is non-empty;
            // never POLLOUT (the chardev is not writable).
            read: self.file.has_events(),
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        // Lightweight waiter: do not nest an `async move { loop { ... } }`
        // state machine. Poll/epoll already use sync `poll()`; this path is
        // for blocking reads and any leftover async_poll callers.
        let bus = self.file.eventbus();
        Box::pin(DrmEventWait {
            dev: self,
            bus,
            sub_id: None,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            // Render nodes are world-rw on Linux (udev's `uaccess`/render
            // group; Alpine's mdev ships 0666), so report the same. NOTE this
            // is fidelity, not a functional fix: nothing in this kernel
            // enforces `Metadata::mode` on open (its only consumers are
            // stat/statx), so 0o660 was never actually blocking NVK's
            // `open(renderD128, O_RDWR)`. It will start mattering the day a
            // permission model lands. The primary node keeps 0660 (it is the
            // privileged KMS device).
            mode: if self.minor >= 128 { 0o666 } else { 0o660 },
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(0xe2, self.minor as usize), // 226 is DRM major
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        drm_ioctl(self, cmd, data)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}
