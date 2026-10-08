//! DRM (Direct Rendering Manager) Scheme for zCore
//!
//! Exposes the DRM subsystem to userspace via IOCTLs and memory mapping.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::future::Future;
use core::pin::Pin;
#[cfg(test)]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::task::{Context, Poll as TaskPoll};
use core::time::Duration;

use crate::sync::{Event, EventBus};
use lock::Mutex;
use rcore_fs::vfs::*;
use zircon_object::vm::VmObject;

use super::drm;
use super::drm_trail;
use crate::error::{LxError, LxResult};
use zcore_drivers::display::edid;

/// Parks until the DRM card fd has a queued event. Flat `Future` (no nested
/// `async` state machine) so blocking card reads / leftover `async_poll`
/// callers stay thin on the coroutine stack.
#[must_use = "future does nothing unless polled/`await`-ed"]
struct DrmEventWait<'a> {
    dev: &'a DrmDev,
    bus: Arc<Mutex<EventBus>>,
    sub_id: Option<u64>,
}

impl Drop for DrmEventWait<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.sub_id.take() {
            self.bus.lock().unsubscribe(id);
        }
    }
}

impl Future for DrmEventWait<'_> {
    type Output = Result<PollStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> TaskPoll<Self::Output> {
        let this = self.as_mut().get_mut();
        match this.dev.poll() {
            Ok(status) if status.read => {
                if let Some(id) = this.sub_id.take() {
                    this.bus.lock().unsubscribe(id);
                }
                return TaskPoll::Ready(Ok(status));
            }
            Ok(_) => {}
            Err(e) => {
                if let Some(id) = this.sub_id.take() {
                    this.bus.lock().unsubscribe(id);
                }
                return TaskPoll::Ready(Err(e));
            }
        }
        if this.sub_id.is_none() {
            let waker = cx.waker().clone();
            this.sub_id = this.bus.lock().subscribe(Box::new(move |ev| {
                if (ev & Event::READABLE).is_empty() {
                    return false;
                }
                waker.wake_by_ref();
                // `false`, not `true`: returning true makes the callback
                // ONE-SHOT, and the bus drops it the moment it fires while
                // `sub_id` stays `Some` naming nothing. So a waiter that woke
                // on READABLE and then found nothing to read -- another reader
                // of the same `drm_file` (a `dup`ed card fd, or a second
                // compositor thread) drained the queue first, or the flag was
                // set without a completion landing -- fell through to the
                // `is_none()` above, did NOT re-subscribe, and parked with no
                // callback on the bus at all. Nothing here re-polls on a tick,
                // so that reader slept for good: a compositor blocked in
                // `read()` on the card fd, which is the frame loop stopping
                // dead. Staying subscribed for the future's whole life costs a
                // spurious wake at most, and every exit path already
                // unsubscribes -- both `Ready` arms of both matches, and `Drop`.
                false
            }));
        }
        match this.dev.poll() {
            Ok(status) if status.read => {
                if let Some(id) = this.sub_id.take() {
                    this.bus.lock().unsubscribe(id);
                }
                TaskPoll::Ready(Ok(status))
            }
            Ok(_) => TaskPoll::Pending,
            Err(e) => {
                if let Some(id) = this.sub_id.take() {
                    this.bus.lock().unsubscribe(id);
                }
                TaskPoll::Ready(Err(e))
            }
        }
    }
}

/// DRM Device INode
pub struct DrmDev {
    inode_id: usize,
    minor: u32,
    /// Per-open caps + event queue (Linux `drm_file`). Shared across `dup` of
    /// the same fd; fresh on each `open_client`.
    file: Arc<drm::DrmFileState>,
}

impl DrmDev {
    pub fn new(minor: u32) -> Self {
        use rcore_fs_devfs::DevFS;
        Self {
            inode_id: DevFS::new_inode_id(),
            minor,
            // Registry placeholder; real opens go through [`Self::open_client`].
            file: drm::DrmFileState::new(),
        }
    }

    /// The GPU this node names, for the driver-private ioctls.
    ///
    /// Every node used to answer with `get_primary_driver()`, so `card1` was
    /// the compute GPU in its sysfs identity but served its nouveau ioctls
    /// from a different card. The fallback keeps a node with no table entry
    /// working exactly as it did.
    fn driver(&self) -> Option<Arc<dyn zcore_drivers::scheme::DrmScheme>> {
        drm::driver_for_minor(self.minor).or_else(drm::get_primary_driver)
    }

    /// `open(2)` on `/dev/dri/card*` / `renderD*`: a fresh per-fd DRM file
    /// state (ATOMIC_CLIENT + event queue), like Linux's `drm_open_helper`.
    pub fn open_client(&self) -> Arc<dyn INode> {
        Arc::new(self.open_client_dev())
    }

    /// The open itself: the state is scoped to this node, and takes the
    /// node's master when no open holds it (`drm_master_open`).
    fn open_client_dev(&self) -> DrmDev {
        DrmDev {
            inode_id: self.inode_id,
            minor: self.minor,
            file: drm::DrmFileState::for_minor(self.minor),
        }
    }

    pub fn file_state(&self) -> &Arc<drm::DrmFileState> {
        &self.file
    }

    /// This open's identity for what the DRM core keeps per `drm_file`
    /// (property blobs): the address of its state, which lives exactly as
    /// long as the open does.
    fn file_owner(&self) -> usize {
        Arc::as_ptr(&self.file) as usize
    }

    /// `drm_mode_obj_set_property_ioctl`, which `SETPROPERTY` also goes
    /// through: the object, of the type the caller named (ENOENT); a
    /// property that object carries (EINVAL, and an encoder carries none);
    /// then `drm_property_change_valid_get`, which refuses an immutable
    /// property and a value outside what the property's type admits
    /// (EINVAL). Only then is the write applied.
    ///
    /// None of it was read once the object and the property each existed
    /// somewhere: `DPMS` on a CRTC, a plane's immutable `type`, `DPMS = 7`,
    /// `ACTIVE = 2`, an `FB_ID` naming a CRTC, a `MODE_ID` naming no blob,
    /// were all "set", so a client probing what it may change was told
    /// everything, and one reading the value back saw it unchanged.
    ///
    /// The write itself is still the DPMS switch or a no-op: the scanout
    /// has no per-object state behind the other properties, and an atomic
    /// property set this way does not commit (Linux commits it).
    fn set_object_property(
        &self,
        obj_id: u32,
        obj_type: u32,
        prop_id: u32,
        value: u64,
    ) -> Result<usize> {
        let Some((kind, props)) = find_mode_object(obj_id, obj_type, true) else {
            return Err(FsError::EntryNotFound);
        };
        // `drm_mode_obj_find_prop_id`.
        if !props.iter().any(|&(id, _)| id == prop_id) {
            return Err(FsError::InvalidParam);
        }
        let Some(spec) = prop_spec(prop_id) else {
            return Err(FsError::InvalidParam);
        };
        if !property_change_valid(&spec, value) {
            return Err(FsError::InvalidParam);
        }
        if prop_id == PROP_DPMS && kind == DRM_MODE_OBJECT_CONNECTOR {
            let off = value != DRM_MODE_DPMS_ON;
            log::debug!(
                "[drm] SETPROPERTY connector={} DPMS={} -> CRTC {}",
                obj_id,
                value,
                if off { "off" } else { "on" }
            );
            drm::set_crtc_blanked(off);
            return Ok(0);
        }
        log::debug!(
            "[drm] OBJ_SETPROPERTY obj={} type={:#x} prop={} val={} (accepted, no-op)",
            obj_id,
            kind,
            prop_id,
            value
        );
        Ok(0)
    }

    /// Sleep until the vblank a blocking `DRM_IOCTL_WAIT_VBLANK` asked for.
    ///
    /// Called from `sys_ioctl` (async) *before* the request reaches
    /// `io_control` (sync), which then reports the sequence that has by then
    /// genuinely completed. Splitting it this way keeps the whole `FileLike`
    /// stack synchronous while still giving this one ioctl real blocking
    /// semantics.
    ///
    /// It must not spin: an earlier implementation busy-waited the 16.7 ms and
    /// starved every other coroutine on the CPU, which looked like the machine
    /// freezing. The synthetic vblank counter is a pure function of the clock,
    /// so there is an exact deadline to sleep to and no need to poll at all.
    ///
    /// Requests that asked for an event instead of blocking are left alone —
    /// that path already defers correctly through the timer queue.
    pub async fn wait_vblank_sleep(&self, data: usize) -> LxResult<()> {
        // Linux caps this wait at 3 seconds (`DRM_WAIT_ON(..., 3 * HZ, ...)`).
        // Match it: a target far in the future must not park a thread forever,
        // and the sync arm reporting the current sequence after the cap is the
        // same answer Linux's timeout path gives.
        const MAX_WAIT: Duration = Duration::from_secs(3);

        if ucheck(data, core::mem::size_of::<DrmWaitVblank>()).is_err() {
            return Ok(()); // io_control will reject it with EFAULT in a moment
        }
        let req = unsafe { *(data as *const DrmWaitVblank) };
        if wait_vblank_check(req.typ).is_err() {
            return Ok(()); // the sync arm refuses it at once; nothing to sleep for
        }
        if req.typ & _DRM_VBLANK_EVENT != 0 {
            return Ok(()); // event form: delivered by the timer queue, never blocks
        }
        const _DRM_VBLANK_RELATIVE: u32 = 0x1;
        const _DRM_VBLANK_NEXTONMISS: u32 = 0x1000_0000;
        // Resolve the target exactly as the sync arm does, or the two would
        // disagree about which vblank was asked for.
        let now_seq = drm::vblank_seq_now();
        let mut target = if req.typ & _DRM_VBLANK_RELATIVE != 0 {
            now_seq.wrapping_add(req.sequence)
        } else {
            req.sequence
        };
        if (target.wrapping_sub(now_seq) as i32) <= 0 {
            if req.typ & _DRM_VBLANK_NEXTONMISS == 0 {
                return Ok(()); // already reached: nothing to wait for
            }
            target = now_seq.wrapping_add(1);
        }
        let cap = kernel_hal::timer::timer_now() + MAX_WAIT;
        // Re-check after sleeping instead of trusting one deadline. A wake-up
        // that lands even a nanosecond before the lattice boundary leaves the
        // counter one short, and the sync arm would then report `target - 1`;
        // the caller's next relative request resolves to a vblank that has
        // just passed, returns instantly, and the frame after it waits a full
        // period. That alternation is not a slow kernel -- it is measured as
        // jitter, and it is exactly what a client pacing on this ioctl would
        // see as stutter. The loop is bounded by the same 3 s cap.
        let mut acct = PreWaitAccount::new(zcore_drivers::scheme::prewait::Kind::WaitVblank);
        while (target.wrapping_sub(drm::vblank_seq_now()) as i32) > 0 {
            let Some(deadline) = drm::vblank_deadline_for_seq(target) else {
                break;
            };
            acct.probe();
            if deadline >= cap {
                crate::process::interruptible(kernel_hal::thread::sleep_until(cap)).await?;
                // Stopped by the 3 s cap, not by the vblank asked for.
                acct.timed_out();
                break;
            }
            crate::process::interruptible(kernel_hal::thread::sleep_until(deadline)).await?;
        }
        Ok(())
    }

    /// Sleep until a blocking `SYNCOBJ_WAIT` / `TIMELINE_WAIT` would succeed
    /// (or its absolute deadline passes).
    ///
    /// Same split as [`wait_vblank_sleep`]: `io_control` is sync and used to
    /// spin-poll the whole timeout, pegging a core. Here we are in the async
    /// syscall path, so we probe with
    /// [`zcore_drivers::scheme::syncobj::wait_ready`] -- which resolves the
    /// pending hardware fences itself, under the table lock it takes anyway
    /// -- and back off between probes ([`fence_poll_wait`]). The sync arm
    /// then finishes the ioctl (usually on the first iteration).
    pub async fn syncobj_wait_sleep(&self, cmd: u32, data: usize) -> LxResult<()> {
        if !zcore_drivers::display::nouveau_uapi_enabled() {
            return Ok(());
        }
        // A request the sync arm will refuse, or answer without waiting, is
        // not slept on. `data` is the caller's own pointer here, so the struct
        // is checked before it is read (`read_syncobj_wait` does not).
        if ucheck(data, syncobj_wait_prefix(cmd)).is_err() {
            return Ok(());
        }
        let Ok(Some(SyncobjWaitReq {
            handles,
            points,
            timeout_nsec,
            flags,
            ..
        })) = read_syncobj_wait(cmd, data)
        else {
            return Ok(());
        };
        let deadline_us = (timeout_nsec.max(0) as u64) / 1000;
        let wait_all = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0;
        let available_only = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0;
        let ready_fn = if available_only {
            zcore_drivers::scheme::syncobj::wait_available_ready
        } else {
            zcore_drivers::scheme::syncobj::wait_ready
        };
        let deadline = core::time::Duration::from_micros(deadline_us);
        let mut acct = PreWaitAccount::new(zcore_drivers::scheme::prewait::Kind::SyncobjWait);
        loop {
            // No `poll_pending()` in front of this. It takes the syncobj
            // table lock and runs `resolve_locked` -- and so does
            // `wait_ready` itself, on the very next line, under the same
            // lock. Every look of this loop therefore walked the pending
            // list TWICE, and a walk reads the landing zone of every fence
            // still in flight out of uncached pinned sysmem. A parked
            // client takes several looks a frame, so that was several
            // wasted trips off the CPU per frame, plus a second round on
            // the one lock the signalling side needs to end the wait.
            match ready_fn(&handles, points.as_deref(), wait_all, deadline_us) {
                Some(answer) => {
                    // `wait_ready` answers Timeout only past `deadline_us`,
                    // so this is the same "gave up on the clock" the poll
                    // helper reports below, reached one look earlier.
                    if matches!(
                        answer,
                        Err(zcore_drivers::scheme::syncobj::WaitOutcome::Timeout)
                    ) {
                        acct.timed_out();
                    }
                    return Ok(());
                }
                None => {
                    // If the absolute deadline is already behind `timer_now`,
                    // wait_ready should have returned Timeout; the helper still
                    // refuses to sleep past it if the clocks disagree slightly.
                    if !fence_poll_wait(acct.probes, deadline).await? {
                        acct.timed_out();
                        return Ok(());
                    }
                    acct.probe();
                }
            }
        }
    }

    /// Wait for a commit's `IN_FENCE_FD` before the sync arm presents.
    ///
    /// An explicit-sync client hands the plane a fence meaning "my rendering
    /// into this buffer has landed". We accepted that property and then
    /// scanned the buffer out regardless, so a commit that arrived before the
    /// GPU finished put a half-drawn frame on screen -- the tearing and torn
    /// rectangles a compositor is using explicit sync precisely to avoid.
    ///
    /// Same split as [`Self::syncobj_wait_sleep`], and for the same reason:
    /// `io_control` is synchronous and `syncobj::wait` spin-polls, so waiting
    /// there would peg a core for the whole timeout. Here we are in the async
    /// syscall path and can really sleep.
    ///
    /// Every early return leaves the commit behaving exactly as before this
    /// existed: a malformed request, an fd that is not a fence, or a TEST_ONLY
    /// probe simply does not wait, and the sync arm reports on it as usual.
    pub async fn atomic_in_fence_sleep(&self, data: usize) -> LxResult<()> {
        let Some((handle, point)) = self.atomic_in_fence(data) else {
            return Ok(());
        };
        let now = kernel_hal::timer::timer_now();
        let deadline_us = now.as_micros() as u64 + PRESENT_FENCE_TIMEOUT_US;
        let handles = [handle];
        let points = [point];
        let deadline = core::time::Duration::from_micros(deadline_us);
        let mut acct = PreWaitAccount::new(zcore_drivers::scheme::prewait::Kind::AtomicInFence);
        loop {
            // Same as in [`Self::syncobj_wait_sleep`]: `wait_ready` resolves
            // the pending fences itself, so a `poll_pending()` in front of it
            // is a second walk of the same list under a second acquisition of
            // the same lock.
            match zcore_drivers::scheme::syncobj::wait_ready(
                &handles,
                Some(&points),
                true,
                deadline_us,
            ) {
                Some(Ok(_)) => return Ok(()),
                Some(Err(outcome)) => {
                    if matches!(
                        outcome,
                        zcore_drivers::scheme::syncobj::WaitOutcome::Timeout
                    ) {
                        // Budgeted, not per-frame. A fence that never signals
                        // times out on EVERY commit, and klog writes
                        // synchronously to the UART -- an uncapped warning here
                        // would be one line per frame forever, which is how
                        // input has been starved on this kernel before.
                        static TIMEOUT_REPORTS: AtomicU32 = AtomicU32::new(0);
                        const MAX_TIMEOUT_REPORTS: u32 = 8;
                        let n = TIMEOUT_REPORTS.fetch_add(1, Ordering::Relaxed);
                        if n < MAX_TIMEOUT_REPORTS {
                            log::warn!(
                                "[drm] ATOMIC IN_FENCE_FD: syncobj handle={} point={} did not \
                                 signal within {} us; presenting anyway (frame may tear){}",
                                handle,
                                point,
                                PRESENT_FENCE_TIMEOUT_US,
                                if n + 1 == MAX_TIMEOUT_REPORTS {
                                    " -- further in-fence timeouts will not be reported"
                                } else {
                                    ""
                                }
                            );
                        }
                    }
                    return Ok(());
                }
                None => {
                    if !fence_poll_wait(acct.probes, deadline).await? {
                        acct.timed_out();
                        return Ok(());
                    }
                    acct.probe();
                }
            }
        }
    }

    /// Wait for the GPU before the synchronous arm serves a blocking
    /// `GEM_CPU_PREP`.
    ///
    /// That ioctl is the one Mesa calls whenever it recycles a buffer or
    /// reads one back, and it blocks by contract. `INode::io_control` is
    /// synchronous, so the driver's arm could only busy-wait: `cpu_prep_wait`
    /// spins on `gpu_spin()` for however long the GPU takes, pegging a core
    /// and starving every other coroutine on it -- on a two-CPU desktop that
    /// is the compositor itself being held off by its own client's buffer
    /// recycle. Same split, and the same reason, as
    /// [`Self::syncobj_wait_sleep`] and [`Self::atomic_in_fence_sleep`]: here
    /// we are in the async syscall path and can really sleep, and the sync arm
    /// then finds the work already done and returns without spinning.
    ///
    /// Every early return leaves the ioctl behaving exactly as before this
    /// existed -- the sync arm still does the whole wait, spin and all. A
    /// `NOWAIT` prep is never slept on: it answers EBUSY instead of blocking,
    /// so a sleep ahead of it would turn the one flag that promises not to
    /// block into a ten-second stall.
    pub async fn cpu_prep_sleep(&self, cmd: u32, data: usize) -> LxResult<()> {
        if !zcore_drivers::display::nouveau_uapi_enabled() {
            return Ok(());
        }
        if !zcore_drivers::display::is_cpu_prep_ioctl(cmd) {
            return Ok(());
        }
        // `data` is the caller's own pointer, checked before it is read: the
        // driver's arm reads it unchecked, but that runs after `io_control`
        // has vetted the fd, and this runs on anything userspace hands us.
        if ucheck(data, zcore_drivers::display::CPU_PREP_REQUEST_BYTES).is_err() {
            return Ok(());
        }
        // SAFETY: `is_cpu_prep_ioctl` accepted the size, and `ucheck` just
        // said those bytes are readable by this process.
        let (handle, flags) = unsafe { zcore_drivers::display::cpu_prep_request(data) };
        if zcore_drivers::display::cpu_prep_is_nowait(flags) {
            return Ok(());
        }
        let fences = drm::cpu_prep_fences(handle, drm::current_pid());
        if fences.is_empty() {
            return Ok(());
        }
        // The driver's own bound, so a sleep here can never outlast the wait
        // it stands in for: the sync arm answers EBUSY past it either way.
        let deadline =
            kernel_hal::timer::timer_now() + core::time::Duration::from_micros(CPU_PREP_TIMEOUT_US);
        let mut acct = PreWaitAccount::new(zcore_drivers::scheme::prewait::Kind::CpuPrep);
        loop {
            // `hw_fences_landed`, not `fences.iter().all(hw_fence_landed)`:
            // one read per landing zone, not one per fence. A buffer two
            // submits of one ring wrote carries two fences at the SAME
            // semaphore word, and this loop takes the list again on every
            // probe.
            if zcore_drivers::scheme::syncobj::hw_fences_landed(&fences) {
                return Ok(());
            }
            if !fence_poll_wait(acct.probes, deadline).await? {
                acct.timed_out();
                return Ok(());
            }
            acct.probe();
        }
    }

    /// Wait for the GPU to finish writing the buffer a **legacy** present is
    /// about to scan out.
    ///
    /// The implicit-sync half of [`Self::atomic_in_fence_sleep`], and the half
    /// that this kernel actually runs: `drm.atomic` is opt-in, so wlroots
    /// drives `SETCRTC` and `PAGE_FLIP`, and neither carries a fence. Linux
    /// gets the same guarantee from the framebuffer's reservation object --
    /// `drm_atomic_helper_prepare_planes` collects the fences sitting on the
    /// BO and the commit waits for them before the flip is programmed. Here
    /// nothing was waited for at all: `present_now_checked` read the buffer
    /// the instant the ioctl arrived, so a compositor that submitted its
    /// rendering and flipped without a round trip had whatever the GPU had
    /// finished so far put on screen. That is the *same* class of defect the
    /// atomic in-fence wait was written for, on the path with all the mileage.
    ///
    /// What is waited on comes from the driver ([`drm::scanout_render_fence`]):
    /// the fence that says the ring belonging to the buffer's owner has
    /// drained. It is conservative in the safe direction -- it can wait for
    /// work that never touched this buffer, and cannot miss work that did.
    ///
    /// Same async split and same failure policy as the atomic wait: every
    /// early return leaves the present exactly as it behaved before this
    /// existed, and a fence that misses the bound is reported and presented
    /// anyway -- a torn frame is a glitch, a frame that never comes is a hang.
    pub async fn present_fence_sleep(&self, cmd: u32, data: usize) -> LxResult<()> {
        // The only fences that exist here are the nouveau-uAPI ring's, so with
        // that surface off there is nothing to ask about -- and this runs on
        // every flip of the software desktop too, which must stay untouched.
        if !zcore_drivers::display::nouveau_uapi_enabled() {
            return Ok(());
        }
        let Some(kind) = legacy_present_kind(cmd) else {
            return Ok(());
        };
        // Read-only, and anything malformed simply does not wait: the sync arm
        // is about to reject it with EFAULT/EINVAL on its own.
        let fb_id = match kind {
            LegacyPresent::PageFlip => {
                if ucheck(data, core::mem::size_of::<DrmModeCrtcPageFlip>()).is_err() {
                    return Ok(());
                }
                unsafe { (*(data as *const DrmModeCrtcPageFlip)).fb_id }
            }
            LegacyPresent::SetCrtc => {
                if ucheck(data, core::mem::size_of::<DrmModeGetCrtc>()).is_err() {
                    return Ok(());
                }
                unsafe { (*(data as *const DrmModeGetCrtc)).fb_id }
            }
        };
        // A `SETCRTC` with a null fb turns the pipe off. It presents nothing,
        // so there is nothing to wait for.
        if fb_id == 0 {
            return Ok(());
        }
        let fences = drm::scanout_render_fence(fb_id);
        let n = FENCE_PRESENTS
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        // `flip_fence_enabled` is the same gate `scanout_render_fence` applies
        // internally, and it is folded into the REPORT rather than into a return
        // of its own: with the hatch off the caller asked for the old behaviour,
        // an empty answer then says nothing about the driver, and a second early
        // return on the present path is a way to break presenting that no test
        // here can reach (this half needs a live device -- see the module docs of
        // `present_fence_tests`).
        let report = fence_report_decision(n, FENCE_REPORT_EVERY) && drm::flip_fence_enabled();
        // An empty answer is the interesting one, and nothing said it out loud
        // until now. The wait is on by default and the atomic path has had its
        // in-fence for as long as it has existed, so the natural reading of a
        // torn frame on real hardware is "the wait did not help". It may never
        // have run: no fence means this present goes straight to the blit, and a
        // GPU still writing the buffer is not waited for at all. Whether that is
        // what happens on a real desktop is a question about a real machine, so
        // the number goes in the log.
        if fences.is_empty() {
            if report {
                log::info!(
                    "[drm] present fence: fb {} (#{}) has NO render fence to wait on -- \
                     the buffer's owner has no ring in flight, or its ring is unknown; \
                     presenting immediately, so a GPU still writing this buffer is not \
                     waited for",
                    fb_id,
                    n
                );
            }
            return Ok(());
        }
        let waited_from = kernel_hal::timer::timer_now();
        let deadline = waited_from + core::time::Duration::from_micros(PRESENT_FENCE_TIMEOUT_US);
        let mut acct = PreWaitAccount::new(zcore_drivers::scheme::prewait::Kind::PresentFence);
        loop {
            // `hw_fences_landed`, not `fences.iter().all(hw_fence_landed)`:
            // one read per landing zone, not one per fence. A buffer two
            // submits of one ring wrote carries two fences at the SAME
            // semaphore word, and this loop takes the list again on every
            // probe.
            if zcore_drivers::scheme::syncobj::hw_fences_landed(&fences) {
                if report {
                    log::info!(
                        "[drm] present fence: fb {} (#{}) waited {}us for {} fence(s) to land",
                        fb_id,
                        n,
                        kernel_hal::timer::timer_now()
                            .saturating_sub(waited_from)
                            .as_micros(),
                        fences.len()
                    );
                }
                return Ok(());
            }
            if !fence_poll_wait(acct.probes, deadline).await? {
                acct.timed_out();
                // Budgeted for the same reason as the atomic wait: a ring that
                // stopped landing fences misses this bound on EVERY frame, and
                // klog writes synchronously to the UART.
                static TIMEOUT_REPORTS: AtomicU32 = AtomicU32::new(0);
                const MAX_TIMEOUT_REPORTS: u32 = 8;
                let n = TIMEOUT_REPORTS.fetch_add(1, Ordering::Relaxed);
                if n < MAX_TIMEOUT_REPORTS {
                    log::warn!(
                        "[drm] present fence: fb {} ({} fence(s)) did not land within {} us; \
                         presenting anyway (frame may tear){}",
                        fb_id,
                        fences.len(),
                        PRESENT_FENCE_TIMEOUT_US,
                        if n + 1 == MAX_TIMEOUT_REPORTS {
                            " -- further present-fence timeouts will not be reported"
                        } else {
                            ""
                        }
                    );
                }
                return Ok(());
            }
            acct.probe();
        }
    }

    /// The fence a pending atomic commit is waiting on, as `(syncobj handle,
    /// point)`, or `None` when there is nothing to wait for.
    ///
    /// Re-walks the request's property arrays looking for `IN_FENCE_FD`. That
    /// duplicates the sync arm's walk, but this runs before it and must not
    /// disturb it: every check here is read-only and every failure is a
    /// `None` that simply skips the wait.
    fn atomic_in_fence(&self, data: usize) -> Option<(u32, u64)> {
        use crate::fs::{FileDesc, SyncobjHandle};
        use crate::process::ProcessExt;
        use zircon_object::task::Thread;

        if !self.file.atomic_client() {
            return None;
        }
        if ucheck(data, core::mem::size_of::<DrmModeAtomic>()).is_err() {
            return None;
        }
        let req = unsafe { *(data as *const DrmModeAtomic) };
        // A TEST_ONLY probe presents nothing, so it has nothing to wait for --
        // and wlroots test-commits its swapchain constantly.
        if req.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
            return None;
        }
        let mut fd: Option<i32> = None;
        walk_atomic_props(&req, |_obj_id, prop_id, value| {
            fold_in_fence(&mut fd, prop_id, value);
            Ok(())
        })
        .ok()?;
        let fd = fd?;
        let thread = kernel_hal::thread::get_current_thread()?
            .downcast::<Thread>()
            .ok()?;
        let linux = thread.proc().try_linux()?;
        let file = linux.get_file_like(FileDesc::from(fd as usize)).ok()?;
        let sync = file.downcast_ref::<SyncobjHandle>()?;
        // A sync_file fd names one fence ("handle reaches point"); a plain
        // syncobj fd names whatever its object currently carries.
        let point = match sync.sync_file_point {
            Some(p) => p,
            None => zcore_drivers::scheme::syncobj::export_snapshot(sync.handle)?,
        };
        Some((sync.handle, point))
    }

    /// Returns the [`VmObject`] representing the file with given `offset` and `len`.
    pub fn get_vmo(&self, offset: usize, len: usize) -> Result<Arc<VmObject>> {
        // MAP_DUMB handed userspace a page-aligned fake mmap offset that encodes
        // the GEM handle in its upper bits (`handle << PAGE_SHIFT`). musl's
        // `mmap()` rejects a non-page-aligned offset with EINVAL *before* the
        // syscall, so the cookie must be page-aligned; recover the handle by
        // shifting it back down.
        let handle_id = handle_from_mmap_cookie(offset);
        if let Some(vmo) = drm::handle_vmo(handle_id) {
            // The dumb buffer's OWN (contiguous, cached) VMO: the mapping keeps
            // the frames alive past DESTROY_DUMB, and the pixels are WB for the
            // renderer -- see `drm::handle_vmo`.
            mmap_len_check(len, vmo.len())?;
            Ok(vmo)
        } else if let Some((phys_addr, size)) =
            zcore_drivers::scheme::gem_mmap::lookup_for(handle_id, drm::current_pid())
        {
            // Driver-private GEM object (currently: nouveau-uAPI GEM_NEW) --
            // same fake-offset space, different table (see
            // drivers/src/scheme/gem_mmap.rs's module doc for why this
            // driver-owned state can't live in `drm::get_handle`'s table).
            // Share ONE physical VMO per handle so GEM_CLOSE cannot free the
            // frames while an older mmap Arc is still live (see `nouveau_cpu_vmo`).
            // Always size the VMO to the full GEM; the caller's `len` only
            // bounds the mapping, not the shared object.
            //
            // `lookup_for`, not `lookup`: the mmap offset is `handle << 12`, a
            // pure function of the handle, so an unchecked lookup let any
            // process map any driver-private GEM object in the system by
            // naming an offset. Linux resolves the offset through the device's
            // `vma_offset_manager` and then checks `drm_vma_node_is_allowed`,
            // whose allow-list is populated when a handle is created -- so a
            // file that never got a handle to the object gets EACCES. The
            // dumb-buffer branch above is already owner-checked inside
            // `handle_vmo`; this was the remaining door.
            mmap_len_check(len, size as usize)?;
            Ok(drm::nouveau_cpu_vmo(handle_id, phys_addr, size as usize))
        } else {
            Err(FsError::InvalidParam)
        }
    }
    /// One DRM ioctl, dispatched on the CANONICAL command -- the encoding
    /// whose `_IOC_SIZE` matches the struct layout the arms below parse.
    /// Callers reach it through [`INode::io_control`], which reconciles the
    /// size the client encoded with the size we parse, exactly as Linux's
    /// `drm_ioctl()` does.
    #[allow(unsafe_code)]
    fn drm_ioctl_dispatch(&self, cmd: u32, data: usize) -> Result<usize> {
        let r = self.drm_ioctl_dispatch_inner(cmd, data);
        // Every REFUSAL of a KMS query, on its own klog channel.
        //
        // Mesa's `wsi_get_connectors()` is the first thing all three
        // `VK_KHR_display` entry points call, and it turns ANY failure of
        // `drmModeGetResources` / `drmModeGetConnector` into
        // `VK_ERROR_OUT_OF_HOST_MEMORY` -- which is the error `vulkaninfo`
        // dies with on real RTX hardware. So "which query refused, on which
        // object, with which errno" is the whole diagnosis, and it has to
        // survive a boot: the per-arm traces logged it on the SAME 48-line
        // budget as the success trace above, and a two-GPU probe (three entry
        // points, each a count pass and a fill pass, every connector id in
        // each) spends that budget on successes long before it reaches the
        // call that fails. The line that explains the error never printed.
        //
        // This channel is therefore separate, and deduped by (nr, object id)
        // rather than merely counted, so libdrm's `goto retry` loop cannot
        // storm the console with one repeated refusal either. It also covers
        // the two paths that had NO trace at all: `GETRESOURCES` /
        // `GETPLANERESOURCES` (no object id to key an arm on), and the
        // render-node `DRM_RENDER_ALLOW` rejection above -- a client whose
        // display fd landed on `renderD128` gets EACCES from GETRESOURCES
        // and the identical OUT_OF_HOST_MEMORY, with nothing in dmesg.
        if let Err(e) = &r {
            if let Some(name) = wsi_query_name(cmd) {
                let id = wsi_query_object_id(cmd, data);
                if wsi_fail_take(cmd & 0xff, id) {
                    kernel_hal::klog_info!(
                        "[drm-wsi] REFUSED {} id={} pid={} minor={} -> {:?} \
                         (Mesa turns this into VK_ERROR_OUT_OF_HOST_MEMORY)",
                        name,
                        id,
                        drm::current_pid(),
                        self.minor,
                        e
                    );
                }
            }
        }
        r
    }

    /// The body of [`DrmDev::drm_ioctl_dispatch`]; see its wrapper for the
    /// refusal trace that wraps it.
    #[allow(unsafe_code)]
    fn drm_ioctl_dispatch_inner(&self, cmd: u32, data: usize) -> Result<usize> {
        // NOTE `data` is NOT necessarily a user address here: [`drm_ioctl`] hands
        // this a kernel bounce buffer whenever it had to reconcile a struct size,
        // exactly as `drm_ioctl_kernel()` hands the handler `kdata`. The
        // `access_ok()` for the argument therefore lives in that wrapper, over
        // the range the CLIENT gave, and every arm below may assume `data` is
        // `_IOC_SIZE(cmd)` readable/writable bytes. Nested pointers inside the
        // struct are still the arm's own to check -- they always point at user
        // memory, whichever buffer the struct itself lives in.
        // Render nodes only accept DRM_RENDER_ALLOW ioctls (drm-uapi.rst
        // "Render nodes"): modeset, dumb-buffer and master/auth commands get
        // EACCES exactly like Linux, so a client probing `renderD128` sees a
        // render node, not a second KMS device. The desktop pins KMS to
        // `card0` via `WLR_DRM_DEVICES`, so enforcing here no longer takes
        // the software-GL path down with a second open KMS node.
        if self.minor >= 128 && !render_allowed(cmd) {
            return Err(FsError::NoPermission);
        }
        // KMS-query trace for Vulkan's VK_KHR_display probe (wsi_display).
        // `vulkaninfo` dies with ERROR_OUT_OF_HOST_MEMORY inside
        // vkGetPhysicalDeviceDisplayPlanePropertiesKHR on RTX, and Mesa
        // returns that code when one of these six GET calls fails (NULL from
        // libdrm) -- but every failure arm below logs at log::warn/debug,
        // INVISIBLE under LOG=error. Name each call and its caller pid via
        // klog (level-filter-free) so one dmesg photo shows the exact query
        // sequence and which one refused. Bounded noise: these six only fire
        // at client startup / probe time, never per frame.
        {
            if let Some(name) = wsi_query_name(cmd) {
                if wsi_trace_take() {
                    kernel_hal::klog_info!(
                        "[drm-wsi] pid={} {} (minor={})",
                        drm::current_pid(),
                        name,
                        self.minor
                    );
                }
            }
        }
        if (cmd & 0xff) == DRM_ECLIPSE_COMPUTE_NR {
            // Matched by NR so the `_IOWR` size encoding need not agree bit for
            // bit -- which means the size the CLIENT chose is the only thing the
            // argument check above saw. `eclipse_compute_ioctl` writes the whole
            // 536-byte struct (`status`, `elapsed_ns`, `grid_threads` and a
            // 512-byte `summary`), so a caller that encodes a smaller one passes
            // the check and then has half a kilobyte written past the end of its
            // buffer. Linux never has this hazard: `ksize` there is the KERNEL's
            // struct size and the buffer is kernel-allocated.
            if (((cmd >> 16) & 0x3fff) as usize) < core::mem::size_of::<DrmEclipseCompute>() {
                return Err(FsError::InvalidParam);
            }
            return eclipse_compute_ioctl(self.minor, data);
        }
        match cmd {
            DRM_IOCTL_VERSION => {
                let node = drm_node_name(self.minor);
                let driver_id =
                    drm::node_driver_id(self.minor, zcore_drivers::display::nouveau_uapi_enabled());
                let compute_node = driver_id == drm::NodeDriverId::Compute;
                // When the nouveau experiment is on, make this visible at the
                // default `warn` level and name the primary DRM driver: it tells
                // us in one boot whether NVK/Mesa even reached VERSION on this
                // node (discovery/enumeration OK) and whether the driver behind
                // it is the real NvidiaGpu ("Nvidia GPU") or a software fallback
                // (which would route every nouveau ioctl to the wrong driver).
                if zcore_drivers::display::nouveau_uapi_enabled() {
                    // klog_info!, not log::warn!: hardware boots default to
                    // LOG=error. This one line tells us in a quiet boot whether
                    // NVK/Mesa even reached VERSION on the node (discovery OK) and
                    // whether the driver behind it is the real NvidiaGpu.
                    //
                    // ONCE PER MINOR. `klog_info!` has no level filter, no rate
                    // limit and writes straight to the UART, and VERSION is on a
                    // path whose rate userspace controls: libdrm calls it on
                    // every `drmGetVersion`, so every `drmGetDevices2` scan, every
                    // Vulkan/EGL probe and every wlroots backend retry emits one.
                    // A desktop that retries in a loop turns that into hundreds
                    // of synchronous serial writes -- the same unthrottled-klog
                    // defect already fixed for the render_allowed line above. The
                    // answer it carries (did a client reach VERSION, and which
                    // driver is behind the node) is the same every time, so the
                    // first line per node is the whole diagnostic.
                    static VERSION_LOGGED: [AtomicBool; 256] =
                        [const { AtomicBool::new(false) }; 256];
                    let slot = (self.minor & 0xff) as usize;
                    if !VERSION_LOGGED[slot].swap(true, Ordering::Relaxed) {
                        let vname = if compute_node {
                            "eclipse-compute"
                        } else {
                            "nouveau"
                        };
                        match self.driver() {
                            Some(d) => kernel_hal::klog_info!(
                                "[drm] VERSION on /dev/dri/{} (minor={}) -> name=\"{}\"; driver={:?} pci_bdf={:x?} (client reached VERSION — DRM discovery OK; logged once per node)",
                                node,
                                self.minor,
                                vname,
                                d.name(),
                                d.pci_bdf()
                            ),
                            None => kernel_hal::klog_info!(
                                "[drm] VERSION on /dev/dri/{} (minor={}) -> name=\"{}\"; driver=<none> (logged once per node)",
                                node,
                                self.minor,
                                vname
                            ),
                        }
                    }
                } else {
                    log::debug!(
                        "[drm] VERSION — /dev/dri/{} opened by userspace (minor={})",
                        node,
                        self.minor
                    );
                }
                // Mesa selects the userspace DRI driver from this NAME. With the
                // nouveau uAPI enabled (the RTX experiment: `nvidia.nouveau_uapi`,
                // only ever set alongside a real NVIDIA card — QEMU's virtio-gpu
                // path never sets it, so it keeps the "zcore" identity), report
                // "nouveau" so Mesa loads nouveau_dri.so, and version 1.4.0 —
                // the drm_nouveau version that gates the NEW submission uAPI
                // (VM_INIT/VM_BIND/EXEC), which is what this driver implements
                // (NOT the legacy GEM_PUSHBUF path, absent here). If Mesa's GL
                // winsys still reaches for GEM_PUSHBUF, the boot log's
                // "[nouveau-uapi] unhandled ioctl ... GEM_PUSHBUF" line says so.
                //
                // Which card a node is comes from `node_driver_id`, not from
                // the flag alone: a SECOND NVIDIA card that is up gets the
                // nouveau identity too, so NVK enumerates both.
                let v = unsafe { &mut *(data as *mut DrmVersion) };
                let (major, minor, patchlevel) = driver_version(driver_id);
                v.version_major = major;
                v.version_minor = minor;
                v.version_patchlevel = patchlevel;

                // `drm_version` copies each string with `drm_copy_field`:
                // `strlen` bytes, no terminator, cut to the room the caller
                // gave, and the length written back is `strlen` whatever the
                // room was. These carried the NUL in both the copy and the
                // length, so `drmGetVersion` reported "nouveau" as 8 bytes.
                let (name, desc): (&[u8], &[u8]) = match driver_id {
                    drm::NodeDriverId::Compute => (b"eclipse-compute", b"Eclipse NVIDIA compute"),
                    drm::NodeDriverId::Nouveau => (b"nouveau", b"nouveau"),
                    drm::NodeDriverId::Software => (b"zcore", b"zCore DRM Driver"),
                };
                let date = b"20260503";
                drm_copy_field(v.name, &mut v.name_len, name)?;
                drm_copy_field(v.date, &mut v.date_len, date)?;
                drm_copy_field(v.desc, &mut v.desc_len, desc)?;
                Ok(0)
            }
            DRM_IOCTL_GET_UNIQUE => {
                let u = unsafe { &mut *(data as *mut DrmUnique) };
                // `drm_getunique`: the bus id is copied only when the whole
                // of it fits (`unique_len >= master->unique_len`), and the
                // length written back is `strlen`, no terminator. This
                // copied a prefix into a short buffer and counted the NUL.
                let unique = b"zcore-gpu";
                if u.unique_len >= unique.len() && !u.unique.is_null() {
                    ucheck(u.unique as usize, unique.len())?;
                    unsafe {
                        core::ptr::copy_nonoverlapping(unique.as_ptr(), u.unique, unique.len());
                    }
                }
                u.unique_len = unique.len();
                Ok(0)
            }
            DRM_IOCTL_GET_CAP => {
                let cap = unsafe { &mut *(data as *mut DrmGetCap) };
                // `drm_getcap` zeroes the answer first, so a refused probe
                // never hands back whatever the client left in `value`.
                cap.value = 0;
                match cap.capability {
                    0x1 => cap.value = 1, // DRM_CAP_DUMB_BUFFER
                    // DRM_CAP_VBLANK_HIGH_CRTC: WAIT_VBLANK reads the CRTC
                    // index from the HIGH_CRTC field (see `wait_vblank_check`),
                    // which is what this cap promises; Linux answers 1 for
                    // every KMS driver.
                    0x2 => cap.value = 1,
                    // DRM_CAP_DUMB_PREFERRED_DEPTH: XRGB8888 scanout = 24.
                    0x3 => cap.value = 24,
                    // DRM_CAP_DUMB_PREFER_SHADOW: the dumb buffer lives behind
                    // a CPU blit over PCIe — clients should render to a shadow
                    // and copy, exactly what this cap advises.
                    0x4 => cap.value = 1,
                    // DRM_CAP_PRIME: IMPORT|EXPORT. wlroots' check_drm_features
                    // *requires* DRM_PRIME_CAP_IMPORT or the whole DRM backend
                    // fails ("PRIME import not supported") — it is mandatory for
                    // any output (pixman or GL), not just GBM clients. We now
                    // implement PRIME_HANDLE_TO_FD / FD_TO_HANDLE (dma-buf), so
                    // advertise it.
                    0x5 => cap.value = 3,
                    0x6 => cap.value = 1,  // DRM_CAP_TIMESTAMP_MONOTONIC
                    0x8 => cap.value = 64, // DRM_CAP_CURSOR_WIDTH
                    0x9 => cap.value = 64, // DRM_CAP_CURSOR_HEIGHT
                    // DRM_CAP_ADDFB2_MODIFIERS: no modifier support by
                    // default. The reason recorded here used to be VM_BIND's
                    // refusal of non-zero PTE kinds, and that is no longer
                    // true -- VM_BIND programs the Turing kinds (0x00..0x06)
                    // verbatim and hands the compressible ones to the RM's
                    // HAL. What still stands is the simpler half: the present
                    // is a copy that reads the framebuffer LINEARLY, so
                    // DRM_FORMAT_MOD_LINEAR is the only layout it can put on
                    // the panel, and advertising more is a promise it cannot
                    // keep -- wlroots would negotiate a block-linear
                    // swapchain with NVK and the desktop would come up as
                    // garbage. (The Vulkan renderer's
                    // VK_EXT_image_drm_format_modifier is a separate device
                    // extension, unaffected by this KMS cap.)
                    // DRM_CAP_ADDFB2_MODIFIERS, and it has to agree with
                    // what `addfb2_check` accepts: see
                    // `drm::scanout_modifiers_enabled` for why this is 0
                    // until the copy engine can do the block-linear swizzle.
                    0x10 => cap.value = u64::from(drm::scanout_modifiers_enabled()),
                    // DRM_CAP_CRTC_IN_VBLANK_EVENT: our page-flip event carries
                    // the crtc_id, so report support (wlroots requires it).
                    0x12 => cap.value = 1,
                    // DRM_CAP_SYNCOBJ / DRM_CAP_SYNCOBJ_TIMELINE: real
                    // (create/destroy/wait/signal all work, see
                    // zcore_drivers::scheme::syncobj), but gated on the same
                    // opt-in flag as the rest of this session's nouveau-uAPI
                    // work (`nvidia.nouveau_uapi`) so a driver/setup nothing
                    // here has touched (virtio-gpu, or NVIDIA without the
                    // flag) never sees a capability bit flip by default.
                    0x13 | 0x14 => {
                        cap.value = zcore_drivers::display::nouveau_uapi_enabled() as u64;
                        // One-shot per cap, VISIBLE on hardware (klog has no level
                        // filter, unlike the log::debug! below). NVK probes
                        // DRM_CAP_SYNCOBJ (0x13) and DRM_CAP_SYNCOBJ_TIMELINE (0x14)
                        // before it will back a timeline VkSemaphore with a kernel
                        // syncobj. If the next boot's dmesg shows these as `-> 1`
                        // but zink still logs "failed to create timeline semaphore"
                        // with NO `SYNCOBJ_CREATE` line after (see that arm), NVK
                        // gave up in USERSPACE over a capability beyond the cap —
                        // the same class as the external-semaphore block, and NOT a
                        // kernel syncobj bug. `->1` followed by `SYNCOBJ_CREATE`
                        // lines means the create path is reached and the failure is
                        // downstream of the syncobj.
                        static SYNCOBJ_CAP_LOGGED: AtomicBool = AtomicBool::new(false);
                        static TIMELINE_CAP_LOGGED: AtomicBool = AtomicBool::new(false);
                        let latch = if cap.capability == 0x14 {
                            &TIMELINE_CAP_LOGGED
                        } else {
                            &SYNCOBJ_CAP_LOGGED
                        };
                        if !latch.swap(true, Ordering::Relaxed) {
                            kernel_hal::klog_info!(
                                "[drm] GET_CAP {} -> {} (probed by pid {})",
                                if cap.capability == 0x14 {
                                    "SYNCOBJ_TIMELINE"
                                } else {
                                    "SYNCOBJ"
                                },
                                cap.value,
                                drm::current_pid()
                            );
                        }
                    }
                    // DRM_CAP_ASYNC_PAGE_FLIP (0x7), DRM_CAP_PAGE_FLIP_TARGET
                    // (0x11) and DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP (0x15) are
                    // honestly 0: none of the three flip forms is implemented.
                    0x7 | 0x11 | 0x15 => cap.value = 0,
                    // A capability this kernel has never heard of is EINVAL
                    // (`drm_getcap`'s default arm), not "0": a client probing a
                    // NEW cap must be able to tell "not supported" from "the
                    // kernel predates the cap", and the value it left in the
                    // struct is not an answer either.
                    _ => {
                        log::debug!(
                            "[drm] GET_CAP cap={:#x} -> EINVAL (unknown)",
                            cap.capability
                        );
                        return Err(FsError::InvalidParam);
                    }
                }
                log::debug!(
                    "[drm] GET_CAP minor={} cap={:#x} -> {}",
                    self.minor,
                    cap.capability,
                    cap.value
                );
                log::debug!("[drm] GET_CAP cap={:#x} -> {}", cap.capability, cap.value);
                Ok(0)
            }
            DRM_IOCTL_GET_MAGIC => {
                // struct drm_auth { __u32 magic; }: `drm_getmagic` mints one
                // per file, once, starting at 1. 0 is reserved for
                // libdrm's `drmIsMaster()` AUTH_MAGIC probe.
                unsafe { *(data as *mut u32) = self.file.magic() };
                Ok(0)
            }
            DRM_IOCTL_GET_CLIENT => {
                // `struct drm_client`: libva enumerates clients at init.
                // Single-client stub: idx 0 is this open; anything else ENOENT.
                // Linux's `drm_getclient` always reports `magic = 0` ("do not
                // return authenticating magic index"). Returning 1 made a
                // client AUTH_MAGIC a number nobody had minted — or, worse,
                // spend the first real GET_MAGIC on this boot.
                let c = unsafe { &mut *(data as *mut DrmClient) };
                if c.idx != 0 {
                    return Err(FsError::EntryNotFound);
                }
                c.auth = 1;
                c.pid = drm::current_pid() as usize;
                c.uid = 0;
                c.magic = 0;
                c.iocs = 0;
                Ok(0)
            }
            DRM_IOCTL_AUTH_MAGIC => {
                // `DRM_MASTER` ioctl: only the master authenticates
                // (EACCES), and only a magic a file of this device holds
                // (`drm_authmagic`: EINVAL), once. Magic 0 is never minted:
                // libdrm's `drmIsMaster()` AUTH_MAGIC(0) relies on EINVAL
                // from the master (EACCES from everyone else).
                if !drm::is_master(self.minor, self.file_owner()) {
                    return Err(FsError::NoPermission);
                }
                let magic = unsafe { *(data as *const u32) };
                if drm::auth_magic(self.minor, magic) {
                    Ok(0)
                } else {
                    if magic != 0 {
                        log::debug!(
                            "[drm] AUTH_MAGIC minor={} magic={:#x} -> EINVAL",
                            self.minor,
                            magic
                        );
                    }
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_SET_MASTER => {
                // `drm_setmaster_ioctl`: the master again is a no-op, another
                // open's master is EBUSY, a free device is taken. Nothing was
                // recorded: every caller was told it was master, and
                // `drm-probe --scanout` or `eclipse-bench`, which stand down
                // on EBUSY, took the display from the running compositor.
                if let Err(drm::MasterError::Busy) = drm::set_master(self.minor, self.file_owner())
                {
                    return Err(FsError::Busy);
                }
                // Become DRM master, but do NOT switch the console to graphics
                // yet: defer that to the first real scanout (`drm::scanout`). If
                // the client stalls before presenting a frame (e.g. its renderer
                // fails to init), the kernel text console stays usable and its
                // logs visible instead of freezing on a black screen.
                log::debug!("[drm] SET_MASTER (minor={})", self.minor);
                // A new compositor session gets a fresh present-failure trace
                // budget. The budget is what keeps a failing present from
                // flooding the console, but as a per-BOOT count it was useless
                // on a rig that stays up for days: the machine that produced
                // the `Failed to set CRTC` log had been up 21 hours and had
                // started labwc several times, so the eight lines that would
                // have named the cause were spent on a session long gone. One
                // compositor session is the right unit -- it is exactly one
                // attempt at bringing the desktop up.
                PRESENT_FAIL_TRACED.store(0, Ordering::Relaxed);
                Ok(0)
            }
            DRM_IOCTL_DROP_MASTER => {
                // `drm_dropmaster_ioctl`: a file that is not the master has
                // nothing to drop (EINVAL), and in particular does not run
                // the console restore below on the compositor's behalf.
                if !drm::drop_master(self.minor, self.file_owner()) {
                    return Err(FsError::InvalidParam);
                }
                // In a seat-managed session (seatd owns tty7 via VT_PROCESS) the
                // SEAT -- not DRM master -- drives the console KD mode: seatd
                // already put tty7 into KD_GRAPHICS and will restore text via
                // its own VT handshake when the session ends. A DROP_MASTER here
                // is then typically TRANSIENT (wlroots/Xwayland resetting the DRM
                // backend during renderer fallback -- e.g. "EGL setup failed,
                // falling back to sw"), not a real relinquish. The old
                // unconditional set_kd_mode(KD_TEXT) flipped the active graphics
                // VT (tty7) back to text, which switch_vt_impl(0) reverted to
                // tty1 -- so the compositor's very first present landed on VT 0
                // and the desktop never appeared on tty7. Restore text only when
                // NO seat owns the graphics VT (bare DRM client / QEMU pixman /
                // Xorg path), where this mechanism is the only one in play.
                let seat_owned = crate::fs::stdio::graphics_vt_seat_owned();
                kernel_hal::klog_info!(
                    "[drm] DROP_MASTER (minor={}) seat_owned={} -- {}",
                    self.minor,
                    seat_owned,
                    if seat_owned {
                        "seat drives KD mode, NOT restoring text"
                    } else {
                        "restoring text console"
                    }
                );
                if !seat_owned {
                    kernel_hal::console::set_kd_mode(kernel_hal::console::KD_TEXT);
                }
                // Forget the compositor's DRM VT ownership so text consoles are
                // no longer gated off screen/input (a live compositor's next
                // present re-claims it).
                //
                // Deliberately NOT done here any more (both were Linux-divergent
                // and both were mutated GLOBALLY by whichever client dropped
                // master, clobbering the live compositor's state):
                //  * cancel_pending_events(): pending DRM events belong to the
                //    drm_file that queued them and survive a master drop in
                //    Linux. A transient DROP_MASTER from a probing client
                //    (Xwayland at session bring-up) inside the one-vblank window
                //    after labwc's first page-flip swallowed labwc's completion
                //    -- wlroots waited on it forever and the desktop froze on
                //    its first frame until a VT cycle forced a re-enable. Events
                //    are now cancelled only when the flip-owning process EXITS
                //    (drm::cancel_events_for_exit in release_process).
                //  * set_atomic_client(false): DRM_CLIENT_CAP_ATOMIC is
                //    per-file in Linux and never revoked by a master drop;
                //    clearing it globally made the compositor's atomic
                //    property view flip mid-session.
                drm::clear_graphics_owner();
                Ok(0)
            }
            DRM_IOCTL_SET_VERSION => {
                // `drm_setversion`: report the interface version (1.4) and
                // the driver's own (`dev->driver->major/minor`, the pair
                // VERSION reports), and refuse a requested pair whose major
                // differs or whose minor is negative or newer than the one
                // there is (EINVAL); -1 means "query only". The answer is
                // written back either way. Xorg's modesetting driver calls
                // this right after open and treats ENOTTY as "not a DRM
                // device". This reported driver 1.0 while VERSION said 1.4
                // under nouveau, and read only the driver major, so a client
                // asking for driver 1.7 was told yes.
                let sv = unsafe { &mut *(data as *mut DrmSetVersion) };
                let (dd_major, dd_minor, _) = driver_version(drm::node_driver_id(
                    self.minor,
                    zcore_drivers::display::nouveau_uapi_enabled(),
                ));
                let req_if = (sv.drm_di_major, sv.drm_di_minor);
                let req_dd = (sv.drm_dd_major, sv.drm_dd_minor);
                sv.drm_di_major = 1;
                sv.drm_di_minor = 4;
                sv.drm_dd_major = dd_major;
                sv.drm_dd_minor = dd_minor;
                if req_if.0 != -1 && (req_if.0 != 1 || req_if.1 < 0 || req_if.1 > 4) {
                    return Err(FsError::InvalidParam);
                }
                if req_dd.0 != -1 && (req_dd.0 != dd_major || req_dd.1 < 0 || req_dd.1 > dd_minor) {
                    return Err(FsError::InvalidParam);
                }
                Ok(0)
            }
            DRM_IOCTL_SET_CLIENT_CAP => {
                // struct drm_set_client_cap { __u64 capability; __u64 value; }
                let cap = unsafe { *(data as *const u64) };
                let value = unsafe { *(data.wrapping_add(8) as *const u64) };
                match cap {
                    DRM_CLIENT_CAP_ATOMIC => {
                        // Linux refuses this with EOPNOTSUPP unless the driver
                        // has DRIVER_ATOMIC. Eclipse's atomic path covers the
                        // software-KMS pipeline and — until it has the same
                        // mileage as the proven legacy path — is opt-in via
                        // the `drm.atomic` cmdline flag (nouveau shipped its
                        // atomic support gated the same way).
                        if !drm::atomic_enabled() || !drm::software_kms_active() {
                            log::debug!(
                                "[drm] SET_CLIENT_CAP ATOMIC -> EOPNOTSUPP (legacy KMS; boot with drm.atomic to enable)"
                            );
                            return Err(FsError::OpNotSupported);
                        }
                        // Linux: values 0..2 (2 = relaxed checking); > 2 EINVAL.
                        if value > 2 {
                            return Err(FsError::InvalidParam);
                        }
                        // Setting atomic also implies universal planes:
                        // `drm_setclientcap` stores the value in both fields.
                        self.file.set_atomic_client(value != 0);
                        self.file.set_universal_planes(value != 0);
                        log::debug!("[drm] SET_CLIENT_CAP ATOMIC={} -> accepted", value);
                        Ok(0)
                    }
                    DRM_CLIENT_CAP_WRITEBACK_CONNECTORS => {
                        // Linux: atomic clients only. There are no writeback
                        // connectors to expose, so accepting is a no-op.
                        if !self.file.atomic_client() || value > 1 {
                            log::debug!("[drm] SET_CLIENT_CAP WRITEBACK -> EINVAL");
                            return Err(FsError::InvalidParam);
                        }
                        Ok(0)
                    }
                    // UNIVERSAL_PLANES: a boolean (`value > 1` is EINVAL)
                    // that `drm_mode_getplane_res` reads. It was accepted and
                    // forgotten, so a legacy client that never set it was
                    // handed the primary plane as if it were an overlay.
                    DRM_CLIENT_CAP_UNIVERSAL_PLANES => {
                        if value > 1 {
                            return Err(FsError::InvalidParam);
                        }
                        self.file.set_universal_planes(value != 0);
                        log::debug!(
                            "[drm] SET_CLIENT_CAP UNIVERSAL_PLANES={} -> accepted",
                            value
                        );
                        Ok(0)
                    }
                    // STEREO_3D, ASPECT_RATIO: a boolean each in Linux
                    // (`drm_setclientcap`: `value > 1` is EINVAL); nothing
                    // here changes with them, so accept the two legal values
                    // and refuse the rest.
                    DRM_CLIENT_CAP_STEREO_3D | DRM_CLIENT_CAP_ASPECT_RATIO => {
                        if value > 1 {
                            return Err(FsError::InvalidParam);
                        }
                        log::debug!("[drm] SET_CLIENT_CAP cap={} -> accepted", cap);
                        Ok(0)
                    }
                    // CURSOR_PLANE_HOTSPOT is EOPNOTSUPP unless the driver has
                    // DRIVER_CURSOR_HOTSPOT (the virtualised ones: virtio-gpu,
                    // vmwgfx, qxl). There is no cursor plane with HOTSPOT_X/Y
                    // properties here, so a client told "yes" would look for
                    // them in vain, or worse, assume a virtualised cursor.
                    DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT => Err(FsError::OpNotSupported),
                    // Anything else: EINVAL, as `drm_setclientcap`'s default
                    // arm. Accepting it told a client that a cap this kernel
                    // has never heard of was now in force.
                    _ => {
                        log::debug!("[drm] SET_CLIENT_CAP cap={} -> EINVAL (unknown)", cap);
                        Err(FsError::InvalidParam)
                    }
                }
            }
            DRM_IOCTL_MODE_CREATE_DUMB => {
                let info = unsafe { &mut *(data as *mut DrmModeCreateDumb) };
                // `drm_mode_create_dumb`: a row is `DIV_ROUND_UP(bpp, 8) *
                // width` bytes, and a width, height or bpp of 0, or a bpp
                // past `U32_MAX - 8`, is EINVAL -- the bpp of 0 is refused
                // here, and the pitch and size checks below answer for the
                // rest (a zero pitch or size, or one past the ceiling). This
                // promoted every bpp below 32 to 32, so a bpp of 0 got a
                // 32-bit buffer and a 16-bit request was sized (and its
                // pitch reported) as if it were 32-bit.
                if info.bpp == 0 {
                    return Err(FsError::InvalidParam);
                }
                let bpp = info.bpp;
                let cpp = (bpp as u64).div_ceil(8);
                // width/height/bpp are userspace-controlled: compute pitch/size
                // in 64-bit. A 32-bit `width*bpp` or `pitch*height` would wrap
                // (e.g. 50000x50000x32) and under-allocate the buffer while
                // echoing a huge size back, becoming an OOB read at scanout.
                // Bound the result to a sane ceiling (64 MiB — a 4K XRGB frame
                // is ~33 MiB) and require pitch to fit the u32 written back.
                const MAX_DUMB_SIZE: u64 = 64 * 1024 * 1024;
                let mut pitch64 = (info.width as u64 * cpp + 63) & !63;
                let mut size64 = pitch64.saturating_mul(info.height as u64);
                // When the compositor requests a full-screen dumb buffer, align
                // its pitch with the display scanout pitch so the CE-offload
                // present path can fire (flat copy needs equal strides). Without
                // this, wlroots' 64-byte-aligned pitch often differs from the
                // GOP framebuffer pitch and every frame falls back to the slow
                // CPU blit (~7-10 FPS on dual RTX).
                if let Some((dw, dh, dp)) = drm::display_mode() {
                    // Only for a 32-bit buffer: the scanout pitch is a 32-bit
                    // pitch, and a narrower buffer keeps the pitch of its own
                    // bpp.
                    if cpp == 4 && info.width == dw && info.height == dh && dp as u64 >= pitch64 {
                        pitch64 = dp as u64;
                        size64 = pitch64.saturating_mul(info.height as u64);
                    }
                }
                if pitch64 == 0
                    || pitch64 > u32::MAX as u64
                    || size64 == 0
                    || size64 > MAX_DUMB_SIZE
                {
                    log::warn!(
                        "[drm] CREATE_DUMB {}x{} bpp={} -> rejected (pitch={} size={} out of range)",
                        info.width, info.height, bpp, pitch64, size64
                    );
                    return Err(FsError::InvalidParam);
                }
                let pitch = pitch64 as u32;
                let size = size64 as usize;

                if let Some(handle) = drm::alloc_buffer(size) {
                    info.handle = handle.id;
                    info.pitch = pitch;
                    info.size = size as u64;
                    log::debug!(
                        "[drm] CREATE_DUMB {}x{} bpp={} -> handle={} pitch={} size={}",
                        info.width,
                        info.height,
                        bpp,
                        handle.id,
                        pitch,
                        size
                    );
                    Ok(0)
                } else {
                    log::error!(
                        "[drm] CREATE_DUMB {}x{} bpp={} -> alloc failed (size={})",
                        info.width,
                        info.height,
                        bpp,
                        size
                    );
                    // Linux: GEM alloc failure is ENOMEM, not ENOSPC.
                    Err(FsError::NoMemory)
                }
            }
            DRM_IOCTL_MODE_ADDFB => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd) };
                // `drm_mode_addfb` is `drm_mode_addfb2` with the fourcc
                // derived from (bpp, depth): a pair the table does not know is
                // EINVAL, and the derived format then goes through the same
                // checks as an ADDFB2, which refuse every format no plane
                // scans out. This arm read neither field, so a 16-bit or a
                // 10-bit framebuffer was wrapped as XRGB8888 and scanned out
                // as garbage, and an ARGB8888 one (32/32) was registered as
                // XRGB8888 and reported back with depth 24.
                let Some(pixel_format) = legacy_fb_format(cmd.bpp, cmd.depth) else {
                    log::debug!(
                        "[drm] ADDFB bpp={} depth={} -> EINVAL (no such format)",
                        cmd.bpp,
                        cmd.depth
                    );
                    return Err(FsError::InvalidParam);
                };
                let as_fb2 = DrmModeFbCmd2 {
                    fb_id: 0,
                    width: cmd.width,
                    height: cmd.height,
                    pixel_format,
                    flags: 0,
                    handles: [cmd.handle, 0, 0, 0],
                    pitches: [cmd.pitch, 0, 0, 0],
                    offsets: [0; 4],
                    modifier: [0; 4],
                };
                // ADDFB has no modifier word at all, so this is always linear.
                let _ = addfb2_check(&as_fb2)?;
                // Linux: unknown GEM handle → ENOENT; bad geometry → EINVAL.
                // Both used to collapse to DeviceError (EIO).
                if drm::resolve_gem_backing_for(cmd.handle, drm::current_pid()).is_none() {
                    return Err(FsError::EntryNotFound);
                }
                if let Some(fb_id) = drm::create_fb_with_format(
                    cmd.handle,
                    cmd.width,
                    cmd.height,
                    cmd.pitch,
                    pixel_format,
                ) {
                    cmd.fb_id = fb_id;
                    Ok(0)
                } else {
                    log::error!(
                        "[drm] ADDFB failed: {}x{} handle={:#x} pitch={} (geometry/format)",
                        cmd.width,
                        cmd.height,
                        cmd.handle,
                        cmd.pitch
                    );
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_ADDFB2 => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd2) };
                let layout = addfb2_check(cmd)?;
                // Linux: unknown GEM handle → ENOENT; bad geometry → EINVAL.
                if drm::resolve_gem_backing_for(cmd.handles[0], drm::current_pid()).is_none() {
                    return Err(FsError::EntryNotFound);
                }
                if let Some(fb_id) = drm::create_fb_with_layout(
                    cmd.handles[0],
                    cmd.width,
                    cmd.height,
                    cmd.pitches[0],
                    cmd.pixel_format,
                    layout,
                ) {
                    cmd.fb_id = fb_id;
                    Ok(0)
                } else {
                    log::error!(
                        "[drm] ADDFB2 failed: {}x{} handle={:#x} pitch={} fmt={:#x} modifier={:#x} \
                         (geometry/format)",
                        cmd.width, cmd.height, cmd.handles[0], cmd.pitches[0], cmd.pixel_format, cmd.modifier[0]
                    );
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_RMFB => {
                let fb_id = unsafe { *(data as *const u32) };
                // ENOENT for an id that is not the caller's, whether it does
                // not exist or belongs to someone else -- `drm_mode_rmfb`
                // walks `file_priv->fbs` and gives the same answer either way,
                // so a prober cannot learn that another client's framebuffer
                // exists. Swallowing the result let any process remove the
                // compositor's scanout framebuffer, after which every SETCRTC
                // and PAGE_FLIP on it fails and wlroots retries the modeset
                // forever.
                //
                // And the CRTC showing this framebuffer goes off with it, as
                // `drm_framebuffer_remove` disables it (the blank of a VT
                // switch under Xorg's modesetting, which RMFBs its scanout
                // buffer on leaving). This arm only dropped the object, so
                // the panel kept showing a frame the client had freed and
                // GETCRTC said the CRTC was on. The process-exit sweep
                // keeps the last frame for the console restore, as before.
                if drm::rmfb_disabling_for(fb_id, drm::current_pid()) {
                    Ok(0)
                } else {
                    Err(FsError::EntryNotFound)
                }
            }
            DRM_IOCTL_MODE_CLOSEFB => {
                // `drm_mode_closefb_ioctl` drops the file's reference and
                // nothing else: the CRTC keeps scanning the buffer out
                // ("close without disabling", what RMFB above is not). The
                // object goes from the table here, so GETCRTC reports fb 0
                // where Linux would keep naming it. An id that is not the
                // caller's is
                // ENOENT, as in `drm_mode_closefb_ioctl` (the lookup and the
                // "is it in this file's list" check both answer that) and as
                // RMFB above already did; this arm said EINVAL.
                let fb_id = unsafe { *(data as *const u32) };
                if drm::rmfb_for(fb_id, drm::current_pid()) {
                    Ok(0)
                } else {
                    Err(FsError::EntryNotFound)
                }
            }
            DRM_IOCTL_MODE_MAP_DUMB => {
                let map = unsafe { &mut *(data as *mut DrmModeMapDumb) };
                // Linux's `drm_gem_dumb_map_offset` looks the handle up first
                // (`drm_gem_object_lookup`: ENOENT for one this file does not
                // hold) and only then mints the offset. A cookie for a handle
                // the caller does not own is not a way in -- `get_vmo` checks
                // again -- but it turned a client's stale or wrong handle into
                // an EINVAL from `mmap()` two calls later instead of the ENOENT
                // here that names the ioctl at fault.
                if !drm::gem_handle_mappable(map.handle) {
                    return Err(FsError::EntryNotFound);
                }
                // Return a page-aligned fake offset (`handle << PAGE_SHIFT`). The
                // subsequent mmap of the dumb buffer passes this back as the file
                // offset; musl's `mmap()` rejects a non-page-aligned offset with
                // EINVAL, and `get_vmo()` shifts it back to the handle id.
                map.offset = mmap_cookie_for(map.handle);
                Ok(0)
            }
            DRM_IOCTL_MODE_DESTROY_DUMB => {
                let handle = unsafe { *(data as *const u32) };
                // `gem_close` already refuses a handle that is not the
                // caller's; reporting its verdict is what tells a client it
                // freed something twice, or something that was never its own.
                // Linux's `drm_mode_destroy_dumb_ioctl` answers EINVAL through
                // `drm_gem_handle_delete`. Swallowing it made every wrong free
                // look like a good one.
                if drm::gem_close(handle) {
                    // Same as GEM_CLOSE: Linux routes both through
                    // `drm_gem_handle_delete`, which drops the file's PRIME
                    // record of the handle with it.
                    self.file.forget_prime_import(handle);
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_SETCRTC => {
                // struct drm_mode_crtc has the same layout as DrmModeGetCrtc.
                let req = unsafe { &mut *(data as *mut DrmModeGetCrtc) };
                // `drm_mode_setcrtc` finds the CRTC before anything else
                // (ENOENT), refuses a connector list with no mode or no fb to
                // set them to (EINVAL) or longer than the card's connectors
                // (EINVAL), and looks every connector in it up (ENOENT). None
                // of it was read: a modeset on a CRTC the card does not have
                // went to the one it has, and a connector list of any content
                // was accepted unread.
                if drm::get_crtc(req.crtc_id).is_none() {
                    return Err(FsError::EntryNotFound);
                }
                // With a mode, `drm_mode_setcrtc` then looks the fb up
                // (ENOENT; -1 names the fb already on the CRTC, EINVAL when
                // there is none), runs the mode through
                // `drm_mode_convert_umode` (EINVAL for a zero clock, a zero
                // active area, sync timings out of order or an aspect-ratio
                // code it does not define) and wants the active area at
                // (x, y) inside the fb (`drm_crtc_check_viewport`, ENOSPC).
                // None of it was read: a mode wider or taller than its fb
                // was scanned out as if the fb fit it, a mode with no clock
                // set the vblank pacing to a fallback and answered done, and
                // -1 was looked up as an fb id.
                let mut fb_id = req.fb_id;
                if req.mode_valid != 0 {
                    if fb_id == u32::MAX {
                        fb_id = drm::get_crtc(req.crtc_id).map_or(0, |c| c.fb_id);
                        if fb_id == 0 {
                            return Err(FsError::InvalidParam);
                        }
                    }
                    let Some(fb) = drm::get_fb(fb_id) else {
                        return Err(FsError::EntryNotFound);
                    };
                    let Some((w, h)) = modeinfo_active_area(&req.mode) else {
                        return Err(FsError::InvalidParam);
                    };
                    if w > fb.width
                        || req.x > fb.width - w
                        || h > fb.height
                        || req.y > fb.height - h
                    {
                        return Err(FsError::NoDeviceSpace);
                    }
                }
                if req.count_connectors > 0 {
                    if req.mode_valid == 0 || fb_id == 0 {
                        return Err(FsError::InvalidParam);
                    }
                    let connectors = drm::get_resources().2;
                    if req.count_connectors as usize > connectors.len() {
                        return Err(FsError::InvalidParam);
                    }
                    let n = req.count_connectors as usize;
                    ucheck_n::<u32>(req.set_connectors_ptr as usize, n)?;
                    for i in 0..n {
                        let id = unsafe { *(req.set_connectors_ptr as *const u32).add(i) };
                        if !connectors.contains(&id) {
                            return Err(FsError::EntryNotFound);
                        }
                    }
                }
                if req.mode_valid != 0 {
                    drm::set_vblank_period_from_modeinfo(&req.mode);
                    if let Err(e) = drm::present_now_checked(fb_id, req.crtc_id, None) {
                        present_failed("SETCRTC", fb_id, req.crtc_id, e)?;
                    }
                } else {
                    // `drm_mode_setcrtc` without a mode turns the pipe off
                    // (`set_config` with `.mode = NULL`), and only looks the
                    // fb up under `mode_valid`, so one named here is ignored
                    // rather than shown. Doing nothing here is half of why a
                    // screen could never be blanked: wlroots disables an
                    // output with DPMS off followed by exactly this call, and
                    // both were no-ops, so the panel kept the last frame lit
                    // while the compositor believed it was dark. The mode
                    // goes with it (`__drm_atomic_helper_set_config` sets it
                    // to NULL and detaches the connectors), which is what
                    // GETCRTC, GETENCODER and GETCONNECTOR report from now
                    // on; this arm kept answering `mode_valid = 1` with the
                    // encoder still on the CRTC, so a compositor starting
                    // after another had disabled the output took the console's
                    // mode for a current one.
                    drm::set_crtc_blanked(true);
                    drm::set_crtc_fb(req.crtc_id, 0);
                    drm::set_crtc_enabled(false);
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_PAGE_FLIP => {
                let flip = unsafe { *(data as *const DrmModeCrtcPageFlip) };
                // Linux `drm_mode_page_flip_ioctl`: unknown flags and a
                // non-zero reserved word are EINVAL; ASYNC is EINVAL while
                // DRM_CAP_ASYNC_PAGE_FLIP is 0 and the TARGET_* flags while
                // DRM_CAP_PAGE_FLIP_TARGET is 0; an unknown fb is ENOENT. A
                // completion event is queued only when the caller asked.
                const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
                const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 0x02;
                const DRM_MODE_PAGE_FLIP_TARGET: u32 = 0x0c;
                const DRM_MODE_PAGE_FLIP_FLAGS: u32 = 0x0f;
                if flip.flags & !DRM_MODE_PAGE_FLIP_FLAGS != 0
                    || flip.reserved != 0
                    || flip.flags & (DRM_MODE_PAGE_FLIP_ASYNC | DRM_MODE_PAGE_FLIP_TARGET) != 0
                {
                    return Err(FsError::InvalidParam);
                }
                // The CRTC first, then the fb, both ENOENT: `drm_crtc_find`
                // and `drm_framebuffer_lookup` in that order. The CRTC id was
                // never read, so a flip aimed at a CRTC the card does not have
                // landed on the one it has.
                let Some(crtc) = drm::get_crtc(flip.crtc_id) else {
                    return Err(FsError::EntryNotFound);
                };
                let Some(fb) = drm::get_fb(flip.fb_id) else {
                    return Err(FsError::EntryNotFound);
                };
                // Then `drm_crtc_check_viewport(crtc, crtc->x, crtc->y,
                // &crtc->mode, fb)`: the CRTC's mode has to fit in the new
                // fb (ENOSPC), and "page flip is not allowed to change frame
                // buffer format" (EINVAL). Neither was read: a flip onto an
                // fb smaller than the mode, or of another format, went to
                // the scanout as if it were the frame the CRTC was set up
                // with. (Linux also refuses a flip on a CRTC with no fb,
                // EBUSY; this tree's flip is also its present, so a client
                // is allowed to start with one, as the GL sequence does.)
                if let Some((w, h, _)) = drm::display_mode() {
                    if fb.width < w || fb.height < h {
                        return Err(FsError::NoDeviceSpace);
                    }
                }
                if let Some(old) = drm::get_fb(crtc.fb_id) {
                    if old.pixel_format != fb.pixel_format {
                        return Err(FsError::InvalidParam);
                    }
                }
                let want_event = flip.flags & DRM_MODE_PAGE_FLIP_EVENT != 0;
                match drm::page_flip(
                    flip.fb_id,
                    flip.crtc_id,
                    flip.user_data,
                    want_event,
                    &self.file,
                ) {
                    Ok(()) => Ok(0),
                    Err(drm::FlipError::Busy) => Err(FsError::Busy),
                    // Same policy as SETCRTC above: only a missing fb fails the
                    // ioctl (and with ENOENT, not EIO). The completion for the
                    // others is already queued.
                    Err(drm::FlipError::Present(e)) => {
                        present_failed("PAGE_FLIP", flip.fb_id, flip.crtc_id, e)?;
                        Ok(0)
                    }
                }
            }
            DRM_IOCTL_WAIT_VBLANK => {
                // union drm_wait_vblank. A software framebuffer has no real
                // vblank, so synthesize the next sequence from the monotonic
                // clock. If the caller asked for an event (`_DRM_VBLANK_EVENT`)
                // deliver a DRM_EVENT_VBLANK on the card fd; otherwise fill the
                // reply and return immediately instead of blocking.
                let req = unsafe { &mut *(data as *mut DrmWaitVblank) };
                wait_vblank_check(req.typ)?;
                let typ = req.typ;
                let signal = req.val1;
                // Only ask the driver for a real vblank when it has hardware
                // KMS support. Without hardware KMS (e.g. the NVIDIA stub
                // registers has_hardware_kms()=false) wait_vblank is
                // implemented as a busy 16.7 ms spin, and calling it on every
                // WAIT_VBLANK ioctl causes severe CPU starvation on a
                // cooperative async runtime — making the system appear frozen.
                if !drm::software_kms_active() {
                    if let Some(driver) = drm::get_primary_driver() {
                        if driver.has_hardware_kms() {
                            let _ = driver.wait_vblank(0);
                        }
                    }
                }
                // Resolve the requested vblank like `drm_wait_vblank_ioctl`:
                // `_DRM_VBLANK_RELATIVE` counts from the current sequence,
                // absolute is taken as is, and `_DRM_VBLANK_NEXTONMISS` moves a
                // target that already passed to the next vblank. The request
                // used to be ignored entirely (event at the next vblank, or an
                // immediate reply), so "+2" or an absolute future MSC came
                // back early with a smaller sequence than asked.
                const _DRM_VBLANK_RELATIVE: u32 = 0x1;
                const _DRM_VBLANK_NEXTONMISS: u32 = 0x1000_0000;
                let now_seq = drm::vblank_seq_now();
                let mut target = if typ & _DRM_VBLANK_RELATIVE != 0 {
                    now_seq.wrapping_add(req.sequence)
                } else {
                    req.sequence
                };
                let passed = (target.wrapping_sub(now_seq) as i32) <= 0;
                if passed && typ & _DRM_VBLANK_NEXTONMISS != 0 {
                    target = now_seq.wrapping_add(1);
                }
                if typ & _DRM_VBLANK_EVENT != 0 {
                    // Post the event when the counter reaches `target`, never
                    // before the next synthetic vblank: delivering it instantly
                    // turns a vblank-paced client loop into a busy spin (see
                    // `schedule_flip_event`).
                    drm::schedule_vblank_event(signal, target, &self.file);
                    // The reply names the vblank the event was queued for
                    // (`drm_queue_vblank_event`: `reply.sequence = req_seq`,
                    // or the current count when the target had already
                    // passed). Xorg's modesetting driver reads it as the MSC
                    // it queued (`ms_queue_vblank`) and was getting 0.
                    // (After the NEXTONMISS move, as `drm_queue_vblank_event`
                    // looks at the moved target.)
                    let still_passed = (target.wrapping_sub(now_seq) as i32) <= 0;
                    req.sequence = if still_passed { now_seq } else { target };
                } else {
                    // Blocking form. The wait itself already happened in
                    // `sys_ioctl` (see `DrmDev::wait_vblank_sleep`): this arm
                    // is synchronous and cannot sleep, so it only reports the
                    // sequence that has completed by the time it runs -- which
                    // is the requested one, because the sleep preceded it.
                    let now = kernel_hal::timer::timer_now();
                    req.typ = 0; // _DRM_VBLANK_ABSOLUTE
                                 // Return the *current* completed vblank sequence.  Returning
                                 // vblank_seq_now()+1 (the upcoming vblank) was incorrect: it
                                 // made the X11 Present MSC tracker believe the display was
                                 // always one vblank ahead, so it added an extra ~16.7 ms wait
                                 // per frame, halving the achievable frame rate.
                    req.sequence = now_seq;
                    req.val1 = now.as_secs(); // tval_sec
                    req.val2 = now.subsec_micros() as u64; // tval_usec
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_SETPLANE => {
                // Primary-plane update: present immediately on the target CRTC.
                // fb_id == 0 disables the plane, which we treat as a no-op.
                let req = unsafe { *(data as *const DrmModeSetPlane) };
                // `drm_mode_setplane` looks the plane up before anything else
                // and answers ENOENT for one that does not exist. The id was
                // not read at all, so a stale or invented plane id presented
                // the fb on the CRTC as if it named the primary plane.
                let Some(plane) = drm::get_plane(req.plane_id) else {
                    return Err(FsError::EntryNotFound);
                };
                if req.fb_id != 0 {
                    // With an fb to show, `drm_mode_setplane` looks the fb up
                    // and then the CRTC, both ENOENT. The CRTC id was not
                    // read either.
                    let Some(fb) = drm::get_fb(req.fb_id) else {
                        return Err(FsError::EntryNotFound);
                    };
                    if drm::get_crtc(req.crtc_id).is_none() {
                        return Err(FsError::EntryNotFound);
                    }
                    // `__setplane_check`: the plane has to be usable on this
                    // CRTC (`possible_crtcs & drm_crtc_mask(crtc)`, EINVAL;
                    // the mask bit is the CRTC's index in the resource list,
                    // which is what GETPLANE advertises), and the source
                    // rectangle, in 16.16, has to lie inside the fb
                    // (`drm_framebuffer_check_src_coords`, ENOSPC). Neither
                    // was read: a plane was put on a CRTC it does not reach,
                    // and a source rectangle past the fb's edge was accepted
                    // and then ignored, so the client believed it was showing
                    // a crop the scanout never made.
                    let index = drm::get_resources()
                        .1
                        .iter()
                        .position(|&id| id == req.crtc_id)
                        .unwrap_or(usize::MAX);
                    if index >= 32 || plane.possible_crtcs & (1 << index) == 0 {
                        return Err(FsError::InvalidParam);
                    }
                    let (fb_w, fb_h) = ((fb.width as u64) << 16, (fb.height as u64) << 16);
                    let (src_x, src_y) = (req.src_x as u64, req.src_y as u64);
                    let (src_w, src_h) = (req.src_w as u64, req.src_h as u64);
                    if src_w > fb_w || src_x > fb_w - src_w || src_h > fb_h || src_y > fb_h - src_h
                    {
                        return Err(FsError::NoDeviceSpace);
                    }
                    if let Err(e) = drm::present_now_checked(req.fb_id, req.crtc_id, None) {
                        present_failed("SETPLANE", req.fb_id, req.crtc_id, e)?;
                    }
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_GETFB => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd) };
                if let Some(fb) = drm::get_fb(cmd.fb_id) {
                    cmd.width = fb.width;
                    cmd.height = fb.height;
                    cmd.pitch = fb.pitch;
                    cmd.bpp = 32;
                    // `drm_mode_getfb` reports the format's depth: 32 with an
                    // alpha channel, 24 without.
                    cmd.depth = if fb.pixel_format == drm::DRM_FORMAT_ARGB8888 {
                        32
                    } else {
                        24
                    };
                    // The backing GEM handle goes only to the client that
                    // created this framebuffer. Linux gates it on DRM master
                    // or CAP_SYS_ADMIN and zeroes the field otherwise, with
                    // the comment "GET_FB() is an unprivileged ioctl so we
                    // must not return a buffer-handle to non-master
                    // processes!". There is no master state to consult here,
                    // and the premise this used to rest on -- that a single
                    // client is implicitly master -- is not true of a session
                    // running Xwayland or any Vulkan probe alongside the
                    // compositor. Handing the handle out made fb ids, which
                    // are sequential from 1, an enumeration route straight to
                    // the compositor's pixels.
                    cmd.handle = if drm::fb_owned_by_caller(&fb) {
                        fb.gem_handle_id
                    } else {
                        0
                    };
                    Ok(0)
                } else {
                    // ENOENT, like `drm_mode_getfb`'s framebuffer lookup. This
                    // answered EINVAL, which reached the boot log only as a
                    // bare `[einval-hunt] ... a1=0xc01c64ad -> EINVAL` with
                    // nothing to say the fb id was the problem -- the same
                    // unknown-fb id that `SETCRTC` was failing on, wearing a
                    // different errno.
                    Err(FsError::EntryNotFound)
                }
            }
            DRM_IOCTL_MODE_GETFB2 => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd2) };
                if let Some(fb) = drm::get_fb(cmd.fb_id) {
                    cmd.width = fb.width;
                    cmd.height = fb.height;
                    cmd.pixel_format = fb.pixel_format;
                    cmd.flags = 0;
                    // Same gate as GETFB above; `drm_mode_getfb2_ioctl`
                    // carries the identical check.
                    cmd.handles = if drm::fb_owned_by_caller(&fb) {
                        [fb.gem_handle_id, 0, 0, 0]
                    } else {
                        [0; 4]
                    };
                    // `fb.pitch` is the uAPI number ADDFB2 was handed, not a
                    // byte count: `create_fb_with_layout` derives its byte
                    // pitch for the size check and stores the request
                    // verbatim. So a tiled framebuffer reads back in 64-byte
                    // blocks, exactly as it was created, and
                    // `a_tiled_framebuffer_reads_back_the_pitch_it_was_made_with`
                    // is what notices if that stops being true.
                    cmd.pitches = [fb.pitch, 0, 0, 0];
                    cmd.offsets = [0; 4];
                    // `drm_mode_getfb2_ioctl` reports the modifier so a
                    // client can re-create the framebuffer from what it
                    // reads back. Answering 0 for a tiled framebuffer hands
                    // it a description of a DIFFERENT surface -- same
                    // handle, same pitch number, linear -- which is the
                    // recipe for the garbage this layout is gated against.
                    //
                    // The flag goes up exactly when the modifier word is one
                    // worth honouring, i.e. for a non-linear layout. Linux
                    // sets it whenever the driver supports modifiers at all,
                    // because there it round-trips the per-fb flag the
                    // client created the framebuffer with; there is no such
                    // stored flag here, and a linear framebuffer's modifier
                    // is 0 either way, so the two spellings describe the
                    // same surface.
                    cmd.modifier = [scanout_layout_modifier(fb.layout), 0, 0, 0];
                    if fb.layout != drm::ScanoutLayout::Linear {
                        cmd.flags |= DRM_MODE_FB_MODIFIERS;
                    }
                    Ok(0)
                } else {
                    // ENOENT, like `drm_mode_getfb2_ioctl`. Same reasoning as
                    // GETFB above.
                    Err(FsError::EntryNotFound)
                }
            }
            DRM_IOCTL_MODE_DIRTYFB => {
                // Flush accumulated damage by re-scanning the framebuffer out.
                // Clients that keep one persistent FB and signal damage with
                // DIRTYFB (X's modesetting shadow, simple toolkits) rely on this
                // to update the screen. A full-frame copy of a swapchain buffer
                // that only has those boxes painted left stale tiles (squares) on
                // the GOP, so what goes up is the damage and not the frame.
                //
                // Each box is blitted ON ITS OWN while there are few enough of
                // them to be worth the per-blit bookkeeping and their bounding
                // union is much bigger than they are. That union used to be the
                // one span, and for the two boxes a toolkit really sends -- a
                // menu here, the shadow it dropped over there -- it is most of
                // the screen: at the 42 MB/s this panel's aperture writes at
                // (see `la-basura-del-dibujado` measurements) a 1920x1080 span is
                // ~99 ms, so a 16x16 menu redraw cost a fifth of a second and
                // three dropped frames. Scanout expands each span to 64-byte WC
                // lines so a partial store cannot smear neighbouring pixels.
                //
                // An oversized, zero, or unreadable clip list means "the whole
                // frame is dirty" (true DIRTYFB semantics for num_clips == 0).
                // Linux accepts up to 256 clips (`DRM_MODE_FB_DIRTY_MAX_CLIPS`);
                // 64 made dense-damage frames fall back to a full-screen blit.
                const MAX_DIRTY_CLIPS: u32 = 256;
                const MAX_DIRTY_SPANS: usize = 8;
                let cmd = unsafe { *(data as *const DrmModeFbDirtyCmd) };
                // `drm_mode_dirtyfb_ioctl`, in its order: a flag it does not
                // define is EINVAL; the fb is looked up (ENOENT); a clip count
                // without a pointer, or a pointer without a count, is EINVAL;
                // ANNOTATE_COPY clips come in (src, dst) pairs, so an odd
                // count is EINVAL; more than DRM_MODE_FB_DIRTY_MAX_CLIPS is
                // EINVAL. None of it was read: a flush of a framebuffer that
                // does not exist, or with a clip list the kernel could not
                // have read, was reported as done.
                const DRM_MODE_FB_DIRTY_ANNOTATE_COPY: u32 = 0x01;
                const DRM_MODE_FB_DIRTY_ANNOTATE_FILL: u32 = 0x02;
                const DRM_MODE_FB_DIRTY_MAX_CLIPS: u32 = 256;
                if cmd.flags & !(DRM_MODE_FB_DIRTY_ANNOTATE_COPY | DRM_MODE_FB_DIRTY_ANNOTATE_FILL)
                    != 0
                {
                    return Err(FsError::InvalidParam);
                }
                if drm::get_fb(cmd.fb_id).is_none() {
                    return Err(FsError::EntryNotFound);
                }
                if (cmd.num_clips == 0) != (cmd.clips_ptr == 0) {
                    return Err(FsError::InvalidParam);
                }
                if cmd.flags & DRM_MODE_FB_DIRTY_ANNOTATE_COPY != 0 && cmd.num_clips % 2 != 0 {
                    return Err(FsError::InvalidParam);
                }
                if cmd.num_clips > DRM_MODE_FB_DIRTY_MAX_CLIPS {
                    return Err(FsError::InvalidParam);
                }
                let mut spans = [(0u32, 0u32, 0u32, 0u32); MAX_DIRTY_SPANS];
                let mut n = 0usize;
                let mut area = 0u64;
                let mut too_many = false;
                let rect = if cmd.num_clips > 0
                    && cmd.num_clips <= MAX_DIRTY_CLIPS
                    && cmd.clips_ptr != 0
                {
                    ucheck_n::<DrmClipRect>(cmd.clips_ptr as usize, cmd.num_clips as usize)?;
                    let mut union: Option<(u32, u32, u32, u32)> = None;
                    for i in 0..cmd.num_clips as usize {
                        let clip = unsafe { *(cmd.clips_ptr as *const DrmClipRect).add(i) };
                        if clip.x2 <= clip.x1 || clip.y2 <= clip.y1 {
                            continue;
                        }
                        let (x1, y1, x2, y2) = (
                            clip.x1 as u32,
                            clip.y1 as u32,
                            clip.x2 as u32,
                            clip.y2 as u32,
                        );
                        if n < MAX_DIRTY_SPANS {
                            spans[n] = (x1, y1, x2 - x1, y2 - y1);
                            n += 1;
                            area += (x2 - x1) as u64 * (y2 - y1) as u64;
                        } else {
                            too_many = true;
                        }
                        union = Some(match union {
                            Some((ux, uy, uw, uh)) => {
                                let nx = ux.min(x1);
                                let ny = uy.min(y1);
                                let fx = (ux + uw).max(x2);
                                let fy = (uy + uh).max(y2);
                                (nx, ny, fx - nx, fy - ny)
                            }
                            None => (x1, y1, x2 - x1, y2 - y1),
                        });
                    }
                    union
                } else {
                    None
                };
                // One box is one box either way, so the boxes only win when there
                // are several of them and they really are far apart: half the
                // union or less. Overlapping or adjacent boxes go up as the union,
                // where they cost one blit instead of several that copy the same
                // pixels twice.
                let by_box = match rect {
                    Some((_, _, uw, uh)) if n > 1 && !too_many => {
                        area.saturating_mul(2) <= uw as u64 * uh as u64
                    }
                    _ => false,
                };
                #[cfg(test)]
                DIRTY_SPANS_BLITTED.store(if by_box { n } else { 1 }, Ordering::Relaxed);
                let presented = if by_box {
                    // Every box, even if one of them cannot be scanned out: they
                    // are separate pieces of damage and dropping the rest because
                    // the first failed would leave the screen half updated.
                    let mut ok = true;
                    for span in spans.iter().take(n) {
                        ok &= drm::present_now_region(cmd.fb_id, 1, Some(*span));
                    }
                    ok
                } else {
                    drm::present_now_region(cmd.fb_id, 1, rect)
                };
                if !presented {
                    // Best-effort: a damage flush that can't scan out (e.g. the
                    // fb id is unknown to the software path) is not fatal — the
                    // client keeps its shadow and will re-present. Returning EIO
                    // here made Xorg's modesetting shadow abort its frame loop,
                    // so swallow it rather than failing every DirtyFB.
                    log::debug!("[drm] DIRTYFB fb={} not presented (no-op)", cmd.fb_id);
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_GETGAMMA | DRM_IOCTL_MODE_SETGAMMA => {
                // Both `drm_mode_gamma_{get,set}_ioctl` look the CRTC up
                // first and answer ENOENT for one that does not exist.
                let lut = unsafe { &*(data as *const DrmModeCrtcLut) };
                if drm::get_crtc(lut.crtc_id).is_none() {
                    return Err(FsError::EntryNotFound);
                }
                // The CRTC has no gamma store (GETCRTC reports `gamma_size`
                // 0: no programmable gamma on this scanout), and Linux says
                // so: `drm_crtc_supports_legacy_gamma` is false, so SETGAMMA
                // is ENOSYS, and GETGAMMA wants the caller's `gamma_size` to
                // be the CRTC's (EINVAL) and then copies that many entries,
                // none. Both answered "done" whatever was asked, so `xrandr
                // --gamma`, gammastep and Xorg's own gamma restore were told
                // the ramp was set, and a GETGAMMA of 256 entries returned
                // without writing one, leaving the caller to read its own
                // uninitialised buffers as the current ramp.
                if cmd == DRM_IOCTL_MODE_SETGAMMA {
                    return Err(FsError::NotSupported);
                }
                if lut.gamma_size != CRTC_GAMMA_SIZE {
                    return Err(FsError::InvalidParam);
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_LIST_LESSEES => {
                // No leases exist; report an empty list. The struct's first u32
                // is `count_lessees` — zero it so the client copies out none.
                unsafe {
                    *(data as *mut u32) = 0;
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_OBJ_SETPROPERTY => {
                let req = unsafe { *(data as *const DrmModeObjSetProperty) };
                self.set_object_property(req.obj_id, req.obj_type, req.prop_id, req.value)
            }
            DRM_IOCTL_MODE_SETPROPERTY => {
                // `struct drm_mode_connector_set_property { u64 value; u32
                // prop_id; u32 connector_id; }`: `drm_connector_property_set_ioctl`
                // is `drm_mode_obj_set_property_ioctl` with the connector type.
                let value = unsafe { *(data as *const u64) };
                let (prop_id, connector_id) = unsafe {
                    (
                        *(data.wrapping_add(8) as *const u32),
                        *(data.wrapping_add(12) as *const u32),
                    )
                };
                self.set_object_property(connector_id, DRM_MODE_OBJECT_CONNECTOR, prop_id, value)
            }
            DRM_IOCTL_MODE_CURSOR | DRM_IOCTL_MODE_CURSOR2 => {
                // Kernel-composited hardware cursor. wlroots is forced onto the
                // legacy KMS path (atomic is rejected), so it drives the pointer
                // with these ioctls; `scanout()` draws the bitmap over each
                // frame. The `drm_mode_cursor2` layout begins with the same 28
                // bytes as `drm_mode_cursor` (flags, crtc_id, x, y, width,
                // height, handle) and only appends hot_x/hot_y — which we don't
                // need for drawing, since the compositor pre-adjusts x/y for the
                // hotspot — so one 28-byte view serves both ioctls.
                #[repr(C)]
                struct DrmModeCursor {
                    flags: u32,
                    crtc_id: u32,
                    x: i32,
                    y: i32,
                    width: u32,
                    height: u32,
                    handle: u32,
                }
                const DRM_MODE_CURSOR_BO: u32 = 0x01;
                const DRM_MODE_CURSOR_MOVE: u32 = 0x02;
                const DRM_MODE_CURSOR_FLAGS: u32 = DRM_MODE_CURSOR_BO | DRM_MODE_CURSOR_MOVE;
                let cur = unsafe { &*(data as *const DrmModeCursor) };
                // `drm_mode_cursor_common` reads the flags first: no flag at
                // all, or one it does not know, is EINVAL. This arm answered
                // success having done nothing, so a client whose request
                // named no operation was never told.
                if cur.flags == 0 || cur.flags & !DRM_MODE_CURSOR_FLAGS != 0 {
                    return Err(FsError::InvalidParam);
                }
                // Then it finds the CRTC: "Unknown CRTC ID" is ENOENT. The
                // id was not looked at, so a cursor aimed at a CRTC that does
                // not exist moved the one that does.
                if drm::get_crtc(cur.crtc_id).is_none() {
                    return Err(FsError::EntryNotFound);
                }
                let mut changed = false;
                if cur.flags & DRM_MODE_CURSOR_BO != 0 {
                    // Linux wraps the handle in a framebuffer of `width x
                    // height` (`drm_internal_framebuffer_create`): a zero
                    // width or height is "bad framebuffer" EINVAL there, and
                    // a cursor larger than DRM_CAP_CURSOR_WIDTH/HEIGHT is
                    // EINVAL too. Nothing else bounds the bitmap the kernel
                    // copies and composites on every frame. This arm read a
                    // zero as "hide the pointer", which only a handle of 0
                    // means.
                    if cur.handle != 0
                        && (cur.width == 0
                            || cur.height == 0
                            || cur.width > drm::MAX_CURSOR_DIM
                            || cur.height > drm::MAX_CURSOR_DIM)
                    {
                        return Err(FsError::InvalidParam);
                    }
                    // Linux builds a framebuffer over the handle
                    // (`drm_mode_cursor_universal`): a handle this file does
                    // not hold is ENOENT (`drm_gem_object_lookup`), and a
                    // buffer too small for `width x height` 32-bit pixels is
                    // EINVAL (`drm_gem_fb_size_check`). Both were reported as
                    // success with the cursor quietly hidden, and the lookup
                    // was the unchecked one: another process's buffer could
                    // be shown as the pointer.
                    if cur.handle != 0 {
                        let Some((_, size)) =
                            drm::resolve_gem_backing_for(cur.handle, drm::current_pid())
                        else {
                            return Err(FsError::EntryNotFound);
                        };
                        let bytes = (cur.width as usize) * (cur.height as usize) * 4;
                        if size < bytes {
                            return Err(FsError::InvalidParam);
                        }
                    }
                    changed |= drm::set_cursor_bo(cur.handle, cur.width, cur.height);
                }
                if cur.flags & DRM_MODE_CURSOR_MOVE != 0 {
                    drm::move_cursor(cur.x, cur.y);
                    changed = true;
                }
                if changed {
                    drm::repaint_for_cursor();
                }
                Ok(0)
            }
            DRM_IOCTL_GEM_CLOSE => {
                let handle = unsafe { *(data as *const u32) };
                // linux-object's own CREATE_DUMB/PRIME table first; a miss
                // there might still be a driver-private handle (e.g.
                // nouveau-uAPI GEM_NEW) the driver itself keeps track of.
                // A nouveau GEM object's memory goes back to the RM inside
                // `nouveau_gem_close`, and nothing here holds a reference to
                // it, so any framebuffer still built on it has to be retired in
                // the same breath -- otherwise `crtc_fb` keeps pointing at
                // memory that now belongs to someone else. Dumb buffers are
                // deliberately NOT retired: their fb holds an `Arc` on the VMO
                // and outlives the handle, exactly as Linux does.
                let generic_closed = drm::gem_close(handle);
                let driver_closed = !generic_closed
                    && self
                        .driver()
                        .map(|d| d.nouveau_gem_close(handle, drm::current_pid()))
                        .unwrap_or(false);
                if driver_closed {
                    drm::retire_framebuffers_for_handle(handle);
                }
                if generic_closed || driver_closed {
                    // Drop the shared CPU-map cache entry; any live mmap Arc
                    // keeps its pin until munmap/Drop.
                    drm::nouveau_cpu_vmo_forget(handle);
                    // This open let go of the handle: a later PRIME import of
                    // the same buffer is a new reference again.
                    self.file.forget_prime_import(handle);
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_GETRESOURCES => {
                let res = unsafe { &mut *(data as *mut DrmModeCardRes) };
                if drm::is_compute_minor(self.minor) {
                    // Headless compute node: not a KMS card. drmIsKMS sees
                    // zero CRTCs/connectors and leaves this node alone, so
                    // labwc stays on card0 even without WLR_DRM_DEVICES.
                    res.count_fbs = 0;
                    res.count_crtcs = 0;
                    res.count_connectors = 0;
                    res.count_encoders = 0;
                    return Ok(0);
                }
                let (_, crtcs, connectors) = drm::get_resources();
                // The caller's framebuffers, as `drm_mode_getresources` walks
                // `file_priv->fbs`; the whole table went to everyone.
                let fbs = drm::framebuffer_ids_for(drm::current_pid());

                // Each list is filled as far as the caller made room and
                // reported at its full length (see `fill_id_list`); this arm
                // copied each one whole or not at all.
                fill_id_list(res.fb_id_ptr, res.count_fbs, &fbs)?;
                fill_id_list(res.crtc_id_ptr, res.count_crtcs, &crtcs)?;
                fill_id_list(res.connector_id_ptr, res.count_connectors, &connectors)?;

                res.count_fbs = fbs.len() as u32;
                res.count_crtcs = crtcs.len() as u32;
                res.count_connectors = connectors.len() as u32;

                // Always expose the synthetic encoder when connectors exist so
                // that `drmIsKMS` (which checks count_crtcs > 0 &&
                // count_connectors > 0 && count_encoders > 0) succeeds.  The
                // synthetic encoder is valid for both the software KMS path and
                // the hardware KMS path: `possible_crtcs = 1` maps to index 0
                // of the CRTC list, which is SYNTH_CRTC_ID on the software path
                // and the hardware CRTC on the hardware path.
                if !connectors.is_empty() {
                    fill_id_list(
                        res.encoder_id_ptr,
                        res.count_encoders,
                        &[drm::SYNTH_ENCODER_ID],
                    )?;
                    res.count_encoders = 1;
                } else {
                    res.count_encoders = 0;
                }

                if let Some(caps) = drm::get_caps() {
                    res.max_width = caps.max_width;
                    res.max_height = caps.max_height;
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_GETCONNECTOR => {
                let conn_res = unsafe { &mut *(data as *mut DrmModeGetConnector) };
                if let Some(conn) = drm::get_connector(conn_res.connector_id) {
                    conn_res.connection = if conn.connected { 1 } else { 2 };
                    // Physical dimensions, best source first:
                    //  1. the connector's own values (NVIDIA RM data);
                    //  2. the display's EDID — the preferred detailed timing
                    //     carries the image size in mm (bytes 66/67/68 of the
                    //     descriptor), and bytes 21/22 give cm as a coarser
                    //     fallback (observed: a 32" TV reported as 270x203mm by
                    //     the old 96-DPI guess vs its real 885x497mm, skewing
                    //     every DPI-aware client);
                    //  3. a ~96 DPI guess from the resolution, so wlroots never
                    //     sees "Physical size: 0x0".
                    let edid_mm = drm::get_connector_edid(conn_res.connector_id)
                        .and_then(|e| zcore_drivers::display::edid::physical_size_mm(&e));
                    let (fallback_w, fallback_h) = edid_mm.unwrap_or_else(|| {
                        drm::display_mode()
                            .map(|(w, h, _)| zcore_drivers::display::edid::estimated_size_mm(w, h))
                            .unwrap_or((1, 1))
                    });
                    conn_res.mm_width = if conn.mm_width > 0 {
                        conn.mm_width
                    } else {
                        fallback_w
                    };
                    conn_res.mm_height = if conn.mm_height > 0 {
                        conn.mm_height
                    } else {
                        fallback_h
                    };
                    // Real DRM_MODE_CONNECTOR_* from the driver (NVIDIA fills
                    // it from the RM's GET_CONNECTOR_DATA); synthetic and
                    // fallback connectors keep the historical 11.
                    conn_res.connector_type = conn.connector_type;
                    conn_res.connector_type_id = 1;
                    // `connector->encoder`: attached while the CRTC has a
                    // mode, 0 once `SETCRTC` disabled it (the connectors are
                    // detached with the mode).
                    conn_res.encoder_id = if drm::crtc_enabled() {
                        drm::SYNTH_ENCODER_ID
                    } else {
                        0
                    };
                    conn_res.subpixel = 0; // SubPixelUnknown

                    // Report exactly one encoder. wlroots calls this twice: once
                    // to learn the counts, then again with allocated arrays.
                    if conn_res.encoders_ptr != 0 && conn_res.count_encoders >= 1 {
                        ucheck_n::<u32>(conn_res.encoders_ptr as usize, 1)?;
                        unsafe {
                            *(conn_res.encoders_ptr as *mut u32) = drm::SYNTH_ENCODER_ID;
                        }
                    }
                    conn_res.count_encoders = 1;

                    // Report exactly one mode: the display's native resolution.
                    if let Some((w, h, _)) = drm::display_mode() {
                        if conn_res.modes_ptr != 0 && conn_res.count_modes >= 1 {
                            let mode = make_modeinfo(w, h);
                            ucheck(conn_res.modes_ptr as usize, mode.len())?;
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    mode.as_ptr(),
                                    conn_res.modes_ptr as *mut u8,
                                    mode.len(),
                                );
                            }
                        }
                        conn_res.count_modes = 1;
                    } else {
                        conn_res.count_modes = 0;
                    }
                    // Standard connector properties (DPMS, link-status,
                    // non-desktop, EDID, and CRTC_ID for atomic clients),
                    // filled as far as the caller made room, as
                    // `drm_mode_object_get_properties` does for this ioctl
                    // too (the modes and encoders above are all-or-nothing
                    // in Linux as well). This list was copied all or nothing.
                    let props = connector_props(conn_res.connector_id, self.file.atomic_client());
                    fill_prop_list(
                        conn_res.props_ptr,
                        conn_res.prop_values_ptr,
                        conn_res.count_props,
                        &props,
                    )?;
                    conn_res.count_props = props.len() as u32;
                    // klog (budget-shared with the wsi trace) so a black-screen
                    // bring-up shows, under LOG=error, whether the compositor
                    // saw a CONNECTED output with a usable mode: connected=false
                    // or modes=0 makes wlroots skip the output and never present.
                    if wsi_trace_take() {
                        kernel_hal::klog_info!(
                            "[drm] GETCONNECTOR id={} connected={} modes={} mode={:?}",
                            conn_res.connector_id,
                            conn.connected,
                            conn_res.count_modes,
                            drm::display_mode()
                        );
                    }
                    Ok(0)
                } else {
                    // The refusal itself is traced by the dispatch wrapper, on
                    // the channel a success storm cannot silence: this is the
                    // one that makes Mesa's wsi_display bail the whole
                    // VK_KHR_display query with OUT_OF_HOST_MEMORY.
                    // ENOENT, as `drm_mode_getconnector`'s lookup answers.
                    Err(FsError::EntryNotFound)
                }
            }
            DRM_IOCTL_MODE_GETENCODER => {
                let enc = unsafe { &mut *(data as *mut DrmModeGetEncoder) };
                // There is one encoder, the synthetic one GETRESOURCES and
                // GETCONNECTOR name; `drm_mode_getencoder` answers ENOENT for
                // any other id. This arm answered every id with that encoder,
                // rewriting the id the client asked about.
                if enc.encoder_id != drm::SYNTH_ENCODER_ID {
                    return Err(FsError::EntryNotFound);
                }
                // DRM_MODE_ENCODER_VIRTUAL=6: correct type for a software/
                // virtual encoder that drives a dumb-buffer scanout path.
                // Reporting NONE(0) causes some compositors to skip property
                // queries and misidentify the output type.
                enc.encoder_type = 6; // DRM_MODE_ENCODER_VIRTUAL
                                      // On the software KMS path the only CRTC is the synthetic one
                                      // (id=1). On the hardware KMS path, report crtc_id=0 (no
                                      // currently active CRTC) because CRTC 1 does not appear in the
                                      // resource list that hardware drivers expose; wlroots will
                                      // configure the CRTC itself via SETCRTC.
                                      // possible_crtcs=1 means bit 0 = index 0 of the CRTC list,
                                      // which is correct in both paths.
                                      // And `encoder->crtc` is NULL once the
                                      // CRTC was disabled: a compositor that
                                      // finds an encoder on a CRTC takes that
                                      // CRTC's mode as current.
                enc.crtc_id = if drm::software_kms_active() && drm::crtc_enabled() {
                    drm::SYNTH_CRTC_ID
                } else {
                    0
                };
                enc.possible_crtcs = 1; // bitmask: CRTC index 0
                enc.possible_clones = 0;
                Ok(0)
            }
            DRM_IOCTL_MODE_GETCRTC => {
                let crtc_res = unsafe { &mut *(data as *mut DrmModeGetCrtc) };
                if let Some(crtc) = drm::get_crtc(crtc_res.crtc_id) {
                    // `drm_mode_getcrtc`: the primary plane's fb, and the
                    // mode (the display's native timings, the only mode the
                    // pipeline has) only while `crtc_state->enable` -- a
                    // CRTC that `SETCRTC` disabled or whose scanout fb was
                    // removed has neither, whatever the panel can do. DPMS
                    // off keeps both. Compositors read this back to seed
                    // their initial output state.
                    let enabled = drm::crtc_enabled();
                    crtc_res.fb_id = if enabled { crtc.fb_id } else { 0 };
                    crtc_res.x = crtc.x;
                    crtc_res.y = crtc.y;
                    crtc_res.gamma_size = CRTC_GAMMA_SIZE;
                    match drm::display_mode() {
                        Some((w, h, _)) if enabled => {
                            crtc_res.mode = make_modeinfo(w, h);
                            crtc_res.mode_valid = 1;
                        }
                        _ => crtc_res.mode_valid = 0,
                    }
                    Ok(0)
                } else {
                    // ENOENT, as `drm_mode_getcrtc`'s lookup answers.
                    Err(FsError::EntryNotFound)
                }
            }
            DRM_IOCTL_MODE_GETPLANERESOURCES => {
                let res = unsafe { &mut *(data as *mut DrmModeGetPlaneRes) };
                // `drm_mode_getplane_res`: "unless userspace set the
                // 'universal planes' capability bit, only advertise
                // overlays". Every plane here is a primary, so a client that
                // never set the cap -- one written when the primary and the
                // cursor were not planes -- gets an empty list, not the
                // scanout plane to drive as an overlay. The cap was accepted
                // and ignored, and the list was the same for everyone.
                let universal = self.file.universal_planes();
                let planes: alloc::vec::Vec<u32> = drm::get_planes()
                    .into_iter()
                    .filter(|&id| {
                        universal
                            || drm::get_plane(id)
                                .is_some_and(|p| p.plane_type == DRM_PLANE_TYPE_OVERLAY)
                    })
                    .collect();
                // Linux fills as many ids as the caller made room for and
                // reports the full count; this arm filled all or nothing.
                fill_id_list(res.plane_id_ptr, res.count_planes, &planes)?;
                res.count_planes = planes.len() as u32;
                Ok(0)
            }
            DRM_IOCTL_MODE_GETPLANE => {
                let res = unsafe { &mut *(data as *mut DrmModeGetPlane) };
                if let Some(plane) = drm::get_plane(res.plane_id) {
                    res.crtc_id = plane.crtc_id;
                    res.fb_id = plane.fb_id;
                    res.possible_crtcs = plane.possible_crtcs;
                    // Advertise the formats the software scanout consumes, via
                    // the two-call pattern (count first, then fill).
                    const FORMATS: [u32; 2] = drm::SCANOUT_FORMATS;
                    if res.format_type_ptr != 0 && res.count_format_types >= FORMATS.len() as u32 {
                        ucheck_n::<u32>(res.format_type_ptr as usize, FORMATS.len())?;
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                FORMATS.as_ptr(),
                                res.format_type_ptr as *mut u32,
                                FORMATS.len(),
                            );
                        }
                    }
                    res.count_format_types = FORMATS.len() as u32;
                    Ok(0)
                } else {
                    // ENOENT, as `drm_mode_getplane`'s lookup answers.
                    Err(FsError::EntryNotFound)
                }
            }
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES => {
                let res = unsafe { &mut *(data as *mut DrmModeObjGetProperties) };
                // `drm_mode_obj_get_properties_ioctl`: the object is found
                // by id AND type (`drm_mode_object_find`; libdrm passes
                // obj_type=ANY, wlroots and Mutter the real type), ENOENT
                // otherwise; then the list is filled as far as the caller
                // made room and reported at its full length. Atomic
                // properties (FB_ID, CRTC_ID, ACTIVE, MODE_ID, rects) only
                // appear for atomic clients, like Linux's atomic filtering;
                // legacy clients keep seeing exactly the pre-atomic set. The
                // one encoder exists but carries no properties (Linux would
                // say EINVAL for an object without a property list; the
                // empty list stays because GETRESOURCES names this id and
                // some clients probe every id it returns). This arm ignored
                // obj_type -- a CRTC id asked about as a plane answered with
                // the CRTC's properties -- and copied the list all or
                // nothing.
                let atomic = self.file.atomic_client();
                let Some((_, props)) = find_mode_object(res.obj_id, res.obj_type, atomic) else {
                    return Err(FsError::EntryNotFound);
                };
                fill_prop_list(res.props_ptr, res.prop_values_ptr, res.count_props, &props)?;
                res.count_props = props.len() as u32;
                log::debug!(
                    "[drm] OBJ_GETPROPERTIES obj_id={} obj_type={:#x} -> {} props",
                    res.obj_id,
                    res.obj_type,
                    props.len()
                );
                Ok(0)
            }
            DRM_IOCTL_MODE_GETPROPERTY => {
                let res = unsafe { &mut *(data as *mut DrmModeGetProperty) };
                let spec = match prop_spec(res.prop_id) {
                    Some(s) => s,
                    None => return Err(FsError::EntryNotFound),
                };
                res.flags = spec.flags;
                let mut name = [0u8; 32];
                let n = spec.name.len().min(31);
                name[..n].copy_from_slice(&spec.name.as_bytes()[..n]);
                res.name = name;
                // Two-call pattern: report counts, fill when arrays are big
                // enough. Enum properties expose both the (value, name) pairs
                // and the raw value list; range/object properties only values.
                if !spec.enums.is_empty()
                    && res.enum_blob_ptr != 0
                    && (res.count_enum_blobs as usize) >= spec.enums.len()
                {
                    // `access_ok()`. These two arrays were the ONLY nested ioctl
                    // pointers in this file written without one, and this ioctl
                    // is unprivileged: a client passing a kernel address as
                    // `enum_blob_ptr`/`values_ptr` had the kernel write its enum
                    // records or property values straight to it. Linux copies
                    // both out with `copy_to_user()`, which faults to EFAULT.
                    ucheck_n::<DrmModePropertyEnum>(res.enum_blob_ptr as usize, spec.enums.len())?;
                    for (i, (val, nm)) in spec.enums.iter().enumerate() {
                        let mut e = DrmModePropertyEnum {
                            value: *val,
                            name: [0u8; 32],
                        };
                        let ln = nm.len().min(31);
                        e.name[..ln].copy_from_slice(&nm.as_bytes()[..ln]);
                        unsafe {
                            *(res.enum_blob_ptr as *mut DrmModePropertyEnum).add(i) = e;
                        }
                    }
                }
                res.count_enum_blobs = spec.enums.len() as u32;
                if !spec.values.is_empty()
                    && res.values_ptr != 0
                    && (res.count_values as usize) >= spec.values.len()
                {
                    ucheck_n::<u64>(res.values_ptr as usize, spec.values.len())?;
                    for (i, v) in spec.values.iter().enumerate() {
                        unsafe {
                            *(res.values_ptr as *mut u64).add(i) = *v;
                        }
                    }
                }
                res.count_values = spec.values.len() as u32;
                Ok(0)
            }
            DRM_IOCTL_MODE_GETPROPBLOB => {
                let res = unsafe { &mut *(data as *mut DrmModeGetBlob) };
                // Property-blob store first (user MODE_ID blobs, the kernel's
                // current-mode blob); EDID blobs keep their own reserved range
                // of ids below it (see `edid_blob_id`).
                if let Some(blob) = drm::get_blob(res.blob_id) {
                    if res.data != 0 && res.length >= blob.len() as u32 {
                        ucheck(res.data as usize, blob.len())?;
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                blob.as_ptr(),
                                res.data as *mut u8,
                                blob.len(),
                            );
                        }
                    }
                    res.length = blob.len() as u32;
                    return Ok(0);
                }
                let connector_id = connector_of_edid_blob(res.blob_id);
                if let Some(conn_id) = connector_id {
                    if let Some(edid) = drm::get_connector_edid(conn_id) {
                        if res.data != 0 && res.length >= edid.len() as u32 {
                            ucheck(res.data as usize, edid.len())?;
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    edid.as_ptr(),
                                    res.data as *mut u8,
                                    edid.len(),
                                );
                            }
                        }
                        res.length = edid.len() as u32;
                        Ok(0)
                    } else {
                        // The connector has no EDID, so no blob carries this
                        // id: ENOENT, as `drm_property_lookup_blob` answers.
                        Err(FsError::EntryNotFound)
                    }
                } else {
                    // No blob carries this id: ENOENT, as
                    // `drm_property_lookup_blob` answers. Both were EINVAL.
                    Err(FsError::EntryNotFound)
                }
            }
            DRM_IOCTL_MODE_CREATEPROPBLOB => {
                let req = unsafe { &mut *(data as *mut DrmModeCreateBlob) };
                // Linux: NULL data / zero length are EINVAL. Bound the copy —
                // real blobs (modes, gamma LUTs) are at most a few KiB.
                if req.data == 0 || req.length == 0 || req.length > 64 * 1024 {
                    return Err(FsError::InvalidParam);
                }
                ucheck(req.data as usize, req.length as usize)?;
                let src = unsafe {
                    core::slice::from_raw_parts(req.data as *const u8, req.length as usize)
                };
                // On this file's list, as `drm_mode_createblob_ioctl` puts
                // it on `file_priv->blobs`: only this open may destroy it,
                // and it goes when the open does.
                req.blob_id = drm::create_blob_owned(src.to_vec(), true, self.file_owner());
                log::debug!(
                    "[drm] CREATEPROPBLOB len={} -> blob={}",
                    req.length,
                    req.blob_id
                );
                Ok(0)
            }
            DRM_IOCTL_MODE_DESTROYPROPBLOB => {
                // struct drm_mode_destroy_blob { __u32 blob_id; }
                let blob_id = unsafe { *(data as *const u32) };
                match drm::destroy_blob(blob_id, self.file_owner()) {
                    drm::BlobDestroy::Destroyed => Ok(0),
                    drm::BlobDestroy::NotFound => Err(FsError::EntryNotFound),
                    // EPERM: "ensure the property was actually created by
                    // this user" (another open file's blob), and the kernel's
                    // own, which is on no file's list. Any client could free
                    // any other's, and the compositor's MODE_ID blob going
                    // away under it makes its next commit fail with ENOENT.
                    drm::BlobDestroy::KernelOwned | drm::BlobDestroy::NotOwner => {
                        Err(FsError::NotPermitted)
                    }
                }
            }
            DRM_IOCTL_MODE_ATOMIC => {
                let req = unsafe { &*(data as *const DrmModeAtomic) };
                // Linux contract: the client must have negotiated
                // DRM_CLIENT_CAP_ATOMIC (EINVAL otherwise), flags must be
                // known, reserved must be 0, TEST_ONLY cannot carry a flip
                // event, and async flips are refused when unsupported.
                if !self.file.atomic_client() {
                    return Err(FsError::InvalidParam);
                }
                if req.flags & !DRM_MODE_ATOMIC_FLAGS != 0 || req.reserved != 0 {
                    return Err(FsError::InvalidParam);
                }
                if req.flags & DRM_MODE_PAGE_FLIP_ASYNC != 0 {
                    // DRM_CAP_ASYNC_PAGE_FLIP / ATOMIC_ASYNC_PAGE_FLIP are 0.
                    return Err(FsError::InvalidParam);
                }
                let test_only = req.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0;
                let want_event = req.flags & DRM_MODE_PAGE_FLIP_EVENT != 0;
                let allow_modeset = req.flags & DRM_MODE_ATOMIC_ALLOW_MODESET != 0;
                if test_only && want_event {
                    return Err(FsError::InvalidParam);
                }
                if req.count_objs == 0 {
                    // Empty commit: Linux allows it, but not with an event.
                    // `prepare_signaling` refuses a `PAGE_FLIP_EVENT` with
                    // no CRTC in the state ("user mode pends on event which
                    // will never reach"); this answered 0 and queued nothing,
                    // and the client's `drmHandleEvent` then waited forever.
                    if want_event {
                        return Err(FsError::InvalidParam);
                    }
                    return Ok(0);
                }
                let mut upd = drm::AtomicUpdate::default();
                walk_atomic_props(req, |obj_id, prop_id, value| {
                    // [swapchain-diag] error!-visible at LOG=error: a wlroots
                    // "Swapchain failed test" can be an unknown/immutable prop
                    // rejected here, not just an atomic_commit check.
                    atomic_stage(&mut upd, obj_id, prop_id, value).inspect_err(|e| {
                        log::error!(
                            "[drm] ATOMIC stage rejected: obj={:#x} prop={:#x} value={:#x} -> {:?}",
                            obj_id,
                            prop_id,
                            value,
                            e
                        );
                    })
                })?;
                log::debug!(
                    "[drm] ATOMIC objs={} test_only={} allow_modeset={} event={} fb={:?} mode_blob={:?} active={:?}",
                    req.count_objs,
                    test_only,
                    allow_modeset,
                    want_event,
                    upd.plane_fb_id,
                    upd.mode_blob,
                    upd.active
                );
                let commit = drm::atomic_commit(
                    &upd,
                    test_only,
                    allow_modeset,
                    want_event,
                    req.user_data,
                    &self.file,
                );
                // OUT_FENCE_PTR writeback: Linux writes -1 on TEST_ONLY/failure
                // and a sync_file fd on success. Real out-fences need HW flip
                // completion; we install a signaled stub when possible.
                if let Some(ptr) = upd.out_fence_ptr {
                    if commit.is_err() || test_only {
                        let _ = write_out_fence_ptr(ptr, true);
                    } else {
                        write_out_fence_ptr(ptr, false)?;
                    }
                }
                commit.map_err(|e| match e {
                    drm::AtomicError::Invalid => FsError::InvalidParam,
                    drm::AtomicError::NotFound => FsError::EntryNotFound,
                    drm::AtomicError::Device => FsError::DeviceError,
                    drm::AtomicError::Busy => FsError::Busy,
                })?;
                Ok(0)
            }
            DRM_IOCTL_SYNCOBJ_CREATE => {
                if !zcore_drivers::display::nouveau_uapi_enabled() {
                    return Err(FsError::OpNotSupported);
                }
                let req = unsafe { &mut *(data as *mut DrmSyncobjCreate) };
                // `drm_syncobj_create_ioctl`: SIGNALED is the only flag.
                if req.flags & !DRM_SYNCOBJ_CREATE_SIGNALED != 0 {
                    return Err(FsError::InvalidParam);
                }
                // Owned by the calling process: given back when it dies
                // (`release_process`), and a `DESTROY` from any other
                // process is ENOENT.
                let handle = zcore_drivers::scheme::syncobj::create_for(
                    drm::current_pid(),
                    req.flags & DRM_SYNCOBJ_CREATE_SIGNALED != 0,
                );
                req.handle = handle;
                // Bounded probe (first N this boot), VISIBLE on hardware. Answers
                // the one thing the "failed to create timeline semaphore" spam
                // cannot: does NVK actually reach the kernel to create the syncobj
                // a timeline VkSemaphore is backed by? The create itself always
                // succeeds here, so a `SYNCOBJ_CREATE` line right before zink's
                // error proves the failure is DOWNSTREAM of the syncobj (NVK
                // internal / device already wedged), not in it. NO line before the
                // error means NVK never got here — it bailed in userspace (cf. the
                // GET_CAP probe above). And the pid + climbing count is the
                // accumulation signature if compositor respawns are leaking.
                {
                    static CREATE_LOG_BUDGET: core::sync::atomic::AtomicU32 =
                        core::sync::atomic::AtomicU32::new(0);
                    let n = CREATE_LOG_BUDGET.fetch_add(1, Ordering::Relaxed);
                    if n < 16 {
                        kernel_hal::klog_info!(
                            "[drm] SYNCOBJ_CREATE #{} pid={} -> handle={} flags={:#x}",
                            n + 1,
                            drm::current_pid(),
                            handle,
                            req.flags
                        );
                    }
                }
                Ok(0)
            }

            DRM_IOCTL_SYNCOBJ_DESTROY => {
                if !zcore_drivers::display::nouveau_uapi_enabled() {
                    return Err(FsError::OpNotSupported);
                }
                let req = unsafe { &*(data as *const DrmSyncobjDestroy) };
                // `drm_syncobj_destroy_ioctl`: "make sure padding is empty".
                if req.pad != 0 {
                    return Err(FsError::InvalidParam);
                }
                if zcore_drivers::scheme::syncobj::destroy_for(drm::current_pid(), req.handle) {
                    Ok(0)
                } else {
                    Err(FsError::EntryNotFound)
                }
            }

            DRM_IOCTL_SYNCOBJ_RESET | DRM_IOCTL_SYNCOBJ_SIGNAL => {
                if !zcore_drivers::display::nouveau_uapi_enabled() {
                    return Err(FsError::OpNotSupported);
                }
                let req = unsafe { &*(data as *const DrmSyncobjArray) };
                // `drm_syncobj_reset_ioctl` / `drm_syncobj_signal_ioctl`: the
                // padding first, then the count.
                if req.pad != 0 {
                    return Err(FsError::InvalidParam);
                }
                if req.count_handles == 0 {
                    return Err(FsError::InvalidParam);
                }
                syncobj_array_bound(req.count_handles)?;
                if req.handles == 0 {
                    return Err(FsError::InvalidParam);
                }
                ucheck_n::<u32>(req.handles as usize, req.count_handles as usize)?;
                let apply = if cmd == DRM_IOCTL_SYNCOBJ_RESET {
                    zcore_drivers::scheme::syncobj::reset
                } else {
                    zcore_drivers::scheme::syncobj::signal
                };
                // The whole array is looked up first, in the caller's own
                // handles (`drm_syncobj_array_find`): one it does not hold
                // is ENOENT and none of the others is touched.
                let handles: alloc::vec::Vec<u32> = (0..req.count_handles as usize)
                    .map(|i| unsafe { *(req.handles as *const u32).add(i) })
                    .collect();
                if !zcore_drivers::scheme::syncobj::all_usable_by(drm::current_pid(), &handles) {
                    return Err(FsError::EntryNotFound);
                }
                for handle in handles {
                    if !apply(handle) {
                        return Err(FsError::EntryNotFound);
                    }
                }
                let h0 = unsafe { *(req.handles as *const u32) };
                trace_syncobj(
                    if cmd == DRM_IOCTL_SYNCOBJ_RESET {
                        "RESET"
                    } else {
                        "SIGNAL"
                    },
                    drm::current_pid(),
                    h0,
                    0,
                    "ok",
                );
                Ok(0)
            }

            DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL => {
                if !zcore_drivers::display::nouveau_uapi_enabled() {
                    return Err(FsError::OpNotSupported);
                }
                let req = unsafe { &*(data as *const DrmSyncobjTimelineArray) };
                // `drm_syncobj_timeline_signal_ioctl`: no flags are defined.
                if req.flags != 0 {
                    return Err(FsError::InvalidParam);
                }
                if req.count_handles == 0 {
                    return Err(FsError::InvalidParam);
                }
                syncobj_array_bound(req.count_handles)?;
                if req.handles == 0 || req.points == 0 {
                    return Err(FsError::InvalidParam);
                }
                ucheck_n::<u32>(req.handles as usize, req.count_handles as usize)?;
                ucheck_n::<u64>(req.points as usize, req.count_handles as usize)?;
                // The caller's own handles, all of them, before any is
                // signaled (`drm_syncobj_array_find`).
                let handles: alloc::vec::Vec<u32> = (0..req.count_handles as usize)
                    .map(|i| unsafe { *(req.handles as *const u32).add(i) })
                    .collect();
                if !zcore_drivers::scheme::syncobj::all_usable_by(drm::current_pid(), &handles) {
                    return Err(FsError::EntryNotFound);
                }
                for (i, handle) in handles.into_iter().enumerate() {
                    let point = unsafe { *(req.points as *const u64).add(i) };
                    if !zcore_drivers::scheme::syncobj::timeline_signal(handle, point) {
                        return Err(FsError::EntryNotFound);
                    }
                }
                let (h0, p0) =
                    unsafe { (*(req.handles as *const u32), *(req.points as *const u64)) };
                trace_syncobj("TIMELINE_SIGNAL", drm::current_pid(), h0, p0, "ok");
                Ok(0)
            }

            DRM_IOCTL_SYNCOBJ_TRANSFER => {
                if !zcore_drivers::display::nouveau_uapi_enabled() {
                    return Err(FsError::OpNotSupported);
                }
                let req = unsafe { &*(data as *const DrmSyncobjTransfer) };
                // `drm_syncobj_transfer_ioctl`: the padding first, then
                // `drm_syncobj_find_fence`, for which WAIT_FOR_SUBMIT is the
                // only flag. Neither field was read.
                if req.pad != 0 {
                    return Err(FsError::InvalidParam);
                }
                if req.flags & !DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT != 0 {
                    return Err(FsError::InvalidParam);
                }
                // Both ends must be the caller's (`drm_syncobj_find` on each).
                if !zcore_drivers::scheme::syncobj::all_usable_by(
                    drm::current_pid(),
                    &[req.dst_handle, req.src_handle],
                ) {
                    return Err(FsError::EntryNotFound);
                }
                // Without WAIT_FOR_SUBMIT the source has to carry a fence at
                // `src_point` already: `dma_fence_chain_find_seqno` is EINVAL
                // for a point nothing has submitted, and a syncobj with no
                // fence at all is EINVAL too (point 0 names the fence there
                // is). With the flag Linux waits for the submission, which is
                // what a transfer deferred on its source does here. Every
                // transfer was deferred, so a client that Linux refuses got a
                // destination that would signal whenever the source did.
                if req.flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT == 0 {
                    let submitted = zcore_drivers::scheme::syncobj::query_submitted(req.src_handle)
                        .unwrap_or(0);
                    if submitted < req.src_point.max(1) {
                        trace_syncobj(
                            "TRANSFER",
                            drm::current_pid(),
                            req.src_handle,
                            req.src_point,
                            "source not submitted (EINVAL)",
                        );
                        return Err(FsError::InvalidParam);
                    }
                }
                let ok = zcore_drivers::scheme::syncobj::transfer(
                    req.dst_handle,
                    req.dst_point,
                    req.src_handle,
                    req.src_point,
                );
                trace_syncobj(
                    "TRANSFER",
                    drm::current_pid(),
                    req.dst_handle,
                    req.dst_point,
                    if ok { "ok" } else { "ENOENT" },
                );
                if ok {
                    Ok(0)
                } else {
                    Err(FsError::EntryNotFound)
                }
            }

            DRM_IOCTL_SYNCOBJ_QUERY => {
                if !zcore_drivers::display::nouveau_uapi_enabled() {
                    return Err(FsError::OpNotSupported);
                }
                let req = unsafe { &*(data as *const DrmSyncobjTimelineArray) };
                // `drm_syncobj_query_ioctl`: LAST_SUBMITTED is the only flag.
                if req.flags & !DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0 {
                    return Err(FsError::InvalidParam);
                }
                if req.count_handles == 0 {
                    return Err(FsError::InvalidParam);
                }
                syncobj_array_bound(req.count_handles)?;
                if req.handles == 0 || req.points == 0 {
                    return Err(FsError::InvalidParam);
                }
                let last_submitted = req.flags & DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0;
                // LAST_SUBMITTED must include in-flight EXEC fences: the fast
                // path returns before the GPU writes the landing zone, and NVK
                // uses this query as the timeline value of that submit. Treating
                // it as a no-op left the timeline at the previous signaled
                // point — Mesa then walked `supported_sync_types` off the NULL
                // terminator (`libvulkan_nouveau.so+0x9cc48`).
                ucheck_n::<u32>(req.handles as usize, req.count_handles as usize)?;
                ucheck_n::<u64>(req.points as usize, req.count_handles as usize)?;
                // The caller's own handles, all of them, before any point is
                // written back (`drm_syncobj_array_find`).
                let handles: alloc::vec::Vec<u32> = (0..req.count_handles as usize)
                    .map(|i| unsafe { *(req.handles as *const u32).add(i) })
                    .collect();
                if !zcore_drivers::scheme::syncobj::all_usable_by(drm::current_pid(), &handles) {
                    return Err(FsError::EntryNotFound);
                }
                let mut first_pt = 0u64;
                for (i, handle) in handles.into_iter().enumerate() {
                    let looked_up = if last_submitted {
                        zcore_drivers::scheme::syncobj::query_submitted(handle)
                    } else {
                        zcore_drivers::scheme::syncobj::query(handle)
                    };
                    let Some(point) = looked_up else {
                        return Err(FsError::EntryNotFound);
                    };
                    if i == 0 {
                        first_pt = point;
                    }
                    unsafe { *(req.points as *mut u64).add(i) = point };
                }
                let h0 = unsafe { *(req.handles as *const u32) };
                trace_syncobj(
                    if last_submitted {
                        "QUERY_LAST_SUBMITTED"
                    } else {
                        "QUERY"
                    },
                    drm::current_pid(),
                    h0,
                    first_pt,
                    "ok",
                );
                Ok(0)
            }

            DRM_IOCTL_SYNCOBJ_WAIT
            | DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE
            | DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT
            | DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE => {
                if !zcore_drivers::display::nouveau_uapi_enabled() {
                    return Err(FsError::OpNotSupported);
                }
                // The request as the async sleeper read it (one reader, so
                // the two cannot disagree on the struct or on what is
                // refused). Nothing to wait for is answered at once, as
                // Linux does, without reading the array.
                let Some(SyncobjWaitReq {
                    timeline,
                    handles,
                    points,
                    timeout_nsec,
                    flags,
                }) = read_syncobj_wait(cmd, data)?
                else {
                    return Ok(0);
                };
                // The caller's own handles (`drm_syncobj_array_find`): a wait
                // on another process's syncobj is ENOENT, not a wait.
                if !zcore_drivers::scheme::syncobj::all_usable_by(drm::current_pid(), &handles) {
                    syncobj_wait_klog(timeline, &handles, "not the caller's handle (ENOENT)");
                    return Err(FsError::EntryNotFound);
                }
                // `timeout_nsec` is an ABSOLUTE CLOCK_MONOTONIC deadline (real
                // Linux semantics, confirmed against this kernel's own
                // `now_monotonic()` -> `kernel_hal::timer::timer_now()`), not a
                // relative duration -- treating it as relative would turn any
                // real userspace deadline (computed from `clock_gettime`) into
                // an effectively unbounded wait.
                let deadline_us = (timeout_nsec.max(0) as u64) / 1000;
                let wait_all = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0;
                let available_only = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0;
                trace_syncobj(
                    if timeline { "TIMELINE_WAIT" } else { "WAIT" },
                    drm::current_pid(),
                    handles.first().copied().unwrap_or(0),
                    points
                        .as_ref()
                        .and_then(|p| p.first().copied())
                        .unwrap_or(0),
                    &alloc::format!(
                        "timeout_ns={} flags={:#x} available={}",
                        timeout_nsec,
                        flags,
                        available_only
                    ),
                );
                let wait_fn = if available_only {
                    zcore_drivers::scheme::syncobj::wait_available
                } else {
                    zcore_drivers::scheme::syncobj::wait
                };
                match wait_fn(&handles, points.as_deref(), wait_all, deadline_us) {
                    zcore_drivers::scheme::syncobj::WaitOutcome::Signaled {
                        first_signaled_index,
                    } => {
                        if timeline {
                            let req = unsafe { &mut *(data as *mut DrmSyncobjTimelineWait) };
                            req.first_signaled = first_signaled_index;
                        } else {
                            let req = unsafe { &mut *(data as *mut DrmSyncobjWait) };
                            req.first_signaled = first_signaled_index;
                        }
                        Ok(0)
                    }
                    // Real Linux returns -ETIME on a wait deadline, and Mesa
                    // checks for it EXACTLY (`errno == ETIME` -> `VK_TIMEOUT`;
                    // anything else -> lost device). Returning EAGAIN here (the
                    // old behaviour) was doubly wrong: libdrm's drmIoctl()
                    // retries EAGAIN, so a caller whose deadline already passed
                    // spins this arm in a tight busy-loop instead of seeing a
                    // timeout, and NVK never gets its VK_TIMEOUT. `FsError::TimedOut`
                    // now maps to `LxError::ETIME` (62).
                    zcore_drivers::scheme::syncobj::WaitOutcome::Timeout => {
                        syncobj_wait_klog(timeline, &handles, "timeout (ETIME to caller)");
                        Err(FsError::TimedOut)
                    }
                    zcore_drivers::scheme::syncobj::WaitOutcome::Invalid => {
                        syncobj_wait_klog(timeline, &handles, "unknown handle (ENOENT to caller)");
                        Err(FsError::EntryNotFound)
                    }
                }
            }

            _ => {
                // Reverse-engineering hook: log every DRM ioctl wlroots/labwc
                // issues that we do not handle. The DRM nr (`(cmd >> 8) & 0xff`)
                // maps to a DRM_IOCTL_* command, so a photo of this line tells us
                // exactly what labwc wants next. `dir` 1=W 2=R 3=RW, `size` is the
                // arg struct length.
                let nr = cmd & 0xff;
                let size = (cmd >> 16) & 0x3fff;
                let dir = cmd >> 30;
                log::debug!(
                    "[drm] UNHANDLED ioctl cmd={:#010x} (drm nr={:#04x} size={} dir={})",
                    cmd,
                    nr,
                    size,
                    dir
                );
                if let Some(driver) = self.driver() {
                    driver
                        .ioctl_owned(cmd, data, drm::current_pid())
                        // Map the driver's errno through instead of folding it
                        // all into DeviceError(EIO): mesa distinguishes them
                        // (e.g. `nouveau_ws_context_killed` tests specifically
                        // for -ENODEV), and a driver that carefully returns
                        // ENODEV/EINVAL/EBUSY only for userspace to see EIO
                        // makes every one of those messages a lie.
                        .map_err(|e| match e {
                            2 => FsError::EntryNotFound, // ENOENT
                            12 => FsError::NoMemory,     // ENOMEM
                            16 => FsError::Busy,         // EBUSY
                            19 => FsError::NoDevice,     // ENODEV
                            22 => FsError::InvalidParam, // EINVAL
                            38 => FsError::NotSupported, // ENOSYS
                            95 => FsError::NotSupported, // EOPNOTSUPP
                            _ => FsError::DeviceError,
                        })
                } else if is_core_drm_nr(nr) {
                    // Linux answers an unknown CORE ioctl number with EINVAL
                    // (`drm_ioctl`: the `drm_ioctls[]` slot has no `.func`, so
                    // `retcode = -EINVAL`), never ENOSYS. Mesa and libdrm read
                    // the two differently: EINVAL is "this kernel does not have
                    // that call", which makes a client fall back, while ENOSYS
                    // leaks out of `drmIoctl` as an unexpected errno.
                    Err(FsError::InvalidParam)
                } else {
                    // Driver-private range with no driver behind the node.
                    Err(FsError::NotSupported)
                }
            }
        }
    }
}

/// `DRM_IOCTL_WAIT_VBLANK`, re-exported for `sys_ioctl`.
///
/// The blocking form of this one ioctl has to sleep, and `INode::io_control`
/// is synchronous — there is no way to yield from inside it. `sys_ioctl` runs
/// in the async syscall dispatcher, so it does the waiting there (see
/// [`DrmDev::wait_vblank_sleep`]) and lets the sync arm below fill in the
/// reply once the requested vblank has actually arrived.
pub const WAIT_VBLANK_IOCTL: u32 = DRM_IOCTL_WAIT_VBLANK;

/// Whether `cmd` is the DRM ioctl numbered `nr`, **whatever struct size it
/// encodes**.
///
/// The ioctl type byte of every DRM command is `'d'` (0x64) and the NR is the
/// low byte; the size is encoded too, and that is exactly what must NOT be
/// matched on. Linux's `drm_ioctl()` selects the handler by NR alone and
/// reconciles the size afterwards, which is why a struct can grow a trailing
/// field without breaking older or newer userspace. Pinning a constant to one
/// size has already cost this tree three separate bugs found only on real
/// hardware (`SYNCOBJ_HANDLE_TO_FD` at 24 bytes, the deadline sizes of
/// `SYNCOBJ_WAIT`/`TIMELINE_WAIT`, and `PRIME_*`), each landing as a fall-
/// through to the driver and an ENOSYS the client read as a lost device.
///
/// `min_size` is the floor the *caller* needs: `sys_ioctl`'s pre-dispatch
/// helpers read the request struct themselves, so a command encoding fewer
/// bytes than they parse must not reach them -- it falls through to the normal
/// path, where [`INode::io_control`] zero-pads it the way Linux does.
pub fn is_drm_ioctl_nr(cmd: u32, nr: u32, min_size: usize) -> bool {
    ((cmd >> 8) & 0xff) == 0x64
        && (cmd & 0xff) == nr
        && (((cmd >> 16) & 0x3fff) as usize) >= min_size
}

/// `DRM_IOCTL_*` numbers `sys_ioctl` intercepts before the inode dispatch,
/// with the byte count each of its helpers parses. See [`is_drm_ioctl_nr`].
pub mod nr {
    /// `struct drm_wait_vblank` (24 B, frozen).
    pub const WAIT_VBLANK: (u32, usize) = (0x3A, 24);
    /// `struct drm_mode_atomic` (56 B, frozen).
    pub const MODE_ATOMIC: (u32, usize) = (0xBC, 56);
    /// `DRM_IOCTL_PRIME_HANDLE_TO_FD` (export), `struct drm_prime_handle`
    /// (12 B, frozen). `drm.h`: `DRM_IOWR(0x2d, struct drm_prime_handle)`.
    /// These two were filed the other way round for a long time, which is
    /// why the export/import arm in `linux-syscall` stopped trusting the
    /// number and read the operation off the struct instead -- see
    /// [`super::prime_request`].
    pub const PRIME_HANDLE_TO_FD: (u32, usize) = (0x2D, 12);
    /// `DRM_IOCTL_PRIME_FD_TO_HANDLE` (import), `struct drm_prime_handle`
    /// (12 B, frozen). `drm.h`: `DRM_IOWR(0x2e, struct drm_prime_handle)`.
    pub const PRIME_FD_TO_HANDLE: (u32, usize) = (0x2E, 12);
    /// `struct drm_mode_create_lease` (24 B).
    pub const MODE_CREATE_LEASE: (u32, usize) = (0xC6, 24);
    /// `struct drm_syncobj_eventfd` (24 B).
    pub const SYNCOBJ_EVENTFD: (u32, usize) = (0xCF, 24);
    /// `struct drm_mode_crtc` (104 B, frozen).
    pub const MODE_SETCRTC: (u32, usize) = (0xA2, 104);
    /// `struct drm_mode_crtc_page_flip` (24 B, frozen).
    pub const MODE_PAGE_FLIP: (u32, usize) = (0xB0, 24);
}

/// `struct drm_prime_handle`, the argument of both PRIME ioctls:
/// `{ __u32 handle; __u32 flags; __s32 fd; }`. On an export `handle` and
/// `flags` are read and `fd` is written; on an import `fd` is read and
/// `handle` is written. The field the ioctl does not read is whatever the
/// caller left there.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmPrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}

/// `DRM_CLOEXEC`, the one flag an export may carry besides [`DRM_RDWR`].
pub const DRM_CLOEXEC: u32 = 0o2000000;
/// `DRM_RDWR`: the dma-buf fd is to be opened read-write.
pub const DRM_RDWR: u32 = 0o2;

/// What a PRIME ioctl asks for, decided the way `drm_ioctl` decides it: by
/// the ioctl NUMBER. `PRIME_HANDLE_TO_FD` is an export of `handle`, whatever
/// the caller left in `fd`; `PRIME_FD_TO_HANDLE` is an import of `fd`,
/// whatever it left in `handle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimeRequest {
    /// `PRIME_HANDLE_TO_FD`: wrap GEM `handle` in a new dma-buf fd.
    Export { handle: u32, flags: u32 },
    /// `PRIME_FD_TO_HANDLE`: the dma-buf behind `fd`, as a GEM handle.
    Import { fd: i32 },
}

/// Reads a PRIME request the way Linux does. `cmd` picks the operation (the
/// caller has already matched it against [`nr::PRIME_HANDLE_TO_FD`] and
/// [`nr::PRIME_FD_TO_HANDLE`], so anything else is `ENOTTY`), then each
/// operation checks only the fields it reads: an export refuses a flag
/// other than `DRM_CLOEXEC | DRM_RDWR` with `EINVAL`
/// (`drm_prime_handle_to_fd_ioctl`), an import of a negative fd is `EBADF`
/// (`dma_buf_get`).
///
/// The operation used to be read off the STRUCT instead: `fd < 0` meant an
/// export, because libdrm's `drmPrimeHandleToFD` presets the output field to
/// -1. That was a workaround for the two NRs being filed the wrong way
/// round in [`nr`], and it broke every exporter that does not preset the
/// field -- a `drm_prime_handle` zeroed by the caller (`fd = 0`) was taken
/// for an import of fd 0, so the export answered `EINVAL` (stdin is not a
/// dma-buf), or imported whatever dma-buf the client happened to hold at
/// fd 0. Linux never looks at `fd` on an export: it is an output.
pub fn prime_request(
    cmd: u32,
    args: DrmPrimeHandle,
) -> core::result::Result<PrimeRequest, LxError> {
    let (export, export_min) = nr::PRIME_HANDLE_TO_FD;
    let (import, import_min) = nr::PRIME_FD_TO_HANDLE;
    if is_drm_ioctl_nr(cmd, export, export_min) {
        if args.flags & !(DRM_CLOEXEC | DRM_RDWR) != 0 {
            return Err(LxError::EINVAL);
        }
        Ok(PrimeRequest::Export {
            handle: args.handle,
            flags: args.flags,
        })
    } else if is_drm_ioctl_nr(cmd, import, import_min) {
        if args.fd < 0 {
            return Err(LxError::EBADF);
        }
        Ok(PrimeRequest::Import { fd: args.fd })
    } else {
        Err(LxError::ENOTTY)
    }
}

/// How long a pre-present fence wait may hold the frame.
///
/// Bounded on purpose, and the bound is shared by both waits ([`DrmDev::
/// atomic_in_fence_sleep`] and [`DrmDev::present_fence_sleep`]) because the
/// trade-off is identical. Linux waits on a pre-flip fence indefinitely, but a
/// fence that never signals must not freeze the desktop: presenting a frame
/// early is a visible glitch, presenting nothing ever is a hang. 100 ms is
/// several frames at any refresh rate we drive, so a fence that misses it is
/// broken rather than slow -- and the cost of being wrong is a stutter.
const PRESENT_FENCE_TIMEOUT_US: u64 = 100_000;

/// How long the async pre-wait for `GEM_CPU_PREP` may sleep.
///
/// The driver's own `CPU_PREP_TIMEOUT_US`, so this sleep can never outlast
/// the wait it stands in for: past it the sync arm answers EBUSY, and a
/// pre-wait still parked there would hold the caller for nothing. Unlike the
/// present fence's bound this is not a tearing/hang trade-off -- it is a
/// mirror, and it belongs next to the driver's number if that one ever moves
/// (a longer one here would stall, a shorter one just hands the tail back to
/// the spin this replaces).
const CPU_PREP_TIMEOUT_US: u64 = 10_000_000;

/// How often a legacy present says what it waited for: the first two of the
/// boot, then on the present cost report's own rhythm.
///
/// The first two answer the question that has no other answer -- whether the
/// implicit-sync wait finds anything to wait on at all. After that it borrows
/// [`drm::FULL_FRAME_REPORT_EVERY`] rather than picking its own number, so the
/// two lines about the same present land together in the klog and the wait can
/// be read against the blit it precedes. The klog writes synchronously to the
/// UART, so a line per frame would be a stutter of its own.
const FENCE_REPORT_EVERY: u64 = drm::FULL_FRAME_REPORT_EVERY;

/// Legacy presents that reached the fence wait with the nouveau uAPI on.
static FENCE_PRESENTS: AtomicU64 = AtomicU64::new(0);

/// Whether the `n`-th present's fence outcome gets a line in the klog.
///
/// No guard against `every == 0`, and not because a zero rhythm cannot arrive:
/// `is_multiple_of(0)` is `false` for every non-zero `n` and `true` only for
/// `0`, so a zero rhythm already reports the opening two presents and then goes
/// quiet, and `n == 0` is already covered by the first arm. A `every != 0 &&`
/// in front of it was a mutant that could not be killed, because it cannot
/// change the answer for any input at all.
fn fence_report_decision(n: u64, every: u64) -> bool {
    n <= 2 || n.is_multiple_of(every)
}

/// When a bounded fence poll should wake for its next probe, or `None` when
/// the deadline leaves no time to sleep and the caller must give up.
///
/// The clamp to `deadline` is what makes the bound a bound: sleeping a whole
/// tick past it turns a 100 ms cap into 101 ms, every frame, on the path that
/// times out. The `None` at the boundary is what keeps a missed deadline from
/// becoming an unbounded sleep -- `sleep_until` takes an absolute instant, and
/// `wake <= now` is either "already expired" or a clock that stepped
/// backwards; both must end the wait, not park on a deadline in the past.
fn next_poll_wake(now: Duration, deadline: Duration, tick: Duration) -> Option<Duration> {
    let next = now + tick;
    let wake = if deadline < next { deadline } else { next };
    if wake <= now {
        None
    } else {
        Some(wake)
    }
}

/// Accounts one pre-wait to [`zcore_drivers::scheme::prewait`], however the
/// wait ends.
///
/// The five pre-waits park a thread BEFORE the driver is ever called, so none
/// of that time appears in the driver's per-ioctl profile -- which starts
/// timing at the dispatch, when the parking is already over. A frame that
/// spends most of itself parked therefore reads, in `/proc/gpudbg`, as a
/// table of small numbers and no account of where the frame went. This is
/// what closes that gap on the machine that has the GPU.
///
/// It accounts on DROP because these functions return from several places --
/// the request does not read, the condition is already met, the deadline
/// passed -- and a `record` at each of them is a record missed the next time
/// a return is added.
struct PreWaitAccount {
    kind: zcore_drivers::scheme::prewait::Kind,
    start: Duration,
    /// Times the loop woke, looked, and went back to sleep. Also what tells a
    /// wait that parked from one that was satisfied on its first look, since
    /// a coarse clock can read zero microseconds for a real park.
    probes: u32,
    timed_out: bool,
}

impl PreWaitAccount {
    fn new(kind: zcore_drivers::scheme::prewait::Kind) -> Self {
        Self {
            kind,
            start: kernel_hal::timer::timer_now(),
            probes: 0,
            timed_out: false,
        }
    }

    /// One more turn of the poll loop.
    fn probe(&mut self) {
        self.probes = self.probes.saturating_add(1);
    }

    /// This wait is ending on its deadline, not on the thing it waited for.
    fn timed_out(&mut self) {
        self.timed_out = true;
    }
}

/// The parked microseconds a pre-wait reports, from how long it was alive and
/// how many times it looked.
///
/// A wait satisfied on its first look was never parked, and the microseconds
/// it was alive for are the ones spent reading its argument struct -- charging
/// those to parking would put a floor under every line of the table and hide
/// the waits that really do park. The probe count, not the clock, is what
/// makes a park: a wait that woke, looked and slept again on a coarse clock
/// can honestly measure zero microseconds, and it still parked.
fn pre_wait_parked_us(elapsed_us: u64, probes: u32) -> u64 {
    if probes == 0 {
        0
    } else {
        elapsed_us
    }
}

impl Drop for PreWaitAccount {
    fn drop(&mut self) {
        // `as_micros` is a u128 and a plain `as u64` wraps. It takes a
        // clock 584 000 years old to overflow honestly, but a timer that
        // jumps does it in one read, and the counter is a sum: one wrapped
        // value poisons the column for the rest of the boot.
        let elapsed_us = core::convert::TryFrom::try_from(
            kernel_hal::timer::timer_now()
                .saturating_sub(self.start)
                .as_micros(),
        )
        .unwrap_or(u64::MAX);
        zcore_drivers::scheme::prewait::record(
            self.kind,
            pre_wait_parked_us(elapsed_us, self.probes),
            self.probes,
            self.timed_out,
        );
    }
}

/// Wait out one interval of a fence poll, on the schedule
/// [`zcore_drivers::scheme::syncobj::fence_poll_step`] sets: `probes` is how
/// many probes this wait has already made.
///
/// `false` means the deadline has arrived (or a clock stepped backwards) and
/// the caller must stop polling -- the same answer [`next_poll_wake`]'s `None`
/// carried when every one of these loops slept a flat millisecond.
///
/// The early probes yield instead of arming a timer. A GPU fence on this
/// hardware lands in far less than the old 1 ms tick, so the tick, not the
/// GPU, was setting the frame rate: see `fence_poll_step` for the whole
/// reasoning and for why the backoff ends up at that same tick.
async fn fence_poll_wait(probes: u32, deadline: Duration) -> LxResult<bool> {
    use zcore_drivers::scheme::syncobj::{fence_poll_step, PollStep};
    // Asked on every look, so the yield arm is interruptible too: it sleeps
    // for no time at all, but a client that spends a whole frame in the busy
    // phase would otherwise take no signal for that frame.
    crate::process::check_signals()?;
    match fence_poll_step(probes) {
        PollStep::Yield => {
            if kernel_hal::timer::timer_now() >= deadline {
                return Ok(false);
            }
            kernel_hal::thread::yield_now().await;
            Ok(true)
        }
        PollStep::Sleep { us } => {
            let Some(wake) = next_poll_wake(
                kernel_hal::timer::timer_now(),
                deadline,
                Duration::from_micros(us),
            ) else {
                return Ok(false);
            };
            // `interruptible`, not a bare `sleep_until`: a sleep is exactly
            // the abandonable future it documents, and a `^C` that lands in
            // the middle of one must not wait it out.
            crate::process::interruptible(kernel_hal::thread::sleep_until(wake)).await?;
            Ok(true)
        }
    }
}

/// Which legacy-KMS present ioctl `cmd` is, for the pre-present fence wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyPresent {
    /// `DRM_IOCTL_MODE_SETCRTC`: binds a fb to a CRTC, and presents it.
    SetCrtc,
    /// `DRM_IOCTL_MODE_PAGE_FLIP`.
    PageFlip,
}

/// The legacy present `cmd` asks for, or `None` for anything else.
///
/// Matched by ioctl NUMBER like every other pre-dispatch helper here, never by
/// the full 32-bit command: the size is encoded in it, and pinning one size is
/// how this tree has lost a wait to a struct that grew a field three times
/// already (see [`is_drm_ioctl_nr`]). The size floor is what *this* helper
/// parses, so a command encoding fewer bytes falls through to the sync arm
/// untouched instead of being read past its end.
pub fn legacy_present_kind(cmd: u32) -> Option<LegacyPresent> {
    let (crtc_nr, crtc_min) = nr::MODE_SETCRTC;
    if is_drm_ioctl_nr(cmd, crtc_nr, crtc_min) {
        return Some(LegacyPresent::SetCrtc);
    }
    let (flip_nr, flip_min) = nr::MODE_PAGE_FLIP;
    if is_drm_ioctl_nr(cmd, flip_nr, flip_min) {
        return Some(LegacyPresent::PageFlip);
    }
    None
}

/// The atomic-commit ioctl. Used by `sys_ioctl` to run
/// [`DrmDev::atomic_in_fence_sleep`] before `io_control`, so a commit
/// carrying an `IN_FENCE_FD` waits for the client's rendering to land before
/// the sync arm scans that buffer out.
pub const ATOMIC_IOCTL: u32 = DRM_IOCTL_MODE_ATOMIC;

/// How many `SETCRTC`/`SETPLANE` presents that put no pixels on the screen get
/// a console line before the trace goes quiet. Eight covers both buffers of a
/// double-buffered swapchain several times over — enough to tell a one-off from
/// a steady state — while staying far short of a per-frame flood. A storm on
/// this path is not hypothetical: the compositor log that prompted this retried
/// its modeset at ~8 Hz for minutes, and a console flood on a slow serial line
/// has wedged spinlocks here before (see the EXEC dedup in `nouveau_uapi.rs`).
const PRESENT_FAIL_TRACE_BUDGET: u32 = 8;
static PRESENT_FAIL_TRACED: AtomicU32 = AtomicU32::new(0);

/// Decide what a failed present means for the ioctl that asked for it, and say
/// so on the console.
///
/// A modeset is a *configuration* operation: `drm_mode_setcrtc` binds a fb to a
/// CRTC and programs a mode, and Linux fails it for a bad argument, never
/// because a frame could not be copied. Answering `EIO` because the blit did
/// not happen conflated the two, and wlroots' legacy backend reads that as the
/// output being broken — it retries the whole modeset next frame, forever,
/// never advancing to page-flips. The compositor log fills with
///
/// ```text
/// [backend/drm/legacy.c:123] connector HDMI-A-1: Failed to set CRTC: I/O error
/// ```
///
/// at frame rate while the kernel says nothing, because every reason inside the
/// present path is a `warn!` and the rig boots at `LOG=error`. A single
/// unpresentable frame took down the whole desktop.
///
/// So: a bad fb id keeps failing the ioctl, with the `ENOENT` Linux uses for it
/// ("Unknown FB ID") rather than `EIO` — that one is a real client error, and
/// the errno points at fb lifetime instead of at the bus. Everything else is
/// reported and swallowed: the CRTC takes the binding it was asked for, the
/// compositor keeps running, and the next present gets another chance. That is
/// the same call `DIRTYFB` already makes a few arms down, for the same reason.
fn present_failed(
    op: &str,
    fb_id: u32,
    crtc_id: u32,
    err: drm::PresentError,
) -> core::result::Result<(), FsError> {
    let n = PRESENT_FAIL_TRACED.fetch_add(1, Ordering::Relaxed);
    if n < PRESENT_FAIL_TRACE_BUDGET {
        // error!, not warn!: the rig boots at LOG=error, and this line is the
        // one that says why the screen is black. The reasons underneath are
        // warn!/klog and were invisible there.
        //
        // For a missing fb, say whether the id was one WE took away. A
        // nouveau-backed fb dies with its GEM handle (Linux's never does,
        // because there the fb holds its own reference), so "the client is
        // presenting an id it never had" and "the client is presenting the id
        // we pulled out from under it" both arrive here looking identical --
        // and they have opposite fixes.
        let taken_by = match err {
            drm::PresentError::NoSuchFb => drm::fb_retired_reason(fb_id),
            _ => None,
        };
        log::error!(
            "[drm] {} fb={} crtc={} did not present: {}{}{}{}",
            op,
            fb_id,
            crtc_id,
            err.as_str(),
            match taken_by {
                Some(why) => alloc::format!(" (this fb was retired by {})", why.as_str()),
                None => alloc::string::String::new(),
            },
            match err {
                drm::PresentError::NoSuchFb => " -> ENOENT to caller",
                _ => " -> reported OK to caller (the modeset stands; only this frame is lost)",
            },
            if n + 1 == PRESENT_FAIL_TRACE_BUDGET {
                " [further present failures not traced]"
            } else {
                ""
            },
        );
    }
    match err {
        drm::PresentError::NoSuchFb => Err(FsError::EntryNotFound),
        // EINVAL, as `drm_mode_setcrtc` answers for a framebuffer no plane
        // can scan out ("Invalid pixel format" / failed atomic check). The
        // CRTC is deliberately NOT bound to it: unlike the two below, this
        // framebuffer will never become presentable, so leaving `crtc_fb`
        // naming it would make every later repaint try it again.
        drm::PresentError::UnsupportedLayout => Err(FsError::InvalidParam),
        drm::PresentError::NoDisplay | drm::PresentError::NoBacking => {
            // We are about to answer 0, so the CRTC really is configured with
            // this fb and `GETCRTC` has to say so. `present_now_checked` binds
            // it on every path that succeeds and returns before binding on the
            // ones that do not, which would otherwise leave the readback
            // naming the previous frame's fb. Safe for both reasons that get
            // here: every consumer of `crtc_fb` -- `repaint_for_cursor` and
            // the next present -- re-checks the display and the fb's backing
            // before it touches a pixel.
            drm::set_crtc_fb(crtc_id, fb_id);
            Ok(())
        }
    }
}

/// True for any of the four `SYNCOBJ_WAIT` / `TIMELINE_WAIT` ioctl numbers
/// (classic + deadline-sized). Used by `sys_ioctl` to run
/// [`DrmDev::syncobj_wait_sleep`] before `io_control`.
pub fn is_syncobj_wait_ioctl(cmd: u32) -> bool {
    is_drm_ioctl_nr(cmd, NR_SYNCOBJ_WAIT, core::mem::size_of::<DrmSyncobjWait>())
        || is_syncobj_timeline_wait(cmd)
}

/// True for nouveau's `GEM_CPU_PREP`. Used by `sys_ioctl` to run
/// [`DrmDev::cpu_prep_sleep`] before `io_control`.
///
/// The number itself is the driver's, so the recogniser is too
/// ([`zcore_drivers::display::is_cpu_prep_ioctl`]) -- a copy of a
/// driver-private NR here is a copy that drifts. This exists only because
/// `linux-syscall` does not link `zcore-drivers`.
pub fn is_cpu_prep_ioctl(cmd: u32) -> bool {
    zcore_drivers::display::is_cpu_prep_ioctl(cmd)
}

/// ioctl NUMBERs of the two syncobj waits. Both structs grew a trailing
/// `deadline_nsec` in 2023 and nothing says they will not grow again, so
/// everything that has to recognise them matches on the number with a size
/// floor -- never on the whole 32-bit command, which carries the size. Pinning
/// to the sizes of the day is what silently dropped NVK's CPU_WAIT probe on
/// real hardware; see the note beside `DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE`.
const NR_SYNCOBJ_WAIT: u32 = 0xC3;
/// See [`NR_SYNCOBJ_WAIT`].
const NR_SYNCOBJ_TIMELINE_WAIT: u32 = 0xCA;

/// Whether `cmd` is the timeline wait rather than the classic one. The two
/// carry different structs, so this decides which one the async sleeper reads,
/// and it has to answer the same way the dispatcher does.
fn is_syncobj_timeline_wait(cmd: u32) -> bool {
    is_drm_ioctl_nr(
        cmd,
        NR_SYNCOBJ_TIMELINE_WAIT,
        core::mem::size_of::<DrmSyncobjTimelineWait>(),
    )
}

// DRM IOCTL numbers (Linux x86_64)
const DRM_IOCTL_VERSION: u32 = 0xC0406400;
const DRM_IOCTL_GET_UNIQUE: u32 = 0xC0106401;
const DRM_IOCTL_GET_MAGIC: u32 = 0xC0046402;
const DRM_IOCTL_AUTH_MAGIC: u32 = 0x40046411;
const DRM_IOCTL_GET_CAP: u32 = 0xC010640C;
const DRM_IOCTL_SET_CLIENT_CAP: u32 = 0x4010640D;
const DRM_IOCTL_GEM_CLOSE: u32 = 0x40086409;
/// `struct drm_client` is 40 bytes on LP64 (`_IOWR('d', 0x0A, …)`).
const DRM_IOCTL_GET_CLIENT: u32 = 0xC028640A;
const DRM_IOCTL_SET_MASTER: u32 = 0x0000641E;
const DRM_IOCTL_DROP_MASTER: u32 = 0x0000641F;

const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC04064A0;
const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC06864A1;
const DRM_IOCTL_MODE_SETCRTC: u32 = 0xC06864A2;
const DRM_IOCTL_MODE_GETENCODER: u32 = 0xC01464A6;
const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC05064A7;

const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC02064B2;
const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC01064B3;
const DRM_IOCTL_MODE_DESTROY_DUMB: u32 = 0xC00464B4;
const DRM_IOCTL_MODE_ADDFB: u32 = 0xC01C64AE;
const DRM_IOCTL_MODE_ADDFB2: u32 = 0xC06864B8;
const DRM_IOCTL_MODE_RMFB: u32 = 0xC00464AF;
/// `struct drm_mode_closefb { u32 fb_id; u32 pad; }` — Linux 6.6+. wlroots
/// prefers it over RMFB when tearing down framebuffers (CLOSEFB drops the
/// caller's reference WITHOUT disabling the plane/CRTC it may still be on);
/// with it unhandled every fb teardown logged "Failed to close FB" and fell
/// back to RMFB.
const DRM_IOCTL_MODE_CLOSEFB: u32 = 0xC00864D0;
const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC01864B0;

const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = 0xC01064B5;
const DRM_IOCTL_MODE_GETPLANE: u32 = 0xC02064B6;
const DRM_IOCTL_MODE_SETPLANE: u32 = 0xC03064B7;
const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u32 = 0xC02064B9;
const DRM_IOCTL_MODE_OBJ_SETPROPERTY: u32 = 0xC01864BA;
const DRM_IOCTL_MODE_GETPROPERTY: u32 = 0xC04064AA;
const DRM_IOCTL_MODE_GETPROPBLOB: u32 = 0xC01064AC;
// Legacy connector property setter (`drmModeConnectorSetProperty`), used by
// wlroots' legacy DRM path to drive the connector DPMS state to "on" during a
// modeset commit. `struct drm_mode_connector_set_property { __u64 value; __u32
// prop_id; __u32 connector_id; }` (16 bytes).
const DRM_IOCTL_MODE_SETPROPERTY: u32 = 0xC01064AB;

// Legacy cursor ioctls (`drmModeSetCursor`/`drmModeMoveCursor`/`...2`). On the
// software-KMS / pixman path there is no hardware cursor plane, so wlroots is
// told to use a software cursor (WLR_NO_HARDWARE_CURSORS=1) and normally never
// issues these. But if that env var is missing, wlroots' legacy backend calls
// drmModeSetCursor during a commit; returning an error (ENOTTY) failed the
// whole frame commit ("Failed to commit frame") and left the screen black.
// Accept them as no-ops so rendering proceeds regardless (the pointer is then
// only visible when the software-cursor path is used).
const DRM_IOCTL_MODE_CURSOR: u32 = 0xC01C64A3;
const DRM_IOCTL_MODE_CURSOR2: u32 = 0xC02464BB;

// Core (non-MODE) vblank wait.
const DRM_IOCTL_WAIT_VBLANK: u32 = 0xC018643A;
// Query an existing framebuffer object.
const DRM_IOCTL_MODE_GETFB: u32 = 0xC01C64AD;
const DRM_IOCTL_MODE_GETFB2: u32 = 0xC06864CE;
// Flush framebuffer damage to the display.
const DRM_IOCTL_MODE_DIRTYFB: u32 = 0xC01864B1;
// Legacy gamma LUT get/set (`struct drm_mode_crtc_lut`, 32 bytes). The Xorg
// modesetting driver reads the CRTC's gamma at startup (to restore on exit) and
// sets an identity ramp during modeset; ENOTTY here made it log an error and
// spin re-issuing it (the SETGAMMA flood on real hardware). The software scanout
// has no gamma hardware, so accept both as no-ops.
const DRM_IOCTL_MODE_GETGAMMA: u32 = 0xC02064A4;
const DRM_IOCTL_MODE_SETGAMMA: u32 = 0xC02064A5;
// Lease enumeration (`struct drm_mode_list_lessees`, 16 bytes). Xorg probes it
// while taking DRM master; there are never any leases here, so report zero.
const DRM_IOCTL_MODE_LIST_LESSEES: u32 = 0xC01064C7;
// Interface-version handshake (`drmSetInterfaceVersion`). The Xorg
// modesetting driver issues it right after open; ENOTTY fails its probe.
const DRM_IOCTL_SET_VERSION: u32 = 0xC0106407;

// Atomic modesetting (`drm-uapi.rst` "Atomic Mode Setting"): one-shot
// multi-object property commit, plus the property-blob objects it rides on
// (`MODE_ID` blobs are created/destroyed by the client per modeset).
const DRM_IOCTL_MODE_ATOMIC: u32 = 0xC03864BC;
const DRM_IOCTL_MODE_CREATEPROPBLOB: u32 = 0xC01064BD;
const DRM_IOCTL_MODE_DESTROYPROPBLOB: u32 = 0xC00464BE;

// DRM sync objects (`drm.h`): core, driver-independent -- their nr range
// (0xBF-0xCF) sits ABOVE `DRM_COMMAND_END` (0xA0), unlike driver-private
// ioctls, so unlike e.g. the nouveau-uAPI numbers these are never offset by
// `DRM_COMMAND_BASE`. See `zcore_drivers::scheme::syncobj` for the actual
// state (lives in `drivers` so a driver's own submission path, e.g.
// `NvidiaGpu`'s nouveau-uAPI `EXEC`, can signal one directly).
const fn drm_iowr_core(nr: u32, size: usize) -> u32 {
    (3u32 << 30) | (0x64u32 << 8) | (nr & 0xff) | (((size as u32) & 0x3fff) << 16)
}
const DRM_IOCTL_SYNCOBJ_CREATE: u32 = drm_iowr_core(0xBF, core::mem::size_of::<DrmSyncobjCreate>());
const DRM_IOCTL_SYNCOBJ_DESTROY: u32 =
    drm_iowr_core(0xC0, core::mem::size_of::<DrmSyncobjDestroy>());
// 0xC1/0xC2 (HANDLE_TO_FD/FD_TO_HANDLE): NOT dispatched here -- like
// PRIME_HANDLE_TO_FD/FD_TO_HANDLE above, they need process fd table access
// this inode-level `io_control` doesn't have, so `linux-syscall`'s
// `sys_ioctl` intercepts them before they ever reach this match (see
// `sys_drm_syncobj_fd` there, and `linux_object::fs::SyncobjHandle`'s
// module doc for what "export" means given the syncobj table is a single
// global handle space, not per-process).
const DRM_IOCTL_SYNCOBJ_WAIT: u32 = drm_iowr_core(0xC3, core::mem::size_of::<DrmSyncobjWait>());
const DRM_IOCTL_SYNCOBJ_RESET: u32 = drm_iowr_core(0xC4, core::mem::size_of::<DrmSyncobjArray>());
const DRM_IOCTL_SYNCOBJ_SIGNAL: u32 = drm_iowr_core(0xC5, core::mem::size_of::<DrmSyncobjArray>());
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: u32 =
    drm_iowr_core(0xCA, core::mem::size_of::<DrmSyncobjTimelineWait>());
const DRM_IOCTL_SYNCOBJ_QUERY: u32 =
    drm_iowr_core(0xCB, core::mem::size_of::<DrmSyncobjTimelineArray>());
const DRM_IOCTL_SYNCOBJ_TRANSFER: u32 =
    drm_iowr_core(0xCC, core::mem::size_of::<DrmSyncobjTransfer>());
const DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL: u32 =
    drm_iowr_core(0xCD, core::mem::size_of::<DrmSyncobjTimelineArray>());
// The 2023 kernel fence-deadline feature APPENDED a `__u64 deadline_nsec` to
// BOTH wait structs (drm_syncobj_wait 32->40, drm_syncobj_timeline_wait 40->48),
// used only when DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE is set. Because the ioctl
// NUMBER encodes the struct size, a newer libdrm (Alpine's 2.4.134, what Mesa
// 26.1.6 links) sends 0xC028_64C3 / 0xC030_64CA, while an older one (QEMU's)
// sends 0xC020_64C3 / 0xC028_64CA. We must accept BOTH sizes: the deadline field
// sits AFTER every field the wait arm reads (handles..first_signaled), so
// parsing the shorter, pre-deadline layout is correct for either -- we just
// never read the optional hint. Pinning to only the 32/40 sizes is exactly what
// silently dropped NVK's `vk_drm_syncobj_get_type` CPU_WAIT probe on real
// hardware (its wait fell through to the driver, so Mesa never set
// VK_SYNC_FEATURE_CPU_WAIT and the first timeline VkSemaphore walked off
// `supported_sync_types` -- the libvulkan_nouveau.so+0x9cc48 NULL deref).
const DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE: u32 =
    drm_iowr_core(0xC3, core::mem::size_of::<DrmSyncobjWait>() + 8);
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE: u32 =
    drm_iowr_core(0xCA, core::mem::size_of::<DrmSyncobjTimelineWait>() + 8);
// 0xCF (EVENTFD): needs the eventfd from the process fd table, so -- like
// HANDLE_TO_FD/FD_TO_HANDLE above -- it is intercepted in `linux-syscall`'s
// `sys_ioctl` before reaching this match (see `sys_drm_syncobj_eventfd`).

#[repr(C)]
struct DrmSyncobjCreate {
    handle: u32,
    flags: u32,
}
const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1 << 0;

#[repr(C)]
struct DrmSyncobjDestroy {
    handle: u32,
    pad: u32,
}

// EXACT `drm.h` layout: `struct drm_syncobj_wait` is 32 bytes and has been
// UABI-frozen since 2017. A trailing `deadline_nsec: u64` used to sit here that
// does NOT exist in the real ABI -- it made `size_of` 40, so the ioctl number
// `drm_iowr_core(0xC3, size_of::<..>())` computed 0xC028_64C3 while libdrm sends
// 0xC020_64C3 (size 32). The exact-`u32` match arm therefore NEVER fired for
// Mesa's `drmSyncobjWait`: it fell through to the driver dispatch, hit no
// nouveau NR, and returned ENOSYS -- which NVK collapses into
// VK_ERROR_DEVICE_LOST. That was every GL client (glxgears AND
// eglgears_wayland) dying at its first submit-sync while the EXEC itself
// succeeded. The size guards below now pin these two.
#[repr(C)]
#[derive(Clone, Copy)]
struct DrmSyncobjWait {
    handles: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    #[allow(dead_code)]
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmSyncobjTimelineWait {
    handles: u64,
    points: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    #[allow(dead_code)]
    pad: u32,
}
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1 << 0;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE: u32 = 1 << 2;
/// A scheduling hint (`dma_fence_set_deadline`) carried by the 40-byte forms
/// of the wait structs; accepted, as Linux does, and not acted on.
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE: u32 = 1 << 3;
const DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED: u32 = 1 << 0;

/// Throttled visibility for syncobj WAIT failures: they return to Mesa as
/// bare errnos with no log of their own, which on real hardware left a
/// vkCreateDevice -13 whose console named no failing ioctl at all. One line
/// per distinct (kind, first handle, flavor); identical repeats collapse so
/// a retry loop (libdrm retries EAGAIN) cannot own the UART.
fn syncobj_wait_klog(timeline: bool, handles: &[u32], kind: &'static str) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
    let first = handles.first().copied().unwrap_or(0);
    let sig = (kind.as_ptr() as u64) ^ ((first as u64) << 1) ^ ((timeline as u64) << 63);
    if LAST.swap(sig, Ordering::Relaxed) != sig {
        // error!, not warn!: the rig boots LOG=error, and a syncobj WAIT
        // failure is exactly the invisible client death this line exists to
        // name (dedup above keeps it storm-proof).
        log::error!(
            "[drm] SYNCOBJ_{}WAIT -> {}: {} handle(s), first={:#x} (identical repeats suppressed)",
            if timeline { "TIMELINE_" } else { "" },
            kind,
            handles.len(),
            first
        );
    }
}

#[repr(C)]
struct DrmSyncobjArray {
    handles: u64,
    count_handles: u32,
    pad: u32,
}

#[repr(C)]
struct DrmSyncobjTimelineArray {
    handles: u64,
    points: u64,
    count_handles: u32,
    flags: u32,
}

// EXACT `drm.h` layout: `struct drm_syncobj_transfer` (32 bytes). Copies the
// fence at `src_handle`@`src_point` onto `dst_handle`@`dst_point`.
#[repr(C)]
struct DrmSyncobjTransfer {
    src_handle: u32,
    dst_handle: u32,
    src_point: u64,
    dst_point: u64,
    flags: u32,
    pad: u32,
}

// WAIT_VBLANK request type flags (`<drm/drm.h>`).
const _DRM_VBLANK_EVENT: u32 = 0x0400_0000;
/// The rest of `drm_vblank_seq_type`, as `drm_wait_vblank_ioctl` reads it.
const _DRM_VBLANK_TYPES_MASK: u32 = 0x1; // ABSOLUTE = 0, RELATIVE = 1
const _DRM_VBLANK_NEXTONMISS_FLAG: u32 = 0x1000_0000;
const _DRM_VBLANK_SECONDARY: u32 = 0x2000_0000;
const _DRM_VBLANK_SIGNAL: u32 = 0x4000_0000;
const _DRM_VBLANK_FLAGS_MASK: u32 =
    _DRM_VBLANK_EVENT | _DRM_VBLANK_SIGNAL | _DRM_VBLANK_SECONDARY | _DRM_VBLANK_NEXTONMISS_FLAG;
const _DRM_VBLANK_HIGH_CRTC_SHIFT: u32 = 1;
const _DRM_VBLANK_HIGH_CRTC_MASK: u32 = 0x1f << _DRM_VBLANK_HIGH_CRTC_SHIFT;

/// What `drm_wait_vblank_ioctl` refuses before it looks at the sequence:
/// `_DRM_VBLANK_SIGNAL` (signals have not been supported for years), any
/// bit outside the type, flag and high-CRTC masks, and a pipe index (the
/// high-CRTC field, or `_DRM_VBLANK_SECONDARY` for pipe 1) the card does
/// not have. Every one is EINVAL. This arm used to read only RELATIVE,
/// NEXTONMISS and EVENT and answer the rest with pipe 0's counter, so a
/// client waiting on the second head of a one-head card, or asking for a
/// signal, got a vblank instead of the error Linux gives.
fn wait_vblank_check(typ: u32) -> Result<()> {
    if typ & _DRM_VBLANK_SIGNAL != 0 {
        return Err(FsError::InvalidParam);
    }
    if typ & !(_DRM_VBLANK_TYPES_MASK | _DRM_VBLANK_FLAGS_MASK | _DRM_VBLANK_HIGH_CRTC_MASK) != 0 {
        return Err(FsError::InvalidParam);
    }
    let high_pipe = typ & _DRM_VBLANK_HIGH_CRTC_MASK;
    let pipe_index = if high_pipe != 0 {
        (high_pipe >> _DRM_VBLANK_HIGH_CRTC_SHIFT) as usize
    } else if typ & _DRM_VBLANK_SECONDARY != 0 {
        1
    } else {
        0
    };
    if pipe_index != 0 && pipe_index >= drm::crtc_count() {
        return Err(FsError::InvalidParam);
    }
    Ok(())
}

// Synthetic KMS property ids (software KMS). Linux allocates property object
// ids from the same idr as every other mode object; here they are fixed small
// ints above the synthetic CRTC/connector/encoder/plane ids. The names and
// semantics follow `drm-kms.rst` "Standard Properties": `type` classifies the
// plane, the connector carries `DPMS`/`link-status`/`non-desktop`/`EDID`, and
// the DRM_MODE_PROP_ATOMIC set (FB_ID..MODE_ID) is only shown to clients that
// negotiated DRM_CLIENT_CAP_ATOMIC, exactly like Linux hides atomic props
// from legacy clients.
const PROP_TYPE: u32 = 10;
const PROP_EDID: u32 = 11;
const PROP_DPMS: u32 = 12;
/// `DRM_MODE_DPMS_ON`. The other three levels (Standby, Suspend, Off) all mean
/// "stop lighting the panel" on a pipe with no power states of its own.
const DRM_MODE_DPMS_ON: u64 = 0;
const PROP_LINK_STATUS: u32 = 13;
const PROP_NON_DESKTOP: u32 = 14;
const PROP_FB_ID: u32 = 15;
/// One property object attached to both the plane and the connector, exactly
/// like Linux's single `prop_crtc_id`.
const PROP_CRTC_ID: u32 = 16;
const PROP_CRTC_X: u32 = 17;
const PROP_CRTC_Y: u32 = 18;
const PROP_CRTC_W: u32 = 19;
const PROP_CRTC_H: u32 = 20;
const PROP_SRC_X: u32 = 21;
const PROP_SRC_Y: u32 = 22;
const PROP_SRC_W: u32 = 23;
const PROP_SRC_H: u32 = 24;
const PROP_ACTIVE: u32 = 25;
const PROP_MODE_ID: u32 = 26;
/// Plane explicit in-fence (`drm_mode_create_standard_properties`).
const PROP_IN_FENCE_FD: u32 = 27;
/// CRTC out-fence pointer (`*mut i32` sync_file fd writeback).
const PROP_OUT_FENCE_PTR: u32 = 28;
/// Plane damage clips: a blob of `drm_mode_rect`, the region of the
/// framebuffer that actually changed since the last commit. Without this
/// property a compositor has no way to tell the kernel what it repainted, so
/// every commit had to be treated as a full-frame present.
const PROP_FB_DAMAGE_CLIPS: u32 = 29;

// Property flags (`drm_mode.h`).
const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
const DRM_MODE_PROP_OBJECT: u32 = 1 << 6; // DRM_MODE_PROP_TYPE(1)
const DRM_MODE_PROP_SIGNED_RANGE: u32 = 2 << 6; // DRM_MODE_PROP_TYPE(2)
const DRM_MODE_PROP_ATOMIC: u32 = 0x8000_0000;

// KMS object types (`drm_mode.h`).
const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
const DRM_MODE_OBJECT_ENCODER: u32 = 0xe0e0_e0e0;
const DRM_MODE_OBJECT_FB: u32 = 0xfbfb_fbfb;
const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;
/// `DRM_MODE_OBJECT_ANY`: a lookup that does not care about the type.
const DRM_MODE_OBJECT_ANY: u32 = 0;

// DRM client capabilities (DRM_IOCTL_SET_CLIENT_CAP).
const DRM_CLIENT_CAP_STEREO_3D: u64 = 1;
const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
/// `DRM_PLANE_TYPE_OVERLAY`: the only plane type `drm_mode_getplane_res` lists
/// to a client without `DRM_CLIENT_CAP_UNIVERSAL_PLANES`.
const DRM_PLANE_TYPE_OVERLAY: u32 = 0;
const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
const DRM_CLIENT_CAP_ASPECT_RATIO: u64 = 4;
const DRM_CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;
const DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;

// drm_mode_atomic flags (`drm_mode.h`).
const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 0x02;
const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;
const DRM_MODE_ATOMIC_FLAGS: u32 = DRM_MODE_PAGE_FLIP_EVENT
    | DRM_MODE_PAGE_FLIP_ASYNC
    | DRM_MODE_ATOMIC_TEST_ONLY
    | DRM_MODE_ATOMIC_NONBLOCK
    | DRM_MODE_ATOMIC_ALLOW_MODESET;

/// Whether a DRM ioctl is flagged `DRM_RENDER_ALLOW` in Linux's
/// `drm_ioctl.c` — the only commands a render node (`renderD128`, minor >=
/// 128) accepts. Everything else (modeset, dumb buffers, master/auth) gets
/// EACCES there, per `drm-uapi.rst` "Render nodes": *"no modesetting or
/// privileged ioctls can be issued on render nodes"*.
fn render_allowed(cmd: u32) -> bool {
    // The NR is the LOW byte. `(cmd >> 8) & 0xff` is the ioctl TYPE byte,
    // which for every DRM ioctl is 'd' (0x64) -- and 0x64 happens to sit
    // inside the driver-private 0x40..=0x9F arm below, so this filter used to
    // accept EVERYTHING on the render node by accident. With the NR extracted
    // correctly the set below is exactly Linux's DRM_RENDER_ALLOW list.
    let nr = cmd & 0xff;
    matches!(nr,
        0x00        // VERSION
        | 0x09      // GEM_CLOSE
        | 0x0C      // GET_CAP
        | 0x2D      // PRIME_HANDLE_TO_FD (handled in the syscall layer)
        | 0x2E      // PRIME_FD_TO_HANDLE (handled in the syscall layer)
        // Driver-specific command range (DRM_COMMAND_BASE..DRM_COMMAND_END).
        // Linux delegates per-command flags to the driver's own ioctl table;
        // our DrmScheme has no flags concept, so the range is passed through
        // and the driver decides (render/exec ioctls are RENDER_ALLOW in
        // practice).
        | 0x40..=0x9F
        | 0xBF..=0xC5 // SYNCOBJ_CREATE..SYNCOBJ_SIGNAL
        | 0xCA..=0xCD // SYNCOBJ_TIMELINE_WAIT..TIMELINE_SIGNAL
        | 0xCF      // SYNCOBJ_EVENTFD
        | 0xD1      // SET_CLIENT_NAME
        | 0xD2      // GEM_CHANGE_HANDLE
    )
}

/// Bounded, level-filter-free trace of the syncobj ops NVK issues while it
/// FUNCTIONALLY probes the timeline feature at device init (Mesa's
/// `vk_drm_syncobj_get_type` auto-detects features rather than trusting the
/// cap: create → signal/timeline-signal → query/transfer/wait → destroy). If
/// it decides timeline is unsupported it masks the feature off, NVK's
/// `sync_types` loses the timeline entry, and the FIRST timeline VkSemaphore
/// (zink's batch fence, wlroots' render timeline) walks off that array — the
/// `libvulkan_nouveau.so+0x9cc48` NULL deref. This names the exact op, args and
/// result that made NVK decide, which no error log catches (every op succeeds).
fn trace_syncobj(op: &str, pid: u64, handle: u32, point: u64, result: &str) {
    static BUDGET: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    if BUDGET.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 48 {
        kernel_hal::klog_info!(
            "[syncobj] {} pid={} handle={:#x} point={} -> {}",
            op,
            pid,
            handle,
            point,
            result
        );
    }
}

/// Driver-private DRM command number (`DRM_COMMAND_BASE + 0x50`) for
/// `struct drm_eclipse_compute`. Nouveau does not use 0x50. Matched by NR
/// so the `_IOWR` size encoding does not have to agree bit-for-bit.
const DRM_ECLIPSE_COMPUTE_NR: u32 = 0x90;

#[repr(C)]
struct DrmEclipseCompute {
    op: u32,
    status: i32,
    elapsed_ns: u64,
    grid_threads: u32,
    reserved: u32,
    summary: [u8; 512],
}

fn drm_node_name(minor: u32) -> alloc::string::String {
    drm::node_name(minor)
}

/// First id of the range reserved for EDID blobs, one per connector.
///
/// A connector's EDID is served through `GETPROPBLOB` like any other property
/// blob, but it is not in the blob store: it comes from the driver on demand.
/// So it gets a reserved slice of the same id space, below the ids
/// `CREATEPROPBLOB` hands out ([`drm::BLOB_ID_BASE`]).
const EDID_BLOB_BASE: u32 = 20_000;

/// The reserved range has to end before the blob store's ids begin, or a
/// user blob and a connector's EDID would answer to the same id and
/// `GETPROPBLOB` -- which tries the store first -- would hide the EDID.
const _: () = assert!(EDID_BLOB_BASE < drm::BLOB_ID_BASE);

/// The blob id that serves `connector_id`'s EDID, and its inverse. The two
/// ends are far apart -- `connector_props` advertises the id, `GETPROPBLOB`
/// resolves it -- so they are one pair of functions.
fn edid_blob_id(connector_id: u32) -> u32 {
    EDID_BLOB_BASE + connector_id
}

/// See [`edid_blob_id`]. `None` for an id outside the reserved range, which
/// includes every id the blob store can hand out.
fn connector_of_edid_blob(blob_id: u32) -> Option<u32> {
    if blob_id >= drm::BLOB_ID_BASE {
        return None;
    }
    blob_id.checked_sub(EDID_BLOB_BASE)
}

/// The fake, page-aligned mmap offset `DRM_IOCTL_MODE_MAP_DUMB` hands back for
/// a GEM handle, and its inverse.
///
/// There is no real file offset behind a GEM buffer, so the handle is encoded
/// in the offset itself. musl's `mmap()` refuses a non-page-aligned offset
/// before the syscall is even made, which is why the cookie is a page shift
/// and not the handle itself. Linux does the same thing through the device's
/// `vma_offset_manager`.
///
/// The two ends live far apart -- the ioctl arm that mints the cookie and
/// `DrmDev::get_vmo`, which is reached from `mmap` -- so they are one pair of
/// functions rather than a shift written out at each end.
fn mmap_cookie_for(handle: u32) -> u64 {
    (handle as u64) << 12
}

/// Whether an `mmap` of `len` bytes fits a GEM object of `object` bytes.
///
/// Linux's `drm_gem_mmap_obj` refuses with EINVAL a mapping longer than the
/// object (measured in whole pages, `drm_vma_node_size`), and the TTM drivers
/// SIGBUS a touch past it. Here the syscall's own past-EOF rule applied: the
/// object backed the head and the tail was fresh zero pages, so a client that
/// mapped a buffer with the wrong size got readable, writable memory that was
/// not the buffer, and nothing said so.
fn mmap_len_check(len: usize, object: usize) -> Result<()> {
    if len > zircon_object::vm::pages(object) * zircon_object::vm::PAGE_SIZE {
        return Err(FsError::InvalidParam);
    }
    Ok(())
}

/// See [`mmap_cookie_for`]. Truncates above 32 bits, so an offset beyond the
/// handle space aliases onto a handle rather than failing; both lookups behind
/// this check the caller owns what it named, so an alias is not a way in.
fn handle_from_mmap_cookie(offset: usize) -> u32 {
    (offset >> 12) as u32
}

/// `access_ok()` for a nested user pointer an ioctl arm is about to read or
/// write directly (`fb_id_ptr`, `clips_ptr`, `handles`, blob `data`, ...):
/// EFAULT unless `[addr, addr + bytes)` lies in the user half. The top-level
/// argument is checked once in `io_control` from the size the ioctl number
/// encodes; every pointer *inside* that struct goes through here before the
/// `unsafe` access, so a client cannot aim the kernel's copy at kernel memory.
fn ucheck(addr: usize, bytes: usize) -> Result<()> {
    if kernel_hal::user::user_range_ok(addr, bytes) {
        Ok(())
    } else {
        Err(FsError::BadAddress)
    }
}

/// [`ucheck`] for an array of `count` `T`s.
fn ucheck_n<T>(addr: usize, count: usize) -> Result<()> {
    match count.checked_mul(core::mem::size_of::<T>()) {
        Some(bytes) => ucheck(addr, bytes),
        None => Err(FsError::InvalidParam),
    }
}

/// `drm_copy_field`: `value` cut to the room in `*buf_len`, no terminator,
/// and `*buf_len` set to the value's full length so the caller can size a
/// second call. A null buffer writes nothing (Linux would fault on it).
fn drm_copy_field(buf: *mut u8, buf_len: &mut usize, value: &[u8]) -> Result<()> {
    let len = value.len().min(*buf_len);
    *buf_len = value.len();
    if len > 0 && !buf.is_null() {
        ucheck(buf as usize, len)?;
        unsafe {
            core::ptr::copy_nonoverlapping(value.as_ptr(), buf, len);
        }
    }
    Ok(())
}

/// `(major, minor, patchlevel)` of the driver behind a node: what VERSION
/// reports and SET_VERSION checks a request against. The compute node is
/// its own 0.1.0; a nouveau node carries nouveau's 1.4.0 (the version that
/// gates the VM_BIND/EXEC uAPI); the software card is 1.0.0.
fn driver_version(id: drm::NodeDriverId) -> (i32, i32, i32) {
    match id {
        drm::NodeDriverId::Compute => (0, 1, 0),
        drm::NodeDriverId::Nouveau => (1, 4, 0),
        drm::NodeDriverId::Software => (1, 0, 0),
    }
}

/// How many handles a syncobj array ioctl may name. Linux puts no bound of
/// its own on `count_handles`: the array is `kmalloc_array`ed, and only
/// past 4 MiB of handles does that fail, with ENOMEM. The five array arms
/// here shared a cap of 64, EINVAL beyond it -- and `vkWaitForFences` with
/// more fences than that is a single `TIMELINE_WAIT` over all of them
/// (`vk_drm_syncobj_wait_many`), so a legal call came back
/// VK_ERROR_UNKNOWN.
const SYNCOBJ_ARRAY_MAX: u32 = 1 << 20;

/// ENOMEM past [`SYNCOBJ_ARRAY_MAX`], as the kernel's allocation would be.
fn syncobj_array_bound(count_handles: u32) -> Result<()> {
    if count_handles > SYNCOBJ_ARRAY_MAX {
        Err(FsError::NoMemory)
    } else {
        Ok(())
    }
}

/// What a `SYNCOBJ_WAIT` / `TIMELINE_WAIT` (deadline-sized or not) asks
/// for, read the same way by the async sleeper and the sync arm.
struct SyncobjWaitReq {
    timeline: bool,
    handles: alloc::vec::Vec<u32>,
    points: Option<alloc::vec::Vec<u64>>,
    timeout_nsec: i64,
    flags: u32,
}

/// Bytes of a wait request's struct that [`read_syncobj_wait`] reads.
///
/// Deadline-sized ioctls carry a trailing hint never read here; the prefix
/// matches the classic structs, which share this layout.
fn syncobj_wait_prefix(cmd: u32) -> usize {
    if is_syncobj_timeline_wait(cmd) {
        core::mem::size_of::<DrmSyncobjTimelineWait>()
    } else {
        core::mem::size_of::<DrmSyncobjWait>()
    }
}

/// Read a wait request. `Ok(None)` is a wait on no handles at all, which
/// Linux answers 0 without reading the array (`count_handles == 0`), and
/// the array bound is [`syncobj_array_bound`].
///
/// `data` must already be known readable for [`syncobj_wait_prefix`] bytes;
/// this does NOT `ucheck` the struct itself. From the sync arm it may be the
/// kernel bounce buffer [`drm_ioctl_reconciled`] made for a client whose
/// struct size differs from ours -- and that is exactly Alpine's libdrm, whose
/// `drm_syncobj_wait` carries `deadline_nsec` (40 bytes, not 32). A `ucheck`
/// here refused that kernel address with EFAULT on bare metal (never under
/// `libos`, where the user-half bound is off, so no host test could see it).
/// NVK's `vk_drm_syncobj_get_type` probe -- a WAIT on a signaled syncobj --
/// then failed, its syncobj type lost `VK_SYNC_FEATURE_CPU_WAIT`, and the
/// first submit that needed a binary CPU-wait sync type walked
/// `supported_sync_types` off its NULL end: `libvulkan_nouveau.so+0xe9c48`,
/// `fault @ 0x8`, in labwc right after its first EXEC. The dispatcher has
/// already done `access_ok()` over the client's range; the async sleeper,
/// which gets the raw user pointer, checks it itself. The nested `handles`
/// and `points` arrays are always user memory and are checked below.
fn read_syncobj_wait(cmd: u32, data: usize) -> Result<Option<SyncobjWaitReq>> {
    // One rule for "is this the timeline wait", so the sleeper and the arm
    // cannot read the same request as different structs.
    let timeline = is_syncobj_timeline_wait(cmd);
    let (handles_ptr, points_ptr, timeout_nsec, count_handles, flags) = if timeline {
        let req = unsafe { *(data as *const DrmSyncobjTimelineWait) };
        (
            req.handles,
            req.points,
            req.timeout_nsec,
            req.count_handles,
            req.flags,
        )
    } else {
        let req = unsafe { *(data as *const DrmSyncobjWait) };
        (
            req.handles,
            0u64,
            req.timeout_nsec,
            req.count_handles,
            req.flags,
        )
    };
    // `drm_syncobj_array_wait_ioctl` / `drm_syncobj_timeline_wait_ioctl`:
    // a flag bit outside the form's set is EINVAL before the count is
    // looked at, and WAIT_AVAILABLE is the timeline form's alone -- on the
    // binary wait it was read and honoured here as if it were legal.
    let allowed = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL
        | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT
        | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE
        | if timeline {
            DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE
        } else {
            0
        };
    if flags & !allowed != 0 {
        return Err(FsError::InvalidParam);
    }
    if count_handles == 0 {
        return Ok(None);
    }
    syncobj_array_bound(count_handles)?;
    if handles_ptr == 0 {
        return Err(FsError::InvalidParam);
    }
    ucheck_n::<u32>(handles_ptr as usize, count_handles as usize)?;
    if timeline && points_ptr != 0 {
        ucheck_n::<u64>(points_ptr as usize, count_handles as usize)?;
    }
    let handles: alloc::vec::Vec<u32> = (0..count_handles as usize)
        .map(|i| unsafe { *(handles_ptr as *const u32).add(i) })
        .collect();
    let points: Option<alloc::vec::Vec<u64>> = if timeline && points_ptr != 0 {
        Some(
            (0..count_handles as usize)
                .map(|i| unsafe { *(points_ptr as *const u64).add(i) })
                .collect(),
        )
    } else {
        None
    };
    Ok(Some(SyncobjWaitReq {
        timeline,
        handles,
        points,
        timeout_nsec,
        flags,
    }))
}

fn eclipse_compute_ioctl(minor: u32, data: usize) -> Result<usize> {
    let req = unsafe { &mut *(data as *mut DrmEclipseCompute) };
    // The node decides the GPU: `ecl-compute` on card2 must launch on the card
    // card2 names, not on whichever one happens to be "the" compute GPU. Falls
    // back to the old global choice for a node with no table entry.
    let driver = drm::driver_for_minor(minor)
        .or_else(drm::get_compute_driver)
        .or_else(drm::get_primary_driver);
    let Some(driver) = driver else {
        req.status = -19; // -ENODEV
        fill_summary(&mut req.summary, "no compute GPU");
        return Ok(0);
    };
    let result = driver.compute_launch(req.op);
    req.status = result.status;
    req.elapsed_ns = result.elapsed_ns;
    req.grid_threads = result.grid_threads;
    fill_summary(&mut req.summary, &result.report);
    Ok(0)
}

/// How many spans the last DIRTYFB blitted: the boxes themselves, or 1 for their
/// bounding union. A test cannot see the difference on the screen when the client
/// painted its whole buffer, and that is exactly the client a test writes.
#[cfg(test)]
static DIRTY_SPANS_BLITTED: AtomicUsize = AtomicUsize::new(0);

/// How many spans the last DIRTYFB blitted.
#[cfg(test)]
fn dirty_spans_blitted_for_test() -> usize {
    DIRTY_SPANS_BLITTED.load(Ordering::Relaxed)
}

fn fill_summary(dst: &mut [u8; 512], src: &str) {
    dst.fill(0);
    let bytes = src.as_bytes();
    let n = core::cmp::min(bytes.len(), dst.len() - 1);
    dst[..n].copy_from_slice(&bytes[..n]);
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmVersion {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: *mut u8,
    date_len: usize,
    date: *mut u8,
    desc_len: usize,
    desc: *mut u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmUnique {
    unique_len: usize,
    unique: *mut u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmGetCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeFbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32, // current encoder
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeFbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

/// The fourcc `drm_mode_legacy_fb_format` derives from an `ADDFB`'s
/// (`bpp`, `depth`), or `None` for a pair it does not know
/// (`DRM_FORMAT_INVALID`, which `drm_mode_addfb` turns into EINVAL). The
/// formats other than the two [`drm::SCANOUT_FORMATS`] are real fourccs that
/// [`addfb2_check`] then refuses, exactly as Linux refuses them on a device
/// whose planes do not scan them out.
fn legacy_fb_format(bpp: u32, depth: u32) -> Option<u32> {
    /// `DRM_FORMAT_C8`
    const C8: u32 = 0x2020_3843;
    /// `DRM_FORMAT_XRGB1555`
    const XRGB1555: u32 = 0x3531_5258;
    /// `DRM_FORMAT_RGB565`
    const RGB565: u32 = 0x3631_4752;
    /// `DRM_FORMAT_RGB888`
    const RGB888: u32 = 0x3432_4752;
    /// `DRM_FORMAT_XRGB2101010`
    const XRGB2101010: u32 = 0x3033_5258;
    Some(match (bpp, depth) {
        (8, 8) => C8,
        (16, 15) => XRGB1555,
        (16, 16) => RGB565,
        (24, 24) => RGB888,
        (32, 24) => drm::DRM_FORMAT_XRGB8888,
        (32, 30) => XRGB2101010,
        (32, 32) => drm::DRM_FORMAT_ARGB8888,
        _ => return None,
    })
}

/// `drm_mode_fb_cmd2.flags`: the two Linux knows. Anything else is EINVAL.
const DRM_MODE_FB_INTERLACED: u32 = 1 << 0;
const DRM_MODE_FB_MODIFIERS: u32 = 1 << 1;

// ===================== DRM format modifiers for scanout =====================

/// `DRM_FORMAT_MOD_VENDOR_NVIDIA`, the top byte of every modifier below.
const DRM_FORMAT_MOD_VENDOR_NVIDIA: u64 = 0x03;
/// `DRM_FORMAT_MOD_LINEAR`, which is also the zero a pre-modifier client
/// leaves in the field.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: `fourcc_mod_code(NONE, ((1ULL << 56) - 1))`.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// `drm_fourcc_canonicalize_nvidia_format_mod`, copied from `drm_fourcc.h`
/// rather than approximated.
///
/// Page kind 0 means "pitch/linear", which a block-linear surface cannot be,
/// so the kernel grandfathers the older `DRM_FORMAT_MOD_NVIDIA_16BX2_BLOCK(v)`
/// modifiers -- which leave `k` at 0 -- onto kind `0xfe`, the generic
/// uncompressed colour kind. Comparing a client's modifier against an
/// advertised list WITHOUT this step silently rejects every client still
/// sending the old spelling.
fn canonicalize_nvidia_modifier(modifier: u64) -> u64 {
    if modifier & 0x10 == 0 || modifier & (0xff << 12) != 0 {
        modifier
    } else {
        modifier | (0xfe << 12)
    }
}

/// The `g` field (bits 21:20) this GPU family speaks: **2**, "Gob Height 8,
/// Turing+ Page Kind mapping". Not 1 -- that is G80..GT2XX, whose GOBs are
/// four rows high -- and not 0, which is Fermi..Volta.
const NVIDIA_MOD_GEN_TURING: u64 = 2;
/// The `s` field (bit 22, plus 27:26 for values above 1): **1**,
/// "Pre-GB20x ... Tegra Xavier-Orin Layout", which covers every Turing and
/// Ampere desktop part.
const NVIDIA_MOD_SECTOR_DESKTOP: u64 = 1;
/// The largest `h` the hardware defines (`SET_SRC_BLOCK_SIZE_HEIGHT` tops out
/// at `_THIRTYTWO_GOBS`).
const NVIDIA_MOD_MAX_LOG2_GOBS_Y: u64 = 5;

/// The page kinds a block-linear scanout buffer may carry: just
/// `NV_MMU_VER2_PTE_KIND_GENERIC_MEMORY` (0x06), which is what NVK's layout
/// library picks for every uncompressed tiled surface on Turing and what
/// `VM_BIND` programs into the page tables verbatim. The page table and the
/// copy engine have to agree on one kind, and this is it.
///
/// `0xfe`, which the canonicalization above can produce, is the generic kind
/// of the **Fermi..Volta** mapping, so it only ever arrives alongside a `g`
/// this decoder has already refused. It is not listed here, because listing
/// it would mean accepting a Turing-generation modifier whose kind came from
/// another generation's table.
///
/// A COMPRESSED kind is refused rather than downgraded: the comptags that
/// give it meaning are allocated nowhere in this tree, and scanning out
/// compressed bytes as if they were uncompressed is the one way this path
/// paints garbage.
const NVIDIA_SCANOUT_PAGE_KINDS: [u8; 1] = [0x06];

/// What layout `modifier` asks for, or `None` if this scanout cannot present
/// it.
///
/// Mirrors `nv_drm_framebuffer_init`'s checks (the lossless-compression field
/// must be zero) and adds the two this tree needs: the GOB generation has to
/// be the one the copy engine's `SET_SRC_BLOCK_SIZE` assumes, and the page
/// kind has to be one `VM_BIND` maps verbatim.
pub(super) fn decode_scanout_modifier(modifier: u64) -> Option<drm::ScanoutLayout> {
    if modifier == DRM_FORMAT_MOD_LINEAR {
        return Some(drm::ScanoutLayout::Linear);
    }
    if modifier == DRM_FORMAT_MOD_INVALID {
        return None;
    }
    if modifier >> 56 != DRM_FORMAT_MOD_VENDOR_NVIDIA {
        return None;
    }
    let m = canonicalize_nvidia_modifier(modifier & 0x00ff_ffff_ffff_ffff);
    // Bit 4 must be 1: without it the value is one of the older
    // non-block-linear NVIDIA modifiers, not a 2D block-linear one.
    if m & 0x10 == 0 {
        return None;
    }
    // Bits 8:5 and 11:9 are reserved and "must be zero"; so is everything
    // from 28 up. Refusing them keeps a future 3D-surface or array-stride
    // modifier from being silently presented as a 2D one.
    if m & 0x00ff_ffff_f000_0fe0 != 0 {
        return None;
    }
    let h = m & 0xf;
    let k = (m >> 12) & 0xff;
    let g = (m >> 20) & 0x3;
    // `s` is bit 22 plus bits 27:26; the high half is already covered by the
    // reserved-bits check above, so only bit 22 is left to read.
    let s = (m >> 22) & 0x1;
    let c = (m >> 23) & 0x7;
    if c != 0 || g != NVIDIA_MOD_GEN_TURING || s != NVIDIA_MOD_SECTOR_DESKTOP {
        return None;
    }
    if h > NVIDIA_MOD_MAX_LOG2_GOBS_Y {
        return None;
    }
    if !NVIDIA_SCANOUT_PAGE_KINDS.contains(&(k as u8)) {
        return None;
    }
    Some(drm::ScanoutLayout::BlockLinear {
        log2_gobs_per_block_y: h as u8,
        page_kind: k as u8,
    })
}

/// The modifier word a layout came from, for `GETFB2` to report back.
///
/// Exact rather than approximate: [`decode_scanout_modifier`] accepts one
/// combination of the compression, sector and generation fields, so the
/// layout plus its two stored fields determine the original word. If that
/// ever stops being true -- a second accepted `g`, say -- this has to store
/// the word instead of rebuilding it, and
/// [`a_tiled_framebuffer_reads_back_as_the_modifier_it_was_made_with`] is
/// what notices.
fn scanout_layout_modifier(layout: drm::ScanoutLayout) -> u64 {
    match layout {
        drm::ScanoutLayout::Linear => 0,
        drm::ScanoutLayout::BlockLinear {
            log2_gobs_per_block_y,
            page_kind,
        } => {
            (DRM_FORMAT_MOD_VENDOR_NVIDIA << 56)
                | 0x10
                | u64::from(log2_gobs_per_block_y)
                | (u64::from(page_kind) << 12)
                | (NVIDIA_MOD_GEN_TURING << 20)
                | (NVIDIA_MOD_SECTOR_DESKTOP << 22)
        }
    }
}

/// `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c, s, g, k, h)` from `drm_fourcc.h`.
///
/// The tests build their modifiers with the macro's own arithmetic rather
/// than with hand-written hex, so a field that moves is caught by them
/// disagreeing with [`decode_scanout_modifier`] rather than by both being
/// wrong in the same way. The plane's `IN_FORMATS` list will be built from
/// it too once there is a present path worth advertising.
#[cfg(test)]
pub(super) const fn nvidia_block_linear_2d(c: u64, s: u64, g: u64, k: u64, h: u64) -> u64 {
    (DRM_FORMAT_MOD_VENDOR_NVIDIA << 56)
        | (0x10
            | (h & 0xf)
            | ((k & 0xff) << 12)
            | ((g & 0x3) << 20)
            | ((s & 0x1) << 22)
            | ((s & 0x6) << 25)
            | ((c & 0x7) << 23))
}

/// What `drm_internal_framebuffer_create` and `framebuffer_check` refuse
/// before a driver ever sees an `ADDFB2`, for the one plane layout here: a
/// single 32-bit plane, no modifiers (`DRM_CAP_ADDFB2_MODIFIERS` is 0).
///
/// None of it was checked: `pixel_format`, `flags`, `modifier`, `offsets`
/// and the extra planes were read for the log line and nothing else, so an
/// NV12 (`mpv --vo=drm`), a 10-bit or a modifier-tiled buffer was wrapped
/// as if it were XR24 and scanned out as garbage, where Linux answers
/// EINVAL and the client falls back or says why.
///
/// `offsets[0]` is the one place this is stricter than Linux, which allows
/// a plane to start inside its buffer: the framebuffer here is its buffer's
/// base (`phys_addr`, and the driver's own fb takes the handle alone), so
/// a non-zero offset would silently scan out from the wrong place. No
/// client of this tree sends one (GBM and dumb buffers start at 0).
fn addfb2_check(cmd: &DrmModeFbCmd2) -> Result<drm::ScanoutLayout> {
    if cmd.flags & !(DRM_MODE_FB_INTERLACED | DRM_MODE_FB_MODIFIERS) != 0 {
        return Err(FsError::InvalidParam);
    }
    let have_modifier = cmd.flags & DRM_MODE_FB_MODIFIERS != 0;
    if have_modifier && !drm::scanout_modifiers_enabled() {
        // "driver does not support fb modifiers" -- which is the honest
        // answer while `DRM_CAP_ADDFB2_MODIFIERS` reads 0, and the two have
        // to agree: a client that asked the cap first and was told no must
        // not find the flag accepted here.
        return Err(FsError::InvalidParam);
    }
    if !drm::SCANOUT_FORMATS.contains(&cmd.pixel_format) {
        // "bad framebuffer format" / no plane supports it
        return Err(FsError::InvalidParam);
    }
    if cmd.width == 0 || cmd.height == 0 {
        return Err(FsError::InvalidParam);
    }
    if cmd.handles[0] == 0 {
        // "no buffer object handle for plane 0"
        return Err(FsError::InvalidParam);
    }
    let layout = if have_modifier {
        // An explicit modifier, which may name a layout the present path
        // cannot read; `decode_scanout_modifier` is the one judge of that.
        match decode_scanout_modifier(cmd.modifier[0]) {
            Some(l) => l,
            None => return Err(FsError::InvalidParam),
        }
    } else {
        // "bad fb modifier" -- Linux refuses a non-zero modifier word
        // whenever the flag is absent, whatever the driver supports.
        if cmd.modifier[0] != 0 {
            return Err(FsError::InvalidParam);
        }
        drm::ScanoutLayout::Linear
    };
    // "bad pitch": less than a row of pixels. (`create_fb` checks it against
    // the buffer as well.) For a block-linear surface the pitch is counted
    // in 64-byte BLOCKS, not bytes -- comparing it against a byte width
    // would reject every legitimate tiled framebuffer by a factor of 64.
    let pitch_bytes = match layout {
        drm::ScanoutLayout::Linear => u64::from(cmd.pitches[0]),
        drm::ScanoutLayout::BlockLinear { .. } => u64::from(cmd.pitches[0]) * drm::GOB_WIDTH_BYTES,
    };
    if pitch_bytes < u64::from(cmd.width) * 4 {
        return Err(FsError::InvalidParam);
    }
    if cmd.offsets[0] != 0 {
        return Err(FsError::InvalidParam);
    }
    for i in 1..4 {
        if cmd.handles[i] != 0 || cmd.pitches[i] != 0 || cmd.offsets[i] != 0 || cmd.modifier[i] != 0
        {
            return Err(FsError::InvalidParam);
        }
    }
    Ok(layout)
}

/// `drm_crtc.gamma_size` of every CRTC here: no legacy gamma store, which is
/// what GETCRTC reports and what GETGAMMA/SETGAMMA hold the caller to.
const CRTC_GAMMA_SIZE: u32 = 0;

/// `struct drm_mode_crtc_lut`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeCrtcLut {
    crtc_id: u32,
    gamma_size: u32,
    red: u64,
    green: u64,
    blue: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: [u8; 68],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetPlaneRes {
    plane_id_ptr: u64,
    count_planes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    possible_crtcs: u32,
    gamma_size: u32,
    count_format_types: u32,
    format_type_ptr: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetProperty {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; 32],
    count_values: u32,
    count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetBlob {
    blob_id: u32,
    length: u32,
    data: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModePropertyEnum {
    value: u64,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeCrtcPageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

/// `union drm_wait_vblank` (24 bytes). The request side is `{ type, sequence,
/// signal }`; the reply side reuses the trailing 16 bytes as `{ tval_sec,
/// tval_usec }`. We model the union as one struct and read/write the overlap by
/// field.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmWaitVblank {
    typ: u32,
    sequence: u32,
    /// request: `signal`; reply: `tval_sec`.
    val1: u64,
    /// request: unused; reply: `tval_usec`.
    val2: u64,
}

/// `struct drm_mode_set_plane` (48 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeSetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    crtc_x: i32,
    crtc_y: i32,
    crtc_w: u32,
    crtc_h: u32,
    // Source values are 16.16 fixed point.
    src_x: u32,
    src_y: u32,
    src_h: u32,
    src_w: u32,
}

/// `struct drm_mode_obj_set_property` (24 bytes after u64 alignment padding).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeObjSetProperty {
    value: u64,
    prop_id: u32,
    obj_id: u32,
    obj_type: u32,
}

/// `struct drm_mode_fb_dirty_cmd` (24 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeFbDirtyCmd {
    fb_id: u32,
    flags: u32,
    color: u32,
    num_clips: u32,
    clips_ptr: u64,
}

/// `struct drm_clip_rect` (8 bytes) — one element of the array `clips_ptr`
/// points to. `x2`/`y2` are exclusive, i.e. the rect covers `[x1, x2) x [y1, y2)`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmClipRect {
    x1: u16,
    y1: u16,
    x2: u16,
    y2: u16,
}

/// `struct drm_set_version` (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmSetVersion {
    drm_di_major: i32,
    drm_di_minor: i32,
    drm_dd_major: i32,
    drm_dd_minor: i32,
}

/// `struct drm_mode_atomic` (56 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeAtomic {
    flags: u32,
    count_objs: u32,
    objs_ptr: u64,
    count_props_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    reserved: u64,
    user_data: u64,
}

/// `struct drm_mode_create_blob` (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeCreateBlob {
    data: u64,
    length: u32,
    blob_id: u32,
}

/// `struct drm_client` (40 bytes on LP64).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmClient {
    idx: i32,
    auth: i32,
    pid: usize,
    uid: usize,
    magic: usize,
    iocs: usize,
}

// Compile-time guards: each DRM ioctl number encodes `sizeof(struct)` in its
// _IOC size field, so a wrong struct layout silently mismatches the ioctl and
// the handler never fires. Assert the sizes that the constants above depend on.
const _: () = {
    use core::mem::size_of;
    assert!(size_of::<DrmModeGetConnector>() == 80); // DRM_IOCTL_MODE_GETCONNECTOR 0x..50..
    assert!(size_of::<DrmModeGetEncoder>() == 20); // DRM_IOCTL_MODE_GETENCODER  0x..14..
    assert!(size_of::<DrmModeFbCmd2>() == 104); // DRM_IOCTL_MODE_ADDFB2      0x..68..
    assert!(size_of::<DrmModeGetCrtc>() == 104); // DRM_IOCTL_MODE_{GET,SET}CRTC 0x..68..
    assert!(size_of::<DrmModeCrtcPageFlip>() == 24); // DRM_IOCTL_MODE_PAGE_FLIP 0x..18..
    assert!(size_of::<DrmModeObjGetProperties>() == 32); // OBJ_GETPROPERTIES 0x..20..
    assert!(size_of::<DrmModeGetProperty>() == 64); // GETPROPERTY        0x..40..
    assert!(size_of::<DrmModeGetBlob>() == 16); // GETPROPBLOB        0x..10..
    assert!(size_of::<DrmModePropertyEnum>() == 40);
    assert!(size_of::<DrmModeGetPlane>() == 32); // DRM_IOCTL_MODE_GETPLANE 0x..20..
    assert!(size_of::<DrmWaitVblank>() == 24); // DRM_IOCTL_WAIT_VBLANK   0x..18..
    assert!(size_of::<DrmModeSetPlane>() == 48); // DRM_IOCTL_MODE_SETPLANE 0x..30..
    assert!(size_of::<DrmModeObjSetProperty>() == 24); // OBJ_SETPROPERTY  0x..18..
    assert!(size_of::<DrmModeFbDirtyCmd>() == 24); // DRM_IOCTL_MODE_DIRTYFB 0x..18..
    assert!(size_of::<DrmClipRect>() == 8); // drm_clip_rect, via DIRTYFB's clips_ptr
    assert!(size_of::<DrmSetVersion>() == 16); // DRM_IOCTL_SET_VERSION   0x..10..
    assert!(size_of::<DrmModeAtomic>() == 56); // DRM_IOCTL_MODE_ATOMIC   0x..38..
    assert!(size_of::<DrmModeCreateBlob>() == 16); // CREATEPROPBLOB      0x..10..
    assert!(size_of::<DrmClient>() == 40); // DRM_IOCTL_GET_CLIENT     0x..28..
    assert!(size_of::<DrmSyncobjCreate>() == 8); // DRM_IOCTL_SYNCOBJ_CREATE   0x..08..
    assert!(size_of::<DrmSyncobjDestroy>() == 8); // DRM_IOCTL_SYNCOBJ_DESTROY  0x..08..
    assert!(size_of::<DrmSyncobjWait>() == 32); // DRM_IOCTL_SYNCOBJ_WAIT     0x..20..
    assert!(size_of::<DrmSyncobjTimelineWait>() == 40); // TIMELINE_WAIT      0x..28..
    assert!(size_of::<DrmSyncobjArray>() == 16); // RESET/SIGNAL              0x..10..
    assert!(size_of::<DrmSyncobjTimelineArray>() == 24); // TIMELINE_SIGNAL/QUERY 0x..18..
    assert!(size_of::<DrmSyncobjTransfer>() == 32); // DRM_IOCTL_SYNCOBJ_TRANSFER 0x..20..
};

/// Pixel clock (kHz) so Mesa/wlroots millihertz lands on `refresh_mhz`:
/// `refresh_mHz = (clock * 1_000_000 / htotal + vtotal/2) / vtotal`.
fn clock_khz_for_refresh_mhz(htotal: u32, vtotal: u32, refresh_mhz: u32) -> u32 {
    if htotal == 0 || vtotal == 0 {
        return 0;
    }
    // `saturating_sub`: with `refresh_mhz == 0` the subtraction goes negative,
    // which is a panic in debug. Only one caller passes a target today (60_000)
    // but the refresh is a parameter, and a zero one should mean "the slowest
    // clock that is still a clock", not a dead kernel.
    let target = (refresh_mhz as u64 * vtotal as u64).saturating_sub(vtotal as u64 / 2);
    let clock = target.saturating_mul(htotal as u64).div_ceil(1_000_000);
    clock.max(1) as u32
}

/// `DRM_MODE_FLAG_*` from `<drm/drm_mode.h>`. Only the three a synthetic or
/// EDID-derived mode can carry.
const DRM_MODE_FLAG_PHSYNC: u32 = 1 << 0;
const DRM_MODE_FLAG_NHSYNC: u32 = 1 << 1;
const DRM_MODE_FLAG_PVSYNC: u32 = 1 << 2;
const DRM_MODE_FLAG_NVSYNC: u32 = 1 << 3;
const DRM_MODE_FLAG_INTERLACE: u32 = 1 << 4;

/// The numbers that go into a `drm_mode_modeinfo`, from whichever source could
/// supply them. Split out from the byte packing so both sources can be tested
/// without a monitor and without the boot-EDID global.
struct Modeline {
    clock_khz: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vrefresh: u32,
    flags: u32,
}

impl Modeline {
    /// The nominal mode: `w`x`h` at 60 Hz with fixed porches.
    ///
    /// A software framebuffer never programs CRT timings, so these are made up
    /// — but they must be *valid*: `hdisplay < hsync_start < hsync_end <
    /// htotal` (and the vertical analogue). The previous +10%/+5% blanking put
    /// `hsync_end > htotal` at 1366×768, which is MODE_H_ILLEGAL; compositors
    /// that recompute refresh from the porches then advertised ~55–59 Hz
    /// instead of 60.
    fn synthetic(w: u32, h: u32) -> Self {
        let hdisplay = w as u16;
        let vdisplay = h as u16;
        // Fixed porches, always strictly increasing for any GOP-sized mode.
        let hsync_start = hdisplay.saturating_add(48);
        let hsync_end = hsync_start.saturating_add(32);
        let htotal = hsync_end.saturating_add(80);
        let vsync_start = vdisplay.saturating_add(3);
        let vsync_end = vsync_start.saturating_add(6);
        let vtotal = vsync_end.saturating_add(32);
        Self {
            clock_khz: clock_khz_for_refresh_mhz(htotal as u32, vtotal as u32, 60_000),
            hdisplay,
            hsync_start,
            hsync_end,
            htotal,
            vdisplay,
            vsync_start,
            vsync_end,
            vtotal,
            vrefresh: 60,
            // -hsync/-vsync. Made up like the rest, and nothing programs it:
            // the sync generator is already running, set by firmware.
            flags: DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_NVSYNC,
        }
    }

    /// The panel's own timing, from the preferred detailed timing of its EDID.
    ///
    /// `None` unless every number survives the trip into a `u16` and the mode
    /// states a refresh, because the fallback is the nominal mode above and
    /// that is strictly better than an illegal one: wlroots handed a mode
    /// whose sync runs past its total drops the output rather than picking
    /// another, and the desktop lands on the text console.
    fn from_panel(t: &edid::DetailedTiming) -> Option<Self> {
        let vrefresh = t.refresh_hz();
        if !t.is_valid() || vrefresh == 0 {
            return None;
        }
        // The uAPI field is 16 bits; a monitor that states more is not a
        // monitor this can describe, so fall back rather than truncate.
        let fit = |v: u32| (v <= u16::MAX as u32).then_some(v as u16);
        let mut flags = 0;
        if t.separate_sync {
            flags |= if t.hsync_positive {
                DRM_MODE_FLAG_PHSYNC
            } else {
                DRM_MODE_FLAG_NHSYNC
            };
            flags |= if t.vsync_positive {
                DRM_MODE_FLAG_PVSYNC
            } else {
                DRM_MODE_FLAG_NVSYNC
            };
        }
        if t.interlaced {
            flags |= DRM_MODE_FLAG_INTERLACE;
        }
        Some(Self {
            clock_khz: t.clock_khz,
            hdisplay: fit(t.hdisplay)?,
            hsync_start: fit(t.hsync_start)?,
            hsync_end: fit(t.hsync_end)?,
            htotal: fit(t.htotal)?,
            vdisplay: fit(t.vdisplay)?,
            vsync_start: fit(t.vsync_start)?,
            vsync_end: fit(t.vsync_end)?,
            vtotal: fit(t.vtotal)?,
            vrefresh,
            flags,
        })
    }
}

/// The native timing of the monitor firmware read at boot, if it stated one.
fn panel_timing() -> Option<edid::DetailedTiming> {
    let (block, len) = zcore_drivers::display::boot_edid()?;
    panel_timing_in(&block, len)
}

/// [`panel_timing`] with the bytes handed in, because the boot EDID is a
/// process-wide global that no test sets and every test reads.
///
/// `len` is how much of the block firmware actually read off the DDC line, and
/// the buffer is a fixed 128 bytes whatever that was: a short read leaves the
/// tail as whatever was there before. So a partial read is refused rather than
/// decoded, the same rule `/dev/fb0` applies to the same global.
fn panel_timing_in(block: &[u8], len: u32) -> Option<edid::DetailedTiming> {
    if (len as usize) < edid::BLOCK_LEN {
        return None;
    }
    edid::preferred_timing(block)
}

/// Build a `struct drm_mode_modeinfo` (68 bytes) for the mode at `w`x`h`.
///
/// The panel's own timing when its EDID describes exactly this mode, and the
/// nominal 60 Hz one otherwise. The refresh is not cosmetic: it is what the
/// compositor paces its repaints to, and what `set_vblank_period_from_modeinfo`
/// turns into the synthetic vblank period every `WAIT_VBLANK` and every flip
/// completion is timed against. Saying 60 to a 144 Hz panel throws away more
/// than half of its scanouts.
/// `DRM_MODE_FLAG_PIC_AR_MASK` is bits 19..=23 of `drm_mode_modeinfo.flags`;
/// `drm_mode_convert_umode` knows the codes 0 (none) to 4 (256:135) and
/// refuses the rest.
const DRM_MODE_FLAG_PIC_AR_SHIFT: u32 = 19;
const DRM_MODE_FLAG_PIC_AR_MAX: u32 = 4;

/// `drm_mode_validate_basic`, on a `struct drm_mode_modeinfo` as the ioctl
/// carries it, plus the aspect-ratio code check of `drm_mode_convert_umode`:
/// the modes the kernel refuses with EINVAL before any driver sees them. For a
/// mode that passes, its active area `(hdisplay, vdisplay)`.
fn modeinfo_active_area(m: &[u8; 68]) -> Option<(u32, u32)> {
    let u16_at = |i: usize| u16::from_ne_bytes([m[i], m[i + 1]]);
    let clock = u32::from_ne_bytes([m[0], m[1], m[2], m[3]]);
    let (hdisplay, hsync_start, hsync_end, htotal) = (u16_at(4), u16_at(6), u16_at(8), u16_at(10));
    let (vdisplay, vsync_start, vsync_end, vtotal) =
        (u16_at(14), u16_at(16), u16_at(18), u16_at(20));
    let flags = u32::from_ne_bytes([m[28], m[29], m[30], m[31]]);
    if (flags >> DRM_MODE_FLAG_PIC_AR_SHIFT) & 0x1f > DRM_MODE_FLAG_PIC_AR_MAX {
        return None;
    }
    if clock == 0 {
        return None;
    }
    if hdisplay == 0 || hsync_start < hdisplay || hsync_end < hsync_start || htotal < hsync_end {
        return None;
    }
    if vdisplay == 0 || vsync_start < vdisplay || vsync_end < vsync_start || vtotal < vsync_end {
        return None;
    }
    Some((hdisplay as u32, vdisplay as u32))
}

fn make_modeinfo(w: u32, h: u32) -> [u8; 68] {
    make_modeinfo_with(w, h, panel_timing().as_ref())
}

/// [`make_modeinfo`] with the panel's timing handed in, so a test can drive
/// both sources without touching the process-wide boot EDID.
fn make_modeinfo_with(w: u32, h: u32, panel: Option<&edid::DetailedTiming>) -> [u8; 68] {
    let mut m = [0u8; 68];
    // The resolution has to match, and that is the whole safety argument: the
    // preferred timing describes the panel's native mode, and firmware is free
    // to have programmed a different one (1080p on a 4K panel is the common
    // case). A timing for a mode that is not scanning out is a refresh rate
    // for a different mode, which is worse than admitting we do not know.
    let ml = panel
        .filter(|t| t.hdisplay == w && t.vdisplay == h)
        .and_then(Modeline::from_panel)
        .unwrap_or_else(|| Modeline::synthetic(w, h));
    m[0..4].copy_from_slice(&ml.clock_khz.to_ne_bytes());
    m[4..6].copy_from_slice(&ml.hdisplay.to_ne_bytes());
    m[6..8].copy_from_slice(&ml.hsync_start.to_ne_bytes());
    m[8..10].copy_from_slice(&ml.hsync_end.to_ne_bytes());
    m[10..12].copy_from_slice(&ml.htotal.to_ne_bytes());
    // hskew @12..14 = 0
    m[14..16].copy_from_slice(&ml.vdisplay.to_ne_bytes());
    m[16..18].copy_from_slice(&ml.vsync_start.to_ne_bytes());
    m[18..20].copy_from_slice(&ml.vsync_end.to_ne_bytes());
    m[20..22].copy_from_slice(&ml.vtotal.to_ne_bytes());
    // vscan @22..24 = 0
    m[24..28].copy_from_slice(&ml.vrefresh.to_ne_bytes()); // vrefresh (Hz)
    m[28..32].copy_from_slice(&ml.flags.to_ne_bytes());
    // type @32..36: DRM_MODE_TYPE_DRIVER(0x40) | DRM_MODE_TYPE_PREFERRED(0x08)
    m[32..36].copy_from_slice(&0x48u32.to_ne_bytes());
    // name @36..68 ("WxH")
    let mut name = [0u8; 32];
    let mut i = 0;
    let put = |buf: &mut [u8; 32], i: &mut usize, val: u32| {
        if val == 0 {
            if *i < buf.len() {
                buf[*i] = b'0';
                *i += 1;
            }
            return;
        }
        let mut digits = [0u8; 10];
        let mut n = 0;
        let mut v = val;
        while v > 0 {
            digits[n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
        }
        while n > 0 && *i < buf.len() {
            n -= 1;
            buf[*i] = digits[n];
            *i += 1;
        }
    };
    put(&mut name, &mut i, w);
    if i < name.len() {
        name[i] = b'x';
        i += 1;
    }
    put(&mut name, &mut i, h);
    m[36..68].copy_from_slice(&name);
    m
}

const I32_MIN_U64: u64 = i32::MIN as i64 as u64;
const MINUS_ONE_U64: u64 = -1i64 as u64;
const I32_MAX_U64: u64 = i32::MAX as u64;
const U32_MAX_U64: u64 = u32::MAX as u64;

/// Metadata served by `DRM_IOCTL_MODE_GETPROPERTY` for one property object:
/// flags, name, and the value/enum lists per its type (`drm-kms.rst` "KMS
/// Properties"). Range properties list `[min, max]`; object properties list
/// the object type they accept; enums list `(value, name)` pairs.
struct PropSpec {
    name: &'static str,
    flags: u32,
    values: &'static [u64],
    enums: &'static [(u64, &'static str)],
}

/// `drm_mode_object_find(dev, file, id, type)` for the objects that carry
/// properties: the object's type and its properties (`obj->properties`,
/// as OBJ_GETPROPERTIES lists them to an atomic client; an encoder has
/// none). `DRM_MODE_OBJECT_ANY` matches any type; any other type has to be
/// the object's own.
fn find_mode_object(
    obj_id: u32,
    obj_type: u32,
    atomic: bool,
) -> Option<(u32, alloc::vec::Vec<(u32, u64)>)> {
    let (kind, props) = if let Some(plane) = drm::get_plane(obj_id) {
        (DRM_MODE_OBJECT_PLANE, plane_props(&plane, atomic))
    } else if drm::get_crtc(obj_id).is_some() {
        (DRM_MODE_OBJECT_CRTC, crtc_props(atomic))
    } else if drm::get_connector(obj_id).is_some() {
        (DRM_MODE_OBJECT_CONNECTOR, connector_props(obj_id, atomic))
    } else if obj_id == drm::SYNTH_ENCODER_ID {
        (DRM_MODE_OBJECT_ENCODER, alloc::vec::Vec::new())
    } else {
        return None;
    };
    if obj_type != DRM_MODE_OBJECT_ANY && obj_type != kind {
        return None;
    }
    Some((kind, props))
}

/// An id list the way `drm_mode_getresources` and `drm_mode_getplane_res`
/// answer one: as many entries as the caller made room for (`count <
/// card_res->count_x`), so a short array gets a prefix, and the caller
/// reads the full length from the count written back. A null pointer
/// writes nothing (Linux would fault on it).
fn fill_id_list(ptr: u64, room: u32, ids: &[u32]) -> Result<()> {
    let fill = ids.len().min(room as usize);
    if ptr != 0 && fill > 0 {
        ucheck_n::<u32>(ptr as usize, fill)?;
        unsafe {
            core::ptr::copy_nonoverlapping(ids.as_ptr(), ptr as *mut u32, fill);
        }
    }
    Ok(())
}

/// A property list the way `drm_mode_object_get_properties` answers one
/// (`*arg_count_props > count` per entry): the ids and values of the first
/// `room` properties, and the caller reads the full length from the count
/// written back. Both arrays are written, so both pointers must be non-null.
fn fill_prop_list(props_ptr: u64, values_ptr: u64, room: u32, props: &[(u32, u64)]) -> Result<()> {
    let fill = props.len().min(room as usize);
    if fill > 0 && props_ptr != 0 && values_ptr != 0 {
        ucheck_n::<u32>(props_ptr as usize, fill)?;
        ucheck_n::<u64>(values_ptr as usize, fill)?;
        for (i, (pid, val)) in props.iter().take(fill).enumerate() {
            unsafe {
                *(props_ptr as *mut u32).add(i) = *pid;
                *(values_ptr as *mut u64).add(i) = *val;
            }
        }
    }
    Ok(())
}

/// `drm_property_change_valid_get`: whether `value` may be written to a
/// property of this spec. Immutable never; a range and a signed range by
/// their bounds; an object property takes 0 or an existing object of the
/// type in `values[0]`; a blob property 0 or an existing blob; an enum one
/// of its listed values.
fn property_change_valid(spec: &PropSpec, value: u64) -> bool {
    if spec.flags & DRM_MODE_PROP_IMMUTABLE != 0 {
        return false;
    }
    // `DRM_MODE_PROP_EXTENDED_TYPE` carries the object and signed-range
    // types; the legacy bits carry the rest.
    const EXTENDED_TYPE: u32 = 0x0000_ffc0;
    match spec.flags & EXTENDED_TYPE {
        DRM_MODE_PROP_OBJECT => {
            if value == 0 {
                return true;
            }
            if value > u32::MAX as u64 {
                return false;
            }
            let id = value as u32;
            match spec.values.first().copied() {
                Some(t) if t == DRM_MODE_OBJECT_FB as u64 => drm::get_fb(id).is_some(),
                Some(t) if t == DRM_MODE_OBJECT_CRTC as u64 => drm::get_crtc(id).is_some(),
                _ => false,
            }
        }
        DRM_MODE_PROP_SIGNED_RANGE => {
            let v = value as i64;
            v >= spec.values[0] as i64 && v <= spec.values[1] as i64
        }
        _ if spec.flags & DRM_MODE_PROP_RANGE != 0 => {
            value >= spec.values[0] && value <= spec.values[1]
        }
        _ if spec.flags & DRM_MODE_PROP_BLOB != 0 => {
            value == 0 || (value <= u32::MAX as u64 && drm::get_blob(value as u32).is_some())
        }
        _ => spec.values.contains(&value),
    }
}

/// The property table of the synthetic pipeline. Names, types and ranges
/// match Linux's standard properties (`drm_mode_create_standard_properties`,
/// `drm_plane_create_*`, `drm_connector_create_standard_properties`).
fn prop_spec(prop_id: u32) -> Option<PropSpec> {
    Some(match prop_id {
        PROP_TYPE => PropSpec {
            name: "type",
            flags: DRM_MODE_PROP_ENUM | DRM_MODE_PROP_IMMUTABLE,
            values: &[0, 1, 2],
            enums: &[(0, "Overlay"), (1, "Primary"), (2, "Cursor")],
        },
        PROP_EDID => PropSpec {
            name: "EDID",
            flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_IMMUTABLE,
            values: &[],
            enums: &[],
        },
        PROP_DPMS => PropSpec {
            name: "DPMS",
            flags: DRM_MODE_PROP_ENUM,
            values: &[0, 1, 2, 3],
            enums: &[(0, "On"), (1, "Standby"), (2, "Suspend"), (3, "Off")],
        },
        PROP_LINK_STATUS => PropSpec {
            name: "link-status",
            flags: DRM_MODE_PROP_ENUM,
            values: &[0, 1],
            enums: &[(0, "Good"), (1, "Bad")],
        },
        PROP_NON_DESKTOP => PropSpec {
            name: "non-desktop",
            flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_IMMUTABLE,
            values: &[0, 1],
            enums: &[],
        },
        PROP_FB_ID => PropSpec {
            name: "FB_ID",
            flags: DRM_MODE_PROP_OBJECT | DRM_MODE_PROP_ATOMIC,
            values: &[DRM_MODE_OBJECT_FB as u64],
            enums: &[],
        },
        PROP_CRTC_ID => PropSpec {
            name: "CRTC_ID",
            flags: DRM_MODE_PROP_OBJECT | DRM_MODE_PROP_ATOMIC,
            values: &[DRM_MODE_OBJECT_CRTC as u64],
            enums: &[],
        },
        PROP_CRTC_X => PropSpec {
            name: "CRTC_X",
            flags: DRM_MODE_PROP_SIGNED_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[I32_MIN_U64, I32_MAX_U64],
            enums: &[],
        },
        PROP_CRTC_Y => PropSpec {
            name: "CRTC_Y",
            flags: DRM_MODE_PROP_SIGNED_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[I32_MIN_U64, I32_MAX_U64],
            enums: &[],
        },
        PROP_CRTC_W => PropSpec {
            name: "CRTC_W",
            flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[0, I32_MAX_U64],
            enums: &[],
        },
        PROP_CRTC_H => PropSpec {
            name: "CRTC_H",
            flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[0, I32_MAX_U64],
            enums: &[],
        },
        PROP_SRC_X => PropSpec {
            name: "SRC_X",
            flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[0, U32_MAX_U64],
            enums: &[],
        },
        PROP_SRC_Y => PropSpec {
            name: "SRC_Y",
            flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[0, U32_MAX_U64],
            enums: &[],
        },
        PROP_SRC_W => PropSpec {
            name: "SRC_W",
            flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[0, U32_MAX_U64],
            enums: &[],
        },
        PROP_SRC_H => PropSpec {
            name: "SRC_H",
            flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[0, U32_MAX_U64],
            enums: &[],
        },
        PROP_ACTIVE => PropSpec {
            name: "ACTIVE",
            flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[0, 1],
            enums: &[],
        },
        PROP_MODE_ID => PropSpec {
            name: "MODE_ID",
            flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_ATOMIC,
            values: &[],
            enums: &[],
        },
        PROP_IN_FENCE_FD => PropSpec {
            name: "IN_FENCE_FD",
            flags: DRM_MODE_PROP_SIGNED_RANGE | DRM_MODE_PROP_ATOMIC,
            // `[-1, INT_MAX]` (`drm_mode_create_standard_properties`): -1 is
            // the "no fence" sentinel and there is no fd below it.
            values: &[MINUS_ONE_U64, I32_MAX_U64],
            enums: &[],
        },
        PROP_OUT_FENCE_PTR => PropSpec {
            name: "OUT_FENCE_PTR",
            flags: DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ATOMIC,
            values: &[0, u64::MAX],
            enums: &[],
        },
        PROP_FB_DAMAGE_CLIPS => PropSpec {
            name: "FB_DAMAGE_CLIPS",
            flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_ATOMIC,
            values: &[],
            enums: &[],
        },
        _ => return None,
    })
}

/// `(prop_id, value)` pairs attached to the synthetic connector. Atomic
/// properties (CRTC_ID) are only listed for clients that negotiated
/// `DRM_CLIENT_CAP_ATOMIC`, mirroring Linux's atomic-property filtering.
fn connector_props(connector_id: u32, atomic: bool) -> alloc::vec::Vec<(u32, u64)> {
    let mut props = alloc::vec::Vec::new();
    // DPMS reads back what the client last set, so a compositor that turned
    // the output off and re-reads the property sees "Off" rather than being
    // told the panel is lit. Link is "Good" and this is a desktop display.
    props.push((
        PROP_DPMS,
        if drm::crtc_blanked() {
            3 // Off
        } else {
            DRM_MODE_DPMS_ON
        },
    ));
    props.push((PROP_LINK_STATUS, 0));
    props.push((PROP_NON_DESKTOP, 0));
    if drm::get_connector_edid(connector_id).is_some() {
        props.push((PROP_EDID, edid_blob_id(connector_id) as u64));
    }
    if atomic {
        let (st, _) = drm::atomic_snapshot();
        let crtc = if st.active { drm::SYNTH_CRTC_ID } else { 0 };
        props.push((PROP_CRTC_ID, crtc as u64));
    }
    props
}

/// `(prop_id, value)` pairs attached to the synthetic CRTC (atomic-only).
fn crtc_props(atomic: bool) -> alloc::vec::Vec<(u32, u64)> {
    let mut props = alloc::vec::Vec::new();
    if atomic {
        let (st, _) = drm::atomic_snapshot();
        props.push((PROP_ACTIVE, st.active as u64));
        props.push((PROP_MODE_ID, st.mode_blob_id as u64));
        // Write-only for commits; readback is always 0 like Linux.
        props.push((PROP_OUT_FENCE_PTR, 0));
    }
    props
}

/// `(prop_id, value)` pairs attached to a plane: `type` for everyone, plus
/// the atomic plane state for atomic clients.
fn plane_props(plane: &drm::DrmPlane, atomic: bool) -> alloc::vec::Vec<(u32, u64)> {
    let mut props = alloc::vec::Vec::new();
    props.push((PROP_TYPE, plane.plane_type as u64));
    if atomic {
        let (st, crtc_fb) = drm::atomic_snapshot();
        let crtc = if crtc_fb != 0 { plane.crtc_id } else { 0 };
        props.push((PROP_FB_ID, crtc_fb as u64));
        props.push((PROP_CRTC_ID, crtc as u64));
        props.push((PROP_CRTC_X, st.crtc_x as i64 as u64));
        props.push((PROP_CRTC_Y, st.crtc_y as i64 as u64));
        props.push((PROP_CRTC_W, st.crtc_w as u64));
        props.push((PROP_CRTC_H, st.crtc_h as u64));
        props.push((PROP_SRC_X, st.src_x as u64));
        props.push((PROP_SRC_Y, st.src_y as u64));
        props.push((PROP_SRC_W, st.src_w as u64));
        props.push((PROP_SRC_H, st.src_h as u64));
        // Default "no in-fence" sentinel.
        props.push((PROP_IN_FENCE_FD, (-1i32) as u64));
        // Damage is per-commit state, never latched: Linux resets
        // FB_DAMAGE_CLIPS to 0 after each atomic commit, and 0 means "the
        // whole plane changed". Reading it back always returns 0.
        props.push((PROP_FB_DAMAGE_CLIPS, 0));
    }
    props
}

/// Fold one `(property, value)` of an atomic request into the IN_FENCE_FD the
/// commit will wait on: the last real fd named. -1 is the "no fence"
/// sentinel and leaves whatever came before it, which is what
/// `atomic_stage_on` does with the same property.
fn fold_in_fence(fd: &mut Option<i32>, prop_id: u32, value: u64) {
    if prop_id == PROP_IN_FENCE_FD && (value as i32) >= 0 {
        *fd = Some(value as i32);
    }
}

/// Walk the `(object, property, value)` triples of a `DRM_IOCTL_MODE_ATOMIC`
/// request, with Linux's bounds, and hand each one to `visit`. A failing
/// `visit` stops the walk and is the walk's error.
///
/// There are two readers of these arrays: the sync ioctl arm that stages the
/// commit, and `DrmDev::atomic_in_fence`, which runs first to find the
/// IN_FENCE_FD to sleep on. They used to be two copies of this loop, which is
/// a standing invitation for one to see a property the other does not -- the
/// commit waiting on a fence it will not stage, or staging one it never
/// waited on. One walk, so they cannot disagree.
///
/// The shape is the uAPI's: `props_ptr`/`prop_values_ptr` are *one* pair of
/// arrays shared by every object, and `count_props_ptr[i]` says how many of
/// them object `i` claims, running on from where the previous object stopped.
/// So each object's span is checked against the user mapping before it is
/// read, not the array as a whole -- its total length is never stated.
fn walk_atomic_props(
    req: &DrmModeAtomic,
    mut visit: impl FnMut(u32, u32, u64) -> Result<()>,
) -> Result<()> {
    if req.count_objs == 0 {
        return Ok(());
    }
    // The pipeline has 3 objects x <=16 properties; bound the user-array walk
    // well above that.
    if req.count_objs > 64 || req.objs_ptr == 0 || req.count_props_ptr == 0 {
        return Err(FsError::InvalidParam);
    }
    ucheck_n::<u32>(req.objs_ptr as usize, req.count_objs as usize)?;
    ucheck_n::<u32>(req.count_props_ptr as usize, req.count_objs as usize)?;
    let mut prop_idx = 0usize;
    for i in 0..req.count_objs as usize {
        let obj_id = unsafe { *(req.objs_ptr as *const u32).add(i) };
        let count_props = unsafe { *(req.count_props_ptr as *const u32).add(i) };
        if count_props > 64 {
            return Err(FsError::InvalidParam);
        }
        if count_props > 0 && (req.props_ptr == 0 || req.prop_values_ptr == 0) {
            return Err(FsError::InvalidParam);
        }
        let span = prop_idx + count_props as usize;
        ucheck_n::<u32>(req.props_ptr as usize, span)?;
        ucheck_n::<u64>(req.prop_values_ptr as usize, span)?;
        for _ in 0..count_props {
            let prop_id = unsafe { *(req.props_ptr as *const u32).add(prop_idx) };
            let value = unsafe { *(req.prop_values_ptr as *const u64).add(prop_idx) };
            prop_idx += 1;
            visit(obj_id, prop_id, value)?;
        }
    }
    Ok(())
}

/// Which of the three KMS object kinds an atomic request names. Resolving the
/// id and staging the property are split so the staging contract -- which
/// property each kind takes, and which errno the rest get -- can be exercised
/// without a display: `drm::get_plane` and friends answer `None` for every id
/// when no display is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicObject {
    Plane,
    Crtc,
    Connector,
}

/// Resolve an atomic request's object id to its kind, or `None` if no object
/// of the pipeline carries that id.
fn atomic_object(obj_id: u32) -> Option<AtomicObject> {
    if drm::get_plane(obj_id).is_some() {
        Some(AtomicObject::Plane)
    } else if drm::get_crtc(obj_id).is_some() {
        Some(AtomicObject::Crtc)
    } else if drm::get_connector(obj_id).is_some() {
        Some(AtomicObject::Connector)
    } else {
        None
    }
}

/// Stage one `(object, property, value)` triple of a `DRM_IOCTL_MODE_ATOMIC`
/// request into the software-KMS update, with Linux's error contract: an
/// unknown object or a property the object doesn't have is ENOENT; an illegal
/// value or a legacy/immutable property in an atomic commit is EINVAL.
fn atomic_stage(upd: &mut drm::AtomicUpdate, obj_id: u32, prop_id: u32, value: u64) -> Result<()> {
    let obj = atomic_object(obj_id).ok_or(FsError::EntryNotFound)?;
    atomic_stage_on(upd, obj, prop_id, value)
}

/// Whether `obj` carries property `prop_id` at all: the per-object
/// property lists of `plane_props` / `crtc_props` / `connector_props`, as an
/// atomic client sees them. `drm_mode_atomic_ioctl` resolves the property on
/// the object (`drm_mode_obj_find_prop_id`) before it looks at the value, so
/// a property some other object owns is ENOENT whatever value comes with it.
fn atomic_object_has(obj: AtomicObject, prop_id: u32) -> bool {
    match obj {
        AtomicObject::Plane => matches!(
            prop_id,
            PROP_TYPE
                | PROP_FB_ID
                | PROP_CRTC_ID
                | PROP_CRTC_X
                | PROP_CRTC_Y
                | PROP_CRTC_W
                | PROP_CRTC_H
                | PROP_SRC_X
                | PROP_SRC_Y
                | PROP_SRC_W
                | PROP_SRC_H
                | PROP_IN_FENCE_FD
                | PROP_FB_DAMAGE_CLIPS
        ),
        AtomicObject::Crtc => matches!(prop_id, PROP_ACTIVE | PROP_MODE_ID | PROP_OUT_FENCE_PTR),
        AtomicObject::Connector => matches!(
            prop_id,
            PROP_CRTC_ID | PROP_DPMS | PROP_EDID | PROP_LINK_STATUS | PROP_NON_DESKTOP
        ),
    }
}

/// [`atomic_stage`] once the object kind is known.
///
/// The value is checked against the property's own spec before any arm
/// looks at it, the way `drm_atomic_set_property` runs
/// `drm_property_change_valid_get` first: an immutable property, a range or
/// enum value the property does not advertise, an object id above 32 bits
/// or naming no framebuffer / CRTC, and a blob id naming no blob are all
/// EINVAL *at the property*, before the commit's check phase. Without it
/// `FB_ID = real_fb | 1 << 32` was truncated to the real framebuffer and
/// presented, `IN_FENCE_FD = 1 << 32` became a wait on fd 0, a CRTC_W of
/// `1 << 31` wrapped negative, and a framebuffer or mode blob that did not
/// exist was staged and answered ENOENT by the commit, which a compositor
/// reads as "the object vanished" rather than "the value is bad".
fn atomic_stage_on(
    upd: &mut drm::AtomicUpdate,
    obj: AtomicObject,
    prop_id: u32,
    value: u64,
) -> Result<()> {
    if !atomic_object_has(obj, prop_id) {
        return Err(FsError::EntryNotFound);
    }
    let spec = prop_spec(prop_id).ok_or(FsError::EntryNotFound)?;
    if !property_change_valid(&spec, value) {
        return Err(FsError::InvalidParam);
    }
    // Every cast below is exact: the range check above bounded the value to
    // the field's type (an object or blob id to 32 bits, a signed range to
    // i32, the unsigned ones to u32).
    match obj {
        AtomicObject::Plane => match prop_id {
            PROP_FB_ID => upd.plane_fb_id = Some(value as u32),
            PROP_CRTC_ID => upd.plane_crtc_id = Some(value as u32),
            PROP_CRTC_X => upd.crtc_x = Some(value as i32),
            PROP_CRTC_Y => upd.crtc_y = Some(value as i32),
            PROP_CRTC_W => upd.crtc_w = Some(value as u32),
            PROP_CRTC_H => upd.crtc_h = Some(value as u32),
            PROP_SRC_X => upd.src_x = Some(value as u32),
            PROP_SRC_Y => upd.src_y = Some(value as u32),
            PROP_SRC_W => upd.src_w = Some(value as u32),
            PROP_SRC_H => upd.src_h = Some(value as u32),
            // IN_FENCE_FD: -1 = none (ignore); the range is `[-1, INT_MAX]`,
            // so anything else is a real fd. It is waited for before the
            // commit presents -- see `DrmDev::atomic_in_fence_sleep`, which
            // runs in the async syscall path ahead of this sync arm. Staging
            // it here is still what makes the commit accept the property.
            PROP_IN_FENCE_FD => {
                let fd = value as i32;
                if fd >= 0 {
                    upd.in_fence_fd = Some(fd);
                }
            }
            PROP_FB_DAMAGE_CLIPS => upd.damage_clips = Some(value as u32),
            // "type" is immutable and was refused above; nothing else of the
            // plane's reaches here.
            _ => return Err(FsError::InvalidParam),
        },
        AtomicObject::Crtc => match prop_id {
            // ACTIVE is advertised as `[0, 1]`, which the range check held.
            PROP_ACTIVE => upd.active = Some(value != 0),
            PROP_MODE_ID => upd.mode_blob = Some(value as u32),
            // OUT_FENCE_PTR: userspace pointer that must receive an i32 fd.
            // NULL is ignored; non-null is staged for writeback after commit.
            PROP_OUT_FENCE_PTR => {
                if value != 0 {
                    ucheck(value as usize, core::mem::size_of::<i32>())?;
                }
                upd.out_fence_ptr = Some(value);
            }
            _ => return Err(FsError::EntryNotFound),
        },
        AtomicObject::Connector => match prop_id {
            PROP_CRTC_ID => upd.connector_crtc_id = Some(value as u32),
            // Properties the connector really has but that an atomic commit
            // cannot set: EDID / link-status / non-desktop are immutable and
            // were refused above; DPMS is legacy-only. Linux answers all
            // four EINVAL -- `drm_mode_atomic_ioctl` looks the property up
            // first and only then refuses it -- and ENOENT here would tell a
            // compositor that enumerated the property that it has since
            // vanished.
            _ => return Err(FsError::InvalidParam),
        },
    }
    Ok(())
}

/// Install a already-signaled sync_file into the caller's fd table, or `None`
/// if the process/context cannot allocate one. Used as an OUT_FENCE_PTR stub:
/// real out-fences need HW flip completion; a signaled fd keeps clients from
/// waiting forever or SIGBUS-ing on an uninitialized pointer.
fn try_signaled_out_fence_fd() -> Option<i32> {
    use crate::fs::SyncobjHandle;
    use crate::process::ProcessExt;
    use zircon_object::task::Thread;

    let thread = kernel_hal::thread::get_current_thread()?
        .downcast::<Thread>()
        .ok()?;
    let linux = thread.proc().try_linux()?;
    let handle = zcore_drivers::scheme::syncobj::create(true);
    let file = SyncobjHandle::new_sync_file(handle, 1);
    match linux.add_file(file) {
        Ok(fd) => Some(i32::from(fd)),
        Err(_) => {
            let _ = zcore_drivers::scheme::syncobj::destroy(handle);
            None
        }
    }
}

/// Write OUT_FENCE_PTR: TEST_ONLY / missing syncobj path → `-1`; otherwise a
/// signaled sync_file fd. Comment in callers: real out-fences need HW flip
/// completion.
fn write_out_fence_ptr(ptr: u64, test_only: bool) -> Result<()> {
    if ptr == 0 {
        return Ok(());
    }
    ucheck(ptr as usize, core::mem::size_of::<i32>())?;
    // Real out-fences need HW flip completion; until then prefer an
    // already-signaled sync_file so explicit-sync clients can proceed, else -1.
    let fd = if test_only {
        -1
    } else {
        try_signaled_out_fence_fd().unwrap_or(-1)
    };
    unsafe {
        *(ptr as *mut i32) = fd;
    }
    Ok(())
}

/// Per-boot budget for the `[drm-wsi]` klog traces. klog writes SYNCHRONOUSLY
/// to the UART with no level filter; if any session process turns out to POLL
/// the KMS query ioctls (rather than probing once at startup), an uncapped
/// trace becomes a console storm -- and a klog storm has starved input on
/// this kernel before (see the EXEC-failure dedup note in nouveau_uapi.rs).
/// ~48 lines cover a full vulkaninfo VK_KHR_display probe sequence with room
/// to spare; after that the tracer goes silent for the rest of the boot and
/// says so once.
/// The six KMS query ioctls Mesa's `wsi_display` issues, by the name a
/// dmesg reader recognises. `None` for anything else.
fn wsi_query_name(cmd: u32) -> Option<&'static str> {
    match cmd {
        DRM_IOCTL_MODE_GETRESOURCES => Some("GETRESOURCES"),
        DRM_IOCTL_MODE_GETCONNECTOR => Some("GETCONNECTOR"),
        DRM_IOCTL_MODE_GETENCODER => Some("GETENCODER"),
        DRM_IOCTL_MODE_GETCRTC => Some("GETCRTC"),
        DRM_IOCTL_MODE_GETPLANERESOURCES => Some("GETPLANERESOURCES"),
        DRM_IOCTL_MODE_GETPLANE => Some("GETPLANE"),
        _ => None,
    }
}

/// The DRM object a KMS query asks about, for the refusal trace: `0` for the
/// two that enumerate rather than name one (`GETRESOURCES`,
/// `GETPLANERESOURCES`), since those carry no id at all.
///
/// The id is a different field in each struct -- `drm_mode_get_connector`
/// puts `connector_id` after four pointers and three counts, `drm_mode_crtc`
/// puts `crtc_id` second -- so this reads each one by name rather than
/// assuming a common prefix, which would print a pointer as an id.
#[allow(unsafe_code)]
fn wsi_query_object_id(cmd: u32, data: usize) -> u32 {
    unsafe {
        match cmd {
            DRM_IOCTL_MODE_GETCONNECTOR => (*(data as *const DrmModeGetConnector)).connector_id,
            DRM_IOCTL_MODE_GETENCODER => (*(data as *const DrmModeGetEncoder)).encoder_id,
            DRM_IOCTL_MODE_GETCRTC => (*(data as *const DrmModeGetCrtc)).crtc_id,
            DRM_IOCTL_MODE_GETPLANE => (*(data as *const DrmModeGetPlane)).plane_id,
            _ => 0,
        }
    }
}

/// Whether to klog this refusal of a KMS query. Separate from
/// [`wsi_trace_take`] on purpose: a refusal is the one line worth a boot, and
/// it must not be spent by the successes that precede it.
///
/// Deduped by `(nr, object id)` rather than merely counted, because libdrm
/// retries (`drmModeGetConnector`'s `goto retry`) and a client that polls the
/// KMS queries would otherwise repeat one refusal until the budget is gone.
/// Each distinct refusal prints once; the table is small and fixed, so a
/// client inventing ids cannot grow it -- past the last slot the channel
/// simply stops, after saying so.
fn wsi_fail_take(nr: u32, id: u32) -> bool {
    use core::sync::atomic::{AtomicBool, AtomicU64};
    static SEEN: [AtomicU64; WSI_FAIL_SLOTS] = [const { AtomicU64::new(0) }; WSI_FAIL_SLOTS];
    static FULL_SAID: AtomicBool = AtomicBool::new(false);
    wsi_fail_take_in(&SEEN, &FULL_SAID, nr, id)
}

/// How many distinct refusals one boot reports.
const WSI_FAIL_SLOTS: usize = 16;

/// [`wsi_fail_take`] over a caller-supplied table.
///
/// The seam is here, BELOW the global, rather than around the whole function:
/// the kernel and a test run the identical claim-a-slot code, and a test gets
/// its own table instead of competing for the one every other test in this
/// binary is also filling through the dispatch wrapper.
fn wsi_fail_take_in(
    seen: &[core::sync::atomic::AtomicU64],
    full_said: &core::sync::atomic::AtomicBool,
    nr: u32,
    id: u32,
) -> bool {
    use core::sync::atomic::Ordering;
    // `(nr << 32) | id`, +1 so a zeroed slot reads as "empty" rather than as
    // "GETRESOURCES on object 0" -- which is exactly the entry GETRESOURCES
    // would claim, and the one refusal that needs no id to be worth printing.
    let key = (((nr as u64) << 32) | id as u64) + 1;
    for slot in seen.iter() {
        match slot.compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed) {
            // Claimed an empty slot: first sight of this refusal.
            Ok(_) => return true,
            // Occupied. By this same refusal? Then it has been reported.
            Err(prev) if prev == key => return false,
            Err(_) => continue,
        }
    }
    if !full_said.swap(true, Ordering::Relaxed) {
        kernel_hal::klog_info!(
            "[drm-wsi] {} distinct KMS-query refusals reported -- silencing the rest \
             for this boot",
            seen.len()
        );
    }
    false
}

fn wsi_trace_take() -> bool {
    use core::sync::atomic::{AtomicU32, Ordering};
    static BUDGET: AtomicU32 = AtomicU32::new(0);
    const MAX: u32 = 48;
    let n = BUDGET.fetch_add(1, Ordering::Relaxed);
    if n == MAX {
        kernel_hal::klog_info!(
            "[drm-wsi] trace budget ({} lines) exhausted -- silencing for this boot \
             (something polls the KMS queries; capped to protect the console path)",
            MAX
        );
    }
    n < MAX
}

/// The canonical encoding of a core DRM ioctl: the `_IOC` word whose
/// `_IOC_SIZE` is the struct layout [`DrmDev::drm_ioctl_dispatch`] parses.
///
/// Linux never dispatches on the encoded command. `drm_ioctl()` takes
/// `nr = _IOC_NR(cmd)`, looks the handler up in `drm_ioctls[]` by that number
/// alone, and then *reconciles* the caller's `_IOC_SIZE(cmd)` with the size of
/// the struct the kernel parses (`drm_ioctl_kernel`: allocate
/// `max(in_size, out_size, drv_size)`, copy the caller's bytes in, zero the
/// rest, run the handler, copy `out_size` bytes back). That is the entire
/// reason a libdrm built against a 2019 `drm.h` keeps working on a 2026 kernel,
/// and why a struct may grow a trailing field without a flag day.
///
/// This tree matched the full 32-bit command instead, so every struct that ever
/// grew a field became a *different* ioctl that fell through to the driver and
/// out as ENOSYS. It cost three separate hand-patches already — the deadline
/// sizes of `SYNCOBJ_WAIT`/`TIMELINE_WAIT`, `SYNCOBJ_HANDLE_TO_FD` matched by
/// NR in the syscall layer, `PRIME_*` likewise — each found only after a client
/// broke on real hardware. Returning `None` here means "not a core ioctl we
/// know": driver-private numbers (`DRM_COMMAND_BASE..DRM_COMMAND_END`) and
/// anything unrecognised pass through untouched.
fn canonical_drm_ioctl(nr: u32) -> Option<u32> {
    Some(match nr {
        0x00 => DRM_IOCTL_VERSION,
        0x01 => DRM_IOCTL_GET_UNIQUE,
        0x02 => DRM_IOCTL_GET_MAGIC,
        0x07 => DRM_IOCTL_SET_VERSION,
        0x09 => DRM_IOCTL_GEM_CLOSE,
        0x0A => DRM_IOCTL_GET_CLIENT,
        0x0C => DRM_IOCTL_GET_CAP,
        0x0D => DRM_IOCTL_SET_CLIENT_CAP,
        0x11 => DRM_IOCTL_AUTH_MAGIC,
        0x1E => DRM_IOCTL_SET_MASTER,
        0x1F => DRM_IOCTL_DROP_MASTER,
        0x3A => DRM_IOCTL_WAIT_VBLANK,
        0xA0 => DRM_IOCTL_MODE_GETRESOURCES,
        0xA1 => DRM_IOCTL_MODE_GETCRTC,
        0xA2 => DRM_IOCTL_MODE_SETCRTC,
        0xA3 => DRM_IOCTL_MODE_CURSOR,
        0xA4 => DRM_IOCTL_MODE_GETGAMMA,
        0xA5 => DRM_IOCTL_MODE_SETGAMMA,
        0xA6 => DRM_IOCTL_MODE_GETENCODER,
        0xA7 => DRM_IOCTL_MODE_GETCONNECTOR,
        0xAA => DRM_IOCTL_MODE_GETPROPERTY,
        0xAB => DRM_IOCTL_MODE_SETPROPERTY,
        0xAC => DRM_IOCTL_MODE_GETPROPBLOB,
        0xAD => DRM_IOCTL_MODE_GETFB,
        0xAE => DRM_IOCTL_MODE_ADDFB,
        0xAF => DRM_IOCTL_MODE_RMFB,
        0xB0 => DRM_IOCTL_MODE_PAGE_FLIP,
        0xB1 => DRM_IOCTL_MODE_DIRTYFB,
        0xB2 => DRM_IOCTL_MODE_CREATE_DUMB,
        0xB3 => DRM_IOCTL_MODE_MAP_DUMB,
        0xB4 => DRM_IOCTL_MODE_DESTROY_DUMB,
        0xB5 => DRM_IOCTL_MODE_GETPLANERESOURCES,
        0xB6 => DRM_IOCTL_MODE_GETPLANE,
        0xB7 => DRM_IOCTL_MODE_SETPLANE,
        0xB8 => DRM_IOCTL_MODE_ADDFB2,
        0xB9 => DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
        0xBA => DRM_IOCTL_MODE_OBJ_SETPROPERTY,
        0xBB => DRM_IOCTL_MODE_CURSOR2,
        0xBC => DRM_IOCTL_MODE_ATOMIC,
        0xBD => DRM_IOCTL_MODE_CREATEPROPBLOB,
        0xBE => DRM_IOCTL_MODE_DESTROYPROPBLOB,
        0xBF => DRM_IOCTL_SYNCOBJ_CREATE,
        0xC0 => DRM_IOCTL_SYNCOBJ_DESTROY,
        0xC3 => DRM_IOCTL_SYNCOBJ_WAIT,
        0xC4 => DRM_IOCTL_SYNCOBJ_RESET,
        0xC5 => DRM_IOCTL_SYNCOBJ_SIGNAL,
        0xC7 => DRM_IOCTL_MODE_LIST_LESSEES,
        0xCA => DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
        0xCB => DRM_IOCTL_SYNCOBJ_QUERY,
        0xCC => DRM_IOCTL_SYNCOBJ_TRANSFER,
        0xCD => DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
        0xCE => DRM_IOCTL_MODE_GETFB2,
        0xD0 => DRM_IOCTL_MODE_CLOSEFB,
        _ => return None,
    })
}

/// Whether `nr` is a core DRM ioctl number at all, i.e. NOT in the
/// driver-private `DRM_COMMAND_BASE..DRM_COMMAND_END` window. Linux answers an
/// unknown core number with EINVAL (`drm_ioctl`: `if (!func) ... -EINVAL`),
/// never ENOSYS.
pub(crate) fn is_core_drm_nr(nr: u32) -> bool {
    !(DRM_COMMAND_BASE..DRM_COMMAND_END).contains(&nr)
}

const DRM_COMMAND_BASE: u32 = 0x40;
const DRM_COMMAND_END: u32 = 0xA0;

const IOC_WRITE_DIR: u32 = 1 << 30;
const IOC_READ_DIR: u32 = 2 << 30;

const fn ioc_size(cmd: u32) -> usize {
    ((cmd >> 16) & 0x3fff) as usize
}

/// The byte counts `drm_ioctl()` derives for one call: what to copy in from the
/// caller, what to copy back, and how big the kernel-side struct must be.
#[derive(Debug, PartialEq, Eq)]
struct IoctlSizes {
    in_size: usize,
    out_size: usize,
    ksize: usize,
}

/// `drm_ioctl()`'s size arithmetic, verbatim:
///
/// ```text
/// in_size = out_size = _IOC_SIZE(cmd);
/// if ((cmd & ioctl->cmd & IOC_IN)  == 0) in_size  = 0;
/// if ((cmd & ioctl->cmd & IOC_OUT) == 0) out_size = 0;
/// ksize = max(max(in_size, out_size), drv_size);
/// ```
///
/// Direction is INTERSECTED with the handler's own, which is what stops a
/// caller from encoding `_IOC_READ` on a write-only ioctl to have the kernel
/// copy a struct back that it was never going to fill.
fn reconcile_sizes(cmd: u32, canon: u32) -> IoctlSizes {
    let user_size = ioc_size(cmd);
    let in_size = if cmd & canon & IOC_WRITE_DIR != 0 {
        user_size
    } else {
        0
    };
    let out_size = if cmd & canon & IOC_READ_DIR != 0 {
        user_size
    } else {
        0
    };
    IoctlSizes {
        in_size,
        out_size,
        ksize: core::cmp::max(core::cmp::max(in_size, out_size), ioc_size(canon)),
    }
}

/// `drm_ioctl()`: reconcile the size the client encoded with the size we parse,
/// then dispatch on the canonical command.
///
/// Linux computes, for the handler found by NR:
///
/// ```text
/// in_size  = out_size = _IOC_SIZE(cmd)
/// if ((cmd & ioctl->cmd & IOC_IN)  == 0) in_size  = 0;
/// if ((cmd & ioctl->cmd & IOC_OUT) == 0) out_size = 0;
/// ksize = max(max(in_size, out_size), drv_size)
/// ```
///
/// then allocates `ksize` bytes, copies `in_size` from the caller, **zeroes the
/// tail**, runs the handler and copies `out_size` back. A caller whose struct is
/// shorter than ours sees the fields it does not know about default to zero; one
/// whose struct is longer keeps its trailing bytes untouched. We do the same,
/// and only when the sizes actually differ — the overwhelmingly common case is
/// an exact match, which dispatches straight through with no copy at all.
fn drm_ioctl(dev: &DrmDev, cmd: u32, data: usize) -> Result<usize> {
    // Read the first field BEFORE dispatching: for an `_IOWR` it is an input
    // the handler is free to overwrite with its reply, and the trail wants to
    // say what was ASKED (`GETPARAM` param 13, `GEM_INFO` handle 7), not what
    // came back -- the answer is already in `ret`.
    let arg0 = first_arg_word(cmd, data);
    let ret = drm_ioctl_reconciled(cmd, data, |canon, kdata| {
        dev.drm_ioctl_dispatch(canon, kdata)
    });
    drm_trail::record(
        drm::current_pid() as u32,
        drm::current_tid() as u32,
        cmd,
        arg0,
        trail_ret(&ret),
    );
    ret
}

/// Record a DRM ioctl the SYSCALL layer answered by itself.
///
/// Three families never reach [`drm_ioctl`] because they have to touch the
/// process fd table: PRIME dma-buf export/import, syncobj export/import, and
/// `SYNCOBJ_EVENTFD`. They are also the three a Vulkan driver leans on
/// hardest, so a trail without them would skip exactly the calls that hand
/// NVK an object it then dereferences.
///
/// `data` is read here, *after* the call rather than before it as
/// [`drm_ioctl`] does: all three keep their subject (`handle`) in the first
/// word and write their reply into a later field, so the distinction the
/// other path needs does not arise.
pub fn trail_record(cmd: u32, data: usize, ret: i64) {
    drm_trail::record(
        drm::current_pid() as u32,
        drm::current_tid() as u32,
        cmd,
        first_arg_word(cmd, data),
        ret,
    );
}

/// The first 64-bit word of an ioctl's argument struct, or 0 when there is
/// none or it cannot be read.
///
/// Every DRM argument struct starts on an 8-byte boundary and its first field
/// is the one that names the subject: `param`, `handle`, `channel`,
/// `crtc_id`, `capability`. Structs shorter than 8 bytes (`MODE_RMFB` and
/// friends carry a bare `__u32`) read as 0 rather than over their end.
///
/// This must not be able to fault: it runs on the ordinary ioctl path for
/// every call, including ones whose argument the dispatcher is about to
/// reject. `UserInPtr::read` is the checked read the rest of this file uses,
/// and its error is simply "no value".
fn first_arg_word(cmd: u32, data: usize) -> u64 {
    if !arg_word_readable(cmd, data) {
        return 0;
    }
    kernel_hal::user::UserInPtr::<u64>::from(data)
        .read()
        .unwrap_or(0)
}

/// Whether [`first_arg_word`] may read eight bytes at `data`.
///
/// Split out from the read itself so it can be tested: the read is the one
/// part of this that cannot run on the host, and the decision NOT to read is
/// the part that keeps a trail entry from turning into a fault.
fn arg_word_readable(cmd: u32, data: usize) -> bool {
    ioc_size(cmd) >= 8 && ucheck(data, 8).is_ok()
}

/// An ioctl result as the trail records it: a count, or a negative errno.
fn trail_ret(ret: &Result<usize>) -> i64 {
    match ret {
        Ok(n) => *n as i64,
        Err(e) => -(crate::error::LxError::from(e) as i64),
    }
}

/// [`drm_ioctl`] with the dispatch injected, so the copy-in / zero-fill /
/// copy-back protocol can be tested without a device.
#[allow(unsafe_code)]
fn drm_ioctl_reconciled(
    cmd: u32,
    data: usize,
    dispatch: impl FnOnce(u32, usize) -> Result<usize>,
) -> Result<usize> {
    let nr = cmd & 0xff;
    let canon = match canonical_drm_ioctl(nr) {
        Some(c) => c,
        // Driver-private or unrecognised: hand it to the dispatcher unchanged,
        // which routes it to the driver (Linux consults the driver's own ioctl
        // table for this range) or fails it.
        None => {
            ucheck(data, ioc_size(cmd))?;
            return dispatch(cmd, data);
        }
    };
    if ioc_size(cmd) == ioc_size(canon) {
        // Fast path: the client's struct is the one the arms parse. No bounce
        // buffer, no copy -- byte-for-byte the behaviour before this layer.
        ucheck(data, ioc_size(cmd))?;
        return dispatch(canon, data);
    }
    let IoctlSizes {
        in_size,
        out_size,
        ksize,
    } = reconcile_sizes(cmd, canon);
    if ksize == 0 {
        return dispatch(canon, data);
    }
    // `access_ok()` over the range the CLIENT encoded, before either copy.
    ucheck(data, core::cmp::max(in_size, out_size))?;
    let mut kdata = alloc::vec![0u8; ksize];
    if in_size != 0 {
        // SAFETY: `ucheck` proved `data..data + in_size` is user memory, and
        // `kdata` is `ksize >= in_size` bytes we own. The tail stays zero, which
        // is the whole point: fields a shorter client did not send must read as
        // zero, not as whatever the arm would otherwise find.
        unsafe {
            core::ptr::copy_nonoverlapping(data as *const u8, kdata.as_mut_ptr(), in_size);
        }
    }
    // `as_mut_ptr`: the arms cast this straight to `*mut T` and write their
    // reply through it, so the pointer they get has to carry write provenance.
    let ret = dispatch(canon, kdata.as_mut_ptr() as usize)?;
    if out_size != 0 {
        // SAFETY: same range, checked above; `kdata` holds at least `out_size`.
        unsafe {
            core::ptr::copy_nonoverlapping(kdata.as_ptr(), data as *mut u8, out_size);
        }
    }
    Ok(ret)
}

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

/// What a failed present costs the ioctl that asked for it.
///
/// The bug these guard: `SETCRTC` answered `EIO` whenever the present path
/// returned false, and wlroots' legacy backend reads a failed
/// `drmModeSetCrtc` as the *output* being broken. It retried the modeset every
/// frame and never reached page-flipping — an 8 Hz storm of
/// `connector HDMI-A-1: Failed to set CRTC: I/O error` for as long as the
/// session lasted, and a desktop that never appeared, because one frame could
/// not be copied.
#[cfg(test)]
mod present_failure_policy_tests;

#[cfg(test)]
mod render_node_and_mode_tests;

/// Linux dispatches a DRM ioctl on its NUMBER and reconciles the struct size
/// afterwards (`drm_ioctl`/`drm_ioctl_kernel`). Matching the full 32-bit
/// command instead turns every struct that ever grew a trailing field into an
/// unknown ioctl, which is how this tree lost `SYNCOBJ_HANDLE_TO_FD` (24 B),
/// the deadline sizes of `SYNCOBJ_WAIT`/`TIMELINE_WAIT`, and `PRIME_*` --
/// each found only when a client broke on real hardware.
#[cfg(test)]
mod ioctl_size_reconciliation_tests;

/// The kernel-side work an OpenGL application actually makes the kernel do,
/// replayed through the real ioctl entry point.
///
/// This is `glxgears` (and every scene of `glmark2`) reduced to the part that
/// lives here. Neither app is interesting to us for its triangles: what they do
/// to this tree is a fixed sequence of DRM ioctls per frame, and every bug that
/// has ever stopped them was in that sequence rather than in the rendering ---
/// an ioctl number that did not match what libdrm encoded, a framebuffer freed
/// under a flip, a completion event that never arrived so the frame loop blocked
/// in `poll()` forever. All of that is reachable with no GPU, no Mesa and no
/// display, because it is all bookkeeping.
///
/// These tests deliberately run with NO output attached, which is the case the
/// present path answers with `PresentError::NoDisplay` and the ioctl arms
/// report as success --- so the sequence runs end to end and what is pinned here
/// is the bookkeeping on either side of the copy. The copy itself, and
/// everything that decides which pixels it moves, is in
/// [`kms_scanout_tests`](super::kms_scanout_tests), which attaches an emulated
/// output first.
#[cfg(test)]
mod gl_client_sequence_tests;

/// The present itself: what pixels actually reach the screen, with an emulated
/// DRM/KMS output attached.
///
/// [`gl_client_sequence_tests`] above replays a GL client's ioctl sequence with
/// nothing to scan out to, so it pins the bookkeeping on either side of the
/// copy. The copy was the half no host test could reach: `primary_display()` is
/// `all_display().first()` and a unit-test binary never runs the hosted kernel's
/// device bring-up, so `software_kms_active()` was false, the synthetic
/// CRTC/connector/plane did not exist, and every present stopped at
/// `PresentError::NoDisplay` before touching a pixel.
///
/// [`kms_emu`](super::super::kms_emu) attaches one for the duration of a test.
/// It is a real [`DisplayScheme`](kernel_hal::drivers::scheme::DisplayScheme)
/// over a heap buffer, so the present goes through `blit_from` exactly as a UEFI
/// GOP or a GPU BAR1 aperture does; it can carry a padded scanline and claim to
/// be write-combining, which is what makes the two mitigations that live in this
/// path -- the 16-pixel line expansion and the non-temporal store loop -- testable
/// at all. Every pixel starts at [`UNTOUCHED`](super::super::kms_emu::UNTOUCHED),
/// so "the present wrote this" and "the present left this alone" are
/// distinguishable; that distinction is the whole point when the bug is writing
/// too few columns or too many.
#[cfg(test)]
mod kms_scanout_tests;

/// The hardware-KMS path: what changes when a driver owns scanout.
///
/// [`kms_scanout_tests`] covers the software path, where the DRM core blits dumb
/// buffers into the framebuffer itself -- what runs under QEMU. It is not the
/// only path on real hardware: with `nvidia.hwflip` (or NVC57E surface flip) the
/// NVIDIA driver declares `has_hardware_kms()` and four things change at once.
/// ADDFB2 asks the driver for its OWN framebuffer object, the flip goes to the
/// driver by that private id, the software blit is skipped, and the pointer has
/// to be put back on top afterwards because the driver's flip replaced the whole
/// scanout. The topology stops being synthetic too, and starts being filtered
/// and de-duplicated across drivers.
///
/// None of it could run in CI: it needs a driver that claims hardware KMS, and
/// the only one is `NvidiaGpu` behind MMIO. [`kms_emu::EmuGpu`] is one that
/// records what it was asked to do -- including refusing a flip, which is the
/// case the software fallback exists for and the one that decides whether a
/// failed flip leaves the panel dark.
#[cfg(test)]
mod hw_kms_tests;

#[cfg(test)]
mod property_table_tests;

#[cfg(test)]
mod atomic_walk_tests;

#[cfg(test)]
mod syncobj_wait_routing_tests;

#[cfg(test)]
mod master_tests;
#[cfg(test)]
mod version_tests;

#[cfg(test)]
mod blob_id_space_tests;

/// CREATE_DUMB bpp, chardev write, ADDFB errno, and DESTROYPROPBLOB EPERM —
/// small contracts that used to lie to clients.
#[cfg(test)]
mod dumb_write_and_addfb_errno_tests;

/// The compute node's own ioctl (`DRM_ECLIPSE_COMPUTE_NR`), which has no tests
/// at all and is the one place in this file where a client's own size encoding
/// decides how much memory the kernel writes.
///
/// Every other DRM ioctl is reconciled against the kernel's struct before it
/// reaches a handler: `drm_ioctl_reconciled` looks the command up in
/// `canonical_drm_ioctl`, allocates a kernel buffer of the KERNEL's size, and
/// copies in and out of that. A driver-private command has no canonical entry,
/// so it takes the other branch -- `ucheck` against the size the CLIENT
/// encoded, then straight to the handler with the user address. And this
/// handler writes all 536 bytes of `struct drm_eclipse_compute`. The floor that
/// refuses a short encoding is therefore the whole defence, it is an open-coded
/// copy of `ioc_size()`, and nothing exercised either side of it.
#[cfg(test)]
mod compute_node_tests;

/// The DRM event queue as a client sees it: one queue per open file, read whole
/// events at a time, with two different errnos for "nothing yet" and "your
/// buffer is too small".
///
/// A compositor's main loop is `poll` then `read`, and both answers are
/// load-bearing. `EAGAIN` means "come back"; `EINVAL` means "your buffer is
/// wrong" -- and this is the one place the tree deliberately diverges from what
/// looks natural, because returning `EAGAIN` for a short buffer livelocks: the
/// queue is still non-empty, so the file stays readable and a blocking reader's
/// wait resolves instantly, forever. The existing tests of this file read events
/// but accept `Err(_) | Ok(0)` where they do, so none of them can tell those two
/// errnos apart, and none of them has two clients open at once.
#[cfg(test)]
mod event_queue_tests;

/// `DRM_IOCTL_MODE_ATOMIC` end to end, and the `OUT_FENCE_PTR` writeback in
/// particular.
///
/// No test in this file issued this ioctl at all: the atomic uAPI is opt-in via
/// the `drm.atomic` cmdline flag, so `SET_CLIENT_CAP(ATOMIC)` refused every
/// client and the whole arm was unreachable. `drm::set_atomic_enabled` makes it
/// reachable, which matters because `OUT_FENCE_PTR` is a pointer the client
/// hands the kernel to write a file descriptor into, and the decision table
/// around that write is subtle: Linux writes `-1` on `TEST_ONLY` and on failure,
/// a real fd on success, and -- the part that is easy to get backwards -- the
/// write happens BEFORE the commit's own error is returned to the client. A
/// client that gets an error with its fence slot untouched reads whatever was in
/// it, which for an uninitialised `int` is a descriptor belonging to something
/// else.
#[cfg(test)]
mod out_fence_tests;

#[cfg(test)]
mod empty_commit_tests;

#[cfg(test)]
mod off_crtc_event_tests;

#[cfg(test)]
mod wait_vblank_validation_tests;

#[cfg(test)]
mod prime_import_close_tests;

#[cfg(test)]
mod client_cap_tests;

#[cfg(test)]
mod mmap_bounds_tests;

#[cfg(test)]
mod present_fence_tests;

#[cfg(test)]
mod card_fd_wait_tests;

#[cfg(test)]
mod syncobj_array_tests;

#[cfg(test)]
mod addfb2_validation_tests;

#[cfg(test)]
mod trail_tests;

/// The async pre-waits resolve the syncobj table exactly once per look.
///
/// `poll_pending()` takes the table lock and runs the module's `resolve_locked`;
/// so does the `wait_ready()` it used to sit in front of, under the same lock.
/// Every look of a parked wait therefore walked the pending-fence list twice,
/// and a walk reads the landing zone of every fence still in flight out of
/// uncached pinned sysmem -- a trip off the CPU per fence, several looks per
/// frame, on a desktop where the other CPU is the compositor trying to take
/// that same lock to end the wait.
///
/// There is no unit test that can catch the second call coming back: the pre-
/// waits need a live `DrmDev`, a process and a GPU, and what they cost is a
/// count, not an answer. The measurement lives in the glxgears bench in
/// `drivers` (`a_parked_wait_walks_the_pending_fences_once_a_look_and_not_twice`),
/// which drives the real `syncobj` module through both shapes; this guards the
/// shape on the kernel side, where the bench cannot reach.
///
/// `syncobj_file.rs` keeps its `poll_pending()` on purpose and is not covered
/// here: it is followed by `query()`, which reads the table WITHOUT resolving
/// it, so there the call is the only thing that advances a landed fence.
#[cfg(test)]
mod pre_wait_resolve_tests;

/// Mesa's `VK_KHR_display` probe, driven exactly as libdrm and `wsi_display`
/// drive it.
///
/// `vulkaninfo` dies on this machine's RTX pair with
/// `vkGetPhysicalDeviceDisplayPlanePropertiesKHR failed with
/// ERROR_OUT_OF_HOST_MEMORY`, and that code is not a memory shortage: Mesa's
/// `wsi_get_connectors` returns it when `drmModeGetResources` OR
/// `drmModeGetConnector` hands back NULL, and libdrm hands back NULL when the
/// ioctl fails. So the whole error is "one of two KMS queries returned an
/// errno", and the only way to see which from here is to make the two-pass
/// sequence libdrm actually makes -- `memclear`, probe for the counts,
/// allocate, ask again -- rather than the one-shot call the rest of these
/// tests make.
#[cfg(test)]
mod wsi_display_probe_tests;

/// The refusal channel of the `VK_KHR_display` probe.
///
/// Mesa turns any failing KMS query into `VK_ERROR_OUT_OF_HOST_MEMORY`, so the
/// one thing a boot has to produce is *which* query refused *which* object.
/// These cover the two ways that line used to be lost: it shared its budget
/// with the successes, and it was keyed on nothing, so a retry loop repeated
/// it instead of other refusals being reported.
#[cfg(test)]
mod wsi_refusal_trace_tests;

/// What `ADDFB2`'s modifier word is allowed to mean.
///
/// Every value here is built with [`nvidia_block_linear_2d`], the macro's own
/// arithmetic from `drm_fourcc.h`, so a field that moves shows up as the
/// builder and the decoder disagreeing rather than as both being wrong the
/// same way.
#[cfg(test)]
mod scanout_modifier_tests;
