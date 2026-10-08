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
mod present_failure_policy_tests {
    use super::*;
    use crate::fs::LxError;

    /// Linux fails `drm_mode_setcrtc` for a fb id it cannot look up, and does
    /// it with `ENOENT` ("Unknown FB ID"). Keep failing that one — it is a real
    /// client error — but with the errno that points at fb lifetime instead of
    /// at the bus.
    #[test]
    fn an_unknown_fb_id_still_fails_the_ioctl_but_as_enoent() {
        let _serialised = drm::test_globals::lock();
        assert_eq!(
            present_failed("SETCRTC", 7, 1, drm::PresentError::NoSuchFb),
            Err(FsError::EntryNotFound),
        );
        assert_eq!(
            LxError::from(FsError::EntryNotFound),
            LxError::ENOENT,
            "EntryNotFound is the arm's way of spelling ENOENT",
        );
    }

    /// Everything else is a frame that could not be copied, not a modeset that
    /// could not be programmed. Report it and carry on: the CRTC keeps its
    /// binding, the compositor keeps running, and the next present gets another
    /// chance — instead of the output being written off for good.
    #[test]
    fn a_frame_that_could_not_be_copied_does_not_fail_the_modeset() {
        let _serialised = drm::test_globals::lock();
        for reason in [drm::PresentError::NoDisplay, drm::PresentError::NoBacking] {
            drm::set_crtc_fb(1, 0);
            assert_eq!(
                present_failed("SETCRTC", 7, 1, reason),
                Ok(()),
                "{:?} must not take the whole output down",
                reason,
            );
            // Answering 0 means the modeset happened, so the readback has to
            // agree: a caller that asks GETCRTC which fb is on the CRTC must
            // be told the one it just set, not the one before it.
            assert_eq!(
                drm::crtc_fb(),
                7,
                "{:?} left GETCRTC naming a different fb than SETCRTC accepted",
                reason,
            );
        }
    }

    /// The console trace is bounded. A per-frame line on this path is exactly
    /// the flood that has wedged spinlocks on a slow serial console before, and
    /// the failure it reports is a steady state, not a one-off.
    #[test]
    fn the_console_trace_is_bounded_however_long_the_storm_runs() {
        let _serialised = drm::test_globals::lock();
        PRESENT_FAIL_TRACED.store(0, Ordering::Relaxed);
        for _ in 0..10_000 {
            let _ = present_failed("SETCRTC", 7, 1, drm::PresentError::NoDisplay);
        }
        let traced = PRESENT_FAIL_TRACED
            .load(Ordering::Relaxed)
            .min(PRESENT_FAIL_TRACE_BUDGET);
        assert_eq!(
            traced, PRESENT_FAIL_TRACE_BUDGET,
            "the budget is spent exactly once, not per call",
        );
    }
}

#[cfg(test)]
mod render_node_and_mode_tests {
    //! Two things in this file that regressed once each and had no test.
    //!
    //! The render node's allow-list used to read the ioctl TYPE byte where it
    //! meant the NR. Every DRM ioctl has type `'d'` (0x64), and 0x64 falls
    //! inside the driver-private `0x40..=0x9F` arm, so the filter matched
    //! *everything*: an unprivileged render client could issue modesetting.
    //!
    //! And the synthetic mode's porches were once computed as +10%/+5%, which
    //! put `hsync_end` past `htotal` at 1366x768. That is MODE_H_ILLEGAL, and
    //! compositors that recompute the refresh from the porches then advertised
    //! 55-59 Hz instead of 60.

    use super::*;

    /// Decode `struct drm_mode_modeinfo` far enough to check its timings.
    fn timings(m: &[u8; 68]) -> (u32, [u16; 4], [u16; 4], u32) {
        let u16_at = |o: usize| u16::from_ne_bytes([m[o], m[o + 1]]);
        let u32_at = |o: usize| u32::from_ne_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]]);
        (
            u32_at(0),
            [u16_at(4), u16_at(6), u16_at(8), u16_at(10)],
            [u16_at(14), u16_at(16), u16_at(18), u16_at(20)],
            u32_at(24),
        )
    }

    /// Every resolution the GOP is likely to hand us, plus the one that broke.
    const MODES: &[(u32, u32)] = &[
        (640, 480),
        (800, 600),
        (1024, 768),
        (1280, 720),
        (1280, 1024),
        (1366, 768),
        (1440, 900),
        (1600, 900),
        (1680, 1050),
        (1920, 1080),
        (1920, 1200),
        (2560, 1440),
        (3840, 2160),
    ];

    #[test]
    fn every_mode_has_strictly_increasing_porches() {
        // `hdisplay < hsync_start < hsync_end < htotal`, and the vertical
        // analogue. Anything else is MODE_H_ILLEGAL / MODE_V_ILLEGAL and the
        // mode is rejected or mis-timed by whoever reads it.
        for &(w, h) in MODES {
            let m = make_modeinfo(w, h);
            let (_, hor, vert, _) = timings(&m);
            assert!(
                hor[0] < hor[1] && hor[1] < hor[2] && hor[2] < hor[3],
                "{}x{} horizontal timings not strictly increasing: {:?}",
                w,
                h,
                hor
            );
            assert!(
                vert[0] < vert[1] && vert[1] < vert[2] && vert[2] < vert[3],
                "{}x{} vertical timings not strictly increasing: {:?}",
                w,
                h,
                vert
            );
        }
    }

    #[test]
    fn every_mode_reports_the_resolution_it_was_asked_for() {
        for &(w, h) in MODES {
            let m = make_modeinfo(w, h);
            let (_, hor, vert, _) = timings(&m);
            assert_eq!(hor[0] as u32, w, "hdisplay for {}x{}", w, h);
            assert_eq!(vert[0] as u32, h, "vdisplay for {}x{}", w, h);
        }
    }

    #[test]
    fn the_refresh_recomputed_from_the_porches_is_sixty_hertz() {
        // This is the check the compositor makes. wlroots and Mesa do not
        // trust `vrefresh`; they recompute it in millihertz from the clock and
        // the totals, and that is what showed 55-59 Hz when the porches were
        // wrong.
        for &(w, h) in MODES {
            let m = make_modeinfo(w, h);
            let (clock, hor, vert, vrefresh) = timings(&m);
            let htotal = hor[3] as u64;
            let vtotal = vert[3] as u64;
            let mhz = (clock as u64 * 1_000_000 / htotal + vtotal / 2) / vtotal;
            // A tolerance, not an equality: the pixel clock is a whole number
            // of kHz and is rounded UP, so the recomputed refresh lands on
            // 60_000 or a hair above it (800x600 gives 60_001).
            //
            // Worth knowing what this test does NOT catch: the clock is
            // derived from `htotal` and `vtotal`, so the arithmetic is
            // self-consistent and *any* porch values recompute to 60 Hz.
            // Wrong porches are caught by
            // `every_mode_has_strictly_increasing_porches`, not here. This one
            // guards the clock, the rounding and the advertised `vrefresh`.
            assert!(
                (60_000..=60_010).contains(&mhz),
                "{}x{} recomputes to {} mHz (clock {} kHz, htotal {}, vtotal {})",
                w,
                h,
                mhz,
                clock,
                htotal,
                vtotal
            );
            assert_eq!(vrefresh, 60, "the advertised vrefresh must agree");
        }
    }

    #[test]
    fn the_mode_name_is_the_resolution_and_is_nul_terminated() {
        let m = make_modeinfo(1920, 1080);
        let name = &m[36..68];
        let end = name
            .iter()
            .position(|&b| b == 0)
            .expect("name must be terminated");
        assert_eq!(&name[..end], b"1920x1080");
    }

    #[test]
    fn a_zero_refresh_target_does_not_underflow() {
        // `refresh_mhz * vtotal - vtotal / 2` goes negative when the target is
        // zero, which is a panic in debug. Only one caller passes 60_000
        // today, but the parameter is there to be passed.
        assert_eq!(
            clock_khz_for_refresh_mhz(2080, 1121, 0),
            1,
            "a zero target gives the slowest clock that is still a clock"
        );
        assert_eq!(clock_khz_for_refresh_mhz(0, 1121, 60_000), 0);
        assert_eq!(clock_khz_for_refresh_mhz(2080, 0, 60_000), 0);
    }

    #[test]
    fn the_clock_is_never_zero_for_a_real_mode() {
        // A mode with clock 0 is rejected outright by every compositor.
        for &(w, h) in MODES {
            let (clock, _, _, _) = timings(&make_modeinfo(w, h));
            assert!(clock > 0, "{}x{} got a zero pixel clock", w, h);
        }
    }

    /// A `DetailedTiming` built straight, so these tests never touch the
    /// process-wide boot EDID (which no test sets and every one of them reads).
    #[allow(clippy::too_many_arguments)]
    fn panel(
        clock_khz: u32,
        (hd, hss, hse, ht): (u32, u32, u32, u32),
        (vd, vss, vse, vt): (u32, u32, u32, u32),
        interlaced: bool,
    ) -> edid::DetailedTiming {
        edid::DetailedTiming {
            clock_khz,
            hdisplay: hd,
            hsync_start: hss,
            hsync_end: hse,
            htotal: ht,
            vdisplay: vd,
            vsync_start: vss,
            vsync_end: vse,
            vtotal: vt,
            interlaced,
            separate_sync: true,
            hsync_positive: false,
            vsync_positive: true,
        }
    }

    /// `1920x1080` at the given refresh, with the DMT geometry. The clock is
    /// chosen so the refresh is exact, which is what makes the assertions
    /// numbers instead of ranges.
    fn dmt_1080p(hz: u32) -> edid::DetailedTiming {
        panel(
            hz * 2200 * 1125 / 1000,
            (1920, 2008, 2052, 2200),
            (1080, 1084, 1089, 1125),
            false,
        )
    }

    #[test]
    fn with_no_edid_the_mode_is_the_nominal_one_byte_for_byte() {
        // The regression guard for everything below: a machine whose firmware
        // read no EDID -- every VM, and the case the whole suite runs in --
        // must get exactly the mode it got before the panel timing existed.
        for &(w, h) in MODES {
            let m = make_modeinfo_with(w, h, None);
            let (clock, hor, vert, vrefresh) = timings(&m);
            let hd = w as u16;
            let vd = h as u16;
            assert_eq!(hor, [hd, hd + 48, hd + 80, hd + 160], "{}x{}", w, h);
            assert_eq!(vert, [vd, vd + 3, vd + 9, vd + 41], "{}x{}", w, h);
            assert_eq!(vrefresh, 60, "{}x{}", w, h);
            assert_eq!(
                clock,
                clock_khz_for_refresh_mhz(hor[3] as u32, vert[3] as u32, 60_000),
                "{}x{}",
                w,
                h
            );
            // -hsync/-vsync, and no interlace.
            assert_eq!(u32::from_ne_bytes([m[28], m[29], m[30], m[31]]), 0x0A);
            // And that is what `make_modeinfo` itself builds, since no test
            // sets a boot EDID.
            assert_eq!(make_modeinfo(w, h), m, "{}x{}", w, h);
        }
    }

    #[test]
    fn a_panel_faster_than_sixty_is_advertised_at_its_own_refresh() {
        // The reason this path exists. The kernel used to answer 60 Hz for
        // every monitor, because 60 was the only refresh it could name: the
        // EDID's pixel clock was decoded nowhere. A compositor told 60 paces
        // its repaints and its WAIT_VBLANK sleeps to 16.7 ms, so on this panel
        // better than half the scanouts show a frame that is already up.
        let m = make_modeinfo_with(1920, 1080, Some(&dmt_1080p(144)));
        let (clock, hor, vert, vrefresh) = timings(&m);
        assert_eq!(vrefresh, 144);
        assert_eq!(clock, 356_400, "the panel's own pixel clock, in kHz");
        assert_eq!(hor, [1920, 2008, 2052, 2200], "the panel's own porches");
        assert_eq!(vert, [1080, 1084, 1089, 1125]);
        // The number that matters is not the one in the `vrefresh` field but
        // the one the pacing is derived from, and both have to agree: a client
        // that leaves `vrefresh` at 0 gets the refresh recomputed from these
        // porches, and that is the path `set_vblank_period_from_modeinfo` runs.
        assert_eq!(drm::refresh_hz_from_modeinfo(&m), Some(144));
        let mut no_vrefresh = m;
        no_vrefresh[24..28].copy_from_slice(&0u32.to_ne_bytes());
        assert_eq!(drm::refresh_hz_from_modeinfo(&no_vrefresh), Some(144));
    }

    #[test]
    fn every_refresh_a_panel_can_state_survives_the_round_trip() {
        // Not just 144: the mode has to carry whatever the monitor says, and
        // the two answers (the stated field and the one recomputed from the
        // porches) have to agree for each, or a compositor gets a different
        // rate depending on which it trusts.
        for hz in [50u32, 60, 75, 100, 120, 144, 165, 240] {
            let m = make_modeinfo_with(1920, 1080, Some(&dmt_1080p(hz)));
            let (_, _, _, vrefresh) = timings(&m);
            assert_eq!(vrefresh, hz, "stated refresh for {} Hz", hz);
            assert_eq!(
                drm::refresh_hz_from_modeinfo(&m),
                Some(hz as u64),
                "recomputed refresh for {} Hz",
                hz
            );
        }
    }

    #[test]
    fn a_panel_whose_native_mode_is_not_the_one_on_screen_is_not_used() {
        // Firmware picks the mode; a 4K panel driven at 1080p is the ordinary
        // case. Its preferred timing then describes 3840x2160 at some refresh
        // that has nothing to do with what is scanning out, so using it would
        // pace the compositor to a mode nobody is displaying.
        let native_4k = panel(
            594_000,
            (3840, 4016, 4104, 4400),
            (2160, 2168, 2178, 2250),
            false,
        );
        let m = make_modeinfo_with(1920, 1080, Some(&native_4k));
        assert_eq!(m, make_modeinfo_with(1920, 1080, None), "must be nominal");
        // One axis matching is not matching.
        let same_width = panel(
            148_500,
            (1920, 2008, 2052, 2200),
            (1200, 1204, 1209, 1245),
            false,
        );
        assert_eq!(
            make_modeinfo_with(1920, 1080, Some(&same_width)),
            make_modeinfo_with(1920, 1080, None)
        );
        // And when it does match, it is used -- otherwise this test would pass
        // with the panel timing wired to nothing.
        assert_ne!(
            make_modeinfo_with(1920, 1080, Some(&dmt_1080p(144))),
            make_modeinfo_with(1920, 1080, None)
        );
    }

    #[test]
    fn an_illegal_panel_timing_falls_back_instead_of_being_advertised() {
        // The one way this change could cost Moebius his desktop. A sync pulse
        // ending past the total is MODE_H_ILLEGAL, and wlroots handed such a
        // mode drops the output rather than picking another -- the desktop
        // falls back to the text console. So a monitor whose descriptor is
        // line noise has to land on the nominal mode, not on the screen.
        let nominal = make_modeinfo_with(1920, 1080, None);
        for bad in [
            // hsync_end past htotal
            panel(
                148_500,
                (1920, 2008, 2300, 2200),
                (1080, 1084, 1089, 1125),
                false,
            ),
            // hsync_start before hdisplay
            panel(
                148_500,
                (1920, 1900, 2052, 2200),
                (1080, 1084, 1089, 1125),
                false,
            ),
            // vsync_end past vtotal
            panel(
                148_500,
                (1920, 2008, 2052, 2200),
                (1080, 1084, 1200, 1125),
                false,
            ),
            // a zero clock: no refresh to derive
            panel(0, (1920, 2008, 2052, 2200), (1080, 1084, 1089, 1125), false),
            // a total that does not fit the 16-bit uAPI field
            panel(
                148_500,
                (1920, 2008, 2052, 70_000),
                (1080, 1084, 1089, 1125),
                false,
            ),
            // a clock so slow the refresh rounds to zero
            panel(1, (1920, 2008, 2052, 2200), (1080, 1084, 1089, 1125), false),
        ] {
            let m = make_modeinfo_with(1920, 1080, Some(&bad));
            assert_eq!(m, nominal, "an unusable timing reached the mode: {:?}", bad);
        }
    }

    #[test]
    fn the_panels_sync_polarity_and_interlace_reach_the_flags() {
        const PHSYNC: u32 = 1 << 0;
        const NHSYNC: u32 = 1 << 1;
        const PVSYNC: u32 = 1 << 2;
        const NVSYNC: u32 = 1 << 3;
        const INTERLACE: u32 = 1 << 4;
        let flags_of = |m: &[u8; 68]| u32::from_ne_bytes([m[28], m[29], m[30], m[31]]);

        let mut t = dmt_1080p(60);
        assert_eq!(
            flags_of(&make_modeinfo_with(1920, 1080, Some(&t))),
            NHSYNC | PVSYNC,
            "-hsync/+vsync, what the timing states"
        );
        t.hsync_positive = true;
        t.vsync_positive = false;
        assert_eq!(
            flags_of(&make_modeinfo_with(1920, 1080, Some(&t))),
            PHSYNC | NVSYNC
        );
        // A descriptor that does not use digital separate sync states no
        // polarity at all, and inventing one is what Linux declines to do.
        t.separate_sync = false;
        assert_eq!(flags_of(&make_modeinfo_with(1920, 1080, Some(&t))), 0);

        // 1080i60: the decoder has already doubled the vertical numbers, so
        // the mode is 1920x1080 with an odd total and the interlace flag. A
        // compositor that is not told it is interlaced renders half a frame.
        let i = panel(
            74_250,
            (1920, 2008, 2052, 2200),
            (1080, 1084, 1094, 1125),
            true,
        );
        let m = make_modeinfo_with(1920, 1080, Some(&i));
        assert_eq!(flags_of(&m) & INTERLACE, INTERLACE);
        let (_, _, vert, vrefresh) = timings(&m);
        assert_eq!(vert[3] % 2, 1, "an interlaced frame has an odd line count");
        assert_eq!(vrefresh, 60, "1080i60 is 60 FIELDS a second");
    }

    #[test]
    fn a_panel_timing_is_still_a_legal_mode_by_the_rule_the_nominal_one_keeps() {
        // The porch invariant the nominal mode is held to, applied to the
        // other source. Non-strict here, because a real panel is allowed a
        // zero front porch or no back porch and Linux accepts it.
        for hz in [50u32, 60, 144, 240] {
            let m = make_modeinfo_with(1920, 1080, Some(&dmt_1080p(hz)));
            let (clock, hor, vert, _) = timings(&m);
            assert!(clock > 0);
            assert!(
                hor[0] <= hor[1] && hor[1] <= hor[2] && hor[2] <= hor[3],
                "{:?}",
                hor
            );
            assert!(
                vert[0] <= vert[1] && vert[1] <= vert[2] && vert[2] <= vert[3],
                "{:?}",
                vert
            );
        }
        // And a zero-porch reduced-blanking panel is accepted, not refused.
        let rb = panel(
            148_500,
            (1920, 1920, 2000, 2000),
            (1080, 1080, 1125, 1125),
            false,
        );
        let m = make_modeinfo_with(1920, 1080, Some(&rb));
        assert_ne!(m, make_modeinfo_with(1920, 1080, None));
        let (_, hor, _, _) = timings(&m);
        assert_eq!(hor, [1920, 1920, 2000, 2000]);
    }

    #[test]
    fn a_partly_read_edid_is_refused_rather_than_decoded() {
        // The buffer is a fixed 128 bytes whatever firmware managed to read, so
        // a short read leaves the tail as whatever was there before it. Taking
        // those bytes gives a pixel clock for a monitor that may not even be
        // plugged in, and from there a vblank period for a mode nobody has.
        let mut block = [0u8; edid::BLOCK_LEN];
        block[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        block[18] = 1;
        block[19] = 4;
        // The DMT 1080p60 descriptor, written straight into slot 0.
        let d: [u8; 18] = [
            0x02, 0x3A, 0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C, 0x45, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x1E,
        ];
        block[54..72].copy_from_slice(&d);
        let sum = block[..edid::BLOCK_LEN - 1]
            .iter()
            .fold(0u8, |a, b| a.wrapping_add(*b));
        block[edid::BLOCK_LEN - 1] = sum.wrapping_neg();

        // A whole block decodes.
        let whole = panel_timing_in(&block, 128).expect("a whole block was refused");
        assert_eq!((whole.hdisplay, whole.vdisplay), (1920, 1080));
        assert_eq!(whole.refresh_hz(), 60);
        // And the same bytes, reported as a short read, do not.
        for len in [0u32, 1, 64, 127] {
            assert_eq!(
                panel_timing_in(&block, len),
                None,
                "a {}-byte read was decoded as a whole block",
                len
            );
        }
    }

    #[test]
    fn the_mode_name_is_the_resolution_whichever_source_the_timings_came_from() {
        // Userspace matches modes by name, so the two sources must not name
        // the same mode differently.
        let a = make_modeinfo_with(1920, 1080, None);
        let b = make_modeinfo_with(1920, 1080, Some(&dmt_1080p(144)));
        assert_eq!(a[36..68], b[36..68]);
        assert_eq!(&a[36..45], b"1920x1080");
        assert_eq!(a[45], 0);
    }

    #[test]
    fn the_render_node_refuses_modesetting() {
        // The regression: reading the TYPE byte instead of the NR matched
        // everything, because 'd' is 0x64 and 0x64 sits inside the
        // driver-private arm. These are the ioctls that must NEVER reach a
        // render client.
        for (nr, what) in [
            (0xA1u32, "MODE_GETRESOURCES"),
            (0xA2, "MODE_GETCRTC"),
            (0xA3, "MODE_SETCRTC"),
            (0xA6, "MODE_GETENCODER"),
            (0xA7, "MODE_GETCONNECTOR"),
            (0xAE, "MODE_ADDFB"),
            (0xAF, "MODE_RMFB"),
            (0xB0, "MODE_PAGE_FLIP"),
            (0xB7, "MODE_ADDFB2"),
            (0xBC, "MODE_ATOMIC"),
            (0x3A, "WAIT_VBLANK"),
            (0x02, "GET_MAGIC"),
            (0x11, "AUTH_MAGIC"),
            (0x07, "SET_MASTER"),
            (0x08, "DROP_MASTER"),
        ] {
            assert!(
                !render_allowed(nr),
                "{} (NR {:#04x}) must not be allowed on a render node",
                what,
                nr
            );
        }
    }

    #[test]
    fn the_render_node_allows_what_a_render_client_needs() {
        for (nr, what) in [
            (0x00u32, "VERSION"),
            (0x09, "GEM_CLOSE"),
            (0x0C, "GET_CAP"),
            (0x2D, "PRIME_HANDLE_TO_FD"),
            (0x2E, "PRIME_FD_TO_HANDLE"),
            (0x40, "driver-private, first"),
            (0x9F, "driver-private, last"),
            (0xBF, "SYNCOBJ_CREATE"),
            (0xC5, "SYNCOBJ_SIGNAL"),
            (0xCA, "SYNCOBJ_TIMELINE_WAIT"),
            (0xCD, "SYNCOBJ_TIMELINE_SIGNAL"),
            (0xCF, "SYNCOBJ_EVENTFD"),
        ] {
            assert!(
                render_allowed(nr),
                "{} (NR {:#04x}) must be allowed on a render node",
                what,
                nr
            );
        }
    }

    #[test]
    fn the_render_filter_reads_the_nr_and_not_the_type_byte() {
        // The shape of the bug, pinned directly: a full ioctl command word
        // whose TYPE byte is 'd' (0x64, inside the driver-private arm) but
        // whose NR is a modesetting one must still be refused. If the filter
        // ever goes back to reading `(cmd >> 8) & 0xff`, this is the test that
        // catches it.
        let setcrtc = 0xC068_64A3u32; // dir=RW, size=0x68, type='d', nr=0xA3
        assert_eq!((setcrtc >> 8) & 0xff, 0x64, "the type byte really is 'd'");
        assert!(
            (0x40..=0x9F).contains(&((setcrtc >> 8) & 0xff)),
            "and it really does fall inside the driver-private arm"
        );
        assert!(
            !render_allowed(setcrtc),
            "SETCRTC must be refused however the command word is dressed up"
        );
    }

    #[test]
    fn a_render_node_refuses_modeset_and_dumb_with_eacces() {
        // Enforcement used to be observe-only: renderD128 accepted CREATE_DUMB
        // and SETCRTC. Linux answers EACCES; the helper already knew, the
        // ioctl path did not.
        use super::gl_client_sequence_tests::Client;
        use crate::error::LxError;
        let render = Client::open(128);
        let mut dumb = DrmModeCreateDumb {
            height: 16,
            width: 16,
            bpp: 32,
            flags: 0,
            handle: 0,
            pitch: 0,
            size: 0,
        };
        assert_eq!(
            render.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut dumb),
            Err(FsError::NoPermission)
        );
        assert_eq!(
            LxError::from(FsError::NoPermission),
            LxError::EACCES,
            "userspace must see EACCES on a render-node modeset/dumb"
        );
        // GET_CAP stays allowed on a render node.
        let mut cap = DrmGetCap {
            capability: 0x1, // DRM_CAP_DUMB_BUFFER
            value: 0,
        };
        assert!(render.ioctl(DRM_IOCTL_GET_CAP, &mut cap).is_ok());
    }

    #[test]
    fn the_interception_filter_checks_type_number_and_a_size_floor() {
        // `is_drm_ioctl_nr` gates what `sys_ioctl` grabs before the inode
        // dispatch. Matching on the number alone would steal another
        // subsystem's ioctl that happens to share it.
        let cmd =
            |dir: u32, size: u32, ty: u32, nr: u32| (dir << 30) | (size << 16) | (ty << 8) | nr;
        let (vb_nr, vb_min) = nr::WAIT_VBLANK;
        assert!(is_drm_ioctl_nr(cmd(3, 24, 0x64, vb_nr), vb_nr, vb_min));
        assert!(
            !is_drm_ioctl_nr(cmd(3, 24, 0x65, vb_nr), vb_nr, vb_min),
            "another subsystem's type byte must not be intercepted"
        );
        assert!(
            !is_drm_ioctl_nr(cmd(3, 24, 0x64, vb_nr + 1), vb_nr, vb_min),
            "a different NR must not match"
        );
        assert!(
            !is_drm_ioctl_nr(cmd(3, 23, 0x64, vb_nr), vb_nr, vb_min),
            "a struct shorter than the one the helper parses must not match"
        );
        assert!(
            is_drm_ioctl_nr(cmd(3, 40, 0x64, vb_nr), vb_nr, vb_min),
            "a GROWN struct must still match: the floor is a minimum, not an equality"
        );
    }
}

/// Linux dispatches a DRM ioctl on its NUMBER and reconciles the struct size
/// afterwards (`drm_ioctl`/`drm_ioctl_kernel`). Matching the full 32-bit
/// command instead turns every struct that ever grew a trailing field into an
/// unknown ioctl, which is how this tree lost `SYNCOBJ_HANDLE_TO_FD` (24 B),
/// the deadline sizes of `SYNCOBJ_WAIT`/`TIMELINE_WAIT`, and `PRIME_*` --
/// each found only when a client broke on real hardware.
#[cfg(test)]
mod ioctl_size_reconciliation_tests {
    use super::*;

    /// Every canonical command must be reachable from its own NR. A typo in
    /// the table (two NRs mapping to one command, or a command filed under the
    /// wrong number) silently reroutes a client's ioctl to another handler,
    /// which is worse than not handling it at all.
    #[test]
    fn every_canonical_command_round_trips_through_its_nr() {
        const ALL: &[u32] = &[
            DRM_IOCTL_VERSION,
            DRM_IOCTL_GET_UNIQUE,
            DRM_IOCTL_GET_MAGIC,
            DRM_IOCTL_SET_VERSION,
            DRM_IOCTL_GEM_CLOSE,
            DRM_IOCTL_GET_CLIENT,
            DRM_IOCTL_GET_CAP,
            DRM_IOCTL_SET_CLIENT_CAP,
            DRM_IOCTL_AUTH_MAGIC,
            DRM_IOCTL_SET_MASTER,
            DRM_IOCTL_DROP_MASTER,
            DRM_IOCTL_WAIT_VBLANK,
            DRM_IOCTL_MODE_GETRESOURCES,
            DRM_IOCTL_MODE_GETCRTC,
            DRM_IOCTL_MODE_SETCRTC,
            DRM_IOCTL_MODE_CURSOR,
            DRM_IOCTL_MODE_GETGAMMA,
            DRM_IOCTL_MODE_SETGAMMA,
            DRM_IOCTL_MODE_GETENCODER,
            DRM_IOCTL_MODE_GETCONNECTOR,
            DRM_IOCTL_MODE_GETPROPERTY,
            DRM_IOCTL_MODE_SETPROPERTY,
            DRM_IOCTL_MODE_GETPROPBLOB,
            DRM_IOCTL_MODE_GETFB,
            DRM_IOCTL_MODE_ADDFB,
            DRM_IOCTL_MODE_RMFB,
            DRM_IOCTL_MODE_PAGE_FLIP,
            DRM_IOCTL_MODE_DIRTYFB,
            DRM_IOCTL_MODE_CREATE_DUMB,
            DRM_IOCTL_MODE_MAP_DUMB,
            DRM_IOCTL_MODE_DESTROY_DUMB,
            DRM_IOCTL_MODE_GETPLANERESOURCES,
            DRM_IOCTL_MODE_GETPLANE,
            DRM_IOCTL_MODE_SETPLANE,
            DRM_IOCTL_MODE_ADDFB2,
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
            DRM_IOCTL_MODE_OBJ_SETPROPERTY,
            DRM_IOCTL_MODE_CURSOR2,
            DRM_IOCTL_MODE_ATOMIC,
            DRM_IOCTL_MODE_CREATEPROPBLOB,
            DRM_IOCTL_MODE_DESTROYPROPBLOB,
            DRM_IOCTL_SYNCOBJ_CREATE,
            DRM_IOCTL_SYNCOBJ_DESTROY,
            DRM_IOCTL_SYNCOBJ_WAIT,
            DRM_IOCTL_SYNCOBJ_RESET,
            DRM_IOCTL_SYNCOBJ_SIGNAL,
            DRM_IOCTL_MODE_LIST_LESSEES,
            DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
            DRM_IOCTL_SYNCOBJ_QUERY,
            DRM_IOCTL_SYNCOBJ_TRANSFER,
            DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
            DRM_IOCTL_MODE_GETFB2,
            DRM_IOCTL_MODE_CLOSEFB,
        ];
        let mut seen = alloc::vec::Vec::new();
        for &cmd in ALL {
            let nr = cmd & 0xff;
            assert_eq!(
                canonical_drm_ioctl(nr),
                Some(cmd),
                "nr {:#04x} does not map back to {:#010x}",
                nr,
                cmd
            );
            assert!(
                !seen.contains(&nr),
                "nr {:#04x} is claimed by two commands",
                nr
            );
            seen.push(nr);
            // Every DRM ioctl's type byte is 'd'.
            assert_eq!(
                (cmd >> 8) & 0xff,
                0x64,
                "{:#010x} is not a DRM command",
                cmd
            );
        }
    }

    /// The regression that motivated the whole layer: the 2023 fence-deadline
    /// feature appended a `__u64 deadline_nsec` to both wait structs, so a
    /// current libdrm encodes 40/48 bytes where this tree parses 32/40. Linux
    /// dispatches both to the same handler; we must too, without the pair of
    /// hand-written `*_DEADLINE` constants that used to be the only reason the
    /// larger encoding worked.
    #[test]
    fn a_grown_struct_reaches_the_same_handler() {
        for (canon, grown) in [
            (DRM_IOCTL_SYNCOBJ_WAIT, DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE),
            (
                DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
                DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE,
            ),
        ] {
            assert_ne!(canon, grown, "the two encodings must actually differ");
            assert_eq!(canonical_drm_ioctl(grown & 0xff), Some(canon));
            // And the kernel-side buffer is the LARGER of the two, so the
            // trailing bytes the client sent survive the round trip.
            let sizes = reconcile_sizes(grown, canon);
            assert_eq!(sizes.ksize, ioc_size(grown));
            assert_eq!(sizes.in_size, ioc_size(grown));
            assert_eq!(sizes.out_size, ioc_size(grown));
        }
    }

    /// A client older than us encodes FEWER bytes. Linux copies only those in,
    /// zeroes the rest of its struct and copies only those back -- it never
    /// writes past the end of the caller's buffer.
    #[test]
    fn a_short_struct_is_zero_padded_and_never_overwritten() {
        let canon = DRM_IOCTL_MODE_GETFB2; // 104 bytes
        let short = (canon & !(0x3fff << 16)) | (64u32 << 16);
        let sizes = reconcile_sizes(short, canon);
        assert_eq!(sizes.in_size, 64);
        assert_eq!(sizes.out_size, 64, "only the caller's 64 bytes go back");
        assert_eq!(sizes.ksize, 104, "but we parse our own full struct");
    }

    /// Direction is intersected with the handler's, so a caller cannot encode
    /// `_IOC_READ` on a write-only ioctl to have a struct copied back.
    #[test]
    fn direction_is_intersected_with_the_handler() {
        // GEM_CLOSE is _IOW: write-only.
        let canon = DRM_IOCTL_GEM_CLOSE;
        assert_eq!(canon & IOC_READ_DIR, 0);
        let forged = canon | IOC_READ_DIR;
        let sizes = reconcile_sizes(forged, canon);
        assert_eq!(sizes.out_size, 0, "nothing may be copied back");
        assert_eq!(sizes.in_size, ioc_size(canon));
    }

    /// Driver-private numbers keep passing through untouched: Linux consults
    /// the driver's own ioctl table for `DRM_COMMAND_BASE..DRM_COMMAND_END`,
    /// and nouveau's uAPI lives there.
    #[test]
    fn driver_private_numbers_are_left_alone() {
        for nr in DRM_COMMAND_BASE..DRM_COMMAND_END {
            assert_eq!(canonical_drm_ioctl(nr), None, "nr {:#04x}", nr);
            assert!(!is_core_drm_nr(nr));
        }
        assert!(is_core_drm_nr(0x00));
        assert!(is_core_drm_nr(0x3A));
        assert!(is_core_drm_nr(0xA0));
        assert!(is_core_drm_nr(0xCF));
    }

    /// The size-mismatched path must actually WORK, not merely be reachable.
    /// It hands the dispatcher a KERNEL bounce buffer, so the argument's
    /// `access_ok()` has to live out here, over the range the client gave --
    /// leaving it inside the dispatcher made every reconciled ioctl EFAULT,
    /// i.e. exactly the calls this layer exists to rescue.
    #[test]
    fn the_reconciled_path_copies_in_zero_fills_and_copies_back() {
        // A client whose `drm_mode_fb_cmd2` is 40 bytes shorter than ours.
        let canon = DRM_IOCTL_MODE_GETFB2;
        let short_size = ioc_size(canon) - 40;
        let short = (canon & !(0x3fff << 16)) | ((short_size as u32) << 16);
        let mut user = alloc::vec![0xAAu8; short_size];
        user[0] = 7; // fb_id
        let user_addr = user.as_ptr() as usize;
        let ret = drm_ioctl_reconciled(short, user_addr, |cmd, kdata| {
            // Dispatched on the canonical command, never the client's.
            assert_eq!(cmd, canon);
            assert_ne!(kdata, user_addr, "the arms must see the bounce buffer");
            // SAFETY: the wrapper owns `ksize` bytes at `kdata`.
            let buf = unsafe { core::slice::from_raw_parts_mut(kdata as *mut u8, ioc_size(canon)) };
            assert_eq!(buf[0], 7, "the client's bytes arrived");
            assert!(
                buf[short_size..].iter().all(|&b| b == 0),
                "the fields the client did not send must read as zero"
            );
            buf[1] = 0x5A; // the handler's reply
            Ok(0)
        });
        assert_eq!(ret, Ok(0));
        assert_eq!(user[1], 0x5A, "the reply reached the client");
        assert_eq!(
            user.len(),
            short_size,
            "and nothing was written past its buffer"
        );
    }

    /// `sys_ioctl`'s pre-dispatch helpers parse the request struct themselves,
    /// so they match on the NR but still need a size floor: a command encoding
    /// fewer bytes than they read must fall through to the padded path instead
    /// of over-reading the caller's buffer.
    #[test]
    fn nr_matching_keeps_a_size_floor() {
        let (n, min) = nr::PRIME_HANDLE_TO_FD;
        assert!(is_drm_ioctl_nr(0xC00C_642D, n, min), "the frozen encoding");
        assert!(is_drm_ioctl_nr(0xC018_642D, n, min), "a grown one");
        assert!(!is_drm_ioctl_nr(0xC008_642D, n, min), "a short one");
        assert!(!is_drm_ioctl_nr(0xC00C_652D, n, min), "not a DRM type byte");
        assert!(!is_drm_ioctl_nr(0xC00C_642E, n, min), "a different NR");
    }

    /// The two PRIME numbers as `drm.h` files them: 0x2d exports, 0x2e
    /// imports. They spent a long time the other way round here, and the
    /// export/import arm compensated by reading the operation off the
    /// struct (`fd < 0`); with the numbers right, the number decides.
    #[test]
    fn prime_handle_to_fd_is_0x2d_and_fd_to_handle_is_0x2e() {
        assert_eq!(
            nr::PRIME_HANDLE_TO_FD,
            (0x2D, 12),
            "DRM_IOWR(0x2d, drm_prime_handle)"
        );
        assert_eq!(
            nr::PRIME_FD_TO_HANDLE,
            (0x2E, 12),
            "DRM_IOWR(0x2e, drm_prime_handle)"
        );
    }

    /// An export is an export because of the ioctl number, whatever the
    /// caller left in the OUTPUT field `fd`: libdrm presets it to -1, a
    /// caller that zeroes the struct leaves 0, and both are exporting. Read
    /// off the struct, the zeroed one became "import stdin".
    #[test]
    fn a_prime_export_is_told_by_its_number_not_by_what_the_fd_field_holds() {
        const HANDLE_TO_FD: u32 = 0xC00C_642D;
        const FD_TO_HANDLE: u32 = 0xC00C_642E;
        let libdrm = DrmPrimeHandle {
            handle: 7,
            flags: DRM_CLOEXEC | DRM_RDWR,
            fd: -1,
        };
        let zeroed = DrmPrimeHandle {
            handle: 7,
            flags: DRM_CLOEXEC,
            fd: 0,
        };
        for args in [libdrm, zeroed] {
            assert_eq!(
                prime_request(HANDLE_TO_FD, args),
                Ok(PrimeRequest::Export {
                    handle: 7,
                    flags: args.flags
                }),
                "fd={} is not read on an export",
                args.fd
            );
        }
        // A grown struct (a newer drm.h) keeps the number.
        assert_eq!(
            prime_request(0xC018_642D, zeroed),
            Ok(PrimeRequest::Export {
                handle: 7,
                flags: DRM_CLOEXEC
            })
        );
        // The import reads `fd` and nothing else: the `handle` field is its
        // output, whatever it holds.
        let import = DrmPrimeHandle {
            handle: 0xDEAD,
            flags: 0,
            fd: 5,
        };
        assert_eq!(
            prime_request(FD_TO_HANDLE, import),
            Ok(PrimeRequest::Import { fd: 5 })
        );
        // Linux's own checks on the fields each one reads.
        assert_eq!(
            prime_request(
                HANDLE_TO_FD,
                DrmPrimeHandle {
                    handle: 7,
                    flags: 0x4,
                    fd: -1
                }
            ),
            Err(LxError::EINVAL),
            "a flag other than DRM_CLOEXEC | DRM_RDWR"
        );
        assert_eq!(
            prime_request(
                FD_TO_HANDLE,
                DrmPrimeHandle {
                    handle: 0,
                    flags: 0,
                    fd: -1
                }
            ),
            Err(LxError::EBADF),
            "dma_buf_get(-1)"
        );
        assert_eq!(
            prime_request(0xC00C_642F, import),
            Err(LxError::ENOTTY),
            "not a PRIME number"
        );
    }
}

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
mod gl_client_sequence_tests {
    use super::*;
    use crate::fs::devfs::kms_emu;

    /// One open DRM file, driven the way libdrm drives it: a request number and
    /// a pointer to a struct the caller owns. Deliberately NOT a set of direct
    /// calls to `drm::*` helpers --- the entry point, the size reconciliation
    /// and the `access_ok` check are part of what a client depends on, and a
    /// test that skips them cannot see an ioctl go unreachable.
    pub(super) struct Client {
        dev: DrmDev,
    }

    impl Client {
        /// `open("/dev/dri/card0")`.
        pub(super) fn open(minor: u32) -> Client {
            Client {
                dev: DrmDev::new(minor).open_client_dev(),
            }
        }

        /// `drmIoctl(fd, request, &arg)`.
        pub(super) fn ioctl<T>(&self, request: u32, arg: &mut T) -> Result<usize> {
            drm_ioctl(&self.dev, request, arg as *mut T as usize)
        }

        /// This open's `drm_file` state.
        pub(super) fn file_state(&self) -> &Arc<drm::DrmFileState> {
            self.dev.file_state()
        }

        /// `mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, offset)`,
        /// down to the object the mapping would be made of.
        pub(super) fn mmap(&self, offset: u64, len: usize) -> Result<Arc<VmObject>> {
            self.dev.get_vmo(offset as usize, len)
        }

        /// `DRM_IOCTL_MODE_MAP_DUMB`: the mmap offset for `handle`.
        pub(super) fn map_dumb(&self, handle: u32) -> Result<u64> {
            let mut req = DrmModeMapDumb {
                handle,
                pad: 0,
                offset: 0,
            };
            self.ioctl(DRM_IOCTL_MODE_MAP_DUMB, &mut req)?;
            Ok(req.offset)
        }

        /// `read(fd, buf, len)` --- how a compositor collects flip completions.
        pub(super) fn read_events(&self, buf: &mut [u8]) -> Result<usize> {
            self.dev.read_at(0, buf)
        }

        /// `write(fd, buf, len)`, which no DRM client has a reason to do.
        pub(super) fn write(&self, buf: &[u8]) -> Result<usize> {
            self.dev.write_at(0, buf)
        }

        /// `poll(fd, POLLIN)` --- the other half of how a compositor waits.
        pub(super) fn poll(&self) -> Result<PollStatus> {
            self.dev.poll()
        }

        /// `drmModeCreateDumbBuffer`: one scanout buffer.
        pub(super) fn create_dumb(&self, width: u32, height: u32) -> DrmModeCreateDumb {
            let mut req = DrmModeCreateDumb {
                height,
                width,
                bpp: 32,
                flags: 0,
                handle: 0,
                pitch: 0,
                size: 0,
            };
            self.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut req)
                .expect("CREATE_DUMB");
            assert_ne!(req.handle, 0, "CREATE_DUMB gave no handle");
            assert_ne!(req.pitch, 0, "CREATE_DUMB gave no pitch");
            req
        }

        /// `drmModeAddFB2`: wrap a buffer in a framebuffer object.
        /// `ADDFB2` declaring a width narrower than the buffer's own pitch, so
        /// the framebuffer has off-screen padding at the end of every row --
        /// what a client with an alignment requirement, or a client whose
        /// surface is narrower than the mode, really registers.
        pub(super) fn addfb2_narrow(&self, buf: &DrmModeCreateDumb, width: u32) -> u32 {
            let mut cmd = DrmModeFbCmd2 {
                fb_id: 0,
                width,
                height: buf.height,
                pixel_format: drm::DRM_FORMAT_XRGB8888,
                flags: 0,
                handles: [buf.handle, 0, 0, 0],
                pitches: [buf.pitch, 0, 0, 0],
                offsets: [0; 4],
                modifier: [0; 4],
            };
            self.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd).expect("ADDFB2");
            assert_ne!(cmd.fb_id, 0, "ADDFB2 gave no fb id");
            cmd.fb_id
        }

        pub(super) fn addfb2(&self, buf: &DrmModeCreateDumb) -> u32 {
            // DRM_FORMAT_XRGB8888, which is what every GL swapchain on this
            // tree ends up presenting. (This used to spell the fourcc
            // "XRC4", and nothing noticed, because nothing looked.)
            let mut cmd = DrmModeFbCmd2 {
                fb_id: 0,
                width: buf.width,
                height: buf.height,
                pixel_format: drm::DRM_FORMAT_XRGB8888,
                flags: 0,
                handles: [buf.handle, 0, 0, 0],
                pitches: [buf.pitch, 0, 0, 0],
                offsets: [0; 4],
                modifier: [0; 4],
            };
            self.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd).expect("ADDFB2");
            assert_ne!(cmd.fb_id, 0, "ADDFB2 gave no fb id");
            cmd.fb_id
        }

        /// `drmModePageFlip` with `DRM_MODE_PAGE_FLIP_EVENT`.
        pub(super) fn page_flip(&self, crtc_id: u32, fb_id: u32, user_data: u64) -> Result<usize> {
            let mut flip = DrmModeCrtcPageFlip {
                crtc_id,
                fb_id,
                flags: 0x01, // DRM_MODE_PAGE_FLIP_EVENT
                reserved: 0,
                user_data,
            };
            self.ioctl(DRM_IOCTL_MODE_PAGE_FLIP, &mut flip)
        }

        /// `drmModeRmFB`.
        pub(super) fn rmfb(&self, fb_id: u32) -> Result<usize> {
            let mut id = fb_id;
            self.ioctl(DRM_IOCTL_MODE_RMFB, &mut id)
        }

        /// `drmModeDestroyDumbBuffer`.
        pub(super) fn destroy_dumb(&self, handle: u32) -> Result<usize> {
            let mut h = handle;
            self.ioctl(DRM_IOCTL_MODE_DESTROY_DUMB, &mut h)
        }
    }

    /// A `DRM_EVENT_FLIP_COMPLETE` as libdrm's `drmHandleEvent` reads it off
    /// the fd: `struct drm_event_vblank`, 32 bytes.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct FlipEvent {
        pub(super) ev_type: u32,
        pub(super) length: u32,
        pub(super) user_data: u64,
        pub(super) crtc_id: u32,
    }

    fn le32(e: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes([e[at], e[at + 1], e[at + 2], e[at + 3]])
    }

    fn le64(e: &[u8], at: usize) -> u64 {
        let (lo, hi) = (le32(e, at) as u64, le32(e, at + 4) as u64);
        if cfg!(target_endian = "little") {
            lo | (hi << 32)
        } else {
            hi | (lo << 32)
        }
    }

    pub(super) fn parse_events(buf: &[u8]) -> alloc::vec::Vec<FlipEvent> {
        buf.chunks_exact(32)
            .map(|e| FlipEvent {
                ev_type: le32(e, 0),
                length: le32(e, 4),
                user_data: le64(e, 8),
                crtc_id: le32(e, 28),
            })
            .collect()
    }

    /// `DRM_EVENT_FLIP_COMPLETE`.
    pub(super) const FLIP_COMPLETE: u32 = 2;

    /// A zeroed `struct drm_mode_card_res`, which is how libdrm starts both
    /// passes of every `drmModeGetResources`.
    pub(super) fn blank_card_res() -> DrmModeCardRes {
        DrmModeCardRes {
            fb_id_ptr: 0,
            crtc_id_ptr: 0,
            connector_id_ptr: 0,
            encoder_id_ptr: 0,
            count_fbs: 0,
            count_crtcs: 0,
            count_connectors: 0,
            count_encoders: 0,
            min_width: 0,
            max_width: 0,
            min_height: 0,
            max_height: 0,
        }
    }

    /// How many framebuffers and GEM handles the whole kernel is holding. The
    /// tables are process-wide here (not per `drm_file` as in Linux), so a leak
    /// shows up as these growing across a frame loop that should be in balance.
    fn table_sizes() -> (usize, usize) {
        drm::table_sizes_for_test()
    }

    /// One frame, in the order libdrm issues it. This is the whole reason the
    /// module exists: if any step of it regresses, no GL application can put a
    /// pixel on the screen, and every one of these steps has broken at least
    /// once.
    #[test]
    fn one_frame_allocates_wraps_flips_and_gets_its_completion() {
        // An output, so CRTC 1 exists: `PAGE_FLIP` looks the CRTC up first,
        // as Linux does, and a card with nothing to scan out reports no
        // CRTC at all. The screen holds the DRM test lock.
        let _screen = kms_emu::attach(64, 64);
        let client = Client::open(0);
        let before = table_sizes();

        let buf = client.create_dumb(64, 64);
        // The pitch is 64-byte aligned, which is what wlroots asks for and what
        // the copy-engine present path needs to match the scanout stride.
        assert_eq!(buf.pitch % 64, 0, "a dumb pitch must be 64-byte aligned");
        assert_eq!(buf.size, buf.pitch as u64 * 64);

        let fb = client.addfb2(&buf);

        // The flip itself, onto the emulated output; it owes an event.
        assert_eq!(client.page_flip(1, fb, 0xDEAD_BEEF), Ok(0));

        // The completion is scheduled for the next synthetic vblank, and
        // whether that slot is already past depends on the wall clock -- so
        // deliver it the way the timer tick would rather than letting the test
        // depend on the timing.
        drm::flush_pending_flip_completions();
        let mut events = [0u8; 64];
        let n = client.read_events(&mut events).expect("a completion event");
        assert_eq!(n, 32, "exactly one 32-byte event");
        assert_eq!(
            parse_events(&events[..n]),
            alloc::vec![FlipEvent {
                ev_type: FLIP_COMPLETE,
                length: 32,
                user_data: 0xDEAD_BEEF,
                crtc_id: 1,
            }],
            "the completion must carry back the client's own cookie",
        );

        // Teardown, in libdrm's order.
        assert_eq!(client.rmfb(fb), Ok(0));
        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
        assert_eq!(
            table_sizes(),
            before,
            "a frame that came and went left something behind",
        );
    }

    /// The `glmark2` shape: scene after scene, each one a swapchain of two
    /// buffers flipped alternately for many frames. What this catches is a leak
    /// or a latch that only shows after the tenth frame -- the single-frame test
    /// above passes happily with a flip counter that never decrements.
    #[test]
    fn a_double_buffered_loop_runs_clean_for_many_frames() {
        // Same output as the single frame above, for the same reason.
        let _screen = kms_emu::attach(64, 64);
        let client = Client::open(0);
        let before = table_sizes();

        let bufs = [client.create_dumb(64, 64), client.create_dumb(64, 64)];
        let fbs = [client.addfb2(&bufs[0]), client.addfb2(&bufs[1])];
        assert_ne!(fbs[0], fbs[1], "two ADDFB2 calls must give two fb ids");

        const FRAMES: u64 = 120;
        let mut collected = alloc::vec::Vec::new();
        for frame in 0..FRAMES {
            let fb = fbs[(frame % 2) as usize];
            assert_eq!(
                client.page_flip(1, fb, frame),
                Ok(0),
                "frame {} was refused",
                frame
            );
            // A compositor reads completions as they come; so does this.
            let mut events = [0u8; 256];
            if let Ok(n) = client.read_events(&mut events) {
                collected.extend(parse_events(&events[..n]));
            }
        }
        // The last flip's completion is scheduled for the next synthetic
        // vblank, and a host test has no timer tick to reach it -- every
        // earlier one was flushed by the flip that followed it. Deliver it the
        // way the timer would, then read what is there.
        drm::flush_pending_flip_completions();
        let mut events = [0u8; 4096];
        if let Ok(n) = client.read_events(&mut events) {
            collected.extend(parse_events(&events[..n]));
        }

        assert_eq!(
            collected.len(),
            FRAMES as usize,
            "one completion per flip, no more and no fewer",
        );
        // In order, and each carrying its own frame number: a dropped or
        // duplicated completion is what left wlroots handling a stale event.
        for (frame, ev) in collected.iter().enumerate() {
            assert_eq!(ev.ev_type, FLIP_COMPLETE);
            assert_eq!(
                ev.user_data, frame as u64,
                "completion {} carries the wrong cookie",
                frame
            );
        }

        for fb in fbs {
            assert_eq!(client.rmfb(fb), Ok(0));
        }
        for buf in &bufs {
            assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
        }
        assert_eq!(
            table_sizes(),
            before,
            "{} frames leaked framebuffer or handle table entries",
            FRAMES,
        );
    }

    /// A flip onto a framebuffer the client already removed. Linux answers
    /// `ENOENT` ("Unknown FB ID"), and the distinction matters: wlroots reads a
    /// failed flip as the output being broken and retries the whole modeset, so
    /// `EIO` here cost the entire desktop, while `ENOENT` names the real
    /// problem (the client's own fb lifetime).
    #[test]
    fn a_flip_onto_a_removed_framebuffer_is_enoent() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);
        let fb = client.addfb2(&buf);
        assert_eq!(client.rmfb(fb), Ok(0));

        assert_eq!(
            client.page_flip(1, fb, 0),
            Err(FsError::EntryNotFound),
            "a flip onto a dead fb must name the fb, not the bus",
        );
        assert_eq!(
            crate::fs::LxError::from(FsError::EntryNotFound),
            crate::fs::LxError::ENOENT,
        );
        // And it owes no event: a completion for a flip that never happened is
        // what leaves a compositor waiting on a frame it will never get.
        let mut events = [0u8; 64];
        assert!(
            matches!(client.read_events(&mut events), Err(_) | Ok(0)),
            "a refused flip must not queue a completion",
        );
        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    }

    /// The flag validation `drm_mode_page_flip_ioctl` does before anything
    /// else. `ASYNC` and the `TARGET_*` flags are `EINVAL` while the matching
    /// capabilities report 0, and so is an unknown flag or a dirty reserved
    /// word -- a client that gets one of these accepted goes on to believe in a
    /// pacing guarantee this tree does not offer.
    #[test]
    fn the_page_flip_flags_are_validated_before_the_framebuffer_is_looked_up() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);
        let fb = client.addfb2(&buf);

        for (flags, reserved, what) in [
            (0x02u32, 0u32, "ASYNC while DRM_CAP_ASYNC_PAGE_FLIP is 0"),
            (
                0x04,
                0,
                "TARGET_ABSOLUTE while DRM_CAP_PAGE_FLIP_TARGET is 0",
            ),
            (
                0x08,
                0,
                "TARGET_RELATIVE while DRM_CAP_PAGE_FLIP_TARGET is 0",
            ),
            (0x10, 0, "a flag outside DRM_MODE_PAGE_FLIP_FLAGS"),
            (0x01, 1, "a non-zero reserved word"),
        ] {
            let mut flip = DrmModeCrtcPageFlip {
                crtc_id: 1,
                fb_id: fb,
                flags,
                reserved,
                user_data: 0,
            };
            assert_eq!(
                client.ioctl(DRM_IOCTL_MODE_PAGE_FLIP, &mut flip),
                Err(FsError::InvalidParam),
                "{} must be EINVAL",
                what,
            );
        }

        // Rejected flips owe no completions.
        let mut events = [0u8; 256];
        assert!(matches!(client.read_events(&mut events), Err(_) | Ok(0)));
        assert_eq!(client.rmfb(fb), Ok(0));
        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    }

    /// `drmIsKMS` wants `count_crtcs > 0 && count_connectors > 0 &&
    /// count_encoders > 0`. With nothing to scan out -- no display registered and
    /// no driver with hardware KMS, which is exactly this host run -- all three
    /// must be zero, so a compositor does not adopt a card it cannot present on
    /// and then fail to build a backend at all. The synthetic CRTC and connector
    /// appear only once `software_kms_active()` does.
    #[test]
    fn a_node_with_nothing_to_scan_out_does_not_claim_to_be_a_kms_card() {
        let _serialised = drm::test_globals::lock();
        assert!(
            !drm::software_kms_active(),
            "this test is about the headless case; a display is registered",
        );
        let client = Client::open(0);
        let mut res = blank_card_res();
        client
            .ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut res)
            .expect("GETRESOURCES");
        assert_eq!(
            (res.count_crtcs, res.count_connectors, res.count_encoders),
            (0, 0, 0),
            "a node that cannot present must fail drmIsKMS",
        );
    }

    /// The count-then-fill protocol every libdrm getter uses: call once with
    /// null pointers to learn the counts, allocate, call again to fill. The two
    /// calls must agree, because the client sizes its heap allocation on the
    /// first answer and the kernel writes on the strength of the second.
    #[test]
    fn the_resource_counts_do_not_change_between_the_probe_and_the_fill() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        // A framebuffer, so `count_fbs` has something to count and the two
        // passes have something to disagree about.
        let buf = client.create_dumb(64, 64);
        let fb = client.addfb2(&buf);

        let mut probe = blank_card_res();
        client
            .ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut probe)
            .expect("GETRESOURCES probe");
        assert!(probe.count_fbs >= 1, "the fb just created must be counted");

        // The fill pass, with a buffer sized from the probe.
        let mut ids = alloc::vec![0u32; probe.count_fbs as usize];
        let mut fill = blank_card_res();
        fill.fb_id_ptr = ids.as_mut_ptr() as u64;
        fill.count_fbs = probe.count_fbs;
        client
            .ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut fill)
            .expect("GETRESOURCES fill");
        assert_eq!(
            (fill.count_fbs, fill.count_crtcs, fill.count_connectors),
            (probe.count_fbs, probe.count_crtcs, probe.count_connectors),
            "the counts moved between the probe and the fill",
        );
        assert!(
            ids.contains(&fb),
            "the fill pass did not report the fb the probe counted",
        );

        assert_eq!(client.rmfb(fb), Ok(0));
        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    }
}

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
mod kms_scanout_tests {
    use super::gl_client_sequence_tests::{blank_card_res, parse_events, Client, FLIP_COMPLETE};
    use super::*;
    use crate::fs::devfs::kms_emu::{self, UNTOUCHED};
    use kernel_hal::mem::phys_to_virt;

    /// The dumb buffer's pixels, reached the way its owner reaches them through
    /// its CPU mapping: `MAP_DUMB` hands out an offset into the backing VMO, and
    /// the backing is contiguous physical memory the kernel can address
    /// directly.
    fn map_dumb(buf: &DrmModeCreateDumb) -> &'static mut [u32] {
        let (pa, size) =
            drm::resolve_gem_backing(buf.handle).expect("a dumb buffer must have backing");
        assert!(size as u64 >= buf.size, "backing smaller than the buffer");
        let va = phys_to_virt(pa as usize);
        // SAFETY: `size` bytes of contiguous physical memory, identity-mapped
        // into the kernel window at `va`, owned by this buffer for as long as
        // the handle lives.
        unsafe { core::slice::from_raw_parts_mut(va as *mut u32, size / 4) }
    }

    /// Paint every pixel of `buf`, PADDING INCLUDED, with `f(x, y)` in the
    /// buffer's own stride coordinates. A swapchain buffer really does have
    /// pixels past the visible width (`CREATE_DUMB` rounds the pitch up to 64
    /// bytes, and matches the display's pitch outright for a full-screen
    /// request), and whether the present is allowed to carry them to the screen
    /// is exactly what the write-combining tests below check.
    fn paint(buf: &DrmModeCreateDumb, f: impl Fn(u32, u32) -> u32) {
        let stride = (buf.pitch / 4) as usize;
        let px = map_dumb(buf);
        for y in 0..buf.height as usize {
            for x in 0..stride {
                px[y * stride + x] = f(x as u32, y as u32);
            }
        }
    }

    /// `drmModeSetCrtc`: the modeset that puts the first frame up.
    fn set_crtc(c: &Client, crtc_id: u32, fb_id: u32, w: u32, h: u32) {
        let mut req = DrmModeGetCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id,
            fb_id,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode: make_modeinfo(w, h),
        };
        c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req).expect("SETCRTC");
    }

    /// `drmModeDirtyFB`: "these boxes changed, put them on the screen".
    fn dirtyfb(c: &Client, fb_id: u32, clips: &[DrmClipRect]) {
        let mut cmd = DrmModeFbDirtyCmd {
            fb_id,
            flags: 0,
            color: 0,
            num_clips: clips.len() as u32,
            clips_ptr: clips.as_ptr() as u64,
        };
        c.ioctl(DRM_IOCTL_MODE_DIRTYFB, &mut cmd).expect("DIRTYFB");
    }

    fn clip(x1: u16, y1: u16, x2: u16, y2: u16) -> DrmClipRect {
        DrmClipRect { x1, y1, x2, y2 }
    }

    /// What `drm_mode_dirtyfb_ioctl` refuses before any driver sees the
    /// flush: an unknown flag (EINVAL), a framebuffer that does not exist
    /// (ENOENT), a clip count and a clip pointer that disagree about whether
    /// there are clips (EINVAL), an odd count with ANNOTATE_COPY, whose clips
    /// come in pairs (EINVAL), and more than 256 clips (EINVAL). None of it
    /// was read: every one of these came back as a flush done. The shapes
    /// Xorg's modesetting shadow sends keep going through.
    #[test]
    fn dirtyfb_refuses_what_linux_refuses() {
        let _screen = kms_emu::attach(64, 16);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, |x, y| tag(0x0066_0000, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

        let clips = [clip(0, 0, 16, 8), clip(16, 8, 32, 16)];
        let ptr = clips.as_ptr() as u64;
        let dirty = |fb_id: u32, flags: u32, num_clips: u32, clips_ptr: u64| {
            let mut cmd = DrmModeFbDirtyCmd {
                fb_id,
                flags,
                color: 0,
                num_clips,
                clips_ptr,
            };
            c.ioctl(DRM_IOCTL_MODE_DIRTYFB, &mut cmd)
        };
        const ANNOTATE_COPY: u32 = 0x01;
        const ANNOTATE_FILL: u32 = 0x02;
        let einval = Err(FsError::InvalidParam);

        assert_eq!(dirty(4242, 0, 1, ptr), Err(FsError::EntryNotFound));
        assert_eq!(
            dirty(fb, 0x4, 1, ptr),
            einval,
            "a flag Linux does not define"
        );
        assert_eq!(dirty(fb, 0, 1, 0), einval, "clips without a pointer");
        assert_eq!(dirty(fb, 0, 0, ptr), einval, "a pointer without clips");
        assert_eq!(
            dirty(fb, ANNOTATE_COPY, 1, ptr),
            einval,
            "copy clips come in pairs"
        );
        assert_eq!(
            dirty(fb, 0, 257, ptr),
            einval,
            "more clips than the kernel reads"
        );

        assert_eq!(dirty(fb, 0, 256, ptr), Ok(0), "exactly the kernel's limit");
        assert_eq!(dirty(fb, ANNOTATE_COPY, 2, ptr), Ok(0));
        assert_eq!(dirty(fb, ANNOTATE_FILL, 1, ptr), Ok(0));
        assert_eq!(dirty(fb, 0, 2, ptr), Ok(0));
        assert_eq!(dirty(fb, 0, 0, 0), Ok(0), "no clips: the whole frame");

        assert_eq!(c.rmfb(fb), Ok(0));
        assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
    }

    /// `struct drm_mode_cursor`, 28 bytes -- the layout the ioctl number
    /// encodes, so a wrong one here would not even reach the arm.
    #[repr(C)]
    struct ModeCursor {
        flags: u32,
        crtc_id: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        handle: u32,
    }

    const CURSOR_BO: u32 = 0x01;
    const CURSOR_MOVE: u32 = 0x02;

    /// `drmModeSetCursor`: hand the kernel a pointer bitmap and place it.
    fn set_cursor(c: &Client, crtc_id: u32, handle: u32, w: u32, h: u32, x: i32, y: i32) {
        let mut cur = ModeCursor {
            flags: CURSOR_BO | CURSOR_MOVE,
            crtc_id,
            x,
            y,
            width: w,
            height: h,
            handle,
        };
        c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur).expect("CURSOR BO");
    }

    /// `drmModeMoveCursor`.
    fn move_cursor(c: &Client, crtc_id: u32, x: i32, y: i32) {
        let mut cur = ModeCursor {
            flags: CURSOR_MOVE,
            crtc_id,
            x,
            y,
            width: 0,
            height: 0,
            handle: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur)
            .expect("CURSOR MOVE");
    }

    /// A pixel value that is recognisable per coordinate, so a wrapped or
    /// shifted copy is visible rather than merely "different".
    fn tag(base: u32, x: u32, y: u32) -> u32 {
        base | (y << 8) | x
    }

    /// The test the module exists for: a flip really does copy the client's
    /// pixels onto the output, unchanged and in the right place.
    #[test]
    fn a_page_flip_puts_the_clients_pixels_on_the_screen() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, |x, y| tag(0x0011_0000, x, y));
        let fb = c.addfb2(&buf);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        for y in 0..16 {
            for x in 0..64 {
                assert_eq!(
                    screen.pixel(x, y),
                    tag(0x0011_0000, x, y),
                    "pixel ({}, {}) never reached the screen",
                    x,
                    y
                );
            }
        }
        // And the client still gets its completion, so its frame loop advances.
        drm::flush_pending_flip_completions();
        let mut b = [0u8; 32];
        assert_eq!(c.read_events(&mut b).expect("completion"), 32);
        let ev = parse_events(&b);
        assert_eq!(ev[0].ev_type, FLIP_COMPLETE);
        assert_eq!(ev[0].user_data, 0xF00D);

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A framebuffer larger than the mode is CLIPPED, not wrapped. The source is
    /// strided, so an implementation that walked it as a flat run would fill the
    /// screen with the framebuffer's first `width * height` pixels -- every row
    /// after the first shifted left. That is the classic "the desktop is skewed"
    /// symptom and it is invisible to any test that does not compare per pixel.
    #[test]
    fn a_framebuffer_bigger_than_the_mode_is_clipped_not_wrapped() {
        let screen = kms_emu::attach(24, 6);
        let c = Client::open(0);
        // 40 columns wide, so `CREATE_DUMB` rounds the pitch up to 48 pixels:
        // the stride and the width differ, which is what makes a flat walk of
        // the source visible at all.
        let buf = c.create_dumb(40, 12);
        assert_eq!(buf.pitch / 4, 48, "the pitch is rounded up to 64 bytes");
        paint(&buf, |x, y| tag(0x0022_0000, x, y));
        let fb = c.addfb2(&buf);

        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 24, 6);

        for y in 0..6 {
            for x in 0..24 {
                assert_eq!(
                    screen.pixel(x, y),
                    tag(0x0022_0000, x, y),
                    "pixel ({}, {}) came from the wrong source row",
                    x,
                    y
                );
            }
        }

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A framebuffer smaller than the mode leaves the rest of the screen alone.
    /// Writing past it would be an out-of-bounds store into the scanout aperture
    /// on real hardware, and the pixels it would land on belong to whatever was
    /// there before -- the text console, usually. `SETCRTC` refuses such a
    /// modeset outright (ENOSPC, `drm_crtc_check_viewport`), so the scanout
    /// is reached the way the kernel's own callers reach it.
    #[test]
    fn a_framebuffer_smaller_than_the_mode_leaves_the_rest_of_the_screen_alone() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);
        let buf = c.create_dumb(16, 4);
        paint(&buf, |x, y| tag(0x0033_0000, x, y));
        let fb = c.addfb2(&buf);

        let mut req = DrmModeGetCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id: drm::SYNTH_CRTC_ID,
            fb_id: fb,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode: make_modeinfo(64, 16),
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req),
            Err(FsError::NoDeviceSpace),
            "a mode the fb cannot hold"
        );
        drm::present_now_checked(fb, drm::SYNTH_CRTC_ID, None).expect("present");

        for y in 0..16 {
            for x in 0..64 {
                let want = if x < 16 && y < 4 {
                    tag(0x0033_0000, x, y)
                } else {
                    UNTOUCHED
                };
                assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
            }
        }

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A damage rectangle repaints its own rows and nothing else. This is what
    /// keeps a `DIRTYFB` client (Xorg's modesetting shadow, simple toolkits)
    /// from paying for a full-frame copy per damage box, and getting it wrong in
    /// the other direction -- copying the whole frame -- is what smeared stale
    /// tiles over the screen from a swapchain buffer with only the boxes drawn.
    #[test]
    fn a_damage_rectangle_repaints_only_its_own_box() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, |x, y| tag(0x0044_0000, x, y));
        let fb = c.addfb2(&buf);

        // Frame one, whole screen.
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
        // The client now changes EVERY pixel of its buffer but declares only one
        // box dirty. That asymmetry is the point: with a full-frame copy the
        // screen would show the new pixels everywhere and the test could not
        // tell the two apart. It is also the real case -- the frame a client has
        // drawn only the damage boxes into is the one whose untouched areas hold
        // a previous frame, and copying them is what put stale tiles on screen.
        // Box edges are on 16-pixel boundaries so the write-combining expansion
        // (which is unconditional here) does not widen them; that widening has
        // its own test below.
        paint(&buf, |x, y| tag(0x0055_0000, x, y));
        dirtyfb(&c, fb, &[clip(16, 4, 32, 8)]);

        for y in 0..16 {
            for x in 0..64 {
                let want = if (16..32).contains(&x) && (4..8).contains(&y) {
                    tag(0x0055_0000, x, y)
                } else {
                    tag(0x0044_0000, x, y)
                };
                assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
            }
        }

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A damage box that does not sit on a 64-byte boundary is widened to whole
    /// write-combining lines. A partial store to a write-combining aperture
    /// flushes a half-full combine buffer over the neighbouring pixels, which is
    /// the leftover-squares corruption; the present rounds the box out to
    /// 16-pixel (64-byte) lines so every store completes a line.
    #[test]
    fn a_damage_box_is_widened_to_whole_write_combining_lines() {
        let screen = kms_emu::attach(64, 4);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 4);
        paint(&buf, |_, _| 0x0000_00AA);
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 4);

        // One pixel, at x = 21: inside the line [16, 32).
        paint(&buf, |x, y| {
            if x == 21 && y == 1 {
                0x0000_00BB
            } else {
                0x0000_00AA
            }
        });
        screen.repaint(UNTOUCHED);
        dirtyfb(&c, fb, &[clip(21, 1, 22, 2)]);

        for x in 0..64 {
            let want = if (16..32).contains(&x) {
                if x == 21 {
                    0x0000_00BB
                } else {
                    0x0000_00AA
                }
            } else {
                UNTOUCHED
            };
            assert_eq!(screen.pixel(x, 1), want, "row 1, x = {}", x);
        }
        for y in [0u32, 2, 3] {
            assert!(
                (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED),
                "row {} was repainted for a box that does not touch it",
                y
            );
        }

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The expansion at the right edge lands in the scanline's OFF-SCREEN
    /// PADDING, and this is the end of the chain that could not be checked
    /// before: `expand_x_for_wc` deliberately rounds the right edge up past the
    /// visible width and caps it at the pitch, and `blit_from` has to accept
    /// that wider run. It clamped to `info.width` instead, which silently
    /// truncated the tail back off and made the whole mitigation inert on
    /// exactly the hardware that needs it -- a padded pitch is the normal case
    /// (a UEFI GOP reports 2048 pixels per scanline for a 1920-wide mode).
    ///
    /// The geometry is the real one: a full-screen `CREATE_DUMB` is given the
    /// DISPLAY's pitch, so the client's own buffer carries those padding pixels
    /// too, and a test can tell padding written from padding skipped.
    #[test]
    fn the_right_edge_expansion_reaches_the_off_screen_padding() {
        // 40 visible columns, 64 per scanline: 24 columns of padding.
        let screen = kms_emu::attach_with(40, 4, 64, true);
        let c = Client::open(0);
        let buf = c.create_dumb(40, 4);
        assert_eq!(
            buf.pitch / 4,
            64,
            "a full-screen dumb buffer takes the display's pitch"
        );
        // Visible columns and padding columns carry different values, so the
        // assertion can say WHERE a pixel came from.
        let src = |x: u32, y: u32| {
            if x < 40 {
                tag(0x0066_0000, x, y)
            } else {
                tag(0x0077_0000, x, y)
            }
        };
        paint(&buf, src);
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 40, 4);

        // A box at the right edge: [36, 40). Expanded, that is [32, 48) --
        // eight visible columns and eight of padding.
        screen.repaint(UNTOUCHED);
        dirtyfb(&c, fb, &[clip(36, 1, 40, 2)]);

        for x in 0..screen.pitch_px() {
            let want = if (32..48).contains(&x) {
                src(x, 1)
            } else {
                UNTOUCHED
            };
            assert_eq!(
                screen.pixel(x, 1),
                want,
                "row 1, x = {} (visible width 40, pitch {})",
                x,
                screen.pitch_px()
            );
        }
        // And nothing spilled into the next scanline, which is what the cap at
        // the pitch is for: past the padding is row 2's pixel 0.
        assert!(
            (0..screen.pitch_px()).all(|x| screen.pixel(x, 2) == UNTOUCHED),
            "the expansion ran past the end of the scanline"
        );

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The software pointer is composited on top of the frame, and a move
    /// restores what it was covering from the framebuffer. wlroots is held on
    /// the legacy KMS path, so it never re-renders the scene for a pointer
    /// move: if the erase half of this is wrong the cursor leaves a trail, and
    /// if the composite half is wrong there is no pointer at all.
    #[test]
    fn the_software_cursor_is_drawn_over_the_frame_and_erased_when_it_moves() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, |_, _| 0x0000_1111);
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

        // An 8x8 fully opaque pointer. The bitmap is read as `w * h`
        // consecutive pixels, so its own stride is its width.
        let cur = c.create_dumb(8, 8);
        {
            let px = map_dumb(&cur);
            for p in px.iter_mut().take(64) {
                *p = 0xFF00_00FF;
            }
        }
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

        for y in 0..16 {
            for x in 0..64 {
                let want = if (4..12).contains(&x) && (2..10).contains(&y) {
                    0xFF00_00FF
                } else {
                    0x0000_1111
                };
                assert_eq!(screen.pixel(x, y), want, "cursor at (4, 2): ({}, {})", x, y);
            }
        }

        move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 6);

        for y in 0..16 {
            for x in 0..64 {
                let want = if (40..48).contains(&x) && (6..14).contains(&y) {
                    0xFF00_00FF
                } else {
                    0x0000_1111
                };
                assert_eq!(
                    screen.pixel(x, y),
                    want,
                    "after the move to (40, 6): ({}, {})",
                    x,
                    y
                );
            }
        }

        // Leave no pointer behind for the tests that follow.
        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// `drm_mode_getcrtc` reports `mode_valid` from `crtc_state->enable`,
    /// `drm_mode_getencoder` names `encoder->crtc` and `drm_mode_getconnector`
    /// `connector->encoder`. A `SETCRTC` without a mode
    /// (`__drm_atomic_helper_set_config` with `.mode = NULL`, which also
    /// ignores the fb the request carries) and an `RMFB` of the scanout
    /// framebuffer (`atomic_remove_fb`) set the mode to NULL and detach the
    /// connectors, so all three answer 0 until the next modeset; DPMS off
    /// only clears `active`, so they stay. Here `mode_valid` was 1 whenever
    /// the panel had native timings and the encoder was always on the CRTC,
    /// so a compositor starting after another had disabled the output (a VT
    /// switch) took the console's mode for a current one; and a `SETCRTC`
    /// without a mode showed the fb it named instead of turning the pipe off.
    #[test]
    fn a_disabled_crtc_reports_no_mode_and_no_encoder_until_the_next_modeset() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let buf = c.create_dumb(32, 8);
        let fb = c.addfb2(&buf);
        let other = c.create_dumb(32, 8);
        let fb_other = c.addfb2(&other);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 8);

        // (GETCRTC mode_valid, GETCRTC fb_id, GETENCODER crtc_id, GETCONNECTOR
        // encoder_id): the pipe as the three lookups describe it.
        let pipe = || {
            let mut crtc: DrmModeGetCrtc = unsafe { core::mem::zeroed() };
            crtc.crtc_id = drm::SYNTH_CRTC_ID;
            c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
            let mut enc: DrmModeGetEncoder = unsafe { core::mem::zeroed() };
            enc.encoder_id = drm::SYNTH_ENCODER_ID;
            c.ioctl(DRM_IOCTL_MODE_GETENCODER, &mut enc)
                .expect("GETENCODER");
            let mut conn: DrmModeGetConnector = unsafe { core::mem::zeroed() };
            conn.connector_id = drm::SYNTH_CONNECTOR_ID;
            c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn)
                .expect("GETCONNECTOR");
            (crtc.mode_valid, crtc.fb_id, enc.crtc_id, conn.encoder_id)
        };
        let on = (1, fb, drm::SYNTH_CRTC_ID, drm::SYNTH_ENCODER_ID);
        let off = (0, 0, 0, 0);
        assert_eq!(pipe(), on, "with a mode set");

        // SETCRTC without a mode: off, and the fb it names is not shown.
        let mut disable: DrmModeGetCrtc = unsafe { core::mem::zeroed() };
        disable.crtc_id = drm::SYNTH_CRTC_ID;
        disable.fb_id = fb_other;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut disable), Ok(0));
        assert_eq!(pipe(), off, "after SETCRTC without a mode");
        assert!(drm::crtc_blanked(), "the pipe is off");
        assert_eq!(drm::crtc_fb(), 0, "the fb of a modeless SETCRTC was shown");

        // The next modeset brings everything back.
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 8);
        assert_eq!(pipe(), on, "after the modeset");

        // DPMS off keeps the mode and the encoder: only `active` goes.
        #[repr(C)]
        struct ConnectorSetProperty {
            value: u64,
            prop_id: u32,
            connector_id: u32,
        }
        let mut dpms = ConnectorSetProperty {
            value: 3, // Off
            prop_id: PROP_DPMS,
            connector_id: drm::SYNTH_CONNECTOR_ID,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut dpms), Ok(0));
        assert!(drm::crtc_blanked());
        assert_eq!(pipe(), on, "DPMS off is not a disable");
        dpms.value = DRM_MODE_DPMS_ON;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut dpms), Ok(0));

        // RMFB of the scanout framebuffer disables the CRTC with it.
        c.rmfb(fb).expect("RMFB");
        assert_eq!(pipe(), off, "after RMFB of the scanout framebuffer");

        c.rmfb(fb_other).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
        c.destroy_dumb(other.handle).expect("DESTROY_DUMB");
    }

    /// `drm_mode_getplane` reports `plane->state->crtc` and `plane->state->fb`:
    /// the CRTC and framebuffer the primary plane shows, 0 and 0 once the
    /// pipe is disabled (a `SETCRTC` without a mode, an `RMFB` of the
    /// scanout) and unchanged under DPMS off, and the fb follows a page
    /// flip. Here the synthetic plane answered its CRTC always and no
    /// framebuffer ever, so a client reading the plane back saw a plane on a
    /// CRTC with nothing on it whatever was on the screen.
    #[test]
    fn the_primary_plane_reports_its_crtc_and_framebuffer_only_while_it_has_them() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let buf = c.create_dumb(32, 8);
        let fb = c.addfb2(&buf);
        let next = c.create_dumb(32, 8);
        let fb_next = c.addfb2(&next);
        let plane = || {
            let mut res: DrmModeGetPlane = unsafe { core::mem::zeroed() };
            res.plane_id = drm::SYNTH_PLANE_ID;
            c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut res)
                .expect("GETPLANE");
            (res.crtc_id, res.fb_id)
        };

        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 8);
        assert_eq!(plane(), (drm::SYNTH_CRTC_ID, fb), "with the fb on the CRTC");

        let mut disable: DrmModeGetCrtc = unsafe { core::mem::zeroed() };
        disable.crtc_id = drm::SYNTH_CRTC_ID;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut disable), Ok(0));
        assert_eq!(plane(), (0, 0), "after SETCRTC without a mode");

        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 8);
        #[repr(C)]
        struct ConnectorSetProperty {
            value: u64,
            prop_id: u32,
            connector_id: u32,
        }
        let mut dpms = ConnectorSetProperty {
            value: 3, // Off
            prop_id: PROP_DPMS,
            connector_id: drm::SYNTH_CONNECTOR_ID,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut dpms), Ok(0));
        assert_eq!(
            plane(),
            (drm::SYNTH_CRTC_ID, fb),
            "DPMS off keeps the plane state"
        );
        dpms.value = DRM_MODE_DPMS_ON;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut dpms), Ok(0));

        assert_eq!(c.page_flip(drm::SYNTH_CRTC_ID, fb_next, 0), Ok(0));
        assert_eq!(
            plane(),
            (drm::SYNTH_CRTC_ID, fb_next),
            "the fb follows a flip"
        );
        drm::flush_pending_flip_completions();
        let mut sink = [0u8; 64];
        let _ = c.read_events(&mut sink);

        c.rmfb(fb_next).expect("RMFB");
        assert_eq!(plane(), (0, 0), "after RMFB of the scanout framebuffer");

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
        c.destroy_dumb(next.handle).expect("DESTROY_DUMB");
    }

    /// `drm_mode_cursor_common` reads the flags before it looks the CRTC up:
    /// no flag at all, or one it does not know, is EINVAL, ahead of the
    /// ENOENT of a CRTC that does not exist. And a handle is wrapped in a
    /// framebuffer of `width x height`, so a zero width or height is EINVAL
    /// (`drm_internal_framebuffer_create`), where only a handle of 0 hides
    /// the pointer. Here a request with no flag or an unknown one answered
    /// success having done nothing, and a zero-sized image hid the pointer
    /// with success; both refusals leave the pointer where it was.
    #[test]
    fn the_cursor_ioctl_reads_its_flags_first_and_refuses_an_empty_image() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, |_, _| 0x0000_1111);
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
        let cur = c.create_dumb(8, 8);
        {
            let px = map_dumb(&cur);
            for p in px.iter_mut().take(64) {
                *p = 0xFF00_00FF;
            }
        }
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);
        let pointer_at = |x0: i32, y0: i32, what: &str| {
            for y in 0..16 {
                for x in 0..64 {
                    let inside =
                        (x0..x0 + 8).contains(&(x as i32)) && (y0..y0 + 8).contains(&(y as i32));
                    let want = if inside { 0xFF00_00FF } else { 0x0000_1111 };
                    assert_eq!(screen.pixel(x, y), want, "{}: ({}, {})", what, x, y);
                }
            }
        };
        pointer_at(4, 2, "before");

        let cursor = |flags: u32, crtc_id: u32, handle: u32, w: u32, h: u32| {
            let mut req = ModeCursor {
                flags,
                crtc_id,
                x: 40,
                y: 6,
                width: w,
                height: h,
                handle,
            };
            c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut req)
        };
        const NO_SUCH_CRTC: u32 = 4242;
        const UNKNOWN: u32 = 0x04;
        let einval = Err(FsError::InvalidParam);
        assert_eq!(
            cursor(0, drm::SYNTH_CRTC_ID, cur.handle, 8, 8),
            einval,
            "no flag"
        );
        assert_eq!(
            cursor(UNKNOWN, drm::SYNTH_CRTC_ID, cur.handle, 8, 8),
            einval,
            "unknown flag"
        );
        assert_eq!(
            cursor(CURSOR_MOVE | UNKNOWN, drm::SYNTH_CRTC_ID, 0, 0, 0),
            einval,
            "an unknown flag next to a known one"
        );
        assert_eq!(
            cursor(0, NO_SUCH_CRTC, cur.handle, 8, 8),
            einval,
            "the flags are read before the CRTC"
        );
        assert_eq!(
            cursor(CURSOR_MOVE, NO_SUCH_CRTC, 0, 0, 0),
            Err(FsError::EntryNotFound),
            "a CRTC that does not exist"
        );
        assert_eq!(
            cursor(CURSOR_BO, drm::SYNTH_CRTC_ID, cur.handle, 0, 8),
            einval,
            "zero width"
        );
        assert_eq!(
            cursor(CURSOR_BO, drm::SYNTH_CRTC_ID, cur.handle, 8, 0),
            einval,
            "zero height"
        );
        pointer_at(4, 2, "after the refusals");

        // The operations themselves are still there: a move, a new image
        // with a move, and a hide with handle 0, whatever the size says.
        assert_eq!(cursor(CURSOR_MOVE, drm::SYNTH_CRTC_ID, 0, 0, 0), Ok(0));
        pointer_at(40, 6, "after the move");
        assert_eq!(
            cursor(
                CURSOR_BO | CURSOR_MOVE,
                drm::SYNTH_CRTC_ID,
                cur.handle,
                8,
                8
            ),
            Ok(0)
        );
        pointer_at(40, 6, "after the image and move");
        assert_eq!(
            cursor(CURSOR_BO, drm::SYNTH_CRTC_ID, 0, 0, 0),
            Ok(0),
            "hide"
        );
        pointer_at(-8, -8, "hidden");

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The topology a compositor reads before it presents anything. With an
    /// output attached this is a KMS card: `drmIsKMS` wants a CRTC, a connector
    /// and an encoder, and wlroots then wants the connector CONNECTED with at
    /// least one mode. Any one of those at zero and the output is skipped
    /// entirely -- the black screen that reports nothing.
    #[test]
    fn the_synthetic_topology_is_what_a_compositor_reads() {
        let _screen = kms_emu::attach(128, 32);
        let c = Client::open(0);

        // Pass one: counts.
        let mut probe = blank_card_res();
        c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut probe)
            .expect("GETRESOURCES");
        assert_eq!(probe.count_crtcs, 1, "drmIsKMS needs a CRTC");
        assert_eq!(probe.count_connectors, 1, "drmIsKMS needs a connector");
        assert_eq!(probe.count_encoders, 1, "drmIsKMS needs an encoder");

        // Pass two: ids, into arrays the caller sized from pass one.
        let mut crtcs = [0u32; 1];
        let mut conns = [0u32; 1];
        let mut encs = [0u32; 1];
        let mut fill = blank_card_res();
        fill.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
        fill.connector_id_ptr = conns.as_mut_ptr() as u64;
        fill.encoder_id_ptr = encs.as_mut_ptr() as u64;
        fill.count_crtcs = 1;
        fill.count_connectors = 1;
        fill.count_encoders = 1;
        c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut fill)
            .expect("GETRESOURCES fill");
        assert_eq!(crtcs[0], drm::SYNTH_CRTC_ID);
        assert_eq!(encs[0], drm::SYNTH_ENCODER_ID);

        // The connector, with the mode wlroots will pick.
        let mut mode = [0u8; 68];
        let mut conn = DrmModeGetConnector {
            encoders_ptr: 0,
            modes_ptr: mode.as_mut_ptr() as u64,
            props_ptr: 0,
            prop_values_ptr: 0,
            count_modes: 1,
            count_props: 0,
            count_encoders: 0,
            encoder_id: 0,
            connector_id: conns[0],
            connector_type: 0,
            connector_type_id: 0,
            connection: 0,
            mm_width: 0,
            mm_height: 0,
            subpixel: 0,
            pad: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn)
            .expect("GETCONNECTOR");
        assert_eq!(conn.connection, 1, "the output must report CONNECTED");
        assert_eq!(conn.count_modes, 1, "and offer a mode");
        assert_eq!(
            u16::from_ne_bytes([mode[4], mode[5]]),
            128,
            "hdisplay is the attached output's width"
        );
        assert_eq!(
            u16::from_ne_bytes([mode[14], mode[15]]),
            32,
            "vdisplay is its height"
        );
        assert!(
            conn.mm_width > 0 && conn.mm_height > 0,
            "a physical size of 0 is an infinite DPI to every client that divides by it"
        );

        // One primary plane on that CRTC -- to a client that asked for
        // universal planes, as every compositor does.
        let mut cap: [u64; 2] = [DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1];
        c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
            .expect("SET_CLIENT_CAP UNIVERSAL_PLANES");
        let mut planes = [0u32; 1];
        let mut plane_res = DrmModeGetPlaneRes {
            plane_id_ptr: planes.as_mut_ptr() as u64,
            count_planes: 1,
        };
        c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut plane_res)
            .expect("GETPLANERESOURCES");
        assert_eq!(plane_res.count_planes, 1);
        assert_eq!(planes[0], drm::SYNTH_PLANE_ID);
    }

    /// `GETCRTC` reports the framebuffer that is really on screen. A compositor
    /// reads this back to decide whether its modeset took, and the id has to be
    /// in the DRM core's namespace, not a driver-private one.
    #[test]
    fn getcrtc_reports_the_framebuffer_that_was_flipped_to() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let buf = c.create_dumb(32, 8);
        paint(&buf, |_, _| 0x0000_2222);
        let fb = c.addfb2(&buf);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");
        drm::flush_pending_flip_completions();
        let mut sink = [0u8; 32];
        let _ = c.read_events(&mut sink);

        let mut crtc = DrmModeGetCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id: drm::SYNTH_CRTC_ID,
            fb_id: 0,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: [0; 68],
        };
        c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
        assert_eq!(crtc.fb_id, fb, "the CRTC does not name the flipped fb");

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A full-frame present fills the scanline out to the last write-combining
    /// line and stops. The lines past it belong to no visible pixel, and the
    /// byte after the last one is the NEXT ROW's leftmost pixel -- running into
    /// it is how a blit smears a frame diagonally down the screen.
    #[test]
    fn a_full_frame_present_stops_at_the_end_of_the_scanline() {
        // 40 visible columns of a 64-pixel scanline, write-combining: so the
        // expansion of [0, 40) is [0, 48) and 16 columns must stay untouched.
        let screen = kms_emu::attach_with(40, 8, 64, true);
        let c = Client::open(0);
        let buf = c.create_dumb(40, 8);
        let src = |x: u32, y: u32| {
            if x < 40 {
                tag(0x0088_0000, x, y)
            } else {
                tag(0x0099_0000, x, y)
            }
        };
        paint(&buf, src);
        let fb = c.addfb2(&buf);

        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 40, 8);

        for y in 0..8 {
            for x in 0..screen.pitch_px() {
                let want = if x < 48 { src(x, y) } else { UNTOUCHED };
                assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
            }
        }

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The pointer at the right edge of a padded scanline does not wrap onto the
    /// next row. Its patch is widened to whole write-combining lines just like a
    /// present, so it can legitimately reach into the off-screen padding -- but
    /// past the padding is the next row's leftmost pixel, and a pointer whose
    /// tail appears on the far left of the line below is the visible form of
    /// that off-by-one.
    #[test]
    fn the_pointer_at_the_right_edge_does_not_wrap_onto_the_next_row() {
        let screen = kms_emu::attach_with(40, 8, 64, true);
        let c = Client::open(0);
        let buf = c.create_dumb(40, 8);
        paint(&buf, |_, _| 0x0000_3333);
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 40, 8);

        // A pointer bitmap tagged by position, so a row or column read from the
        // wrong place in it is visible rather than merely "opaque".
        let cur = c.create_dumb(8, 8);
        {
            let px = map_dumb(&cur);
            for (i, p) in px.iter_mut().take(64).enumerate() {
                *p = 0xFF00_0000 | ((i as u32 / 8) << 8) | (i as u32 % 8);
            }
        }
        // x = 36: four columns visible, four in the padding.
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 36, 1);

        for row in 0..8u32 {
            for x in 0..screen.pitch_px() {
                let p = screen.pixel(x, row);
                let in_cursor = (36..44).contains(&x) && (1..8).contains(&row);
                if in_cursor {
                    // Exactly the pointer pixel for this position, taken from
                    // the right row and column of the bitmap.
                    let want = 0xFF00_0000 | ((row - 1) << 8) | (x - 36);
                    assert_eq!(want, p, "pointer pixel at ({}, {})", x, row);
                } else {
                    assert_ne!(
                        p >> 24,
                        0xFF,
                        "a pointer pixel landed at ({}, {}) -- outside the pointer",
                        x,
                        row
                    );
                }
            }
        }

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A damage box that runs off the end of the framebuffer is clamped to it.
    /// The clip rectangle comes straight from a client, and the present reads
    /// the framebuffer at `(y * stride + x)` -- an unclamped box is an
    /// out-of-bounds read of whatever follows the buffer, painted on screen.
    #[test]
    fn a_damage_box_that_runs_off_the_framebuffer_is_clamped_to_it() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, |x, y| tag(0x00AA_0000, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

        paint(&buf, |x, y| tag(0x00BB_0000, x, y));
        screen.repaint(UNTOUCHED);
        // Bottom-right corner, running far past both edges.
        dirtyfb(&c, fb, &[clip(56, 12, 200, 200)]);

        for y in 0..16 {
            for x in 0..64 {
                // [56, 64) widened to the 16-pixel line [48, 64), rows 12..16.
                let want = if x >= 48 && y >= 12 {
                    tag(0x00BB_0000, x, y)
                } else {
                    UNTOUCHED
                };
                assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
            }
        }

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// Drain whatever flip completions are outstanding, so a later read only
    /// sees the ones the test is about.
    ///
    /// One read is not a drain: the queue hands out as much as fits and keeps the
    /// rest, and an empty queue answers EAGAIN rather than zero. A test that
    /// queued more than this buffer holds would leave completions behind for a
    /// later assertion to trip over -- a suite that fails somewhere else, which
    /// is the worst kind of noise to build in. Read until the queue says it has
    /// nothing, with a bound so a queue that always answers cannot hang the
    /// suite instead of failing it.
    fn drain_completions(c: &Client) {
        drm::flush_pending_flip_completions();
        let mut sink = [0u8; 256];
        for _ in 0..1024 {
            match c.read_events(&mut sink) {
                Ok(n) if n > 0 => continue,
                _ => return,
            }
        }
        panic!("the event queue never drained");
    }

    /// The bug the pause machinery had, from the compositor's side. During the
    /// deferred console-GPU bring-up scanout is parked: every flip is reported
    /// complete and no pixel is written, which is deliberate -- labwc's BAR1
    /// traffic must stay out of the SEC2 window. What was missing is the other
    /// half. The compositor was TOLD those frames landed, so it will not draw
    /// them again, and nothing put the last one up when the window closed: an
    /// idle desktop sat on a pre-pause frame with scanout fully alive.
    #[test]
    fn the_frame_dropped_while_scanout_was_paused_reaches_the_panel_on_resume() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let before = c.create_dumb(64, 16);
        paint(&before, |x, y| tag(0x0011_0000, x, y));
        let fb_before = c.addfb2(&before);
        c.page_flip(drm::SYNTH_CRTC_ID, fb_before, 1)
            .expect("the frame before the pause");
        assert_eq!(
            screen.pixel(7, 3),
            tag(0x0011_0000, 7, 3),
            "frame one is up"
        );
        drain_completions(&c);

        // The bring-up parks scanout. labwc knows nothing about it and renders.
        drm::set_scanout_paused_for(drm::SCANOUT_PAUSE_MAX);
        let during = c.create_dumb(64, 16);
        paint(&during, |x, y| tag(0x0022_0000, x, y));
        let fb_during = c.addfb2(&during);
        c.page_flip(drm::SYNTH_CRTC_ID, fb_during, 2)
            .expect("a flip during the pause is still accepted");

        // Nothing reached the panel: that is what the pause is for.
        assert_eq!(
            screen.pixel(7, 3),
            tag(0x0011_0000, 7, 3),
            "the pause let a frame through to the framebuffer"
        );
        // And the client was told it completed, which is why it will never
        // draw that frame again and why the kernel owes it a repaint.
        drm::flush_pending_flip_completions();
        let mut b = [0u8; 32];
        assert_eq!(c.read_events(&mut b).expect("completion"), 32);
        assert_eq!(parse_events(&b)[0].user_data, 2);

        drm::set_scanout_paused(false);

        for y in 0..16 {
            for x in 0..64 {
                assert_eq!(
                    screen.pixel(x, y),
                    tag(0x0022_0000, x, y),
                    "pixel ({}, {}) is still the pre-pause frame: the resume \
                     left the desktop frozen with scanout running",
                    x,
                    y
                );
            }
        }

        c.rmfb(fb_before).expect("RMFB");
        c.rmfb(fb_during).expect("RMFB");
        c.destroy_dumb(before.handle).expect("DESTROY_DUMB");
        c.destroy_dumb(during.handle).expect("DESTROY_DUMB");
    }

    /// And the visible half of the same disagreement. A pointer move does not
    /// re-blit the frame; it restores the two ~64x64 windows it touches FROM
    /// `crtc_fb`. With the panel a frame behind that buffer -- which is exactly
    /// what a dropped present leaves -- those windows paste pieces of a frame
    /// nobody has seen into the one still on screen: a ring of garbage that
    /// follows the cursor, invisible on a flat wallpaper and obvious over a
    /// window shadow.
    ///
    /// The watchdog is what gets there: it lifts the pause on a clock read
    /// without anyone presenting, so the first thing to run afterwards can well
    /// be a mouse move. A zero-length window is that same code path without a
    /// test that sleeps.
    #[test]
    fn a_pointer_move_after_the_watchdog_does_not_paste_pieces_of_the_unseen_frame() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let before = c.create_dumb(64, 16);
        paint(&before, |x, y| tag(0x0011_0000, x, y));
        let fb_before = c.addfb2(&before);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb_before, 64, 16);
        drain_completions(&c);

        // A pointer the kernel composites itself, placed off to one side.
        let ptr = c.create_dumb(8, 8);
        paint(&ptr, |_, _| 0xFFFF_FFFF);
        set_cursor(&c, drm::SYNTH_CRTC_ID, ptr.handle, 8, 8, 4, 4);
        drain_completions(&c);

        drm::set_scanout_paused_for(drm::SCANOUT_PAUSE_MAX);
        let during = c.create_dumb(64, 16);
        paint(&during, |x, y| tag(0x0022_0000, x, y));
        let fb_during = c.addfb2(&during);
        c.page_flip(drm::SYNTH_CRTC_ID, fb_during, 3)
            .expect("a flip during the pause is still accepted");
        drain_completions(&c);

        // The bring-up never came back, so the watchdog is what resumes -- with
        // no present of its own. `Duration::ZERO` is a window already closed.
        drm::set_scanout_paused_for(core::time::Duration::ZERO);
        assert!(
            !drm::scanout_paused(),
            "the watchdog did not lift the pause"
        );

        move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 8);

        // Every pixel the pointer does not cover belongs to ONE frame. Before
        // the fix the answer was "frame one, except two windows of frame two".
        let mut saw_second = false;
        for y in 0..16 {
            for x in 0..64 {
                let px = screen.pixel(x, y);
                if px == tag(0x0022_0000, x, y) {
                    saw_second = true;
                    continue;
                }
                assert_ne!(
                    px,
                    tag(0x0011_0000, x, y),
                    "pixel ({}, {}) is still the frame the panel was showing \
                     while the rest came from the one it never saw -- that is \
                     the garbage around the cursor",
                    x,
                    y
                );
            }
        }
        assert!(saw_second, "the pointer move put nothing on the screen");

        c.rmfb(fb_before).expect("RMFB");
        c.rmfb(fb_during).expect("RMFB");
        c.destroy_dumb(before.handle).expect("DESTROY_DUMB");
        c.destroy_dumb(during.handle).expect("DESTROY_DUMB");
        c.destroy_dumb(ptr.handle).expect("DESTROY_DUMB");
    }

    /// The other side of the damage rule: a `DIRTYFB` clip that covers the whole
    /// framebuffer IS a catch-up, so it clears the mark. Leaving it set would
    /// cost a redundant full repaint on the next pointer move.
    #[test]
    fn a_damage_rect_over_the_whole_screen_does_catch_the_panel_up() {
        let _screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let before = c.create_dumb(64, 16);
        paint(&before, |x, y| tag(0x0011_0000, x, y));
        let fb_before = c.addfb2(&before);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb_before, 64, 16);
        drain_completions(&c);

        drm::set_scanout_paused_for(drm::SCANOUT_PAUSE_MAX);
        let during = c.create_dumb(64, 16);
        paint(&during, |x, y| tag(0x0022_0000, x, y));
        let fb_during = c.addfb2(&during);
        c.page_flip(drm::SYNTH_CRTC_ID, fb_during, 5)
            .expect("a flip during the pause is still accepted");
        drain_completions(&c);
        drm::set_scanout_paused_for(core::time::Duration::ZERO);
        assert!(!drm::scanout_paused());
        assert!(drm::scanout_is_stale_for_test());

        dirtyfb(&c, fb_during, &[clip(0, 0, 64, 16)]);

        assert!(
            !drm::scanout_is_stale_for_test(),
            "a clip over the whole framebuffer put every row up, so the mark \
             must go -- keeping it costs a full repaint on the next mouse move"
        );

        c.rmfb(fb_before).expect("RMFB");
        c.rmfb(fb_during).expect("RMFB");
        c.destroy_dumb(before.handle).expect("DESTROY_DUMB");
        c.destroy_dumb(during.handle).expect("DESTROY_DUMB");
    }

    /// The popup's geometry, swept. Moebius's power menu comes up with whole
    /// runs of it missing -- the desktop showing through where the panel should
    /// be -- and the runs are in the same place on every frame, so whatever
    /// drops them is arithmetic tied to the box, not a race or a stale cache.
    ///
    /// This asks the narrowest version of that question the kernel can answer on
    /// its own: for a damage box, does the present write EVERY pixel inside it?
    /// A box is not a set of independent rows here -- the blit widens columns to
    /// write-combining lines and walks bands -- so an off-by-one in any of that
    /// arithmetic shows up as pixels inside the box still carrying the previous
    /// frame, which is exactly the symptom. Geometries chosen to be hostile:
    /// the real panel (152x135) at several offsets, odd sizes, the single pixel,
    /// a box on each edge, and the whole frame.
    #[test]
    fn every_pixel_inside_a_damage_box_is_written_whatever_its_geometry() {
        const W: u32 = 200;
        const H: u32 = 160;
        let screen = kms_emu::attach(W, H);
        let c = Client::open(0);
        let buf = c.create_dumb(W, H);
        let fb = c.addfb2(&buf);

        let boxes: [(u32, u32, u32, u32); 10] = [
            (0, 0, 152, 135),   // the panel, flush at the origin
            (7, 3, 152, 135),   // and at an odd offset, both axes
            (48, 25, 152, 135), // and where it does not fit: clipped right
            (1, 1, 7, 2),       // narrower than one WC line
            (13, 11, 31, 17),   // odd on every number
            (W - 1, H - 1, 1, 1),
            (0, H - 1, W, 1), // the last row, whole
            (W - 3, 0, 3, H), // the last columns, whole
            (0, 0, W, H),     // the whole frame through the damage path
            (9, 9, 16, 16),   // exactly one WC line wide, aligned to none
        ];

        for (i, &(bx, by, bw, bh)) in boxes.iter().enumerate() {
            let old = 0x0100_0000 * (2 * i as u32 + 1);
            let new = 0x0100_0000 * (2 * i as u32 + 2);

            // The frame that is already on the panel.
            paint(&buf, |x, y| tag(old, x, y));
            c.page_flip(drm::SYNTH_CRTC_ID, fb, 200 + i as u64)
                .expect("the frame before the damage");
            drain_completions(&c);
            assert_eq!(
                screen.pixel(0, 0),
                tag(old, 0, 0),
                "box {:?}: the first frame never got up",
                (bx, by, bw, bh)
            );

            // The client repaints the same buffer and names only its box.
            paint(&buf, |x, y| tag(new, x, y));
            dirtyfb(
                &c,
                fb,
                &[clip(
                    bx as u16,
                    by as u16,
                    (bx + bw) as u16,
                    (by + bh) as u16,
                )],
            );

            let (cw, ch) = (bw.min(W - bx), bh.min(H - by));
            for y in by..by + ch {
                for x in bx..bx + cw {
                    assert_eq!(
                        screen.pixel(x, y),
                        tag(new, x, y),
                        "box {:?}: pixel ({}, {}) inside it still carries the \
                         previous frame",
                        (bx, by, bw, bh),
                        x,
                        y
                    );
                }
            }
            // Columns are widened to write-combining lines on purpose, so only
            // the ROWS outside the box are guaranteed untouched.
            for y in (0..by).chain(by + ch..H) {
                for x in 0..W {
                    assert_eq!(
                        screen.pixel(x, y),
                        tag(old, x, y),
                        "box {:?}: row {} is outside it and was repainted",
                        (bx, by, bw, bh),
                        y
                    );
                }
            }
        }

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The same sweep on a WRITE-COMBINING output with a padded scanline, which
    /// is what real hardware is: UEFI reports a `PixelsPerScanLine` wider than
    /// the mode, the framebuffer is mapped WC, and on x86_64 `blit_from` then
    /// takes the non-temporal store loop instead of the ordinary copy. That loop
    /// is a different implementation of the same promise, so the promise has to
    /// be checked against it too -- and it is the one that runs on the machine
    /// where Moebius sees the popup come up with pieces missing.
    #[test]
    fn every_pixel_inside_a_damage_box_is_written_on_a_write_combining_output() {
        const W: u32 = 204;
        const H: u32 = 184;
        // 204 -> the pitch UEFI would report, wider than the mode.
        let screen = kms_emu::attach_with(W, H, 256, true);
        let c = Client::open(0);
        let buf = c.create_dumb(W, H);
        let fb = c.addfb2(&buf);

        // The popup's own surface, and the offsets the report names: pieces
        // missing every 64 px, i.e. every 256 bytes of a row.
        let boxes: [(u32, u32, u32, u32); 6] = [
            (0, 0, 204, 184),
            (0, 0, 180, 160),
            (12, 12, 180, 160),
            (13, 11, 63, 65),
            (64, 0, 8, H),
            (W - 1, H - 1, 1, 1),
        ];

        for (i, &(bx, by, bw, bh)) in boxes.iter().enumerate() {
            let old = 0x0100_0000 * (2 * i as u32 + 1);
            let new = 0x0100_0000 * (2 * i as u32 + 2);

            paint(&buf, |x, y| tag(old, x, y));
            c.page_flip(drm::SYNTH_CRTC_ID, fb, 300 + i as u64)
                .expect("the frame before the damage");
            drain_completions(&c);

            paint(&buf, |x, y| tag(new, x, y));
            dirtyfb(
                &c,
                fb,
                &[clip(
                    bx as u16,
                    by as u16,
                    (bx + bw) as u16,
                    (by + bh) as u16,
                )],
            );

            let (cw, ch) = (bw.min(W - bx), bh.min(H - by));
            for y in by..by + ch {
                for x in bx..bx + cw {
                    assert_eq!(
                        screen.pixel(x, y),
                        tag(new, x, y),
                        "box {:?}: pixel ({}, {}) inside it still carries the \
                         previous frame -- byte {} of its row",
                        (bx, by, bw, bh),
                        x,
                        y,
                        x * 4
                    );
                }
            }
        }

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A damage rect is not a catch-up. `DRM_IOCTL_MODE_DIRTYFB` copies the
    /// boxes the client names and nothing else, so after a dropped present the
    /// panel is still a frame behind everywhere outside them -- and the cursor
    /// repaint would go back to restoring rects from a buffer the panel does not
    /// show. Only a whole frame may clear the mark.
    ///
    /// The box has to name the framebuffer the panel already carries for this to
    /// be reachable at all: a box on any other one is promoted to a whole frame
    /// before it gets here (see
    /// `a_damage_box_on_a_fresh_buffer_puts_the_whole_frame_up`), and a whole
    /// frame is a catch-up. What is left is the client re-damaging the buffer
    /// that IS up while `crtc_fb` points at the one the pause swallowed: the
    /// panel does carry that buffer, so the box is honoured, and the panel is
    /// still not showing what the cursor repaint would read.
    #[test]
    fn a_damage_rect_does_not_catch_a_panel_up_from_a_dropped_frame() {
        let _screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let before = c.create_dumb(64, 16);
        paint(&before, |x, y| tag(0x0011_0000, x, y));
        let fb_before = c.addfb2(&before);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb_before, 64, 16);
        drain_completions(&c);

        drm::set_scanout_paused_for(drm::SCANOUT_PAUSE_MAX);
        let during = c.create_dumb(64, 16);
        paint(&during, |x, y| tag(0x0022_0000, x, y));
        let fb_during = c.addfb2(&during);
        c.page_flip(drm::SYNTH_CRTC_ID, fb_during, 4)
            .expect("a flip during the pause is still accepted");
        drain_completions(&c);
        drm::set_scanout_paused_for(core::time::Duration::ZERO);
        assert!(!drm::scanout_paused());

        // A four-pixel box, the way a blinking cursor in a terminal damages, on
        // the buffer the panel really carries.
        assert_eq!(drm::panel_fb_for_test(), fb_before);
        dirtyfb(&c, fb_before, &[clip(0, 0, 4, 4)]);

        assert!(
            drm::scanout_is_stale_for_test(),
            "a damage rect cleared the mark, so the next pointer move will \
             restore its windows from a frame the panel is not showing"
        );

        // And the other half of the same situation: the box that names the
        // framebuffer the pause swallowed cannot be honoured -- the panel does
        // not carry it -- so it becomes a whole frame, which heals the frame the
        // pause dropped instead of waiting for a pointer move to expose it.
        dirtyfb(&c, fb_during, &[clip(0, 0, 4, 4)]);
        assert!(
            !drm::scanout_is_stale_for_test(),
            "a box on a framebuffer the panel does not carry has to become a \
             whole frame, and a whole frame catches the panel up"
        );
        assert_eq!(drm::panel_fb_for_test(), fb_during);

        c.rmfb(fb_before).expect("RMFB");
        c.rmfb(fb_during).expect("RMFB");
        c.destroy_dumb(before.handle).expect("DESTROY_DUMB");
        c.destroy_dumb(during.handle).expect("DESTROY_DUMB");
    }

    /// A damage-clipped present is counted, and counted as its own kind.
    ///
    /// The present's phase-timing line used to sit behind `if rect.is_none()`, so
    /// the path a compositor with damage tracking actually drives -- every frame
    /// labwc puts up -- printed nothing at all, and the number that says whether
    /// the source flush is oversized was the one number never reported. Nothing
    /// can see a klog line from here, so the counters are what this asserts: the
    /// two kinds are tallied separately, because they happen at rates nothing
    /// alike and one divisor for both either drowns the log or hides the clipped
    /// path again.
    #[test]
    fn a_damage_clipped_present_is_counted_as_its_own_kind() {
        let _screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let buf = c.create_dumb(64, 16);
        paint(&buf, |x, y| tag(0x0066_0000, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
        drain_completions(&c);

        let (frames0, rects0) = drm::present_report_counts_for_test();

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 9).expect("flip");
        drain_completions(&c);
        let (frames1, rects1) = drm::present_report_counts_for_test();
        assert!(frames1 > frames0, "a full-frame present was not counted");
        assert_eq!(rects1, rects0, "a full frame was counted as a damage box");

        dirtyfb(&c, fb, &[clip(8, 4, 24, 8)]);
        let (frames2, rects2) = drm::present_report_counts_for_test();
        assert!(
            rects2 > rects1,
            "a damage-clipped present was not counted, so it will never be \
             reported either -- which is the defect this change is about"
        );
        assert_eq!(frames2, frames1, "a damage box was counted as a full frame");

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A damage box says "only these pixels changed in the frame already on the
    /// panel". A compositor with a swapchain presents a DIFFERENT framebuffer
    /// almost every frame, and a recycled swapchain buffer holds, outside the
    /// region it was just drawn into, whatever frame it was last used for.
    ///
    /// So honouring the box against another buffer leaves the panel carrying two
    /// frames at once. That is invisible until something reads the panel's own
    /// content back -- and `repaint_for_cursor` does exactly that, restoring its
    /// two ~64x64 windows from `crtc_fb`. A pointer move over a region the box
    /// did not touch then pastes the new buffer's older content into the frame
    /// still up: garbage in a ring around the cursor, appearing exactly when a
    /// popup opens, because that is when a fresh buffer arrives with a box around
    /// the popup and nothing else.
    ///
    /// Linux throws the clips away and declares a full update whenever
    /// `state->fb != old_state->fb` (`drm_atomic_helper_damage_iter_init`).
    /// The whole point, staged: the client keeps writing the buffer AFTER the
    /// present has already copied those rows, and the repair pass picks up what
    /// arrived. Without it the panel keeps the pixels the copy happened to catch,
    /// which is the stain Moebius sees on a freshly redrawn title bar or menu.
    ///
    /// The screen is 200 rows so the blit takes two bands of `BLIT_CHUNK_ROWS`,
    /// and the hook writes on the SECOND band -- rows the first band already
    /// carried to the panel. That ordering is the whole test: a write before the
    /// first band would simply be copied, and would prove nothing.
    #[test]
    fn the_repair_pass_picks_up_what_arrived_after_the_copy_passed() {
        let screen = kms_emu::attach(192, 200);
        drm::set_present_repair_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 200);
        paint(&buf, |x, y| tag(0x0066_0000, x, y));
        let fb = c.addfb2(&buf);
        let pixels = map_dumb(&buf);
        let stride = (buf.pitch / 4) as usize;
        let late = pixels.as_mut_ptr() as usize;

        kms_emu::on_blit_band(move |band| {
            // Second band only: by now rows 0..128 are already on the panel.
            if band != 1 {
                return;
            }
            // SAFETY: the dumb buffer outlives this present, and nothing else
            // writes it while the hook runs.
            let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
            for y in 0..128usize {
                for x in 64..128usize {
                    p[y * stride + x] = tag(0x0077_0000, x as u32, y as u32);
                }
            }
        });

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        assert!(
            kms_emu::mid_blit_calls() >= 2,
            "the blit has to take at least two bands for this to stage anything, took {}",
            kms_emu::mid_blit_calls()
        );
        assert!(
            drm::repair_rounds_for_test() >= 1,
            "a source that moved under the copy has to cost at least one repair round"
        );
        for y in 0..128 {
            for x in 64..128 {
                assert_eq!(
                    screen.pixel(x, y),
                    tag(0x0077_0000, x, y),
                    "pixel ({}, {}) was written after the copy passed and never repaired",
                    x,
                    y
                );
            }
        }
        // And the repair touched only the band that moved.
        for y in 0..128 {
            for x in (0..64).chain(128..192) {
                assert_eq!(
                    screen.pixel(x, y),
                    tag(0x0066_0000, x, y),
                    "pixel ({}, {}) outside the band that moved",
                    x,
                    y
                );
            }
        }
    }

    /// The source report has to fire on a present where NOTHING changed, because
    /// that is the case it exists for: a black rectangle that just sits there is
    /// black in both reads, so it differs in no band and the mismatch line never
    /// fires. If this line shared the mismatch line's trigger, the static case --
    /// the one Moebius is looking at -- would never be described at all.
    #[test]
    fn the_source_report_fires_even_when_nothing_changed() {
        let _screen = kms_emu::attach(192, 200);
        drm::set_present_probe_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 200);
        // Half opaque, half fully transparent black: a settled buffer that holds
        // a black region, which is exactly the shape being diagnosed.
        paint(&buf, |x, y| if x < 96 { tag(0x0044_0000, x, y) } else { 0 });
        let fb = c.addfb2(&buf);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x3333).expect("flip");

        assert_eq!(
            drm::probe_reports_for_test(),
            0,
            "nothing moved under the copy, so there is no mismatch to report"
        );
        // The range, not just the floor: a count above the budget would be
        // reads that wrote no line, and then this would pass without the line
        // this test is about ever having been written.
        let zero_reads = drm::zero_reports_for_test();
        assert!(
            (1..=drm::probe_report_budget_for_test()).contains(&zero_reads),
            "the source report has to fire anyway -- that is the whole point of \
             it -- and inside the budget, so it really wrote its line; got {}",
            zero_reads
        );
    }

    /// "Not one sampled pixel is black" is worth saying once. On Moebius's boot
    /// it got said eight times and the budget was gone 27.2 s in, before the menu
    /// whose black rectangle the flag exists to explain had been opened at all.
    #[test]
    fn a_source_with_no_black_is_described_once_and_then_stops() {
        let _screen = kms_emu::attach(64, 64);
        drm::set_present_probe_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 64);
        // Not one zero pixel anywhere: `tag` is seeded from a non-zero base, so
        // every pixel is opaque and distinct.
        paint(&buf, |x, y| tag(0x0055_0000, x, y));
        let fb = c.addfb2(&buf);

        for i in 0..4 {
            c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x4400 + i)
                .expect("flip");
        }

        assert_eq!(
            drm::zero_reports_for_test(),
            4,
            "every present still reads the source -- the budget cuts the lines, \
             not the reads"
        );
        assert_eq!(
            drm::clean_source_lines_for_test(),
            drm::clean_source_report_budget_for_test(),
            "four black-free frames say the same sentence, so only the baseline \
             line gets written"
        );
    }

    /// What Moebius's 27-sep 16:4x boot narrowed the search to. Its klog says the
    /// source was clean in BOTH reads for 32 seconds and 320-odd frames, with the
    /// "carries black" and "went black mid-copy" budgets sitting unspent -- so if
    /// a black rectangle was on screen in that window, the kernel put it there.
    ///
    /// This pins the invariant that claim rests on: a present copies every visible
    /// pixel and INVENTS nothing. Black is the interesting failure, but the
    /// assertion is exact equality, because the two ways the kernel could show
    /// black it was not given are writing a zero and **not writing at all** -- and
    /// an unwritten pixel keeps whatever the panel held, which at boot is black.
    /// `UNTOUCHED` is the emulator's sentinel for "never written", so equality
    /// catches that case by name instead of it hiding as a plausible colour.
    ///
    /// Four geometries, because the two machines differ where it matters: QEMU
    /// reports an UNPADDED pitch (7680 for 1920, exactly 4 bytes a pixel) and the
    /// RTX's UEFI reports `PixelsPerScanLine` PADDED (2048 for a 1920-wide mode),
    /// and `blit_from` picks a different right limit for each -- `padded_w` for row
    /// copies, `visible_w` per pixel. Write-combining picks the store path, and on
    /// x86_64 the WC one is the non-temporal loop for real.
    #[test]
    fn a_present_copies_every_visible_pixel_and_invents_no_black() {
        for &(w, h, pitch_px, wc) in &[
            (64u32, 32u32, 64u32, false),
            (64, 32, 64, true),
            // Padded, which is the real-hardware case and the one where the two
            // right limits stop agreeing.
            (40, 16, 64, true),
            (40, 16, 64, false),
        ] {
            let screen = kms_emu::attach_with(w, h, pitch_px, wc);
            let c = Client::open(0);
            let buf = c.create_dumb(w, h);
            // Every pixel opaque and distinct, and not one of them zero: `tag`
            // is seeded from a non-zero base, so a zero on the panel can only
            // have been invented by the copy.
            paint(&buf, |x, y| tag(0x0044_0000, x, y));
            let fb = c.addfb2(&buf);
            let src = map_dumb(&buf);
            let stride = (buf.pitch / 4) as usize;

            c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x9009).expect("flip");

            for y in 0..h {
                for x in 0..w {
                    let got = screen.pixel(x, y);
                    let want = src[y as usize * stride + x as usize];
                    assert_eq!(
                        got,
                        want,
                        "{}x{} pitch {} wc {}: panel pixel ({}, {}) is {:#010x}, source \
                         says {:#010x}{}",
                        w,
                        h,
                        pitch_px,
                        wc,
                        x,
                        y,
                        got,
                        want,
                        if got == 0 {
                            " -- the copy INVENTED black"
                        } else if got == kms_emu::UNTOUCHED {
                            " -- never written, so the panel keeps what it held"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
    }

    /// The hole Moebius's 27-sep QEMU boot opened. Its klog said, on every frame
    /// of a 65-second run, that not one sampled pixel was black -- AND said on
    /// nearly every one of those same frames that the client was still writing
    /// the buffer. Both are true at once, because the source is sampled BEFORE
    /// the copy: a clear-to-black that lands mid-copy gets blitted to the screen
    /// and the old source check never saw it. So "not handed over black" only
    /// ever meant "not black when we looked".
    #[test]
    fn black_that_arrives_while_the_kernel_is_copying_gets_its_own_line() {
        let _screen = kms_emu::attach(192, 200);
        drm::set_present_probe_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 200);
        // Opaque and distinct everywhere: the FIRST read finds no black at all,
        // which is exactly what Moebius's boot reported.
        paint(&buf, |x, y| tag(0x0055_0000, x, y));
        let fb = c.addfb2(&buf);
        let pixels = map_dumb(&buf);
        let stride = (buf.pitch / 4) as usize;
        let late = pixels.as_mut_ptr() as usize;

        // The compositor clears a region to black while the copy is in flight --
        // a popup being repainted over the desktop.
        kms_emu::on_blit_band(move |band| {
            if band != 1 {
                return;
            }
            // SAFETY: the dumb buffer outlives this present, and nothing else
            // writes it while the hook runs.
            let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
            for y in 0..64usize {
                for x in 64..128usize {
                    p[y * stride + x] = 0;
                }
            }
        });

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x7007).expect("flip");

        assert!(
            kms_emu::mid_blit_calls() >= 2,
            "the hook has to have fired mid-copy for this test to mean anything"
        );
        assert_eq!(
            drm::clean_source_lines_for_test(),
            1,
            "the first read still found no black, so the clean line is what the \
             OLD probe would have said -- and on its own it is misleading"
        );
        assert_eq!(
            drm::grew_source_lines_for_test(),
            1,
            "black that was not there before the copy and is there after was \
             handed over, just later than the sample, and that needs saying"
        );
    }

    /// And it must not double-report: a black rectangle that just sits there is
    /// black in BOTH reads, so it is the source line's business and not this
    /// one's. Without the `>` this would fire on every static black frame and
    /// bury the frames where black actually arrived mid-copy.
    #[test]
    fn black_that_was_already_there_is_not_reported_as_having_arrived() {
        let _screen = kms_emu::attach(192, 200);
        drm::set_present_probe_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 200);
        // A black rectangle sitting still, and nothing touching the buffer
        // during the copy.
        paint(&buf, |x, y| if x < 64 { 0 } else { tag(0x0066_0000, x, y) });
        let fb = c.addfb2(&buf);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x8008).expect("flip");

        assert_eq!(
            drm::black_source_lines_for_test(),
            1,
            "the source carried black, so that line fires"
        );
        assert_eq!(
            drm::grew_source_lines_for_test(),
            0,
            "it was black before the copy too, so nothing arrived mid-copy"
        );
    }

    /// And the point of the split: the cheap answer cannot eat the budget the
    /// deciding frames need. This is Moebius's boot in miniature -- a long
    /// black-free run first, and THEN the frame that carries black.
    #[test]
    fn a_frame_that_carries_black_is_still_reported_after_a_long_black_free_run() {
        let _screen = kms_emu::attach(64, 64);
        drm::set_present_probe_enabled(true);
        let c = Client::open(0);

        let clean = c.create_dumb(64, 64);
        paint(&clean, |x, y| tag(0x0055_0000, x, y));
        let clean_fb = c.addfb2(&clean);
        // More than the whole shared budget, which is what makes this test bite:
        // with one budget between the two answers these presents spend it and the
        // frame below gets no line.
        let run = drm::probe_report_budget_for_test() + 2;
        for i in 0..run {
            c.page_flip(drm::SYNTH_CRTC_ID, clean_fb, 0x5500 + u64::from(i))
                .expect("flip");
        }
        assert_eq!(
            drm::black_source_lines_for_test(),
            0,
            "no frame carried black yet, so that budget is untouched"
        );

        // Now the frame that decides: a black region the compositor handed over.
        let black = c.create_dumb(64, 64);
        paint(
            &black,
            |x, y| if x < 32 { tag(0x0066_0000, x, y) } else { 0 },
        );
        let black_fb = c.addfb2(&black);
        c.page_flip(drm::SYNTH_CRTC_ID, black_fb, 0x6666)
            .expect("flip");

        assert_eq!(
            drm::black_source_lines_for_test(),
            1,
            "the frame that carries black has to get its line even after a long \
             black-free run -- that run is exactly what spent the shared budget \
             on Moebius's boot"
        );
    }

    /// The repair is one more writer that goes around the band skip, so it has to
    /// make the skip forget -- the same rule the cursor, a damage box, a blank, a
    /// VT and the copy engine all follow.
    ///
    /// The skip's stored hash means "the panel holds these pixels in these rows".
    /// A repair writes the panel with a plain `blit_chunked`, so after it the
    /// panel holds what the REPAIR copied while the hash still describes what the
    /// first copy put there. Leave that stale and a later frame whose pixels
    /// happen to match the old hash gets skipped over a panel that does not hold
    /// them -- stale pixels left on screen by the very path that exists to stop
    /// leaving stale pixels on screen.
    ///
    /// The control for this one is
    /// `a_band_the_panel_already_holds_is_not_copied_again`: there a second
    /// present of the same pixels skips every band. Here the first present
    /// repairs, so the second must skip none.
    #[test]
    fn a_present_that_repaired_makes_the_next_one_copy_again() {
        let screen = kms_emu::attach(192, 200);
        drm::set_present_repair_enabled(true);
        drm::set_present_skip_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 200);
        paint(&buf, |x, y| tag(0x0055_0000, x, y));
        let fb = c.addfb2(&buf);
        let pixels = map_dumb(&buf);
        let stride = (buf.pitch / 4) as usize;
        let late = pixels.as_mut_ptr() as usize;

        // Present 1: the source moves under the copy, so the repair runs.
        kms_emu::on_blit_band(move |band| {
            if band != 1 {
                return;
            }
            // SAFETY: the dumb buffer outlives this present, and nothing else
            // writes it while the hook runs.
            let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
            for y in 0..128usize {
                for x in 64..128usize {
                    p[y * stride + x] = tag(0x0099_0000, x as u32, y as u32);
                }
            }
        });
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x1111)
            .expect("first flip");
        assert!(
            drm::repair_rounds_for_test() >= 1,
            "the staging did not make the source move, so this test proves nothing"
        );

        // Present 2: nothing touches the source, and it is exactly what the panel
        // was last left holding. Without the invalidation the skip would believe
        // its own stale hash and skip.
        kms_emu::clear_mid_blit();
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x2222)
            .expect("second flip");
        assert_eq!(
            drm::skipped_bands_for_test(),
            0,
            "a repair wrote the panel outside the skip, so the skip must have forgotten those rows"
        );
        // And the panel still agrees with the source everywhere, which is the
        // outcome the invalidation is protecting.
        for y in (0..200).step_by(17) {
            for x in (0..192).step_by(23) {
                let want = if y < 128 && (64..128).contains(&x) {
                    tag(0x0099_0000, x, y)
                } else {
                    tag(0x0055_0000, x, y)
                };
                assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
            }
        }
    }

    /// A repair round copies the span that moved and NOT the whole window. The
    /// claim that a round costs what actually moved rests on this, and reading the
    /// destination afterwards cannot show it when the source agrees everywhere:
    /// so the hook drops a sentinel on the panel outside the span, between the
    /// present's own blit and the repair's, and a repair that repainted the whole
    /// window would erase it.
    #[test]
    fn a_repair_round_copies_only_the_span_that_moved() {
        const SENTINEL: u32 = 0xDEAD_BEEF;
        let screen = kms_emu::attach(192, 200);
        let pitch_px = screen.pitch_px();
        drm::set_present_repair_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 200);
        paint(&buf, |x, y| tag(0x0088_0000, x, y));
        let fb = c.addfb2(&buf);
        let stride = (buf.pitch / 4) as usize;
        let late = map_dumb(&buf).as_mut_ptr() as usize;

        kms_emu::on_blit_band(move |band| match band {
            // Second band of the present's own blit: move one band of the source
            // after the rows carrying it have already gone to the panel.
            1 => {
                // SAFETY: the dumb buffer outlives this present.
                let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
                for y in 0..128usize {
                    for x in 64..128usize {
                        p[y * stride + x] = tag(0x0099_0000, x as u32, y as u32);
                    }
                }
            }
            // First band of the repair's blit: mark the panel outside the span.
            2 => {
                for y in 0..128u32 {
                    kms_emu::poke(pitch_px, 0, y, SENTINEL);
                    kms_emu::poke(pitch_px, 191, y, SENTINEL);
                }
            }
            _ => {}
        });

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        assert!(
            drm::repair_rounds_for_test() >= 1,
            "a source that moved under the copy has to cost at least one repair \
             round; the blit took {} bands",
            kms_emu::mid_blit_calls()
        );
        for y in 0..128 {
            assert_eq!(
                (screen.pixel(0, y), screen.pixel(191, y)),
                (SENTINEL, SENTINEL),
                "row {}: the repair repainted columns outside the span that moved",
                y
            );
        }
        // And the span itself still got repaired.
        assert_eq!(screen.pixel(64, 0), tag(0x0099_0000, 64, 0));
    }

    /// With the probe armed and the repair NOT armed the bracket IS taken -- the
    /// probe needs it -- so this is the one arrangement where the repair's own
    /// check is the only thing standing between a measurement and a copy nobody
    /// asked for. It must stay a measurement.
    #[test]
    fn the_probe_alone_measures_and_does_not_repair() {
        let screen = kms_emu::attach(192, 200);
        drm::set_present_probe_enabled(true);
        // Repair is ON by default; this test is specifically the probe WITHOUT
        // the repair pass, so disarm it for the duration.
        drm::set_present_repair_enabled(false);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 200);
        paint(&buf, |x, y| tag(0x00AA_0000, x, y));
        let fb = c.addfb2(&buf);
        let stride = (buf.pitch / 4) as usize;
        let late = map_dumb(&buf).as_mut_ptr() as usize;

        kms_emu::on_blit_band(move |band| {
            if band != 1 {
                return;
            }
            // SAFETY: the dumb buffer outlives this present.
            let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
            for y in 0..128usize {
                for x in 64..128usize {
                    p[y * stride + x] = tag(0x00BB_0000, x as u32, y as u32);
                }
            }
        });

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        assert_eq!(
            drm::repair_rounds_for_test(),
            0,
            "the probe must not repair -- it reports"
        );
        assert_eq!(
            screen.pixel(64, 0),
            tag(0x00AA_0000, 64, 0),
            "the panel keeps what the copy caught"
        );
    }

    /// A source that is still moving during the repair costs a SECOND round, and
    /// the second round is the one that puts the latest pixels up. Without this
    /// the budget could be one and nothing would notice.
    #[test]
    fn a_source_still_moving_during_the_repair_costs_a_second_round() {
        let screen = kms_emu::attach(192, 200);
        drm::set_present_repair_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 200);
        paint(&buf, |x, y| tag(0x00CC_0000, x, y));
        let fb = c.addfb2(&buf);
        let stride = (buf.pitch / 4) as usize;
        let late = map_dumb(&buf).as_mut_ptr() as usize;

        // Band 1 moves the span during the present's own blit; band 2 moves it
        // AGAIN during the first repair round, so only a second round can catch up.
        kms_emu::on_blit_band(move |band| {
            let base = match band {
                1 => 0x00DD_0000u32,
                2 => 0x00EE_0000u32,
                _ => return,
            };
            // SAFETY: the dumb buffer outlives this present.
            let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
            for y in 0..128usize {
                for x in 64..128usize {
                    p[y * stride + x] = tag(base, x as u32, y as u32);
                }
            }
        });

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        assert_eq!(
            drm::repair_rounds_for_test(),
            2,
            "a source that moved again under the repair owes a second round"
        );
        assert_eq!(
            screen.pixel(64, 0),
            tag(0x00EE_0000, 64, 0),
            "the second round has to put the latest pixels up"
        );
    }

    /// The same race with the repair NOT armed: the panel keeps the stale pixels.
    /// This is the defect itself, pinned, so the test above cannot pass for some
    /// reason other than the repair -- and so that turning the flag off is known
    /// to still mean what it says.
    #[test]
    fn without_the_repair_the_stale_pixels_stay_on_the_panel() {
        let screen = kms_emu::attach(192, 200);
        // The repair is ON by default since `drm.present_repair` was flipped
        // (it is `=off` that disarms it now), and this test IS the unrepaired
        // race, so it has to disarm the flag itself -- as its neighbours do.
        drm::set_present_repair_enabled(false);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 200);
        paint(&buf, |x, y| tag(0x0066_0000, x, y));
        let fb = c.addfb2(&buf);
        let pixels = map_dumb(&buf);
        let stride = (buf.pitch / 4) as usize;
        let late = pixels.as_mut_ptr() as usize;

        kms_emu::on_blit_band(move |band| {
            if band != 1 {
                return;
            }
            // SAFETY: as above.
            let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
            for y in 0..128usize {
                for x in 64..128usize {
                    p[y * stride + x] = tag(0x0077_0000, x as u32, y as u32);
                }
            }
        });

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        assert_eq!(
            drm::repair_rounds_for_test(),
            0,
            "the repair must not run once the cmdline has disarmed it"
        );
        assert_eq!(
            screen.pixel(64, 0),
            tag(0x0066_0000, 64, 0),
            "with no repair the panel keeps what the copy caught"
        );
    }

    /// The repair pass must be free on a buffer nobody is writing: zero extra
    /// rounds, and the frame on screen is exactly the frame in the buffer. Every
    /// claim about what the repair costs rests on this -- a pass that ran rounds
    /// on a settled present would be paying on every frame of a healthy desktop.
    #[test]
    fn the_repair_pass_runs_no_rounds_on_a_settled_buffer() {
        let screen = kms_emu::attach(192, 8);
        drm::set_present_repair_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 8);
        paint(&buf, |x, y| tag(0x0033_0000, x, y));
        let fb = c.addfb2(&buf);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        assert_eq!(
            drm::repair_rounds_for_test(),
            0,
            "a buffer nobody is writing must cost no repair rounds"
        );
        for y in 0..8 {
            for x in 0..192 {
                assert_eq!(
                    screen.pixel(x, y),
                    tag(0x0033_0000, x, y),
                    "pixel ({}, {}) with the repair armed",
                    x,
                    y
                );
            }
        }
    }

    // --- the unchanged-band skip (`drm.present_skip`) ---
    //
    // Every one of these turns on a sentinel: a value poked straight onto the
    // emulated panel between two presents. A band the present copies overwrites
    // it; a band the present skips leaves it there. That is the only way to see
    // the difference from outside, because the source says the same thing either
    // way -- and it is also the shape of the defect, since a skip that is wrong
    // shows up as exactly such a leftover pixel.

    /// Presenting the same buffer twice copies nothing the second time. This is
    /// the whole point: on real hardware CPU stores into the console GPU's BAR1
    /// serve at ~42 MB/s, so a frame nobody changed costs ~99 ms of pure waste.
    #[test]
    fn a_band_the_panel_already_holds_is_not_copied_again() {
        const SENTINEL: u32 = 0x0BAD_F00D;
        let screen = kms_emu::attach(64, 48);
        let pitch_px = screen.pitch_px();
        drm::set_present_skip_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 48);
        paint(&buf, |x, y| tag(0x0011_0000, x, y));
        let fb = c.addfb2(&buf);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");
        assert_eq!(
            drm::skipped_bands_for_test(),
            0,
            "the first present of a boot knows nothing and must copy everything"
        );
        // One pixel of each band, marked on the panel and not in the buffer.
        for b in 0..3u32 {
            kms_emu::poke(pitch_px, 0, b * 16, SENTINEL);
        }

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

        assert_eq!(
            drm::skipped_bands_for_test(),
            3,
            "the same buffer again: every band is already up there"
        );
        for b in 0..3u32 {
            assert_eq!(
                screen.pixel(0, b * 16),
                SENTINEL,
                "band {} was copied again although nothing changed",
                b
            );
        }
    }

    /// And without the flag the same two presents copy the frame twice, which is
    /// what fixes the behaviour to the flag rather than to the state: the
    /// sentinel goes away.
    #[test]
    fn without_the_skip_every_band_is_copied_again() {
        const SENTINEL: u32 = 0x0BAD_BEEF;
        let screen = kms_emu::attach(64, 48);
        let pitch_px = screen.pitch_px();
        let c = Client::open(0);
        let buf = c.create_dumb(64, 48);
        paint(&buf, |x, y| tag(0x0022_0000, x, y));
        let fb = c.addfb2(&buf);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");
        kms_emu::poke(pitch_px, 0, 16, SENTINEL);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

        assert_eq!(drm::skipped_bands_for_test(), 0);
        assert_eq!(
            screen.pixel(0, 16),
            tag(0x0022_0000, 0, 16),
            "with no skip armed the present must copy every band"
        );
    }

    /// A band whose pixels moved is copied; the bands around it are not. The
    /// saving and the correctness are the same claim, and this is where they meet:
    /// a desktop changes a few rows per frame and must still show them.
    #[test]
    fn only_the_band_that_changed_is_copied() {
        const SENTINEL: u32 = 0x0FEE_1DEA;
        let screen = kms_emu::attach(64, 48);
        let pitch_px = screen.pitch_px();
        drm::set_present_skip_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 48);
        paint(&buf, |x, y| tag(0x0033_0000, x, y));
        let fb = c.addfb2(&buf);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        // Mark all three bands on the panel, then change one pixel of the middle
        // one in the BUFFER.
        for b in 0..3u32 {
            kms_emu::poke(pitch_px, 0, b * 16, SENTINEL);
        }
        let stride = (buf.pitch / 4) as usize;
        map_dumb(&buf)[20 * stride + 5] = 0x00C0_FFEE;

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

        assert_eq!(
            drm::skipped_bands_for_test(),
            2,
            "one band moved, so two were already on the panel"
        );
        assert_eq!(
            screen.pixel(5, 20),
            0x00C0_FFEE,
            "the pixel that changed has to reach the panel"
        );
        assert_eq!(
            screen.pixel(0, 16),
            tag(0x0033_0000, 0, 16),
            "the band that changed is copied whole, sentinel and all"
        );
        assert_eq!(screen.pixel(0, 0), SENTINEL, "band 0 did not change");
        assert_eq!(screen.pixel(0, 32), SENTINEL, "band 2 did not change");
    }

    /// The cursor is composited ON TOP of the frame, so the rows it covers are
    /// not the frame's pixels and the next present must copy them again. Without
    /// that the pointer's previous position stays on screen until something else
    /// happens to change those rows -- the very defect this path exists to stop
    /// causing.
    #[test]
    fn the_rows_under_the_cursor_are_copied_again() {
        const SENTINEL: u32 = 0x0C0F_FEE0;
        let screen = kms_emu::attach(64, 48);
        let pitch_px = screen.pitch_px();
        drm::set_present_skip_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 48);
        paint(&buf, |x, y| tag(0x0044_0000, x, y));
        let fb = c.addfb2(&buf);
        // An opaque 8x8 pointer parked inside band 0.
        let cur = c.create_dumb(8, 8);
        paint(&cur, |_, _| 0xFF00_FF00);
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 0, 0);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        // Row 12 is in the pointer's band (rows 0..16) but BELOW the 8-row
        // pointer itself: a sentinel under the pointer would be overwritten by
        // the pointer's own pixels and say nothing about the band.
        kms_emu::poke(pitch_px, 0, 12, SENTINEL);
        kms_emu::poke(pitch_px, 0, 32, SENTINEL);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

        assert_eq!(
            screen.pixel(0, 12),
            tag(0x0044_0000, 0, 12),
            "the band under the pointer must be copied again"
        );
        assert_eq!(
            screen.pixel(0, 32),
            SENTINEL,
            "and a band nowhere near the pointer must not"
        );
    }

    /// Two bands change with a skipped band between them, and BOTH have to reach
    /// the panel. This is the one that says the dirty run is closed when a skipped
    /// band interrupts it: a run left open across the gap blits the right number
    /// of rows from the wrong place and the second changed band never arrives.
    #[test]
    fn two_bands_with_a_gap_between_them_both_arrive() {
        const SENTINEL: u32 = 0x0A11_0A11;
        let screen = kms_emu::attach(64, 48);
        let pitch_px = screen.pitch_px();
        drm::set_present_skip_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 48);
        paint(&buf, |x, y| tag(0x0077_0000, x, y));
        let fb = c.addfb2(&buf);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        kms_emu::poke(pitch_px, 0, 20, SENTINEL);
        let stride = (buf.pitch / 4) as usize;
        {
            let m = map_dumb(&buf);
            m[4 * stride + 1] = 0x0011_1111;
            m[36 * stride + 2] = 0x0022_2222;
        }

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

        assert_eq!(
            drm::skipped_bands_for_test(),
            1,
            "the band between the two that moved is the only one still up there"
        );
        assert_eq!(
            screen.pixel(1, 4),
            0x0011_1111,
            "the first band that moved has to arrive"
        );
        assert_eq!(
            screen.pixel(2, 36),
            0x0022_2222,
            "and so does the one past the gap"
        );
        assert_eq!(
            screen.pixel(0, 20),
            SENTINEL,
            "the band between them was not copied"
        );
    }

    /// A damage box does not take the skip at all. The box is already the
    /// client's own answer to "what changed", and the skip's state is indexed
    /// from its window's top row -- so a box would have it describing rows by a
    /// different origin than the cursor invalidation uses. The pixels look the
    /// same either way, which is why this asserts on which path ran.
    #[test]
    fn a_damage_box_does_not_take_the_skip() {
        let screen = kms_emu::attach(64, 48);
        drm::set_present_skip_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 48);
        paint(&buf, |x, y| tag(0x0088_0000, x, y));
        let fb = c.addfb2(&buf);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");
        let after_frame = drm::skip_presents_for_test();
        assert_eq!(after_frame, 1, "a whole frame takes the skip");

        paint(&buf, |x, y| tag(0x0099_0000, x, y));
        dirtyfb(&c, fb, &[clip(0, 16, 64, 32)]);

        assert_eq!(
            drm::skip_presents_for_test(),
            after_frame,
            "a damage box must not go through the skip"
        );
        assert_eq!(
            screen.pixel(0, 20),
            tag(0x0099_0000, 0, 20),
            "and the box is still copied"
        );
        assert_eq!(
            screen.pixel(0, 4),
            tag(0x0088_0000, 0, 4),
            "while the rows outside it keep what they had"
        );
    }

    /// A damage box copies part of the frame without the skip's knowledge, so
    /// everything it remembers stops being true. Keeping the hashes would leave
    /// the rows the box did NOT cover claiming to hold pixels that a later
    /// present then refuses to copy.
    #[test]
    fn a_damage_box_forgets_what_the_panel_held() {
        const SENTINEL: u32 = 0x0D06_0D06;
        let screen = kms_emu::attach(64, 48);
        let pitch_px = screen.pitch_px();
        drm::set_present_skip_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 48);
        paint(&buf, |x, y| tag(0x0055_0000, x, y));
        let fb = c.addfb2(&buf);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        dirtyfb(&c, fb, &[clip(0, 0, 64, 16)]);
        kms_emu::poke(pitch_px, 0, 32, SENTINEL);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

        assert_eq!(
            drm::skipped_bands_for_test(),
            0,
            "after a damage box the skip knows nothing again"
        );
        assert_eq!(
            screen.pixel(0, 32),
            tag(0x0055_0000, 0, 32),
            "a band the skip still claimed was left stale on the panel"
        );
    }

    /// Blanking paints the panel black, which is not the frame's pixels. A skip
    /// that kept its hashes across a blank would leave the screen black: every
    /// band would match, so the present that comes back would copy nothing.
    #[test]
    fn a_blank_forgets_what_the_panel_held() {
        let screen = kms_emu::attach(64, 48);
        drm::set_present_skip_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 48);
        paint(&buf, |x, y| tag(0x0066_0000, x, y));
        let fb = c.addfb2(&buf);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        drm::set_crtc_blanked(true);
        assert_eq!(screen.pixel(0, 32), 0, "blanking paints the panel black");
        drm::set_crtc_blanked(false);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

        assert_eq!(
            drm::skipped_bands_for_test(),
            0,
            "nothing is known about a panel that was blanked"
        );
        for y in [0u32, 16, 32, 47] {
            assert_eq!(
                screen.pixel(0, y),
                tag(0x0066_0000, 0, y),
                "row {} stayed black after the blank",
                y
            );
        }
    }

    /// And the repair pass changes nothing about a damage box on the buffer the
    /// panel already carries: the box is still the only thing copied. Arming a
    /// repair must not quietly turn every present into a whole frame.
    #[test]
    fn the_repair_pass_does_not_widen_a_damage_box() {
        let screen = kms_emu::attach(192, 8);
        drm::set_present_repair_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(192, 8);
        paint(&buf, |x, y| tag(0x0044_0000, x, y));
        let fb = c.addfb2(&buf);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

        // Repaint the whole buffer, then damage only one band of it.
        paint(&buf, |x, y| tag(0x0055_0000, x, y));
        dirtyfb(&c, fb, &[clip(64, 0, 128, 8)]);

        assert_eq!(drm::repair_rounds_for_test(), 0);
        for y in 0..8 {
            for x in 0..192 {
                let want = if (64..128).contains(&x) {
                    tag(0x0055_0000, x, y)
                } else {
                    tag(0x0044_0000, x, y)
                };
                assert_eq!(
                    screen.pixel(x, y),
                    want,
                    "pixel ({}, {}) -- the repair widened the box",
                    x,
                    y
                );
            }
        }
    }

    #[test]
    fn a_damage_box_on_a_fresh_buffer_puts_the_whole_frame_up() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        // The frame on the panel.
        let a = c.create_dumb(64, 16);
        paint(&a, |x, y| tag(0x00AA_0000, x, y));
        let fb_a = c.addfb2(&a);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb_a, 64, 16);
        drain_completions(&c);
        assert_eq!(screen.pixel(0, 0), tag(0x00AA_0000, 0, 0));
        assert_eq!(drm::panel_fb_for_test(), fb_a);

        // The next swapchain buffer. Every pixel of it differs from the frame up.
        let b = c.create_dumb(64, 16);
        paint(&b, |x, y| tag(0x0011_0000, x, y));
        let fb_b = c.addfb2(&b);

        dirtyfb(&c, fb_b, &[clip(8, 4, 24, 8)]);

        assert_eq!(
            screen.pixel(10, 5),
            tag(0x0011_0000, 10, 5),
            "the damaged region itself did not reach the panel"
        );
        assert_eq!(
            screen.pixel(40, 12),
            tag(0x0011_0000, 40, 12),
            "outside the box the panel still carries the PREVIOUS framebuffer, so              it is holding two frames at once -- and `repaint_for_cursor` reads              that region back from `crtc_fb` on the next pointer move"
        );
        assert_eq!(drm::panel_fb_for_test(), fb_b);

        c.rmfb(fb_a).expect("RMFB a");
        c.rmfb(fb_b).expect("RMFB b");
        c.destroy_dumb(a.handle).expect("DESTROY_DUMB a");
        c.destroy_dumb(b.handle).expect("DESTROY_DUMB b");
    }

    /// And the optimisation is still there for the case it exists for: the client
    /// re-presents the framebuffer the panel already carries, so the pixels
    /// outside the box really are the ones on screen. Copying the whole frame
    /// here is the 8.3 MB of CPU stores per blinking caret that the damage path
    /// was added to avoid, so "promote everything" would not be a fix.
    #[test]
    fn a_damage_box_on_the_buffer_already_up_still_copies_only_the_box() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let a = c.create_dumb(64, 16);
        paint(&a, |x, y| tag(0x00AA_0000, x, y));
        let fb = c.addfb2(&a);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
        drain_completions(&c);

        // Repaint the WHOLE buffer but report one box, which is the client lying
        // about its damage -- and the kernel is entitled to believe it here.
        paint(&a, |x, y| tag(0x0011_0000, x, y));
        dirtyfb(&c, fb, &[clip(8, 4, 24, 8)]);

        assert_eq!(
            screen.pixel(10, 5),
            tag(0x0011_0000, 10, 5),
            "the box was not copied at all"
        );
        assert_eq!(
            screen.pixel(40, 12),
            tag(0x00AA_0000, 40, 12),
            "the whole frame was copied, so the damage path no longer shrinks              anything"
        );

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
    }

    /// Blanking paints the panel black, so black is what a damage box would be
    /// leaving in place. Un-blanking happens on the next present, at the top of
    /// `present_now_checked` -- and if that present is a damage box the screen
    /// stays black with one rectangle of desktop in it.
    #[test]
    fn the_present_that_unblanks_does_not_leave_one_rectangle_on_black() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let a = c.create_dumb(64, 16);
        paint(&a, |x, y| tag(0x00AA_0000, x, y));
        let fb = c.addfb2(&a);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
        drain_completions(&c);

        drm::set_crtc_blanked(true);
        assert_eq!(screen.pixel(40, 12), 0, "blanking left the panel lit");
        assert_eq!(
            drm::panel_fb_for_test(),
            0,
            "black pixels are not this framebuffer's pixels"
        );

        dirtyfb(&c, fb, &[clip(8, 4, 24, 8)]);
        assert_eq!(
            screen.pixel(40, 12),
            tag(0x00AA_0000, 40, 12),
            "the panel is still black everywhere the box did not touch"
        );

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
    }

    /// A present a pause acknowledged without drawing does not make the panel
    /// carry that framebuffer -- it carries the one from before, which is the
    /// whole point of `SCANOUT_STALE`. Recording it here would tell the next
    /// damage box it may keep its region, on a panel a frame behind.
    #[test]
    fn a_present_a_pause_acknowledged_does_not_claim_the_panel() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let a = c.create_dumb(64, 16);
        paint(&a, |x, y| tag(0x00AA_0000, x, y));
        let fb_a = c.addfb2(&a);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb_a, 64, 16);
        drain_completions(&c);

        let b = c.create_dumb(64, 16);
        paint(&b, |x, y| tag(0x0011_0000, x, y));
        let fb_b = c.addfb2(&b);

        drm::set_scanout_paused(true);
        c.page_flip(drm::SYNTH_CRTC_ID, fb_b, 21).expect("flip");
        drain_completions(&c);
        assert_eq!(
            screen.pixel(0, 0),
            tag(0x00AA_0000, 0, 0),
            "a paused present drew"
        );
        assert_eq!(
            drm::panel_fb_for_test(),
            fb_a,
            "the panel was credited with a frame nobody drew"
        );

        // Resuming puts the whole frame up, which is what makes the panel carry
        // it -- and only then is a box on it meaningful again.
        drm::set_scanout_paused(false);
        assert_eq!(screen.pixel(0, 0), tag(0x0011_0000, 0, 0));
        assert_eq!(drm::panel_fb_for_test(), fb_b);

        c.rmfb(fb_a).expect("RMFB a");
        c.rmfb(fb_b).expect("RMFB b");
        c.destroy_dumb(a.handle).expect("DESTROY_DUMB a");
        c.destroy_dumb(b.handle).expect("DESTROY_DUMB b");
    }

    /// Retiring the framebuffer the panel carries forgets it. Ids are handed out
    /// again, so without this a later buffer landing on the same number would
    /// inherit "already on screen" and have its first box honoured against a
    /// frame that is not its own.
    #[test]
    fn retiring_the_framebuffer_on_the_panel_forgets_it() {
        let _screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let a = c.create_dumb(64, 16);
        paint(&a, |x, y| tag(0x00AA_0000, x, y));
        let fb = c.addfb2(&a);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
        drain_completions(&c);
        assert_eq!(drm::panel_fb_for_test(), fb);

        c.rmfb(fb).expect("RMFB");
        assert_eq!(
            drm::panel_fb_for_test(),
            0,
            "a retired id is still recorded as the frame on the panel"
        );

        c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
    }

    /// A present the console swallowed leaves the panel carrying nothing. While a
    /// text VT is foreground the compositor's pixels are dropped -- reported as
    /// complete so its frame loop keeps running -- and the console prints over the
    /// last frame. So when the graphics VT comes back, the panel is not showing
    /// any framebuffer, and the first present's damage box would paint one
    /// rectangle of desktop into a screen full of console text.
    #[test]
    fn a_present_the_console_swallowed_leaves_the_panel_carrying_nothing() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let a = c.create_dumb(64, 16);
        paint(&a, |x, y| tag(0x00AA_0000, x, y));
        let fb = c.addfb2(&a);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
        drain_completions(&c);
        assert_eq!(drm::panel_fb_for_test(), fb);

        // A text VT is foreground: the compositor owns VT 7, the user is on 1.
        drm::set_graphics_vt_for_test(Some(7));
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 31)
            .expect("a suppressed flip is still reported complete");
        drain_completions(&c);
        assert_eq!(
            drm::panel_fb_for_test(),
            0,
            "the panel is still credited with a frame the console is printing over"
        );

        // Back to the desktop. Repaint the buffer so a box on it would be
        // visibly different from what is up, then damage one corner of it.
        drm::set_graphics_vt_for_test(None);
        paint(&a, |x, y| tag(0x0033_0000, x, y));
        dirtyfb(&c, fb, &[clip(0, 0, 4, 4)]);
        assert_eq!(
            screen.pixel(40, 12),
            tag(0x0033_0000, 40, 12),
            "the first present after the VT came back honoured its box, so the \
             screen is console text with one rectangle of desktop in it"
        );

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
    }

    /// A present that could not put pixels anywhere does not get to say the panel
    /// carries its framebuffer. Crediting it would hand the next damage box a
    /// reference frame that was never drawn.
    #[test]
    fn a_present_that_failed_does_not_claim_the_panel() {
        let _screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let a = c.create_dumb(64, 16);
        paint(&a, |x, y| tag(0x00AA_0000, x, y));
        let fb = c.addfb2(&a);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
        drain_completions(&c);
        assert_eq!(drm::panel_fb_for_test(), fb);

        assert!(
            drm::present_now_checked(0x0BAD_F00D, drm::SYNTH_CRTC_ID, None).is_err(),
            "an id nobody registered must not present"
        );
        assert_eq!(
            drm::panel_fb_for_test(),
            fb,
            "a present that put no pixels anywhere was credited with the panel"
        );

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
    }

    /// Does the cursor patch read past the framebuffer's own right edge?
    ///
    /// The call site's comment says it clips "to what the framebuffer covers
    /// (`fb_width`/`fb_height`), not to the screen: a client fb narrower or
    /// shorter than the display would otherwise have the patch read past the end
    /// of a row -- the next row's pixels -- and paint that onto the scanout as a
    /// shifted square trailing the pointer". The height half does clip to `fh`.
    /// The width half never mentions `fw` again: it bounds `x` at the row PITCH,
    /// which is >= `fw` by construction. So a framebuffer narrower than its own
    /// pitch has the columns in between read and painted.
    #[test]
    fn the_cursor_patch_does_not_paint_the_framebuffers_row_padding() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        // A 40-pixel buffer registered as a 32-pixel-wide framebuffer: eight
        // columns of row padding, and the screen is wider than either.
        let buf = c.create_dumb(40, 16);
        paint(&buf, |x, y| tag(0x0055_0000, x, y));
        let fb = c.addfb2_narrow(&buf, 32);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 16);
        drain_completions(&c);

        // Whatever the present put on screen for the padding columns is the
        // baseline: the cursor must not change it.
        let before: alloc::vec::Vec<u32> = (32..48u32).map(|x| screen.pixel(x, 4)).collect();

        // An 8x8 opaque pointer at the framebuffer's right edge: its patch
        // reaches columns 24..32, and the write-combining widening takes the
        // read out to the row pitch.
        set_cursor(&c, drm::SYNTH_CRTC_ID, buf.handle, 8, 8, 24, 0);
        move_cursor(&c, drm::SYNTH_CRTC_ID, 24, 2);

        let after: alloc::vec::Vec<u32> = (32..48u32).map(|x| screen.pixel(x, 4)).collect();
        assert_eq!(
            before, after,
            "the pointer painted the framebuffer's off-screen row padding onto \
             the visible screen, past the framebuffer's own right edge"
        );

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The probe must be able to say "nothing wrote this window" -- on a buffer
    /// nobody is writing.
    ///
    /// That is the half of the diagnostic that is easy to get wrong and fatal to
    /// get wrong: if a settled present reports, every frame reports, and the
    /// finding it exists to deliver is buried in noise. Here nothing but the
    /// test touches the dumb buffer between the two reads, so the honest answer
    /// is silence.
    #[test]
    fn an_armed_probe_says_nothing_about_a_buffer_nobody_is_writing() {
        let _screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let first = c.create_dumb(64, 16);
        paint(&first, |x, y| tag(0x0011_0000, x, y));
        let fb_first = c.addfb2(&first);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb_first, 64, 16);
        drain_completions(&c);

        drm::set_present_probe_enabled(true);
        let reports_before = drm::probe_reports_for_test();

        let second = c.create_dumb(64, 16);
        paint(&second, |x, y| tag(0x0022_0000, x, y));
        let fb_second = c.addfb2(&second);
        c.page_flip(drm::SYNTH_CRTC_ID, fb_second, 7).expect("flip");
        drain_completions(&c);
        // And the damage path too, because that is the one labwc drives.
        dirtyfb(&c, fb_second, &[clip(8, 4, 24, 8)]);

        assert_eq!(
            drm::probe_reports_for_test(),
            reports_before,
            "a settled buffer was reported as changing under the blit, so every \
             frame will report and the log will say nothing"
        );

        c.rmfb(fb_first).expect("RMFB");
        c.rmfb(fb_second).expect("RMFB");
        c.destroy_dumb(first.handle).expect("DESTROY_DUMB");
        c.destroy_dumb(second.handle).expect("DESTROY_DUMB");
    }

    /// And it must not change what reaches the panel. A diagnostic that alters
    /// the thing it measures is not one: the probe reads the window twice and
    /// invalidates it in between, and the pixels on screen have to be exactly
    /// the ones an unarmed boot would have put there.
    #[test]
    fn an_armed_probe_puts_the_same_pixels_on_the_panel() {
        let _screen = kms_emu::attach(64, 16);
        let c = Client::open(0);

        let fb_buf = c.create_dumb(64, 16);
        paint(&fb_buf, |x, y| tag(0x0033_0000, x, y));
        let fb_id = c.addfb2(&fb_buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb_id, 64, 16);
        drain_completions(&c);

        drm::set_present_probe_enabled(true);
        // Repaint to a value the panel does not already hold, then present a
        // damage box over part of it -- the shape the probe is armed for.
        paint(&fb_buf, |x, y| tag(0x0044_0000, x, y));
        // `clip` is a DRM clip rect: x1, y1, x2, y2, not x/y/w/h.
        dirtyfb(&c, fb_id, &[clip(5, 3, 26, 12)]);

        for y in 3..12u32 {
            for x in 5..26u32 {
                assert_eq!(
                    _screen.pixel(x, y),
                    tag(0x0044_0000, x, y),
                    "the probe changed what the present wrote"
                );
            }
        }

        c.rmfb(fb_id).expect("RMFB");
        c.destroy_dumb(fb_buf.handle).expect("DESTROY_DUMB");
    }

    // -----------------------------------------------------------------------
    // The desktop, simulated: labwc presenting while the pointer moves.
    //
    // The garbage this exists for cannot be photographed into a test. What CAN
    // be written down is the contract the screen owes its user: it shows the
    // frame the compositor last presented, with the pointer on top, and nothing
    // else. Every step below checks the WHOLE panel against that, so a stray
    // rectangle is caught wherever it lands and whatever it holds -- a piece of
    // an older frame, a piece of one not presented yet, or black.
    // -----------------------------------------------------------------------

    /// One pixel of the desktop labwc composes for frame `n`.
    ///
    /// The frame number is in every pixel and the alpha byte is always `0xff`,
    /// which buys two things. A window of another frame pasted into this one
    /// does not resemble anything here, so it is caught by value and not merely
    /// by position. And `0x00000000` -- the black of the rectangles -- cannot
    /// come out of any legitimate scene, so if the panel holds it, the kernel
    /// put it there.
    fn desktop_px(n: u32, x: u32, y: u32) -> u32 {
        0xFF00_0000 | ((n & 0xff) << 16) | ((y & 0xff) << 8) | (x & 0xff)
    }

    /// What the screen ought to show, kept beside the emulated panel.
    struct Panel {
        w: u32,
        h: u32,
        /// The frame the compositor last PRESENTED, pixel for pixel. Not the
        /// buffer it currently holds: a buffer being drawn into is not a frame
        /// anybody has asked for.
        scene: alloc::vec::Vec<u32>,
        /// Where the pointer is drawn and how big it is. The position is signed
        /// because a pointer really does hang off the left and top edges: the
        /// kernel clips it, and a model that could not express that would never
        /// ask what the clipping does.
        cursor: Option<(i32, i32, u32, u32)>,
        /// The pointer image, `bmp_w` pixels per row, alpha `0xff` or `0x00`
        /// only -- a partly transparent pointer would need the blend written
        /// out twice, and what these tests are about is what shows THROUGH it.
        bmp: alloc::vec::Vec<u32>,
        bmp_w: u32,
    }

    impl Panel {
        fn new(w: u32, h: u32, frame: u32, bmp: alloc::vec::Vec<u32>, bmp_w: u32) -> Self {
            let mut p = Panel {
                w,
                h,
                scene: alloc::vec::Vec::new(),
                cursor: None,
                bmp,
                bmp_w,
            };
            p.present(frame);
            p
        }

        /// The panel was painted one colour, by a blank, and nothing is drawn
        /// on top of it.
        fn fill(&mut self, px: u32) {
            self.scene.clear();
            self.scene.resize((self.w * self.h) as usize, px);
            self.cursor = None;
        }

        /// A damage box of frame `n` went up and the rest of the panel kept the
        /// frame that was already there -- the panel holding two frames at once,
        /// on purpose, which is legitimate only while the box really is all that
        /// changed.
        fn present_box(&mut self, n: u32, x: u32, y: u32, w: u32, h: u32) {
            for py in y..(y + h).min(self.h) {
                for px in x..(x + w).min(self.w) {
                    self.scene[(py * self.w + px) as usize] = desktop_px(n, px, py);
                }
            }
        }

        /// The compositor put frame `n` up, whole.
        fn present(&mut self, n: u32) {
            self.scene.resize((self.w * self.h) as usize, 0);
            for y in 0..self.h {
                for x in 0..self.w {
                    self.scene[(y * self.w + x) as usize] = desktop_px(n, x, y);
                }
            }
        }

        fn want(&self, x: u32, y: u32) -> u32 {
            if let Some((cx, cy, cw, ch)) = self.cursor {
                let (dx, dy) = (x as i64 - cx as i64, y as i64 - cy as i64);
                if dx >= 0 && dx < cw as i64 && dy >= 0 && dy < ch as i64 {
                    let s = self.bmp[(dy as u32 * self.bmp_w + dx as u32) as usize];
                    if s >> 24 != 0 {
                        return s | 0xFF00_0000;
                    }
                }
            }
            self.scene[(y * self.w + x) as usize]
        }

        /// Compare every visible pixel, and say in the failure which of the two
        /// ways it is wrong -- the two need work in opposite places.
        fn check(&self, screen: &kms_emu::Screen, step: &str) {
            for y in 0..self.h {
                for x in 0..self.w {
                    let want = self.want(x, y);
                    let got = screen.pixel(x, y);
                    if got == want {
                        continue;
                    }
                    let how = if got == 0 {
                        ", and it is BLACK: no scene of this desktop holds a \
                         zero pixel, so the kernel put it there"
                    } else if got == UNTOUCHED {
                        ", and nothing ever wrote it"
                    } else {
                        ", which is a piece of another frame"
                    };
                    panic!(
                        "{}: pixel ({}, {}) reads {:#010x} and the frame on \
                         screen says {:#010x}{}",
                        step, x, y, got, want, how
                    );
                }
            }
        }
    }

    /// A pointer image: opaque in a cross, transparent in the corners, so the
    /// desktop shows through it and a wrong read under the pointer is visible
    /// rather than hidden behind an opaque square.
    fn pointer_bitmap(w: u32, h: u32) -> alloc::vec::Vec<u32> {
        let mut v = alloc::vec::Vec::new();
        for y in 0..h {
            for x in 0..w {
                let on = x >= w / 4 && x < w - w / 4 || y >= h / 4 && y < h - h / 4;
                v.push(if on { 0xFFFF_FFFF } else { 0x0000_0000 });
            }
        }
        v
    }

    /// A damage box must not let the pointer paste the rest of the client's
    /// buffer onto the screen.
    ///
    /// The pointer is composited on top of every present, over a window widened
    /// to whole write-combining lines, so its window routinely reaches outside a
    /// damage box. Inside the box the client's buffer and the panel hold the same
    /// pixels. Outside it they do not: the panel holds the frame that was there
    /// before, and the buffer holds whatever the client has in it now -- for a
    /// compositor that redraws only its damage an older frame, and for one that
    /// has begun the next frame transparent black. Taking the buffer's pixels
    /// there puts a rectangle the client never declared on the screen, right
    /// where the pointer is.
    ///
    /// Same fault as the one `CursorUnder` is named for, reached through the
    /// present instead of through a pointer move, and found by the damage-box
    /// steps of the soak below rather than by anybody thinking of it.
    #[test]
    fn a_damage_box_must_not_let_the_pointer_paste_the_rest_of_the_clients_buffer() {
        const W: u32 = 120;
        const H: u32 = 96;
        const CUR: u32 = 16;
        let screen = kms_emu::attach_with(W, H, 128, true);
        let c = Client::open(0);
        let buf = c.create_dumb(W, H);
        paint(&buf, |x, y| desktop_px(1, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
        drain_completions(&c);

        let bmp = pointer_bitmap(CUR, CUR);
        let cur = c.create_dumb(CUR, CUR);
        {
            let px = map_dumb(&cur);
            for (i, v) in bmp.iter().enumerate() {
                px[i] = *v;
            }
        }
        // Near the top, far above the damage box: the pointer's window lies
        // wholly outside what the present is about to copy.
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, CUR, CUR, 38, 4);
        let mut panel = Panel::new(W, H, 1, bmp.clone(), CUR);
        panel.cursor = Some((38, 4, CUR, CUR));
        panel.check(&screen, "frame 1 with the pointer on it");

        // The client redraws ONE box and starts the next frame everywhere else,
        // which is a renderer clearing to transparent black. Then it declares
        // just the box.
        let (bx, by, bw, bh) = (32u32, 48u32, 64u32, 32u32);
        paint(&buf, |x, y| {
            if (bx..bx + bw).contains(&x) && (by..by + bh).contains(&y) {
                desktop_px(2, x, y)
            } else {
                0x0000_0000
            }
        });
        dirtyfb(
            &c,
            fb,
            &[clip(
                bx as u16,
                by as u16,
                (bx + bw) as u16,
                (by + bh) as u16,
            )],
        );
        drain_completions(&c);

        panel.present_box(2, bx, by, bw, bh);
        panel.check(&screen, "the damage box went up and the pointer did not");
        // And it is the save that did it, not a lucky agreement between two
        // sources: every pixel of this window came from outside the box.
        assert!(
            drm::cursor_px_from_save_for_test() > 0,
            "the pointer composited without reading the save, so this test \
             passed for some other reason"
        );

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// Two small pieces of damage far apart must not drag the whole span between
    /// them through the aperture.
    ///
    /// A DIRTYFB with several clip rects used to go up as their bounding union,
    /// and for the two boxes a toolkit really sends -- a menu here, the shadow it
    /// dropped over there -- that union is most of the screen. The panel's
    /// aperture takes writes at about 42 MB/s, so a whole 1920x1080 span is ~99 ms
    /// and three dropped frames for what the client said was two 16x16 boxes.
    ///
    /// The screen cannot tell the two apart for a client that painted its whole
    /// buffer, which is every client a test writes, so this paints the WHOLE
    /// buffer with the new frame and then asserts that what is between the boxes
    /// still holds the old one. That is only true if the boxes went up as boxes.
    /// The span count is asserted as well, for the cases where the union is the
    /// right answer and the screen agrees either way.
    #[test]
    fn two_far_apart_damage_boxes_do_not_drag_the_whole_span_between_them() {
        const W: u32 = 120;
        const H: u32 = 96;
        let screen = kms_emu::attach_with(W, H, 128, true);
        let c = Client::open(0);
        let buf = c.create_dumb(W, H);
        paint(&buf, |x, y| desktop_px(1, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
        drain_completions(&c);
        let mut panel = Panel::new(W, H, 1, alloc::vec::Vec::new(), 0);
        panel.check(&screen, "the first frame");

        // Two 16x16 boxes in opposite corners. Their union is 80x72, thirty times
        // what they are.
        paint(&buf, |x, y| desktop_px(2, x, y));
        dirtyfb(&c, fb, &[clip(16, 8, 32, 24), clip(80, 64, 96, 80)]);
        drain_completions(&c);
        assert_eq!(
            dirty_spans_blitted_for_test(),
            2,
            "two far-apart boxes went up as one span"
        );
        panel.present_box(2, 16, 8, 16, 16);
        panel.present_box(2, 80, 64, 16, 16);
        panel.check(
            &screen,
            "two boxes went up and the span between them did not",
        );

        // Two boxes that touch. Splitting these copies the same bytes twice for
        // nothing, so the union is the right answer and the count says so.
        paint(&buf, |x, y| desktop_px(3, x, y));
        dirtyfb(&c, fb, &[clip(16, 8, 48, 24), clip(32, 8, 64, 24)]);
        drain_completions(&c);
        assert_eq!(
            dirty_spans_blitted_for_test(),
            1,
            "two touching boxes went up separately, copying the overlap twice"
        );
        panel.present_box(3, 16, 8, 48, 16);
        panel.check(&screen, "two touching boxes went up as one span");

        // More boxes than the kernel will track one by one, and far enough apart
        // that it would otherwise rather split them: the union, because keeping
        // only the first eight would DROP the ninth box and leave that piece of
        // the client's redraw off the screen.
        paint(&buf, |x, y| desktop_px(4, x, y));
        let many: alloc::vec::Vec<DrmClipRect> =
            (0..9).map(|i| clip(0, i * 10, 16, i * 10 + 4)).collect();
        dirtyfb(&c, fb, &many);
        drain_completions(&c);
        assert_eq!(
            dirty_spans_blitted_for_test(),
            1,
            "nine boxes were tracked one by one, so the ninth went nowhere"
        );
        panel.present_box(4, 0, 0, 16, 84);
        panel.check(&screen, "nine boxes went up as one span");

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// Destroying the framebuffer that is ON THE SCREEN turns the pipe off, and
    /// the next whole frame brings the pointer back with nothing left behind.
    ///
    /// `drm_mode_rmfb` goes through `drm_framebuffer_remove`, and on an atomic
    /// driver `atomic_remove_fb` disables the CRTC whose primary plane showed
    /// the fb (mode NULL, `active = false`): the output goes dark, which is why
    /// no compositor removes the framebuffer it is scanning out (wlroots keeps
    /// the buffer locked until the next flip has landed). This test used to
    /// expect the panel to keep the freed frame, which is what the kernel did
    /// before it followed Linux here. What it guards is the pointer across that
    /// state: a move while the pipe is off draws nothing, because there is no
    /// scanout to draw on, and when a client puts a whole frame up again the
    /// pointer arrives at its latest position with no ghost of the old one --
    /// the fault this was written for was a move with `crtc_fb == 0` that moved
    /// the bookkeeping (`cursor.drawn`) without moving the image.
    #[test]
    fn destroying_the_framebuffer_on_screen_turns_the_pipe_off_and_the_pointer_comes_back_with_the_next_frame(
    ) {
        const W: u32 = 120;
        const H: u32 = 96;
        const CUR: u32 = 16;
        let screen = kms_emu::attach_with(W, H, 128, true);
        let c = Client::open(0);
        let buf = c.create_dumb(W, H);
        paint(&buf, |x, y| desktop_px(1, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
        drain_completions(&c);

        let bmp = pointer_bitmap(CUR, CUR);
        let cur = c.create_dumb(CUR, CUR);
        {
            let px = map_dumb(&cur);
            for (i, v) in bmp.iter().enumerate() {
                px[i] = *v;
            }
        }
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, CUR, CUR, 20, 20);
        let mut panel = Panel::new(W, H, 1, bmp.clone(), CUR);
        panel.cursor = Some((20, 20, CUR, CUR));
        panel.check(&screen, "the pointer is on the frame");

        // The client drops the framebuffer it presented: the CRTC that showed
        // it is disabled, so the panel goes dark, pointer included.
        c.rmfb(fb).expect("RMFB the framebuffer on the CRTC");
        assert_eq!(
            drm::crtc_fb(),
            0,
            "RMFB really unbinds the CRTC's framebuffer"
        );
        assert!(
            drm::crtc_blanked(),
            "RMFB of the framebuffer on the CRTC disables it (atomic_remove_fb)"
        );
        let dark = |what: &str| {
            for y in 0..H {
                for x in 0..W {
                    assert_eq!(
                        screen.pixel(x, y),
                        0,
                        "{}: pixel ({}, {}) is lit",
                        what,
                        x,
                        y
                    );
                }
            }
        };
        dark("the pipe went dark");

        // A move with the pipe off: nothing to draw on, so nothing is drawn --
        // and nothing of the old image is left to find later.
        move_cursor(&c, drm::SYNTH_CRTC_ID, 60, 40);
        dark("a pointer move on a dark pipe");

        // The client puts a whole frame up again. The pointer is where it was
        // last moved to, and nowhere else.
        let buf2 = c.create_dumb(W, H);
        paint(&buf2, |x, y| desktop_px(2, x, y));
        let fb2 = c.addfb2(&buf2);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb2, W, H);
        drain_completions(&c);
        let mut panel = Panel::new(W, H, 2, bmp.clone(), CUR);
        panel.cursor = Some((60, 40, CUR, CUR));
        panel.check(
            &screen,
            "the frame came back with the pointer at its new place",
        );

        // And it still comes off entirely.
        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        panel.cursor = None;
        panel.check(&screen, "the pointer was hidden");

        c.rmfb(fb2).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
        c.destroy_dumb(buf2.handle).expect("DESTROY_DUMB 2");
    }

    /// A pointer straddling the right edge of a framebuffer SMALLER than the mode
    /// must still be erasable, or its outer columns stay on the panel for good.
    ///
    /// The two paths clip the pointer's window against different things. The
    /// present composites into the client's buffer, so it clips to the buffer:
    /// for a 96-wide framebuffer on a 120-wide mode, a pointer at x=92 gets a
    /// window that stops at x=96. The move path draws straight onto the panel, so
    /// it clips to the PANEL: the same pointer gets a window reaching x=112. Each
    /// one saves what its own window covered, so a present after a move recorded
    /// the narrow window over the wide one -- and the next erase put back only 16
    /// of the 20 columns. The four it did not own were pointer pixels, on top of
    /// the desktop, with nothing left that knew they were there.
    ///
    /// This is not the fault Moebius is looking at (labwc presents a framebuffer
    /// the size of the mode), but it is the same family: a window written in one
    /// place and restored in another.
    #[test]
    fn a_pointer_past_the_edge_of_a_narrow_framebuffer_must_still_come_off() {
        const W: u32 = 120;
        const H: u32 = 96;
        const SMALL_W: u32 = 96;
        const SMALL_H: u32 = 80;
        const CUR: u32 = 16;
        let screen = kms_emu::attach_with(W, H, 128, true);
        let c = Client::open(0);

        // Frame 1 over the whole panel, so the part the narrow framebuffer never
        // touches holds a frame of its own and a leftover there is visible.
        let big = c.create_dumb(W, H);
        paint(&big, |x, y| desktop_px(1, x, y));
        let fb_big = c.addfb2(&big);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb_big, W, H);
        drain_completions(&c);

        let small = c.create_dumb(SMALL_W, SMALL_H);
        let fb_small = c.addfb2(&small);

        let bmp = pointer_bitmap(CUR, CUR);
        let cur = c.create_dumb(CUR, CUR);
        {
            let px = map_dumb(&cur);
            for (i, v) in bmp.iter().enumerate() {
                px[i] = *v;
            }
        }
        let mut panel = Panel::new(W, H, 1, bmp.clone(), CUR);

        // Straddling x=96: four columns of the pointer land where the narrow
        // framebuffer does not reach. A pointer move draws them; only a present
        // that knows they are there can undo them.
        let (px0, py0) = (92i32, 15i32);
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, CUR, CUR, px0, py0);
        panel.cursor = Some((px0, py0, CUR, CUR));
        panel.check(&screen, "the pointer hangs off the narrow framebuffer");

        // The narrow present composites the pointer again, and this is where the
        // window used to shrink.
        paint(&small, |x, y| desktop_px(2, x, y));
        // A flip onto a framebuffer the mode does not fit in is ENOSPC
        // (`drm_mode_page_flip_ioctl`, like `SETCRTC` in
        // `a_framebuffer_smaller_than_the_mode_leaves_the_rest_of_the_screen_alone`),
        // so the narrow scanout is reached the way the kernel's own callers
        // reach it, and the CRTC is left holding the fb as the flip did.
        drm::present_now_checked(fb_small, drm::SYNTH_CRTC_ID, None)
            .expect("present the narrow framebuffer");
        drm::set_crtc_fb(drm::SYNTH_CRTC_ID, fb_small);
        panel.present_box(2, 0, 0, SMALL_W, SMALL_H);
        panel.check(&screen, "frame 2 from the narrow framebuffer");
        assert!(
            drm::cursor_windows_widened_for_test() > 0,
            "the composite never widened its window, so this test is not \
             exercising the fix"
        );

        // Now take the pointer away. Everything it drew has to come off, columns
        // past the framebuffer's edge included.
        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        panel.cursor = None;
        panel.check(&screen, "the pointer was hidden and left nothing behind");

        c.rmfb(fb_small).expect("RMFB small");
        c.rmfb(fb_big).expect("RMFB big");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(small.handle).expect("DESTROY_DUMB small");
        c.destroy_dumb(big.handle).expect("DESTROY_DUMB big");
    }

    /// Three hundred steps of the desktop, in a deterministic order nobody chose
    /// by hand, with the whole panel checked against the contract after every
    /// one of them.
    ///
    /// The single-scenario tests above each say "this exact sequence must not do
    /// this exact wrong thing", which only ever catches the fault whoever wrote
    /// them had already thought of. Moebius reports rectangles that are still
    /// there after the one cause we found, so what is needed now is the opposite
    /// shape of test: put the panel through combinations of presents, pointer
    /// moves, pointer hides, buffer reuse and blank round-trips, and assert the
    /// one rule the user reads off the screen -- the panel shows the frame that
    /// was last PRESENTED, with the pointer on top, and nothing else -- after
    /// every single step. A failing seed prints the step that broke it and the
    /// operation log, which is a reproduction.
    ///
    /// Two buffers, alternating, because that is what wlroots does, and the
    /// scribble step blacks out a box in the buffer the kernel last copied FROM:
    /// a released buffer the compositor has started drawing into. Every pixel of
    /// a legitimate frame is opaque and carries its frame number, so a black
    /// pixel or a pixel from another frame is caught by value, not by position.
    #[test]
    fn three_hundred_steps_of_desktop_never_leave_anything_but_the_frame_and_the_pointer() {
        // Every combination of the two present flags, because the band skip is
        // the one mechanism that can decide a band is already on the panel and
        // never copy it again -- so a stale pixel it leaves stays for good --
        // and the repair is the other writer of the panel that does not go
        // through the blit. Two seeds each, so the order of operations is not
        // one order.
        let mut widened = 0usize;
        for (skip, repair) in [(false, false), (true, false), (false, true), (true, true)] {
            for seed in [0x5EED_1234u32, 0x0BAD_C0DE] {
                widened += soak_the_desktop(seed, skip, repair);
            }
        }
        // A present on a framebuffer smaller than the mode has to have caught the
        // pointer across its edge at least once in all of this, because that is
        // the only thing that leaves a piece of the pointer to erase later. If it
        // never happened, those steps are not exercising what they were added for
        // -- and a whole fix would be untested with every test still green.
        assert!(
            widened > 0,
            "no composite in 2400 steps widened its window, so the \
             smaller-than-the-mode steps never put the pointer across the edge"
        );
    }

    fn soak_the_desktop(seed_in: u32, skip: bool, repair: bool) -> usize {
        const W: u32 = 120;
        const H: u32 = 96;
        const CUR: u32 = 16;
        let screen = kms_emu::attach_with(W, H, 128, true);
        drm::set_present_skip_enabled(skip);
        drm::set_present_repair_enabled(repair);
        let c = Client::open(0);

        let bufs = [c.create_dumb(W, H), c.create_dumb(W, H)];
        let mut fbs = [c.addfb2(&bufs[0]), c.addfb2(&bufs[1])];
        // A framebuffer SMALLER than the mode, which is a case the present path
        // has its own arithmetic for (`image_pitch_px`, and the pointer clipped
        // to what the buffer covers rather than to the row stride). Its width is
        // a multiple of 16 so the write-combining widening of the copy is a
        // no-op and the model does not have to know how the kernel widens.
        const SMALL_W: u32 = 96;
        const SMALL_H: u32 = 80;
        let small = c.create_dumb(SMALL_W, SMALL_H);
        let fb_small = c.addfb2(&small);
        let bmp = pointer_bitmap(CUR, CUR);
        let cur = c.create_dumb(CUR, CUR);
        {
            let px = map_dumb(&cur);
            for (i, v) in bmp.iter().enumerate() {
                px[i] = *v;
            }
        }
        // A pointer of a DIFFERENT size, because a theme change or a client
        // setting its own cursor really does hand the kernel another bitmap while
        // the old one is on the screen. What the old one covered has to come back
        // even though the new window is not the old window.
        const CUR2: u32 = 8;
        let bmp2 = pointer_bitmap(CUR2, CUR2);
        let cur2 = c.create_dumb(CUR2, CUR2);
        {
            let px = map_dumb(&cur2);
            for (i, v) in bmp2.iter().enumerate() {
                px[i] = *v;
            }
        }
        // And the size a real theme actually uses. On a 120x96 panel a 64x64
        // pointer is a third of the screen, so its window is clipped on two edges
        // at once for most positions -- and what it covers is 16 KB of save, where
        // the 16x16 one was 1 KB.
        const CUR3: u32 = 64;
        let bmp3 = pointer_bitmap(CUR3, CUR3);
        let cur3 = c.create_dumb(CUR3, CUR3);
        {
            let px = map_dumb(&cur3);
            for (i, v) in bmp3.iter().enumerate() {
                px[i] = *v;
            }
        }
        // A framebuffer BIGGER than the mode. The copy stops at the panel's edge,
        // so the pointer's window is computed against a buffer whose rows run past
        // the screen -- the opposite arithmetic to the smaller one, and the case
        // the window widening has to refuse.
        const BIG_W: u32 = 160;
        const BIG_H: u32 = 128;
        let big = c.create_dumb(BIG_W, BIG_H);
        let fb_big = c.addfb2(&big);
        let mut cur_sz = CUR;
        let mut cur_h = cur.handle;

        let mut frame: u32 = 1;
        let mut slot = 0usize;
        paint(&bufs[slot], |x, y| desktop_px(frame, x, y));
        set_crtc(&c, drm::SYNTH_CRTC_ID, fbs[slot], W, H);
        drain_completions(&c);
        let mut model = Panel::new(W, H, frame, bmp.clone(), CUR);
        model.check(&screen, "the first frame");

        // Which framebuffer the panel is holding entirely. A damage box is only
        // honoured while the panel already holds that framebuffer: otherwise the
        // rest of the panel belongs to a different frame and honouring the box
        // would leave two frames on screen at once, which is what put a menu on
        // the screen twice once already.
        let mut panel_fb = fbs[slot];
        let mut pos = (8i32, 8i32);
        let mut shown = false;
        // The most bands any one present left alone. Asserted below, because a
        // soak that never took the skip would say nothing about it -- the one
        // lesson of the probe that measured in the wrong place.
        let mut most_skipped = 0usize;
        // Damage flushes that went up as their own boxes rather than as one
        // bounding span.
        let mut by_box = 0usize;
        let mut log: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
        // A 32-bit LCG: the sequence is fixed, so a failure here reproduces on
        // any machine, and the log below names the step.
        let mut seed: u32 = seed_in;
        let mut rnd = |m: u32| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 13) % m
        };
        for step in 0..300u32 {
            let what;
            match rnd(18) {
                0..=3 => {
                    // The compositor renders a finished frame into the other
                    // buffer of its chain and puts it up, whole, as labwc does.
                    frame += 1;
                    slot ^= 1;
                    let f = frame;
                    paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                    c.page_flip(drm::SYNTH_CRTC_ID, fbs[slot], step as u64)
                        .expect("flip");
                    drain_completions(&c);
                    model.present(frame);
                    panel_fb = fbs[slot];
                    // A full present composites the pointer at wherever it is
                    // now, so that is what is on the panel.
                    model.cursor = if shown {
                        Some((pos.0, pos.1, cur_sz, cur_sz))
                    } else {
                        None
                    };
                    what = alloc::format!("step {}: presented frame {}", step, frame);
                }
                4..=6 => {
                    // Anywhere from wholly off the left or top edge to wholly
                    // past the right or bottom one. The kernel clips both the
                    // window it draws and the window it reads back, and those
                    // two have to clip the same way or the pointer leaves its
                    // widened margins on the screen for good.
                    pos = (
                        rnd(W + 2 * CUR) as i32 - CUR as i32,
                        rnd(H + 2 * CUR) as i32 - CUR as i32,
                    );
                    move_cursor(&c, drm::SYNTH_CRTC_ID, pos.0, pos.1);
                    if shown {
                        model.cursor = Some((pos.0, pos.1, cur_sz, cur_sz));
                    }
                    what = alloc::format!("step {}: pointer moved to {:?}", step, pos);
                }
                7 => {
                    // The released buffer is the compositor's again and it has
                    // started the next frame in it by clearing a box to
                    // transparent black. Nothing is presented: this changes
                    // nothing that may reach the screen.
                    let (bx, by) = (rnd(W - 8), rnd(H - 8));
                    let (bw, bh) = (rnd(W - bx) + 1, rnd(H - by) + 1);
                    let stride = (bufs[slot].pitch / 4) as usize;
                    let px = map_dumb(&bufs[slot]);
                    for y in by..by + bh {
                        for x in bx..bx + bw {
                            px[y as usize * stride + x as usize] = 0x0000_0000;
                        }
                    }
                    what = alloc::format!(
                        "step {}: the client cleared {}x{}+{}+{} in the buffer it \
                         had presented",
                        step,
                        bw,
                        bh,
                        bx,
                        by
                    );
                }
                8 => {
                    // The scene has not changed and the compositor puts it up
                    // again, which is what an idle desktop does all day. This is
                    // the ONLY operation the band skip can act on: every band
                    // hashes to what the panel is already holding, so a band
                    // whose pixels are not really there stays wrong for good.
                    slot ^= 1;
                    let f = frame;
                    paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                    c.page_flip(drm::SYNTH_CRTC_ID, fbs[slot], step as u64)
                        .expect("flip");
                    drain_completions(&c);
                    // The same pixels the model already held -- unless a bare
                    // blank has painted the panel one colour since, which is
                    // exactly what a re-present has to put right.
                    model.present(frame);
                    panel_fb = fbs[slot];
                    model.cursor = if shown {
                        Some((pos.0, pos.1, cur_sz, cur_sz))
                    } else {
                        None
                    };
                    most_skipped = most_skipped.max(drm::skipped_bands_for_test());
                    what = alloc::format!("step {}: presented frame {} AGAIN", step, frame);
                }
                12 => {
                    shown = !shown;
                    if shown {
                        set_cursor(&c, drm::SYNTH_CRTC_ID, cur_h, cur_sz, cur_sz, pos.0, pos.1);
                        model.cursor = Some((pos.0, pos.1, cur_sz, cur_sz));
                    } else {
                        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
                        model.cursor = None;
                    }
                    what = alloc::format!("step {}: pointer shown={}", step, shown);
                }
                9 => {
                    // A client with damage tracking: it redraws a box and asks
                    // for that box only. Sometimes into the buffer the panel is
                    // already holding, where the box may be honoured, and
                    // sometimes into the other one, where honouring it would put
                    // two frames on the screen at once.
                    frame += 1;
                    if rnd(2) == 0 {
                        slot ^= 1;
                    }
                    let f = frame;
                    paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                    // x aligned to 16 so the write-combining widening of the box
                    // is a no-op: the model then says what the SCREEN must show
                    // without having to know how the kernel widens anything.
                    let bx = rnd(W / 16) * 16;
                    let bw = (rnd(W / 16 - bx / 16) + 1) * 16;
                    let by = rnd(H - 1);
                    let bh = rnd(H - by) + 1;
                    dirtyfb(
                        &c,
                        fbs[slot],
                        &[clip(
                            bx as u16,
                            by as u16,
                            (bx + bw) as u16,
                            (by + bh) as u16,
                        )],
                    );
                    drain_completions(&c);
                    if fbs[slot] == panel_fb {
                        model.present_box(frame, bx, by, bw, bh);
                        what = alloc::format!(
                            "step {}: frame {} as a damage box {}x{}+{}+{}",
                            step,
                            frame,
                            bw,
                            bh,
                            bx,
                            by
                        );
                    } else {
                        // The panel was holding another framebuffer, so the box
                        // cannot stand alone and the whole frame has to go up.
                        model.present(frame);
                        what = alloc::format!(
                            "step {}: frame {} as a damage box on a framebuffer the \
                             panel was not holding, so the whole frame",
                            step,
                            frame
                        );
                    }
                    panel_fb = fbs[slot];
                    model.cursor = if shown {
                        Some((pos.0, pos.1, cur_sz, cur_sz))
                    } else {
                        None
                    };
                }
                13 => {
                    // A client whose framebuffer is smaller than the mode. The
                    // present copies what the buffer covers and the rest of the
                    // panel keeps the frame it had, so the panel holds two frames
                    // at once -- and the pointer is composited over a window the
                    // copy may not reach.
                    frame += 1;
                    let f = frame;
                    paint(&small, |x, y| desktop_px(f, x, y));
                    // A flip onto it is ENOSPC (the mode does not fit), so the
                    // present is made the way the kernel's own callers make it
                    // and the CRTC is left holding the fb as the flip did.
                    drm::present_now_checked(fb_small, drm::SYNTH_CRTC_ID, None)
                        .expect("present the narrow framebuffer");
                    drm::set_crtc_fb(drm::SYNTH_CRTC_ID, fb_small);
                    model.present_box(frame, 0, 0, SMALL_W, SMALL_H);
                    // The panel does not hold this framebuffer entirely -- only
                    // its top-left corner -- so no damage box on it may be
                    // honoured on its own.
                    panel_fb = 0;
                    model.cursor = if shown {
                        Some((pos.0, pos.1, cur_sz, cur_sz))
                    } else {
                        None
                    };
                    what = alloc::format!(
                        "step {}: frame {} from a {}x{} framebuffer, smaller than the mode",
                        step,
                        frame,
                        SMALL_W,
                        SMALL_H
                    );
                }
                10 => {
                    // DPMS off and on again, with the frame that follows it --
                    // which is what a screen coming back actually looks like.
                    drm::set_crtc_blanked(true);
                    drm::set_crtc_blanked(false);
                    frame += 1;
                    slot ^= 1;
                    let f = frame;
                    paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                    c.page_flip(drm::SYNTH_CRTC_ID, fbs[slot], step as u64)
                        .expect("flip");
                    drain_completions(&c);
                    model.present(frame);
                    panel_fb = fbs[slot];
                    model.cursor = if shown {
                        Some((pos.0, pos.1, cur_sz, cur_sz))
                    } else {
                        None
                    };
                    what = alloc::format!("step {}: blanked, unblanked, frame {}", step, frame);
                }
                14 => {
                    // A client whose framebuffer is BIGGER than the mode.
                    frame += 1;
                    let f = frame;
                    paint(&big, |x, y| desktop_px(f, x, y));
                    c.page_flip(drm::SYNTH_CRTC_ID, fb_big, step as u64)
                        .expect("flip the oversized framebuffer");
                    drain_completions(&c);
                    // Its top-left corner holds the same pixels a framebuffer of
                    // the mode's size would, so the screen must show this frame
                    // entirely.
                    model.present(frame);
                    // The panel holds a corner of this framebuffer, not the whole
                    // of it, so no damage box may be honoured on its own after
                    // this.
                    panel_fb = 0;
                    model.cursor = if shown {
                        Some((pos.0, pos.1, cur_sz, cur_sz))
                    } else {
                        None
                    };
                    what = alloc::format!(
                        "step {}: frame {} from a {}x{} framebuffer, bigger than the mode",
                        step,
                        frame,
                        BIG_W,
                        BIG_H
                    );
                }
                15 => {
                    // TWO damage boxes in one DIRTYFB, which is what a client with
                    // real damage tracking sends: a menu and the shadow it dropped
                    // somewhere else. The kernel may copy them in one span or in
                    // two, and either way nothing between them may move.
                    frame += 1;
                    let f = frame;
                    paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                    let bx = rnd(W / 32) * 16;
                    let bw = (rnd(2) + 1) * 16;
                    let by = rnd(H / 2);
                    let bh = rnd(H / 2 - by) + 1;
                    let cx2 = 64 + rnd(2) * 16;
                    let cw2 = (rnd(2) + 1) * 16;
                    let cy2 = H / 2 + rnd(H / 4);
                    let ch2 = rnd(H - cy2) + 1;
                    dirtyfb(
                        &c,
                        fbs[slot],
                        &[
                            clip(bx as u16, by as u16, (bx + bw) as u16, (by + bh) as u16),
                            clip(
                                cx2 as u16,
                                cy2 as u16,
                                (cx2 + cw2) as u16,
                                (cy2 + ch2) as u16,
                            ),
                        ],
                    );
                    drain_completions(&c);
                    if fbs[slot] == panel_fb {
                        // The TWO boxes and nothing between them: far apart and
                        // small, so the kernel blits each one instead of their
                        // bounding union, and everything outside them keeps the
                        // frame it had. Both x spans are 16-aligned so the
                        // write-combining widening of each is a no-op.
                        model.present_box(frame, bx, by, bw, bh);
                        model.present_box(frame, cx2, cy2, cw2, ch2);
                        assert_eq!(
                            dirty_spans_blitted_for_test(),
                            2,
                            "step {}: two boxes {}x{}+{}+{} and {}x{}+{}+{} went up \
                             as one span, so the screen agreeing means nothing",
                            step,
                            bw,
                            bh,
                            bx,
                            by,
                            cw2,
                            ch2,
                            cx2,
                            cy2
                        );
                        by_box += 1;
                        what = alloc::format!(
                            "step {}: frame {} as TWO damage boxes {}x{}+{}+{} and {}x{}+{}+{}",
                            step,
                            frame,
                            bw,
                            bh,
                            bx,
                            by,
                            cw2,
                            ch2,
                            cx2,
                            cy2
                        );
                    } else {
                        model.present(frame);
                        what = alloc::format!(
                            "step {}: frame {} as two damage boxes on a framebuffer the \
                             panel was not holding, so the whole frame",
                            step,
                            frame
                        );
                    }
                    panel_fb = fbs[slot];
                    model.cursor = if shown {
                        Some((pos.0, pos.1, cur_sz, cur_sz))
                    } else {
                        None
                    };
                }
                16 => {
                    // The pointer changes SIZE where it stands. The old window is
                    // not the new window, so what the old pointer covered can only
                    // come back from what the blit that drew it saved.
                    cur_sz = match cur_sz {
                        CUR => CUR2,
                        CUR2 => CUR3,
                        _ => CUR,
                    };
                    cur_h = match cur_sz {
                        CUR => cur.handle,
                        CUR2 => cur2.handle,
                        _ => cur3.handle,
                    };
                    model.bmp = match cur_sz {
                        CUR => bmp.clone(),
                        CUR2 => bmp2.clone(),
                        _ => bmp3.clone(),
                    };
                    model.bmp_w = cur_sz;
                    shown = true;
                    set_cursor(&c, drm::SYNTH_CRTC_ID, cur_h, cur_sz, cur_sz, pos.0, pos.1);
                    model.cursor = Some((pos.0, pos.1, cur_sz, cur_sz));
                    what =
                        alloc::format!("step {}: pointer resized to {}x{}", step, cur_sz, cur_sz);
                }
                17 => {
                    // The client DESTROYS the framebuffer the panel is scanning
                    // out and makes another one from the same buffer. In Linux
                    // `drm_framebuffer_remove` disables the CRTC that showed it
                    // (`atomic_remove_fb`), so the panel goes dark, pointer and
                    // all, until a whole frame goes up again -- which is what a
                    // client that remade its framebuffer does next, and what
                    // keeps a compositor from ever removing the fb on screen.
                    // (Only when it IS the fb on the CRTC: after a narrow present
                    // or a damage flush into the other buffer the CRTC holds a
                    // different one, and removing this one touches nothing.)
                    let on_crtc = drm::crtc_fb() == fbs[slot];
                    c.rmfb(fbs[slot])
                        .expect("RMFB the framebuffer on the panel");
                    if on_crtc {
                        assert_eq!(
                            screen.pixel(0, 0),
                            0,
                            "step {}: RMFB of the fb on the CRTC left the pipe lit",
                            step
                        );
                    }
                    fbs[slot] = c.addfb2(&bufs[slot]);
                    let f = frame;
                    paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                    set_crtc(&c, drm::SYNTH_CRTC_ID, fbs[slot], W, H);
                    drain_completions(&c);
                    model.present(frame);
                    panel_fb = fbs[slot];
                    model.cursor = if shown {
                        Some((pos.0, pos.1, cur_sz, cur_sz))
                    } else {
                        None
                    };
                    what = alloc::format!(
                        "step {}: the framebuffer on the panel was destroyed, the pipe \
                         went dark, and frame {} went up on the remade one",
                        step,
                        frame
                    );
                }
                _ => {
                    // DPMS off and on again with NOTHING presented after it,
                    // which a stray DPMS write really can do. The blank paints
                    // over the pointer too, so now the panel is one colour and
                    // nothing is drawn -- and whatever the pointer was covering
                    // is not under it any more. Anything the next move puts back
                    // there is a rectangle of the old desktop on a black screen.
                    drm::set_crtc_blanked(true);
                    drm::set_crtc_blanked(false);
                    // Black pixels are not a framebuffer's pixels, so the panel
                    // holds nothing now and the next damage box cannot be
                    // honoured on its own.
                    panel_fb = 0;
                    model.fill(screen.pixel(0, 0));
                    what = alloc::format!("step {}: blanked and unblanked, no frame", step);
                }
            }
            log.push(what.clone());
            if log.len() > 8 {
                log.remove(0);
            }
            model.check(
                &screen,
                &alloc::format!(
                    "{} [skip={} repair={} seed={:#x}] HISTORY {:?}",
                    what,
                    skip,
                    repair,
                    seed_in,
                    log
                ),
            );
        }
        assert!(frame < 256, "the frame number has to stay in one byte");
        // How many composites had to widen their window, for the caller to sum:
        // that only happens when a smaller-than-the-mode present catches the
        // pointer across the edge of the client's framebuffer, which no single
        // run of 300 steps is guaranteed to reach.
        assert!(
            by_box > 0,
            "no damage flush ever went up box by box, so the two-box steps say \
             nothing about the span the kernel picks"
        );
        let widened = drm::cursor_windows_widened_for_test();
        assert_eq!(
            skip && most_skipped > 0,
            skip,
            "with the band skip armed some present had to skip a band, or this \
             soak did not exercise it at all"
        );

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        drm::set_present_skip_enabled(false);
        drm::set_present_repair_enabled(false);
        c.rmfb(fb_small).expect("RMFB small");
        c.destroy_dumb(small.handle).expect("DESTROY_DUMB small");
        for fb in fbs {
            c.rmfb(fb).expect("RMFB");
        }
        c.destroy_dumb(cur3.handle)
            .expect("DESTROY_DUMB big cursor");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(cur2.handle)
            .expect("DESTROY_DUMB small cursor");
        c.rmfb(fb_big).expect("RMFB big");
        c.destroy_dumb(big.handle).expect("DESTROY_DUMB big");
        for b in bufs {
            c.destroy_dumb(b.handle).expect("DESTROY_DUMB");
        }
        widened
    }

    /// The bug Moebius sees: a pointer move pastes the frame labwc is STILL
    /// DRAWING into the frame that is on the screen.
    ///
    /// Our software-KMS present does not hold the client's buffer the way a real
    /// display engine does -- it copies it and completes the flip -- so the
    /// compositor is free to start the next frame in that same buffer. Nothing
    /// is wrong with that; it is what a released buffer is for. What is wrong is
    /// that `repaint_for_cursor` then goes back and READS that buffer to erase
    /// and redraw its two ~64x64 windows, and a renderer begins a frame by
    /// clearing to transparent black. So the window under the pointer gets a
    /// black rectangle pasted into a frame that has no black in it, and it
    /// appears exactly when something new is being drawn -- a menu, a popup --
    /// which is precisely when Moebius sees it and precisely why labwc and
    /// lunarbar are not at fault.
    ///
    /// Nothing here needs a GPU: the race is not a race at all from the
    /// kernel's side, because the two events are ordered by the ioctls.
    #[test]
    fn a_pointer_move_must_not_paste_the_frame_the_compositor_is_still_drawing() {
        const W: u32 = 120;
        const H: u32 = 64;
        // A padded, write-combining scanline: the UEFI shape, and the one where
        // the cursor patch widens its columns for real.
        let screen = kms_emu::attach_with(W, H, 128, true);
        let c = Client::open(0);
        let buf = c.create_dumb(W, H);
        paint(&buf, |x, y| desktop_px(0, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
        drain_completions(&c);

        let bmp = pointer_bitmap(16, 16);
        let cur = c.create_dumb(16, 16);
        {
            let px = map_dumb(&cur);
            for (i, v) in bmp.iter().enumerate() {
                px[i] = *v;
            }
        }
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 16, 16, 8, 8);

        let mut panel = Panel::new(W, H, 0, bmp, 16);
        panel.cursor = Some((8, 8, 16, 16));
        panel.check(&screen, "frame 0 up and the pointer composited on it");

        // labwc starts frame 1 in the buffer it just presented: a renderer opens
        // a frame by clearing the region it is about to draw to transparent
        // black. It has not presented anything, so the screen still shows frame
        // 0 -- and must keep showing it.
        let popup = (40u32, 16u32, 104u32, 48u32);
        {
            let px = map_dumb(&buf);
            let stride = (buf.pitch / 4) as usize;
            for y in popup.1..popup.3 {
                for x in popup.0..popup.2 {
                    px[y as usize * stride + x as usize] = 0x0000_0000;
                }
            }
        }
        panel.check(
            &screen,
            "the compositor cleared a popup box in its own buffer and \
             presented nothing",
        );

        // The pointer moves into that box, which is what a user does to open the
        // menu they are pointing at.
        move_cursor(&c, drm::SYNTH_CRTC_ID, 56, 24);
        panel.cursor = Some((56, 24, 16, 16));
        panel.check(&screen, "the pointer moved over the box being drawn");

        // A few pixels further, which is what a mouse actually does: the old and
        // new windows OVERLAP, so an erase that read the panel instead of what
        // was saved would capture the pointer it is erasing and blend the new one
        // over it, baking a trail in.
        move_cursor(&c, drm::SYNTH_CRTC_ID, 59, 27);
        panel.cursor = Some((59, 27, 16, 16));
        panel.check(&screen, "the pointer moved three pixels inside the box");

        // And out again, which is where the erase half used to read that buffer.
        move_cursor(&c, drm::SYNTH_CRTC_ID, 8, 8);
        panel.cursor = Some((8, 8, 16, 16));
        panel.check(&screen, "the pointer moved back out of the box");

        // The right edge, where the pointer's window runs off the visible width
        // into the scanline's off-screen padding. Those columns are written, so
        // they have to be read and put back too -- and the model above cannot see
        // them, which is exactly why a pointer left behind there would never be
        // noticed. Read them, visit, leave, and they must be as they were.
        let padding = |s: &kms_emu::Screen| {
            let mut v = alloc::vec::Vec::new();
            for y in 24..40 {
                for x in W..s.pitch_px() {
                    v.push(s.pixel(x, y));
                }
            }
            v
        };
        let before = padding(&screen);
        move_cursor(&c, drm::SYNTH_CRTC_ID, 112, 24);
        panel.cursor = Some((112, 24, 16, 16));
        panel.check(&screen, "the pointer at the right edge");
        move_cursor(&c, drm::SYNTH_CRTC_ID, 8, 8);
        panel.cursor = Some((8, 8, 16, 16));
        panel.check(&screen, "the pointer left the right edge");
        assert_eq!(
            padding(&screen),
            before,
            "the pointer stayed in the scanline padding: the columns its blit \
             wrote are not the columns the restore put back"
        );

        // Now labwc finishes frame 1 and presents it. The screen catches up, the
        // pointer is composited on top of it -- and what it is covering has to be
        // re-remembered from the frame that just went up, or the next move erases
        // with frame 0's pixels.
        paint(&buf, |x, y| desktop_px(1, x, y));
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xC0DE).expect("flip");
        drain_completions(&c);
        panel.present(1);
        panel.check(&screen, "frame 1 presented with the pointer on it");
        move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 40);
        panel.cursor = Some((40, 40, 16, 16));
        panel.check(&screen, "the pointer moved after frame 1 went up");

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The saved pixels describe the panel, so anything that repaints the panel
    /// behind the pointer's back has to make the kernel forget them.
    ///
    /// Blanking is the case with no second chance. It paints the panel black and
    /// does NOT touch `cursor.drawn`, and un-blanking is allowed to happen on a
    /// bare DPMS write with no frame behind it (see `set_crtc_blanked`). So the
    /// first pointer move after that would erase the pointer by putting back what
    /// it was covering before the screen went black -- one rectangle of the old
    /// desktop on a black screen, which is the same class of bug as the one the
    /// save exists to fix and would have been introduced by fixing it.
    /// llvmpipe hands over a frame it has not finished: its tiles arrive in
    /// worker threads while the kernel is already copying. That one frame is not
    /// the kernel's to fix -- there is no fence to wait on (`SYNCOBJ_TIMELINE`
    /// answers 0) and the buffer really did hold those pixels when they were
    /// read.
    ///
    /// What IS the kernel's is whether the black it copied SURVIVES. labwc
    /// presents a WHOLE FRAME every time -- measured on Moebius's boot, 1600
    /// presents and not one `DIRTYFB` -- so the very next present carries every
    /// pixel and nothing of the mix may be left on the panel. A rectangle that
    /// stays while a menu sits open is a rectangle nobody is overwriting, and
    /// that would be ours.
    #[test]
    fn black_copied_mid_frame_does_not_survive_the_next_present() {
        for skip in [false, true] {
            const W: u32 = 120;
            // Tall enough that the copy takes two bands, derived from the real
            // band size: the boundary between two bands is the only place a
            // test can get INSIDE a copy, so a height that hardcoded it would
            // stop testing anything the day the constant moves -- and the
            // assertion further down that the race landed is what would say so.
            let chunk = drm::blit_chunk_rows_for_test();
            let h = chunk + chunk / 4;
            // The box the client blacks out: inside the rows the SECOND band
            // copies, so the kernel reaches it after the hook has run.
            let (by, bh) = (chunk + 4, chunk / 4 - 8);
            let (bx, bw) = (40u32, 64u32);
            let screen = kms_emu::attach_with(W, h, 128, true);
            // The band-skipping present is what Moebius has been booting with,
            // and it is the one mechanism that could decide a band already
            // matches and never copy it again. Both ways, same assertion.
            drm::set_present_skip_enabled(skip);
            let c = Client::open(0);
            let buf = c.create_dumb(W, h);
            paint(&buf, |x, y| desktop_px(0, x, y));
            let fb = c.addfb2(&buf);
            set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, h);
            drain_completions(&c);

            // The copy goes band by band, asking the output for its framebuffer
            // once per band. Clearing a box of the second band's rows on the
            // FIRST ask is a tile that goes black after the source was sampled
            // and before the kernel reaches it: the copy carries it and neither
            // read of the probe sees anything else.
            let stride = (buf.pitch / 4) as usize;
            let px = map_dumb(&buf);
            // The hook wants `Send`, and a raw pointer is not, so the address
            // travels as an integer; the buffer it names is this test's own and
            // outlives the hook, which `clear_mid_blit` takes down below.
            let base = px.as_mut_ptr() as usize;
            kms_emu::on_blit_band(move |n| {
                if n != 0 {
                    return;
                }
                // SAFETY: the dumb buffer outlives this test and the hook runs
                // inside the present, on this thread, while nothing else writes
                // those rows.
                let p = unsafe {
                    core::slice::from_raw_parts_mut(base as *mut u32, stride * h as usize)
                };
                for y in by..by + bh {
                    for x in bx..bx + bw {
                        p[y as usize * stride + x as usize] = 0x0000_0000;
                    }
                }
            });
            paint(&buf, |x, y| desktop_px(1, x, y));
            c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xBEEF).expect("flip");
            drain_completions(&c);
            kms_emu::clear_mid_blit();

            // The race really happened, or the rest of this test proves nothing.
            assert_eq!(
                screen.pixel(bx + bw / 2, by + bh / 2),
                0x0000_0000,
                "skip={}: the hook did not land inside the copy, so this test is \
                 not about anything",
                skip
            );

            // Now the compositor presents a finished frame, whole, as labwc does
            // every time. Nothing of the mix may be left.
            paint(&buf, |x, y| desktop_px(2, x, y));
            c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");
            drain_completions(&c);

            for y in 0..h {
                for x in 0..W {
                    let got = screen.pixel(x, y);
                    assert_eq!(
                        got,
                        desktop_px(2, x, y),
                        "skip={}: ({}, {}) reads {:#010x} after a whole finished \
                         frame went up{}",
                        skip,
                        x,
                        y,
                        got,
                        if got == 0 {
                            " -- the black the kernel copied mid-frame is STILL \
                             THERE, so nothing overwrote it"
                        } else {
                            ""
                        }
                    );
                }
            }

            drm::set_present_skip_enabled(false);
            c.rmfb(fb).expect("RMFB");
            c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
        }
    }

    /// The escape hatch really takes the old path, and this is what the old path
    /// does.
    ///
    /// `drm.cursor_from_client` exists for one risk the fix cannot measure from
    /// here: reading the panel puts an aperture read on the pointer path, and
    /// nobody has measured how slow a read of an NVIDIA BAR1 window is. If that
    /// turns out to drag the pointer, this flag makes the machine usable again.
    /// Its price is exactly the defect, so the test says so: the same steps as
    /// `a_pointer_move_must_not_paste_the_frame_the_compositor_is_still_drawing`
    /// put the black rectangle back. A flag whose only honest test is "the bug
    /// returns" is a flag nobody should leave on, which is the point.
    #[test]
    fn the_escape_hatch_reads_the_clients_framebuffer_again_black_rectangles_and_all() {
        const W: u32 = 120;
        const H: u32 = 64;
        let screen = kms_emu::attach_with(W, H, 128, true);
        drm::set_cursor_from_client(true);
        let c = Client::open(0);
        let buf = c.create_dumb(W, H);
        paint(&buf, |x, y| desktop_px(0, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
        drain_completions(&c);

        let bmp = pointer_bitmap(16, 16);
        let cur = c.create_dumb(16, 16);
        {
            let px = map_dumb(&cur);
            for (i, v) in bmp.iter().enumerate() {
                px[i] = *v;
            }
        }
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 16, 16, 8, 8);

        // The compositor starts the next frame in the buffer it presented.
        {
            let px = map_dumb(&buf);
            let stride = (buf.pitch / 4) as usize;
            for y in 16..48usize {
                for x in 40..104usize {
                    px[y * stride + x] = 0x0000_0000;
                }
            }
        }
        move_cursor(&c, drm::SYNTH_CRTC_ID, 56, 24);

        // The pointer's window is columns 48..72 -- widened to the
        // write-combining boundary -- and every pixel of it the pointer does not
        // cover now holds the black the compositor had not finished drawing over.
        let mut black = 0;
        for y in 24..40u32 {
            for x in 48..72u32 {
                if screen.pixel(x, y) == 0 {
                    black += 1;
                }
            }
        }
        assert!(
            black > 0,
            "the flag did not take the old path: nothing pasted the half-drawn \
             frame, so there is nothing for the hatch to be an escape from"
        );

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A panel that will not hand its pixels back keeps the older route, and that
    /// route still has to draw and erase a pointer.
    ///
    /// The fix above reads the panel, which a panel that is not ARGB8888 cannot
    /// do, so the read-the-client's-framebuffer path is still there for it. No
    /// machine this kernel meets has such a panel -- a UEFI GOP, virtio-gpu and an
    /// NVIDIA BAR1 aperture are all 32-bit -- so nothing else would ever run it,
    /// and an untested fallback is one that stops working without anyone finding
    /// out. This is the only test that takes it, and it is the reason the
    /// emulated panel can be told to refuse.
    #[test]
    fn a_panel_that_cannot_be_read_back_still_gets_its_pointer_drawn_and_erased() {
        let screen = kms_emu::attach(64, 16);
        screen.refuse_read_back();
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, |_, _| 0xFF00_1111);
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

        let cur = c.create_dumb(8, 8);
        {
            let px = map_dumb(&cur);
            for p in px.iter_mut().take(64) {
                *p = 0xFF00_00FF;
            }
        }
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);
        move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 6);

        for y in 0..16 {
            for x in 0..64 {
                let want = if (40..48).contains(&x) && (6..14).contains(&y) {
                    0xFF00_00FF
                } else {
                    0xFF00_1111
                };
                assert_eq!(
                    screen.pixel(x, y),
                    want,
                    "({}, {}) after a move on a panel that refuses read-back",
                    x,
                    y
                );
            }
        }

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    #[test]
    fn blanking_the_panel_makes_the_kernel_forget_what_the_pointer_was_covering() {
        const W: u32 = 120;
        const H: u32 = 64;
        let screen = kms_emu::attach_with(W, H, 128, true);
        let c = Client::open(0);
        let buf = c.create_dumb(W, H);
        paint(&buf, |x, y| desktop_px(3, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
        drain_completions(&c);

        let bmp = pointer_bitmap(16, 16);
        let cur = c.create_dumb(16, 16);
        {
            let px = map_dumb(&cur);
            for (i, v) in bmp.iter().enumerate() {
                px[i] = *v;
            }
        }
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 16, 16, 8, 8);

        drm::set_crtc_blanked(true);
        // A DPMS write on its own, with nothing presented after it.
        drm::set_crtc_blanked(false);
        move_cursor(&c, drm::SYNTH_CRTC_ID, 60, 30);

        // Whatever colour the blank left, the panel is one colour: the only thing
        // allowed on it is the pointer. A restored save would be a rectangle of
        // the desktop, wherever the pointer had been.
        let blank = screen.pixel(0, 0);
        for y in 0..H {
            for x in 0..W {
                if (60..76).contains(&x) && (30..46).contains(&y) {
                    continue;
                }
                assert_eq!(
                    screen.pixel(x, y),
                    blank,
                    "({}, {}) is not the blanked panel: the pointer put back what \
                     it was covering before the screen went black",
                    x,
                    y
                );
            }
        }

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// `drm_mode_rmfb` removes the framebuffer and, through
    /// `drm_framebuffer_remove`, disables the CRTC that was showing it; a
    /// framebuffer that is not on the CRTC goes without touching it; and
    /// `CLOSEFB` only drops the object, the scanout stays. Here RMFB never
    /// turned anything off: the panel kept a frame the client had freed and
    /// the CRTC read as on.
    #[test]
    fn removing_the_framebuffer_on_the_crtc_turns_it_off_and_closing_it_does_not() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);
        let fb_of = |base: u32| {
            let buf = c.create_dumb(64, 16);
            paint(&buf, |x, y| tag(base, x, y));
            c.addfb2(&buf)
        };
        let fb_a = fb_of(0x0077_0000);
        let fb_b = fb_of(0x0088_0000);
        let fb_c = fb_of(0x0099_0000);

        c.page_flip(drm::SYNTH_CRTC_ID, fb_a, 0xF00D).expect("flip");
        assert_eq!(screen.pixel(3, 2), tag(0x0077_0000, 3, 2));

        // A framebuffer that is not on the CRTC: nothing changes on screen.
        let mut id = fb_b;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_RMFB, &mut id), Ok(0));
        assert!(
            !drm::crtc_blanked(),
            "removing another fb turned the CRTC off"
        );
        assert_eq!(screen.pixel(3, 2), tag(0x0077_0000, 3, 2));
        assert_eq!(drm::crtc_fb(), fb_a);

        // The one being scanned out: the CRTC goes off with it.
        let mut id = fb_a;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_RMFB, &mut id), Ok(0));
        assert!(drm::crtc_blanked(), "the CRTC stayed on without its fb");
        assert_eq!(screen.pixel(3, 2), 0, "the panel kept the freed frame");
        assert_eq!(drm::crtc_fb(), 0);

        // CLOSEFB: the object goes, the scanout stays.
        c.page_flip(drm::SYNTH_CRTC_ID, fb_c, 0xF00E).expect("flip");
        assert!(!drm::crtc_blanked());
        let mut id = fb_c;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_CLOSEFB, &mut id), Ok(0));
        assert!(!drm::crtc_blanked(), "CLOSEFB turned the CRTC off");
        assert_eq!(screen.pixel(3, 2), tag(0x0099_0000, 3, 2));
        let mut id = fb_c;
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_RMFB, &mut id),
            Err(FsError::EntryNotFound),
            "a closed fb is gone"
        );
    }

    /// `drm_mode_create_dumb` refuses a width, height or bpp of 0 and a bpp
    /// past `U32_MAX - 8` (EINVAL), and sizes the buffer from the bpp asked
    /// for: `DIV_ROUND_UP(bpp, 8)` bytes per pixel. Here every bpp below 32
    /// became 32 -- a bpp of 0 got a 32-bit buffer, and a 16-bit request
    /// was sized and reported as 32-bit.
    #[test]
    fn a_dumb_buffer_is_sized_by_the_bpp_asked_for_and_a_zero_is_refused() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        let einval = Err(FsError::InvalidParam);
        let create = |width: u32, height: u32, bpp: u32| {
            let mut req = DrmModeCreateDumb {
                height,
                width,
                bpp,
                flags: 0,
                handle: 0,
                pitch: 0,
                size: 0,
            };
            c.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut req)
                .map(|_| (req.pitch, req.size, req.handle))
        };
        assert_eq!(create(64, 4, 0), einval, "a bpp of 0 got a buffer");
        assert_eq!(create(0, 4, 32), einval);
        assert_eq!(create(64, 0, 32), einval);
        assert_eq!(create(64, 4, u32::MAX - 7), einval);

        let mut handles = alloc::vec::Vec::new();
        for (bpp, pitch) in [(32, 256), (16, 128), (8, 64), (24, 192), (12, 128), (1, 64)] {
            let (p, size, handle) = create(64, 4, bpp).expect("CREATE_DUMB");
            assert_eq!(p, pitch, "pitch for bpp {}", bpp);
            assert_eq!(size, pitch as u64 * 4, "size for bpp {}", bpp);
            handles.push(handle);
        }
        for mut handle in handles {
            c.ioctl(DRM_IOCTL_MODE_DESTROY_DUMB, &mut handle)
                .expect("DESTROY_DUMB");
        }
    }
}

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
mod hw_kms_tests {
    use super::gl_client_sequence_tests::{blank_card_res, parse_events, Client, FLIP_COMPLETE};
    use super::*;
    use crate::fs::devfs::kms_emu::{self, EmuGpu, UNTOUCHED};
    use alloc::vec::Vec;
    use kernel_hal::mem::phys_to_virt;

    /// Paint a dumb buffer through its physical backing (see
    /// `kms_scanout_tests::paint`, which this deliberately mirrors).
    fn paint(buf: &DrmModeCreateDumb, value: u32) {
        let (pa, size) =
            drm::resolve_gem_backing(buf.handle).expect("a dumb buffer must have backing");
        let va = phys_to_virt(pa as usize);
        // SAFETY: `size` bytes of contiguous physical memory owned by this
        // buffer, identity-mapped into the kernel window at `va`.
        let px = unsafe { core::slice::from_raw_parts_mut(va as *mut u32, size / 4) };
        for p in px.iter_mut() {
            *p = value;
        }
    }

    fn set_crtc(c: &Client, crtc_id: u32, fb_id: u32, w: u32, h: u32) {
        let mut req = DrmModeGetCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id,
            fb_id,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode: make_modeinfo(w, h),
        };
        c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req).expect("SETCRTC");
    }

    fn get_crtc_fb(c: &Client, crtc_id: u32) -> u32 {
        let mut crtc = DrmModeGetCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id,
            fb_id: 0,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: [0; 68],
        };
        c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
        crtc.fb_id
    }

    /// `struct drm_mode_cursor`, 28 bytes.
    #[repr(C)]
    struct ModeCursor {
        flags: u32,
        crtc_id: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        handle: u32,
    }

    fn set_cursor(c: &Client, crtc_id: u32, handle: u32, w: u32, h: u32, x: i32, y: i32) {
        let mut cur = ModeCursor {
            flags: 0x01 | 0x02, // BO | MOVE
            crtc_id,
            x,
            y,
            width: w,
            height: h,
            handle,
        };
        c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur).expect("CURSOR");
    }

    /// `drmWaitVBlank` with a relative target of 0, which asks for the current
    /// sequence and must not block.
    fn wait_vblank(c: &Client) {
        const DRM_VBLANK_RELATIVE: u32 = 0x1;
        let mut req = DrmWaitVblank {
            typ: DRM_VBLANK_RELATIVE,
            sequence: 0,
            val1: 0,
            val2: 0,
        };
        c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req)
            .expect("WAIT_VBLANK");
    }

    /// Read the CRTC and connector ids `drmModeGetResources` would hand a
    /// compositor, in the two passes libdrm makes.
    fn topology(c: &Client) -> (Vec<u32>, Vec<u32>) {
        let mut probe = blank_card_res();
        c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut probe)
            .expect("GETRESOURCES");
        let mut crtcs = alloc::vec![0u32; probe.count_crtcs as usize];
        let mut conns = alloc::vec![0u32; probe.count_connectors as usize];
        let mut fill = blank_card_res();
        fill.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
        fill.connector_id_ptr = conns.as_mut_ptr() as u64;
        fill.count_crtcs = probe.count_crtcs;
        fill.count_connectors = probe.count_connectors;
        c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut fill)
            .expect("GETRESOURCES fill");
        (crtcs, conns)
    }

    /// `DRM_CLIENT_CAP_UNIVERSAL_PLANES` on or off for this client.
    fn universal_planes(c: &Client, on: bool) {
        let mut cap: [u64; 2] = [DRM_CLIENT_CAP_UNIVERSAL_PLANES, on as u64];
        c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
            .expect("SET_CLIENT_CAP UNIVERSAL_PLANES");
    }

    fn planes(c: &Client) -> Vec<u32> {
        let mut probe = DrmModeGetPlaneRes {
            plane_id_ptr: 0,
            count_planes: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut probe)
            .expect("GETPLANERESOURCES");
        let mut ids = alloc::vec![0u32; probe.count_planes as usize];
        let mut fill = DrmModeGetPlaneRes {
            plane_id_ptr: ids.as_mut_ptr() as u64,
            count_planes: probe.count_planes,
        };
        c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut fill)
            .expect("GETPLANERESOURCES fill");
        ids
    }

    /// The frame goes to the driver and the CPU never touches the scanout. If
    /// the software blit ran too, every present would pay for a full-frame copy
    /// over PCIe that the display engine had already made unnecessary -- which
    /// is the whole reason the hardware path exists.
    #[test]
    fn a_driver_that_owns_scanout_gets_the_frame_and_the_cpu_does_not_blit() {
        let screen = kms_emu::attach(64, 16);
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, 0x0000_ABCD);
        let fb = c.addfb2(&buf);

        // ADDFB2 asked the driver for its own framebuffer object, with the
        // geometry the client asked for.
        let created = gpu.created_fbs();
        assert_eq!(created.len(), 1, "the driver was not asked for an fb");
        assert_eq!(created[0].gem_handle, buf.handle);
        assert_eq!((created[0].width, created[0].height), (64, 16));
        assert_eq!(created[0].pitch, buf.pitch);
        let driver_fb = created[0].driver_fb_id;
        assert_ne!(driver_fb, fb, "the two namespaces must not coincide");

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x1234).expect("flip");

        // The driver was flipped to ITS OWN id, not the core's.
        assert_eq!(gpu.flips(), alloc::vec![driver_fb]);
        // And nothing was copied into the framebuffer.
        assert!(
            (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED)),
            "the software blit ran even though the driver took the flip"
        );
        // The client still gets its completion.
        drm::flush_pending_flip_completions();
        let mut b = [0u8; 32];
        assert_eq!(c.read_events(&mut b).expect("completion"), 32);
        let ev = parse_events(&b);
        assert_eq!(ev[0].ev_type, FLIP_COMPLETE);
        assert_eq!(ev[0].user_data, 0x1234);

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A driver that refuses the flip must not leave the panel dark: the core
    /// falls back to the software blit. This is the failure mode the fallback
    /// was written for -- a display engine that will not take the surface -- and
    /// it is unreachable without a driver that can say no.
    #[test]
    fn a_driver_that_refuses_the_flip_falls_back_to_the_software_blit() {
        let screen = kms_emu::attach(64, 16);
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
        gpu.refuse_flips();
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, 0x0000_BEEF);
        let fb = c.addfb2(&buf);

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");

        assert_eq!(
            gpu.flips().len(),
            1,
            "the driver was still offered the flip"
        );
        assert!(
            (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_BEEF)),
            "the refused flip left the screen unwritten"
        );

        drm::flush_pending_flip_completions();
        let mut sink = [0u8; 32];
        let _ = c.read_events(&mut sink);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The pointer is put back on top of a frame the driver flipped. A driver
    /// flip short-circuits the only place the software cursor is composited, so
    /// every accepted flip used to land a frame with no pointer in it -- and a
    /// cursor move on this path stands down entirely, so nothing else would ever
    /// draw it again.
    #[test]
    fn the_pointer_is_put_back_on_top_of_a_frame_the_driver_flipped() {
        let screen = kms_emu::attach(64, 16);
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, 0x0000_1111);
        let fb = c.addfb2(&buf);
        // A first flip so the CRTC names this framebuffer.
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");
        drm::flush_pending_flip_completions();
        let mut sink = [0u8; 32];
        let _ = c.read_events(&mut sink);

        let cur = c.create_dumb(8, 8);
        paint(&cur, 0xFF00_00FF);
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

        // Setting the pointer draws nothing on this path: the software repaint
        // stands down, because on real hardware the display engine is scanning
        // out the client's own surface and a CPU composite into the boot
        // framebuffer would be invisible.
        assert!(
            (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED)),
            "a cursor move repainted while a driver owns scanout"
        );

        screen.repaint(UNTOUCHED);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 2).expect("flip");

        // Now the pointer is there, over the pixels of the frame it belongs to.
        // The patch the composite writes is the pointer's rectangle widened to
        // whole 16-pixel write-combining lines -- [0, 16) here, for a pointer at
        // x = 4 -- and it carries the frame's own pixels in the columns the
        // pointer does not cover, which is what makes it a composite rather than
        // a stamp. Everything outside the patch stays as the driver left it.
        for y in 0..16 {
            for x in 0..64 {
                let in_patch = x < 16 && (2..10).contains(&y);
                let want = if (4..12).contains(&x) && (2..10).contains(&y) {
                    0xFF00_00FF
                } else if in_patch {
                    0x0000_1111
                } else {
                    UNTOUCHED
                };
                assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
            }
        }
        assert_eq!(gpu.flips().len(), 2);

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        drm::flush_pending_flip_completions();
        let _ = c.read_events(&mut sink);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// Only a hardware-KMS driver's topology is exposed once one exists. Mixing
    /// a non-KMS driver's CRTCs in alongside produces a topology with two CRTCs
    /// sharing one synthetic encoder, and wlroots answers that with "Failed to
    /// create DRM backend" -- no desktop at all.
    #[test]
    fn a_non_kms_drivers_topology_is_not_mixed_in_with_a_kms_one() {
        let screen = kms_emu::attach(64, 16);
        let _virtio = screen.attach_gpu(EmuGpu::new("emu-virtio").with_ids(50, 51, 52));
        let _nvidia = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
        let c = Client::open(0);
        universal_planes(&c, true);

        let (crtcs, conns) = topology(&c);
        assert_eq!(crtcs, alloc::vec![60], "the non-KMS CRTC was exposed too");
        assert_eq!(conns, alloc::vec![61]);
        assert_eq!(planes(&c), alloc::vec![62]);
    }

    /// `drm_mode_getplane_res` lists only overlay planes until the client
    /// sets `DRM_CLIENT_CAP_UNIVERSAL_PLANES` (or ATOMIC, which implies it):
    /// a legacy client was written when the primary and the cursor were not
    /// planes, and would drive the scanout plane as an overlay. The cap was
    /// accepted and forgotten, and the list was the same for everyone. The
    /// flag is per open file, and the list is filled as far as the caller's
    /// buffer goes, with the full count reported.
    #[test]
    fn only_a_client_that_asked_for_universal_planes_is_told_about_the_primaries() {
        let screen = kms_emu::attach(64, 16);
        let _first = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-0").with_ids(60, 61, 62));
        let _second = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-1").with_ids(70, 71, 72));
        let c = Client::open(0);

        assert_eq!(
            planes(&c),
            Vec::<u32>::new(),
            "a legacy client was handed a primary plane"
        );
        universal_planes(&c, true);
        let mut all = planes(&c);
        all.sort_unstable();
        assert_eq!(all, alloc::vec![62, 72]);
        universal_planes(&c, false);
        assert_eq!(planes(&c), Vec::<u32>::new(), "the cap can be taken back");

        // Per file: what one client asked for does not change another's list.
        universal_planes(&c, true);
        let legacy = Client::open(0);
        assert_eq!(planes(&legacy), Vec::<u32>::new());

        // Room for one of the two: that one is filled, and the count says two.
        let mut one = [0u32; 1];
        let mut fill = DrmModeGetPlaneRes {
            plane_id_ptr: one.as_mut_ptr() as u64,
            count_planes: 1,
        };
        c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut fill)
            .expect("GETPLANERESOURCES with room for one");
        assert_eq!(fill.count_planes, 2);
        assert!(
            all.contains(&one[0]),
            "the slot the caller had was left empty"
        );
    }

    /// `drm_setclientcap` stores the ATOMIC value in `universal_planes` too:
    /// an atomic client sees the primary plane without asking for universal
    /// planes by name, and giving atomic back takes the planes with it.
    #[test]
    fn atomic_carries_universal_planes_with_it() {
        let (_screen, c) = super::out_fence_tests::atomic_client(64, 16);
        assert_eq!(planes(&c), alloc::vec![drm::SYNTH_PLANE_ID]);
        let mut cap: [u64; 2] = [DRM_CLIENT_CAP_ATOMIC, 0];
        c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
            .expect("SET_CLIENT_CAP ATOMIC off");
        assert_eq!(
            planes(&c),
            Vec::<u32>::new(),
            "atomic off, planes still listed"
        );
    }

    /// Two GPUs of the same model return the SAME synthetic ids, and a topology
    /// that repeats an id makes wlroots create two outputs with identical
    /// resource ids -- which ends in 0x0 dumb-buffer allocations and EINVAL.
    /// This is the dual-card case, so it is the one that has to hold.
    #[test]
    fn two_gpus_reporting_the_same_ids_are_each_reported_once() {
        let screen = kms_emu::attach(64, 16);
        let _first = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-0").with_ids(60, 61, 62));
        let _second = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-1").with_ids(60, 61, 62));
        let c = Client::open(0);

        let (crtcs, conns) = topology(&c);
        assert_eq!(
            crtcs,
            alloc::vec![60],
            "a duplicate CRTC id reached userspace"
        );
        assert_eq!(
            conns,
            alloc::vec![61],
            "a duplicate connector id reached userspace"
        );
    }

    /// `GETCRTC` reports the framebuffer id in the DRM CORE's namespace, even
    /// though the driver answers with its own. Handing a client a driver-private
    /// id would make its next `RMFB` or `GETFB` name a framebuffer that does not
    /// exist -- and the ids look alike, so nothing would say so.
    #[test]
    fn getcrtc_reports_the_core_framebuffer_id_not_the_drivers() {
        let screen = kms_emu::attach(32, 8);
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
        let c = Client::open(0);
        let buf = c.create_dumb(32, 8);
        paint(&buf, 0x0000_2222);
        let fb = c.addfb2(&buf);
        let driver_fb = gpu.created_fbs()[0].driver_fb_id;

        set_crtc(&c, 60, fb, 32, 8);

        let reported = get_crtc_fb(&c, 60);
        assert_eq!(reported, fb, "GETCRTC did not report the core's fb id");
        assert_ne!(reported, driver_fb, "the driver's private id leaked out");

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// `WAIT_VBLANK` reaches a driver that really has hardware vblank.
    #[test]
    fn wait_vblank_reaches_a_driver_that_owns_scanout() {
        let screen = kms_emu::attach(64, 16);
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
        let c = Client::open(0);

        wait_vblank(&c);

        assert_eq!(
            gpu.vblank_waits(),
            1,
            "the driver was not asked for a vblank"
        );
    }

    /// And never reaches one without it, in either configuration that can
    /// arise. A driver with no hardware KMS implements `wait_vblank` as a busy
    /// 16.7 ms spin, so calling it per `WAIT_VBLANK` starves a cooperative async
    /// runtime and the whole system looks frozen; the synthetic timer paces the
    /// software path instead. Two guards stand between the ioctl and that spin,
    /// and they cover different cases: with an output attached the software-KMS
    /// check stops it, and with no output at all (a VirtIO-only guest) only the
    /// driver's own `has_hardware_kms()` does.
    #[test]
    fn wait_vblank_never_reaches_a_driver_that_does_not_own_scanout() {
        // No output: `software_kms_active()` is false, so the per-driver check
        // is the only thing left.
        {
            let headless = kms_emu::headless();
            let gpu = headless.attach_gpu(EmuGpu::new("emu-virtio"));
            wait_vblank(&Client::open(0));
            assert_eq!(
                gpu.vblank_waits(),
                0,
                "a 16.7 ms driver spin was entered with no output attached"
            );
        }
        // With an output, the software path owns the pacing.
        let screen = kms_emu::attach(64, 16);
        let gpu = screen.attach_gpu(EmuGpu::new("emu-virtio"));
        wait_vblank(&Client::open(0));
        assert_eq!(
            gpu.vblank_waits(),
            0,
            "a 16.7 ms driver spin was entered while software KMS drives the output"
        );
    }

    /// Under pure software KMS the driver is NOT asked to make a framebuffer of
    /// its own. It has no destroy path here, so one per ADDFB2 is a leak for
    /// every frame a compositor ever allocates -- and nothing would ever use it,
    /// because the software path does the copy itself.
    #[test]
    fn a_driver_that_does_not_own_scanout_is_not_asked_for_framebuffers() {
        let screen = kms_emu::attach(64, 16);
        let gpu = screen.attach_gpu(EmuGpu::new("emu-virtio"));
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, 0x0000_3333);
        let fb = c.addfb2(&buf);

        assert!(
            gpu.created_fbs().is_empty(),
            "a driver framebuffer was created with nothing to use it"
        );
        // And the software path still put the frame on the screen.
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");
        assert!(
            gpu.flips().is_empty(),
            "a non-KMS driver was offered a flip"
        );
        assert_eq!(screen.pixel(0, 0), 0x0000_3333);

        drm::flush_pending_flip_completions();
        let mut sink = [0u8; 32];
        let _ = c.read_events(&mut sink);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// `DRM_MODE_CURSOR` with the MOVE bit alone, which is what a compositor
    /// sends on every pointer motion.
    fn move_cursor(c: &Client, crtc_id: u32, x: i32, y: i32) {
        let mut cur = ModeCursor {
            flags: 0x02, // MOVE
            crtc_id,
            x,
            y,
            width: 0,
            height: 0,
            handle: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur)
            .expect("CURSOR MOVE");
    }

    /// Paint a dumb buffer so every word says which word it is. The cursor
    /// bitmap is read tightly packed at `w * h` words while the buffer's own
    /// pitch is rounded up to 64 bytes, so a bitmap taken by stride instead of
    /// by row would hand the plane the wrong words -- and a flat fill could not
    /// tell the two apart.
    fn paint_indexed(buf: &DrmModeCreateDumb, base: u32) {
        let (pa, size) =
            drm::resolve_gem_backing(buf.handle).expect("a dumb buffer must have backing");
        let va = phys_to_virt(pa as usize);
        // SAFETY: `size` bytes of contiguous physical memory owned by this
        // buffer, identity-mapped into the kernel window at `va`.
        let px = unsafe { core::slice::from_raw_parts_mut(va as *mut u32, size / 4) };
        for (i, p) in px.iter_mut().enumerate() {
            *p = base | i as u32;
        }
    }

    /// Wipe the scanout, flip `fb`, and report the pixel under the pointer's
    /// top-left corner. `UNTOUCHED` means the CPU composited nothing there,
    /// which is what must happen once the display engine owns the pointer -- a
    /// hardware plane and a software composite both drawing leaves two pointers
    /// on screen, the CPU one a frame behind.
    fn pointer_pixel_after_a_flip(screen: &kms_emu::Screen, c: &Client, fb: u32, seq: u64) -> u32 {
        screen.repaint(UNTOUCHED);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, seq).expect("flip");
        drm::flush_pending_flip_completions();
        let mut sink = [0u8; 32];
        let _ = c.read_events(&mut sink);
        screen.pixel(4, 2)
    }

    /// With `nvidia.hwcursor` on and a plane that takes the image, the display
    /// engine owns the pointer: it gets the bitmap and every motion, and the CPU
    /// never composites again. That last half is the point -- the hardware plane
    /// and the software compositor drawing the same pointer leaves two of them
    /// on screen, the CPU one smearing a frame behind.
    #[test]
    fn the_display_engine_cursor_plane_takes_the_pointer_when_the_driver_accepts_it() {
        let screen = kms_emu::attach(64, 16);
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_cursor_plane());
        drm::set_hw_cursor_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, 0x0000_1111);
        let fb = c.addfb2(&buf);
        // SETCRTC, not just a flip: it is what makes the CRTC name this
        // framebuffer, and the software compositor has nothing to repaint from
        // until it does. Without it the stand-downs below would hold for the
        // wrong reason.
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
        screen.repaint(UNTOUCHED);

        // 8 wide by 4 high, deliberately not square and not a multiple of the
        // buffer's own 16-pixel stride: the bitmap is 32 consecutive words, so a
        // plane fed by stride, or given the dimensions the other way round, gets
        // caught here rather than drawing a garbled pointer on real hardware.
        let cur = c.create_dumb(8, 4);
        paint_indexed(&cur, 0xFF00_0000);
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 4, 4, 2);

        let images = gpu.cursor_images();
        assert_eq!(images.len(), 1, "the plane was not offered the image");
        assert_eq!((images[0].width, images[0].height), (8, 4));
        let want: Vec<u32> = (0..32).map(|i| 0xFF00_0000 | i).collect();
        assert_eq!(images[0].argb, want, "the plane got the wrong words");

        // And it was landed on the pointer's position. Two moves: taking the
        // image puts the plane where the pointer already is (without that, a
        // client that sets an image and never moves again leaves the pointer
        // wherever the plane happened to be), then the MOVE half of the same
        // ioctl carries it to (4, 2).
        assert_eq!(gpu.cursor_moves(), alloc::vec![(0, 0), (4, 2)]);

        // Nothing was drawn by the CPU, either by the cursor ioctl...
        assert!(
            (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED)),
            "the CPU composited a pointer the display engine owns"
        );
        // ...or by the frame that follows it.
        assert_eq!(
            pointer_pixel_after_a_flip(&screen, &c, fb, 2),
            UNTOUCHED,
            "a driver flip put a second, software pointer on the screen"
        );
        // A motion is one register write in the driver and nothing else.
        screen.repaint(UNTOUCHED);
        move_cursor(&c, drm::SYNTH_CRTC_ID, 9, 3);
        assert_eq!(gpu.cursor_moves().last(), Some(&(9, 3)));
        assert!(
            (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED)),
            "a pointer motion repainted while the display engine owns the plane"
        );

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// A driver whose plane will not take the image must leave the software
    /// pointer in charge. The upload goes through the RM gate and can fail on a
    /// real card (no cursor surface on the head, a format the engine refuses);
    /// standing down on the CPU side anyway is a desktop with no pointer at all.
    #[test]
    fn a_driver_that_refuses_the_cursor_image_leaves_the_software_pointer_in_charge() {
        let screen = kms_emu::attach(64, 16);
        // Hardware KMS, but no cursor plane to give.
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
        drm::set_hw_cursor_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, 0x0000_1111);
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

        let cur = c.create_dumb(8, 8);
        paint(&cur, 0xFF00_00FF);
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

        // The image was offered and refused, so the plane was never moved.
        assert_eq!(gpu.cursor_images().len(), 1, "the plane was not offered");
        assert!(
            gpu.cursor_moves().is_empty(),
            "a refused plane was moved anyway"
        );
        // And the CPU is drawing the pointer again.
        assert_eq!(
            pointer_pixel_after_a_flip(&screen, &c, fb, 2),
            0xFF00_00FF,
            "the pointer is drawn by nobody"
        );

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// Hiding the pointer switches the hardware plane off. The software path
    /// hides by simply not drawing, so a plane that is never told stays
    /// composited by the display engine -- the pointer sticks on screen after
    /// the compositor has hidden it (fullscreen video, a game grabbing it) and
    /// nothing userspace does can take it away.
    #[test]
    fn hiding_the_pointer_switches_the_hardware_plane_off() {
        let screen = kms_emu::attach(64, 16);
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_cursor_plane());
        drm::set_hw_cursor_enabled(true);
        let c = Client::open(0);

        let cur = c.create_dumb(8, 8);
        paint(&cur, 0xFF00_00FF);
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);
        assert_eq!(gpu.cursor_images().len(), 1);
        assert_eq!(gpu.cursor_hides(), 0, "the plane was hidden while in use");

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        assert_eq!(gpu.cursor_hides(), 1, "the plane was left switched on");

        // And a second hide does not go back to the driver: there is nothing on
        // the plane to switch off, and this runs per hidden frame.
        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        assert_eq!(gpu.cursor_hides(), 1);

        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    }

    /// A cursor plane without hardware KMS: the display engine composites the
    /// pointer while the CPU still blits the frame. That is `nvidia.hwcursor`
    /// without `nvidia.hwflip`, the configuration the flag was added for, and it
    /// is the only one where the software compositor is running AND has to leave
    /// the pointer alone -- draw it anyway and there are two pointers on screen,
    /// the CPU one lagging a frame behind the plane.
    #[test]
    fn the_cursor_plane_can_own_the_pointer_while_the_cpu_still_blits_the_frame() {
        let screen = kms_emu::attach(64, 16);
        let gpu = screen.attach_gpu(EmuGpu::new("emu-gpu").with_cursor_plane());
        drm::set_hw_cursor_enabled(true);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, 0x0000_1111);
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

        // The CPU really is driving the scanout here.
        assert!(
            (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_1111)),
            "the software blit did not run"
        );

        let cur = c.create_dumb(8, 8);
        paint(&cur, 0xFF00_00FF);
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

        assert_eq!(gpu.cursor_images().len(), 1, "the plane was not offered");
        assert!(
            (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_1111)),
            "the CPU composited a pointer the plane already owns"
        );

        // A motion goes to the plane and repaints nothing.
        move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 6);
        assert_eq!(gpu.cursor_moves().last(), Some(&(40, 6)));
        assert!(
            (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_1111)),
            "a pointer motion repainted while the plane owns the pointer"
        );

        // And the frame after it is still blitted by the CPU, pointer-free.
        screen.repaint(UNTOUCHED);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");
        drm::flush_pending_flip_completions();
        let mut sink = [0u8; 32];
        let _ = c.read_events(&mut sink);
        assert!(
            (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_1111)),
            "the frame was not blitted, or carried a software pointer"
        );

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The plane is not offered the image unless the flag is on. The
    /// display-engine cursor is opt-in (`nvidia.hwcursor`) precisely because it
    /// is the half of the bring-up that is not trusted yet, so a driver that
    /// implements it must not start owning the pointer on a default boot.
    #[test]
    fn the_plane_is_not_offered_the_image_unless_the_flag_is_on() {
        let screen = kms_emu::attach(64, 16);
        // A plane that WOULD take it, which is what makes the flag the only
        // thing standing between this boot and the hardware path.
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_cursor_plane());
        drm::set_hw_cursor_enabled(false);
        let c = Client::open(0);
        let buf = c.create_dumb(64, 16);
        paint(&buf, 0x0000_1111);
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

        let cur = c.create_dumb(8, 8);
        paint(&cur, 0xFF00_00FF);
        set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

        assert!(
            gpu.cursor_images().is_empty(),
            "the plane was offered the image with the flag off"
        );
        assert!(gpu.cursor_moves().is_empty());
        assert_eq!(
            pointer_pixel_after_a_flip(&screen, &c, fb, 2),
            0xFF00_00FF,
            "the pointer is drawn by nobody"
        );

        set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
        assert_eq!(
            gpu.cursor_hides(),
            0,
            "a plane that never took the pointer was told to hide it"
        );
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }
    // ---- the EDID a driver reports, and whether the core serves it ----

    /// A whole block with a correct header and checksum, as a monitor sends one.
    fn real_edid() -> [u8; 128] {
        let mut b = [0u8; 128];
        b[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        b[18] = 1;
        b[19] = 4;
        b[21] = 60;
        b[22] = 34;
        let sum = b[..127].iter().fold(0u8, |s, x| s.wrapping_add(*x));
        b[127] = sum.wrapping_neg();
        b
    }

    /// The defect this pair of tests exists for. `NvidiaGpu::get_connector_edid`
    /// used to build a block out of the 32 bytes the RM gives it by padding the
    /// rest with zeros, and the DRM core served whatever a driver reported after
    /// a length check alone. Those bytes cannot pass a checksum, so wlroots --
    /// through libdisplay-info, which checks -- threw the whole block away and
    /// the output lost the make, the model and the size that WERE in the 32 real
    /// bytes. Worse, the kernel refused the same block for its own mode (every
    /// decoder gates on `block_valid`), so the EDID it handed out and the mode it
    /// advertised could disagree about the same monitor.
    #[test]
    fn a_driver_that_reports_something_that_is_not_an_edid_has_it_refused() {
        let screen = kms_emu::attach(64, 16);
        let mut padded = real_edid();
        // Keep the header, drop the checksum: exactly the shape zero-padding
        // produces, and exactly the shape a length check lets through.
        padded[127] = padded[127].wrapping_add(1);
        let gpu = screen.attach_gpu(EmuGpu::new("emu-edid").with_edid(padded));

        assert_eq!(
            drm::get_connector_edid(41),
            None,
            "a block that fails its own checksum was served as a monitor's identity"
        );
        drop(gpu);
    }

    /// And the other direction, so the refusal is not simply "always none":
    /// a driver reporting a real block still has it served, byte for byte.
    #[test]
    fn a_driver_that_reports_a_real_edid_has_it_served_unchanged() {
        let screen = kms_emu::attach(64, 16);
        let good = real_edid();
        let gpu = screen.attach_gpu(EmuGpu::new("emu-edid").with_edid(good));

        assert_eq!(drm::get_connector_edid(41), Some(good));
        drop(gpu);
    }

    /// The 32 bytes the RM actually gives, completed the way the driver now
    /// completes them, go through. The two halves of the fix have to agree: a
    /// core that refuses without a driver that repairs would just lose the
    /// monitor's identity instead of keeping it.
    #[test]
    fn the_thirty_two_byte_head_the_rm_gives_is_served_once_completed() {
        let screen = kms_emu::attach(64, 16);
        let head = &real_edid()[..32];
        let completed = zcore_drivers::display::edid::finish_partial_block(head)
            .expect("a real head completes");
        let gpu = screen.attach_gpu(EmuGpu::new("emu-edid").with_edid(completed));

        assert_eq!(drm::get_connector_edid(41), Some(completed));
        drop(gpu);
    }

    /// A zeroed ioctl argument, for the arms whose structs have no
    /// `Default`. All of them are `repr(C)` integers, for which zero is a
    /// value.
    fn zeroed<T: Copy>() -> T {
        // SAFETY: every struct this is used for is plain integers.
        unsafe { core::mem::zeroed() }
    }

    /// Every mode-object lookup Linux answers ENOENT for an id that does not
    /// exist (`drm_mode_object_find` and its typed wrappers), and the one
    /// encoder is the only encoder. Here GETCRTC, GETPLANE, GETCONNECTOR,
    /// GETPROPBLOB and CLOSEFB said EINVAL; GETENCODER answered any id with
    /// the synthetic encoder and rewrote the id; OBJ_GETPROPERTIES gave an
    /// unknown id the encoder's empty list; and SETPLANE, CURSOR, the gamma
    /// pair, OBJ_SETPROPERTY and SETPROPERTY never looked the object up at
    /// all and reported success. The ids the client really has keep
    /// working.
    #[test]
    fn unknown_mode_object_ids_answer_enoent_like_linux() {
        let screen = kms_emu::attach(32, 8);
        let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
        let c = Client::open(0);
        const BOGUS: u32 = 4242;
        let enoent = Err(FsError::EntryNotFound);

        // GETCRTC / GETPLANE / GETCONNECTOR: the typed lookups.
        let mut crtc: DrmModeGetCrtc = zeroed();
        crtc.crtc_id = BOGUS;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc), enoent);
        crtc.crtc_id = 60;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc), Ok(0));

        let mut plane: DrmModeGetPlane = zeroed();
        plane.plane_id = BOGUS;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut plane), enoent);
        plane.plane_id = 62;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut plane), Ok(0));

        let mut conn: DrmModeGetConnector = zeroed();
        conn.connector_id = BOGUS;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn), enoent);
        conn.connector_id = 61;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn), Ok(0));

        // GETENCODER: one encoder, and an unknown id is not renamed to it.
        let mut enc: DrmModeGetEncoder = zeroed();
        enc.encoder_id = BOGUS;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETENCODER, &mut enc), enoent);
        assert_eq!(
            enc.encoder_id, BOGUS,
            "the id the client asked about was rewritten"
        );
        enc.encoder_id = drm::SYNTH_ENCODER_ID;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETENCODER, &mut enc), Ok(0));
        assert_eq!(enc.encoder_id, drm::SYNTH_ENCODER_ID);

        // OBJ_GETPROPERTIES: an unknown id is not "an object with no
        // properties"; the encoder still is.
        let mut props: DrmModeObjGetProperties = zeroed();
        props.obj_id = BOGUS;
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut props),
            enoent
        );
        props.obj_id = drm::SYNTH_ENCODER_ID;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut props), Ok(0));
        assert_eq!(props.count_props, 0, "the encoder carries no properties");
        props.obj_id = 61;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut props), Ok(0));
        assert!(props.count_props > 0, "the connector does");

        // GETPROPBLOB: neither a blob id nor an EDID id of a connector that
        // does not exist.
        let mut blob: DrmModeGetBlob = zeroed();
        blob.blob_id = BOGUS;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPROPBLOB, &mut blob), enoent);
        blob.blob_id = edid_blob_id(BOGUS);
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPROPBLOB, &mut blob), enoent);

        // CLOSEFB, like RMFB.
        let mut fb_id = BOGUS;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_CLOSEFB, &mut fb_id), enoent);

        // SETPLANE looks the plane up first; disabling the real one is fine.
        let mut set_plane: DrmModeSetPlane = zeroed();
        set_plane.plane_id = BOGUS;
        set_plane.crtc_id = 60;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut set_plane), enoent);
        set_plane.plane_id = 62;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut set_plane), Ok(0));

        // CURSOR: "Unknown CRTC ID".
        let mut cur = ModeCursor {
            flags: 0x02, // MOVE
            crtc_id: BOGUS,
            x: 1,
            y: 1,
            width: 0,
            height: 0,
            handle: 0,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur), enoent);
        cur.crtc_id = 60;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur), Ok(0));

        // GETGAMMA / SETGAMMA: `struct drm_mode_crtc_lut` starts with the
        // CRTC id.
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct CrtcLut {
            crtc_id: u32,
            gamma_size: u32,
            red: u64,
            green: u64,
            blue: u64,
        }
        let mut lut: CrtcLut = zeroed();
        lut.crtc_id = BOGUS;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETGAMMA, &mut lut), enoent);
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETGAMMA, &mut lut), enoent);
        lut.crtc_id = 60;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETGAMMA, &mut lut), Ok(0));
        // A CRTC with no gamma store: `drm_crtc_supports_legacy_gamma`.
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_SETGAMMA, &mut lut),
            Err(FsError::NotSupported)
        );

        // OBJ_SETPROPERTY: the object and the property must both exist.
        let mut set = DrmModeObjSetProperty {
            value: DRM_MODE_DPMS_ON,
            prop_id: PROP_DPMS,
            obj_id: BOGUS,
            obj_type: 0,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut set), enoent);
        set.obj_id = 61;
        set.prop_id = BOGUS;
        // A property the object does not carry: `drm_mode_obj_find_prop_id`
        // misses and the ioctl's EINVAL stands (it was ENOENT here).
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut set),
            Err(FsError::InvalidParam)
        );
        set.prop_id = PROP_DPMS;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut set), Ok(0));

        // SETPROPERTY: `struct drm_mode_connector_set_property`.
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct ConnectorSetProperty {
            value: u64,
            prop_id: u32,
            connector_id: u32,
        }
        let mut cset = ConnectorSetProperty {
            value: DRM_MODE_DPMS_ON,
            prop_id: PROP_DPMS,
            connector_id: BOGUS,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut cset), enoent);
        cset.connector_id = 61;
        cset.prop_id = BOGUS;
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut cset),
            Err(FsError::InvalidParam)
        );
        cset.prop_id = PROP_DPMS;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut cset), Ok(0));
    }

    /// `drm_mode_setcrtc` finds the CRTC first and every connector it is
    /// handed (ENOENT), and refuses a connector list with no mode or no fb
    /// to set, or longer than the card's connectors (EINVAL);
    /// `drm_mode_page_flip_ioctl` and `drm_mode_setplane` find the CRTC
    /// too. None of the three read the CRTC id, and SETCRTC never read its
    /// connector list: a modeset or a flip aimed at a CRTC the card does not
    /// have landed on the one it has. The real ids keep working.
    #[test]
    fn setcrtc_page_flip_and_setplane_look_the_crtc_and_the_connectors_up() {
        let screen = kms_emu::attach(32, 8);
        let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
        let c = Client::open(0);
        const BOGUS: u32 = 4242;
        let enoent = Err(FsError::EntryNotFound);
        let buf = c.create_dumb(32, 8);
        paint(&buf, 0x0000_3333);
        let fb = c.addfb2(&buf);
        // What the CRTC shows before this test touches it (the core's
        // `crtc_fb` is process-wide, so it may carry a neighbour's id).
        let before = get_crtc_fb(&c, 60);
        assert_ne!(before, fb);

        let setcrtc = |crtc_id: u32, connectors: &[u32], mode_valid: u32, fb_id: u32| {
            let mut req = DrmModeGetCrtc {
                set_connectors_ptr: connectors.as_ptr() as u64,
                count_connectors: connectors.len() as u32,
                crtc_id,
                fb_id,
                x: 0,
                y: 0,
                gamma_size: 0,
                mode_valid,
                mode: make_modeinfo(32, 8),
            };
            c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req)
        };
        assert_eq!(
            setcrtc(BOGUS, &[61], 1, fb),
            enoent,
            "a CRTC the card does not have"
        );
        assert_eq!(
            setcrtc(60, &[BOGUS], 1, fb),
            enoent,
            "a connector it does not have"
        );
        assert_eq!(
            setcrtc(60, &[61, BOGUS], 1, fb),
            Err(FsError::InvalidParam),
            "more connectors than the card has"
        );
        assert_eq!(
            setcrtc(60, &[61], 0, fb),
            Err(FsError::InvalidParam),
            "connectors but no mode"
        );
        assert_eq!(
            setcrtc(60, &[61], 1, 0),
            enoent,
            "a mode with fb 0: the fb lookup comes before the connector rules"
        );
        assert_eq!(
            get_crtc_fb(&c, 60),
            before,
            "a refused modeset presented anyway"
        );
        assert_eq!(
            setcrtc(60, &[61], 1, fb),
            Ok(0),
            "the real CRTC and connector"
        );
        assert_eq!(get_crtc_fb(&c, 60), fb);

        let mut flip = DrmModeCrtcPageFlip {
            crtc_id: BOGUS,
            fb_id: fb,
            flags: 0x01, // DRM_MODE_PAGE_FLIP_EVENT
            reserved: 0,
            user_data: 7,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_PAGE_FLIP, &mut flip), enoent);
        let mut events = [0u8; 256];
        assert!(
            matches!(c.read_events(&mut events), Err(_) | Ok(0)),
            "a refused flip queued a completion"
        );

        let mut set_plane: DrmModeSetPlane = zeroed();
        set_plane.plane_id = 62;
        set_plane.crtc_id = BOGUS;
        set_plane.fb_id = fb;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut set_plane), enoent);
        set_plane.crtc_id = 60;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut set_plane), Ok(0));

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// `__setplane_check`: the plane has to be usable on the CRTC named
    /// (`possible_crtcs`, EINVAL) and the source rectangle, in 16.16, has to
    /// lie inside the fb (ENOSPC). Neither was read: a plane went onto a CRTC
    /// it does not reach, and a crop past the fb's edge was accepted, so a
    /// client believed it was showing a crop the scanout never made.
    #[test]
    fn setplane_wants_a_crtc_the_plane_reaches_and_a_source_inside_the_fb() {
        let screen = kms_emu::attach(32, 8);
        let _first = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-0").with_ids(60, 61, 62));
        let _second = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-1").with_ids(70, 71, 72));
        let c = Client::open(0);
        let buf = c.create_dumb(32, 8);
        paint(&buf, 0x0000_7777);
        let fb = c.addfb2(&buf);
        // The resource list decides the CRTC indices; the plane of each
        // card is its CRTC id plus two.
        let (crtcs, _) = topology(&c);
        assert_eq!(crtcs.len(), 2);
        let (front, back) = (crtcs[0], crtcs[1]);
        let plane_of = |crtc: u32| crtc + 2;

        let set_plane = |plane_id: u32, crtc_id: u32, src: [u32; 4]| {
            let mut req: DrmModeSetPlane = zeroed();
            req.plane_id = plane_id;
            req.crtc_id = crtc_id;
            req.fb_id = fb;
            req.crtc_w = 32;
            req.crtc_h = 8;
            req.src_x = src[0] << 16;
            req.src_y = src[1] << 16;
            req.src_w = src[2] << 16;
            req.src_h = src[3] << 16;
            c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut req)
        };
        let enospc = Err(FsError::NoDeviceSpace);

        // Every plane advertises `possible_crtcs = 1`, CRTC index 0: the
        // plane of the card listed second is not usable on its own CRTC, by
        // what the client was told, and the first's is.
        assert_eq!(
            set_plane(plane_of(back), back, [0, 0, 32, 8]),
            Err(FsError::InvalidParam),
            "a CRTC the plane's mask does not reach"
        );
        assert_eq!(set_plane(plane_of(front), front, [0, 0, 32, 8]), Ok(0));

        assert_eq!(
            set_plane(plane_of(front), front, [0, 0, 33, 8]),
            enospc,
            "wider than the fb"
        );
        assert_eq!(
            set_plane(plane_of(front), front, [0, 0, 32, 9]),
            enospc,
            "taller than the fb"
        );
        assert_eq!(
            set_plane(plane_of(front), front, [1, 0, 32, 8]),
            enospc,
            "x pushes it past the edge"
        );
        assert_eq!(
            set_plane(plane_of(front), front, [0, 1, 32, 8]),
            enospc,
            "y pushes it past the bottom"
        );
        assert_eq!(
            set_plane(plane_of(front), front, [16, 4, 16, 4]),
            Ok(0),
            "a crop that fits"
        );
        assert_eq!(
            set_plane(plane_of(front), front, [0, 0, 0, 0]),
            Ok(0),
            "no source rectangle at all"
        );

        // A fractional source edge counts: 31.5 wide from x = 0.75 is past 32.
        let mut req: DrmModeSetPlane = zeroed();
        req.plane_id = plane_of(front);
        req.crtc_id = front;
        req.fb_id = fb;
        req.src_x = 3 << 14;
        req.src_w = (31 << 16) | (1 << 15);
        req.src_h = 8 << 16;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut req), enospc);

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// `drm_mode_page_flip_ioctl`, once the CRTC and the fb are found: the
    /// CRTC's mode has to fit in the new fb (`drm_crtc_check_viewport`,
    /// ENOSPC), and "page flip is not allowed to change frame buffer format"
    /// (EINVAL). Neither was read: a flip onto an fb narrower than the mode,
    /// or of another format, was scanned out as the frame the CRTC was set
    /// up with, and the client was told it had flipped.
    #[test]
    fn page_flip_wants_an_fb_that_holds_the_mode_and_keeps_the_format() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let (crtcs, _) = topology(&c);
        let crtc = crtcs[0];
        let xr24 = c.create_dumb(32, 8);
        paint(&xr24, 0x0000_5555);
        let fb_xr24 = c.addfb2(&xr24);
        set_crtc(&c, crtc, fb_xr24, 32, 8);
        assert_eq!(get_crtc_fb(&c, crtc), fb_xr24);

        let addfb2 = |buf: &DrmModeCreateDumb, pixel_format: u32| {
            let mut cmd = DrmModeFbCmd2 {
                fb_id: 0,
                width: buf.width,
                height: buf.height,
                pixel_format,
                flags: 0,
                handles: [buf.handle, 0, 0, 0],
                pitches: [buf.pitch, 0, 0, 0],
                offsets: [0; 4],
                modifier: [0; 4],
            };
            c.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd).expect("ADDFB2");
            cmd.fb_id
        };
        let narrow = c.create_dumb(16, 8);
        let fb_narrow = addfb2(&narrow, drm::DRM_FORMAT_XRGB8888);
        let short = c.create_dumb(32, 4);
        let fb_short = addfb2(&short, drm::DRM_FORMAT_XRGB8888);
        let ar24 = c.create_dumb(32, 8);
        paint(&ar24, 0xff00_6666);
        let fb_ar24 = addfb2(&ar24, drm::DRM_FORMAT_ARGB8888);
        let ar24_too = c.create_dumb(32, 8);
        let fb_ar24_too = addfb2(&ar24_too, drm::DRM_FORMAT_ARGB8888);

        assert_eq!(
            c.page_flip(crtc, fb_narrow, 1),
            Err(FsError::NoDeviceSpace),
            "narrower than the mode"
        );
        assert_eq!(
            c.page_flip(crtc, fb_short, 2),
            Err(FsError::NoDeviceSpace),
            "shorter than the mode"
        );
        assert_eq!(
            c.page_flip(crtc, fb_ar24, 3),
            Err(FsError::InvalidParam),
            "XRGB8888 on the CRTC, ARGB8888 flipped"
        );
        assert_eq!(
            get_crtc_fb(&c, crtc),
            fb_xr24,
            "a refused flip presented anyway"
        );
        let mut events = [0u8; 256];
        assert!(
            matches!(c.read_events(&mut events), Err(_) | Ok(0)),
            "a refused flip queued a completion"
        );

        // A modeset may change the format; a flip may then keep the new one.
        set_crtc(&c, crtc, fb_ar24, 32, 8);
        assert_eq!(
            c.page_flip(crtc, fb_xr24, 4),
            Err(FsError::InvalidParam),
            "ARGB8888 on the CRTC, XRGB8888 flipped"
        );
        assert_eq!(c.page_flip(crtc, fb_ar24_too, 5), Ok(0));
        drm::flush_pending_flip_completions();
        assert_eq!(get_crtc_fb(&c, crtc), fb_ar24_too);
        let _ = c.read_events(&mut events);

        for fb in [fb_xr24, fb_narrow, fb_short, fb_ar24, fb_ar24_too] {
            c.rmfb(fb).expect("RMFB");
        }
        for buf in [&xr24, &narrow, &short, &ar24, &ar24_too] {
            c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
        }
    }

    /// `drm_mode_obj_set_property_ioctl`, and SETPROPERTY through it: the
    /// object of the type named (ENOENT), a property the object carries
    /// (EINVAL; an encoder carries none), then `drm_property_change_valid_get`:
    /// not immutable, and a value the property's type admits (EINVAL). Any
    /// value for any known property on any existing object was "set".
    #[test]
    fn a_property_write_is_checked_against_the_object_and_the_property() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let (crtcs, conns) = topology(&c);
        let (crtc, conn) = (crtcs[0], conns[0]);
        universal_planes(&c, true);
        let plane = planes(&c)[0];
        let buf = c.create_dumb(32, 8);
        let fb = c.addfb2(&buf);
        let einval = Err(FsError::InvalidParam);
        let enoent = Err(FsError::EntryNotFound);
        let set = |obj_id: u32, obj_type: u32, prop_id: u32, value: u64| {
            let mut req = DrmModeObjSetProperty {
                value,
                prop_id,
                obj_id,
                obj_type,
            };
            c.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut req)
        };
        // The object, of the type asked for.
        assert_eq!(
            set(crtc, DRM_MODE_OBJECT_CONNECTOR, PROP_ACTIVE, 1),
            enoent,
            "a CRTC is not a connector"
        );
        assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_ACTIVE, 1), Ok(0));
        assert_eq!(set(crtc, DRM_MODE_OBJECT_ANY, PROP_ACTIVE, 1), Ok(0));
        // A property the object carries.
        assert_eq!(
            set(crtc, DRM_MODE_OBJECT_CRTC, PROP_DPMS, DRM_MODE_DPMS_ON),
            einval,
            "DPMS is the connector's"
        );
        assert_eq!(
            set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_TYPE, 1),
            einval,
            "type is the plane's"
        );
        assert_eq!(
            set(drm::SYNTH_ENCODER_ID, DRM_MODE_OBJECT_ENCODER, PROP_DPMS, 0),
            einval,
            "an encoder carries none"
        );
        assert_eq!(
            set(conn, DRM_MODE_OBJECT_CONNECTOR, 0xdead, 0),
            einval,
            "no such property"
        );
        // Immutable.
        assert_eq!(set(plane, DRM_MODE_OBJECT_PLANE, PROP_TYPE, 1), einval);
        assert_eq!(
            set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_NON_DESKTOP, 0),
            einval
        );
        // An enum takes one of its listed values.
        assert_eq!(set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_DPMS, 7), einval);
        assert_eq!(
            set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_LINK_STATUS, 1),
            Ok(0)
        );
        // A range and a signed range, by their bounds.
        assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_ACTIVE, 2), einval);
        assert_eq!(
            set(
                plane,
                DRM_MODE_OBJECT_PLANE,
                PROP_CRTC_X,
                i32::MIN as i64 as u64
            ),
            Ok(0)
        );
        assert_eq!(
            set(
                plane,
                DRM_MODE_OBJECT_PLANE,
                PROP_CRTC_X,
                i32::MAX as u64 + 1
            ),
            einval
        );
        assert_eq!(
            set(
                plane,
                DRM_MODE_OBJECT_PLANE,
                PROP_CRTC_W,
                i32::MAX as u64 + 1
            ),
            einval
        );
        assert_eq!(
            set(plane, DRM_MODE_OBJECT_PLANE, PROP_SRC_W, u32::MAX as u64),
            Ok(0)
        );
        // An object property: 0, or an object of the property's type.
        assert_eq!(set(plane, DRM_MODE_OBJECT_PLANE, PROP_FB_ID, 0), Ok(0));
        assert_eq!(
            set(plane, DRM_MODE_OBJECT_PLANE, PROP_FB_ID, fb as u64),
            Ok(0)
        );
        // (Not "the CRTC's id": framebuffer ids and mode-object ids are
        // separate namespaces here, so fb 1 and CRTC 1 can both exist.)
        assert_eq!(
            set(plane, DRM_MODE_OBJECT_PLANE, PROP_FB_ID, 0xdead_0000),
            einval,
            "no such fb"
        );
        assert_eq!(
            set(plane, DRM_MODE_OBJECT_PLANE, PROP_CRTC_ID, crtc as u64),
            Ok(0)
        );
        assert_eq!(
            set(plane, DRM_MODE_OBJECT_PLANE, PROP_CRTC_ID, conn as u64),
            einval
        );
        assert_eq!(
            set(
                plane,
                DRM_MODE_OBJECT_PLANE,
                PROP_CRTC_ID,
                (1 << 32) | crtc as u64
            ),
            einval,
            "not a 32-bit id, whatever its low word names"
        );
        // A blob property: 0, or an existing blob.
        let bytes = [7u8; 68];
        let mut blob = DrmModeCreateBlob {
            data: bytes.as_ptr() as u64,
            length: bytes.len() as u32,
            blob_id: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
            .expect("CREATEPROPBLOB");
        assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_MODE_ID, 0), Ok(0));
        assert_eq!(
            set(
                crtc,
                DRM_MODE_OBJECT_CRTC,
                PROP_MODE_ID,
                blob.blob_id as u64
            ),
            Ok(0)
        );
        assert_eq!(
            set(crtc, DRM_MODE_OBJECT_CRTC, PROP_MODE_ID, 0xdead_beef),
            einval
        );
        let mut blob_id = blob.blob_id;
        c.ioctl(DRM_IOCTL_MODE_DESTROYPROPBLOB, &mut blob_id)
            .expect("DESTROYPROPBLOB");

        // SETPROPERTY is the same call with the connector type.
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct ConnectorSetProperty {
            value: u64,
            prop_id: u32,
            connector_id: u32,
        }
        let cset = |connector_id: u32, prop_id: u32, value: u64| {
            let mut req = ConnectorSetProperty {
                value,
                prop_id,
                connector_id,
            };
            c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut req)
        };
        assert_eq!(
            cset(crtc, PROP_DPMS, DRM_MODE_DPMS_ON),
            enoent,
            "a CRTC is not a connector"
        );
        assert_eq!(cset(conn, PROP_TYPE, 1), einval);
        assert_eq!(cset(conn, PROP_DPMS, 9), einval);
        assert_eq!(cset(conn, PROP_DPMS, 3), Ok(0), "Off");
        assert!(drm::crtc_blanked(), "and the DPMS write still lands");
        assert_eq!(cset(conn, PROP_DPMS, DRM_MODE_DPMS_ON), Ok(0));
        assert!(!drm::crtc_blanked());
    }

    /// `drm_mode_gamma_{get,set}_ioctl` on a CRTC whose `gamma_size` is 0,
    /// which is what GETCRTC reports here: SETGAMMA is ENOSYS
    /// (`drm_crtc_supports_legacy_gamma`), GETGAMMA wants the caller's
    /// `gamma_size` to be the CRTC's (EINVAL) and then copies that many
    /// entries, none. Both answered "done": a 256-entry ramp was "set", and
    /// a 256-entry GETGAMMA returned without writing one.
    #[test]
    fn gamma_ioctls_hold_the_caller_to_a_crtc_with_no_gamma_store() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let (crtcs, _) = topology(&c);
        let crtc = crtcs[0];
        let mut info: DrmModeGetCrtc = zeroed();
        info.crtc_id = crtc;
        c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut info).expect("GETCRTC");
        assert_eq!(info.gamma_size, 0, "no gamma store, as GETCRTC says");

        let mut red = [0x1111u16; 256];
        let mut green = [0x2222u16; 256];
        let mut blue = [0x3333u16; 256];
        let (r, g, b) = (
            red.as_mut_ptr() as u64,
            green.as_mut_ptr() as u64,
            blue.as_mut_ptr() as u64,
        );
        let gamma = |cmd: u32, crtc_id: u32, gamma_size: u32| {
            let mut lut = DrmModeCrtcLut {
                crtc_id,
                gamma_size,
                red: r,
                green: g,
                blue: b,
            };
            c.ioctl(cmd, &mut lut)
        };
        for size in [0u32, 256] {
            assert_eq!(
                gamma(DRM_IOCTL_MODE_SETGAMMA, crtc, size),
                Err(FsError::NotSupported),
                "SETGAMMA with {} entries",
                size
            );
        }
        assert_eq!(
            gamma(DRM_IOCTL_MODE_GETGAMMA, crtc, 256),
            Err(FsError::InvalidParam),
            "not the CRTC's gamma_size"
        );
        assert_eq!(gamma(DRM_IOCTL_MODE_GETGAMMA, crtc, 0), Ok(0));
        assert!(
            red.iter().all(|&v| v == 0x1111)
                && green.iter().all(|&v| v == 0x2222)
                && blue.iter().all(|&v| v == 0x3333),
            "zero entries copied"
        );
        assert_eq!(
            gamma(DRM_IOCTL_MODE_GETGAMMA, 0xdead_0000, 0),
            Err(FsError::EntryNotFound)
        );
    }

    /// With a mode, `drm_mode_setcrtc` looks the fb up (ENOENT; -1 is the
    /// fb already on the CRTC, EINVAL when there is none), refuses a mode
    /// `drm_mode_validate_basic` would not have (a zero clock, a zero active
    /// area, sync timings out of order) or an aspect-ratio code it does not
    /// define (EINVAL), and wants the active area at (x, y) inside the fb
    /// (ENOSPC). None of it was read: a 64-wide mode on a 32-wide fb was
    /// scanned out, and a clockless mode was paced from a fallback.
    #[test]
    fn setcrtc_wants_a_mode_that_is_one_and_an_fb_that_holds_it() {
        // The synthetic pipe: its CRTC reports exactly the fb the core has on
        // it, so "nothing on the CRTC" is a state this test can reach.
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let (crtcs, conns) = topology(&c);
        let (crtc, conn) = (crtcs[0], conns[0]);
        let buf = c.create_dumb(32, 8);
        paint(&buf, 0x0000_4444);
        let fb = c.addfb2(&buf);
        let einval = Err(FsError::InvalidParam);
        let enospc = Err(FsError::NoDeviceSpace);

        let setcrtc = |fb_id: u32, x: u32, y: u32, mode: [u8; 68]| {
            let connectors = [conn];
            let mut req = DrmModeGetCrtc {
                set_connectors_ptr: connectors.as_ptr() as u64,
                count_connectors: 1,
                crtc_id: crtc,
                fb_id,
                x,
                y,
                gamma_size: 0,
                mode_valid: 1,
                mode,
            };
            c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req)
        };
        let good = make_modeinfo(32, 8);
        let put_u16 =
            |m: &mut [u8; 68], at: usize, v: u16| m[at..at + 2].copy_from_slice(&v.to_ne_bytes());

        // The fb: -1 with nothing on the CRTC, and an id that is not one.
        let mut off = DrmModeGetCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id: crtc,
            fb_id: 0,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: [0; 68],
        };
        c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut off)
            .expect("SETCRTC off");
        assert_eq!(get_crtc_fb(&c, crtc), 0);
        assert_eq!(
            setcrtc(u32::MAX, 0, 0, good),
            einval,
            "-1 with no fb on the CRTC"
        );
        assert_eq!(
            setcrtc(4242, 0, 0, good),
            Err(FsError::EntryNotFound),
            "an fb that does not exist"
        );

        // The viewport: the active area at (x, y) has to lie inside the fb.
        assert_eq!(
            setcrtc(fb, 0, 0, make_modeinfo(64, 8)),
            enospc,
            "wider than the fb"
        );
        assert_eq!(
            setcrtc(fb, 0, 0, make_modeinfo(32, 16)),
            enospc,
            "taller than the fb"
        );
        assert_eq!(
            setcrtc(fb, 1, 0, good),
            enospc,
            "x pushes it past the right edge"
        );
        assert_eq!(
            setcrtc(fb, 0, 1, good),
            enospc,
            "y pushes it past the bottom"
        );
        assert_eq!(
            setcrtc(fb, 0x1_0000, 0, good),
            enospc,
            "an x with high bits set is outside any fb"
        );

        // The mode: what `drm_mode_validate_basic` refuses.
        let mut m = good;
        m[0..4].copy_from_slice(&0u32.to_ne_bytes());
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "clock 0");
        let mut m = good;
        put_u16(&mut m, 4, 0);
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "hdisplay 0");
        let mut m = good;
        put_u16(&mut m, 6, 31);
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "hsync_start before hdisplay");
        let mut m = good;
        put_u16(&mut m, 8, u16::from_ne_bytes([good[6], good[7]]) - 1);
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "hsync_end before hsync_start");
        let mut m = good;
        put_u16(&mut m, 10, u16::from_ne_bytes([good[8], good[9]]) - 1);
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "htotal before hsync_end");
        let mut m = good;
        put_u16(&mut m, 14, 0);
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "vdisplay 0");
        let mut m = good;
        put_u16(&mut m, 16, 7);
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "vsync_start before vdisplay");
        let mut m = good;
        put_u16(&mut m, 18, u16::from_ne_bytes([good[16], good[17]]) - 1);
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "vsync_end before vsync_start");
        let mut m = good;
        put_u16(&mut m, 20, u16::from_ne_bytes([good[18], good[19]]) - 1);
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "vtotal before vsync_end");
        let mut m = good;
        let flags = u32::from_ne_bytes([good[28], good[29], good[30], good[31]]);
        m[28..32].copy_from_slice(&(flags | (5 << 19)).to_ne_bytes());
        assert_eq!(setcrtc(fb, 0, 0, m), einval, "aspect-ratio code 5");
        assert_eq!(
            get_crtc_fb(&c, crtc),
            0,
            "a refused modeset presented anyway"
        );

        // What passes: the fb that fits, with the last aspect-ratio code the
        // kernel defines, and then -1 for the same fb again.
        let mut m = good;
        m[28..32].copy_from_slice(&(flags | (4 << 19)).to_ne_bytes());
        assert_eq!(setcrtc(fb, 0, 0, m), Ok(0));
        assert_eq!(get_crtc_fb(&c, crtc), fb);
        assert_eq!(
            setcrtc(u32::MAX, 0, 0, good),
            Ok(0),
            "-1 is the fb on the CRTC"
        );
        assert_eq!(get_crtc_fb(&c, crtc), fb);

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// On the hardware-KMS path the driver's `get_crtc` carries its own
    /// framebuffer id, in its own namespace (`EMU_DRIVER_FB_BASE` here), and
    /// the core only overrides it while a DRM framebuffer is on the CRTC.
    /// `drm_mode_getcrtc` reports `fb_id = 0` and `mode_valid = 0` for a CRTC
    /// that `SETCRTC` disabled, so the driver's id must not show through
    /// once the DRM one is gone.
    #[test]
    fn a_disabled_hardware_crtc_reports_no_framebuffer_and_no_mode() {
        let screen = kms_emu::attach(32, 8);
        let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
        let c = Client::open(0);
        let buf = c.create_dumb(32, 8);
        let fb = c.addfb2(&buf);
        let read = || {
            let mut crtc: DrmModeGetCrtc = zeroed();
            crtc.crtc_id = 60;
            c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
            (crtc.mode_valid, crtc.fb_id)
        };
        set_crtc(&c, 60, fb, 32, 8);
        assert_eq!(read(), (1, fb), "with the framebuffer on the CRTC");

        let mut disable: DrmModeGetCrtc = zeroed();
        disable.crtc_id = 60;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut disable), Ok(0));
        assert_eq!(read(), (0, 0), "the driver's own fb id showed through");

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// The hardware driver's `get_plane` carries its own framebuffer id, in
    /// its own namespace (`EMU_DRIVER_FB_BASE`), like its `get_crtc`. The
    /// core reports the DRM framebuffer on the plane instead, and 0 with no
    /// CRTC once the pipe is disabled; the driver's id must never show.
    #[test]
    fn the_hardware_primary_plane_never_shows_the_drivers_own_fb_id() {
        let screen = kms_emu::attach(32, 8);
        let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
        let c = Client::open(0);
        let buf = c.create_dumb(32, 8);
        let fb = c.addfb2(&buf);
        let plane = || {
            let mut res: DrmModeGetPlane = zeroed();
            res.plane_id = 62;
            c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut res)
                .expect("GETPLANE");
            (res.crtc_id, res.fb_id)
        };
        assert_eq!(plane(), (0, 0), "nothing on the plane yet");
        set_crtc(&c, 60, fb, 32, 8);
        assert_eq!(plane(), (60, fb), "the DRM framebuffer, not the driver's");

        let mut disable: DrmModeGetCrtc = zeroed();
        disable.crtc_id = 60;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut disable), Ok(0));
        assert_eq!(plane(), (0, 0), "the driver's own fb id showed through");

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// `CURSOR` with a buffer: Linux wraps the handle in a framebuffer, so a
    /// handle the file does not hold is ENOENT and a buffer too small for
    /// `width x height` pixels is EINVAL. Both came back as success with the
    /// pointer quietly hidden, so a compositor whose cursor upload went
    /// wrong was never told. A buffer that fits keeps working.
    #[test]
    fn a_cursor_needs_a_handle_of_its_own_that_fits_the_image() {
        let screen = kms_emu::attach(32, 8);
        let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
        let c = Client::open(0);
        const BOGUS: u32 = 4242;
        let cursor = |handle: u32, w: u32, h: u32| {
            let mut cur = ModeCursor {
                flags: 0x01, // BO
                crtc_id: 60,
                x: 0,
                y: 0,
                width: w,
                height: h,
                handle,
            };
            c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur)
        };
        assert_eq!(cursor(BOGUS, 16, 16), Err(FsError::EntryNotFound));

        // 16 rows of a 64-byte pitch: 1 KiB, room for 16x16 and not 64x64.
        let small = c.create_dumb(16, 16);
        assert_eq!(cursor(small.handle, 64, 64), Err(FsError::InvalidParam));
        assert_eq!(cursor(small.handle, 16, 17), Err(FsError::InvalidParam));
        assert_eq!(cursor(small.handle, 16, 16), Ok(0));
        // Hiding the cursor names no buffer and needs none.
        assert_eq!(cursor(0, 0, 0), Ok(0));

        c.destroy_dumb(small.handle).expect("DESTROY_DUMB");
    }

    /// `drm_mode_getresources` and `drm_mode_object_get_properties` (which
    /// GETCONNECTOR uses for its property list too) write as many entries
    /// as the caller made room for and report the full length, so a short
    /// array gets a prefix and the count to allocate; and
    /// `drm_mode_obj_get_properties_ioctl` finds the object by id and type,
    /// so a connector asked about as a plane is ENOENT. Here the lists
    /// were copied all or nothing, and any type matched any id.
    #[test]
    fn lists_are_filled_as_far_as_the_caller_made_room_and_objects_match_their_type() {
        let screen = kms_emu::attach(32, 8);
        let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
        let c = Client::open(0);
        const SENTINEL: u32 = 0xfeed_beef;
        let enoent = Err(FsError::EntryNotFound);

        // GETRESOURCES: two framebuffers, room for one.
        let a = c.create_dumb(32, 8);
        let b = c.create_dumb(32, 8);
        let fb_a = c.addfb2(&a);
        let fb_b = c.addfb2(&b);
        let mut probe = blank_card_res();
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut probe), Ok(0));
        let total = probe.count_fbs;
        assert!(total >= 2, "both framebuffers are listed");
        let mut all = alloc::vec![SENTINEL; total as usize];
        let mut full = blank_card_res();
        full.fb_id_ptr = all.as_mut_ptr() as u64;
        full.count_fbs = total;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut full), Ok(0));
        assert!(all.contains(&fb_a) && all.contains(&fb_b));
        let mut one = [SENTINEL; 2];
        let mut short = blank_card_res();
        short.fb_id_ptr = one.as_mut_ptr() as u64;
        short.count_fbs = 1;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut short), Ok(0));
        assert_eq!(
            short.count_fbs, total,
            "the full length comes back with a short array"
        );
        assert_eq!(
            one[0], all[0],
            "the first entry is written into the room there is"
        );
        assert_eq!(one[1], SENTINEL, "nothing is written past the room");

        // OBJ_GETPROPERTIES on the connector: room for one of its properties.
        let mut count: DrmModeObjGetProperties = zeroed();
        count.obj_id = 61;
        count.obj_type = DRM_MODE_OBJECT_CONNECTOR;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut count), Ok(0));
        let n = count.count_props;
        assert!(n >= 2, "the connector carries several properties");
        let mut ids = alloc::vec![SENTINEL; n as usize];
        let mut vals = alloc::vec![u64::MAX; n as usize];
        let mut full = count;
        full.props_ptr = ids.as_mut_ptr() as u64;
        full.prop_values_ptr = vals.as_mut_ptr() as u64;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut full), Ok(0));
        assert!(!ids.contains(&SENTINEL));
        let mut one_id = [SENTINEL; 2];
        let mut one_val = [u64::MAX; 2];
        let mut short = count;
        short.props_ptr = one_id.as_mut_ptr() as u64;
        short.prop_values_ptr = one_val.as_mut_ptr() as u64;
        short.count_props = 1;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut short), Ok(0));
        assert_eq!(short.count_props, n);
        assert_eq!((one_id[0], one_val[0]), (ids[0], vals[0]));
        assert_eq!((one_id[1], one_val[1]), (SENTINEL, u64::MAX));

        // GETCONNECTOR's property list, the same way.
        let mut conn: DrmModeGetConnector = zeroed();
        conn.connector_id = 61;
        let mut conn_id = [SENTINEL; 2];
        let mut conn_val = [u64::MAX; 2];
        conn.props_ptr = conn_id.as_mut_ptr() as u64;
        conn.prop_values_ptr = conn_val.as_mut_ptr() as u64;
        conn.count_props = 1;
        assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn), Ok(0));
        assert_eq!(conn.count_props, n);
        assert_eq!((conn_id[0], conn_val[0]), (ids[0], vals[0]));
        assert_eq!((conn_id[1], conn_val[1]), (SENTINEL, u64::MAX));

        // The object must be of the type asked for; ANY matches every type.
        universal_planes(&c, true);
        let plane = planes(&c)[0];
        let by_type = |id: u32, ty: u32| {
            let mut req: DrmModeObjGetProperties = zeroed();
            req.obj_id = id;
            req.obj_type = ty;
            c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut req)
        };
        assert_eq!(by_type(61, DRM_MODE_OBJECT_PLANE), enoent);
        assert_eq!(by_type(61, DRM_MODE_OBJECT_CONNECTOR), Ok(0));
        assert_eq!(by_type(plane, DRM_MODE_OBJECT_CRTC), enoent);
        assert_eq!(by_type(plane, DRM_MODE_OBJECT_PLANE), Ok(0));
        assert_eq!(by_type(60, DRM_MODE_OBJECT_CONNECTOR), enoent);
        assert_eq!(by_type(60, DRM_MODE_OBJECT_CRTC), Ok(0));
        assert_eq!(by_type(drm::SYNTH_ENCODER_ID, DRM_MODE_OBJECT_CRTC), enoent);
        assert_eq!(
            by_type(drm::SYNTH_ENCODER_ID, DRM_MODE_OBJECT_ENCODER),
            Ok(0)
        );
        assert_eq!(by_type(60, DRM_MODE_OBJECT_ANY), Ok(0));
    }
}

#[cfg(test)]
mod property_table_tests {
    //! The KMS property table, and the contract between advertising a property
    //! and accepting it in an atomic commit.
    //!
    //! Userspace matches properties **by name**: libdrm hands a compositor
    //! `drmModePropertyRes.name` and wlroots does `strcmp(prop->name, "FB_ID")`.
    //! So a typo in this table does not produce an error anywhere -- the
    //! property simply stops existing for every client, and the compositor
    //! falls back or gives up. Nothing in the tree checked those strings.
    //!
    //! The other half is drift between the two sides of the same property.
    //! `connector_props`/`crtc_props`/`plane_props` say which properties an
    //! object has; `prop_spec` describes them; `atomic_stage_on` accepts them.
    //! Three lists that must agree, in three different places in this file.

    use super::*;

    /// Every property id the pipeline uses. Kept honest in both directions by
    /// `every_property_the_pipeline_uses_is_in_the_table`, which also sweeps
    /// the id space so a `PROP_*` added to `prop_spec` and forgotten here
    /// fails rather than quietly escaping every test below.
    const ALL_PROPS: &[(u32, &str)] = &[
        (PROP_TYPE, "type"),
        (PROP_EDID, "EDID"),
        (PROP_DPMS, "DPMS"),
        (PROP_LINK_STATUS, "link-status"),
        (PROP_NON_DESKTOP, "non-desktop"),
        (PROP_FB_ID, "FB_ID"),
        (PROP_CRTC_ID, "CRTC_ID"),
        (PROP_CRTC_X, "CRTC_X"),
        (PROP_CRTC_Y, "CRTC_Y"),
        (PROP_CRTC_W, "CRTC_W"),
        (PROP_CRTC_H, "CRTC_H"),
        (PROP_SRC_X, "SRC_X"),
        (PROP_SRC_Y, "SRC_Y"),
        (PROP_SRC_W, "SRC_W"),
        (PROP_SRC_H, "SRC_H"),
        (PROP_ACTIVE, "ACTIVE"),
        (PROP_MODE_ID, "MODE_ID"),
        (PROP_IN_FENCE_FD, "IN_FENCE_FD"),
        (PROP_OUT_FENCE_PTR, "OUT_FENCE_PTR"),
        (PROP_FB_DAMAGE_CLIPS, "FB_DAMAGE_CLIPS"),
    ];

    /// `DRM_MODE_PROP_LEGACY_TYPE` / `DRM_MODE_PROP_EXTENDED_TYPE` from
    /// `uapi/drm/drm_mode.h`. BITMASK (1 << 5) is in the legacy mask even
    /// though this tree has no bitmask property yet.
    const LEGACY_TYPE: u32 = 0x0000_003a;
    const EXTENDED_TYPE: u32 = 0x0000_ffc0;

    fn spec(prop_id: u32) -> PropSpec {
        prop_spec(prop_id).expect("every property in ALL_PROPS must be in the table")
    }

    /// The name field of `struct drm_mode_get_property` is `char name[32]`,
    /// and `GETPROPERTY` copies at most 31 bytes into it to leave the NUL.
    const NAME_FIELD: usize = 32;

    #[test]
    fn every_property_the_pipeline_uses_is_in_the_table() {
        for (id, name) in ALL_PROPS {
            assert!(
                prop_spec(*id).is_some(),
                "property {} ({}) has no entry, so GETPROPERTY answers ENOENT \
                 for a property the object says it has",
                name,
                id,
            );
        }
        // And the other way round. The ids are hand-assigned small integers,
        // so sweeping well past the end of the range is enough to find one
        // that `prop_spec` knows and this module does not -- which would
        // otherwise slip through every test here without a sound.
        let known: alloc::vec::Vec<u32> = (0..1024).filter(|id| prop_spec(*id).is_some()).collect();
        let listed: alloc::vec::Vec<u32> = ALL_PROPS.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            known, listed,
            "the property table and ALL_PROPS have drifted apart",
        );
        assert!(prop_spec(u32::MAX).is_none());
    }

    /// Two properties sharing an id means the second is unreachable. In
    /// `prop_spec` and `atomic_stage_on` the compiler already says so --
    /// `unreachable_patterns`, which `deny(warnings)` turns into an error --
    /// so what is left for this test is the list above, where a copy-paste
    /// that pairs the wrong constant with a name compiles fine and would
    /// silently stop testing one of the two properties.
    #[test]
    fn property_ids_are_unique() {
        for (i, (id, name)) in ALL_PROPS.iter().enumerate() {
            for (other_id, other_name) in &ALL_PROPS[i + 1..] {
                assert_ne!(id, other_id, "{} and {} share id {}", name, other_name, id,);
            }
        }
    }

    /// The names are the uAPI. A compositor finds a property by comparing this
    /// string against a literal, so "CRTC_W" spelled "CRTC_w" is not a
    /// warning anywhere -- the plane just stops having a width.
    #[test]
    fn every_property_name_is_the_one_userspace_matches_on() {
        for (id, name) in ALL_PROPS {
            assert_eq!(
                spec(*id).name,
                *name,
                "property {} is advertised under a different name than Linux's",
                id,
            );
        }
    }

    /// `GETPROPERTY` truncates into `char name[32]`, keeping 31 bytes. A
    /// longer name is not refused; it arrives cut, and the compare fails.
    #[test]
    fn no_name_is_long_enough_to_be_truncated_on_the_way_out() {
        for (id, name) in ALL_PROPS {
            let s = spec(*id);
            assert!(!s.name.is_empty(), "{} has no name at all", id);
            assert!(
                s.name.len() < NAME_FIELD,
                "{} is {} bytes and would reach userspace cut to {}",
                name,
                s.name.len(),
                NAME_FIELD - 1,
            );
            for (val, enum_name) in s.enums {
                assert!(
                    enum_name.len() < NAME_FIELD,
                    "{}={} of {} would reach userspace cut",
                    enum_name,
                    val,
                    name,
                );
            }
        }
    }

    /// `drm_property_type_valid()`: a property carries a legacy type or an
    /// extended one, never both and never neither. `GETPROPERTY` reports
    /// `flags` verbatim, and libdrm switches on exactly this to decide whether
    /// to read the value list as a range, an enum or an object id.
    #[test]
    fn every_property_has_exactly_one_type() {
        for (id, name) in ALL_PROPS {
            let flags = spec(*id).flags;
            let legacy = flags & LEGACY_TYPE;
            let extended = flags & EXTENDED_TYPE;
            if extended != 0 {
                assert_eq!(
                    legacy, 0,
                    "{} carries an extended type and a legacy one at once",
                    name,
                );
            } else {
                assert_ne!(legacy, 0, "{} has no type at all", name);
                assert!(
                    legacy.is_power_of_two(),
                    "{} carries more than one legacy type ({:#x})",
                    name,
                    legacy,
                );
            }
        }
    }

    /// An enum property serves both lists, and `GETPROPERTY` fills them from
    /// two different fields of the same spec. They have to be the same set, in
    /// the same order: a client that reads `values` to know what it may set,
    /// and `enum_blob` to name it, would otherwise see two different menus.
    #[test]
    fn enum_properties_list_their_own_values() {
        for (id, name) in ALL_PROPS {
            let s = spec(*id);
            if s.flags & DRM_MODE_PROP_ENUM == 0 {
                assert!(
                    s.enums.is_empty(),
                    "{} is not an enum but carries enum entries",
                    name,
                );
                continue;
            }
            assert!(!s.enums.is_empty(), "{} is an enum with no entries", name);
            let from_enums: alloc::vec::Vec<u64> = s.enums.iter().map(|(v, _)| *v).collect();
            assert_eq!(
                s.values,
                &from_enums[..],
                "{}'s value list and enum list disagree",
                name,
            );
        }
    }

    /// A range property's value list is `[min, max]` -- exactly two, in order.
    /// The order is what tells the two range types apart: `CRTC_X` spans
    /// `i32::MIN..=i32::MAX`, whose bit patterns as `u64` run *backwards*, so
    /// a property marked plain RANGE when it should be SIGNED_RANGE fails
    /// here. That is not cosmetic: libdrm clamps a client's value to the
    /// advertised range, and an unsigned reading of `i32::MIN` is 4 billion.
    #[test]
    fn range_properties_carry_a_min_and_a_max_in_their_own_signedness() {
        for (id, name) in ALL_PROPS {
            let s = spec(*id);
            let signed = s.flags & EXTENDED_TYPE == DRM_MODE_PROP_SIGNED_RANGE;
            if s.flags & DRM_MODE_PROP_RANGE == 0 && !signed {
                continue;
            }
            assert_eq!(
                s.values.len(),
                2,
                "{} is a range and must list exactly [min, max]",
                name,
            );
            let (min, max) = (s.values[0], s.values[1]);
            if signed {
                assert!(
                    (min as i64) <= (max as i64),
                    "{} has min {} above max {} read as signed",
                    name,
                    min as i64,
                    max as i64,
                );
            } else {
                assert!(min <= max, "{} has min {} above max {}", name, min, max);
            }
        }
    }

    /// An object property names the one object type it accepts, and a blob
    /// property carries no list at all: its value *is* the blob id.
    #[test]
    fn object_and_blob_properties_carry_the_list_their_type_implies() {
        for (id, name) in ALL_PROPS {
            let s = spec(*id);
            if s.flags & EXTENDED_TYPE == DRM_MODE_PROP_OBJECT {
                assert_eq!(
                    s.values.len(),
                    1,
                    "{} must name exactly one object type",
                    name,
                );
            }
            if s.flags & DRM_MODE_PROP_BLOB != 0 {
                assert!(s.values.is_empty(), "{} is a blob with a value list", name);
                assert!(s.enums.is_empty(), "{} is a blob with enum entries", name);
            }
        }
    }

    /// A plane whose ids do not matter: `atomic_stage_on` is past the lookup.
    fn a_plane() -> drm::DrmPlane {
        drm::DrmPlane {
            id: drm::SYNTH_PLANE_ID,
            crtc_id: drm::SYNTH_CRTC_ID,
            fb_id: 0,
            possible_crtcs: 1,
            plane_type: 1,
        }
    }

    fn advertised() -> alloc::vec::Vec<(AtomicObject, u32, u64)> {
        let mut out = alloc::vec::Vec::new();
        for (p, v) in plane_props(&a_plane(), true) {
            out.push((AtomicObject::Plane, p, v));
        }
        for (p, v) in crtc_props(true) {
            out.push((AtomicObject::Crtc, p, v));
        }
        for (p, v) in connector_props(2, true) {
            out.push((AtomicObject::Connector, p, v));
        }
        out
    }

    /// The advertised set is the menu a compositor gets from
    /// `GETPLANE`/`OBJ_GETPROPERTIES`, and a property left out of it does not
    /// exist as far as userspace is concerned -- staging it still works, so
    /// nothing else in this module would notice. Pin the whole set.
    #[test]
    fn each_object_advertises_the_whole_set_of_properties_it_can_stage() {
        let _serialised = drm::test_globals::lock();

        let mut plane: alloc::vec::Vec<u32> = plane_props(&a_plane(), true)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        plane.sort_unstable();
        let mut want = alloc::vec![
            PROP_TYPE,
            PROP_FB_ID,
            PROP_CRTC_ID,
            PROP_CRTC_X,
            PROP_CRTC_Y,
            PROP_CRTC_W,
            PROP_CRTC_H,
            PROP_SRC_X,
            PROP_SRC_Y,
            PROP_SRC_W,
            PROP_SRC_H,
            PROP_IN_FENCE_FD,
            PROP_FB_DAMAGE_CLIPS,
        ];
        want.sort_unstable();
        assert_eq!(plane, want, "the plane's property menu changed");

        let mut crtc: alloc::vec::Vec<u32> = crtc_props(true).into_iter().map(|(p, _)| p).collect();
        crtc.sort_unstable();
        let mut want = alloc::vec![PROP_ACTIVE, PROP_MODE_ID, PROP_OUT_FENCE_PTR];
        want.sort_unstable();
        assert_eq!(crtc, want, "the CRTC's property menu changed");

        // EDID is only advertised when a display actually has one, which is
        // never on the host; everything else on the connector is fixed.
        let mut conn: alloc::vec::Vec<u32> = connector_props(2, true)
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| *p != PROP_EDID)
            .collect();
        conn.sort_unstable();
        let mut want = alloc::vec![PROP_DPMS, PROP_LINK_STATUS, PROP_NON_DESKTOP, PROP_CRTC_ID,];
        want.sort_unstable();
        assert_eq!(conn, want, "the connector's property menu changed");
    }

    /// Linux hides atomic properties from a client that never set
    /// `DRM_CLIENT_CAP_ATOMIC` (`drm_mode_object_get_properties` skips
    /// `DRM_MODE_PROP_ATOMIC`). Showing them to a legacy client -- X11, or
    /// anything driving the pipeline through SETCRTC -- invites it to set
    /// properties the legacy path never reads back.
    #[test]
    fn a_non_atomic_client_is_shown_no_atomic_properties() {
        let _serialised = drm::test_globals::lock();
        let legacy = plane_props(&a_plane(), false)
            .into_iter()
            .chain(crtc_props(false))
            .chain(connector_props(2, false));
        for (prop_id, _) in legacy {
            let s = spec(prop_id);
            assert_eq!(
                s.flags & DRM_MODE_PROP_ATOMIC,
                0,
                "{} is an atomic property and was shown to a legacy client",
                s.name,
            );
        }
    }

    #[test]
    fn every_property_an_object_advertises_is_described_by_the_table() {
        let _serialised = drm::test_globals::lock();
        for (obj, prop_id, _) in advertised() {
            assert!(
                prop_spec(prop_id).is_some(),
                "{:?} advertises property {} that GETPROPERTY cannot describe",
                obj,
                prop_id,
            );
        }
    }

    /// The invariant that ties the three lists together. An object advertises
    /// a property, so a commit naming it must not be told it does not exist:
    /// either it stages, or it is refused as unsettable. ENOENT is reserved
    /// for a property the object really does not have.
    ///
    /// This is also where the tree used to diverge from Linux. Linux's
    /// `drm_mode_atomic_ioctl` looks the property up *first* and only then
    /// refuses an immutable one, so EDID / link-status / non-desktop on a
    /// connector are EINVAL there; here they fell through to ENOENT, which
    /// reads to a compositor as the property having disappeared between the
    /// enumeration and the commit.
    #[test]
    fn a_property_an_object_advertises_is_never_answered_enoent() {
        let _serialised = drm::test_globals::lock();
        for (obj, prop_id, value) in advertised() {
            let mut upd = drm::AtomicUpdate::default();
            let got = atomic_stage_on(&mut upd, obj, prop_id, value);
            assert_ne!(
                got,
                Err(FsError::EntryNotFound),
                "{:?} advertises property {} and then denies having it",
                obj,
                prop_id,
            );
        }
    }

    /// The immutable connector properties, whose EINVAL the test above only
    /// sees when a display is attached to advertise them.
    #[test]
    fn the_immutable_connector_properties_are_refused_not_disowned() {
        for prop_id in [PROP_EDID, PROP_LINK_STATUS, PROP_NON_DESKTOP, PROP_DPMS] {
            let mut upd = drm::AtomicUpdate::default();
            assert_eq!(
                atomic_stage_on(&mut upd, AtomicObject::Connector, prop_id, 0),
                Err(FsError::InvalidParam),
                "property {} must be refused as unsettable, not as unknown",
                prop_id,
            );
        }
    }

    /// Every property the spec marks ATOMIC has to reach a staging arm on the
    /// object that advertises it, and no other object may take it. Staging
    /// `SRC_W` on a CRTC would silently write the plane's field.
    #[test]
    fn an_atomic_property_stages_on_its_own_object_and_nowhere_else() {
        let plane = [
            PROP_FB_ID,
            PROP_CRTC_ID,
            PROP_CRTC_X,
            PROP_CRTC_Y,
            PROP_CRTC_W,
            PROP_CRTC_H,
            PROP_SRC_X,
            PROP_SRC_Y,
            PROP_SRC_W,
            PROP_SRC_H,
            PROP_IN_FENCE_FD,
            PROP_FB_DAMAGE_CLIPS,
        ];
        let crtc = [PROP_ACTIVE, PROP_MODE_ID, PROP_OUT_FENCE_PTR];
        let connector = [PROP_CRTC_ID];

        for (obj, own) in [
            (AtomicObject::Plane, &plane[..]),
            (AtomicObject::Crtc, &crtc[..]),
            (AtomicObject::Connector, &connector[..]),
        ] {
            for prop_id in own {
                let s = spec(*prop_id);
                assert_ne!(
                    s.flags & DRM_MODE_PROP_ATOMIC,
                    0,
                    "{} is staged in an atomic commit but not advertised as ATOMIC",
                    s.name,
                );
                let mut upd = drm::AtomicUpdate::default();
                // A value every one of them accepts: 0 is "none"/"off"
                // everywhere, and IN_FENCE_FD reads it as fd 0.
                assert_eq!(
                    atomic_stage_on(&mut upd, obj, *prop_id, 0),
                    Ok(()),
                    "{:?} cannot stage its own property {}",
                    obj,
                    s.name,
                );
            }
            // And the ones that belong to somebody else are unknown here.
            for (other_id, other_name) in ALL_PROPS {
                if own.contains(other_id) {
                    continue;
                }
                let mut upd = drm::AtomicUpdate::default();
                let got = atomic_stage_on(&mut upd, obj, *other_id, 0);
                assert_ne!(
                    got,
                    Ok(()),
                    "{:?} accepted {}, which is not its property",
                    obj,
                    other_name,
                );
            }
        }
    }

    /// An object id that names nothing is ENOENT, and it is the *object*
    /// lookup that says so: with no display attached no id resolves, which is
    /// what makes the rest of this module able to run at all.
    #[test]
    fn an_object_id_that_names_nothing_is_enoent() {
        let _serialised = drm::test_globals::lock();
        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(atomic_object(0xDEAD_BEEF), None);
        assert_eq!(
            atomic_stage(&mut upd, 0xDEAD_BEEF, PROP_FB_ID, 0),
            Err(FsError::EntryNotFound),
        );
    }

    /// "type" is IMMUTABLE. Accepting it would let a client turn the primary
    /// plane into a cursor for the rest of the session.
    #[test]
    fn the_immutable_plane_type_is_refused() {
        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(
            atomic_stage_on(&mut upd, AtomicObject::Plane, PROP_TYPE, 2),
            Err(FsError::InvalidParam),
        );
        assert_ne!(
            spec(PROP_TYPE).flags & DRM_MODE_PROP_IMMUTABLE,
            0,
            "the refusal above is only right because the property is immutable",
        );
    }

    /// ACTIVE is advertised as `[0, 1]`, so the guard has to hold that line:
    /// `Some(value != 0)` would read 2 as "on" and quietly accept a value the
    /// property says is out of range.
    #[test]
    fn active_takes_only_the_two_values_it_advertises() {
        for (value, want_on) in [(0u64, false), (1, true)] {
            let mut upd = drm::AtomicUpdate::default();
            assert_eq!(
                atomic_stage_on(&mut upd, AtomicObject::Crtc, PROP_ACTIVE, value),
                Ok(()),
            );
            assert_eq!(upd.active, Some(want_on));
        }
        for value in [2u64, 3, u64::MAX] {
            let mut upd = drm::AtomicUpdate::default();
            assert_eq!(
                atomic_stage_on(&mut upd, AtomicObject::Crtc, PROP_ACTIVE, value),
                Err(FsError::InvalidParam),
                "ACTIVE={} is outside the advertised [0, 1]",
                value,
            );
            assert_eq!(upd.active, None, "a refused value must not be staged");
        }
    }

    /// IN_FENCE_FD is a SIGNED_RANGE whose -1 means "no fence". Anything below
    /// that is a bad fd, and anything at or above 0 is a real one to wait on.
    /// The sentinel must not be staged: `Some(-1)` would send the commit
    /// looking for fd -1 in the caller's table.
    #[test]
    fn the_in_fence_sentinel_is_accepted_without_being_staged() {
        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(
            atomic_stage_on(
                &mut upd,
                AtomicObject::Plane,
                PROP_IN_FENCE_FD,
                -1i64 as u64
            ),
            Ok(()),
        );
        assert_eq!(upd.in_fence_fd, None, "-1 means no fence, not fd -1");

        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(
            atomic_stage_on(&mut upd, AtomicObject::Plane, PROP_IN_FENCE_FD, 7),
            Ok(()),
        );
        assert_eq!(upd.in_fence_fd, Some(7));

        for bad in [-2i64, -1000, i32::MIN as i64] {
            let mut upd = drm::AtomicUpdate::default();
            assert_eq!(
                atomic_stage_on(&mut upd, AtomicObject::Plane, PROP_IN_FENCE_FD, bad as u64,),
                Err(FsError::InvalidParam),
                "fd {} is below the -1 sentinel",
                bad,
            );
        }
    }

    /// OUT_FENCE_PTR is a userspace pointer the kernel writes an i32 into. A
    /// null one is "no out-fence wanted" and is staged as-is; a non-null one
    /// goes through `access_ok()` before anything is written to it, which is
    /// what stops a client aiming the write at kernel memory.
    #[test]
    fn a_null_out_fence_pointer_is_staged_and_a_bad_one_is_refused() {
        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(
            atomic_stage_on(&mut upd, AtomicObject::Crtc, PROP_OUT_FENCE_PTR, 0),
            Ok(()),
        );
        assert_eq!(upd.out_fence_ptr, Some(0));

        // A real address of the right size passes the check.
        let slot = 0i32;
        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(
            atomic_stage_on(
                &mut upd,
                AtomicObject::Crtc,
                PROP_OUT_FENCE_PTR,
                &slot as *const i32 as u64,
            ),
            Ok(()),
        );
        assert_eq!(upd.out_fence_ptr, Some(&slot as *const i32 as u64));
    }
}

#[cfg(test)]
mod atomic_walk_tests {
    //! The walk over a `DRM_IOCTL_MODE_ATOMIC` request's property arrays.
    //!
    //! Its shape is the uAPI's and it is easy to get subtly wrong:
    //! `props_ptr` and `prop_values_ptr` are **one** pair of arrays shared by
    //! every object in the request, and `count_props_ptr[i]` says how many of
    //! them object `i` takes, carrying on from where the previous object
    //! stopped. Nothing states their total length, so each object's span is
    //! what gets checked against the user mapping.
    //!
    //! Until this walk was pulled out there were two copies of it: the ioctl
    //! arm that stages the commit, and the fence scan that runs first to find
    //! the IN_FENCE_FD to sleep on. Two readers of the same arrays is a
    //! standing invitation for one to see a property the other does not.
    //!
    //! One thing here cannot be tested from the host, and it is worth saying
    //! so rather than leaving someone to wonder: the `access_ok()` on each
    //! object's span always passes. On libos there is no user/kernel split,
    //! so `user_range_ok` only refuses a null pointer with bytes to move,
    //! and that case is already caught by the explicit check above it. Taking
    //! both span checks out leaves every test here green. What *is* covered
    //! is the arithmetic that feeds them: the running index, the two counts
    //! that bound it, and `ucheck_n`'s refusal to wrap.

    use super::*;
    use alloc::vec::Vec;

    /// Back a request with real arrays. Everything stays alive as long as the
    /// `Request` does, which is what makes the raw pointers inside it sound.
    struct Request {
        objs: Vec<u32>,
        counts: Vec<u32>,
        props: Vec<u32>,
        values: Vec<u64>,
    }

    impl Request {
        fn new(objs: &[u32], counts: &[u32], props: &[u32], values: &[u64]) -> Self {
            Self {
                objs: objs.to_vec(),
                counts: counts.to_vec(),
                props: props.to_vec(),
                values: values.to_vec(),
            }
        }

        fn req(&self) -> DrmModeAtomic {
            DrmModeAtomic {
                flags: 0,
                count_objs: self.objs.len() as u32,
                objs_ptr: self.objs.as_ptr() as u64,
                count_props_ptr: self.counts.as_ptr() as u64,
                props_ptr: self.props.as_ptr() as u64,
                prop_values_ptr: self.values.as_ptr() as u64,
                reserved: 0,
                user_data: 0,
            }
        }

        fn visited(&self) -> Result<Vec<(u32, u32, u64)>> {
            let mut seen = Vec::new();
            walk_atomic_props(&self.req(), |o, p, v| {
                seen.push((o, p, v));
                Ok(())
            })?;
            Ok(seen)
        }
    }

    #[test]
    fn a_request_is_visited_object_by_object_in_order() {
        let r = Request::new(
            &[4, 1],
            &[2, 1],
            &[PROP_FB_ID, PROP_CRTC_ID, PROP_ACTIVE],
            &[7, 1, 1],
        );
        assert_eq!(
            r.visited().unwrap(),
            alloc::vec![
                (4, PROP_FB_ID, 7),
                (4, PROP_CRTC_ID, 1),
                (1, PROP_ACTIVE, 1),
            ],
        );
    }

    /// The one that bites. The shared arrays are indexed by a *running*
    /// counter, not restarted per object: the second object's properties
    /// begin where the first object's ended. Restarting at 0 would hand
    /// object 1 object 0's properties -- a commit that looks well-formed and
    /// programs the wrong thing.
    #[test]
    fn the_shared_arrays_run_on_from_one_object_to_the_next() {
        let r = Request::new(
            &[4, 1, 2],
            &[2, 1, 1],
            &[PROP_SRC_W, PROP_SRC_H, PROP_ACTIVE, PROP_CRTC_ID],
            &[100, 200, 1, 1],
        );
        let seen = r.visited().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(
            seen[2],
            (1, PROP_ACTIVE, 1),
            "the CRTC got the plane's properties: the index restarted",
        );
        assert_eq!(seen[3], (2, PROP_CRTC_ID, 1));
    }

    /// An object may name no properties at all, and then it consumes none of
    /// the shared arrays -- the next object still starts where the last one
    /// that had properties stopped.
    #[test]
    fn an_object_with_no_properties_consumes_none_of_the_shared_arrays() {
        let r = Request::new(&[4, 1, 2], &[1, 0, 1], &[PROP_FB_ID, PROP_CRTC_ID], &[7, 1]);
        assert_eq!(
            r.visited().unwrap(),
            alloc::vec![(4, PROP_FB_ID, 7), (2, PROP_CRTC_ID, 1)],
        );
    }

    #[test]
    fn an_empty_request_visits_nothing() {
        let r = Request::new(&[], &[], &[], &[]);
        assert_eq!(r.visited().unwrap(), Vec::new());
    }

    /// Both counts are bounded before anything is read. The pipeline has three
    /// objects with sixteen properties between them; the bound is well above
    /// that and its job is to keep a hostile request from walking for a long
    /// time inside the kernel.
    #[test]
    fn the_two_counts_are_bounded() {
        let objs: Vec<u32> = (0..65).collect();
        let counts: Vec<u32> = alloc::vec![0; 65];
        let r = Request::new(&objs, &counts, &[], &[]);
        assert_eq!(r.visited(), Err(FsError::InvalidParam), "65 objects");

        let objs: Vec<u32> = (0..64).collect();
        let counts: Vec<u32> = alloc::vec![0; 64];
        let r = Request::new(&objs, &counts, &[], &[]);
        assert!(r.visited().is_ok(), "64 objects is the last accepted");

        let props: Vec<u32> = alloc::vec![PROP_ACTIVE; 65];
        let values: Vec<u64> = alloc::vec![0; 65];
        let r = Request::new(&[1], &[65], &props, &values);
        assert_eq!(
            r.visited(),
            Err(FsError::InvalidParam),
            "65 properties on one object",
        );
        let r = Request::new(&[1], &[64], &props, &values);
        assert_eq!(
            r.visited().map(|v| v.len()),
            Ok(64),
            "64 on one object is the last accepted",
        );
    }

    /// A request that claims properties but hands no array to read them from.
    /// Claiming none is fine: that is how a client names an object without
    /// changing anything on it.
    #[test]
    fn an_absent_array_is_only_an_error_when_there_is_something_to_read() {
        let r = Request::new(&[4], &[1], &[PROP_FB_ID], &[7]);

        let mut req = r.req();
        req.props_ptr = 0;
        assert_eq!(
            walk_atomic_props(&req, |_, _, _| Ok(())),
            Err(FsError::InvalidParam),
        );

        let mut req = r.req();
        req.prop_values_ptr = 0;
        assert_eq!(
            walk_atomic_props(&req, |_, _, _| Ok(())),
            Err(FsError::InvalidParam),
        );

        // Same request with nothing claimed: the arrays are never touched.
        let empty = Request::new(&[4], &[0], &[], &[]);
        let mut req = empty.req();
        req.props_ptr = 0;
        req.prop_values_ptr = 0;
        assert_eq!(walk_atomic_props(&req, |_, _, _| Ok(())), Ok(()));

        // The object list itself is not optional.
        let mut req = r.req();
        req.objs_ptr = 0;
        assert_eq!(
            walk_atomic_props(&req, |_, _, _| Ok(())),
            Err(FsError::InvalidParam),
        );
        let mut req = r.req();
        req.count_props_ptr = 0;
        assert_eq!(
            walk_atomic_props(&req, |_, _, _| Ok(())),
            Err(FsError::InvalidParam),
        );
    }

    /// A property the pipeline refuses stops the commit there and then. The
    /// walk must not carry on staging the rest: a partly-applied atomic commit
    /// is the one thing the atomic uAPI promises cannot happen.
    #[test]
    fn a_refused_property_stops_the_walk_where_it_failed() {
        // One object claiming three properties, the middle one immutable.
        let r = Request::new(
            &[4],
            &[3],
            &[PROP_FB_ID, PROP_TYPE, PROP_CRTC_ID],
            &[7, 1, 1],
        );
        let mut seen = Vec::new();
        let got = walk_atomic_props(&r.req(), |o, p, v| {
            seen.push((o, p, v));
            // `type` is immutable, exactly as `atomic_stage_on` says.
            if p == PROP_TYPE {
                return Err(FsError::InvalidParam);
            }
            Ok(())
        });
        assert_eq!(got, Err(FsError::InvalidParam));
        assert_eq!(seen.len(), 2, "the third property was read anyway");
    }

    /// The reason the walk is shared. The fence scan runs before the commit
    /// and picks the IN_FENCE_FD to sleep on; the commit then stages it. When
    /// a request names the property twice, both have to land on the same one,
    /// or the commit sleeps on a fence it will not use.
    #[test]
    fn the_fence_scan_and_the_staging_pick_the_same_in_fence() {
        // Both readings of the same request: `atomic_in_fence`'s fold, and
        // what the ioctl arm ends up staging.
        let both = |values: &[u64]| -> (Option<i32>, Option<i32>) {
            let props = alloc::vec![PROP_IN_FENCE_FD; values.len()];
            let counts = alloc::vec![1u32; values.len()];
            let objs = alloc::vec![4u32; values.len()];
            let r = Request::new(&objs, &counts, &props, values);

            let mut fd: Option<i32> = None;
            walk_atomic_props(&r.req(), |_, prop_id, value| {
                fold_in_fence(&mut fd, prop_id, value);
                Ok(())
            })
            .unwrap();

            let mut upd = drm::AtomicUpdate::default();
            walk_atomic_props(&r.req(), |_, prop_id, value| {
                atomic_stage_on(&mut upd, AtomicObject::Plane, prop_id, value)
            })
            .unwrap();
            (fd, upd.in_fence_fd)
        };

        for (values, want) in [
            (alloc::vec![3u64, 9], Some(9)),            // last one wins
            (alloc::vec![3u64, -1i64 as u64], Some(3)), // the sentinel keeps it
            (alloc::vec![0u64], Some(0)),               // fd 0 is a real fd
            (alloc::vec![-1i64 as u64], None),          // only the sentinel
        ] {
            let (scanned, staged) = both(&values);
            assert_eq!(scanned, want, "the fence scan read {:?} wrong", values);
            assert_eq!(
                staged, scanned,
                "the commit stages a different fence than it sleeps on",
            );
        }
    }

    /// `ucheck_n` multiplies a userspace count by a struct size. The bounds
    /// above keep that far from overflowing, but the helper is the guard for
    /// every nested array in this file, and some of those counts are not
    /// bounded at all.
    #[test]
    fn a_count_that_overflows_its_byte_size_is_refused_not_wrapped() {
        let buf = [0u64; 4];
        let addr = buf.as_ptr() as usize;
        assert_eq!(ucheck_n::<u64>(addr, 4), Ok(()));
        // 2^61 u64s is 2^64 bytes: wrapping would make this a zero-length
        // range, which `user_range_ok` waves through.
        assert_eq!(
            ucheck_n::<u64>(addr, 1usize << 61),
            Err(FsError::InvalidParam),
        );
        assert_eq!(
            ucheck_n::<u64>(addr, usize::MAX),
            Err(FsError::InvalidParam)
        );
        // A zero-length range is fine from anywhere, a non-empty one is not
        // from a null pointer.
        assert_eq!(ucheck(0, 0), Ok(()));
        assert_eq!(ucheck(0, 1), Err(FsError::BadAddress));
    }

    /// The walk's own bounds are what keep the multiply above out of reach:
    /// 64 objects x 64 properties x 8 bytes is nowhere near `usize`.
    #[test]
    fn the_walks_bounds_keep_the_span_far_from_overflowing() {
        let widest = 64usize * 64 * core::mem::size_of::<u64>();
        assert!(widest < 1 << 20, "the span a request can ask for grew");
    }
}

#[cfg(test)]
mod syncobj_wait_routing_tests {
    //! Which commands take the async sleep path, and the mmap cookie.
    //!
    //! `sys_ioctl` asks `is_syncobj_wait_ioctl` whether to park the caller
    //! before running the sync arm. When it says no, the sync arm spin-polls
    //! the whole timeout and pegs a core -- the starvation `WAIT_VBLANK` used
    //! to cause. So this router has to recognise every wait the dispatcher
    //! will accept, and the dispatcher resolves ioctls by NUMBER.
    //!
    //! It used to match four exact 32-bit commands instead, which carry the
    //! struct size. Both wait structs already grew once (the 2023
    //! `deadline_nsec`), and the next libdrm to append a field would have sent
    //! a command the dispatcher handles and this router does not: the wait
    //! would have worked, at the cost of a core spinning for its whole
    //! timeout, with nothing in any log to say why.
    //!
    //! What is *not* covered, so nobody reads more into these than is there:
    //! the two places that ask `is_syncobj_timeline_wait` which struct to read
    //! -- the async sleeper and the dispatch arm -- both need a live device
    //! and `nouveau_uapi_enabled()`, so inverting either one's answer leaves
    //! this module green. What the tests pin is the rule itself, and that both
    //! callers now ask the same one instead of spelling it out twice.

    use super::*;

    fn wait_cmd(nr: u32, size: usize) -> u32 {
        drm_iowr_core(nr, size)
    }

    const CLASSIC: usize = core::mem::size_of::<DrmSyncobjWait>();
    const TIMELINE: usize = core::mem::size_of::<DrmSyncobjTimelineWait>();

    /// The two sizes in the wild today, named so a change to either is loud.
    #[test]
    fn the_two_sizes_libdrm_sends_today_are_both_waits() {
        for cmd in [
            DRM_IOCTL_SYNCOBJ_WAIT,
            DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE,
            DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
            DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE,
        ] {
            assert!(
                is_syncobj_wait_ioctl(cmd),
                "{:#x} is a wait and must take the async path",
                cmd,
            );
        }
        assert_eq!(CLASSIC, 32, "drm_syncobj_wait grew");
        assert_eq!(TIMELINE, 40, "drm_syncobj_timeline_wait grew");
    }

    /// The one that was broken. A struct that grows again keeps working
    /// through the dispatcher, which matches on the number, so the router has
    /// to follow it there.
    #[test]
    fn a_struct_that_grows_again_is_still_a_wait() {
        for extra in [8, 16, 24, 64, 1000] {
            let classic = wait_cmd(NR_SYNCOBJ_WAIT, CLASSIC + extra);
            assert!(
                is_syncobj_wait_ioctl(classic),
                "a {}-byte drm_syncobj_wait stopped being a wait",
                CLASSIC + extra,
            );
            assert!(!is_syncobj_timeline_wait(classic), "and it is not timeline");

            let timeline = wait_cmd(NR_SYNCOBJ_TIMELINE_WAIT, TIMELINE + extra);
            assert!(
                is_syncobj_wait_ioctl(timeline),
                "a {}-byte drm_syncobj_timeline_wait stopped being a wait",
                TIMELINE + extra,
            );
            assert!(is_syncobj_timeline_wait(timeline));
        }
    }

    /// The floor is the async path's own requirement, not the dispatcher's.
    /// The sleeper reads the struct **in place** in user memory, so it can
    /// only run once the client has actually sent a whole one; a short request
    /// still reaches the sync arm, which copies it into a zero-filled kernel
    /// buffer and is safe with it. Saying so here because the asymmetry looks
    /// like an oversight otherwise.
    #[test]
    fn a_request_too_short_to_read_in_place_is_left_to_the_sync_arm() {
        for size in [0, 1, CLASSIC - 1] {
            assert!(!is_syncobj_wait_ioctl(wait_cmd(NR_SYNCOBJ_WAIT, size)));
        }
        assert!(is_syncobj_wait_ioctl(wait_cmd(NR_SYNCOBJ_WAIT, CLASSIC)));

        for size in [0, CLASSIC, TIMELINE - 1] {
            assert!(!is_syncobj_timeline_wait(wait_cmd(
                NR_SYNCOBJ_TIMELINE_WAIT,
                size
            )));
        }
        assert!(is_syncobj_timeline_wait(wait_cmd(
            NR_SYNCOBJ_TIMELINE_WAIT,
            TIMELINE
        )));
    }

    /// The NUMBER decides which struct the sleeper reads, and it must decide
    /// it the same way the dispatch arm does. Reading a timeline request as a
    /// classic one takes `count_handles` and `flags` from the wrong offsets.
    #[test]
    fn the_number_decides_the_struct_not_the_size() {
        // A timeline-sized classic wait is still classic.
        let odd = wait_cmd(NR_SYNCOBJ_WAIT, TIMELINE);
        assert!(is_syncobj_wait_ioctl(odd));
        assert!(!is_syncobj_timeline_wait(odd));
        // And the canonical commands the dispatch arm sees agree.
        assert!(!is_syncobj_timeline_wait(DRM_IOCTL_SYNCOBJ_WAIT));
        assert!(is_syncobj_timeline_wait(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT));
        assert!(!is_syncobj_timeline_wait(DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE));
        assert!(is_syncobj_timeline_wait(
            DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE
        ));
    }

    /// Everything else stays off the sleep path. The neighbouring syncobj
    /// numbers are the ones that would hurt: RESET and SIGNAL carry a
    /// different struct entirely, and parking on one would read it wrong.
    #[test]
    fn nothing_but_the_two_waits_takes_the_sleep_path() {
        for cmd in [
            DRM_IOCTL_SYNCOBJ_CREATE,
            DRM_IOCTL_SYNCOBJ_DESTROY,
            DRM_IOCTL_SYNCOBJ_RESET,
            DRM_IOCTL_SYNCOBJ_SIGNAL,
            DRM_IOCTL_SYNCOBJ_QUERY,
            DRM_IOCTL_SYNCOBJ_TRANSFER,
            DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
        ] {
            assert!(!is_syncobj_wait_ioctl(cmd), "{:#x} is not a wait", cmd);
        }
        // And the type byte still has to be DRM's, whatever the number says.
        let not_drm = (3u32 << 30) | (0x65 << 8) | NR_SYNCOBJ_WAIT | ((CLASSIC as u32) << 16);
        assert!(!is_syncobj_wait_ioctl(not_drm));
    }

    /// `MAP_DUMB` hands userspace `handle << 12` as a fake file offset and
    /// `get_vmo` shifts it back. The two live far apart in this file and are
    /// the only thing standing between a client's `mmap()` and the right
    /// buffer, so pin the round trip.
    #[test]
    fn the_mmap_cookie_round_trips_for_every_handle() {
        for handle in [1u32, 2, 0xFF, 0x1234, 0x000F_FFFF, 0x8000_0000, u32::MAX] {
            let offset = mmap_cookie_for(handle);
            assert_eq!(
                offset & 0xFFF,
                0,
                "musl rejects a non-page-aligned mmap offset before the syscall",
            );
            assert_eq!(
                handle_from_mmap_cookie(offset as usize),
                handle,
                "handle {} does not survive the cookie",
                handle,
            );
        }
    }

    /// And the limit of that encoding, written down rather than discovered.
    /// The decode truncates to 32 bits, so offsets above `u32::MAX << 12`
    /// alias onto a handle. It is not a way in -- both lookups behind it check
    /// the caller owns the handle -- but it is a surprise worth naming.
    #[test]
    fn an_offset_above_the_handle_space_aliases_rather_than_failing() {
        let aliased = ((1u64 << 32) | 5) << 12;
        assert_eq!(handle_from_mmap_cookie(aliased as usize), 5);
    }
}

#[cfg(test)]
mod version_tests {
    //! `drm_version` and `drm_getunique` copy their strings with `strlen`
    //! bytes and no terminator (`drm_copy_field`: cut to the room, length
    //! written back in full; `drm_getunique`: only when the whole of it
    //! fits), and `drm_setversion` reports the driver's own version, the
    //! pair VERSION reports, and refuses a request past it. Here the three
    //! strings carried the NUL in both the copy and the length, GET_UNIQUE
    //! copied a prefix, and SET_VERSION said driver 1.0 on a nouveau node
    //! (VERSION: 1.4) while reading only the requested major.
    use super::gl_client_sequence_tests::Client;
    use super::*;

    fn zeroed<T>() -> T {
        // SAFETY: integers and raw pointers, for which zero is a value.
        unsafe { core::mem::zeroed() }
    }

    struct NouveauOn(bool);
    impl NouveauOn {
        fn new(on: bool) -> Self {
            let was = zcore_drivers::display::nouveau_uapi_enabled();
            zcore_drivers::display::set_nouveau_uapi_enabled(on);
            NouveauOn(was)
        }
    }
    impl Drop for NouveauOn {
        fn drop(&mut self) {
            zcore_drivers::display::set_nouveau_uapi_enabled(self.0);
        }
    }

    #[test]
    fn version_and_unique_count_and_copy_their_strings_like_linux() {
        let _serialised = drm::test_globals::lock();
        let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        let _nouveau = NouveauOn::new(false);
        let c = Client::open(0);
        const SENTINEL: u8 = 0xEE;

        // The count call: lengths without the terminator.
        let mut v: DrmVersion = zeroed();
        assert_eq!(c.ioctl(DRM_IOCTL_VERSION, &mut v), Ok(0));
        assert_eq!(
            (v.version_major, v.version_minor, v.version_patchlevel),
            (1, 0, 0)
        );
        assert_eq!(v.name_len, "zcore".len());
        assert_eq!(v.date_len, "20260503".len());
        assert_eq!(v.desc_len, "zCore DRM Driver".len());

        // Exact room: the bytes, no NUL after them.
        let mut name = [SENTINEL; 6];
        let mut desc = [SENTINEL; 17];
        let mut fill: DrmVersion = zeroed();
        fill.name = name.as_mut_ptr();
        fill.name_len = 5;
        fill.desc = desc.as_mut_ptr();
        fill.desc_len = 16;
        assert_eq!(c.ioctl(DRM_IOCTL_VERSION, &mut fill), Ok(0));
        assert_eq!(&name[..5], b"zcore");
        assert_eq!(name[5], SENTINEL, "a NUL was written past the room");
        assert_eq!(&desc[..16], b"zCore DRM Driver");
        assert_eq!(desc[16], SENTINEL);
        assert_eq!((fill.name_len, fill.desc_len), (5, 16));

        // Less room than the string: a prefix, and the full length back.
        let mut short = [SENTINEL; 4];
        let mut fill: DrmVersion = zeroed();
        fill.name = short.as_mut_ptr();
        fill.name_len = 3;
        assert_eq!(c.ioctl(DRM_IOCTL_VERSION, &mut fill), Ok(0));
        assert_eq!(&short[..3], b"zco");
        assert_eq!(short[3], SENTINEL);
        assert_eq!(fill.name_len, 5);

        // GET_UNIQUE: nothing for a buffer the bus id does not fit in, the
        // whole of it and no NUL when it does, `strlen` back both times.
        let mut u: DrmUnique = zeroed();
        assert_eq!(c.ioctl(DRM_IOCTL_GET_UNIQUE, &mut u), Ok(0));
        assert_eq!(u.unique_len, "zcore-gpu".len());
        let mut buf = [SENTINEL; 10];
        let mut u: DrmUnique = zeroed();
        u.unique = buf.as_mut_ptr();
        u.unique_len = 8;
        assert_eq!(c.ioctl(DRM_IOCTL_GET_UNIQUE, &mut u), Ok(0));
        assert_eq!(buf, [SENTINEL; 10], "a prefix of the bus id was copied");
        assert_eq!(u.unique_len, 9);
        u.unique_len = 9;
        assert_eq!(c.ioctl(DRM_IOCTL_GET_UNIQUE, &mut u), Ok(0));
        assert_eq!(&buf[..9], b"zcore-gpu");
        assert_eq!(buf[9], SENTINEL);
        assert_eq!(u.unique_len, 9);
    }

    #[test]
    fn set_version_reports_the_driver_version_of_the_node_and_checks_the_minor() {
        let _serialised = drm::test_globals::lock();
        let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        let einval = || Err(FsError::InvalidParam);
        let query = |c: &Client, di: (i32, i32), dd: (i32, i32)| {
            let mut sv = DrmSetVersion {
                drm_di_major: di.0,
                drm_di_minor: di.1,
                drm_dd_major: dd.0,
                drm_dd_minor: dd.1,
            };
            let r = c.ioctl(DRM_IOCTL_SET_VERSION, &mut sv);
            (r, (sv.drm_dd_major, sv.drm_dd_minor))
        };
        for (nouveau, dd) in [(false, (1, 0)), (true, (1, 4))] {
            let _nouveau = NouveauOn::new(nouveau);
            let c = Client::open(0);
            // VERSION and SET_VERSION name the same driver version.
            let mut v: DrmVersion = zeroed();
            assert_eq!(c.ioctl(DRM_IOCTL_VERSION, &mut v), Ok(0));
            assert_eq!((v.version_major, v.version_minor), dd);
            assert_eq!(query(&c, (1, 4), (-1, 0)), (Ok(0), dd));
            // The interface: 1.0 to 1.4, nothing else.
            assert_eq!(query(&c, (1, 0), (-1, 0)).0, Ok(0));
            assert_eq!(query(&c, (1, 5), (-1, 0)).0, einval());
            assert_eq!(query(&c, (2, 0), (-1, 0)).0, einval());
            // The driver: its major, and a minor up to its own. The answer
            // is written back on a refusal too.
            assert_eq!(query(&c, (-1, 0), dd), (Ok(0), dd));
            assert_eq!(query(&c, (-1, 0), (dd.0, 0)), (Ok(0), dd));
            assert_eq!(query(&c, (-1, 0), (dd.0, dd.1 + 1)), (einval(), dd));
            assert_eq!(query(&c, (-1, 0), (dd.0, -1)), (einval(), dd));
            assert_eq!(query(&c, (-1, 0), (dd.0 + 1, 0)), (einval(), dd));
        }
    }
}
#[cfg(test)]
mod master_tests {
    //! `drm_auth.c`: one master per node, and the magic handshake.

    use super::gl_client_sequence_tests::Client;
    use super::*;

    fn set_master(c: &Client) -> Result<usize> {
        c.ioctl(DRM_IOCTL_SET_MASTER, &mut 0u8)
    }
    fn drop_master(c: &Client) -> Result<usize> {
        c.ioctl(DRM_IOCTL_DROP_MASTER, &mut 0u8)
    }
    fn magic(c: &Client) -> u32 {
        let mut magic = 0u32;
        c.ioctl(DRM_IOCTL_GET_MAGIC, &mut magic).expect("GET_MAGIC");
        magic
    }
    fn auth(c: &Client, magic: u32) -> Result<usize> {
        let mut magic = magic;
        c.ioctl(DRM_IOCTL_AUTH_MAGIC, &mut magic)
    }

    /// `drm_master_open` / `drm_setmaster_ioctl` / `drm_dropmaster_ioctl`:
    /// the first open of a node is its master; a second open's SET_MASTER is
    /// EBUSY while the first holds it and its DROP_MASTER is EINVAL; the
    /// master's own SET_MASTER is a no-op; once dropped, or once the holding
    /// file closes, the next SET_MASTER takes it. Nothing was recorded, so
    /// every SET_MASTER and every DROP_MASTER succeeded for everyone.
    #[test]
    fn one_open_holds_the_master_until_it_drops_it_or_closes() {
        let _serialised = drm::test_globals::lock();
        // A node of its own: minor 0's master is whichever test opened it.
        let first = Client::open(77);
        let second = Client::open(77);
        assert_eq!(
            set_master(&second),
            Err(FsError::Busy),
            "held by the first open"
        );
        assert_eq!(set_master(&first), Ok(0), "the master again: a no-op");
        assert_eq!(
            drop_master(&second),
            Err(FsError::InvalidParam),
            "not the master"
        );
        assert_eq!(drop_master(&first), Ok(0));
        assert_eq!(
            drop_master(&first),
            Err(FsError::InvalidParam),
            "already dropped"
        );
        assert_eq!(set_master(&second), Ok(0), "free, so taken");
        assert_eq!(
            set_master(&first),
            Err(FsError::Busy),
            "and now held by the second"
        );
        drop(second);
        assert_eq!(
            set_master(&first),
            Ok(0),
            "released with the file that held it"
        );
    }

    /// `drm_getmagic` / `drm_authmagic`: a magic is minted per file, once;
    /// only the master authenticates (EACCES), only a magic a file of this
    /// node holds (EINVAL), and a magic is spent by the AUTH_MAGIC that
    /// names it or by its file closing. Every file was told magic 1 and
    /// every AUTH_MAGIC from anyone, of anything, was "authenticated".
    #[test]
    fn a_magic_is_minted_per_file_and_only_the_master_spends_it() {
        let _serialised = drm::test_globals::lock();
        let master = Client::open(78);
        let client = Client::open(78);
        let m_client = magic(&client);
        assert_ne!(m_client, 0);
        assert_eq!(magic(&client), m_client, "the same file, the same magic");
        assert_ne!(magic(&master), m_client, "another file, another magic");

        assert_eq!(
            auth(&client, m_client),
            Err(FsError::NoPermission),
            "not the master"
        );
        assert_eq!(
            auth(&master, m_client + 1000),
            Err(FsError::InvalidParam),
            "never minted"
        );
        assert_eq!(auth(&master, m_client), Ok(0));
        assert_eq!(auth(&master, m_client), Err(FsError::InvalidParam), "spent");

        let elsewhere = Client::open(79);
        assert_eq!(
            auth(&master, magic(&elsewhere)),
            Err(FsError::InvalidParam),
            "minted on another node"
        );
        let closing = Client::open(78);
        let m_closing = magic(&closing);
        drop(closing);
        assert_eq!(
            auth(&master, m_closing),
            Err(FsError::InvalidParam),
            "died with its file"
        );
    }

    /// libdrm `drmIsMaster()`: AUTH_MAGIC(0). Linux answers EINVAL when the
    /// caller is master and EACCES otherwise. Success would also pass the
    /// probe, but EINVAL is the ABI, and it is what the einval-hunt was
    /// reporting as a fault at compositor start.
    #[test]
    fn auth_magic_zero_is_einval_from_the_master_and_eacces_from_anyone_else() {
        let _serialised = drm::test_globals::lock();
        let master = Client::open(80);
        let client = Client::open(80);
        assert_eq!(auth(&master, 0), Err(FsError::InvalidParam));
        assert_eq!(auth(&client, 0), Err(FsError::NoPermission));
    }

    /// Linux keeps magics on `drm_device`, so `card{n}` and `renderD{128+n}`
    /// share the map. GET_MAGIC on the render node is EACCES (not
    /// DRM_RENDER_ALLOW); a mint on that file must still be spendable from
    /// the card, which is how a DRI2 client that opened the render node
    /// gets authenticated by the compositor on `card0`.
    #[test]
    fn a_magic_minted_on_the_render_node_authenticates_on_the_card() {
        let _serialised = drm::test_globals::lock();
        let card = Client::open(81);
        let render = Client::open(drm::RENDER_MINOR_BASE + 81);
        let mut probe = 0u32;
        assert_eq!(
            render.ioctl(DRM_IOCTL_GET_MAGIC, &mut probe),
            Err(FsError::NoPermission),
            "GET_MAGIC is not DRM_RENDER_ALLOW"
        );
        let minted = render.file_state().magic();
        assert_ne!(minted, 0);
        assert_eq!(auth(&card, minted), Ok(0));
        assert_eq!(auth(&card, minted), Err(FsError::InvalidParam), "spent");
    }
}

#[cfg(test)]
mod blob_id_space_tests {
    //! The property-blob id space, which has three tenants and no referee.
    //!
    //! `GETPROPBLOB` takes an id and nothing else -- libdrm identifies a blob
    //! purely by id -- and resolves it against the blob store first, then the
    //! range reserved for connector EDIDs. The synthetic KMS objects and the
    //! framebuffers number from 1 upwards in the same space.
    //!
    //! Until now the only thing keeping the three apart was a comment and the
    //! literal `20000` written out at both ends of the EDID encoding, in two
    //! files. Now the bases are named, the encoding is one pair of functions,
    //! and the gap between them is a compile-time assertion.

    use super::gl_client_sequence_tests::Client;
    use super::*;

    #[test]
    fn an_edid_blob_id_round_trips_for_every_connector() {
        for connector in [0u32, 1, 2, 3, 16, 255, 4096] {
            let id = edid_blob_id(connector);
            assert_eq!(
                connector_of_edid_blob(id),
                Some(connector),
                "connector {} does not survive its blob id",
                connector,
            );
        }
    }

    /// The store's ids and the EDID range must not meet. They do not today by
    /// a margin of ten thousand, and that margin is the number of connectors
    /// the encoding can name -- far more than a machine has, but write it
    /// down, because the failure would be a connector's EDID silently shadowed
    /// by somebody's MODE_ID blob.
    #[test]
    fn the_edid_range_ends_before_the_blob_store_begins() {
        // The gap itself is a `const _: () = assert!(...)` beside the
        // constant, so it is a build error rather than a test failure. What is
        // left here is that the decoder honours it.
        let last = drm::BLOB_ID_BASE - EDID_BLOB_BASE - 1;
        assert_eq!(connector_of_edid_blob(edid_blob_id(last)), Some(last));
        // One past the end belongs to the store, not to a connector.
        assert_eq!(connector_of_edid_blob(drm::BLOB_ID_BASE), None);
        assert_eq!(connector_of_edid_blob(drm::BLOB_ID_BASE + 1), None);
        assert_eq!(connector_of_edid_blob(u32::MAX), None);
    }

    /// And nothing below the range is an EDID either: the synthetic KMS object
    /// ids and the framebuffer ids live down there.
    #[test]
    fn the_low_ids_belong_to_objects_and_framebuffers() {
        for id in [
            0,
            drm::SYNTH_CRTC_ID,
            drm::SYNTH_ENCODER_ID,
            drm::SYNTH_PLANE_ID,
            1000,
            EDID_BLOB_BASE - 1,
        ] {
            assert_eq!(
                connector_of_edid_blob(id),
                None,
                "id {} is not an EDID blob",
                id,
            );
        }
        assert_eq!(connector_of_edid_blob(EDID_BLOB_BASE), Some(0));
    }

    /// Ids the store hands out are unique, land where they are supposed to,
    /// and keep landing there after a destroy -- an id is never reused, which
    /// is what stops a client that freed a blob from reading a later one
    /// through the same number.
    #[test]
    fn the_store_numbers_its_blobs_above_the_reserved_range_and_never_reuses_one() {
        let _serialised = drm::test_globals::lock();
        let a = drm::create_blob(alloc::vec![1u8, 2, 3], true);
        let b = drm::create_blob(alloc::vec![4u8], true);
        assert!(
            a >= drm::BLOB_ID_BASE,
            "blob {} is inside the EDID range",
            a
        );
        assert!(b > a, "ids must not repeat");
        assert_eq!(drm::get_blob(a).as_deref(), Some(&[1u8, 2, 3][..]));

        assert!(matches!(
            drm::destroy_blob(a, 0),
            drm::BlobDestroy::Destroyed
        ));
        assert_eq!(drm::get_blob(a), None);
        let c = drm::create_blob(alloc::vec![5u8], true);
        assert!(c > b, "a freed id came back: {} after {}", c, b);

        assert!(matches!(
            drm::destroy_blob(b, 0),
            drm::BlobDestroy::Destroyed
        ));
        assert!(matches!(
            drm::destroy_blob(c, 0),
            drm::BlobDestroy::Destroyed
        ));
    }

    /// Linux splits `DESTROYPROPBLOB`'s refusals: ENOENT for an id that names
    /// nothing, EPERM for a blob the caller did not create. The kernel's own
    /// current-mode blob is the second case, and a client that could free it
    /// would take `MODE_ID` readback down with it.
    #[test]
    fn only_the_creator_may_destroy_a_blob() {
        let _serialised = drm::test_globals::lock();
        let kernel = drm::create_blob(alloc::vec![0u8; 68], false);
        assert!(
            matches!(drm::destroy_blob(kernel, 0), drm::BlobDestroy::KernelOwned),
            "a kernel-owned blob must answer EPERM, not vanish",
        );
        assert!(
            drm::get_blob(kernel).is_some(),
            "and it must still be there afterwards",
        );

        assert!(matches!(
            drm::destroy_blob(drm::BLOB_ID_BASE - 1, 0),
            drm::BlobDestroy::NotFound
        ));
        assert!(matches!(
            drm::destroy_blob(edid_blob_id(2), 0),
            drm::BlobDestroy::NotFound
        ));
    }
    /// `drm_mode_destroyblob_ioctl` frees a blob only for the file that
    /// created it ("ensure the property was actually created by this user",
    /// EPERM otherwise), and `drm_release` frees what a file leaves behind.
    /// Any client could destroy any other's -- the compositor's MODE_ID blob
    /// going away under it makes its next commit fail with ENOENT -- and a
    /// closed client's blobs were kept for ever.
    #[test]
    fn a_blob_belongs_to_the_file_that_created_it_and_dies_with_it() {
        let _serialised = drm::test_globals::lock();
        let owner = Client::open(0);
        let other = Client::open(0);
        let create = |c: &Client| {
            let bytes = [7u8; 68];
            let mut blob = DrmModeCreateBlob {
                data: bytes.as_ptr() as u64,
                length: bytes.len() as u32,
                blob_id: 0,
            };
            c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
                .expect("CREATEPROPBLOB");
            blob.blob_id
        };
        let destroy = |c: &Client, id: u32| {
            let mut id = id;
            c.ioctl(DRM_IOCTL_MODE_DESTROYPROPBLOB, &mut id)
        };

        let id = create(&owner);
        assert_eq!(
            destroy(&other, id),
            Err(FsError::NotPermitted),
            "another file's blob"
        );
        // Readable by anyone: the lookup has no owner check.
        let mut get = DrmModeGetBlob {
            blob_id: id,
            length: 0,
            data: 0,
        };
        assert_eq!(other.ioctl(DRM_IOCTL_MODE_GETPROPBLOB, &mut get), Ok(0));
        assert_eq!(get.length, 68);
        assert_eq!(destroy(&owner, id), Ok(0));
        assert_eq!(destroy(&owner, id), Err(FsError::EntryNotFound), "twice");

        let kernel = drm::create_blob(alloc::vec![0u8; 68], false);
        assert_eq!(
            destroy(&owner, kernel),
            Err(FsError::NotPermitted),
            "the kernel's own"
        );

        let left_behind = create(&owner);
        let kept = create(&other);
        drop(owner);
        assert!(
            drm::get_blob(left_behind).is_none(),
            "a closed file's blob outlived it"
        );
        assert!(
            drm::get_blob(kept).is_some(),
            "and took a stranger's with it"
        );
        assert_eq!(destroy(&other, left_behind), Err(FsError::EntryNotFound));
        assert_eq!(destroy(&other, kept), Ok(0));
    }
}

/// CREATE_DUMB bpp, chardev write, ADDFB errno, and DESTROYPROPBLOB EPERM —
/// small contracts that used to lie to clients.
#[cfg(test)]
mod dumb_write_and_addfb_errno_tests {
    use super::gl_client_sequence_tests::Client;
    use super::*;
    use crate::error::LxError;

    #[test]
    fn destroypropblob_of_a_kernel_blob_is_eperm_to_userspace() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let kernel = drm::create_blob(alloc::vec![0u8; 68], false);
        let mut id = kernel;
        assert_eq!(
            client.ioctl(DRM_IOCTL_MODE_DESTROYPROPBLOB, &mut id),
            Err(FsError::NotPermitted)
        );
        assert_eq!(LxError::from(FsError::NotPermitted), LxError::EPERM);
        assert!(drm::get_blob(kernel).is_some());
    }

    #[test]
    fn get_client_zero_is_self_and_one_is_enoent() {
        let c = Client::open(0);
        let mut client = DrmClient {
            idx: 0,
            auth: 0,
            pid: 0,
            uid: 0,
            magic: 0,
            iocs: 0,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_GET_CLIENT, &mut client), Ok(0));
        assert_eq!(client.auth, 1);
        assert_eq!(client.magic, 0);
        client.idx = 1;
        assert_eq!(
            c.ioctl(DRM_IOCTL_GET_CLIENT, &mut client),
            Err(FsError::EntryNotFound)
        );
    }

    #[test]
    fn dirtyfb_rejects_unknown_flags() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        let buf = c.create_dumb(16, 16);
        let fb = c.addfb2(&buf);
        let mut cmd = DrmModeFbDirtyCmd {
            fb_id: fb,
            flags: 1 << 3,
            color: 0,
            num_clips: 0,
            clips_ptr: 0,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_DIRTYFB, &mut cmd),
            Err(FsError::InvalidParam)
        );
        assert_eq!(c.rmfb(fb), Ok(0));
        assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
    }

    #[test]
    fn create_dumb_oom_is_enomem_not_enospc() {
        assert_eq!(LxError::from(FsError::NoMemory), LxError::ENOMEM);
        assert_ne!(LxError::from(FsError::NoMemory), LxError::ENOSPC);
    }

    #[test]
    fn create_dumb_refuses_bpp_zero_and_honours_bpp_sixteen() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        let mut zero = DrmModeCreateDumb {
            height: 16,
            width: 16,
            bpp: 0,
            flags: 0,
            handle: 0,
            pitch: 0,
            size: 0,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut zero),
            Err(FsError::InvalidParam)
        );
        let mut bpp16 = DrmModeCreateDumb {
            height: 16,
            width: 16,
            bpp: 16,
            flags: 0,
            handle: 0,
            pitch: 0,
            size: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut bpp16)
            .expect("CREATE_DUMB bpp=16");
        // pitch = round_up(width * bpp/8, 64) = round_up(32, 64) = 64
        assert_eq!(bpp16.pitch, 64, "bpp=16 must not be inflated to 32");
        assert_eq!(bpp16.size, 64 * 16);
        assert_eq!(c.destroy_dumb(bpp16.handle), Ok(0));
    }

    #[test]
    fn a_drm_chardev_write_is_einval() {
        let dev = DrmDev::new(0);
        assert_eq!(dev.write_at(0, b"x"), Err(FsError::InvalidParam));
        assert_eq!(LxError::from(FsError::InvalidParam), LxError::EINVAL);
    }

    #[test]
    fn addfb2_of_an_unknown_handle_is_enoent() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        let mut cmd = DrmModeFbCmd2 {
            fb_id: 0,
            width: 16,
            height: 16,
            pixel_format: drm::DRM_FORMAT_XRGB8888,
            flags: 0,
            handles: [0xDEAD_u32, 0, 0, 0],
            pitches: [64, 0, 0, 0],
            offsets: [0; 4],
            modifier: [0; 4],
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd),
            Err(FsError::EntryNotFound)
        );
        assert_eq!(LxError::from(FsError::EntryNotFound), LxError::ENOENT);
        assert_eq!(cmd.fb_id, 0);
    }
}

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
mod compute_node_tests {
    use super::gl_client_sequence_tests::Client;
    use super::*;
    use crate::fs::devfs::kms_emu::{self, EmuGpu};

    /// `_IOWR('d', DRM_ECLIPSE_COMPUTE_NR, size)` -- the command word a client
    /// builds, with the encoded size under the test's control.
    fn compute_cmd(size: u32) -> u32 {
        IOC_WRITE_DIR | IOC_READ_DIR | (size << 16) | (b'd' as u32) << 8 | DRM_ECLIPSE_COMPUTE_NR
    }

    /// A request with recognisable contents, so "was not written" is a real
    /// assertion rather than "happens to be zero".
    fn blank_request() -> DrmEclipseCompute {
        DrmEclipseCompute {
            op: 0,
            status: 0x5A5A_5A5A,
            elapsed_ns: 0x5A5A_5A5A_5A5A_5A5A,
            grid_threads: 0x5A5A_5A5A,
            reserved: 0,
            summary: [0x5A; 512],
        }
    }

    fn summary_str(req: &DrmEclipseCompute) -> alloc::string::String {
        let end = req.summary.iter().position(|&b| b == 0).unwrap_or(0);
        alloc::string::String::from_utf8_lossy(&req.summary[..end]).into_owned()
    }

    /// A client that encodes a struct smaller than the kernel's is refused
    /// outright. The handler writes 536 bytes at the address it is given, and
    /// the only bound anything checked was the size in the client's own command
    /// word, so accepting a short encoding is half a kilobyte written past the
    /// end of the caller's buffer -- from an unprivileged ioctl.
    #[test]
    fn a_short_size_encoding_is_refused_instead_of_writing_past_the_caller() {
        let _screen = kms_emu::headless();
        let c = Client::open(0);
        let mut req = blank_request();

        let err = c
            .ioctl(compute_cmd(8), &mut req)
            .expect_err("a short encoding must not reach the handler");
        assert_eq!(err, FsError::InvalidParam);
        // And it was refused BEFORE anything was written.
        assert_eq!(req.status, 0x5A5A_5A5A, "the handler ran anyway");
        assert!(
            req.summary.iter().all(|&b| b == 0x5A),
            "the summary was written"
        );
    }

    /// One byte short is still short. The floor is `<`, and an off-by-one here
    /// is the whole bug it exists to prevent.
    #[test]
    fn an_encoding_one_byte_short_is_still_refused() {
        let _screen = kms_emu::headless();
        let c = Client::open(0);
        let mut req = blank_request();
        let exact = core::mem::size_of::<DrmEclipseCompute>() as u32;

        assert_eq!(
            c.ioctl(compute_cmd(exact - 1), &mut req),
            Err(FsError::InvalidParam)
        );
        // The exact size is accepted, so the refusal above is the size check
        // and not the command being unknown.
        assert_eq!(c.ioctl(compute_cmd(exact), &mut req), Ok(0));
    }

    /// With no GPU at all the answer is in-band: the ioctl SUCCEEDS and the
    /// status field carries `-ENODEV`. Returning an ioctl error instead would
    /// be indistinguishable from "this kernel has no compute ioctl", which is
    /// what a probing client falls back on.
    #[test]
    fn a_node_with_no_gpu_answers_enodev_in_band_not_with_an_ioctl_error() {
        let _screen = kms_emu::headless();
        let c = Client::open(0);
        let mut req = blank_request();

        assert_eq!(
            c.ioctl(
                compute_cmd(core::mem::size_of::<DrmEclipseCompute>() as u32),
                &mut req
            ),
            Ok(0)
        );
        assert_eq!(
            req.status, -19,
            "-ENODEV belongs in the reply, not in errno"
        );
        assert_eq!(summary_str(&req), "no compute GPU");
    }

    /// With a driver present the driver's own answer is passed through --
    /// including its refusal. `EmuGpu` does not implement `compute_launch`, so
    /// it gives the trait default, which is exactly what a driver that has not
    /// wired compute up returns on real hardware.
    #[test]
    fn a_driver_without_compute_support_answers_with_its_own_status() {
        let screen = kms_emu::headless();
        let _gpu = screen.attach_gpu(EmuGpu::new("emu-gpu"));
        let c = Client::open(0);
        let mut req = blank_request();

        assert_eq!(
            c.ioctl(
                compute_cmd(core::mem::size_of::<DrmEclipseCompute>() as u32),
                &mut req
            ),
            Ok(0)
        );
        assert_eq!(
            req.status, -38,
            "-ENOSYS from the driver, not the core's -ENODEV"
        );
        assert_ne!(summary_str(&req), "no compute GPU");
        assert!(
            !summary_str(&req).is_empty(),
            "the driver's report was dropped"
        );
        // The reply's other fields are always written, so a client cannot read
        // a previous launch's numbers.
        assert_eq!(req.elapsed_ns, 0);
        assert_eq!(req.grid_threads, 0);
    }

    /// The summary is always NUL-terminated, even when the driver's report is
    /// longer than the field. It is read by C as a string, so a report that
    /// filled all 512 bytes would run off the end of the struct.
    #[test]
    fn the_summary_is_nul_terminated_even_when_the_report_overflows_it() {
        let mut dst = [0xFFu8; 512];
        let long = alloc::string::String::from_utf8(alloc::vec![b'x'; 600]).unwrap();
        fill_summary(&mut dst, &long);
        assert_eq!(dst[511], 0, "no room left for the terminator");
        assert!(dst[..511].iter().all(|&b| b == b'x'));

        // And a short report clears what a previous, longer one left behind.
        fill_summary(&mut dst, "ok");
        assert_eq!(&dst[..3], b"ok\0");
        assert!(dst[3..].iter().all(|&b| b == 0), "stale bytes survived");
    }
}

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
mod event_queue_tests {
    use super::gl_client_sequence_tests::{parse_events, Client, FLIP_COMPLETE};
    use super::*;
    use crate::fs::devfs::kms_emu;

    /// Put one flip completion on `c`'s queue. The caller holds the attached
    /// [`kms_emu::Screen`] the present needs.
    fn queue_one_flip(c: &Client, user_data: u64) -> (u32, u32) {
        let buf = c.create_dumb(32, 8);
        let fb = c.addfb2(&buf);
        c.page_flip(drm::SYNTH_CRTC_ID, fb, user_data)
            .expect("flip");
        drm::flush_pending_flip_completions();
        (fb, buf.handle)
    }

    /// An empty queue is `EAGAIN`, not a short read and not `EINVAL`. A
    /// compositor that gets anything else on the very common "poll woke me for
    /// something else" path treats the card fd as broken and tears the output
    /// down.
    #[test]
    fn an_empty_queue_reads_eagain_and_not_a_broken_fd() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let mut buf = [0u8; 32];

        assert_eq!(c.read_events(&mut buf), Err(FsError::Again));
        // And nothing was written into the buffer.
        assert!(buf.iter().all(|&b| b == 0));
    }

    /// A buffer too small for one event reads 0 bytes, as `drm_read()` puts
    /// the event back and returns what it had read so far, and the event
    /// STAYS queued. `EAGAIN` here is a livelock (the queue is non-empty, so
    /// the file is still readable and the wait resolves instantly, over and
    /// over), dropping the event instead would lose the flip completion
    /// wlroots is waiting on -- a desktop frozen on its current frame -- and
    /// `EINVAL`, which this answered, is a failed read to `drmHandleEvent`
    /// where Linux hands it "nothing this time". And a write to the card fd
    /// is EINVAL (the DRM file operations have no `.write`); this took the
    /// bytes and said they were written.
    #[test]
    fn a_buffer_too_small_for_one_event_reads_nothing_and_keeps_the_event() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let (fb, handle) = queue_one_flip(&c, 0xABCD);

        assert_eq!(c.write(b"not a DRM event"), Err(FsError::InvalidParam));

        let mut small = [0u8; 16];
        assert_eq!(c.read_events(&mut small), Ok(0));
        assert!(
            small.iter().all(|&b| b == 0),
            "a partial event was delivered"
        );

        // Still there, and still whole.
        let mut full = [0u8; 32];
        assert_eq!(c.read_events(&mut full).expect("the event survived"), 32);
        let ev = parse_events(&full);
        assert_eq!(ev[0].ev_type, FLIP_COMPLETE);
        assert_eq!(ev[0].user_data, 0xABCD);
        // Drained now.
        assert_eq!(c.read_events(&mut full), Err(FsError::Again));

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(handle).expect("DESTROY_DUMB");
    }

    /// One client's completion is not readable by another. The queue lives on
    /// the open file, like Linux's `struct drm_file`: with a single device-wide
    /// stream, a probing client (Xwayland during session bring-up) reads the
    /// compositor's flip completion out from under it and wlroots then waits on
    /// an event that has already been consumed.
    #[test]
    fn one_clients_flip_completion_is_not_readable_by_another() {
        let _screen = kms_emu::attach(32, 8);
        let flipper = Client::open(0);
        let bystander = Client::open(0);
        let (fb, handle) = queue_one_flip(&flipper, 0x1234);

        let mut buf = [0u8; 32];
        assert_eq!(
            bystander.read_events(&mut buf),
            Err(FsError::Again),
            "another open file drained the completion"
        );
        assert!(!bystander.poll().expect("poll").read);

        // And the client that asked for it still has it.
        assert_eq!(flipper.read_events(&mut buf).expect("own completion"), 32);
        assert_eq!(parse_events(&buf)[0].user_data, 0x1234);

        flipper.rmfb(fb).expect("RMFB");
        flipper.destroy_dumb(handle).expect("DESTROY_DUMB");
    }

    /// `poll` says readable exactly while an event is queued. It is what parks
    /// the compositor's main loop: stuck at readable burns a core in a spin, and
    /// stuck at not-readable is a desktop that never sees its own flip land.
    #[test]
    fn poll_reports_readable_only_while_an_event_is_queued() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        assert!(
            !c.poll().expect("poll").read,
            "readable with an empty queue"
        );

        let (fb, handle) = queue_one_flip(&c, 1);
        assert!(
            c.poll().expect("poll").read,
            "not readable with an event queued"
        );

        // A short read, which takes nothing, must not clear it either.
        let mut small = [0u8; 8];
        assert_eq!(c.read_events(&mut small), Ok(0));
        assert!(
            c.poll().expect("poll").read,
            "a short read consumed the event"
        );

        let mut full = [0u8; 32];
        assert_eq!(c.read_events(&mut full).expect("drain"), 32);
        assert!(!c.poll().expect("poll").read, "readable after the drain");
        // Linux drm_poll never reports POLLOUT on the chardev.
        assert!(!c.poll().expect("poll").write);

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(handle).expect("DESTROY_DUMB");
    }

    /// Several queued events come out one read at a time, in order, and a
    /// buffer big enough for two takes two. A reader that got them out of order
    /// would mis-pair completions with the frames that asked for them.
    #[test]
    fn queued_events_come_out_in_order_and_a_big_buffer_takes_several() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let buf = c.create_dumb(32, 8);
        let fb = c.addfb2(&buf);

        // Two frames, each completion collected... by a reader that waits until
        // both are in, which is what a compositor doing two outputs looks like.
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x11).expect("flip 1");
        drm::flush_pending_flip_completions();
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x22).expect("flip 2");
        drm::flush_pending_flip_completions();

        let mut both = [0u8; 64];
        assert_eq!(c.read_events(&mut both).expect("two events"), 64);
        let ev = parse_events(&both);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].user_data, 0x11, "the events came out reversed");
        assert_eq!(ev[1].user_data, 0x22);
        assert_eq!(
            ev[0].length, 32,
            "the wire length must match the reader's stride"
        );

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }
}

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
mod out_fence_tests {
    use super::gl_client_sequence_tests::Client;
    use super::*;
    use crate::fs::devfs::kms_emu;
    use alloc::vec::Vec;

    /// A value that is neither a valid fd nor `-1`, so "was written" and "was
    /// left alone" are distinguishable.
    const UNWRITTEN: i32 = 0x5A5A_5A5A;

    /// The property arrays of one atomic request, kept alive for as long as the
    /// request points at them.
    pub(super) struct Request {
        objs: Vec<u32>,
        counts: Vec<u32>,
        props: Vec<u32>,
        values: Vec<u64>,
    }

    impl Request {
        pub(super) fn new(objs: &[u32], counts: &[u32], props: &[u32], values: &[u64]) -> Request {
            Request {
                objs: objs.to_vec(),
                counts: counts.to_vec(),
                props: props.to_vec(),
                values: values.to_vec(),
            }
        }

        fn ioctl(&self, flags: u32) -> DrmModeAtomic {
            DrmModeAtomic {
                flags,
                count_objs: self.objs.len() as u32,
                objs_ptr: self.objs.as_ptr() as u64,
                count_props_ptr: self.counts.as_ptr() as u64,
                props_ptr: self.props.as_ptr() as u64,
                prop_values_ptr: self.values.as_ptr() as u64,
                reserved: 0,
                user_data: 0,
            }
        }
    }

    /// An atomic client on an output, with the cmdline flag on. Returns the
    /// screen (which holds the test lock and puts the flag back on Drop) and the
    /// client.
    pub(super) fn atomic_client(width: u32, height: u32) -> (kms_emu::Screen, Client) {
        let screen = kms_emu::attach(width, height);
        drm::set_atomic_enabled(true);
        let c = Client::open(0);
        let mut cap: [u64; 2] = [DRM_CLIENT_CAP_ATOMIC, 1];
        c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
            .expect("SET_CLIENT_CAP ATOMIC");
        (screen, c)
    }

    /// A commit the check phase accepts as is: leaving the CRTC inactive needs
    /// no mode and no modeset flag. It is the baseline every refusal below is
    /// measured against, so a refusal cannot be the request's own fault.
    fn benign_request() -> Request {
        Request::new(&[drm::SYNTH_CRTC_ID], &[1], &[PROP_ACTIVE], &[0])
    }

    pub(super) fn commit(c: &Client, req: &Request, flags: u32) -> Result<usize> {
        let mut ioctl = req.ioctl(flags);
        c.ioctl(DRM_IOCTL_MODE_ATOMIC, &mut ioctl)
    }

    /// Without the cap the ioctl is refused, and the cap itself is refused
    /// unless the boot asked for it. That is the gate the whole arm sits behind
    /// -- and with it shut, the rest of this module is unreachable, which is why
    /// nothing tested it.
    #[test]
    fn the_atomic_ioctl_is_refused_until_the_boot_and_the_client_both_opt_in() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let mut cap: [u64; 2] = [DRM_CLIENT_CAP_ATOMIC, 1];

        // Flag off: the capability is EOPNOTSUPP, like a Linux driver without
        // DRIVER_ATOMIC, so a compositor falls back to legacy KMS.
        assert_eq!(
            c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap),
            Err(FsError::OpNotSupported)
        );
        let req = benign_request();
        assert_eq!(
            commit(&c, &req, 0),
            Err(FsError::InvalidParam),
            "a non-atomic client got an atomic commit"
        );

        // Flag on, cap negotiated: now it is reachable.
        drm::set_atomic_enabled(true);
        c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
            .expect("SET_CLIENT_CAP ATOMIC");
        assert!(commit(&c, &req, 0).is_ok());
    }

    /// A commit that FAILS still writes the out-fence slot, and writes `-1`.
    /// The writeback sits before the commit's error is mapped on purpose: the
    /// slot is the client's `int out_fence` local, and leaving it untouched
    /// means the client reads whatever was on its stack and then closes or waits
    /// on a descriptor that belongs to something else.
    #[test]
    fn a_failed_commit_still_writes_minus_one_into_the_out_fence_slot() {
        let (_screen, c) = atomic_client(32, 8);
        let mut slot: i32 = UNWRITTEN;
        // A plane given a framebuffer but no CRTC: every value is legal for
        // its property, so it stages fine and is refused by the commit's
        // check phase (`drm_atomic_plane_check`: "FB set but no CRTC").
        let buf = c.create_dumb(32, 8);
        let fb = c.addfb2(&buf);
        let req = Request::new(
            &[drm::SYNTH_CRTC_ID, drm::SYNTH_PLANE_ID],
            &[1, 2],
            &[PROP_OUT_FENCE_PTR, PROP_FB_ID, PROP_CRTC_ID],
            &[&mut slot as *mut i32 as u64, fb as u64, 0],
        );

        assert_eq!(
            commit(&c, &req, 0),
            Err(FsError::InvalidParam),
            "a framebuffer on a plane with no CRTC was accepted"
        );
        assert_eq!(slot, -1, "the client's fence slot was left uninitialised");
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// `drm_atomic_set_property` runs `drm_property_change_valid_get` on every
    /// value before anything is staged: an object id that names no
    /// framebuffer or CRTC, a blob id that names no blob, and a value outside
    /// the property's range are EINVAL at the property, and the out-fence
    /// slot is never touched because the commit never ran. Here a
    /// non-existent FB_ID, CRTC_ID and MODE_ID were staged and answered
    /// ENOENT by the commit, with `-1` written into the slot; and the values
    /// were truncated to the field's width instead of checked, so
    /// `FB_ID = fb | 1 << 32` presented `fb`, `IN_FENCE_FD = 1 << 32` waited
    /// on fd 0, `CRTC_W = 1 << 31` wrapped negative and `SRC_X = 1 << 32`
    /// became 0. A compositor that reads ENOENT retires the object; one whose
    /// bad value is accepted never learns it sent one.
    #[test]
    fn a_value_the_property_cannot_take_is_refused_at_the_property_not_by_the_commit() {
        let (_screen, c) = atomic_client(32, 8);
        let buf = c.create_dumb(32, 8);
        let fb = c.addfb2(&buf);
        const NO_SUCH: u64 = 0x999;
        let plane = drm::SYNTH_PLANE_ID;
        let crtc = drm::SYNTH_CRTC_ID;

        for (obj, prop, value, what) in [
            (
                plane,
                PROP_FB_ID,
                NO_SUCH,
                "a framebuffer that does not exist",
            ),
            (plane, PROP_CRTC_ID, NO_SUCH, "a CRTC that does not exist"),
            (
                crtc,
                PROP_MODE_ID,
                NO_SUCH,
                "a mode blob that does not exist",
            ),
            (
                plane,
                PROP_FB_DAMAGE_CLIPS,
                NO_SUCH,
                "a damage blob that does not exist",
            ),
            (
                plane,
                PROP_FB_ID,
                fb as u64 | 1 << 32,
                "a framebuffer id above 32 bits",
            ),
            (
                plane,
                PROP_CRTC_ID,
                crtc as u64 | 1 << 32,
                "a CRTC id above 32 bits",
            ),
            (
                plane,
                PROP_IN_FENCE_FD,
                1 << 32,
                "an in-fence fd above INT_MAX",
            ),
            (
                plane,
                PROP_IN_FENCE_FD,
                -2i64 as u64,
                "an in-fence fd below -1",
            ),
            (plane, PROP_CRTC_W, 1 << 31, "a CRTC_W above INT_MAX"),
            (plane, PROP_CRTC_H, u64::MAX, "a CRTC_H above INT_MAX"),
            (plane, PROP_SRC_X, 1 << 32, "a SRC_X above UINT_MAX"),
            (plane, PROP_SRC_W, 1 << 32, "a SRC_W above UINT_MAX"),
            (plane, PROP_CRTC_X, 1 << 31, "a CRTC_X above INT_MAX"),
            (crtc, PROP_ACTIVE, 2, "an ACTIVE that is neither 0 nor 1"),
        ] {
            let mut slot: i32 = UNWRITTEN;
            let req = Request::new(
                &[crtc, obj],
                &[1, 1],
                &[PROP_OUT_FENCE_PTR, prop],
                &[&mut slot as *mut i32 as u64, value],
            );
            assert_eq!(
                commit(&c, &req, DRM_MODE_ATOMIC_TEST_ONLY),
                Err(FsError::InvalidParam),
                "{} was not refused as an invalid value",
                what,
            );
            assert_eq!(
                slot, UNWRITTEN,
                "{}: the commit never ran, so the fence slot must be left alone",
                what,
            );
        }

        // A property some other object owns is ENOENT even with a value it
        // could never take, and so is one nobody has: `drm_mode_atomic_ioctl`
        // looks the property up on the object before anything is checked.
        for (obj, prop, value, what) in [
            (crtc, PROP_FB_ID, NO_SUCH, "FB_ID on the CRTC"),
            (plane, PROP_ACTIVE, 2, "ACTIVE on the plane"),
            (plane, 0xDEAD, NO_SUCH, "a property nobody has"),
        ] {
            let mut slot: i32 = UNWRITTEN;
            let req = Request::new(
                &[crtc, obj],
                &[1, 1],
                &[PROP_OUT_FENCE_PTR, prop],
                &[&mut slot as *mut i32 as u64, value],
            );
            assert_eq!(
                commit(&c, &req, DRM_MODE_ATOMIC_TEST_ONLY),
                Err(FsError::EntryNotFound),
                "{} was not refused as a property the object does not have",
                what,
            );
            assert_eq!(
                slot, UNWRITTEN,
                "{}: the fence slot must be left alone",
                what
            );
        }

        // The same properties with values they do take are staged: the
        // refusals above are the values, not the properties. FB_ID names the
        // real framebuffer and CRTC_ID the real CRTC, so the check phase
        // accepts the plane too.
        let mut slot: i32 = UNWRITTEN;
        let req = Request::new(
            &[crtc, plane],
            &[1, 11],
            &[
                PROP_OUT_FENCE_PTR,
                PROP_FB_ID,
                PROP_CRTC_ID,
                PROP_IN_FENCE_FD,
                PROP_CRTC_W,
                PROP_CRTC_H,
                PROP_SRC_X,
                PROP_SRC_W,
                PROP_CRTC_X,
                PROP_FB_DAMAGE_CLIPS,
                PROP_SRC_Y,
                PROP_SRC_H,
            ],
            &[
                &mut slot as *mut i32 as u64,
                fb as u64,
                crtc as u64,
                -1i64 as u64,
                32,
                8,
                0,
                32 << 16,
                0,
                0,
                0,
                8 << 16,
            ],
        );
        assert_eq!(commit(&c, &req, DRM_MODE_ATOMIC_TEST_ONLY), Ok(0));
        assert_eq!(slot, -1, "TEST_ONLY writes -1 into the slot");

        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }

    /// `TEST_ONLY` writes `-1` too: nothing was committed, so there is nothing
    /// to fence. Handing back a real (already signaled) fd here would leak one
    /// descriptor per `TEST_ONLY` probe, and wlroots probes on every output
    /// reconfiguration.
    #[test]
    fn a_test_only_commit_writes_minus_one_and_presents_nothing() {
        let (screen, c) = atomic_client(32, 8);
        let mut slot: i32 = UNWRITTEN;
        let req = Request::new(
            &[drm::SYNTH_CRTC_ID],
            &[2],
            &[PROP_OUT_FENCE_PTR, PROP_ACTIVE],
            &[&mut slot as *mut i32 as u64, 0],
        );

        commit(&c, &req, DRM_MODE_ATOMIC_TEST_ONLY).expect("TEST_ONLY commit");

        // What this pins is that the slot IS written, with `-1`. That it is `-1`
        // *rather than a real fd* is not separable here and no sharper test will
        // separate it: installing the signaled stub needs a current thread with
        // a Linux fd table, and a hosted test has neither, so
        // `try_signaled_out_fence_fd` returns `None` and the success leg writes
        // `-1` too. Swapping the two legs therefore survives this module by
        // construction; the leg that is checkable is checked above and in
        // `a_failed_commit_still_writes_minus_one_into_the_out_fence_slot`.
        assert_eq!(slot, -1, "TEST_ONLY did not write the fence slot");
        assert!(
            (0..8).all(|y| (0..32).all(|x| screen.pixel(x, y) == kms_emu::UNTOUCHED)),
            "a TEST_ONLY commit put pixels on the screen"
        );
    }

    /// A property the walk rejects aborts the commit BEFORE the fence is
    /// written. The client sees an error and its slot untouched, which is the
    /// one case where not writing is right: no commit was attempted, so there is
    /// no fence to describe -- and `-1` would look like "committed, no fence".
    #[test]
    fn a_rejected_property_aborts_before_the_fence_is_written() {
        let (_screen, c) = atomic_client(32, 8);
        let mut slot: i32 = UNWRITTEN;
        // The fence pointer is staged first, then an unknown property id.
        let req = Request::new(
            &[drm::SYNTH_CRTC_ID],
            &[2],
            &[PROP_OUT_FENCE_PTR, 0xDEAD],
            &[&mut slot as *mut i32 as u64, 0],
        );

        assert!(
            commit(&c, &req, 0).is_err(),
            "an unknown property was staged"
        );
        assert_eq!(
            slot, UNWRITTEN,
            "a commit that never ran handed the client a fence"
        );
    }

    /// A NULL out-fence pointer is legal and writes nothing. libdrm passes NULL
    /// whenever the caller did not ask for a fence, so faulting on it would
    /// refuse every ordinary commit.
    #[test]
    fn a_null_out_fence_pointer_is_accepted_and_writes_nothing() {
        let (_screen, c) = atomic_client(32, 8);
        let req = Request::new(
            &[drm::SYNTH_CRTC_ID],
            &[2],
            &[PROP_OUT_FENCE_PTR, PROP_ACTIVE],
            &[0, 0],
        );

        commit(&c, &req, 0).expect("a commit with no fence must be accepted");
    }

    /// The flag guards of the arm, all of which Linux enforces: an unknown flag,
    /// a non-zero `reserved`, an async flip (this tree advertises no async
    /// support) and `TEST_ONLY` carrying a flip event are each `EINVAL`. A
    /// kernel that quietly accepts a flag it does not implement is worse than
    /// one that refuses it: the client then waits for behaviour that never
    /// arrives.
    #[test]
    fn the_flag_guards_refuse_what_this_tree_does_not_implement() {
        let (_screen, c) = atomic_client(32, 8);
        let req = benign_request();

        let unknown = DRM_MODE_ATOMIC_FLAGS.wrapping_add(1) & !DRM_MODE_ATOMIC_FLAGS;
        assert_eq!(commit(&c, &req, unknown), Err(FsError::InvalidParam));
        assert_eq!(
            commit(&c, &req, DRM_MODE_PAGE_FLIP_ASYNC),
            Err(FsError::InvalidParam),
            "an async flip was accepted without async support"
        );
        assert_eq!(
            commit(
                &c,
                &req,
                DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_PAGE_FLIP_EVENT
            ),
            Err(FsError::InvalidParam),
            "a test commit was allowed to queue a flip event"
        );
        // `reserved` must be zero.
        let mut ioctl = req.ioctl(0);
        ioctl.reserved = 1;
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_ATOMIC, &mut ioctl),
            Err(FsError::InvalidParam)
        );
        // And the same request with none of that is fine, so the refusals above
        // are the flags and not the request.
        assert!(commit(&c, &req, 0).is_ok());
    }

    /// Turning a CRTC on is a modeset, and a modeset needs both the client's
    /// `ALLOW_MODESET` flag and a mode. Linux refuses `ACTIVE=1` without the
    /// flag ("[CRTC] requires full modeset") and again without a mode; a kernel
    /// that let either through would light a CRTC with no timings programmed,
    /// which on real hardware is a blank panel the compositor believes is up.
    #[test]
    fn activating_a_crtc_needs_both_the_modeset_flag_and_a_mode() {
        let (_screen, c) = atomic_client(32, 8);
        let req = Request::new(&[drm::SYNTH_CRTC_ID], &[1], &[PROP_ACTIVE], &[1]);

        assert_eq!(
            commit(&c, &req, 0),
            Err(FsError::InvalidParam),
            "a modeset went through without ALLOW_MODESET"
        );
        assert_eq!(
            commit(&c, &req, DRM_MODE_ATOMIC_ALLOW_MODESET),
            Err(FsError::InvalidParam),
            "a CRTC was activated with no mode set"
        );
        // Leaving it off is not a modeset, so the same property with the other
        // value needs neither.
        assert!(commit(&c, &benign_request(), 0).is_ok());
    }

    /// And with a mode in hand, the `ALLOW_MODESET` flag is still required on
    /// its own. Both guards refuse the same request with the same errno, so this
    /// is the only shape that tells them apart: a commit that carries a mode has
    /// nothing left to object to except the missing flag. Linux's rule is that a
    /// client which has not opted into modesetting never gets one -- wlroots
    /// relies on it to probe configurations without disturbing the screen.
    #[test]
    fn a_modeset_that_carries_a_mode_still_needs_the_allow_modeset_flag() {
        let (_screen, c) = atomic_client(32, 8);
        // The panel's own mode: anything else is refused for a different reason.
        let mode = make_modeinfo(32, 8);
        let mut blob = DrmModeCreateBlob {
            data: mode.as_ptr() as u64,
            length: mode.len() as u32,
            blob_id: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
            .expect("CREATEPROPBLOB");
        assert_ne!(blob.blob_id, 0, "the mode blob was not created");

        let req = Request::new(
            &[drm::SYNTH_CRTC_ID],
            &[2],
            &[PROP_MODE_ID, PROP_ACTIVE],
            &[u64::from(blob.blob_id), 1],
        );

        assert_eq!(
            commit(&c, &req, 0),
            Err(FsError::InvalidParam),
            "a modeset went through without ALLOW_MODESET"
        );
        commit(&c, &req, DRM_MODE_ATOMIC_ALLOW_MODESET)
            .expect("a modeset with the flag and a matching mode must be accepted");
    }

    /// A `MODE_ID` blob has to be exactly one `drm_mode_modeinfo` and has to
    /// name the mode the panel actually scans out. Neither is pedantry: the
    /// timings are read out of the blob by offset, so a short one reads past it,
    /// and a mode this tree cannot scan out is the "wlroots picked a mode we
    /// don't scan out" failure, where the commit succeeds and the screen stays
    /// black.
    ///
    /// Two of the three are defended twice, so do not go chasing a surviving
    /// mutation here: a short blob and a missing one both end up read as
    /// `0x0`, which the panel-mode comparison refuses anyway. Deleting either
    /// of those two checks on its own therefore keeps every assertion below
    /// green. What the test pins is the contract -- none of the three is ever
    /// accepted -- not which line does the refusing.
    #[test]
    fn a_mode_blob_must_be_one_modeinfo_and_name_the_panels_own_mode() {
        let (_screen, c) = atomic_client(32, 8);

        let new_blob = |bytes: &[u8]| -> u32 {
            let mut blob = DrmModeCreateBlob {
                data: bytes.as_ptr() as u64,
                length: bytes.len() as u32,
                blob_id: 0,
            };
            c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
                .expect("CREATEPROPBLOB");
            blob.blob_id
        };

        let short = new_blob(&[0u8; 32]);
        let wrong_size = new_blob(&make_modeinfo(64, 16));
        let unknown = 0x7FFF_FFFF;

        for (blob_id, what) in [
            (short, "a 32-byte blob"),
            (wrong_size, "a mode the panel does not have"),
            (unknown, "a blob that does not exist"),
        ] {
            let req = Request::new(
                &[drm::SYNTH_CRTC_ID],
                &[2],
                &[PROP_MODE_ID, PROP_ACTIVE],
                &[u64::from(blob_id), 1],
            );
            assert!(
                commit(&c, &req, DRM_MODE_ATOMIC_ALLOW_MODESET).is_err(),
                "{} was accepted as a mode",
                what
            );
        }
    }
}

#[cfg(test)]
mod empty_commit_tests {
    //! `ATOMIC` with `count_objs == 0`. Linux accepts an empty commit (there
    //! is nothing to check and nothing to apply) but refuses one that asks
    //! for a page-flip event: with no CRTC in the state there is nothing to
    //! signal it from, and `prepare_signaling` says so with EINVAL rather
    //! than let the client pend on an event that never comes. This arm
    //! answered 0 to both.
    use super::out_fence_tests::{atomic_client, commit, Request};
    use super::*;

    fn empty() -> Request {
        Request::new(&[], &[], &[], &[])
    }

    #[test]
    fn an_empty_commit_is_fine_unless_it_asks_for_an_event() {
        let (_screen, c) = atomic_client(32, 8);
        let req = empty();
        for flags in [
            0,
            DRM_MODE_ATOMIC_TEST_ONLY,
            DRM_MODE_ATOMIC_NONBLOCK,
            DRM_MODE_ATOMIC_ALLOW_MODESET,
            DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_ATOMIC_ALLOW_MODESET,
        ] {
            assert_eq!(commit(&c, &req, flags), Ok(0), "flags {flags:#x}");
        }
        for flags in [
            DRM_MODE_PAGE_FLIP_EVENT,
            DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK,
            DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_ALLOW_MODESET,
        ] {
            assert_eq!(
                commit(&c, &req, flags),
                Err(FsError::InvalidParam),
                "flags {flags:#x}: an event with nothing to signal it from"
            );
        }
        // Nothing was queued for the refused ones: the client reads no
        // event, now or after the vblank that would have carried one.
        drm::flush_pending_flip_completions();
        let mut buf = [0u8; 64];
        assert_eq!(
            c.read_events(&mut buf),
            Err(FsError::Again),
            "a refused commit still queued an event"
        );
        // The refusal is about the missing CRTC, not the flag: the same
        // event on a commit that names the CRTC (here, the one that turns it
        // on) is owed and delivered.
        let mode = make_modeinfo(32, 8);
        let mut blob = DrmModeCreateBlob {
            data: mode.as_ptr() as u64,
            length: mode.len() as u32,
            blob_id: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
            .expect("CREATEPROPBLOB");
        let on_crtc = Request::new(
            &[drm::SYNTH_CRTC_ID],
            &[2],
            &[PROP_MODE_ID, PROP_ACTIVE],
            &[u64::from(blob.blob_id), 1],
        );
        assert_eq!(
            commit(
                &c,
                &on_crtc,
                DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_ALLOW_MODESET
            ),
            Ok(0)
        );
        drm::flush_pending_flip_completions();
        assert_eq!(c.read_events(&mut buf).map(|n| n / 32), Ok(1));
    }
}

#[cfg(test)]
mod off_crtc_event_tests {
    //! `ATOMIC` with `PAGE_FLIP_EVENT` on a CRTC that is off and stays off.
    //! `drm_atomic_crtc_check` refuses it with EINVAL on purpose: a client
    //! asking to be woken for a frame on a suspended pipe is taken to have a
    //! bug in its frame loop, the same answer WAIT_VBLANK and the legacy page
    //! flip give on a disabled pipe. This scheduled the event anyway.
    use super::out_fence_tests::{atomic_client, commit, Request};
    use super::*;

    fn mode_blob(c: &super::gl_client_sequence_tests::Client) -> u32 {
        let mode = make_modeinfo(32, 8);
        let mut blob = DrmModeCreateBlob {
            data: mode.as_ptr() as u64,
            length: mode.len() as u32,
            blob_id: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
            .expect("CREATEPROPBLOB");
        blob.blob_id
    }

    fn active(on: u64) -> Request {
        Request::new(&[drm::SYNTH_CRTC_ID], &[1], &[PROP_ACTIVE], &[on])
    }

    /// `drm_mode_getcrtc` reads `crtc_state->enable`, which `MODE_ID` sets
    /// and unsets (`drm_atomic_set_mode_prop_for_crtc`): after a commit with
    /// `MODE_ID = 0` the CRTC answers `mode_valid = 0` and the encoder names
    /// no CRTC, until a commit sets a mode again; `ACTIVE = 0` on its own
    /// keeps the mode. Here `mode_valid` stayed 1 through all of it.
    #[test]
    fn a_commit_that_unsets_the_mode_leaves_the_crtc_with_none() {
        let (_screen, c) = atomic_client(32, 8);
        let blob = mode_blob(&c);
        let pipe = || {
            let mut crtc: DrmModeGetCrtc = unsafe { core::mem::zeroed() };
            crtc.crtc_id = drm::SYNTH_CRTC_ID;
            c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
            let mut enc: DrmModeGetEncoder = unsafe { core::mem::zeroed() };
            enc.encoder_id = drm::SYNTH_ENCODER_ID;
            c.ioctl(DRM_IOCTL_MODE_GETENCODER, &mut enc)
                .expect("GETENCODER");
            (crtc.mode_valid, enc.crtc_id)
        };
        let modeset = |mode: u64, on: u64| {
            Request::new(
                &[drm::SYNTH_CRTC_ID],
                &[2],
                &[PROP_MODE_ID, PROP_ACTIVE],
                &[mode, on],
            )
        };
        let on = (1, drm::SYNTH_CRTC_ID);
        let off = (0, 0);

        commit(&c, &modeset(blob as u64, 1), DRM_MODE_ATOMIC_ALLOW_MODESET)
            .expect("a modeset with the panel's mode");
        assert_eq!(pipe(), on, "with a mode set");
        commit(&c, &modeset(0, 0), DRM_MODE_ATOMIC_ALLOW_MODESET).expect("MODE_ID = 0");
        assert_eq!(pipe(), off, "after MODE_ID = 0");
        commit(&c, &modeset(blob as u64, 1), DRM_MODE_ATOMIC_ALLOW_MODESET)
            .expect("the mode again");
        assert_eq!(pipe(), on, "after the mode is set again");
        commit(&c, &active(0), DRM_MODE_ATOMIC_ALLOW_MODESET).expect("ACTIVE = 0");
        assert_eq!(pipe(), on, "ACTIVE = 0 keeps the mode");
    }

    /// One event read off the fd, or none.
    fn events(c: &super::gl_client_sequence_tests::Client) -> usize {
        drm::flush_pending_flip_completions();
        let mut buf = [0u8; 64];
        match c.read_events(&mut buf) {
            Ok(n) => n / 32,
            Err(FsError::Again) => 0,
            Err(e) => panic!("read: {:?}", e),
        }
    }

    #[test]
    fn an_event_on_a_crtc_that_is_off_and_stays_off_is_refused() {
        let (_screen, c) = atomic_client(32, 8);
        const EVENT: u32 = DRM_MODE_PAGE_FLIP_EVENT;
        const MODESET: u32 = DRM_MODE_ATOMIC_ALLOW_MODESET;

        // A fresh CRTC is off. Leaving it off is fine; asking for an event
        // while doing so is not, whether the commit names the CRTC or only
        // its plane (whose state drags the CRTC's in, on Linux).
        assert_eq!(commit(&c, &active(0), 0), Ok(0));
        assert_eq!(commit(&c, &active(0), MODESET), Ok(0));
        assert_eq!(commit(&c, &active(0), EVENT), Err(FsError::InvalidParam));
        assert_eq!(
            commit(&c, &active(0), EVENT | MODESET),
            Err(FsError::InvalidParam)
        );
        let plane_only = Request::new(&[drm::SYNTH_PLANE_ID], &[1], &[PROP_CRTC_X], &[0]);
        assert_eq!(commit(&c, &plane_only, 0), Ok(0));
        assert_eq!(
            commit(&c, &plane_only, EVENT),
            Err(FsError::InvalidParam),
            "a plane update with an event on an off CRTC"
        );
        assert_eq!(events(&c), 0, "a refused commit queued its event");
        // TEST_ONLY carries no event, so it is not refused for this reason
        // (the TEST_ONLY|EVENT combination is refused earlier, on its own).
        assert_eq!(commit(&c, &active(0), DRM_MODE_ATOMIC_TEST_ONLY), Ok(0));

        // Turning it on with an event: allowed, and the event comes.
        let blob = mode_blob(&c);
        let on = Request::new(
            &[drm::SYNTH_CRTC_ID],
            &[2],
            &[PROP_MODE_ID, PROP_ACTIVE],
            &[u64::from(blob), 1],
        );
        assert_eq!(commit(&c, &on, EVENT | MODESET), Ok(0));
        assert_eq!(events(&c), 1);
        // On and staying on: the ordinary frame.
        assert_eq!(commit(&c, &active(1), EVENT), Ok(0));
        assert_eq!(events(&c), 1);
        // Turning it off with an event: allowed too (it was on), and the
        // event comes.
        assert_eq!(commit(&c, &active(0), EVENT | MODESET), Ok(0));
        assert_eq!(events(&c), 1);
        // Off and staying off again: refused again, nothing queued.
        assert_eq!(commit(&c, &active(0), EVENT), Err(FsError::InvalidParam));
        assert_eq!(commit(&c, &plane_only, EVENT), Err(FsError::InvalidParam));
        assert_eq!(events(&c), 0);
    }
}

#[cfg(test)]
mod wait_vblank_validation_tests {
    //! What `WAIT_VBLANK` refuses, driven through the ioctl entry point the
    //! way `drmWaitVBlank` drives it: `_DRM_VBLANK_SIGNAL`, a bit outside
    //! the masks, and a pipe the card does not have, each EINVAL in
    //! `drm_wait_vblank_ioctl` before the sequence is even looked at. This
    //! arm answered all of them with pipe 0's counter.
    use super::gl_client_sequence_tests::Client;
    use super::*;
    use crate::fs::devfs::kms_emu::{self, EmuGpu};

    const RELATIVE: u32 = 0x1;

    fn wait(c: &Client, typ: u32, sequence: u32) -> Result<u32> {
        let mut req = DrmWaitVblank {
            typ,
            sequence,
            val1: 0,
            val2: 0,
        };
        c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req)
            .map(|_| req.sequence)
    }

    fn high_crtc(index: u32) -> u32 {
        index << _DRM_VBLANK_HIGH_CRTC_SHIFT
    }

    #[test]
    fn a_signal_an_unknown_bit_or_a_missing_pipe_is_refused_before_the_wait() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);

        // The shapes libdrm sends on a one-head card all go through: a
        // relative query, the same with the high-CRTC field naming pipe 0,
        // NEXTONMISS, and the event form.
        let now = wait(&c, RELATIVE, 0).expect("a relative query");
        assert_eq!(wait(&c, RELATIVE | high_crtc(0), 0), Ok(now));
        assert_eq!(
            wait(&c, RELATIVE | _DRM_VBLANK_NEXTONMISS_FLAG, 0).map(|s| s >= now),
            Ok(true)
        );
        assert_eq!(wait(&c, RELATIVE | _DRM_VBLANK_EVENT, 1).is_ok(), true);

        // Signals: EINVAL, whatever else is set.
        for typ in [
            _DRM_VBLANK_SIGNAL,
            _DRM_VBLANK_SIGNAL | RELATIVE,
            _DRM_VBLANK_SIGNAL | _DRM_VBLANK_EVENT | RELATIVE,
        ] {
            assert_eq!(wait(&c, typ, 0), Err(FsError::InvalidParam), "{typ:#x}");
        }
        // A bit outside the type, flag and high-CRTC masks.
        for bit in [1 << 6, 1 << 12, 1 << 25, 1 << 27, 1 << 31] {
            assert_eq!(
                wait(&c, RELATIVE | bit, 0),
                Err(FsError::InvalidParam),
                "bit {bit:#x}"
            );
        }
        // A pipe this card does not have: the high-CRTC field past 0, or
        // SECONDARY (pipe 1).
        for typ in [
            RELATIVE | high_crtc(1),
            RELATIVE | high_crtc(31),
            RELATIVE | _DRM_VBLANK_SECONDARY,
            RELATIVE | _DRM_VBLANK_EVENT | _DRM_VBLANK_SECONDARY,
            RELATIVE | _DRM_VBLANK_EVENT | high_crtc(1),
        ] {
            assert_eq!(wait(&c, typ, 0), Err(FsError::InvalidParam), "{typ:#x}");
        }
        // Refused before anything is scheduled: the fd carries nothing (the
        // one accepted event above is owed at the next vblank, which no
        // timer delivers here).
        let mut buf = [0u8; 128];
        assert_eq!(c.read_events(&mut buf), Err(FsError::Again));
    }

    /// The event form answers with the vblank the event was queued for:
    /// the resolved target, or the current count when the target had
    /// already passed (`drm_queue_vblank_event`). Xorg's modesetting
    /// driver keeps that as the MSC it queued; this arm left the request's
    /// own sequence in place, so a relative "+2" read back as 2.
    #[test]
    fn the_event_form_replies_with_the_vblank_it_queued() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        const EVENT: u32 = _DRM_VBLANK_EVENT;
        // The counter runs on the clock, so bracket each reply between two
        // readings rather than pinning it to one.
        let bracket = |typ: u32, seq: u32| {
            let before = wait(&c, RELATIVE, 0).expect("now");
            let got = wait(&c, typ, seq).expect("the event form");
            let after = wait(&c, RELATIVE, 0).expect("now");
            (before, got, after)
        };
        let within = |lo: u32, got: u32, hi: u32| {
            (got.wrapping_sub(lo) as i32) >= 0 && (hi.wrapping_sub(got) as i32) >= 0
        };

        // Relative +2: two past the count at the time.
        let (b, got, a) = bracket(RELATIVE | EVENT, 2);
        assert!(
            within(b + 2, got, a + 2),
            "relative +2 replied {} at {}..{}",
            got,
            b,
            a
        );
        // Absolute, ahead: exactly that.
        let now = wait(&c, RELATIVE, 0).expect("now");
        assert_eq!(wait(&c, EVENT, now + 50), Ok(now + 50));
        // Absolute, already passed: the current count, like Linux, which
        // sends the event at once in that case.
        let (b, got, a) = bracket(EVENT, now.wrapping_sub(3));
        assert!(
            within(b, got, a),
            "a passed target replied {} at {}..{}",
            got,
            b,
            a
        );
        // Passed with NEXTONMISS: the target moved to the next vblank.
        let (b, got, a) = bracket(EVENT | _DRM_VBLANK_NEXTONMISS_FLAG, now.wrapping_sub(3));
        assert!(
            within(b + 1, got, a + 1),
            "NEXTONMISS replied {} at {}..{}",
            got,
            b,
            a
        );
    }

    /// With two heads (two drivers that own scanout, each with a CRTC of
    /// its own), pipe 1 exists -- by the high-CRTC field or as SECONDARY --
    /// and pipe 2 does not. What separates reading the field from merely
    /// refusing anything in it.
    #[test]
    fn a_second_head_makes_pipe_one_a_real_pipe_and_pipe_two_still_not() {
        let screen = kms_emu::attach(64, 16);
        let _a = screen.attach_gpu(EmuGpu::hardware_kms("emu-a").with_ids(40, 41, 42));
        let _b = screen.attach_gpu(EmuGpu::hardware_kms("emu-b").with_ids(60, 61, 62));
        let c = Client::open(0);
        assert_eq!(drm::crtc_count(), 2);

        assert!(wait(&c, RELATIVE, 0).is_ok());
        assert!(
            wait(&c, RELATIVE | high_crtc(1), 0).is_ok(),
            "pipe 1 by the field"
        );
        assert!(
            wait(&c, RELATIVE | _DRM_VBLANK_SECONDARY, 0).is_ok(),
            "pipe 1 as SECONDARY"
        );
        for typ in [
            RELATIVE | high_crtc(2),
            RELATIVE | high_crtc(31),
            RELATIVE | _DRM_VBLANK_SECONDARY | high_crtc(2),
        ] {
            assert_eq!(wait(&c, typ, 0), Err(FsError::InvalidParam), "{typ:#x}");
        }
    }
}

#[cfg(test)]
mod prime_import_close_tests {
    //! `GEM_CLOSE` and `DESTROY_DUMB` are where a file lets go of a handle:
    //! what it imports after that is a new reference, as in Linux, where
    //! `drm_gem_handle_delete` drops the `drm_prime_file_private` entry with
    //! the handle on both paths.
    use super::gl_client_sequence_tests::Client;
    use super::*;

    #[test]
    fn closing_the_handle_forgets_the_import_so_the_next_one_counts_again() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        let by_gem_close = c.create_dumb(16, 16);
        let by_destroy_dumb = c.create_dumb(16, 16);
        for h in [by_gem_close.handle, by_destroy_dumb.handle] {
            assert!(c.file_state().note_prime_import(h));
            assert!(c.file_state().holds_prime_import(h));
        }
        let mut h = by_gem_close.handle;
        assert_eq!(c.ioctl(DRM_IOCTL_GEM_CLOSE, &mut h), Ok(0));
        assert!(
            !c.file_state().holds_prime_import(by_gem_close.handle),
            "GEM_CLOSE left the import on record"
        );
        assert!(
            c.file_state().holds_prime_import(by_destroy_dumb.handle),
            "closing one handle forgot the other"
        );
        assert_eq!(c.destroy_dumb(by_destroy_dumb.handle), Ok(0));
        assert!(
            !c.file_state().holds_prime_import(by_destroy_dumb.handle),
            "DESTROY_DUMB left the import on record"
        );
        for h in [by_gem_close.handle, by_destroy_dumb.handle] {
            assert!(c.file_state().note_prime_import(h), "counts again");
            c.file_state().forget_prime_import(h);
        }
        // A close that fails (handle already gone) forgets nothing, and says so.
        assert!(c.file_state().note_prime_import(by_gem_close.handle));
        let mut h = by_gem_close.handle;
        assert_eq!(
            c.ioctl(DRM_IOCTL_GEM_CLOSE, &mut h),
            Err(FsError::InvalidParam)
        );
        assert!(c.file_state().holds_prime_import(by_gem_close.handle));
        c.file_state().forget_prime_import(by_gem_close.handle);
    }
}

#[cfg(test)]
mod client_cap_tests {
    //! `GET_CAP` and `SET_CLIENT_CAP` against `drm_getcap` and
    //! `drm_setclientcap`: what is known, what is boolean, and what is EINVAL.
    use super::gl_client_sequence_tests::Client;
    use super::*;

    fn get_cap(c: &Client, capability: u64) -> Result<u64> {
        let mut req = DrmGetCap {
            capability,
            value: 0xdead_beef,
        };
        c.ioctl(DRM_IOCTL_GET_CAP, &mut req).map(|_| req.value)
    }

    fn set_cap(c: &Client, cap: u64, value: u64) -> Result<usize> {
        let mut req: [u64; 2] = [cap, value];
        c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut req)
    }

    #[test]
    fn get_cap_knows_high_crtc_and_refuses_a_capability_it_has_never_heard_of() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        assert_eq!(get_cap(&c, 0x1), Ok(1), "DUMB_BUFFER");
        assert_eq!(
            get_cap(&c, 0x2),
            Ok(1),
            "VBLANK_HIGH_CRTC: WAIT_VBLANK reads it"
        );
        assert_eq!(get_cap(&c, 0x6), Ok(1), "TIMESTAMP_MONOTONIC");
        assert_eq!(get_cap(&c, 0x12), Ok(1), "CRTC_IN_VBLANK_EVENT");
        for known_zero in [0x7u64, 0x11, 0x15] {
            assert_eq!(get_cap(&c, known_zero), Ok(0), "cap {:#x}", known_zero);
        }
        for unknown in [0x0u64, 0xa, 0xf, 0x16, 0x100, u64::MAX] {
            let mut req = DrmGetCap {
                capability: unknown,
                value: 0xdead_beef,
            };
            assert_eq!(
                c.ioctl(DRM_IOCTL_GET_CAP, &mut req).err(),
                Some(FsError::InvalidParam),
                "cap {:#x}",
                unknown
            );
            assert_eq!(req.value, 0, "the value is zeroed, not left as it came");
        }
    }

    #[test]
    fn set_client_cap_takes_booleans_and_refuses_hotspot_and_the_unknown() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        for boolean in [
            DRM_CLIENT_CAP_STEREO_3D,
            DRM_CLIENT_CAP_UNIVERSAL_PLANES,
            DRM_CLIENT_CAP_ASPECT_RATIO,
        ] {
            assert_eq!(set_cap(&c, boolean, 0), Ok(0), "cap {} off", boolean);
            assert_eq!(set_cap(&c, boolean, 1), Ok(0), "cap {} on", boolean);
            for bad in [2u64, 5, u64::MAX] {
                assert_eq!(
                    set_cap(&c, boolean, bad),
                    Err(FsError::InvalidParam),
                    "cap {} value {}",
                    boolean,
                    bad
                );
            }
        }
        assert_eq!(
            set_cap(&c, DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT, 1),
            Err(FsError::OpNotSupported),
            "no virtualised cursor plane here"
        );
        assert_eq!(
            set_cap(&c, DRM_CLIENT_CAP_WRITEBACK_CONNECTORS, 1),
            Err(FsError::InvalidParam),
            "writeback needs an atomic client first"
        );
        for unknown in [0u64, 7, 8, 0x100, u64::MAX] {
            assert_eq!(
                set_cap(&c, unknown, 1),
                Err(FsError::InvalidParam),
                "cap {}",
                unknown
            );
            assert_eq!(set_cap(&c, unknown, 0), Err(FsError::InvalidParam));
        }
    }
}

#[cfg(test)]
mod mmap_bounds_tests {
    //! `MAP_DUMB` names a handle the file holds, and the `mmap` that follows
    //! is no longer than the object -- `drm_gem_dumb_map_offset` and
    //! `drm_gem_mmap_obj`.
    use super::gl_client_sequence_tests::Client;
    use super::*;
    use zcore_drivers::scheme::gem_mmap;
    use zircon_object::vm::PAGE_SIZE;

    /// A nouveau-range handle no GPU slice hands out in these tests.
    const H: u32 = 0xbfff_0008;

    #[test]
    fn map_dumb_is_enoent_for_a_handle_the_file_does_not_hold() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        let buf = c.create_dumb(16, 16);
        assert_eq!(c.map_dumb(buf.handle), Ok(mmap_cookie_for(buf.handle)));
        assert_eq!(
            c.map_dumb(buf.handle + 0x100),
            Err(FsError::EntryNotFound),
            "a dumb handle nobody created"
        );
        assert_eq!(
            c.map_dumb(H),
            Err(FsError::EntryNotFound),
            "a nouveau handle nobody registered"
        );
        // A nouveau GEM the process holds maps through the same ioctl, as
        // `drm_gem_object_lookup` finds any GEM object of the file.
        gem_mmap::register(H, 0x1000_0000, 2 * PAGE_SIZE as u64, drm::current_pid());
        assert_eq!(c.map_dumb(H), Ok(mmap_cookie_for(H)));
        assert!(gem_mmap::unregister(H));
        assert_eq!(c.map_dumb(H), Err(FsError::EntryNotFound), "gone again");
        assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
        assert_eq!(
            c.map_dumb(buf.handle),
            Err(FsError::EntryNotFound),
            "a destroyed dumb handle"
        );
    }

    #[test]
    fn an_mmap_longer_than_the_object_is_einval_and_one_that_fits_is_the_object() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        // 64 x 64 x 4 = 16 KiB: four whole pages.
        let buf = c.create_dumb(64, 64);
        assert_eq!(buf.size, 4 * PAGE_SIZE as u64);
        let off = c.map_dumb(buf.handle).unwrap();
        for len in [1usize, PAGE_SIZE, 3 * PAGE_SIZE + 1, 4 * PAGE_SIZE] {
            let vmo = c
                .mmap(off, len)
                .unwrap_or_else(|e| panic!("len {}: {:?}", len, e));
            assert_eq!(vmo.len(), 4 * PAGE_SIZE, "len {}", len);
        }
        for len in [4 * PAGE_SIZE + 1, 5 * PAGE_SIZE, 1 << 30] {
            assert_eq!(
                c.mmap(off, len).err(),
                Some(FsError::InvalidParam),
                "len {}",
                len
            );
        }
        // A 16 x 16 buffer is 1 KiB in a page of its own: the page is the
        // object's, the next one is not.
        let small = c.create_dumb(16, 16);
        let off = c.map_dumb(small.handle).unwrap();
        assert!(c.mmap(off, PAGE_SIZE).is_ok());
        assert_eq!(
            c.mmap(off, PAGE_SIZE + 1).err(),
            Some(FsError::InvalidParam)
        );
        assert_eq!(c.destroy_dumb(small.handle), Ok(0));
        assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
    }

    #[test]
    fn the_nouveau_side_measures_the_object_the_same_way() {
        let _serialised = drm::test_globals::lock();
        let c = Client::open(0);
        // Two pages and a byte: three pages of object.
        gem_mmap::register(H, 0x1000_0000, 2 * PAGE_SIZE as u64 + 1, drm::current_pid());
        let off = c.map_dumb(H).unwrap();
        assert_eq!(
            c.mmap(off, 3 * PAGE_SIZE + 1).err(),
            Some(FsError::InvalidParam),
            "past the object"
        );
        assert!(
            c.mmap(off, 3 * PAGE_SIZE).is_ok(),
            "the object's three pages"
        );
        assert!(gem_mmap::unregister(H));
        drm::nouveau_cpu_vmo_forget(H);
    }
}

#[cfg(test)]
mod present_fence_tests {
    //! The pre-present fence wait: which ioctls take it, and its clock.
    //!
    //! Both halves are pure, and both are the part that can be got wrong
    //! silently. The router decides whether a frame waits at all -- miss
    //! `PAGE_FLIP` and the whole implicit-sync path is dead code on a running
    //! desktop, with nothing in any log to say so, because the present still
    //! happens and merely shows a half-drawn buffer. The clock decides whether
    //! a wait that times out ends: `sleep_until` takes an ABSOLUTE instant, so
    //! a deadline already in the past must end the wait rather than park the
    //! compositor on it.
    //!
    //! What is not covered here, because it needs a live device and a driver:
    //! the fb lookup, the driver's fence answer, the timeout warning, the text of
    //! the two report lines, and the `+ 1` that makes the count one-based (a
    //! zero-based count would print `#0` for the first present and open with
    //! three lines instead of two -- visible only in a klog no test reads). The
    //! report rhythm IS covered, because it is the only thing that will ever say
    //! whether the wait found a fence at all, and a rhythm that reports nothing
    //! is a diagnostic that lies by omission.

    use super::*;

    /// The first two presents of a boot always report. Those two are the whole
    /// point: they answer "does this find a fence at all" before anything else
    /// has had a chance to go wrong, and a rhythm that started at 64 would
    /// answer it a second into the session at the earliest.
    #[test]
    fn the_first_two_presents_of_a_boot_always_say_what_they_found() {
        assert!(fence_report_decision(1, FENCE_REPORT_EVERY));
        assert!(fence_report_decision(2, FENCE_REPORT_EVERY));
    }

    /// And the third does not, or the klog is a line per frame -- which it
    /// writes synchronously to the UART, so it would be a stutter of its own.
    #[test]
    fn the_third_present_is_quiet() {
        assert!(!fence_report_decision(3, FENCE_REPORT_EVERY));
        for n in [4u64, 5, 63, 65, 127] {
            assert!(!fence_report_decision(n, FENCE_REPORT_EVERY), "n = {}", n);
        }
    }

    /// After that, one line every `FENCE_REPORT_EVERY` presents, counted from
    /// the first: `#64`, `#128`, and so on.
    #[test]
    fn then_one_line_every_sixty_four_presents() {
        for k in 1..=8u64 {
            let n = k * FENCE_REPORT_EVERY;
            assert!(fence_report_decision(n, FENCE_REPORT_EVERY), "n = {}", n);
        }
    }

    /// `n = 0` cannot happen -- the counter is read after its increment -- but
    /// the multiple-of test would call every rhythm true for it, so the answer
    /// is pinned rather than left to `0 % 64 == 0`.
    #[test]
    fn the_count_is_one_based_and_zero_is_not_a_present() {
        assert!(fence_report_decision(0, FENCE_REPORT_EVERY));
    }

    /// A rhythm of zero reports the first two and then goes quiet. Nothing
    /// divides by zero on the way there: `is_multiple_of(0)` answers `false` for
    /// a non-zero count, which is exactly "not on the rhythm". The constant is
    /// not configurable today, so this is about the function staying safe if it
    /// ever becomes so.
    #[test]
    fn a_rhythm_of_zero_reports_the_opening_and_nothing_else() {
        assert!(fence_report_decision(1, 0));
        assert!(fence_report_decision(2, 0));
        for n in [3u64, 64, 128, u64::MAX] {
            assert!(!fence_report_decision(n, 0), "n = {}", n);
        }
    }

    /// The fence line and the present cost line describe the SAME present, so
    /// they share one rhythm and land together in the klog: `waited 0us for 0
    /// fence(s)` next to `cpu blit 12000us` is the pair that says whether the
    /// wait is what costs the frame. Two independent numbers would drift apart
    /// and leave a reader counting frames between them.
    #[test]
    fn the_fence_line_keeps_the_cost_lines_rhythm() {
        assert_eq!(FENCE_REPORT_EVERY, drm::FULL_FRAME_REPORT_EVERY);
    }

    /// The last present a 64-bit counter can reach still reports on its rhythm
    /// rather than panicking or wrapping: the counter itself saturates.
    #[test]
    fn the_rhythm_holds_at_the_top_of_the_counter() {
        let top = u64::MAX - (u64::MAX % FENCE_REPORT_EVERY);
        assert!(fence_report_decision(top, FENCE_REPORT_EVERY));
        assert!(!fence_report_decision(u64::MAX, FENCE_REPORT_EVERY));
    }

    /// `SETCRTC` and `PAGE_FLIP` both present, and both must wait.
    #[test]
    fn the_two_legacy_presents_are_recognised_at_their_real_sizes() {
        assert_eq!(
            legacy_present_kind(DRM_IOCTL_MODE_SETCRTC),
            Some(LegacyPresent::SetCrtc)
        );
        assert_eq!(
            legacy_present_kind(DRM_IOCTL_MODE_PAGE_FLIP),
            Some(LegacyPresent::PageFlip)
        );
    }

    /// The struct sizes the `nr` table claims are the ones the dispatch arms
    /// actually read, so a struct that changes shape breaks here first.
    #[test]
    fn the_size_floors_match_the_structs_the_wait_parses() {
        assert_eq!(nr::MODE_SETCRTC.1, core::mem::size_of::<DrmModeGetCrtc>());
        assert_eq!(
            nr::MODE_PAGE_FLIP.1,
            core::mem::size_of::<DrmModeCrtcPageFlip>()
        );
    }

    /// A struct that grows a trailing field keeps waiting. This is the bug
    /// `is_drm_ioctl_nr` exists for, and pinning the 32-bit command instead
    /// would have made a wider `drm_mode_crtc` skip the wait in silence.
    #[test]
    fn a_larger_encoded_struct_is_still_the_same_present() {
        for extra in [8usize, 16, 64] {
            assert_eq!(
                legacy_present_kind(drm_iowr_core(
                    nr::MODE_SETCRTC.0,
                    nr::MODE_SETCRTC.1 + extra
                )),
                Some(LegacyPresent::SetCrtc)
            );
            assert_eq!(
                legacy_present_kind(drm_iowr_core(
                    nr::MODE_PAGE_FLIP.0,
                    nr::MODE_PAGE_FLIP.1 + extra
                )),
                Some(LegacyPresent::PageFlip)
            );
        }
    }

    /// Below the floor the helper would read past the request, so it must fall
    /// through to the sync arm, which zero-pads it the way Linux does.
    #[test]
    fn a_short_request_does_not_take_the_wait() {
        assert_eq!(
            legacy_present_kind(drm_iowr_core(nr::MODE_SETCRTC.0, nr::MODE_SETCRTC.1 - 1)),
            None
        );
        assert_eq!(
            legacy_present_kind(drm_iowr_core(nr::MODE_PAGE_FLIP.0, 8)),
            None
        );
    }

    /// Everything else, including the ioctls with their own waits: a present
    /// fence wait on an atomic commit would wait twice, and on a syncobj wait
    /// it would wait for the wrong thing entirely.
    #[test]
    fn no_other_ioctl_takes_the_present_wait() {
        for cmd in [
            ATOMIC_IOCTL,
            WAIT_VBLANK_IOCTL,
            DRM_IOCTL_SYNCOBJ_WAIT,
            DRM_IOCTL_MODE_DIRTYFB,
            DRM_IOCTL_MODE_SETPLANE,
        ] {
            assert_eq!(legacy_present_kind(cmd), None, "cmd {:#x} waits", cmd);
        }
        // Right NR, wrong ioctl type byte: another subsystem's 0xA2.
        let foreign = drm_iowr_core(nr::MODE_SETCRTC.0, nr::MODE_SETCRTC.1) & !(0xff << 8);
        assert_eq!(legacy_present_kind(foreign), None);
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// The ordinary tick, while the deadline is far away.
    #[test]
    fn a_poll_far_from_its_deadline_sleeps_one_tick() {
        assert_eq!(next_poll_wake(ms(10), ms(110), ms(1)), Some(ms(11)));
    }

    /// Clamped to the deadline, or a 100 ms cap becomes 101 ms on every frame
    /// that times out.
    #[test]
    fn the_last_poll_never_sleeps_past_the_deadline() {
        assert_eq!(
            next_poll_wake(ms(10), ms(10) + ms(1) / 2, ms(1)),
            Some(ms(10) + ms(1) / 2)
        );
    }

    /// At or past the deadline the wait ends. Parking on an instant already
    /// behind `timer_now` is what would hang the compositor instead of
    /// presenting a possibly-torn frame.
    #[test]
    fn an_expired_deadline_ends_the_wait_instead_of_sleeping() {
        assert_eq!(next_poll_wake(ms(10), ms(10), ms(1)), None);
        assert_eq!(next_poll_wake(ms(10), ms(9), ms(1)), None);
        assert_eq!(next_poll_wake(ms(10), Duration::ZERO, ms(1)), None);
    }

    /// A zero tick is a busy loop, not a sleep: `wake == now` ends the wait
    /// rather than yielding forever at the same instant.
    #[test]
    fn a_zero_tick_ends_the_wait_rather_than_spinning() {
        assert_eq!(next_poll_wake(ms(10), ms(110), Duration::ZERO), None);
    }
}

#[cfg(test)]
mod card_fd_wait_tests {
    //! The waiter behind a blocking `read()` on the card fd.
    //!
    //! A compositor's frame loop lives here: it page-flips, then blocks reading
    //! the card fd until the completion arrives. The wait is an event-bus
    //! subscription, and the bus drops a callback that returns `true` as soon
    //! as it fires. So a waiter that woke, found nothing to read, and parked
    //! again had to make sure it still HAD a callback -- nothing in this file
    //! re-polls the fd on a tick, so a park with no callback is a frame loop
    //! that stops for good.
    //!
    //! Waking with nothing to read is ordinary, not exotic: another reader of
    //! the same `drm_file` (a `dup`ed fd, a second compositor thread) can drain
    //! the queue in between, and the bus flag is latched independently of the
    //! queue.

    use super::*;
    use crate::sync::Event;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, RawWaker, RawWakerVTable, Waker};

    /// A waker that counts how many times the bus reached it.
    fn counting_waker(hits: &AtomicUsize) -> Waker {
        fn raw(ptr: *const ()) -> RawWaker {
            unsafe fn clone(ptr: *const ()) -> RawWaker {
                raw(ptr)
            }
            unsafe fn wake(ptr: *const ()) {
                wake_by_ref(ptr)
            }
            unsafe fn wake_by_ref(ptr: *const ()) {
                // SAFETY: `ptr` is the `&AtomicUsize` this waker was built
                // from, which outlives the waker (it is a local of the test).
                unsafe { &*(ptr as *const AtomicUsize) }.fetch_add(1, Ordering::Relaxed);
            }
            unsafe fn drop(_: *const ()) {}
            RawWaker::new(ptr, &RawWakerVTable::new(clone, wake, wake_by_ref, drop))
        }
        // SAFETY: the vtable above only ever reads `ptr` as the `&AtomicUsize`.
        unsafe { Waker::from_raw(raw(hits as *const AtomicUsize as *const ())) }
    }

    /// The bug this pins down: one wake that produced nothing to read used to
    /// leave the reader parked with no callback on the bus.
    #[test]
    fn a_wake_that_finds_nothing_to_read_leaves_the_reader_still_subscribed() {
        let hits = AtomicUsize::new(0);
        let waker = counting_waker(&hits);
        let mut cx = Context::from_waker(&waker);

        let dev = DrmDev::new(0);
        let bus = dev.file.eventbus();
        let mut fut = core::pin::pin!(DrmEventWait {
            dev: &dev,
            bus: bus.clone(),
            sub_id: None,
        });

        // Nothing queued, so the first poll parks with a callback on the bus.
        assert!(matches!(fut.as_mut().poll(&mut cx), TaskPoll::Pending));
        assert_eq!(
            bus.lock().get_callback_len(),
            1,
            "parked without a callback"
        );

        // A readable edge that leaves the queue empty: somebody else read it.
        bus.lock().change(Event::empty(), Event::READABLE);
        assert_eq!(hits.load(Ordering::Relaxed), 1, "the edge has to wake it");

        // Woken, still nothing to read, parks again -- and MUST still be
        // subscribed. This is the assertion the one-shot callback failed.
        assert!(matches!(fut.as_mut().poll(&mut cx), TaskPoll::Pending));
        assert_eq!(
            bus.lock().get_callback_len(),
            1,
            "parked with no callback on the bus: nothing will ever wake this \
             reader again, and a compositor blocked in read() on the card fd is \
             a frame loop that has stopped"
        );

        // And the proof it is a live callback and not merely a present one: the
        // next edge reaches the same waiter.
        bus.lock().change(Event::READABLE, Event::empty());
        bus.lock().change(Event::empty(), Event::READABLE);
        assert_eq!(
            hits.load(Ordering::Relaxed),
            2,
            "the second edge never reached the reader"
        );
    }

    /// The other half, so the fix cannot be "never unsubscribe": dropping the
    /// future still takes the callback off the bus. A card fd is opened and
    /// closed on every compositor restart, and the bus evicts its oldest entry
    /// when the table fills, so a leak here would silently cost somebody else
    /// their wakeup.
    #[test]
    fn dropping_the_waiter_takes_its_callback_off_the_bus() {
        let hits = AtomicUsize::new(0);
        let waker = counting_waker(&hits);
        let mut cx = Context::from_waker(&waker);

        let dev = DrmDev::new(0);
        let bus = dev.file.eventbus();
        {
            let mut fut = core::pin::pin!(DrmEventWait {
                dev: &dev,
                bus: bus.clone(),
                sub_id: None,
            });
            assert!(matches!(fut.as_mut().poll(&mut cx), TaskPoll::Pending));
            assert_eq!(bus.lock().get_callback_len(), 1);
        }
        assert_eq!(
            bus.lock().get_callback_len(),
            0,
            "the callback outlived the waiter"
        );
    }
}

#[cfg(test)]
mod syncobj_array_tests {
    //! The syncobj ioctls that take an array of handles, driven through the
    //! ioctl entry point with the nouveau uAPI on: how many handles they
    //! take, and what an empty array means.
    use super::gl_client_sequence_tests::Client;
    use super::*;

    /// The nouveau uAPI switch, on for one test and put back after; under
    /// `drm::test_globals::lock()`, like every process-wide DRM knob. The
    /// tests also hold the eventfd tests' lock: signaling a syncobj fires
    /// the process-wide signal hook those tests install, whose walk
    /// delivers their waiter (see `syncobj_eventfd`'s `TEST_SERIAL`).
    struct NouveauOn(bool);
    impl NouveauOn {
        fn new() -> Self {
            let was = zcore_drivers::display::nouveau_uapi_enabled();
            zcore_drivers::display::set_nouveau_uapi_enabled(true);
            NouveauOn(was)
        }
    }
    impl Drop for NouveauOn {
        fn drop(&mut self) {
            zcore_drivers::display::set_nouveau_uapi_enabled(self.0);
        }
    }

    fn create(c: &Client, signaled: bool) -> u32 {
        let mut req = DrmSyncobjCreate {
            handle: 0,
            flags: if signaled {
                DRM_SYNCOBJ_CREATE_SIGNALED
            } else {
                0
            },
        };
        c.ioctl(DRM_IOCTL_SYNCOBJ_CREATE, &mut req).expect("CREATE");
        req.handle
    }

    fn destroy(c: &Client, handle: u32) {
        let mut req = DrmSyncobjDestroy { handle, pad: 0 };
        c.ioctl(DRM_IOCTL_SYNCOBJ_DESTROY, &mut req)
            .expect("DESTROY");
    }

    /// `SYNCOBJ_WAIT` with an absolute deadline already passed: what is
    /// signaled now decides. Gives back `first_signaled`.
    fn wait(c: &Client, handles: &[u32], flags: u32) -> Result<u32> {
        let mut req = DrmSyncobjWait {
            handles: handles.as_ptr() as u64,
            timeout_nsec: 0,
            count_handles: handles.len() as u32,
            flags,
            first_signaled: 0xdead,
            pad: 0,
        };
        c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT, &mut req)
            .map(|_| req.first_signaled)
    }

    fn timeline_wait(c: &Client, handles: &[u32], points: &[u64], flags: u32) -> Result<u32> {
        let mut req = DrmSyncobjTimelineWait {
            handles: handles.as_ptr() as u64,
            points: points.as_ptr() as u64,
            timeout_nsec: 0,
            count_handles: handles.len() as u32,
            flags,
            first_signaled: 0xdead,
            pad: 0,
        };
        c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut req)
            .map(|_| req.first_signaled)
    }

    /// `RESET` or `SIGNAL`.
    fn array(c: &Client, cmd: u32, handles: &[u32]) -> Result<usize> {
        let mut req = DrmSyncobjArray {
            handles: handles.as_ptr() as u64,
            count_handles: handles.len() as u32,
            pad: 0,
        };
        c.ioctl(cmd, &mut req)
    }

    /// `TIMELINE_SIGNAL` or `QUERY`, `points` read or written per arm.
    fn timeline_array(c: &Client, cmd: u32, handles: &[u32], points: &mut [u64]) -> Result<usize> {
        let mut req = DrmSyncobjTimelineArray {
            handles: handles.as_ptr() as u64,
            points: points.as_mut_ptr() as u64,
            count_handles: handles.len() as u32,
            flags: 0,
        };
        c.ioctl(cmd, &mut req)
    }

    /// NVK's `vk_drm_syncobj_get_type` probe exactly as Alpine's libdrm
    /// 2.4.134 sends it: a WAIT on a SIGNALED syncobj, timeout 0, in the
    /// deadline-sized struct (40 bytes; 48 for the timeline form). Anything
    /// but 0 strips `VK_SYNC_FEATURE_CPU_WAIT` from NVK's syncobj type, and
    /// the first submit that needs a binary CPU-wait type derefs the NULL at
    /// the end of `supported_sync_types` (`libvulkan_nouveau.so+0xe9c48`).
    ///
    /// These sizes go through `drm_ioctl_reconciled`'s kernel bounce buffer,
    /// and the arm used to `ucheck` that kernel address: EFAULT on bare metal.
    /// Under `libos` the user-half bound is off, so this test cannot see that
    /// half by itself -- it pins the path (bounce buffer, reply copied back)
    /// that the fix in `read_syncobj_wait` is about.
    #[test]
    fn the_deadline_sized_waits_nvk_probes_with_succeed_on_a_signaled_syncobj() {
        #[repr(C)]
        struct WaitDeadline {
            wait: DrmSyncobjWait,
            deadline_nsec: u64,
        }
        #[repr(C)]
        struct TimelineWaitDeadline {
            wait: DrmSyncobjTimelineWait,
            deadline_nsec: u64,
        }
        assert_eq!(core::mem::size_of::<WaitDeadline>(), 40);
        assert_eq!(core::mem::size_of::<TimelineWaitDeadline>(), 48);

        let _serialised = drm::test_globals::lock();
        let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        let _on = NouveauOn::new();
        let c = Client::open(0);
        let h = [create(&c, true)];

        let mut req = WaitDeadline {
            wait: DrmSyncobjWait {
                handles: h.as_ptr() as u64,
                timeout_nsec: 0,
                count_handles: 1,
                flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
                first_signaled: 0xdead,
                pad: 0,
            },
            deadline_nsec: 0,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT_DEADLINE, &mut req), Ok(0));
        assert_eq!(
            req.wait.first_signaled, 0,
            "reply copied back to the client"
        );

        let points = [1u64];
        let mut req = TimelineWaitDeadline {
            wait: DrmSyncobjTimelineWait {
                handles: h.as_ptr() as u64,
                points: points.as_ptr() as u64,
                timeout_nsec: 0,
                count_handles: 1,
                flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
                first_signaled: 0xdead,
                pad: 0,
            },
            deadline_nsec: 0,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT_DEADLINE, &mut req),
            Ok(0)
        );
        assert_eq!(req.wait.first_signaled, 0);

        destroy(&c, h[0]);
    }

    /// `vkWaitForFences` on more fences than 64 is one `TIMELINE_WAIT` over
    /// all of them (`vk_drm_syncobj_wait_many`), and `vkResetFences` one
    /// `RESET`; Linux takes any number the allocator does. Every array arm
    /// used to stop at 64 with EINVAL, which Mesa reports as a lost device.
    #[test]
    fn the_array_ioctls_take_more_than_sixty_four_handles() {
        let _serialised = drm::test_globals::lock();
        let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        let _on = NouveauOn::new();
        let c = Client::open(0);
        const N: usize = 100;
        let handles: alloc::vec::Vec<u32> = (0..N).map(|_| create(&c, true)).collect();
        assert_eq!(wait(&c, &handles, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL), Ok(0));
        let ones = alloc::vec![1u64; N];
        assert_eq!(
            timeline_wait(&c, &handles, &ones, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL),
            Ok(0)
        );
        // RESET all: nothing is signaled any more, whichever is asked.
        assert_eq!(array(&c, DRM_IOCTL_SYNCOBJ_RESET, &handles), Ok(0));
        assert_eq!(wait(&c, &handles, 0), Err(FsError::TimedOut));
        assert_eq!(
            wait(&c, &handles[N - 1..], 0),
            Err(FsError::TimedOut),
            "the hundredth was reset too"
        );
        // SIGNAL all: the hundredth is signaled, and the first one found.
        assert_eq!(array(&c, DRM_IOCTL_SYNCOBJ_SIGNAL, &handles), Ok(0));
        assert_eq!(wait(&c, &handles[N - 1..], 0), Ok(0));
        assert_eq!(wait(&c, &handles, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL), Ok(0));
        let mut points = alloc::vec![0u64; N];
        assert_eq!(
            timeline_array(&c, DRM_IOCTL_SYNCOBJ_QUERY, &handles, &mut points),
            Ok(0)
        );
        assert!(points.iter().all(|&p| p == 1), "{:?}", points);
        // TIMELINE_SIGNAL all to 5: the query and the wait see every one.
        let mut fives = alloc::vec![5u64; N];
        assert_eq!(
            timeline_array(&c, DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &handles, &mut fives),
            Ok(0)
        );
        assert_eq!(
            timeline_array(&c, DRM_IOCTL_SYNCOBJ_QUERY, &handles, &mut points),
            Ok(0)
        );
        assert!(points.iter().all(|&p| p == 5), "{:?}", points);
        assert_eq!(
            timeline_wait(&c, &handles, &fives, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL),
            Ok(0)
        );
        let sixes = alloc::vec![6u64; N];
        assert_eq!(
            timeline_wait(&c, &handles, &sixes, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL),
            Err(FsError::TimedOut)
        );
        // Only the last one at 6: found at its index, not at one of the 64.
        assert_eq!(
            timeline_array(
                &c,
                DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
                &handles[N - 1..],
                &mut [6u64]
            ),
            Ok(0)
        );
        assert_eq!(
            timeline_wait(&c, &handles, &sixes, 0),
            Ok(N as u32 - 1),
            "first_signaled"
        );
        for h in handles {
            destroy(&c, h);
        }
    }

    /// A wait on no handles is answered 0 at once, without reading the
    /// array (Linux `drm_syncobj_wait_ioctl`); the arms that change or read
    /// state refuse an empty array with EINVAL, as Linux does.
    #[test]
    fn a_wait_with_no_handles_returns_at_once_and_the_other_arrays_refuse_it() {
        let _serialised = drm::test_globals::lock();
        let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        let _on = NouveauOn::new();
        let c = Client::open(0);
        let none: [u32; 0] = [];
        let mut req = DrmSyncobjWait {
            handles: 0,
            timeout_nsec: 0,
            count_handles: 0,
            flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
            first_signaled: 0xdead,
            pad: 0,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT, &mut req), Ok(0));
        assert_eq!(req.first_signaled, 0xdead, "not written");
        let mut req = DrmSyncobjTimelineWait {
            handles: 0,
            points: 0,
            timeout_nsec: 0,
            count_handles: 0,
            flags: 0,
            first_signaled: 0xdead,
            pad: 0,
        };
        assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut req), Ok(0));
        assert_eq!(req.first_signaled, 0xdead);
        for cmd in [DRM_IOCTL_SYNCOBJ_RESET, DRM_IOCTL_SYNCOBJ_SIGNAL] {
            assert_eq!(
                array(&c, cmd, &none),
                Err(FsError::InvalidParam),
                "{:#x}",
                cmd
            );
        }
        // Real (if empty) arrays behind the pointers, so an arm that went
        // on to read its first entry would fail the assertion, not crash.
        let mut no_points = [0u64; 1];
        for cmd in [DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, DRM_IOCTL_SYNCOBJ_QUERY] {
            assert_eq!(
                timeline_array(&c, cmd, &none, &mut no_points[..0]),
                Err(FsError::InvalidParam),
                "{:#x}",
                cmd
            );
        }
    }

    /// Past what the kernel would allocate for the array, ENOMEM, before
    /// the array is looked at (it is never read here: the pointer is null).
    #[test]
    fn an_array_past_what_the_kernel_would_allocate_is_enomem() {
        let _serialised = drm::test_globals::lock();
        let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        let _on = NouveauOn::new();
        let c = Client::open(0);
        let too_many = SYNCOBJ_ARRAY_MAX + 1;
        let mut req = DrmSyncobjWait {
            handles: 0,
            timeout_nsec: 0,
            count_handles: too_many,
            flags: 0,
            first_signaled: 0,
            pad: 0,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT, &mut req),
            Err(FsError::NoMemory)
        );
        let mut req = DrmSyncobjTimelineWait {
            handles: 0,
            points: 0,
            timeout_nsec: 0,
            count_handles: too_many,
            flags: 0,
            first_signaled: 0,
            pad: 0,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut req),
            Err(FsError::NoMemory)
        );
        for cmd in [DRM_IOCTL_SYNCOBJ_RESET, DRM_IOCTL_SYNCOBJ_SIGNAL] {
            let mut req = DrmSyncobjArray {
                handles: 0,
                count_handles: too_many,
                pad: 0,
            };
            assert_eq!(c.ioctl(cmd, &mut req), Err(FsError::NoMemory), "{:#x}", cmd);
        }
        for cmd in [DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, DRM_IOCTL_SYNCOBJ_QUERY] {
            let mut req = DrmSyncobjTimelineArray {
                handles: 0,
                points: 0,
                count_handles: too_many,
                flags: 0,
            };
            assert_eq!(c.ioctl(cmd, &mut req), Err(FsError::NoMemory), "{:#x}", cmd);
        }
        // The bound itself is fine, and a null array under it is EINVAL as
        // before (Linux: EFAULT from the copy).
        let mut req = DrmSyncobjArray {
            handles: 0,
            count_handles: SYNCOBJ_ARRAY_MAX,
            pad: 0,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_SYNCOBJ_SIGNAL, &mut req),
            Err(FsError::InvalidParam)
        );
        let mut req = DrmSyncobjWait {
            handles: 0,
            timeout_nsec: 0,
            count_handles: 1,
            flags: 0,
            first_signaled: 0,
            pad: 0,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT, &mut req),
            Err(FsError::InvalidParam)
        );
    }

    /// `drm_syncobj_transfer_ioctl`: the padding is EINVAL, and so is any
    /// flag but WAIT_FOR_SUBMIT (`drm_syncobj_find_fence`); without that
    /// flag the source has to carry a fence at `src_point` already, so a
    /// source with none, or a timeline point nothing has submitted, is
    /// EINVAL and the destination is untouched; with it the transfer waits
    /// for the submission. Neither field was read and every transfer was
    /// deferred.
    #[test]
    fn transfer_reads_its_padding_and_flags_and_wants_a_submitted_source() {
        let _serialised = drm::test_globals::lock();
        let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        let _on = NouveauOn::new();
        let c = Client::open(0);
        let signaled = create(&c, true);
        let fresh = create(&c, false);
        let timeline = create(&c, false);
        let dst = create(&c, false);
        let transfer = |dst_handle: u32, src_handle: u32, src_point: u64, flags: u32, pad: u32| {
            let mut req = DrmSyncobjTransfer {
                src_handle,
                dst_handle,
                src_point,
                dst_point: 0,
                flags,
                pad,
            };
            c.ioctl(DRM_IOCTL_SYNCOBJ_TRANSFER, &mut req)
        };
        let point = |handle: u32| {
            let mut points = [0xdeadu64];
            timeline_array(&c, DRM_IOCTL_SYNCOBJ_QUERY, &[handle], &mut points).expect("QUERY");
            points[0]
        };
        let einval = Err(FsError::InvalidParam);

        assert_eq!(transfer(dst, signaled, 0, 0, 1), einval, "padding");
        for bad in [1u32, 4, 0x8000_0000] {
            assert_eq!(
                transfer(dst, signaled, 0, bad, 0),
                einval,
                "flags {:#x}",
                bad
            );
        }
        assert_eq!(
            transfer(dst, fresh, 0, 0, 0),
            einval,
            "a source with no fence"
        );
        timeline_array(
            &c,
            DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
            &[timeline],
            &mut [3u64],
        )
        .expect("TIMELINE_SIGNAL 3");
        assert_eq!(
            transfer(dst, timeline, 5, 0, 0),
            einval,
            "a point nothing has submitted"
        );
        assert_eq!(point(dst), 0, "none of the refused transfers touched dst");

        assert_eq!(transfer(dst, timeline, 3, 0, 0), Ok(0), "a submitted point");
        assert_eq!(point(dst), 1);
        let dst2 = create(&c, false);
        assert_eq!(
            transfer(dst2, signaled, 0, 0, 0),
            Ok(0),
            "a binary with a fence"
        );
        assert_eq!(point(dst2), 1);

        // WAIT_FOR_SUBMIT: the transfer waits for point 5 to be submitted.
        let dst3 = create(&c, false);
        assert_eq!(
            transfer(dst3, timeline, 5, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, 0),
            Ok(0)
        );
        assert_eq!(point(dst3), 0, "not yet");
        timeline_array(
            &c,
            DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
            &[timeline],
            &mut [5u64],
        )
        .expect("TIMELINE_SIGNAL 5");
        assert_eq!(point(dst3), 1, "and then it lands");

        for h in [signaled, fresh, timeline, dst, dst2, dst3] {
            destroy(&c, h);
        }
    }

    /// The flag and padding rules of `drm_syncobj.c`, ioctl by ioctl:
    /// CREATE takes SIGNALED alone, DESTROY/RESET/SIGNAL want their pad
    /// empty, TIMELINE_SIGNAL has no flags, QUERY takes LAST_SUBMITTED alone,
    /// WAIT takes ALL, FOR_SUBMIT and DEADLINE, and TIMELINE_WAIT those plus
    /// AVAILABLE. Every one of them was accepted here; the binary WAIT even
    /// honoured AVAILABLE.
    #[test]
    fn every_syncobj_ioctl_refuses_the_flags_and_padding_linux_refuses() {
        let _serialised = drm::test_globals::lock();
        let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
        let _on = NouveauOn::new();
        let c = Client::open(0);
        // CREATE.
        for bad in [2u32, 3, 0x8000_0000] {
            let mut req = DrmSyncobjCreate {
                handle: 0,
                flags: bad,
            };
            assert_eq!(
                c.ioctl(DRM_IOCTL_SYNCOBJ_CREATE, &mut req),
                Err(FsError::InvalidParam),
                "CREATE flags {:#x}",
                bad
            );
        }
        let s = create(&c, true);
        let t = create(&c, false);
        // DESTROY with a dirty pad: refused, and the handle survives.
        let mut req = DrmSyncobjDestroy { handle: s, pad: 1 };
        assert_eq!(
            c.ioctl(DRM_IOCTL_SYNCOBJ_DESTROY, &mut req),
            Err(FsError::InvalidParam)
        );
        assert_eq!(wait(&c, &[s], 0), Ok(0), "still there, still signaled");
        // RESET / SIGNAL with a dirty pad: refused, and nothing happens.
        for cmd in [DRM_IOCTL_SYNCOBJ_RESET, DRM_IOCTL_SYNCOBJ_SIGNAL] {
            let handles = [s];
            let mut req = DrmSyncobjArray {
                handles: handles.as_ptr() as u64,
                count_handles: 1,
                pad: 7,
            };
            assert_eq!(c.ioctl(cmd, &mut req), Err(FsError::InvalidParam));
        }
        assert_eq!(wait(&c, &[s], 0), Ok(0), "the refused RESET reset nothing");
        // TIMELINE_SIGNAL / QUERY flags.
        let handles = [t];
        let mut points = [5u64];
        let mut req = DrmSyncobjTimelineArray {
            handles: handles.as_ptr() as u64,
            points: points.as_mut_ptr() as u64,
            count_handles: 1,
            flags: 1,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &mut req),
            Err(FsError::InvalidParam),
            "TIMELINE_SIGNAL has no flags"
        );
        for bad in [2u32, 3, 0x100] {
            req.flags = bad;
            assert_eq!(
                c.ioctl(DRM_IOCTL_SYNCOBJ_QUERY, &mut req),
                Err(FsError::InvalidParam),
                "QUERY flags {:#x}",
                bad
            );
        }
        req.flags = DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED;
        assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_QUERY, &mut req), Ok(0));
        assert_eq!(
            points[0], 0,
            "never signaled: the refused signal did not land"
        );
        // WAIT: AVAILABLE is the timeline form's; DEADLINE is a hint on both.
        assert_eq!(
            wait(&c, &[s], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE),
            Err(FsError::InvalidParam)
        );
        for bad in [0x10u32, 0x20, 0x8000_0000] {
            assert_eq!(
                wait(&c, &[s], bad),
                Err(FsError::InvalidParam),
                "WAIT {:#x}",
                bad
            );
            assert_eq!(
                timeline_wait(&c, &[s], &[1], bad),
                Err(FsError::InvalidParam),
                "TIMELINE_WAIT {:#x}",
                bad
            );
        }
        assert_eq!(
            wait(&c, &[s], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE),
            Ok(0),
            "DEADLINE is accepted"
        );
        assert_eq!(
            wait(&c, &[s], DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT),
            Ok(0),
            "FOR_SUBMIT is accepted"
        );
        assert_eq!(
            timeline_wait(
                &c,
                &[s],
                &[1],
                DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE
            ),
            Ok(0)
        );
        // An empty array still goes through the flag check first.
        assert_eq!(wait(&c, &[], 0x10), Err(FsError::InvalidParam));
        destroy(&c, s);
        destroy(&c, t);
    }
}

#[cfg(test)]
mod addfb2_validation_tests {
    //! What `ADDFB2` refuses, driven through the ioctl entry point the way
    //! `drmModeAddFB2WithModifiers` drives it. Linux's
    //! `drm_internal_framebuffer_create` / `framebuffer_check` reject an
    //! unknown flag, a modifier without `DRM_MODE_FB_MODIFIERS`, a format no
    //! plane scans out, a zero dimension, a missing handle, a pitch shorter
    //! than a row, and anything on the planes the format does not have,
    //! every one of them EINVAL and none of them creating a framebuffer.
    //! This arm used to look only at the handle and the pitch: a client
    //! probing formats got a framebuffer of `NV12` that scanned out as
    //! `XRGB8888`, and the test helper above registered `XRC4`, a fourcc
    //! that does not exist, for a hundred tests without anyone noticing.
    use super::gl_client_sequence_tests::Client;
    use super::*;

    /// `DRM_FORMAT_NV12`: a real fourcc, two planes, and nothing here scans
    /// it out.
    const DRM_FORMAT_NV12: u32 = 0x3231_564e;
    /// The fourcc the test helper used to send, which is no format at all.
    const NOT_A_FOURCC: u32 = 0x3443_5258;

    fn cmd(buf: &DrmModeCreateDumb) -> DrmModeFbCmd2 {
        DrmModeFbCmd2 {
            fb_id: 0,
            width: buf.width,
            height: buf.height,
            pixel_format: drm::DRM_FORMAT_XRGB8888,
            flags: 0,
            handles: [buf.handle, 0, 0, 0],
            pitches: [buf.pitch, 0, 0, 0],
            offsets: [0; 4],
            modifier: [0; 4],
        }
    }

    /// `ADDFB2` with `cmd`, and the framebuffer table before and after.
    fn addfb2(client: &Client, cmd: &mut DrmModeFbCmd2) -> (Result<usize>, usize, usize) {
        let before = drm::table_sizes_for_test().0;
        let r = client.ioctl(DRM_IOCTL_MODE_ADDFB2, cmd);
        (r, before, drm::table_sizes_for_test().0)
    }

    /// Every refusal is the same three things: EINVAL, no fb id written,
    /// and the framebuffer table untouched.
    #[track_caller]
    fn refused(client: &Client, mut cmd: DrmModeFbCmd2, what: &str) {
        let (r, before, after) = addfb2(client, &mut cmd);
        assert_eq!(r, Err(FsError::InvalidParam), "{what}: not EINVAL");
        assert_eq!(cmd.fb_id, 0, "{what}: an fb id came back with the error");
        assert_eq!(after, before, "{what}: a framebuffer was created anyway");
    }

    #[track_caller]
    fn accepted(client: &Client, mut cmd: DrmModeFbCmd2, what: &str) -> u32 {
        let (r, before, after) = addfb2(client, &mut cmd);
        assert_eq!(r, Ok(0), "{what}: refused");
        assert_ne!(cmd.fb_id, 0, "{what}: no fb id");
        assert_eq!(after, before + 1, "{what}: no framebuffer in the table");
        cmd.fb_id
    }

    fn getfb2(client: &Client, fb_id: u32) -> DrmModeFbCmd2 {
        let mut q = DrmModeFbCmd2 {
            fb_id,
            width: 0,
            height: 0,
            pixel_format: 0,
            flags: 0,
            handles: [0; 4],
            pitches: [0; 4],
            offsets: [0; 4],
            modifier: [0; 4],
        };
        client.ioctl(DRM_IOCTL_MODE_GETFB2, &mut q).expect("GETFB2");
        q
    }

    fn getfb_depth(client: &Client, fb_id: u32) -> u32 {
        let mut q = DrmModeFbCmd {
            fb_id,
            width: 0,
            height: 0,
            pitch: 0,
            bpp: 0,
            depth: 0,
            handle: 0,
        };
        client.ioctl(DRM_IOCTL_MODE_GETFB, &mut q).expect("GETFB");
        q.depth
    }

    /// A format no plane scans out is EINVAL, and so is a fourcc that is
    /// not a format. The two this tree knows go through, and `GETFB2` gives
    /// each one back as registered, with `GETFB` reporting the depth Linux
    /// derives from it (24 without alpha, 32 with).
    #[test]
    fn only_the_scanout_formats_are_accepted_and_come_back_as_registered() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);

        let mut nv12 = cmd(&buf);
        nv12.pixel_format = DRM_FORMAT_NV12;
        refused(&client, nv12, "NV12");
        let mut xrc4 = cmd(&buf);
        xrc4.pixel_format = NOT_A_FOURCC;
        refused(&client, xrc4, "XRC4");
        let mut zero = cmd(&buf);
        zero.pixel_format = 0;
        refused(&client, zero, "format 0");

        let xr24 = accepted(&client, cmd(&buf), "XR24");
        let mut ar = cmd(&buf);
        ar.pixel_format = drm::DRM_FORMAT_ARGB8888;
        let ar24 = accepted(&client, ar, "AR24");

        assert_eq!(getfb2(&client, xr24).pixel_format, drm::DRM_FORMAT_XRGB8888);
        assert_eq!(getfb2(&client, ar24).pixel_format, drm::DRM_FORMAT_ARGB8888);
        assert_eq!(
            getfb_depth(&client, xr24),
            24,
            "XR24 has no alpha: depth 24"
        );
        assert_eq!(getfb_depth(&client, ar24), 32, "AR24 has alpha: depth 32");

        assert_eq!(client.rmfb(xr24), Ok(0));
        assert_eq!(client.rmfb(ar24), Ok(0));
        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    }

    /// `DRM_MODE_FB_INTERLACED` is a flag this tree accepts (and ignores, as
    /// most drivers do). `DRM_MODE_FB_MODIFIERS` is refused because no
    /// modifier is supported, and so is any bit Linux does not define.
    #[test]
    fn interlaced_is_the_only_flag_a_client_may_set() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);

        let mut interlaced = cmd(&buf);
        interlaced.flags = DRM_MODE_FB_INTERLACED;
        let fb = accepted(&client, interlaced, "INTERLACED");
        assert_eq!(client.rmfb(fb), Ok(0));

        let mut modifiers = cmd(&buf);
        modifiers.flags = DRM_MODE_FB_MODIFIERS;
        refused(&client, modifiers, "MODIFIERS with a linear modifier");

        let mut unknown = cmd(&buf);
        unknown.flags = 1 << 2;
        refused(&client, unknown, "flag bit 2");
        let mut high = cmd(&buf);
        high.flags = 1 << 31;
        refused(&client, high, "flag bit 31");
        let mut both = cmd(&buf);
        both.flags = DRM_MODE_FB_INTERLACED | (1 << 5);
        refused(&client, both, "INTERLACED plus an unknown bit");

        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    }

    /// `DRM_CAP_ADDFB2_MODIFIERS` as a client reads it.
    fn addfb2_modifiers_cap(c: &Client) -> u64 {
        let mut req = DrmGetCap {
            capability: 0x10,
            value: 0xdead_beef,
        };
        c.ioctl(DRM_IOCTL_GET_CAP, &mut req).expect("GET_CAP");
        req.value
    }

    /// With modifiers off -- the default -- the cap and `ADDFB2` have to
    /// give the same answer: a client that asked `DRM_CAP_ADDFB2_MODIFIERS`
    /// and was told 0 must not then find `DRM_MODE_FB_MODIFIERS` accepted,
    /// and the other way round.
    ///
    /// With them on, a framebuffer whose modifier names a layout this GPU
    /// really produces is accepted, and the present declines it, which is
    /// how a DRM driver says "not scanout-able". Accepting it and copying it
    /// as if it were pitched is the desktop full of garbage this gate
    /// exists to prevent.
    #[test]
    fn the_modifier_cap_and_what_addfb2_takes_are_the_same_switch() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);

        // DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 0): one GOB
        // per block, so the surface is its own 64x8 block and a 64x64 dumb
        // buffer is big enough. The pitch counts BLOCKS: 64 pixels of 4
        // bytes is 256 bytes, which is 4 blocks.
        let turing = nvidia_block_linear_2d(0, 1, 2, 0x06, 0);
        let tiled = |b: &DrmModeCreateDumb| {
            let mut c = cmd(b);
            c.flags = DRM_MODE_FB_MODIFIERS;
            c.modifier[0] = turing;
            c.pitches[0] = 4;
            c
        };

        // Off (the default).
        assert!(!drm::scanout_modifiers_enabled());
        assert_eq!(addfb2_modifiers_cap(&client), 0, "DRM_CAP_ADDFB2_MODIFIERS");
        refused(&client, tiled(&buf), "a modifier while the cap says 0");
        let mut linear_flagged = cmd(&buf);
        linear_flagged.flags = DRM_MODE_FB_MODIFIERS;
        refused(
            &client,
            linear_flagged,
            "even DRM_FORMAT_MOD_LINEAR behind the flag, while the cap says 0",
        );

        // On.
        drm::set_scanout_modifiers_enabled(true);
        assert_eq!(addfb2_modifiers_cap(&client), 1, "DRM_CAP_ADDFB2_MODIFIERS");
        let fb = accepted(&client, tiled(&buf), "a Turing block-linear modifier");

        // ...and the present declines it rather than painting it.
        assert_eq!(
            drm::present_now_checked(fb, drm::SYNTH_CRTC_ID, None),
            Err(drm::PresentError::UnsupportedLayout),
            "a tiled framebuffer must not be copied as if it were pitched"
        );
        // A linear one alongside it is NOT refused for its layout, so the
        // decline is about the tiling and not about the flag being on. (It
        // still fails for want of an emulated display in this test, which is
        // a different error and the point: the layout check comes first,
        // because an unreadable layout is the framebuffer's own defect and
        // holds whether or not anything is plugged in.)
        let mut linear = cmd(&buf);
        linear.flags = DRM_MODE_FB_MODIFIERS;
        let plain = accepted(&client, linear, "DRM_FORMAT_MOD_LINEAR with the flag");
        assert_ne!(
            drm::present_now_checked(plain, drm::SYNTH_CRTC_ID, None),
            Err(drm::PresentError::UnsupportedLayout),
            "a linear framebuffer is readable whatever the flag says"
        );

        drm::set_scanout_modifiers_enabled(false);
    }

    /// `GETFB2` has to describe the framebuffer that exists, modifier and
    /// all. Reporting 0 for a tiled one describes a DIFFERENT surface --
    /// same handle, same pitch number, linear -- and a client that
    /// re-creates it from the readback gets the garbage this layout is
    /// gated against.
    #[test]
    fn a_tiled_framebuffer_reads_back_as_the_modifier_it_was_made_with() {
        let _serialised = drm::test_globals::lock();
        drm::set_scanout_modifiers_enabled(true);
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);

        for h in 0..=2u64 {
            let turing = nvidia_block_linear_2d(0, 1, 2, 0x06, h);
            let mut c = cmd(&buf);
            c.flags = DRM_MODE_FB_MODIFIERS;
            c.modifier[0] = turing;
            c.pitches[0] = 4;
            let fb = accepted(&client, c, "a Turing block-linear modifier");
            let back = getfb2(&client, fb);
            assert_eq!(back.modifier[0], turing, "h={} did not round-trip", h);
            assert_ne!(
                back.flags & DRM_MODE_FB_MODIFIERS,
                0,
                "h={}: the modifier is only meaningful with the flag",
                h
            );
        }

        // And a linear framebuffer still reads back as one, with no flag.
        let plain = accepted(&client, cmd(&buf), "a plain linear framebuffer");
        let back = getfb2(&client, plain);
        assert_eq!(back.modifier[0], 0);
        assert_eq!(back.flags & DRM_MODE_FB_MODIFIERS, 0);

        drm::set_scanout_modifiers_enabled(false);
    }

    /// `GETFB2` answers the pitch in the units `ADDFB2` took it, which for a
    /// tiled framebuffer is 64-byte blocks.
    ///
    /// The framebuffer stores bytes, so reporting the stored number straight
    /// describes a surface 64 times wider than the one that exists -- and a
    /// client that re-creates the framebuffer from its own readback gets
    /// EINVAL at best and the wrong surface at worst. Round-tripping it is
    /// the whole point of answering the modifier in the first place.
    #[test]
    fn a_tiled_framebuffer_reads_back_the_pitch_it_was_made_with() {
        let _serialised = drm::test_globals::lock();
        drm::set_scanout_modifiers_enabled(true);
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);
        let turing = nvidia_block_linear_2d(0, 1, 2, 0x06, 0);

        // 64 pixels x 4 bytes = 256 bytes = 4 blocks of 64.
        let mut c = cmd(&buf);
        c.flags = DRM_MODE_FB_MODIFIERS;
        c.modifier[0] = turing;
        c.pitches[0] = 4;
        let fb = accepted(&client, c, "a Turing block-linear modifier");
        assert_eq!(
            getfb2(&client, fb).pitches[0],
            4,
            "the readback has to be in blocks, like the request was"
        );

        // A linear framebuffer keeps speaking bytes.
        let plain = accepted(&client, cmd(&buf), "a plain linear framebuffer");
        assert_eq!(getfb2(&client, plain).pitches[0], cmd(&buf).pitches[0]);

        drm::set_scanout_modifiers_enabled(false);
    }

    /// The pitch of a block-linear framebuffer counts 64-byte blocks, so the
    /// "shorter than a row" check has to multiply before it compares.
    /// Reading it as bytes rejects every real tiled framebuffer by a factor
    /// of 64; not reading it at all accepts one 64 times too small and lets
    /// the present walk off the buffer.
    #[test]
    fn a_block_linear_pitch_is_counted_in_blocks() {
        let _serialised = drm::test_globals::lock();
        drm::set_scanout_modifiers_enabled(true);
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);
        let turing = nvidia_block_linear_2d(0, 1, 2, 0x06, 0);
        let with_pitch = |blocks: u32| {
            let mut c = cmd(&buf);
            c.flags = DRM_MODE_FB_MODIFIERS;
            c.modifier[0] = turing;
            c.pitches[0] = blocks;
            c
        };

        // 64 pixels x 4 bytes = 256 bytes = 4 blocks. Four is exactly a row.
        let fb = accepted(&client, with_pitch(4), "a pitch of exactly one row");
        assert_ne!(fb, 0);
        // Three blocks is 192 bytes, short of the 256 a row needs.
        refused(&client, with_pitch(3), "a pitch shorter than a row");
        // And the byte count that would be right for a LINEAR fb is 256
        // blocks, i.e. 16 KiB per row -- far past this 16 KiB buffer once
        // the eight rows of the block are counted.
        refused(
            &client,
            with_pitch(256),
            "a pitch given in bytes by mistake",
        );

        drm::set_scanout_modifiers_enabled(false);
    }

    /// A modifier without the flag, and anything at all on planes 1 to 3 of
    /// a one-plane format, are `framebuffer_check`'s "bad fb modifier" and
    /// "buffer object handle for plane N" refusals.
    #[test]
    fn a_modifier_or_a_second_plane_on_a_linear_one_plane_format_is_refused() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);

        // DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0,0,0,0,0): a real modifier.
        let mut tiled = cmd(&buf);
        tiled.modifier[0] = 0x0300_0000_0000_0010;
        refused(&client, tiled, "a tiled modifier without the flag");
        let mut inv = cmd(&buf);
        inv.modifier[0] = u64::MAX;
        refused(&client, inv, "DRM_FORMAT_MOD_INVALID");

        for plane in 1..4 {
            let mut h = cmd(&buf);
            h.handles[plane] = buf.handle;
            refused(&client, h, "a handle on an extra plane");
            let mut p = cmd(&buf);
            p.pitches[plane] = buf.pitch;
            refused(&client, p, "a pitch on an extra plane");
            let mut o = cmd(&buf);
            o.offsets[plane] = 64;
            refused(&client, o, "an offset on an extra plane");
            let mut m = cmd(&buf);
            m.modifier[plane] = 1;
            refused(&client, m, "a modifier on an extra plane");
        }

        // What a real NV12 client sends: two planes, second handle and
        // pitch set. Refused for the format before the planes are looked
        // at, and still refused.
        let mut nv12 = cmd(&buf);
        nv12.pixel_format = DRM_FORMAT_NV12;
        nv12.handles[1] = buf.handle;
        nv12.pitches[1] = buf.pitch;
        nv12.offsets[1] = buf.pitch * 64;
        refused(&client, nv12, "a two-plane NV12");

        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    }

    /// Plane 0 itself: a zero dimension, no handle, a pitch shorter than a
    /// row of pixels, or an offset into the buffer (which this tree does not
    /// scan out from) are all EINVAL, where the short pitch used to be
    /// EFAULT-flavoured `DeviceError` and the rest were accepted.
    #[test]
    fn plane_zero_needs_a_size_a_handle_a_full_pitch_and_no_offset() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);

        let mut w0 = cmd(&buf);
        w0.width = 0;
        refused(&client, w0, "width 0");
        let mut h0 = cmd(&buf);
        h0.height = 0;
        refused(&client, h0, "height 0");
        let mut nh = cmd(&buf);
        nh.handles[0] = 0;
        refused(&client, nh, "handle 0");
        let mut short = cmd(&buf);
        short.pitches[0] = buf.width * 4 - 4;
        refused(&client, short, "a pitch one pixel short");
        let mut zero_pitch = cmd(&buf);
        zero_pitch.pitches[0] = 0;
        refused(&client, zero_pitch, "pitch 0");
        let mut wide = cmd(&buf);
        wide.width = u32::MAX;
        refused(&client, wide, "a width whose row overflows u32");
        // A width whose row, multiplied in u32, wraps to 64 bytes: the pitch
        // comparison has to be done wider than the fields are.
        let mut wrap = cmd(&buf);
        wrap.width = 0x4000_0010;
        refused(&client, wrap, "a width whose row wraps to a short one");
        let mut off = cmd(&buf);
        off.offsets[0] = 64;
        refused(&client, off, "an offset on plane 0");

        // The exact pitch is fine, and so is one wider than the row: a
        // narrow framebuffer over a wide buffer is what `addfb2_narrow`
        // registers for the alignment tests.
        let mut exact = cmd(&buf);
        exact.pitches[0] = buf.width * 4;
        let fb = accepted(&client, exact, "an exact pitch");
        assert_eq!(client.rmfb(fb), Ok(0));
        let mut narrow = cmd(&buf);
        narrow.width = 32;
        let fb = accepted(&client, narrow, "a narrow fb over a wide buffer");
        assert_eq!(client.rmfb(fb), Ok(0));

        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    }

    /// `ADDFB` with `bpp`/`depth`, and the framebuffer table before and
    /// after.
    fn addfb(
        client: &Client,
        buf: &DrmModeCreateDumb,
        bpp: u32,
        depth: u32,
    ) -> (Result<usize>, u32, usize, usize) {
        let mut cmd = DrmModeFbCmd {
            fb_id: 0,
            width: buf.width,
            height: buf.height,
            pitch: buf.pitch,
            bpp,
            depth,
            handle: buf.handle,
        };
        let before = drm::table_sizes_for_test().0;
        let r = client.ioctl(DRM_IOCTL_MODE_ADDFB, &mut cmd);
        (r, cmd.fb_id, before, drm::table_sizes_for_test().0)
    }

    /// `drm_mode_legacy_fb_format`'s table, and nothing outside it.
    #[test]
    fn the_legacy_bpp_depth_table_is_linuxs() {
        assert_eq!(legacy_fb_format(32, 24), Some(drm::DRM_FORMAT_XRGB8888));
        assert_eq!(legacy_fb_format(32, 32), Some(drm::DRM_FORMAT_ARGB8888));
        // Real formats no plane here scans out: fourccs, spelled as Linux
        // spells them.
        assert_eq!(legacy_fb_format(8, 8), Some(u32::from_le_bytes(*b"C8  ")));
        assert_eq!(legacy_fb_format(16, 15), Some(u32::from_le_bytes(*b"XR15")));
        assert_eq!(legacy_fb_format(16, 16), Some(u32::from_le_bytes(*b"RG16")));
        assert_eq!(legacy_fb_format(24, 24), Some(u32::from_le_bytes(*b"RG24")));
        assert_eq!(legacy_fb_format(32, 30), Some(u32::from_le_bytes(*b"XR30")));
        for (bpp, depth) in [
            (0, 0),
            (32, 16),
            (16, 24),
            (24, 32),
            (64, 64),
            (32, 0),
            (0, 24),
        ] {
            assert_eq!(legacy_fb_format(bpp, depth), None, "{bpp}/{depth}");
        }
    }

    /// The legacy `ADDFB` is an `ADDFB2` with the fourcc derived from
    /// (bpp, depth): 32/24 registers XRGB8888 and 32/32 ARGB8888 (`GETFB2`
    /// gives the format back and `GETFB` the depth), every other pair is
    /// EINVAL with no framebuffer created, whether the table knows it
    /// (16/16 is RGB565, which nothing here scans out) or not (32/16), and
    /// a pitch shorter than a row is refused as it is on `ADDFB2`. This
    /// arm read neither field: a 16-bit framebuffer scanned out as
    /// XRGB8888 garbage, and an ARGB8888 one came back as depth 24.
    #[test]
    fn legacy_addfb_derives_the_format_from_bpp_and_depth() {
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let buf = client.create_dumb(64, 64);

        let (r, fb, before, after) = addfb(&client, &buf, 32, 24);
        assert_eq!(r, Ok(0));
        assert_eq!(after, before + 1);
        assert_eq!(getfb2(&client, fb).pixel_format, drm::DRM_FORMAT_XRGB8888);
        assert_eq!(getfb_depth(&client, fb), 24);
        assert_eq!(client.rmfb(fb), Ok(0));

        let (r, fb, before, after) = addfb(&client, &buf, 32, 32);
        assert_eq!(r, Ok(0));
        assert_eq!(after, before + 1);
        assert_eq!(getfb2(&client, fb).pixel_format, drm::DRM_FORMAT_ARGB8888);
        assert_eq!(getfb_depth(&client, fb), 32, "32/32 is ARGB8888, depth 32");
        assert_eq!(client.rmfb(fb), Ok(0));

        for (bpp, depth) in [
            (16, 16),
            (16, 15),
            (24, 24),
            (8, 8),
            (32, 30),
            (32, 16),
            (0, 0),
        ] {
            let (r, fb, before, after) = addfb(&client, &buf, bpp, depth);
            assert_eq!(r, Err(FsError::InvalidParam), "{bpp}/{depth}: not EINVAL");
            assert_eq!(fb, 0, "{bpp}/{depth}: an fb id came back with the error");
            assert_eq!(
                after, before,
                "{bpp}/{depth}: a framebuffer was created anyway"
            );
        }

        // The ADDFB2 checks apply to the legacy form too: a pitch shorter
        // than a row of 4-byte pixels.
        let mut short = DrmModeFbCmd {
            fb_id: 0,
            width: buf.width,
            height: buf.height,
            pitch: buf.width * 4 - 4,
            bpp: 32,
            depth: 24,
            handle: buf.handle,
        };
        assert_eq!(
            client.ioctl(DRM_IOCTL_MODE_ADDFB, &mut short),
            Err(FsError::InvalidParam)
        );
        assert_eq!(short.fb_id, 0);

        assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
    }
}

#[cfg(test)]
mod trail_tests {
    //! What the crash-time trail records about an ioctl's outcome.
    //!
    //! The recording itself needs a process and a user address space, so what
    //! is checked here is the part that does not: the translation from an
    //! ioctl `Result` to the number userspace saw, and the refusal to read an
    //! argument struct that has no first word.

    use super::*;

    /// The negative errno, not a bare -1: the whole point of the trail is that
    /// a reader can tell `EINVAL` (userspace asked for something we do not
    /// have) from `ENOENT` (it named an object that is gone).
    #[test]
    fn a_failed_ioctl_is_recorded_as_its_own_errno() {
        assert_eq!(trail_ret(&Err(FsError::InvalidParam)), -22);
        assert_eq!(trail_ret(&Err(FsError::EntryNotFound)), -2);
        assert_eq!(trail_ret(&Err(FsError::NotSupported)), -38);
        assert_eq!(trail_ret(&Err(FsError::BadAddress)), -14);
    }

    #[test]
    fn a_successful_ioctl_is_recorded_as_its_return_value() {
        assert_eq!(trail_ret(&Ok(0)), 0);
        assert_eq!(trail_ret(&Ok(7)), 7);
    }

    /// `MODE_RMFB` and friends carry a bare `__u32`. Reading eight bytes off
    /// one would run past the struct the client allocated, so an argument that
    /// short has no first word at all.
    #[test]
    fn an_argument_struct_shorter_than_a_word_has_no_first_word() {
        // `_IOC_SIZE` 4 (`MODE_RMFB`) and 0 (`SET_MASTER`), at a plausible
        // user address: the size is what refuses them, not the address.
        assert!(!arg_word_readable(0xC004_64AF, 0x1000));
        assert!(!arg_word_readable(0x0000_641E, 0x1000));
        // 16 bytes (`GETPARAM`) at the same address is the case that does get
        // read, so the test above is about the size and nothing else.
        assert!(arg_word_readable(0xC010_6440, 0x1000));
    }

    /// A null pointer is never read: `ucheck` is the same gate the dispatch
    /// arms use, and a trail entry is not worth a fault.
    ///
    /// Only the null case is asserted. The upper bound of the user range is
    /// the host's under `libos`, where `user_range_ok` accepts every address,
    /// so a "kernel address is refused" assertion here would be testing this
    /// build rather than the rule.
    #[test]
    fn a_null_argument_is_not_read() {
        assert!(!arg_word_readable(0xC010_6440, 0));
        assert_eq!(first_arg_word(0xC010_6440, 0), 0);
    }
}

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
mod pre_wait_resolve_tests {
    /// The bodies of the pre-waits that probe with `wait_ready`, from their
    /// signature to the start of the next item at the same indentation.
    /// `#[cfg(test)]` cannot delimit them: the first one in this file is on
    /// line 10, so splitting there would leave nothing to look at and the
    /// test would pass on anything.
    fn body<'a>(src: &'a str, signature: &str) -> &'a str {
        let after = src
            .split_once(signature)
            .unwrap_or_else(|| panic!("{} is no longer in this file", signature))
            .1;
        let end = after.find("\n    /// ").unwrap_or(after.len());
        &after[..end]
    }

    #[test]
    fn a_pre_wait_does_not_resolve_the_table_twice_per_look() {
        let src = include_str!("drm_scheme.rs");
        for name in [
            "pub async fn syncobj_wait_sleep(",
            "pub async fn atomic_in_fence_sleep(",
        ] {
            let body = body(src, name);
            assert!(
                body.contains("wait_ready(") || body.contains("ready_fn("),
                "{} no longer probes with wait_ready: this test is measuring nothing",
                name
            );
            let hits = body
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.contains("poll_pending("))
                .count();
            assert_eq!(
                hits, 0,
                "a poll_pending() is back in {}: wait_ready already resolves \
                 the table, so this is a second walk of the pending list and a \
                 second take of the lock the signalling side needs",
                name
            );
        }
    }

    /// A pre-wait that holds a LIST of fences reads each landing zone once
    /// per look, not once per fence.
    ///
    /// `fences.iter().all(hw_fence_landed)` reads the zone once per entry, and
    /// these lists name one zone twice whenever two submits of one ring wrote
    /// the buffer -- a channel has a single semaphore word. `hw_fences_landed`
    /// gives the same answer off one read per address. The bench cannot reach
    /// here (these need a live `DrmDev`, a process and a GPU) and what they
    /// cost is a count, not an answer, so the shape is what is guarded; the
    /// count itself is measured next to the function, in
    /// `a_list_of_fences_reads_each_landing_zone_once_and_answers_like_all`.
    #[test]
    fn a_pre_wait_on_a_list_of_fences_reads_each_landing_zone_once() {
        let src = include_str!("drm_scheme.rs");
        for name in [
            "pub async fn cpu_prep_sleep(",
            "pub async fn present_fence_sleep(",
        ] {
            let body = body(src, name);
            let code = || {
                body.lines()
                    .filter(|l| !l.trim_start().starts_with("//"))
                    .collect::<alloc::string::String>()
            };
            assert!(
                code().contains("hw_fences_landed("),
                "{} no longer waits with hw_fences_landed",
                name
            );
            // Named exactly: `hw_fence_landed(` is the per-fence call, and
            // `hw_fences_landed(` -- with the s -- is not a superstring of
            // it, so this catches the reversion and nothing else. Looking for
            // a bare `all(` as well would fail on any unrelated `all` a later
            // refactor puts in these bodies.
            assert!(
                !code().contains("hw_fence_landed("),
                "{} is back to reading its landing zones one fence at a time",
                name
            );
        }
    }

    /// Every pre-wait is interruptible, and so is the backoff the fence ones
    /// share.
    ///
    /// These five are the only places in this kernel where a thread sleeps
    /// with no fd behind it and no readiness waker to fire: nothing but the
    /// fence it waits for, or its own deadline, ever ends them. They used to
    /// ask nothing about signals, so a `^C` on a GL client did nothing until
    /// the wait finished on its own -- three seconds for `WAIT_VBLANK`, the
    /// client's own deadline for the syncobj waits, `GEM_CPU_PREP`'s own for
    /// that one. On a desktop that is the first `^C` on `glxgears` landing
    /// nowhere and the second one killing it.
    ///
    /// Linux sleeps all of these interruptibly and answers `-ERESTARTSYS`
    /// (`drm_syncobj_wait`, `drm_wait_vblank`), and libdrm's `drmIoctl()`
    /// retries on `EINTR`, so the only caller that notices the change is the
    /// one being killed -- which is the point: the signal is taken at the
    /// syscall boundary.
    ///
    /// Nothing can run these in a test (they need a live `DrmDev`, a process
    /// and a GPU), so the guard is on the shape, like the two above.
    #[test]
    fn every_pre_wait_can_be_interrupted_by_a_signal() {
        let src = include_str!("drm_scheme.rs");
        for name in [
            "pub async fn wait_vblank_sleep(",
            "pub async fn syncobj_wait_sleep(",
            "pub async fn atomic_in_fence_sleep(",
            "pub async fn cpu_prep_sleep(",
            "pub async fn present_fence_sleep(",
        ] {
            let body = body(src, name);
            assert!(
                body.lines()
                    .next()
                    .unwrap_or_default()
                    .contains("-> LxResult<()>"),
                "{} has no way to tell its caller a signal arrived, so the \
                 ioctl cannot answer EINTR",
                name
            );
            let code: alloc::string::String = body
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect();
            // A bare `sleep_until` is a sleep no signal can cut short, which
            // is exactly what these were.
            assert_eq!(
                code.matches("kernel_hal::thread::sleep_until(").count(),
                code.matches("interruptible(kernel_hal::thread::sleep_until(")
                    .count(),
                "{} sleeps somewhere a signal cannot reach",
                name
            );
            assert!(
                code.contains("interruptible(kernel_hal::thread::sleep_until(")
                    || code.contains("fence_poll_wait(acct.probes, deadline).await?"),
                "{} waits without ever asking whether a signal arrived",
                name
            );
        }
        // The backoff the four fence waits park in. Sliced by hand: `body`
        // stops at the next indented doc comment, and this one is a free
        // function whose neighbours are not indented.
        let fpw = src
            .split_once("async fn fence_poll_wait(")
            .expect("fence_poll_wait is no longer in this file")
            .1;
        let fpw = &fpw[..fpw.find("\n}\n").expect("an unterminated function")];
        assert!(
            fpw.contains("check_signals()?"),
            "fence_poll_wait no longer asks about signals, so a client in its \
             busy phase takes none for the whole frame"
        );
        assert!(
            fpw.contains("interruptible(kernel_hal::thread::sleep_until("),
            "fence_poll_wait is back to an uninterruptible sleep"
        );
    }

    /// A wait satisfied on its first look reports no parking, however long
    /// its argument took to read; one that looked twice reports the lot.
    #[test]
    fn only_a_wait_that_looked_twice_reports_parked_time() {
        use super::pre_wait_parked_us;
        assert_eq!(
            pre_wait_parked_us(37, 0),
            0,
            "argument parsing was charged as parking"
        );
        assert_eq!(pre_wait_parked_us(37, 1), 37);
        assert_eq!(
            pre_wait_parked_us(0, 9),
            0,
            "a coarse clock is not a reason to drop the park"
        );
    }

    /// Every pre-wait accounts itself, so `/proc/gpudbg` can say how much of
    /// a frame went into parking.
    ///
    /// A pre-wait parks the thread BEFORE the driver is called, so none of it
    /// lands in the driver's per-ioctl profile. One of these five left
    /// unaccounted is time that simply does not appear anywhere, and the
    /// symptom is a profile that adds up to far less than the frame -- which
    /// is exactly the question the table was added to answer. Nothing can run
    /// these functions in a test (they need a live `DrmDev`, a process and a
    /// GPU), so the guard is on the shape.
    #[test]
    fn every_pre_wait_accounts_its_parking() {
        let src = include_str!("drm_scheme.rs");
        for (name, kind) in [
            ("pub async fn wait_vblank_sleep(", "Kind::WaitVblank"),
            ("pub async fn syncobj_wait_sleep(", "Kind::SyncobjWait"),
            ("pub async fn atomic_in_fence_sleep(", "Kind::AtomicInFence"),
            ("pub async fn cpu_prep_sleep(", "Kind::CpuPrep"),
            ("pub async fn present_fence_sleep(", "Kind::PresentFence"),
        ] {
            let body = body(src, name);
            assert!(
                body.contains(kind),
                "{} does not open a PreWaitAccount for {}",
                name,
                kind
            );
            // An accountant that is never told about a probe reports every
            // park as a wait that was satisfied at once -- the counters would
            // be there and would say nothing.
            assert!(
                body.contains("acct.probe()"),
                "{} never counts a probe",
                name
            );
        }
        // And each kind is used by exactly one of them: two waits sharing a
        // kind would sum into one line and neither could be read.
        for kind in [
            "Kind::WaitVblank",
            "Kind::SyncobjWait",
            "Kind::AtomicInFence",
            "Kind::CpuPrep",
            "Kind::PresentFence",
        ] {
            assert_eq!(
                src.matches(&alloc::format!(
                    "PreWaitAccount::new(zcore_drivers::scheme::prewait::{})",
                    kind
                ))
                .count(),
                1,
                "{} is opened by more or fewer than one pre-wait",
                kind
            );
        }
    }
}

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
mod wsi_display_probe_tests {
    use super::gl_client_sequence_tests::{blank_card_res, Client};
    use super::*;
    use crate::fs::devfs::kms_emu::{self, EmuGpu};
    use alloc::vec::Vec;

    fn blank_connector(id: u32) -> DrmModeGetConnector {
        DrmModeGetConnector {
            encoders_ptr: 0,
            modes_ptr: 0,
            props_ptr: 0,
            prop_values_ptr: 0,
            count_modes: 0,
            count_props: 0,
            count_encoders: 0,
            encoder_id: 0,
            connector_id: id,
            connector_type: 0,
            connector_type_id: 0,
            connection: 0,
            mm_width: 0,
            mm_height: 0,
            subpixel: 0,
            pad: 0,
        }
    }

    /// `drmModeGetResources`, pass for pass. `None` is libdrm's NULL.
    fn drm_mode_get_resources(c: &Client) -> Option<(Vec<u32>, Vec<u32>, u32)> {
        for _ in 0..4 {
            let mut res = blank_card_res();
            c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut res).ok()?;
            let counts = res;
            let mut crtcs = alloc::vec![0u32; res.count_crtcs as usize];
            let mut conns = alloc::vec![0u32; res.count_connectors as usize];
            let mut encs = alloc::vec![0u32; res.count_encoders as usize];
            if res.count_crtcs != 0 {
                res.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
            }
            if res.count_connectors != 0 {
                res.connector_id_ptr = conns.as_mut_ptr() as u64;
            }
            if res.count_encoders != 0 {
                res.encoder_id_ptr = encs.as_mut_ptr() as u64;
            }
            c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut res).ok()?;
            if counts.count_crtcs < res.count_crtcs
                || counts.count_connectors < res.count_connectors
                || counts.count_encoders < res.count_encoders
            {
                continue; // libdrm's `goto retry`
            }
            // libdrm copies out only as many ids as the FILL pass reported
            // (`drmAllocCpy(ptr, res.count_x, ...)`), so a count that SHRANK
            // between the passes leaves the tail of the probe-sized buffer
            // untouched -- and a helper that returned it whole would hand the
            // caller trailing zeros and probe connector 0.
            crtcs.truncate(res.count_crtcs as usize);
            conns.truncate(res.count_connectors as usize);
            return Some((crtcs, conns, res.count_encoders));
        }
        None
    }

    /// `drmModeGetConnector`, pass for pass. `None` is libdrm's NULL, which is
    /// what Mesa turns into `VK_ERROR_OUT_OF_HOST_MEMORY`.
    fn drm_mode_get_connector(c: &Client, id: u32) -> Option<(u32, u32, Vec<u32>)> {
        for _ in 0..4 {
            let mut conn = blank_connector(id);
            c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn).ok()?;
            let counts = conn;
            let mut props = alloc::vec![0u32; conn.count_props as usize];
            let mut prop_values = alloc::vec![0u64; conn.count_props as usize];
            let mut modes = alloc::vec![0u8; conn.count_modes as usize * 68];
            let mut encoders = alloc::vec![0u32; conn.count_encoders as usize];
            if conn.count_props != 0 {
                conn.props_ptr = props.as_mut_ptr() as u64;
                conn.prop_values_ptr = prop_values.as_mut_ptr() as u64;
            }
            if conn.count_modes != 0 {
                conn.modes_ptr = modes.as_mut_ptr() as u64;
            }
            if conn.count_encoders != 0 {
                conn.encoders_ptr = encoders.as_mut_ptr() as u64;
            }
            c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn).ok()?;
            if counts.count_props < conn.count_props
                || counts.count_modes < conn.count_modes
                || counts.count_encoders < conn.count_encoders
            {
                continue;
            }
            props.truncate(conn.count_props as usize);
            return Some((conn.connection, conn.count_modes, props));
        }
        None
    }

    /// Mesa's `wsi_get_connectors`: every id `drmModeGetResources` advertised
    /// has to answer `drmModeGetConnector`, or the whole `VK_KHR_display`
    /// query dies with `ERROR_OUT_OF_HOST_MEMORY`.
    fn wsi_get_connectors(c: &Client) -> core::result::Result<usize, &'static str> {
        let (_, conns, _) = drm_mode_get_resources(c).ok_or("drmModeGetResources -> NULL")?;
        for id in &conns {
            if drm_mode_get_connector(c, *id).is_none() {
                return Err("drmModeGetConnector -> NULL");
            }
        }
        Ok(conns.len())
    }

    #[test]
    fn vulkaninfo_probes_the_software_kms_topology_without_an_error() {
        let _screen = kms_emu::attach(640, 480);
        let c = Client::open(0);
        assert_eq!(wsi_get_connectors(&c), Ok(1));
    }

    #[test]
    fn vulkaninfo_probes_a_hardware_kms_gpu_without_an_error() {
        let screen = kms_emu::attach(640, 480);
        let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
        let c = Client::open(0);
        assert!(
            wsi_get_connectors(&c).is_ok(),
            "{:?}",
            wsi_get_connectors(&c)
        );
    }

    /// The invariant: an id GETRESOURCES advertised stays answerable even if
    /// scanout ownership changes before the client's next ioctl.
    ///
    /// The lookups used to ask `software_kms_active()` before deciding whether
    /// a DRIVER id was answerable AT ALL, and the fallback behind that gate
    /// only knows the synthetic ids (1..4). So a client that read the topology
    /// with the driver owning scanout got driver ids, and the moment the
    /// answer flipped, every one of them came back EINVAL. Mesa's
    /// `wsi_get_connectors` reports a miss on an advertised id as
    /// `VK_ERROR_OUT_OF_HOST_MEMORY` for the whole `VK_KHR_display` query.
    ///
    /// On the flip's DIRECTION, because it matters for what this does and does
    /// not claim: in production `software_kms_active()` is
    /// `primary_display().is_some() && !drivers.first().has_hardware_kms()`,
    /// and the NVIDIA driver's `has_hardware_kms()` is
    /// `surfaceflip_enabled() && hwflip_ready()`. `g_hwflip.ready` is only
    /// ever assigned `NV_TRUE` and never cleared
    /// (`nvidia-rm-sys/vendor/eclipse_rm_init.c`), so THAT lever moves
    /// `software_kms_active()` true -> false only, which is the harmless
    /// direction: the synthetic fallback still answers the synthetic ids. The
    /// levers that can move it the other way are the boot framebuffer
    /// appearing and `register_driver` putting a different driver at index 0,
    /// both of which are boot-time events here. So this is a latent hole, not
    /// a proven cause of anything; the emulator moves the KMS flag because it
    /// is the one lever it has, and the code under test reads nothing but
    /// `software_kms_active()`.
    #[test]
    fn a_connector_stays_answerable_when_scanout_moves_mid_probe() {
        let screen = kms_emu::attach(640, 480);
        let gpu = screen.attach_gpu(EmuGpu::hardware_kms("gpu0").with_ids(2001, 1001, 3001));
        let c = Client::open(0);

        // What `drmModeGetResources` advertised while the driver owned scanout.
        let (crtcs, conns, _) = drm_mode_get_resources(&c).expect("GETRESOURCES");
        assert_eq!(conns, alloc::vec![1001]);
        assert_eq!(crtcs, alloc::vec![2001]);
        assert_eq!(drm_mode_get_plane_resources(&c), alloc::vec![3001]);

        // The flip ladder goes away between the two ioctls, as it does when
        // `hwflip_ready()` has not latched yet.
        gpu.set_hardware_kms(false);

        assert!(
            drm_mode_get_connector(&c, 1001).is_some(),
            "GETCONNECTOR refused an id GETRESOURCES had just advertised: \
             that is the EINVAL Mesa reports as ERROR_OUT_OF_HOST_MEMORY"
        );
        assert_eq!(wsi_get_connectors(&c), Ok(1));

        // The same window exists for the other two object types a compositor
        // reads back after GETRESOURCES/GETPLANERESOURCES.
        let mut crtc = blank_get_crtc(2001);
        assert!(
            c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).is_ok(),
            "GETCRTC refused an advertised CRTC id"
        );
        let mut plane = blank_get_plane(3001);
        assert!(
            c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut plane).is_ok(),
            "GETPLANE refused an advertised plane id"
        );
    }

    fn blank_get_crtc(id: u32) -> DrmModeGetCrtc {
        DrmModeGetCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id: id,
            fb_id: 0,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: [0u8; 68],
        }
    }

    fn blank_get_plane(id: u32) -> DrmModeGetPlane {
        DrmModeGetPlane {
            plane_id: id,
            crtc_id: 0,
            fb_id: 0,
            possible_crtcs: 0,
            gamma_size: 0,
            count_format_types: 0,
            format_type_ptr: 0,
        }
    }

    /// `drmModeGetPlaneResources`, both passes, after the one call every
    /// plane-aware client makes first: `drm_mode_getplane_res` lists only
    /// overlays to a file without `DRM_CLIENT_CAP_UNIVERSAL_PLANES`, and every
    /// plane here is a primary, so without the cap the list is empty for
    /// everyone, as on Linux.
    fn drm_mode_get_plane_resources(c: &Client) -> Vec<u32> {
        let mut cap: [u64; 2] = [DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1];
        c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
            .expect("SET_CLIENT_CAP UNIVERSAL_PLANES");
        let mut probe = DrmModeGetPlaneRes {
            plane_id_ptr: 0,
            count_planes: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut probe)
            .expect("GETPLANERESOURCES");
        let mut ids = alloc::vec![0u32; probe.count_planes as usize];
        let mut fill = DrmModeGetPlaneRes {
            plane_id_ptr: ids.as_mut_ptr() as u64,
            count_planes: probe.count_planes,
        };
        c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut fill)
            .expect("GETPLANERESOURCES fill");
        ids
    }

    /// Two cards, as on the machine the bug was photographed on.
    #[test]
    fn vulkaninfo_probes_two_hardware_kms_gpus_without_an_error() {
        let screen = kms_emu::attach(640, 480);
        let _g0 = screen.attach_gpu(EmuGpu::hardware_kms("gpu0").with_ids(2001, 1001, 3001));
        let _g1 = screen.attach_gpu(EmuGpu::hardware_kms("gpu1").with_ids(2101, 1101, 3101));
        let c = Client::open(0);
        assert!(
            wsi_get_connectors(&c).is_ok(),
            "{:?}",
            wsi_get_connectors(&c)
        );
    }

    /// Two cards of the SAME model, which is what is actually in the machine:
    /// the driver returns the same synthetic ids from both, and
    /// GETPLANERESOURCES used to list the id twice. `drmModeGetPlane` on
    /// either entry answers with the one plane, so the second entry
    /// contradicts the first. `get_resources` has de-duplicated CRTCs and
    /// connectors for this reason all along; the plane list had not.
    #[test]
    fn two_cards_of_the_same_model_do_not_list_the_same_plane_twice() {
        let screen = kms_emu::attach(640, 480);
        let _g0 = screen.attach_gpu(EmuGpu::hardware_kms("gpu0").with_ids(2001, 1001, 3001));
        let _g1 = screen.attach_gpu(EmuGpu::hardware_kms("gpu1").with_ids(2001, 1001, 3001));
        let c = Client::open(0);
        assert_eq!(drm_mode_get_plane_resources(&c), alloc::vec![3001]);
        let (crtcs, conns, _) = drm_mode_get_resources(&c).expect("GETRESOURCES");
        assert_eq!(crtcs, alloc::vec![2001]);
        assert_eq!(conns, alloc::vec![1001]);
    }
}

/// The refusal channel of the `VK_KHR_display` probe.
///
/// Mesa turns any failing KMS query into `VK_ERROR_OUT_OF_HOST_MEMORY`, so the
/// one thing a boot has to produce is *which* query refused *which* object.
/// These cover the two ways that line used to be lost: it shared its budget
/// with the successes, and it was keyed on nothing, so a retry loop repeated
/// it instead of other refusals being reported.
#[cfg(test)]
mod wsi_refusal_trace_tests {
    use super::*;

    /// The id a refusal names comes from a different field in each struct, so
    /// a common-prefix read would print a pointer. The two enumerating
    /// queries name no object at all and must say `0`, not whatever word
    /// happens to sit at their front (`fb_id_ptr`, `plane_id_ptr`).
    #[test]
    fn each_query_reports_the_object_it_actually_asked_about() {
        let conn = DrmModeGetConnector {
            encoders_ptr: 0xdead_0001,
            modes_ptr: 0xdead_0002,
            props_ptr: 0xdead_0003,
            prop_values_ptr: 0xdead_0004,
            count_modes: 1,
            count_props: 5,
            count_encoders: 1,
            encoder_id: 77,
            connector_id: 1001,
            connector_type: 11,
            connector_type_id: 1,
            connection: 1,
            mm_width: 0,
            mm_height: 0,
            subpixel: 0,
            pad: 0,
        };
        assert_eq!(
            wsi_query_object_id(DRM_IOCTL_MODE_GETCONNECTOR, &conn as *const _ as usize),
            1001,
            "GETCONNECTOR must report connector_id, not the encoder before it"
        );

        let crtc = DrmModeGetCrtc {
            set_connectors_ptr: 0xdead_0005,
            count_connectors: 0,
            crtc_id: 2001,
            fb_id: 0,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: [0; 68],
        };
        assert_eq!(
            wsi_query_object_id(DRM_IOCTL_MODE_GETCRTC, &crtc as *const _ as usize),
            2001
        );

        let plane = DrmModeGetPlane {
            plane_id: 3001,
            crtc_id: 0,
            fb_id: 0,
            possible_crtcs: 1,
            gamma_size: 0,
            count_format_types: 0,
            format_type_ptr: 0,
        };
        assert_eq!(
            wsi_query_object_id(DRM_IOCTL_MODE_GETPLANE, &plane as *const _ as usize),
            3001
        );

        let enc = DrmModeGetEncoder {
            encoder_id: 4001,
            encoder_type: 6,
            crtc_id: 0,
            possible_crtcs: 1,
            possible_clones: 0,
        };
        assert_eq!(
            wsi_query_object_id(DRM_IOCTL_MODE_GETENCODER, &enc as *const _ as usize),
            4001
        );

        // The enumerating pair: no object, and their first word is a pointer.
        let res = DrmModeGetPlaneRes {
            plane_id_ptr: 0x7fff_dead_beef,
            count_planes: 2,
        };
        assert_eq!(
            wsi_query_object_id(DRM_IOCTL_MODE_GETPLANERESOURCES, &res as *const _ as usize),
            0,
            "GETPLANERESOURCES names no object; it must not print its pointer"
        );
        // And an ioctl that is not a KMS query is not on this channel at all.
        assert!(wsi_query_name(DRM_IOCTL_SYNCOBJ_CREATE).is_none());
        assert_eq!(
            wsi_query_name(DRM_IOCTL_MODE_GETRESOURCES),
            Some("GETRESOURCES")
        );
    }

    /// The claim-a-slot rule, over a table of this test's own: the kernel's
    /// is global and every other test in this binary fills it through the
    /// dispatch wrapper, so capacity there is nobody's to assert.
    ///
    /// What has to hold: a repeated refusal prints once (libdrm's
    /// `drmModeGetConnector` retries, and a client that polls the KMS queries
    /// must not storm the console), a *different* refusal still gets through
    /// after it (the bug that hid the failing connector behind everything
    /// logged before it), and the table stops rather than grows when a client
    /// invents ids.
    #[test]
    fn a_repeated_refusal_prints_once_and_never_crowds_out_a_new_one() {
        use core::sync::atomic::{AtomicBool, AtomicU64};
        const GETCONNECTOR_NR: u32 = 0xA7;
        const GETRESOURCES_NR: u32 = 0xA0;
        const SLOTS: usize = 4;
        let seen: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
        let full = AtomicBool::new(false);
        let take = |nr, id| wsi_fail_take_in(&seen, &full, nr, id);

        assert!(take(GETCONNECTOR_NR, 1001), "first sight prints");
        assert!(
            !take(GETCONNECTOR_NR, 1001),
            "the same refusal, repeated, is silent"
        );
        assert!(
            take(GETCONNECTOR_NR, 1002),
            "a different connector is a different refusal"
        );
        assert!(
            take(GETRESOURCES_NR, 0),
            "object 0 on another query is its own entry, not an empty slot"
        );
        assert!(
            !take(GETRESOURCES_NR, 0),
            "...and it is remembered like any other"
        );

        // Three slots spent on three distinct refusals; one left.
        assert!(take(GETCONNECTOR_NR, 1003));
        assert!(
            !take(GETCONNECTOR_NR, 1004),
            "past the last slot the channel goes quiet"
        );
        // A refusal already in the table is still recognised as a repeat
        // rather than re-reported now that the table is full.
        assert!(!take(GETCONNECTOR_NR, 1001));

        // The one pair that collides with the empty-slot sentinel: without
        // the +1 the key for `(0, 0)` IS zero, so claiming the slot leaves it
        // reading as empty and the same refusal prints for ever.
        let fresh: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
        let fresh_full = AtomicBool::new(false);
        assert!(wsi_fail_take_in(&fresh, &fresh_full, 0, 0));
        assert!(
            !wsi_fail_take_in(&fresh, &fresh_full, 0, 0),
            "nr 0 / id 0 must be remembered like any other key"
        );
    }
}

/// What `ADDFB2`'s modifier word is allowed to mean.
///
/// Every value here is built with [`nvidia_block_linear_2d`], the macro's own
/// arithmetic from `drm_fourcc.h`, so a field that moves shows up as the
/// builder and the decoder disagreeing rather than as both being wrong the
/// same way.
#[cfg(test)]
mod scanout_modifier_tests {
    use super::*;

    /// The fields a Turing framebuffer carries: no compression, the desktop
    /// sector layout, the Turing+ page-kind generation, and the generic
    /// uncompressed colour kind.
    const C_NONE: u64 = 0;
    const S_DESKTOP: u64 = 1;
    const G_TURING: u64 = 2;
    const K_GENERIC_TURING: u64 = 0x06;

    fn block_linear(h: u64) -> u64 {
        nvidia_block_linear_2d(C_NONE, S_DESKTOP, G_TURING, K_GENERIC_TURING, h)
    }

    /// The one modifier the present path can actually read, and the zero a
    /// client leaves behind when it sends no modifier at all.
    #[test]
    fn linear_is_the_zero_modifier() {
        assert_eq!(decode_scanout_modifier(0), Some(drm::ScanoutLayout::Linear));
    }

    /// Every block height the hardware defines, and nothing above it:
    /// `SET_SRC_BLOCK_SIZE_HEIGHT` stops at `_THIRTYTWO_GOBS` (h = 5), so a
    /// larger one would be programmed as a masked-off value and the copy
    /// engine would walk the surface with the wrong stride.
    #[test]
    fn the_six_block_heights_decode_and_the_seventh_does_not() {
        for h in 0..=5u64 {
            assert_eq!(
                decode_scanout_modifier(block_linear(h)),
                Some(drm::ScanoutLayout::BlockLinear {
                    log2_gobs_per_block_y: h as u8,
                    page_kind: K_GENERIC_TURING as u8,
                }),
                "h={} is a defined block height",
                h
            );
        }
        for h in 6..=15u64 {
            assert_eq!(
                decode_scanout_modifier(block_linear(h)),
                None,
                "h={} is past _THIRTYTWO_GOBS",
                h
            );
        }
    }

    /// The older `DRM_FORMAT_MOD_NVIDIA_16BX2_BLOCK(v)` spelling is
    /// `(0, 0, 0, 0, v)`: GOB generation 0 and sector layout 0, which is the
    /// Fermi..Volta / Tegra arrangement, NOT Turing's. It names a different
    /// bit layout in memory, so presenting it as if it were ours would paint
    /// garbage, and it is refused on that ground rather than on its page
    /// kind.
    ///
    /// What the canonicalization is for is the other case: a client that
    /// sends Turing's generation but leaves `k` at 0. Kind 0 means
    /// "pitch/linear", which a block-linear surface cannot be, so
    /// `drm_fourcc_canonicalize_nvidia_format_mod` remaps it to 0xfe -- a
    /// Fermi..Volta kind, which this decoder then refuses rather than
    /// quietly presenting under a Turing modifier.
    #[test]
    fn the_old_16bx2_spelling_and_a_zero_page_kind_are_both_refused() {
        assert_eq!(
            decode_scanout_modifier(nvidia_block_linear_2d(0, 0, 0, 0, 2)),
            None,
            "16BX2_BLOCK names the Fermi..Volta layout, not Turing's"
        );
        assert_eq!(
            decode_scanout_modifier(nvidia_block_linear_2d(C_NONE, S_DESKTOP, G_TURING, 0, 2)),
            None,
            "kind 0 canonicalizes to 0xfe, which is not Turing's generic kind"
        );
    }

    /// The three fields that describe a layout we would read wrong, each
    /// refused on its own so a later edit cannot drop one silently.
    #[test]
    fn compression_the_wrong_gob_generation_and_the_wrong_sector_layout_are_refused() {
        // c != 0: lossless compression, whose comptags nothing in this tree
        // allocates. Scanning the bytes out uncompressed is garbage, which
        // is why nv_drm_framebuffer_init refuses it too.
        for c in 1..=7u64 {
            assert_eq!(
                decode_scanout_modifier(nvidia_block_linear_2d(
                    c,
                    S_DESKTOP,
                    G_TURING,
                    K_GENERIC_TURING,
                    0
                )),
                None,
                "compression type {} must not be presented",
                c
            );
        }
        // g = 1 is "Gob Height 4, G80 - GT2XX": a different GOB shape
        // entirely. g = 0 is Fermi..Volta, whose page-kind mapping differs
        // from the one VM_BIND programs.
        for g in [0u64, 1, 3] {
            assert_eq!(
                decode_scanout_modifier(nvidia_block_linear_2d(
                    C_NONE,
                    S_DESKTOP,
                    g,
                    K_GENERIC_TURING,
                    0
                )),
                None,
                "GOB generation {} is not Turing's",
                g
            );
        }
        // s = 0 is the Tegra sector layout; the bits below the page kind are
        // arranged differently and the surface cannot be shared.
        assert_eq!(
            decode_scanout_modifier(nvidia_block_linear_2d(
                C_NONE,
                0,
                G_TURING,
                K_GENERIC_TURING,
                0
            )),
            None
        );
    }

    /// A page kind VM_BIND does not program verbatim is refused rather than
    /// downgraded: the page tables and the copy engine have to agree on the
    /// same kind, and a depth or compressible surface is not something this
    /// scanout should be putting on a panel at all.
    #[test]
    fn only_turings_generic_uncompressed_colour_kind_is_accepted() {
        for k in 0x01..=0xffu64 {
            let want = k == 0x06;
            assert_eq!(
                decode_scanout_modifier(nvidia_block_linear_2d(C_NONE, S_DESKTOP, G_TURING, k, 0))
                    .is_some(),
                want,
                "page kind {:#04x}",
                k
            );
        }
    }

    /// Anything that is not an NVIDIA block-linear modifier at all.
    #[test]
    fn foreign_reserved_and_invalid_modifiers_are_refused() {
        // DRM_FORMAT_MOD_INVALID.
        assert_eq!(decode_scanout_modifier(0x00ff_ffff_ffff_ffff), None);
        // Another vendor's (Intel's Y-tiling is vendor 1).
        assert_eq!(decode_scanout_modifier((1u64 << 56) | 2), None);
        // NVIDIA vendor, but bit 4 clear: not a 2D block-linear modifier.
        assert_eq!(decode_scanout_modifier(3u64 << 56), None);
        // The reserved fields "must be zero": 8:5, 11:9 and everything from
        // 28 up. A future 3D-surface modifier sets one of these and must not
        // be presented as if it were 2D.
        for bit in [5u64, 8, 9, 11, 28, 40, 55] {
            let m = block_linear(0) | (1u64 << bit);
            assert_eq!(
                decode_scanout_modifier(m),
                None,
                "reserved bit {} must refuse the modifier",
                bit
            );
        }
    }

    /// The size arithmetic a block-linear framebuffer needs, which differs
    /// from `pitch * height` in both terms: the pitch counts 64-byte blocks,
    /// and the height is padded up to a whole block.
    #[test]
    fn a_block_linear_surface_is_measured_in_blocks_and_padded_to_one() {
        // 1920 pixels of 4 bytes is 7680 bytes, which is 120 blocks.
        // h = 4 means blocks are 8 << 4 = 128 rows tall, so 1080 rows pad up
        // to 1152.
        assert_eq!(
            drm::block_linear_size(120, 1080, 4),
            Some(120 * 64 * 1152),
            "the last block row is addressed whole; a size from 1080 would \
             let a present read past the buffer"
        );
        // Exactly one block tall: no padding to add.
        assert_eq!(drm::block_linear_size(1, 8, 0), Some(64 * 8));
        // One row past it: a second whole block.
        assert_eq!(drm::block_linear_size(1, 9, 0), Some(2 * 64 * 8));
        // The arithmetic must not wrap: a pitch and height a client is free
        // to send have to come back as None, not as a small size that would
        // pass the buffer check.
        assert_eq!(drm::block_linear_size(u32::MAX, u32::MAX, 5), None);
    }
}
