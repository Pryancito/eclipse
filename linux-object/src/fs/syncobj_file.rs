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
//! Wayland-native client rarely polls these fds the same way; it uses
//! `SYNCOBJ_EVENTFD` instead.
//!
//! Hardware fences advance only when something calls
//! [`poll_pending`](zcore_drivers::scheme::syncobj::poll_pending). The
//! eventfd path already arms a short timer for that; sync_file waiters must
//! do the same, and must publish [`Event::READABLE`] so `sys_poll`'s
//! `subscribe_readiness` wakes the moment the point lands rather than on a
//! timer tick (or never).

use super::*;
use crate::sync::{Event, EventBus};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lock::Mutex;
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
    signaled: Arc<AtomicBool>,
    /// Wakes `sys_poll` subscribers when the fence becomes ready.
    eventbus: Arc<Mutex<EventBus>>,
}

/// One live sync_file that is still waiting for its point. Dropped once
/// signaled (or when the syncobj disappears).
struct SyncFileWaiter {
    handle: u32,
    point: u64,
    signaled: Arc<AtomicBool>,
    eventbus: Arc<Mutex<EventBus>>,
}

lazy_static::lazy_static! {
    static ref WAITERS: Mutex<Vec<SyncFileWaiter>> = Mutex::new(Vec::new());
}
static WAITER_COUNT: AtomicUsize = AtomicUsize::new(0);

impl_kobject!(SyncobjHandle);

impl SyncobjHandle {
    pub fn new(handle: u32) -> Arc<Self> {
        Arc::new(Self {
            base: KObjectBase::new(),
            handle,
            sync_file_point: None,
            signaled: Arc::new(AtomicBool::new(false)),
            eventbus: EventBus::new(),
        })
    }

    /// A `sync_file` fd: the fence "`handle` reaches `point`".
    pub fn new_sync_file(handle: u32, point: u64) -> Arc<Self> {
        // If the snapshot is already satisfied at export time (the common
        // case on the software path: EXEC signals its syncobjs before
        // returning), latch ready immediately so the first `sync_wait(fd, 0)`
        // succeeds. On real hardware the fence is often still in flight —
        // register a waiter and arm the HW-fence poller instead.
        let already = zcore_drivers::scheme::syncobj::query(handle)
            .map(|p| p >= point)
            .unwrap_or(false);
        let signaled = Arc::new(AtomicBool::new(already));
        let eventbus = EventBus::new();
        if already {
            eventbus.lock().set(Event::READABLE);
        } else {
            register_waiter(handle, point, signaled.clone(), eventbus.clone());
        }
        Arc::new(Self {
            base: KObjectBase::new(),
            handle,
            sync_file_point: Some(point),
            signaled,
            eventbus,
        })
    }

    /// `SYNCOBJ_FD_TO_HANDLE` without `IMPORT_SYNC_FILE`: hand the importing
    /// process the handle number, together with a reference of its own.
    ///
    /// In Linux every import creates a new handle in the importer's table, and
    /// each handle is one reference on the object, so the importer's later
    /// `SYNCOBJ_DESTROY` drops only what the import took. Here the handle
    /// space is global and the number comes back unchanged, which made the
    /// import free: the only references were the creator's and the fd's, and
    /// the fd is closed right after importing. A GLX client's swapchain
    /// syncobj travels client -> Xwayland -> labwc through two such imports,
    /// and each of those two frees its handle on its own schedule
    /// (`xcb_dri3_free_syncobj`, the `wp_linux_drm_syncobj_timeline_v1`
    /// destroy), so whichever of the three destroyed first destroyed it for
    /// the other two: the client's next wait answered ENOENT, the
    /// compositor's next `SYNCOBJ_EVENTFD` failed.
    ///
    /// `None` for a sync_file fd (a fence, not an object; the caller reports
    /// the flag mismatch) or a syncobj that no longer exists.
    pub fn import_opaque(&self) -> Option<u32> {
        if self.sync_file_point.is_some() {
            return None;
        }
        if !zcore_drivers::scheme::syncobj::add_ref(self.handle) {
            return None;
        }
        Some(self.handle)
    }

    /// `SYNC_IOC_MERGE`: a sync_file that is ready once both `self` and
    /// `other` are (a `dma_fence_array` in Linux). `None` unless both fds are
    /// sync_files, which Linux answers with `ENOENT`.
    ///
    /// Mesa's window-system code merges two of these on every acquire of a
    /// swapchain image that has been presented before ("the compositor
    /// released it" and "its previous present completed"), and imports the
    /// result into the acquire semaphore. With no ioctl at all on a sync_file
    /// fd, that acquire failed after the first `image_count` frames and zink
    /// killed the GLX swapchain: `zink: swapchain killed`, then
    /// `GLXBadCurrentWindow` from the `glXSwapBuffers` fallback, with nothing
    /// in dmesg because the client's error print is compiled out.
    pub fn merge(&self, other: &SyncobjHandle) -> Option<Arc<Self>> {
        let a = self.sync_file_point?;
        let b = other.sync_file_point?;
        let merged =
            zcore_drivers::scheme::syncobj::merge_fences(&[(self.handle, a), (other.handle, b)]);
        // The merged syncobj's one reference belongs to this fd.
        Some(Self::new_sync_file(merged, 1))
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
        // Hardware fences only advance when something resolves them. Without
        // this, a blocking `sync_wait` re-polls forever against a stale point
        // and Mesa kills the GLX swapchain.
        let _ = zcore_drivers::scheme::syncobj::poll_pending();
        let reached = zcore_drivers::scheme::syncobj::query(self.handle)
            .map(|p| p >= point)
            .unwrap_or(false);
        if reached {
            self.publish_ready();
        }
        reached
    }

    fn publish_ready(&self) {
        if self
            .signaled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.eventbus.lock().set(Event::READABLE);
        } else {
            // Already latched (e.g. by the signal hook); still publish so a
            // late subscriber sees the latched bit.
            self.eventbus.lock().set(Event::READABLE);
        }
    }
}

fn register_waiter(
    handle: u32,
    point: u64,
    signaled: Arc<AtomicBool>,
    eventbus: Arc<Mutex<EventBus>>,
) {
    {
        let mut waiters = WAITERS.lock();
        waiters.push(SyncFileWaiter {
            handle,
            point,
            signaled,
            eventbus,
        });
        WAITER_COUNT.store(waiters.len(), Ordering::SeqCst);
    }
    // Same poller the SYNCOBJ_EVENTFD path uses: without it, a GPU fence that
    // lands with nobody else issuing syncobj ioctls is never noticed.
    super::syncobj_eventfd::ensure_hw_fence_poller();
}

/// How many sync_file fds are still waiting on a future point. The eventfd
/// poller consults this so it keeps running when only GLX/DRI3 (not wlroots
/// eventfd) is waiting.
pub(super) fn pending_waiter_count() -> usize {
    WAITER_COUNT.load(Ordering::Relaxed)
}

/// Called from the syncobj point-advance hook (and from our own poll path
/// after `poll_pending`). Latch + wake every sync_file whose point is now
/// reached.
pub(super) fn wake_ready_waiters() {
    if WAITER_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut fire: Vec<(Arc<AtomicBool>, Arc<Mutex<EventBus>>)> = Vec::new();
    {
        let mut waiters = WAITERS.lock();
        let mut i = 0;
        while i < waiters.len() {
            match zcore_drivers::scheme::syncobj::query(waiters[i].handle) {
                Some(cur) if cur >= waiters[i].point => {
                    let w = waiters.swap_remove(i);
                    fire.push((w.signaled, w.eventbus));
                }
                None => {
                    // Syncobj gone: the fd can never become ready. Drop the
                    // waiter (Mesa will time out the same way Linux does).
                    waiters.swap_remove(i);
                }
                _ => i += 1,
            }
        }
        WAITER_COUNT.store(waiters.len(), Ordering::SeqCst);
    }
    for (signaled, bus) in fire {
        signaled.store(true, Ordering::Release);
        bus.lock().set(Event::READABLE);
    }
}

impl Drop for SyncobjHandle {
    fn drop(&mut self) {
        // Last fd reference: drop the syncobj table ref taken at export/dup.
        let _ = zcore_drivers::scheme::syncobj::destroy(self.handle);
        // Drop any waiter we registered (fd closed before the fence landed).
        if let Some(point) = self.sync_file_point {
            if !self.signaled.load(Ordering::Acquire) {
                let mut waiters = WAITERS.lock();
                waiters.retain(|w| {
                    !(core::ptr::eq(Arc::as_ptr(&w.signaled), Arc::as_ptr(&self.signaled))
                        && w.handle == self.handle
                        && w.point == point)
                });
                WAITER_COUNT.store(waiters.len(), Ordering::SeqCst);
            }
        }
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
        loop {
            let status = self.poll(events)?;
            if !events.wants_read() || status.read {
                return Ok(status);
            }
            let bus = self.eventbus.clone();
            crate::sync::wait_for_event(bus, Event::READABLE).await?;
        }
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        // Opaque syncobj fds are not the sync_wait path; leave them
        // unsubscribable so sys_poll keeps its short tick.
        self.sync_file_point?;
        // Re-check before parking: a fence that landed between the scan and
        // subscribe must latch READABLE so the EventBus fires the waker now.
        let _ = self.fence_ready();
        let mask = super::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_readiness_on(
            &self.eventbus,
            mask,
            waker,
        ))
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
        // The signal hook (or a direct poll after an explicit signal) must
        // publish READABLE.
        wake_ready_waiters();
        let status = fd.poll(PollEvents::IN).expect("poll after signal");
        assert!(status.read, "once the point lands, POLLIN must fire");
        drop(fd);
    }

    /// The X11 route of a swapchain image: the client exports the syncobj,
    /// Xwayland imports it and closes the fd, and later frees its handle on
    /// its own schedule. The client must still own its object afterwards.
    #[test]
    fn an_importer_freeing_its_handle_does_not_take_the_creators_syncobj() {
        let handle = zcore_drivers::scheme::syncobj::create(false);
        // HANDLE_TO_FD: the fd takes its own reference.
        assert!(zcore_drivers::scheme::syncobj::add_ref(handle));
        let fd = SyncobjHandle::new(handle);
        // FD_TO_HANDLE in the importing process, then close(fd).
        let imported = fd.import_opaque().expect("a live syncobj imports");
        assert_eq!(imported, handle, "the handle space is global");
        drop(fd);
        // xcb_dri3_free_syncobj -> drmSyncobjDestroy in the importer.
        assert!(zcore_drivers::scheme::syncobj::destroy(imported));
        assert!(
            zcore_drivers::scheme::syncobj::timeline_signal(handle, 3),
            "the creator's handle must survive the importer's destroy"
        );
        assert_eq!(zcore_drivers::scheme::syncobj::query(handle), Some(3));
        // The creator's own destroy is the last reference.
        assert!(zcore_drivers::scheme::syncobj::destroy(handle));
        assert_eq!(zcore_drivers::scheme::syncobj::query(handle), None);
    }

    /// Mesa's external-sync probe: export and import inside ONE process, then
    /// destroy both handles. Every reference must be accounted for, so the
    /// object is gone after the second destroy and not before.
    #[test]
    fn an_export_import_round_trip_in_one_process_balances_its_references() {
        let handle = zcore_drivers::scheme::syncobj::create(false);
        assert!(zcore_drivers::scheme::syncobj::add_ref(handle));
        let fd = SyncobjHandle::new(handle);
        let imported = fd.import_opaque().expect("a live syncobj imports");
        drop(fd);
        assert!(zcore_drivers::scheme::syncobj::destroy(imported));
        assert_eq!(
            zcore_drivers::scheme::syncobj::query(handle),
            Some(0),
            "one handle is still held"
        );
        assert!(zcore_drivers::scheme::syncobj::destroy(handle));
        assert_eq!(zcore_drivers::scheme::syncobj::query(handle), None);
    }

    /// A sync_file fd is a fence, not the object: importing it as a syncobj
    /// would alias the two, so `import_opaque` refuses and takes no reference.
    #[test]
    fn a_sync_file_fd_is_not_importable_as_a_syncobj() {
        let handle = zcore_drivers::scheme::syncobj::create(false);
        assert!(zcore_drivers::scheme::syncobj::add_ref(handle));
        let fd = SyncobjHandle::new_sync_file(handle, 1);
        assert!(fd.import_opaque().is_none());
        drop(fd);
        assert!(zcore_drivers::scheme::syncobj::destroy(handle));
        assert_eq!(zcore_drivers::scheme::syncobj::query(handle), None);
    }

    /// `SYNC_IOC_MERGE`: ready only once both fences are.
    #[test]
    fn a_merged_sync_file_is_ready_only_when_both_fences_are() {
        let a = zcore_drivers::scheme::syncobj::create(false);
        let b = zcore_drivers::scheme::syncobj::create(false);
        assert!(zcore_drivers::scheme::syncobj::add_ref(a));
        assert!(zcore_drivers::scheme::syncobj::add_ref(b));
        let fa = SyncobjHandle::new_sync_file(a, 2);
        let fb = SyncobjHandle::new_sync_file(b, 1);
        let m = fa.merge(&fb).expect("two sync_files merge");
        assert!(!m.poll(PollEvents::IN).expect("poll").read);
        assert!(zcore_drivers::scheme::syncobj::timeline_signal(a, 2));
        wake_ready_waiters();
        assert!(
            !m.poll(PollEvents::IN).expect("poll").read,
            "one fence of two is not enough"
        );
        assert!(zcore_drivers::scheme::syncobj::timeline_signal(b, 1));
        wake_ready_waiters();
        assert!(
            m.poll(PollEvents::IN).expect("poll").read,
            "both fences reached: the merged sync_file is ready"
        );
        drop(m);
        drop(fa);
        drop(fb);
        assert!(zcore_drivers::scheme::syncobj::destroy(a));
        assert!(zcore_drivers::scheme::syncobj::destroy(b));
    }

    /// Mesa imports the merged sync_file into the acquire semaphore and then
    /// closes every fd and destroys every surrogate in one go. The semaphore
    /// must keep waiting for both fences.
    #[test]
    fn closing_the_merged_fd_after_import_keeps_the_dependency() {
        use zcore_drivers::scheme::syncobj as so;
        let a = so::create(false);
        let b = so::create(false);
        assert!(so::add_ref(a));
        assert!(so::add_ref(b));
        let fa = SyncobjHandle::new_sync_file(a, 1);
        let fb = SyncobjHandle::new_sync_file(b, 1);
        let m = fa.merge(&fb).expect("merge");
        let sem = so::create(false);
        assert!(so::import_snapshot(sem, m.handle, 1));
        drop(m);
        drop(fa);
        drop(fb);
        assert_eq!(so::query(sem), Some(0), "nothing has signaled");
        assert!(so::timeline_signal(a, 1));
        assert_eq!(so::query(sem), Some(0), "one of two");
        assert!(so::timeline_signal(b, 1));
        assert_eq!(so::query(sem), Some(1), "both: the semaphore signals");
        assert!(so::destroy(sem));
        assert!(so::destroy(a));
        assert!(so::destroy(b));
    }

    /// An opaque syncobj fd is the object, not a fence: it does not merge.
    #[test]
    fn an_opaque_syncobj_fd_does_not_merge() {
        let a = zcore_drivers::scheme::syncobj::create(false);
        assert!(zcore_drivers::scheme::syncobj::add_ref(a));
        assert!(zcore_drivers::scheme::syncobj::add_ref(a));
        let opaque = SyncobjHandle::new(a);
        let fence = SyncobjHandle::new_sync_file(a, 1);
        assert!(opaque.merge(&fence).is_none());
        assert!(fence.merge(&opaque).is_none());
        drop(opaque);
        drop(fence);
        assert!(zcore_drivers::scheme::syncobj::destroy(a));
    }

    #[test]
    fn subscribe_readiness_is_offered_for_sync_files() {
        let handle = zcore_drivers::scheme::syncobj::create(false);
        assert!(zcore_drivers::scheme::syncobj::add_ref(handle));
        let fd = SyncobjHandle::new_sync_file(handle, 1);
        // A no-op waker is enough: we only care that the fd participates in
        // sys_poll's covered path (Some(...)), not that it fires.
        let waker = {
            use core::task::{RawWaker, RawWakerVTable, Waker};
            fn clone(p: *const ()) -> RawWaker {
                RawWaker::new(p, &VTABLE)
            }
            fn wake(_: *const ()) {}
            fn wake_by_ref(_: *const ()) {}
            fn drop(_: *const ()) {}
            static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
            unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
        };
        assert!(
            fd.subscribe_readiness(PollEvents::IN, &waker).is_some(),
            "sys_poll must be able to park on a sync_file"
        );
        drop(fd);
    }
}
