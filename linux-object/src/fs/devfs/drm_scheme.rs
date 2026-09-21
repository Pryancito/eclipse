//! DRM (Direct Rendering Manager) Scheme for zCore
//!
//! Exposes the DRM subsystem to userspace via IOCTLs and memory mapping.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::task::{Context, Poll as TaskPoll};
use core::time::Duration;

use crate::sync::{Event, EventBus};
use lock::Mutex;
use rcore_fs::vfs::*;
use zircon_object::vm::VmObject;

use super::drm;

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
                true
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
        Arc::new(DrmDev {
            inode_id: self.inode_id,
            minor: self.minor,
            file: drm::DrmFileState::new(),
        })
    }

    pub fn file_state(&self) -> &Arc<drm::DrmFileState> {
        &self.file
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
    pub async fn wait_vblank_sleep(&self, data: usize) {
        // Linux caps this wait at 3 seconds (`DRM_WAIT_ON(..., 3 * HZ, ...)`).
        // Match it: a target far in the future must not park a thread forever,
        // and the sync arm reporting the current sequence after the cap is the
        // same answer Linux's timeout path gives.
        const MAX_WAIT: Duration = Duration::from_secs(3);

        if ucheck(data, core::mem::size_of::<DrmWaitVblank>()).is_err() {
            return; // io_control will reject it with EFAULT in a moment
        }
        let req = unsafe { *(data as *const DrmWaitVblank) };
        if req.typ & _DRM_VBLANK_EVENT != 0 {
            return; // event form: delivered by the timer queue, never blocks
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
                return; // already reached: nothing to wait for
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
        while (target.wrapping_sub(drm::vblank_seq_now()) as i32) > 0 {
            let Some(deadline) = drm::vblank_deadline_for_seq(target) else {
                break;
            };
            if deadline >= cap {
                kernel_hal::thread::sleep_until(cap).await;
                break;
            }
            kernel_hal::thread::sleep_until(deadline).await;
        }
    }

    /// Sleep until a blocking `SYNCOBJ_WAIT` / `TIMELINE_WAIT` would succeed
    /// (or its absolute deadline passes).
    ///
    /// Same split as [`wait_vblank_sleep`]: `io_control` is sync and used to
    /// spin-poll the whole timeout, pegging a core. Here we are in the async
    /// syscall path, so we poll pending fences, probe with
    /// [`zcore_drivers::scheme::syncobj::wait_ready`], and sleep ~1 ms (or
    /// until the deadline) between probes. The sync arm then finishes the
    /// ioctl (usually on the first iteration).
    pub async fn syncobj_wait_sleep(&self, cmd: u32, data: usize) {
        if !zcore_drivers::display::nouveau_uapi_enabled() {
            return;
        }
        let timeline = is_syncobj_timeline_wait(cmd);
        // Deadline-sized ioctls carry a trailing hint we never read; the
        // prefix matches the classic structs.
        let prefix = if timeline {
            core::mem::size_of::<DrmSyncobjTimelineWait>()
        } else {
            core::mem::size_of::<DrmSyncobjWait>()
        };
        if ucheck(data, prefix).is_err() {
            return;
        }
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
        const MAX_HANDLES: u32 = 64;
        if count_handles == 0 || count_handles > MAX_HANDLES || handles_ptr == 0 {
            return;
        }
        if ucheck_n::<u32>(handles_ptr as usize, count_handles as usize).is_err() {
            return;
        }
        if timeline
            && points_ptr != 0
            && ucheck_n::<u64>(points_ptr as usize, count_handles as usize).is_err()
        {
            return;
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
        let deadline_us = (timeout_nsec.max(0) as u64) / 1000;
        let wait_all = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0;
        let available_only = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0;
        let ready_fn = if available_only {
            zcore_drivers::scheme::syncobj::wait_available_ready
        } else {
            zcore_drivers::scheme::syncobj::wait_ready
        };
        loop {
            let _ = zcore_drivers::scheme::syncobj::poll_pending();
            match ready_fn(&handles, points.as_deref(), wait_all, deadline_us) {
                Some(_) => return,
                None => {
                    let now = kernel_hal::timer::timer_now();
                    // Absolute CLOCK_MONOTONIC deadline in Duration form.
                    let abs = core::time::Duration::from_micros(deadline_us);
                    let tick = core::time::Duration::from_millis(1);
                    let next = now + tick;
                    let wake = if abs < next { abs } else { next };
                    // If the absolute deadline is already behind `timer_now`,
                    // wait_ready should have returned Timeout; still avoid an
                    // unbounded sleep if the clocks disagree slightly.
                    if wake <= now {
                        return;
                    }
                    kernel_hal::thread::sleep_until(wake).await;
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
    pub async fn atomic_in_fence_sleep(&self, data: usize) {
        let Some((handle, point)) = self.atomic_in_fence(data) else {
            return;
        };
        // Bounded on purpose. Linux waits on an in-fence indefinitely, but a
        // fence that never signals must not freeze the desktop: presenting a
        // frame early is a visible glitch, presenting nothing ever is a hang.
        // 100 ms is several frames at any refresh rate we drive, so a fence
        // that misses it is broken rather than slow -- and the cost of being
        // wrong is a stutter, not a freeze.
        const IN_FENCE_TIMEOUT_US: u64 = 100_000;
        let now = kernel_hal::timer::timer_now();
        let deadline_us = now.as_micros() as u64 + IN_FENCE_TIMEOUT_US;
        let handles = [handle];
        let points = [point];
        loop {
            let _ = zcore_drivers::scheme::syncobj::poll_pending();
            match zcore_drivers::scheme::syncobj::wait_ready(
                &handles,
                Some(&points),
                true,
                deadline_us,
            ) {
                Some(Ok(_)) => return,
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
                                IN_FENCE_TIMEOUT_US,
                                if n + 1 == MAX_TIMEOUT_REPORTS {
                                    " -- further in-fence timeouts will not be reported"
                                } else {
                                    ""
                                }
                            );
                        }
                    }
                    return;
                }
                None => {
                    let now = kernel_hal::timer::timer_now();
                    let abs = core::time::Duration::from_micros(deadline_us);
                    let next = now + core::time::Duration::from_millis(1);
                    let wake = if abs < next { abs } else { next };
                    if wake <= now {
                        return;
                    }
                    kernel_hal::thread::sleep_until(wake).await;
                }
            }
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
        let _ = len;
        if let Some(vmo) = drm::handle_vmo(handle_id) {
            // The dumb buffer's OWN (contiguous, cached) VMO: the mapping keeps
            // the frames alive past DESTROY_DUMB, and the pixels are WB for the
            // renderer -- see `drm::handle_vmo`.
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
            let _ = len;
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
        // render node, not a second KMS device.
        if self.minor >= 128 && !render_allowed(cmd) {
            // OBSERVE, DO NOT ENFORCE (yet).
            //
            // `render_allowed` used to extract the NR as `(cmd >> 8) & 0xff`,
            // which is the ioctl TYPE byte -- 'd' (0x64) for every DRM ioctl,
            // and 0x64 sits inside the driver-private `0x40..=0x9F` arm. The
            // filter has therefore ACCEPTED EVERYTHING since it was written,
            // and renderD128 has behaved as a fully-open second KMS node.
            //
            // Fixing the extraction (correct, and it now matches Linux's
            // DRM_RENDER_ALLOW set exactly) would in the same step start
            // refusing every dumb-buffer and modeset ioctl on the render node
            // -- CREATE_DUMB/MAP_DUMB/ADDFB2/PAGE_FLIP/... -- on a software-GL
            // desktop that currently boots and that this change cannot be
            // tested against. Turning a silent no-op into an enforcing gate
            // blind is exactly the kind of regression worth avoiding, so log
            // the would-be refusal and let the call through. Flip this to
            // `return Err(FsError::NoPermission)` once a boot log shows the
            // line never appears on the software path.
            //
            // De-duped per NR: the caller controls the rate, and `klog_info!`
            // has no level filter, no rate limit and goes straight out the
            // UART -- an unthrottled line here would let a client that retries
            // in a loop flood the serial console and stall the boot.
            static REFUSAL_LOGGED: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
            let nr = (cmd & 0xff) as usize;
            if !REFUSAL_LOGGED[nr].swap(true, Ordering::Relaxed) {
                kernel_hal::klog_info!(
                    "[drm] render node: ioctl {:#010x} (drm nr={:#04x}) is NOT in Linux's \
                     DRM_RENDER_ALLOW set -- allowed anyway for now, see render_allowed()",
                    cmd,
                    cmd & 0xff
                );
            }
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
            let wsi_name = match cmd {
                DRM_IOCTL_MODE_GETRESOURCES => Some("GETRESOURCES"),
                DRM_IOCTL_MODE_GETCONNECTOR => Some("GETCONNECTOR"),
                DRM_IOCTL_MODE_GETENCODER => Some("GETENCODER"),
                DRM_IOCTL_MODE_GETCRTC => Some("GETCRTC"),
                DRM_IOCTL_MODE_GETPLANERESOURCES => Some("GETPLANERESOURCES"),
                DRM_IOCTL_MODE_GETPLANE => Some("GETPLANE"),
                _ => None,
            };
            if let Some(name) = wsi_name {
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
                let compute_node = drm::is_compute_minor(self.minor);
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
                let nouveau = zcore_drivers::display::nouveau_uapi_enabled();
                let v = unsafe { &mut *(data as *mut DrmVersion) };
                if compute_node {
                    v.version_major = 0;
                    v.version_minor = 1;
                    v.version_patchlevel = 0;
                } else if nouveau {
                    v.version_major = 1;
                    v.version_minor = 4;
                    v.version_patchlevel = 0;
                } else {
                    v.version_major = 1;
                    v.version_minor = 0;
                    v.version_patchlevel = 0;
                }

                let name: &[u8] = if compute_node {
                    b"eclipse-compute\0"
                } else if nouveau {
                    b"nouveau\0"
                } else {
                    b"zcore\0"
                };
                let date = b"20260503\0";
                let desc: &[u8] = if compute_node {
                    b"Eclipse NVIDIA compute\0"
                } else if nouveau {
                    b"nouveau\0"
                } else {
                    b"zCore DRM Driver\0"
                };

                unsafe {
                    if v.name_len > 0 && !v.name.is_null() {
                        let len = core::cmp::min(v.name_len, name.len());
                        ucheck(v.name as usize, len)?;
                        core::ptr::copy_nonoverlapping(name.as_ptr(), v.name, len);
                    }
                    if v.date_len > 0 && !v.date.is_null() {
                        let len = core::cmp::min(v.date_len, date.len());
                        ucheck(v.date as usize, len)?;
                        core::ptr::copy_nonoverlapping(date.as_ptr(), v.date, len);
                    }
                    if v.desc_len > 0 && !v.desc.is_null() {
                        let len = core::cmp::min(v.desc_len, desc.len());
                        ucheck(v.desc as usize, len)?;
                        core::ptr::copy_nonoverlapping(desc.as_ptr(), v.desc, len);
                    }
                }
                v.name_len = name.len();
                v.date_len = date.len();
                v.desc_len = desc.len();
                Ok(0)
            }
            DRM_IOCTL_GET_UNIQUE => {
                let u = unsafe { &mut *(data as *mut DrmUnique) };
                let name = b"zcore-gpu\0";
                unsafe {
                    if u.unique_len > 0 && !u.unique.is_null() {
                        let len = core::cmp::min(u.unique_len, name.len());
                        ucheck(u.unique as usize, len)?;
                        core::ptr::copy_nonoverlapping(name.as_ptr(), u.unique, len);
                    }
                }
                u.unique_len = name.len();
                Ok(0)
            }
            DRM_IOCTL_GET_CAP => {
                let cap = unsafe { &mut *(data as *mut DrmGetCap) };
                match cap.capability {
                    0x1 => cap.value = 1, // DRM_CAP_DUMB_BUFFER
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
                    // DRM_CAP_ADDFB2_MODIFIERS: report NO modifier support.
                    // Our scanout is a CPU blit that reads the framebuffer
                    // LINEARLY, so the only layout it can present is
                    // DRM_FORMAT_MOD_LINEAR. Advertising modifier support (1)
                    // let wlroots negotiate a BLOCK-LINEAR swapchain with NVK
                    // (PTE kind 0x06), and binding it hit VM_BIND's refusal of
                    // non-zero PTE kinds ("PTE kind 0x06 requested (tiled) ...
                    // refusing"), so `vkBindImageMemory` failed and the
                    // swapchain never allocated (`gbm_bo_create failed`,
                    // "Swapchain for output 'HDMI-A-1' failed test"). With 0,
                    // wlroots restricts scanout to implicit/linear buffers,
                    // which bind with PTE kind 0 and blit correctly. (The
                    // Vulkan renderer's VK_EXT_image_drm_format_modifier is a
                    // separate device extension, unaffected by this KMS cap.)
                    0x10 => cap.value = 0, // DRM_CAP_ADDFB2_MODIFIERS
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
                    // Everything else — DRM_CAP_ASYNC_PAGE_FLIP (0x7),
                    // DRM_CAP_PAGE_FLIP_TARGET (0x11) and
                    // DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP (0x15) — is honestly 0.
                    _ => cap.value = 0,
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
            // A single DRM client on the primary node is implicitly master;
            // accept (drop-)master so seatd/wlroots session activation succeeds.
            // Magic/auth: `drmIsMaster()` authenticates magic 0 and treats
            // success as "this fd is DRM master". wlroots' dumb-buffer allocator
            // (pixman path) requires master, so always succeed — the single
            // client on the primary node is implicitly master here.
            DRM_IOCTL_GET_MAGIC => {
                // struct drm_auth { __u32 magic; }
                unsafe { *(data as *mut u32) = 1 };
                Ok(0)
            }
            DRM_IOCTL_AUTH_MAGIC => Ok(0),
            DRM_IOCTL_SET_MASTER => {
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
                // drm_setversion: report the current interface (1.4) and
                // driver (1.0) versions, validating any requested majors.
                // -1 means "query only". Xorg's modesetting driver calls this
                // right after open and treats ENOTTY as "not a DRM device".
                let sv = unsafe { &mut *(data as *mut DrmSetVersion) };
                let req_if = (sv.drm_di_major, sv.drm_di_minor);
                let req_dd = sv.drm_dd_major;
                sv.drm_di_major = 1;
                sv.drm_di_minor = 4;
                sv.drm_dd_major = 1;
                sv.drm_dd_minor = 0;
                if req_if.0 != -1 && (req_if.0 != 1 || req_if.1 < 0 || req_if.1 > 4) {
                    return Err(FsError::InvalidParam);
                }
                if req_dd != -1 && req_dd != 1 {
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
                        // Setting atomic also implies universal planes.
                        self.file.set_atomic_client(value != 0);
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
                    // STEREO_3D, UNIVERSAL_PLANES, ASPECT_RATIO: accept.
                    _ => {
                        log::debug!("[drm] SET_CLIENT_CAP cap={} -> accepted", cap);
                        Ok(0)
                    }
                }
            }
            DRM_IOCTL_MODE_CREATE_DUMB => {
                let info = unsafe { &mut *(data as *mut DrmModeCreateDumb) };
                let bpp = info.bpp.max(32);
                // width/height/bpp are userspace-controlled: compute pitch/size
                // in 64-bit. A 32-bit `width*bpp` or `pitch*height` would wrap
                // (e.g. 50000x50000x32) and under-allocate the buffer while
                // echoing a huge size back, becoming an OOB read at scanout.
                // Bound the result to a sane ceiling (64 MiB — a 4K XRGB frame
                // is ~33 MiB) and require pitch to fit the u32 written back.
                const MAX_DUMB_SIZE: u64 = 64 * 1024 * 1024;
                let mut pitch64 = (info.width as u64 * bpp as u64 / 8 + 63) & !63;
                let mut size64 = pitch64.saturating_mul(info.height as u64);
                // When the compositor requests a full-screen dumb buffer, align
                // its pitch with the display scanout pitch so the CE-offload
                // present path can fire (flat copy needs equal strides). Without
                // this, wlroots' 64-byte-aligned pitch often differs from the
                // GOP framebuffer pitch and every frame falls back to the slow
                // CPU blit (~7-10 FPS on dual RTX).
                if let Some((dw, dh, dp)) = drm::display_mode() {
                    if info.width == dw && info.height == dh && dp as u64 >= pitch64 {
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
                    Err(FsError::NoDeviceSpace)
                }
            }
            DRM_IOCTL_MODE_ADDFB => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd) };
                if let Some(fb_id) = drm::create_fb(cmd.handle, cmd.width, cmd.height, cmd.pitch) {
                    cmd.fb_id = fb_id;
                    Ok(0)
                } else {
                    // [swapchain-diag] error!-visible at LOG=error: a failed FB
                    // creation makes wlroots' swapchain test fail before any
                    // atomic commit is even attempted.
                    log::error!(
                        "[drm] ADDFB failed: {}x{} handle={:#x} pitch={} (create_fb returned None)",
                        cmd.width,
                        cmd.height,
                        cmd.handle,
                        cmd.pitch
                    );
                    Err(FsError::DeviceError)
                }
            }
            DRM_IOCTL_MODE_ADDFB2 => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd2) };
                if let Some(fb_id) =
                    drm::create_fb(cmd.handles[0], cmd.width, cmd.height, cmd.pitches[0])
                {
                    cmd.fb_id = fb_id;
                    Ok(0)
                } else {
                    // [swapchain-diag] error!-visible at LOG=error: the scanout
                    // buffer wlroots hands us is rejected HERE, before the atomic
                    // TEST_ONLY commit — so an empty "ATOMIC reject" grep with
                    // this line present localises the failure to FB creation.
                    log::error!(
                        "[drm] ADDFB2 failed: {}x{} handle={:#x} pitch={} fmt={:#x} modifier={:#x} \
                         (create_fb returned None)",
                        cmd.width, cmd.height, cmd.handles[0], cmd.pitches[0], cmd.pixel_format, cmd.modifier[0]
                    );
                    Err(FsError::DeviceError)
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
                if drm::rmfb_for(fb_id, drm::current_pid()) {
                    Ok(0)
                } else {
                    Err(FsError::EntryNotFound)
                }
            }
            DRM_IOCTL_MODE_CLOSEFB => {
                // Our software-KMS `rmfb` already only drops the fb object —
                // scanout keeps showing the last blitted frame until the next
                // present — which is exactly CLOSEFB's "close without
                // disabling" contract. Reject unknown ids like Linux (EINVAL
                // via DeviceError is close enough for wlroots' fallback).
                let fb_id = unsafe { *(data as *const u32) };
                if drm::rmfb_for(fb_id, drm::current_pid()) {
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_MAP_DUMB => {
                let map = unsafe { &mut *(data as *mut DrmModeMapDumb) };
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
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_SETCRTC => {
                // struct drm_mode_crtc has the same layout as DrmModeGetCrtc.
                let req = unsafe { &mut *(data as *mut DrmModeGetCrtc) };
                if req.mode_valid != 0 {
                    drm::set_vblank_period_from_modeinfo(&req.mode);
                }
                if req.fb_id != 0 {
                    if let Err(e) = drm::present_now_checked(req.fb_id, req.crtc_id, None) {
                        present_failed("SETCRTC", req.fb_id, req.crtc_id, e)?;
                    }
                } else {
                    // `drm_mode_setcrtc` with a null fb turns the pipe off
                    // (`set_config` with `.fb = NULL`). Doing nothing here is
                    // half of why a screen could never be blanked: wlroots
                    // disables an output with DPMS off followed by exactly this
                    // call, and both were no-ops, so the panel kept the last
                    // frame lit while the compositor believed it was dark.
                    drm::set_crtc_blanked(true);
                    drm::set_crtc_fb(req.crtc_id, 0);
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
                if drm::get_fb(flip.fb_id).is_none() {
                    return Err(FsError::EntryNotFound);
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
                if req.fb_id != 0 {
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
                    cmd.depth = 24;
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
                    cmd.pixel_format = 0x3432_5258; // DRM_FORMAT_XRGB8888 ("XR24")
                    cmd.flags = 0;
                    // Same gate as GETFB above; `drm_mode_getfb2_ioctl`
                    // carries the identical check.
                    cmd.handles = if drm::fb_owned_by_caller(&fb) {
                        [fb.gem_handle_id, 0, 0, 0]
                    } else {
                        [0; 4]
                    };
                    cmd.pitches = [fb.pitch, 0, 0, 0];
                    cmd.offsets = [0; 4];
                    cmd.modifier = [0; 4];
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
                // to update the screen. When clip rects are given, blit only
                // their bounding union — a full-frame copy of a swapchain
                // buffer that only has those boxes painted left stale tiles
                // (squares) on the GOP. Scanout expands the union to 64-byte
                // WC lines so a partial store cannot smear neighbouring pixels.
                // An oversized, zero, or unreadable clip list means "the whole
                // frame is dirty" (true DIRTYFB semantics for num_clips == 0).
                let cmd = unsafe { *(data as *const DrmModeFbDirtyCmd) };
                let rect = if cmd.num_clips > 0 && cmd.num_clips <= 64 && cmd.clips_ptr != 0 {
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
                if !drm::present_now_region(cmd.fb_id, 1, rect) {
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
                // No programmable gamma on the software scanout: accept and
                // ignore. (Get leaves the caller's ramp buffers untouched, which
                // Xorg treats as the identity it will "restore" on exit — a
                // no-op against our no-op Set.)
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
                // Legacy property writes (connector DPMS, plane rotation, …).
                // The software scanout has no programmable object state, so
                // accept and ignore rather than failing the client's modeset.
                let req = unsafe { *(data as *const DrmModeObjSetProperty) };
                // Same DPMS handling as the connector-specific setter above:
                // `drm_mode_obj_set_property_ioctl` funnels into the very same
                // `drm_mode_connector_set_obj_prop`.
                if req.prop_id == PROP_DPMS && drm::get_connector(req.obj_id).is_some() {
                    drm::set_crtc_blanked(req.value != DRM_MODE_DPMS_ON);
                    return Ok(0);
                }
                log::debug!(
                    "[drm] OBJ_SETPROPERTY obj={} type={:#x} prop={} val={} (accepted, no-op)",
                    req.obj_id,
                    req.obj_type,
                    req.prop_id,
                    req.value
                );
                Ok(0)
            }
            DRM_IOCTL_MODE_SETPROPERTY => {
                // Legacy connector property write — wlroots sets the DPMS
                // property to "on" as part of committing a modeset. Software
                // scanout is always powered, so accept and ignore rather than
                // failing the commit (which left the screen blank with
                // "Failed to set DPMS property").
                let value = unsafe { *(data as *const u64) };
                let (prop_id, connector_id) = unsafe {
                    (
                        *(data.wrapping_add(8) as *const u32),
                        *(data.wrapping_add(12) as *const u32),
                    )
                };
                // DPMS is the one legacy connector property with an effect
                // here. Linux routes it through `connector->funcs->dpms`,
                // which disables the CRTC for anything but "On"; the other
                // three levels (Standby, Suspend, Off) all mean "stop lighting
                // the panel" on a pipe with no power states of its own.
                if prop_id == PROP_DPMS {
                    let off = value != DRM_MODE_DPMS_ON;
                    log::debug!(
                        "[drm] SETPROPERTY connector={} DPMS={} -> CRTC {}",
                        connector_id,
                        value,
                        if off { "off" } else { "on" }
                    );
                    drm::set_crtc_blanked(off);
                    return Ok(0);
                }
                log::debug!(
                    "[drm] SETPROPERTY connector={} prop={} val={} (accepted, no-op)",
                    connector_id,
                    prop_id,
                    value
                );
                Ok(0)
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
                let cur = unsafe { &*(data as *const DrmModeCursor) };
                let mut changed = false;
                if cur.flags & DRM_MODE_CURSOR_BO != 0 {
                    // Linux: a cursor larger than DRM_CAP_CURSOR_WIDTH/HEIGHT
                    // is EINVAL. Nothing else bounds the bitmap the kernel
                    // copies and composites on every frame.
                    if cur.handle != 0
                        && (cur.width > drm::MAX_CURSOR_DIM || cur.height > drm::MAX_CURSOR_DIM)
                    {
                        return Err(FsError::InvalidParam);
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
                let (fbs, crtcs, connectors) = drm::get_resources();

                if res.fb_id_ptr != 0 && res.count_fbs >= fbs.len() as u32 {
                    ucheck_n::<u32>(res.fb_id_ptr as usize, fbs.len())?;
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            fbs.as_ptr(),
                            res.fb_id_ptr as *mut u32,
                            fbs.len(),
                        );
                    }
                }
                if res.crtc_id_ptr != 0 && res.count_crtcs >= crtcs.len() as u32 {
                    ucheck_n::<u32>(res.crtc_id_ptr as usize, crtcs.len())?;
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            crtcs.as_ptr(),
                            res.crtc_id_ptr as *mut u32,
                            crtcs.len(),
                        );
                    }
                }
                if res.connector_id_ptr != 0 && res.count_connectors >= connectors.len() as u32 {
                    ucheck_n::<u32>(res.connector_id_ptr as usize, connectors.len())?;
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            connectors.as_ptr(),
                            res.connector_id_ptr as *mut u32,
                            connectors.len(),
                        );
                    }
                }

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
                    if res.encoder_id_ptr != 0 && res.count_encoders >= 1 {
                        ucheck_n::<u32>(res.encoder_id_ptr as usize, 1)?;
                        unsafe {
                            *(res.encoder_id_ptr as *mut u32) = drm::SYNTH_ENCODER_ID;
                        }
                    }
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
                    conn_res.encoder_id = drm::SYNTH_ENCODER_ID;
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
                    // non-desktop, EDID, and CRTC_ID for atomic clients), via
                    // the usual two-call count/fill pattern.
                    let props = connector_props(conn_res.connector_id, self.file.atomic_client());
                    if !props.is_empty()
                        && conn_res.props_ptr != 0
                        && conn_res.prop_values_ptr != 0
                        && conn_res.count_props >= props.len() as u32
                    {
                        ucheck_n::<u32>(conn_res.props_ptr as usize, props.len())?;
                        ucheck_n::<u64>(conn_res.prop_values_ptr as usize, props.len())?;
                        for (i, (pid, val)) in props.iter().enumerate() {
                            unsafe {
                                *(conn_res.props_ptr as *mut u32).add(i) = *pid;
                                *(conn_res.prop_values_ptr as *mut u64).add(i) = *val;
                            }
                        }
                    }
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
                    // klog: this refusal makes Mesa's wsi_display bail the whole
                    // VK_KHR_display query with OUT_OF_HOST_MEMORY -- it must be
                    // visible under LOG=error. Same per-boot budget as the call
                    // trace: a retry loop on a refused id must not storm klog.
                    if wsi_trace_take() {
                        kernel_hal::klog_info!(
                            "[drm-wsi] GETCONNECTOR id={} -> NOT FOUND (EINVAL)",
                            conn_res.connector_id
                        );
                    }
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_GETENCODER => {
                let enc = unsafe { &mut *(data as *mut DrmModeGetEncoder) };
                enc.encoder_id = drm::SYNTH_ENCODER_ID;
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
                enc.crtc_id = if drm::software_kms_active() {
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
                    crtc_res.fb_id = crtc.fb_id;
                    crtc_res.x = crtc.x;
                    crtc_res.y = crtc.y;
                    crtc_res.gamma_size = 0;
                    // Report the current mode: the display's native timings
                    // (the only mode the pipeline has). Linux fills this from
                    // crtc->state; compositors read it back to seed their
                    // initial output state.
                    if let Some((w, h, _)) = drm::display_mode() {
                        crtc_res.mode = make_modeinfo(w, h);
                        crtc_res.mode_valid = 1;
                    } else {
                        crtc_res.mode_valid = 0;
                    }
                    Ok(0)
                } else {
                    if wsi_trace_take() {
                        kernel_hal::klog_info!(
                            "[drm-wsi] GETCRTC id={} -> NOT FOUND (EINVAL)",
                            crtc_res.crtc_id
                        );
                    }
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_GETPLANERESOURCES => {
                let res = unsafe { &mut *(data as *mut DrmModeGetPlaneRes) };
                let planes = drm::get_planes();
                if res.plane_id_ptr != 0 && res.count_planes >= planes.len() as u32 {
                    ucheck_n::<u32>(res.plane_id_ptr as usize, planes.len())?;
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            planes.as_ptr(),
                            res.plane_id_ptr as *mut u32,
                            planes.len(),
                        );
                    }
                }
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
                    const FORMATS: [u32; 2] = [
                        0x3432_5258, // DRM_FORMAT_XRGB8888 ("XR24")
                        0x3432_5241, // DRM_FORMAT_ARGB8888 ("AR24")
                    ];
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
                    if wsi_trace_take() {
                        kernel_hal::klog_info!(
                            "[drm-wsi] GETPLANE id={} -> NOT FOUND (EINVAL)",
                            res.plane_id
                        );
                    }
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES => {
                let res = unsafe { &mut *(data as *mut DrmModeObjGetProperties) };
                // Identify the object by id (libdrm often passes obj_type=ANY).
                // Look up any registered plane — not only SYNTH_PLANE_ID — so
                // hardware planes (e.g. NVIDIA 3001, VirtIO 3000) are also
                // classified as PRIMARY/OVERLAY/CURSOR by wlroots. Atomic
                // properties (FB_ID, CRTC_ID, ACTIVE, MODE_ID, rects) only
                // appear for atomic clients, like Linux's atomic filtering;
                // legacy clients keep seeing exactly the pre-atomic set.
                let atomic = self.file.atomic_client();
                let props: alloc::vec::Vec<(u32, u64)> = if let Some(p) = drm::get_plane(res.obj_id)
                {
                    plane_props(&p, atomic)
                } else if drm::get_crtc(res.obj_id).is_some() {
                    crtc_props(atomic)
                } else if drm::get_connector(res.obj_id).is_some() {
                    connector_props(res.obj_id, atomic)
                } else {
                    // Encoders exist but carry no properties; anything
                    // else is unknown. Keep the historical empty-list
                    // answer (Linux: EINVAL/ENOENT) — some clients probe
                    // every id returned by GETRESOURCES.
                    alloc::vec::Vec::new()
                };
                let n = props.len();
                // Both output arrays are written below, so both pointers must be
                // non-null: a client passing props_ptr set but prop_values_ptr=0
                // would otherwise trigger a kernel write to address 0.
                if n > 0
                    && res.props_ptr != 0
                    && res.prop_values_ptr != 0
                    && (res.count_props as usize) >= n
                {
                    ucheck_n::<u32>(res.props_ptr as usize, n)?;
                    ucheck_n::<u64>(res.prop_values_ptr as usize, n)?;
                    for (i, (pid, val)) in props.iter().enumerate() {
                        unsafe {
                            *(res.props_ptr as *mut u32).add(i) = *pid;
                            *(res.prop_values_ptr as *mut u64).add(i) = *val;
                        }
                    }
                }
                res.count_props = n as u32;
                log::debug!(
                    "[drm] OBJ_GETPROPERTIES obj_id={} obj_type={:#x} -> {} props",
                    res.obj_id,
                    res.obj_type,
                    n
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
                        Err(FsError::InvalidParam)
                    }
                } else {
                    Err(FsError::InvalidParam)
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
                req.blob_id = drm::create_blob(src.to_vec(), true);
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
                match drm::destroy_blob(blob_id) {
                    drm::BlobDestroy::Destroyed => Ok(0),
                    drm::BlobDestroy::NotFound => Err(FsError::EntryNotFound),
                    // Linux: only the creator may destroy a blob -> EPERM-ish.
                    drm::BlobDestroy::KernelOwned => Err(FsError::NoPermission),
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
                    // Empty commit: no objects, no events (Linux allows it).
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
                let handle = zcore_drivers::scheme::syncobj::create(
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
                if zcore_drivers::scheme::syncobj::destroy(req.handle) {
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
                const MAX_HANDLES: u32 = 64;
                if req.count_handles == 0 || req.count_handles > MAX_HANDLES || req.handles == 0 {
                    return Err(FsError::InvalidParam);
                }
                ucheck_n::<u32>(req.handles as usize, req.count_handles as usize)?;
                let apply = if cmd == DRM_IOCTL_SYNCOBJ_RESET {
                    zcore_drivers::scheme::syncobj::reset
                } else {
                    zcore_drivers::scheme::syncobj::signal
                };
                for i in 0..req.count_handles {
                    let handle = unsafe { *(req.handles as *const u32).add(i as usize) };
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
                const MAX_HANDLES: u32 = 64;
                if req.count_handles == 0
                    || req.count_handles > MAX_HANDLES
                    || req.handles == 0
                    || req.points == 0
                {
                    return Err(FsError::InvalidParam);
                }
                ucheck_n::<u32>(req.handles as usize, req.count_handles as usize)?;
                ucheck_n::<u64>(req.points as usize, req.count_handles as usize)?;
                for i in 0..req.count_handles as usize {
                    let handle = unsafe { *(req.handles as *const u32).add(i) };
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
                const MAX_HANDLES: u32 = 64;
                if req.count_handles == 0
                    || req.count_handles > MAX_HANDLES
                    || req.handles == 0
                    || req.points == 0
                {
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
                let mut first_pt = 0u64;
                for i in 0..req.count_handles as usize {
                    let handle = unsafe { *(req.handles as *const u32).add(i) };
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
                // One rule for "is this the timeline wait", shared with the
                // async sleeper that runs before this arm, so the two cannot
                // read the same request as different structs.
                let timeline = is_syncobj_timeline_wait(cmd);
                // Both structs share this prefix layout, so a single path
                // can read the common fields regardless of which ioctl.
                let (handles_ptr, points_ptr, timeout_nsec, count_handles, flags) = if timeline {
                    let req = unsafe { &*(data as *const DrmSyncobjTimelineWait) };
                    (
                        req.handles,
                        req.points,
                        req.timeout_nsec,
                        req.count_handles,
                        req.flags,
                    )
                } else {
                    let req = unsafe { &*(data as *const DrmSyncobjWait) };
                    (
                        req.handles,
                        0,
                        req.timeout_nsec,
                        req.count_handles,
                        req.flags,
                    )
                };
                const MAX_HANDLES: u32 = 64;
                if count_handles == 0 || count_handles > MAX_HANDLES || handles_ptr == 0 {
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
                            2 => FsError::EntryNotFound,  // ENOENT
                            12 => FsError::NoDeviceSpace, // ENOMEM
                            16 => FsError::Busy,          // EBUSY
                            19 => FsError::NoDevice,      // ENODEV
                            22 => FsError::InvalidParam,  // EINVAL
                            38 => FsError::NotSupported,  // ENOSYS
                            95 => FsError::NotSupported,  // EOPNOTSUPP
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
    /// `struct drm_prime_handle` (12 B, frozen).
    pub const PRIME_FD_TO_HANDLE: (u32, usize) = (0x2D, 12);
    /// `struct drm_prime_handle` (12 B, frozen).
    pub const PRIME_HANDLE_TO_FD: (u32, usize) = (0x2E, 12);
    /// `struct drm_mode_create_lease` (24 B).
    pub const MODE_CREATE_LEASE: (u32, usize) = (0xC6, 24);
    /// `struct drm_syncobj_eventfd` (24 B).
    pub const SYNCOBJ_EVENTFD: (u32, usize) = (0xCF, 24);
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
    #[allow(dead_code)]
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
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE: u32 = 1 << 2;
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    flags: u32,
    #[allow(dead_code)]
    pad: u32,
}

// WAIT_VBLANK request type flags (`<drm/drm.h>`).
const _DRM_VBLANK_EVENT: u32 = 0x0400_0000;

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
const DRM_MODE_OBJECT_FB: u32 = 0xfbfb_fbfb;

// DRM client capabilities (DRM_IOCTL_SET_CLIENT_CAP).
const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
const DRM_CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;

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

/// Build a `struct drm_mode_modeinfo` (68 bytes) for a simple 60 Hz mode at
/// `w`x`h`. Timings are nominal — a software framebuffer never programs real CRT
/// timings — but they must be *valid*: `hdisplay < hsync_start < hsync_end <
/// htotal` (and the vertical analogue). The previous +10%/+5% blanking put
/// `hsync_end > htotal` at 1366×768, which is MODE_H_ILLEGAL; compositors that
/// recompute refresh from the porches then advertised ~55–59 Hz instead of 60.
fn make_modeinfo(w: u32, h: u32) -> [u8; 68] {
    let mut m = [0u8; 68];
    let hdisplay = w as u16;
    let vdisplay = h as u16;
    // Fixed porches, always strictly increasing for any GOP-sized mode.
    let hsync_start = hdisplay.saturating_add(48);
    let hsync_end = hsync_start.saturating_add(32);
    let htotal = hsync_end.saturating_add(80);
    let vsync_start = vdisplay.saturating_add(3);
    let vsync_end = vsync_start.saturating_add(6);
    let vtotal = vsync_end.saturating_add(32);
    let clock = clock_khz_for_refresh_mhz(htotal as u32, vtotal as u32, 60_000);
    m[0..4].copy_from_slice(&clock.to_ne_bytes());
    m[4..6].copy_from_slice(&hdisplay.to_ne_bytes());
    m[6..8].copy_from_slice(&hsync_start.to_ne_bytes());
    m[8..10].copy_from_slice(&hsync_end.to_ne_bytes());
    m[10..12].copy_from_slice(&htotal.to_ne_bytes());
    // hskew @12..14 = 0
    m[14..16].copy_from_slice(&vdisplay.to_ne_bytes());
    m[16..18].copy_from_slice(&vsync_start.to_ne_bytes());
    m[18..20].copy_from_slice(&vsync_end.to_ne_bytes());
    m[20..22].copy_from_slice(&vtotal.to_ne_bytes());
    // vscan @22..24 = 0
    m[24..28].copy_from_slice(&60u32.to_ne_bytes()); // vrefresh (Hz)
                                                     // flags @28..32: NHSYNC (1<<1) | PVSYNC (1<<3), typical CVT polarity
    m[28..32].copy_from_slice(&0x0Au32.to_ne_bytes());
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
            values: &[I32_MIN_U64, I32_MAX_U64],
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

/// [`atomic_stage`] once the object kind is known.
fn atomic_stage_on(
    upd: &mut drm::AtomicUpdate,
    obj: AtomicObject,
    prop_id: u32,
    value: u64,
) -> Result<()> {
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
            // IN_FENCE_FD: -1 = none (ignore). A real fd is waited for before
            // the commit presents -- see `DrmDev::atomic_in_fence_sleep`, which
            // runs in the async syscall path ahead of this sync arm. Staging it
            // here is still what makes the commit accept the property.
            PROP_IN_FENCE_FD => {
                let fd = value as i32;
                if fd < -1 {
                    return Err(FsError::InvalidParam);
                }
                if fd >= 0 {
                    upd.in_fence_fd = Some(fd);
                }
            }
            PROP_FB_DAMAGE_CLIPS => upd.damage_clips = Some(value as u32),
            // "type" is immutable.
            PROP_TYPE => return Err(FsError::InvalidParam),
            _ => return Err(FsError::EntryNotFound),
        },
        AtomicObject::Crtc => match prop_id {
            PROP_ACTIVE => {
                if value > 1 {
                    return Err(FsError::InvalidParam);
                }
                upd.active = Some(value != 0);
            }
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
            // cannot set: DPMS is legacy-only, and EDID / link-status /
            // non-desktop are immutable. Linux answers all four EINVAL --
            // `drm_mode_atomic_ioctl` looks the property up first and only
            // then refuses it -- and ENOENT here would tell a compositor that
            // enumerated the property that it has since vanished.
            PROP_DPMS | PROP_EDID | PROP_LINK_STATUS | PROP_NON_DESKTOP => {
                return Err(FsError::InvalidParam)
            }
            _ => return Err(FsError::EntryNotFound),
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
    drm_ioctl_reconciled(cmd, data, |canon, kdata| {
        dev.drm_ioctl_dispatch(canon, kdata)
    })
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
            // EINVAL, like `drm_read()` with nothing read yet. EAGAIN here was
            // a livelock: the queue is non-empty, so READABLE stays set and a
            // blocking reader's wait resolves instantly, over and over.
            drm::EventRead::TooSmall => Err(FsError::InvalidParam),
        }
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Ok(_buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: self.file.has_events(),
            // Keep write=true for now: reporting write=false made labwc's
            // DRM epoll actually park and exposed a #DF at session start
            // (heap corruption while the card fd stopped looking always-
            // ready). Linux semantics are "readable for events"; revisit
            // once the UserContext/#DF path at labwc bring-up is solid.
            write: true,
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
        assert!(is_drm_ioctl_nr(0xC00C_642E, n, min), "the frozen encoding");
        assert!(is_drm_ioctl_nr(0xC018_642E, n, min), "a grown one");
        assert!(!is_drm_ioctl_nr(0xC008_642E, n, min), "a short one");
        assert!(!is_drm_ioctl_nr(0xC00C_652E, n, min), "not a DRM type byte");
        assert!(!is_drm_ioctl_nr(0xC00C_642D, n, min), "a different NR");
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
                dev: DrmDev::new(minor),
            }
        }

        /// `drmIoctl(fd, request, &arg)`.
        pub(super) fn ioctl<T>(&self, request: u32, arg: &mut T) -> Result<usize> {
            drm_ioctl(&self.dev, request, arg as *mut T as usize)
        }

        /// `read(fd, buf, len)` --- how a compositor collects flip completions.
        pub(super) fn read_events(&self, buf: &mut [u8]) -> Result<usize> {
            self.dev.read_at(0, buf)
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
        pub(super) fn addfb2(&self, buf: &DrmModeCreateDumb) -> u32 {
            // DRM_FORMAT_XRGB8888, which is what every GL swapchain on this
            // tree ends up presenting.
            const DRM_FORMAT_XRGB8888: u32 = 0x3443_5258;
            let mut cmd = DrmModeFbCmd2 {
                fb_id: 0,
                width: buf.width,
                height: buf.height,
                pixel_format: DRM_FORMAT_XRGB8888,
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
        let _serialised = drm::test_globals::lock();
        let client = Client::open(0);
        let before = table_sizes();

        let buf = client.create_dumb(64, 64);
        // The pitch is 64-byte aligned, which is what wlroots asks for and what
        // the copy-engine present path needs to match the scanout stride.
        assert_eq!(buf.pitch % 64, 0, "a dumb pitch must be 64-byte aligned");
        assert_eq!(buf.size, buf.pitch as u64 * 64);

        let fb = client.addfb2(&buf);

        // The flip itself. There is no display to blit into, and the arms treat
        // that as a frame that could not be copied rather than a modeset that
        // failed -- so a flip is still accepted and still owes an event.
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
        let _serialised = drm::test_globals::lock();
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
    /// there before -- the text console, usually.
    #[test]
    fn a_framebuffer_smaller_than_the_mode_leaves_the_rest_of_the_screen_alone() {
        let screen = kms_emu::attach(64, 16);
        let c = Client::open(0);
        let buf = c.create_dumb(16, 4);
        paint(&buf, |x, y| tag(0x0033_0000, x, y));
        let fb = c.addfb2(&buf);

        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

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

        // One primary plane on that CRTC.
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

        let (crtcs, conns) = topology(&c);
        assert_eq!(crtcs, alloc::vec![60], "the non-KMS CRTC was exposed too");
        assert_eq!(conns, alloc::vec![61]);
        assert_eq!(planes(&c), alloc::vec![62]);
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

        assert!(matches!(drm::destroy_blob(a), drm::BlobDestroy::Destroyed));
        assert_eq!(drm::get_blob(a), None);
        let c = drm::create_blob(alloc::vec![5u8], true);
        assert!(c > b, "a freed id came back: {} after {}", c, b);

        assert!(matches!(drm::destroy_blob(b), drm::BlobDestroy::Destroyed));
        assert!(matches!(drm::destroy_blob(c), drm::BlobDestroy::Destroyed));
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
            matches!(drm::destroy_blob(kernel), drm::BlobDestroy::KernelOwned),
            "a kernel-owned blob must answer EPERM, not vanish",
        );
        assert!(
            drm::get_blob(kernel).is_some(),
            "and it must still be there afterwards",
        );

        assert!(matches!(
            drm::destroy_blob(drm::BLOB_ID_BASE - 1),
            drm::BlobDestroy::NotFound
        ));
        assert!(matches!(
            drm::destroy_blob(edid_blob_id(2)),
            drm::BlobDestroy::NotFound
        ));
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

    /// A buffer too small for one event is `EINVAL`, and the event STAYS
    /// queued. `EAGAIN` here is a livelock (the queue is non-empty, so the file
    /// is still readable and the wait resolves instantly, over and over), and
    /// dropping the event instead would lose the flip completion wlroots is
    /// waiting on -- a desktop frozen on its current frame.
    #[test]
    fn a_buffer_too_small_for_one_event_is_einval_and_keeps_the_event() {
        let _screen = kms_emu::attach(32, 8);
        let c = Client::open(0);
        let (fb, handle) = queue_one_flip(&c, 0xABCD);

        let mut small = [0u8; 16];
        assert_eq!(c.read_events(&mut small), Err(FsError::InvalidParam));
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

        // A refused short read must not clear it either.
        let mut small = [0u8; 8];
        assert_eq!(c.read_events(&mut small), Err(FsError::InvalidParam));
        assert!(
            c.poll().expect("poll").read,
            "a short read consumed the event"
        );

        let mut full = [0u8; 32];
        assert_eq!(c.read_events(&mut full).expect("drain"), 32);
        assert!(!c.poll().expect("poll").read, "readable after the drain");
        // Writable throughout, on purpose: reporting write=false made labwc's
        // DRM epoll park and exposed a #DF at session start.
        assert!(c.poll().expect("poll").write);

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
    struct Request {
        objs: Vec<u32>,
        counts: Vec<u32>,
        props: Vec<u32>,
        values: Vec<u64>,
    }

    impl Request {
        fn new(objs: &[u32], counts: &[u32], props: &[u32], values: &[u64]) -> Request {
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
    fn atomic_client(width: u32, height: u32) -> (kms_emu::Screen, Client) {
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

    fn commit(c: &Client, req: &Request, flags: u32) -> Result<usize> {
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
        // A plane pointed at a CRTC that does not exist: staged fine, refused by
        // the commit's check phase.
        let req = Request::new(
            &[drm::SYNTH_CRTC_ID, drm::SYNTH_PLANE_ID],
            &[1, 1],
            &[PROP_OUT_FENCE_PTR, PROP_CRTC_ID],
            &[&mut slot as *mut i32 as u64, 0x999],
        );

        assert_eq!(
            commit(&c, &req, 0),
            Err(FsError::EntryNotFound),
            "the bogus CRTC reference was accepted"
        );
        assert_eq!(slot, -1, "the client's fence slot was left uninitialised");
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
}
