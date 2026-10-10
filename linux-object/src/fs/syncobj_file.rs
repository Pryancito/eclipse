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
    flags: Mutex<OpenFlags>,
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
            flags: Mutex::new(OpenFlags::RDWR | OpenFlags::CLOEXEC),
        })
    }

    /// A `sync_file` fd: the fence "`handle` reaches `point`".
    pub fn new_sync_file(handle: u32, point: u64) -> Arc<Self> {
        // If the snapshot is already satisfied at export time (the common
        // case on the software path: EXEC signals its syncobjs before
        // returning), latch ready immediately so the first `sync_wait(fd, 0)`
        // succeeds. On real hardware the fence is often still in flight —
        // register a waiter and arm the HW-fence poller instead.
        // Resolve first: the fence may have landed between `export_fence` and
        // this call (common on a fast GPU). Without it we would register a
        // waiter for a point already reached and rely entirely on the poller.
        let _ = zcore_drivers::scheme::syncobj::poll_pending();
        let already = zcore_drivers::scheme::syncobj::query(handle)
            .map(|p| p >= point)
            .unwrap_or(false);
        let signaled = Arc::new(AtomicBool::new(already));
        let eventbus = EventBus::new();
        if already {
            eventbus.lock().set(Event::READABLE);
        } else {
            register_waiter(handle, point, signaled.clone(), eventbus.clone());
            // Fence may have landed in the gap between `query` and the insert
            // (same race `syncobj_eventfd::register` closes). Re-check unlocked.
            let _ = zcore_drivers::scheme::syncobj::poll_pending();
            wake_ready_waiters();
        }
        Arc::new(Self {
            base: KObjectBase::new(),
            handle,
            sync_file_point: Some(point),
            signaled,
            eventbus,
            flags: Mutex::new(OpenFlags::RDWR | OpenFlags::CLOEXEC),
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
    pub fn import_opaque(&self, pid: u64) -> Option<u32> {
        if self.sync_file_point.is_some() {
            return None;
        }
        // The importing process holds the new reference: given back when it
        // dies, and its to `DESTROY` (the exporter's reference stays).
        if !zcore_drivers::scheme::syncobj::add_ref_for(pid, self.handle) {
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
    // `query` is asked with `WAITERS` RELEASED, and that is the whole point of
    // the three passes below.
    //
    // `query` resolves the syncobj table, and a point that advances while it
    // does runs the point-advance upcall -- which is
    // `syncobj_eventfd::on_syncobj_signaled`, and its first line calls this
    // very function. Asking under the lock therefore re-entered
    // `WAITERS.lock()` on a cpu that already held it, and a ticket mutex is
    // not re-entrant: the `KERNEL STOP` on real hardware had HOLDER and
    // waiter both at this function's `WAITERS.lock()`, on cpu 8, with two
    // more cpus queued behind it.
    //
    // Nothing is delivered twice: a nested call that retires a waiter first
    // leaves it gone from the pass below, and only a waiter this pass itself
    // removed is fired.
    let probe: Vec<(u32, u64)> = {
        let waiters = WAITERS.lock();
        waiters.iter().map(|w| (w.handle, w.point)).collect()
    };
    // `(handle, point, reached)`. `false` is a syncobj that is gone: the fd
    // can never become ready, so the waiter is dropped without firing (Mesa
    // times out the same way Linux does).
    let mut done: Vec<(u32, u64, bool)> = Vec::new();
    for (handle, point) in probe {
        match zcore_drivers::scheme::syncobj::query(handle) {
            Some(cur) if cur >= point => done.push((handle, point, true)),
            None => done.push((handle, point, false)),
            _ => {}
        }
    }
    if done.is_empty() {
        return;
    }
    let mut fire: Vec<(Arc<AtomicBool>, Arc<Mutex<EventBus>>)> = Vec::new();
    {
        let mut waiters = WAITERS.lock();
        let mut i = 0;
        while i < waiters.len() {
            match done
                .iter()
                .find(|&&(h, p, _)| h == waiters[i].handle && p == waiters[i].point)
            {
                Some(&(_, _, reached)) => {
                    let w = waiters.swap_remove(i);
                    if reached {
                        fire.push((w.signaled, w.eventbus));
                    }
                }
                None => i += 1,
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
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        // Same trap as epoll/perf before `take_settable`: a hardcoded
        // `flags()` plus a no-op `set_flags` made `fcntl(F_SETFL, O_NONBLOCK)`
        // "succeed" while `F_GETFL` never changed.
        self.flags.lock().take_settable(f);
        Ok(())
    }

    // Linux `drm_syncobj_file_fops` / `sync_file_fops` have no `.read`/`.write`
    // — vfs returns `-EINVAL`, not `-ENOSYS`.
    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
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
            // Keep the HW-fence poller armed while we sleep on the EventBus:
            // a pure `wait_for_event` only returns when READABLE is published,
            // and that publish is what the poller (or a later syncobj ioctl)
            // does. If arming was missed, re-arm here and also re-check on a
            // short tick so GLX `sync_wait` cannot hang forever after the
            // GPU has already written the landing zone.
            super::syncobj_eventfd::ensure_hw_fence_poller();
            let bus = self.eventbus.clone();
            let wait = crate::sync::wait_for_event(bus, Event::READABLE);
            let tick = kernel_hal::thread::sleep_until(kernel_hal::timer::deadline_after(
                core::time::Duration::from_millis(1),
            ));
            futures::pin_mut!(wait);
            futures::pin_mut!(tick);
            // Either the EventBus fired (fence published) or the tick expired
            // (re-run `fence_ready` via `poll` above). Errors from the wait
            // (EINTR) must propagate.
            match futures::future::select(wait, tick).await {
                futures::future::Either::Left((Err(e), _)) => return Err(e),
                futures::future::Either::Left((Ok(_), _))
                | futures::future::Either::Right((_, _)) => {}
            }
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

    /// `read`/`write` on a syncobj fd must be `-EINVAL`, not `-ENOSYS`:
    /// Linux syncobj/sync_file fops have no read/write ops.
    #[test]
    fn read_write_on_syncobj_are_einval_not_enosys() {
        use async_std::task::block_on;
        let fd = SyncobjHandle::new(0);
        let mut buf = [0u8; 8];
        assert_eq!(block_on(fd.read(&mut buf)), Err(LxError::EINVAL));
        assert_eq!(fd.write(&[0u8; 8]), Err(LxError::EINVAL));
        assert_eq!(block_on(fd.read_at(0, &mut buf)), Err(LxError::EINVAL));
    }

    /// `fcntl(F_SETFL, O_NONBLOCK)` on a syncobj/sync_file fd used to return
    /// success while `F_GETFL` stayed forever at the hardcoded RDWR|CLOEXEC.
    #[test]
    fn set_flags_turns_a_blocking_syncobj_fd_non_blocking() {
        let fd = SyncobjHandle::new(0);
        assert!(!fd.flags().non_block());
        assert!(fd.flags().close_on_exec());
        let mut f = fd.flags();
        f.set(OpenFlags::NON_BLOCK, true);
        fd.set_flags(f).unwrap();
        assert!(fd.flags().non_block() && fd.flags().close_on_exec());
    }

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
        let imported = fd.import_opaque(0).expect("a live syncobj imports");
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
        let imported = fd.import_opaque(0).expect("a live syncobj imports");
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
        assert!(fd.import_opaque(0).is_none());
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

    /// The GLX/DRI3 `SwapBuffers` path: EXEC attaches a HW fence, Mesa exports
    /// a `SYNC_FD` (`export_fence` → carrier syncobj + `new_sync_file`), and
    /// `sync_wait` polls that fd. The GPU landing the zone must make the fd
    /// POLLIN without any further syncobj ioctl from the waiter — otherwise
    /// glxgears shows one static frame and never prints FPS
    /// (`eglgears_wayland` still works because it uses SYNCOBJ_EVENTFD).
    #[test]
    fn a_glx_exported_sync_file_becomes_pollable_when_its_hw_fence_lands() {
        let _serial = super::super::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        super::super::syncobj_eventfd::init();

        let src = zcore_drivers::scheme::syncobj::create(false);
        let mut zone: u32 = 0;
        let zone_ptr = &mut zone as *mut u32;
        assert!(zcore_drivers::scheme::syncobj::attach_hw_fence(
            src,
            1,
            zone_ptr as usize,
            0,
            1,
            0,
            true,
        ));
        let carrier =
            zcore_drivers::scheme::syncobj::export_fence(src).expect("export SYNC_FD carrier");
        let fd = SyncobjHandle::new_sync_file(carrier, 1);
        assert!(
            !fd.poll(PollEvents::IN).expect("poll").read,
            "fence still in flight at export"
        );
        assert!(
            pending_waiter_count() > 0,
            "the sync_file must be waiting on the in-flight fence"
        );

        // SAFETY: zone is this stack frame; the syncobj layer reads it through
        // the address handed to attach_hw_fence.
        unsafe { zone_ptr.write_volatile(1) };
        // No EventBus publish yet — only what Mesa's blocking poll does:
        // re-enter fence_ready → poll_pending → publish.
        assert!(
            fd.poll(PollEvents::IN).expect("poll after land").read,
            "once the GPU lands the zone, sync_wait(fd) must see POLLIN"
        );
        drop(fd);
        let _ = zcore_drivers::scheme::syncobj::destroy(src);
    }

    /// Mesa often exports the SYNC_FD before the submit that will signal it.
    /// The sync_file waiter must keep the HW poller armed across that gap.
    #[test]
    fn a_sync_file_waiter_keeps_the_poller_armed_before_the_fence_exists() {
        let _serial = super::super::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        super::super::syncobj_eventfd::init();

        let handle = zcore_drivers::scheme::syncobj::create(false);
        assert!(zcore_drivers::scheme::syncobj::add_ref(handle));
        let fd = SyncobjHandle::new_sync_file(handle, 1);
        assert!(pending_waiter_count() > 0);
        assert!(
            !zcore_drivers::scheme::syncobj::has_pending(),
            "nothing submitted yet"
        );
        // Re-arm explicitly (register_waiter already did); the point is that
        // sync_file waiters alone are enough — eventfd-only would stand down.
        super::super::syncobj_eventfd::ensure_hw_fence_poller();
        assert!(
            super::super::syncobj_eventfd::poller_is_armed(),
            "a sync_file waiter with no fence yet must still leave the poller \
             armed, or a later submit is never noticed"
        );
        drop(fd);
        let _ = zcore_drivers::scheme::syncobj::destroy(handle);
    }
}

/// The `KERNEL STOP` this module earned on real hardware: one lock, one cpu,
/// taken twice.
///
/// `wake_ready_waiters` asked `syncobj::query` about each waiter with
/// `WAITERS` held. `query` resolves the syncobj table, and a point that
/// advances there fires the point-advance upcall —
/// `syncobj_eventfd::on_syncobj_signaled`, whose first line calls
/// `wake_ready_waiters`. A ticket mutex is not re-entrant, so the second
/// `WAITERS.lock()` spun on this cpu's own hold with interrupts off, for ever.
///
/// The test asserts the property rather than reproducing the hang: a hanging
/// test is a test nobody can read the failure of. A probe hook, standing in
/// for the real one, reports whether `WAITERS` was free when the upcall ran.
#[cfg(test)]
mod reentrancy_tests {
    use super::*;
    use core::sync::atomic::AtomicU32;
    use zcore_drivers::scheme::syncobj;

    /// How many times the probe hook ran, and whether `WAITERS` was free on
    /// every one of them (it starts true and only ever goes false, so one bad
    /// upcall is enough to fail).
    static UPCALLS: AtomicU32 = AtomicU32::new(0);
    static ALWAYS_FREE: AtomicBool = AtomicBool::new(true);

    /// How long the probe gives the registry before calling it held.
    ///
    /// A single `try_lock` cannot answer the question this test asks. It
    /// fails for a lock held by *anybody*, and on a host build there is no
    /// "this cpu" to ask instead: `HeldByCurrentCpu` is a constant `false`
    /// there. Meanwhile the hardware-fence poller
    /// (`syncobj_eventfd::arm_poller`, armed by this test's own
    /// `new_sync_file`) fires from a timer and takes the registry for a few
    /// instructions -- so a `try_lock` that happened to land inside that
    /// window reported the re-entrancy bug when there was none. That is a
    /// flake, not a finding: red about once in eight runs of the suite, and
    /// nothing in the kernel had changed.
    ///
    /// The two holds differ in *kind*, not in duration, and that is what the
    /// probe below keys on. A re-entrant hold -- the bug this test exists for
    /// -- belongs to the very call stack the upcall runs on, so it cannot be
    /// released until that stack unwinds: it never comes free while the probe
    /// is looking, however long the probe looks. A concurrent hold is over in
    /// the handful of instructions `wake_ready_waiters` keeps the registry
    /// for.
    ///
    /// So the probe *blocks* on another thread instead of spinning on this
    /// one. That matters on the single-vCPU runner: a spin loop here keeps
    /// the cpu away from the concurrent holder, so its own budget ran out
    /// while the holder never got scheduled to release -- the budget was a
    /// bound on the probe's cpu time, not on the hold. A blocking acquire
    /// hands the cpu to the holder and returns the instant it releases, so
    /// this deadline bounds the *hold*. Five seconds against a hold of a
    /// `Vec` collect; the deadline is only ever reached by the bug.
    const PROBE_DEADLINE: core::time::Duration = core::time::Duration::from_secs(5);

    /// Stands in for `syncobj_eventfd::on_syncobj_signaled`, which is what the
    /// kernel really installs, and asks the one question that matters: can the
    /// registry be taken while this upcall runs? The real hook's first line
    /// takes it.
    ///
    /// The acquire happens on a thread of its own, and the hook waits for
    /// word back rather than for the lock, for the reason in
    /// [`PROBE_DEADLINE`]. Under a re-entrant hold that thread is stuck
    /// exactly where the kernel deadlocked -- but it is not *this* thread, so
    /// the test reports the bug instead of hanging on it, and the thread
    /// finishes on its own as soon as the offending stack unwinds.
    fn probe_hook(_handle: u32, _point: u64) {
        extern crate std;
        UPCALLS.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(WAITERS.lock());
            let _ = tx.send(());
        });
        if rx.recv_timeout(PROBE_DEADLINE).is_err() {
            ALWAYS_FREE.store(false, Ordering::SeqCst);
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn the_point_advance_upcall_never_runs_with_the_waiter_registry_held() {
        // The hook is process-wide; serialise with the tests that install the
        // real one.
        let _serial = super::super::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        // `WAITERS` is process-wide and the other tests in this module keep
        // their own sync_files in it, so this one asks after ITS OWN waiter
        // rather than counting the registry: the count was 2 whenever another
        // test had a fence in flight, and the `clear()` that tried to make it
        // 1 dropped that test's waiter along the way. Red in 4 of 6 parallel
        // runs of the suite, green in 5 of 5 with `--test-threads=1`.
        UPCALLS.store(0, Ordering::SeqCst);
        ALWAYS_FREE.store(true, Ordering::SeqCst);
        syncobj::set_signal_hook(probe_hook);

        let handle = syncobj::create(false);
        assert!(syncobj::add_ref(handle), "keep a ref for the test itself");
        // A sync_file on a point still in flight: this is what puts a waiter in
        // the registry, which is what makes `wake_ready_waiters` call `query`.
        let fd = SyncobjHandle::new_sync_file(handle, 1);
        assert!(
            !fd.poll(PollEvents::IN).expect("poll").read,
            "the fence has not landed yet"
        );
        assert!(
            WAITERS.lock().iter().any(|w| w.handle == handle),
            "the sync_file has to be in the registry, which is what makes \
             `wake_ready_waiters` call `query`"
        );

        // The client submits: a hardware fence whose landing zone still reads
        // zero, so nothing is resolved at attach time.
        let mut landing_zone: u32 = 0;
        let zone = &mut landing_zone as *mut u32;
        assert!(syncobj::attach_hw_fence(
            handle,
            1,
            zone as usize,
            0,
            1,
            0,
            false
        ));
        // The GPU writes the payload. Nobody is told; the fence is resolved
        // lazily, inside the very `query` that `wake_ready_waiters` makes.
        // SAFETY: the zone is this frame's own local, and the syncobj layer
        // reads it through the same address it was handed.
        unsafe { zone.write_volatile(1) };

        wake_ready_waiters();

        assert!(
            UPCALLS.load(Ordering::SeqCst) > 0,
            "the landed fence must have advanced the point and fired the upcall, \
             or this test is asserting nothing"
        );
        assert!(
            ALWAYS_FREE.load(Ordering::SeqCst),
            "the point-advance upcall ran while this cpu held WAITERS: the real \
             hook re-takes that lock and a ticket mutex is not re-entrant"
        );
        assert!(
            fd.poll(PollEvents::IN).expect("poll").read,
            "the landed fence has to make the sync_file pollable"
        );
        assert!(
            !WAITERS.lock().iter().any(|w| w.handle == handle),
            "the waiter is retired once its point is reached: one left behind \
             keeps the hardware fence poller running for ever"
        );
        drop(fd);
        let _ = syncobj::destroy(handle);
        // Put the kernel's own hook back for whatever runs next.
        super::super::syncobj_eventfd::init();
    }
}
