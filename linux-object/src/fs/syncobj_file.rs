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

    fn dup(&self) -> Arc<dyn FileLike> {
        // Another fd reference — bump the syncobj refcount to match Drop.
        let _ = zcore_drivers::scheme::syncobj::add_ref(self.handle);
        let duped = Arc::new(Self {
            base: KObjectBase::new(),
            handle: self.handle,
            sync_file_point: self.sync_file_point,
            signaled: self.signaled.clone(),
            eventbus: self.eventbus.clone(),
        });
        // A dup of an unready sync_file shares the waiter via the Arc pair;
        // no second registration.
        duped
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
            if !events.contains(PollEvents::IN) || status.read {
                return Ok(status);
            }
            let bus = self.eventbus.clone();
            crate::sync::wait_for_event(bus, Event::READABLE).await;
        }
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        if self.sync_file_point.is_none() {
            // Opaque syncobj fds are not the sync_wait path; leave them
            // unsubscribable so sys_poll keeps its short tick.
            return None;
        }
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
