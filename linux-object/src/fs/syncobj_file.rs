//! A file object wrapping a DRM syncobj handle, for `SYNCOBJ_HANDLE_TO_FD`/
//! `SYNCOBJ_FD_TO_HANDLE` -- lets a syncobj cross a `fork`/`exec` or be
//! passed between processes (e.g. `SCM_RIGHTS` on a Unix socket), the same
//! way [`DmaBuf`](super::DmaBuf) does for PRIME GEM buffers.
//!
//! The syncobj table itself (`zcore_drivers::scheme::syncobj`) is a single
//! GLOBAL handle space, not per-process (see that module's own doc) -- so
//! unlike a real dma-buf, which carries actual backing memory, "export"
//! here doesn't move or copy any state: the handle number is already
//! globally valid, and this file just carries it across the fd boundary.
//! "Import" hands back that same handle number.
//!
//! Each exported fd holds a reference on the syncobj (`add_ref` at export /
//! `dup`, [`destroy`](zcore_drivers::scheme::syncobj::destroy) on Drop), so
//! `SYNCOBJ_DESTROY` on the creating handle does not free an object that still
//! has live fds — matching real DRM.
//!
//! # Polling a sync_file
//!
//! Mesa's `sync_wait()` is literally `poll(fd, POLLIN, timeout)`. Linux
//! `sync_file_poll` returns `EPOLLIN` once the wrapped fence is signaled.
//! Returning "never ready" here made every zero-timeout status check fail
//! and every blocking wait hang until the caller's deadline — which under
//! GLX/DRI3 (client ↔ Xwayland exchanging sync_files) collapses into
//! `zink: swapchain killed` / `GLXBadCurrentWindow` on `SwapBuffers`. A
//! Wayland-native client rarely polls these fds the same way.

use super::*;
use core::sync::atomic::{AtomicBool, Ordering};
use zircon_object::object::*;

/// A file object carrying a syncobj handle number across the fd boundary.
pub struct SyncobjHandle {
    base: KObjectBase,
    pub handle: u32,
    /// `None` for a syncobj fd (`HANDLE_TO_FD`): the fd names the object
    /// itself, and importing it hands the same handle back.
    ///
    /// `Some(point)` for a **`sync_file`** fd
    /// (`HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE`): the fd names a FENCE that was
    /// current on `handle` at export time, i.e. "`handle` reaching `point`".
    /// Importing one (`FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE`) gives that fence
    /// to a DIFFERENT syncobj, which is how Mesa hands a client's completed
    /// work to the X server and back — the one thing the two fd kinds must
    /// not confuse, since importing a sync_file as if it were a syncobj would
    /// alias the two objects instead of copying one fence between them.
    pub sync_file_point: Option<u64>,
    /// Latched the first time a sync_file fence is observed signaled. A real
    /// `sync_file` wraps the fence at export time; later reset/signal of the
    /// source syncobj must not make this fd go unready again.
    signaled: AtomicBool,
}

impl_kobject!(SyncobjHandle);

impl SyncobjHandle {
    pub fn new(handle: u32) -> Arc<Self> {
        Arc::new(Self {
            base: KObjectBase::new(),
            handle,
            sync_file_point: None,
            signaled: AtomicBool::new(false),
        })
    }

    /// A `sync_file` fd: the fence "`handle` reaches `point`".
    pub fn new_sync_file(handle: u32, point: u64) -> Arc<Self> {
        // If the snapshot is already satisfied at export time (the common
        // case: EXEC signals its syncobjs before returning), latch ready
        // immediately so the first `sync_wait(fd, 0)` succeeds.
        let already = zcore_drivers::scheme::syncobj::query(handle)
            .map(|p| p >= point)
            .unwrap_or(false);
        Arc::new(Self {
            base: KObjectBase::new(),
            handle,
            sync_file_point: Some(point),
            signaled: AtomicBool::new(already),
        })
    }

    /// Whether this fd should report `POLLIN` (sync_file fence reached).
    fn fence_ready(&self) -> bool {
        let Some(point) = self.sync_file_point else {
            // Opaque syncobj fds are not polled by Mesa's sync_wait path.
            return false;
        };
        if self.signaled.load(Ordering::Acquire) {
            return true;
        }
        let reached = zcore_drivers::scheme::syncobj::query(self.handle)
            .map(|p| p >= point)
            .unwrap_or(false);
        if reached {
            self.signaled.store(true, Ordering::Release);
        }
        reached
    }
}

impl Drop for SyncobjHandle {
    fn drop(&mut self) {
        // Last fd reference: drop the syncobj table ref taken at export/dup.
        let _ = zcore_drivers::scheme::syncobj::destroy(self.handle);
    }
}

#[async_trait]
impl FileLike for SyncobjHandle {
    fn flags(&self) -> OpenFlags {
        OpenFlags::RDWR | OpenFlags::CLOEXEC
    }

    fn set_flags(&self, _f: OpenFlags) -> LxResult {
        Ok(())
    }

    fn dup(&self) -> Arc<dyn FileLike> {
        // Another fd reference — bump the syncobj refcount to match Drop.
        let _ = zcore_drivers::scheme::syncobj::add_ref(self.handle);
        Arc::new(Self {
            base: KObjectBase::new(),
            handle: self.handle,
            sync_file_point: self.sync_file_point,
            signaled: AtomicBool::new(self.signaled.load(Ordering::Acquire)),
        })
    }

    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }

    /// Match Linux `sync_file_poll`: `POLLIN` once the wrapped fence is
    /// signaled. Opaque syncobj fds stay never-ready (Mesa does not poll them
    /// via `sync_wait`).
    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        let ready = self.fence_ready();
        Ok(PollStatus {
            read: ready,
            write: false,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        // sys_poll uses the sync `poll` path; keep this consistent for any
        // leftover caller.
        self.poll(events)
    }
}

#[cfg(test)]
mod sync_file_poll_tests {
    use super::*;

    /// The regression this guards. Mesa's `sync_wait(fd, 0)` is a zero-timeout
    /// `poll(POLLIN)`. With EXEC having already signaled the syncobj before
    /// export, the first check must see ready — "never ready" made every
    /// DRI3 fence handshake look failed and Zink killed the GLX swapchain.
    #[test]
    fn a_sync_file_exported_after_signal_is_immediately_pollable() {
        let handle = zcore_drivers::scheme::syncobj::create(false);
        assert!(zcore_drivers::scheme::syncobj::timeline_signal(handle, 3));
        let fd = SyncobjHandle::new_sync_file(handle, 3);
        // Export takes its own table ref; mirror that so Drop's destroy is a
        // dec_ref, not a free under the test's feet.
        assert!(zcore_drivers::scheme::syncobj::add_ref(handle));
        let status = fd.poll(PollEvents::IN).expect("poll");
        assert!(
            status.read,
            "sync_wait(fd, 0) must succeed when the fence was already met at export"
        );
        drop(fd);
    }

    /// Until the point arrives, poll stays quiet — same as an unsignaled
    /// Linux sync_file.
    #[test]
    fn a_sync_file_for_a_future_point_is_not_ready_yet() {
        let handle = zcore_drivers::scheme::syncobj::create(false);
        assert!(zcore_drivers::scheme::syncobj::timeline_signal(handle, 1));
        assert!(zcore_drivers::scheme::syncobj::add_ref(handle));
        let fd = SyncobjHandle::new_sync_file(handle, 5);
        let status = fd.poll(PollEvents::IN).expect("poll");
        assert!(!status.read, "point 5 has not been reached");
        assert!(zcore_drivers::scheme::syncobj::timeline_signal(handle, 5));
        let status = fd.poll(PollEvents::IN).expect("poll after signal");
        assert!(status.read, "once the point lands, POLLIN must fire");
        drop(fd);
    }
}
