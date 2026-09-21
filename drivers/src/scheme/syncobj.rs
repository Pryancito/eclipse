//! Generic DRM sync objects (`drm_syncobj`) -- a timeline counter each GEM
//! client can create, signal, and wait on. Part of core DRM (ioctl numbers
//! `0xBF`-`0xCF`, above `DRM_COMMAND_END`), not driver-specific: any
//! `DrmScheme` implementor can use these, and the ioctls themselves are
//! dispatched generically by `linux-object`'s `drm_scheme.rs`.
//!
//! Lives in `drivers` (not `linux-object`, where the ioctls are actually
//! parsed) so that a driver's own submission path -- e.g. `NvidiaGpu`'s
//! nouveau-uAPI `EXEC` handler in `nvidia.rs` -- can signal a syncobj
//! directly after a real hardware completion, without needing to call
//! back up into a higher crate layer (this crate has no dependency on
//! `linux-object`, and layering only allows calls downward).
//!
//! # Model
//!
//! Every syncobj is a timeline: a `u64` counter starting at 0 (or 1, if
//! created with `DRM_SYNCOBJ_CREATE_SIGNALED`). "Binary" (legacy,
//! non-timeline) signal/wait is just timeline point 1. A point advances
//! either by an explicit call (`signal`/`timeline_signal`, from an ioctl or
//! from a driver that waited for the hardware itself) or -- the fast path --
//! through a **pending hardware fence** ([`attach_hw_fence`]): the driver
//! submits work, appends a host-semaphore RELEASE of `payload` into a sysmem
//! `fence_va`, and records "handle reaches `point` once `*fence_va >=
//! payload`". The submitting ioctl then returns immediately; the fence is
//! resolved LAZILY on every table access (wait/query/export/import/transfer)
//! and by [`poll_pending`], which an upper layer drives from a timer while
//! eventfd waiters are armed. There is no `dma_fence`/interrupt here, but the
//! observable semantics match Linux: a signaled syncobj is never a lie (the
//! GPU really did write the fence), and CPU and GPU no longer serialise on
//! every submission.
//!
//! [`wait`] is a bounded poll of that counter. Prefer the async sleep loop in
//! `sys_ioctl` ([`wait_ready`] + `poll_pending`) before the sync `io_control`
//! arm runs: that path yields the CPU. The spin inside [`wait`] remains as a
//! short fallback when the condition is already met (or when a caller bypasses
//! the async pre-wait). Long ago this module doc claimed spin-poll was the
//! only option because `io_control` is synchronous; that is still true for the
//! inode path itself, but `sys_ioctl` now sleeps first for
//! `SYNCOBJ_WAIT`/`TIMELINE_WAIT` the same way it does for `WAIT_VBLANK`.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use lock::Mutex;

struct Syncobj {
    handle: u32,
    point: u64,
    /// Live references: the creating handle starts at 1; each
    /// `SYNCOBJ_HANDLE_TO_FD` export (and each `dup` of that fd) bumps it.
    /// [`destroy`] only removes the object when this drops to zero, so an
    /// exported fd keeps the syncobj alive past `SYNCOBJ_DESTROY` — matching
    /// real DRM (and fixing the stale-handle window the old no-refcount
    /// table left for Mesa fence export).
    refs: u32,
    /// A pending dependency on another syncobj: `(src, target, dst_point)`.
    /// This object reaches `dst_point` once `src` reaches `target`.
    ///
    /// Set by a `sync_file` import (see [`import_snapshot`], where `dst_point`
    /// is 1, because a binary import really does replace the binary fence) and
    /// by a [`transfer`] whose source has not landed yet. `None` for the normal
    /// case, and cleared whenever the object is signaled or reset directly,
    /// mirroring how a real `drm_syncobj` REPLACES its fence on those
    /// operations rather than accumulating them.
    ///
    /// `dst_point` used to be missing, so a software timeline-to-timeline
    /// transfer landed `dst` at 1 whatever point was asked for. Linux allocates
    /// a `dma_fence_chain` node and `drm_syncobj_add_point(dst, chain, fence,
    /// args->dst_point)`, so `dst` reaches exactly that point. The gap was a
    /// hang, not a rounding error: wlroots' `linux-drm-syncobj-v1` moves a
    /// client's acquire point into its own timeline at a point it chooses, then
    /// waits on it, so a point that never arrives freezes that surface.
    linked: Option<(u32, u64, u64)>,
}

/// "`handle` reaches `point` once the GPU has written a value >= `payload`
/// into the u32 at `fence_va`" -- the lazy form of a hardware completion.
/// `fence_va` is a kernel virtual address of pinned sysmem (the channel's
/// own scratch buffer, see `eclipse_rm_exec_fast_prepare`); `payload` is a
/// per-context monotonic sequence, so landing is a wrapping `>=`, which
/// stays correct when several fences share one landing zone.
/// `fence_gpu_va` is the same semaphore in the *producer* channel's GPU VA
/// space (`buf_gpu_va + fence_sem_off`); 0 means unknown (CPU wait only for
/// cross-ctx ACQUIRE scaffolding).
#[derive(Clone, Copy)]
struct PendingFence {
    handle: u32,
    point: u64,
    fence_va: usize,
    /// Producer-ctx GPU VA of the fence semaphore; 0 = unknown.
    fence_gpu_va: u64,
    payload: u32,
    /// GPU context (channel) the fence was submitted on: lets a submit on
    /// the SAME channel treat the fence as already ordered before it
    /// ([`wait_ordered`]), and names the ring to latch wedged on timeout.
    ctx_idx: u32,
    submitted_us: u64,
}

/// The point `handle` counts as having reached: its own counter, plus the
/// binary signal a pending `sync_file` import contributes once its source
/// reaches the point captured at export time. `depth` bounds the (exotic)
/// case of an import whose source is itself waiting on an import.
///
/// Callers must already hold the table lock (and have resolved pending
/// hardware fences first, see [`resolve_locked`]).
fn effective_point(objects: &[Syncobj], handle: u32, depth: u8) -> Option<u64> {
    let obj = objects.iter().find(|o| o.handle == handle)?;
    let mut point = obj.point;
    if let (Some((src, target, dst_point)), true) = (obj.linked, depth > 0) {
        if let Some(src_point) = effective_point(objects, src, depth - 1) {
            if src_point >= target {
                point = point.max(dst_point.max(1));
            }
        }
    }
    Some(point)
}

/// Link-following depth for [`effective_point`].
const LINK_DEPTH: u8 = 4;

struct SyncobjTable {
    objects: Vec<Syncobj>,
    pending: Vec<PendingFence>,
}

static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);

lazy_static::lazy_static! {
    static ref TABLE: Mutex<SyncobjTable> = Mutex::new(SyncobjTable {
        objects: Vec::new(),
        pending: Vec::new(),
    });
}

/// Number of pending hardware fences, mirrored outside the lock so the
/// eventfd poller (and [`poll_pending`]'s fast exit) can check it for free.
static PENDING_COUNT: AtomicUsize = AtomicUsize::new(0);

/// A pending fence older than this is a hung ring. Same bound the driver's
/// old synchronous poll used, so the behaviour on a GPU hang is unchanged:
/// the context is latched wedged (via the timeout hook) and the waiter is
/// released instead of parking forever.
const FENCE_TIMEOUT_US: u64 = 10_000_000;

/// Optional upcall fired whenever a syncobj point advances, so an upper layer
/// (linux-object) can service `SYNCOBJ_EVENTFD` registrations — deliver an
/// eventfd once its target point is reached. Signals arrive from BOTH the
/// ioctl path and this crate's own `EXEC` completion (`nvidia.rs`), so the
/// notification has to originate here, at the single choke point every point
/// advance passes through, rather than in the caller. A null slot (the
/// default, and the common case with no eventfd registered) costs one relaxed
/// load per signal. Same registration idiom as `kernel_hal`'s `KLOG_EMIT_FN`.
static SIGNAL_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Optional upcall for a pending hardware fence that did not land within
/// [`FENCE_TIMEOUT_US`]: `(ctx_idx, fence_va, payload, handle, point)`. The
/// GPU driver registers it to latch the context wedged and capture its
/// hang probe -- the work its synchronous EXEC used to do inline.
static FENCE_TIMEOUT_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register the point-advance upcall (see [`SIGNAL_HOOK`]). Called once at boot.
pub fn set_signal_hook(f: fn(u32, u64)) {
    SIGNAL_HOOK.store(f as usize, Ordering::SeqCst);
}

/// Register the fence-timeout upcall (see [`FENCE_TIMEOUT_HOOK`]).
pub fn set_fence_timeout_hook(f: fn(u32, usize, u32, u32, u64)) {
    FENCE_TIMEOUT_HOOK.store(f as usize, Ordering::SeqCst);
}

/// Fire the point-advance upcall, if one is registered. Must be called with the
/// [`TABLE`] lock RELEASED: the hook re-enters this module (`query`) to re-check
/// waiters, which re-takes that lock.
#[inline]
fn notify_signal(handle: u32, point: u64) {
    let p = SIGNAL_HOOK.load(Ordering::Relaxed);
    if p != 0 {
        // SAFETY: `p` is only ever stored by `set_signal_hook`, from a value of
        // exactly this `fn(u32, u64)` type.
        let f: fn(u32, u64) = unsafe { core::mem::transmute(p) };
        f(handle, point);
    }
}

#[inline]
fn now_us() -> u64 {
    unsafe { crate::bus::drivers_timer_now_as_micros() }
}

/// [`fence_landed`] for drivers that wait on a kernel fence outside the
/// syncobj table (nouveau `GEM_CPU_PREP`).
pub fn hw_fence_landed(fence_va: usize, payload: u32) -> bool {
    fence_landed(fence_va, payload)
}

/// Whether the GPU has written `payload` (or a later one) into `fence_va`.
#[inline]
fn fence_landed(fence_va: usize, payload: u32) -> bool {
    // SAFETY: `fence_va` is a kernel mapping of pinned sysmem published by
    // the driver for exactly this read (`attach_hw_fence`'s contract).
    let v = unsafe { core::ptr::read_volatile(fence_va as *const u32) };
    (v.wrapping_sub(payload) as i32) >= 0
}

// --- Counters for /proc/gpudbg -----------------------------------------------
static WAIT_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WAIT_SPIN_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WAIT_MAX_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WAIT_TIMEOUTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FENCES_LANDED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FENCES_TIMED_OUT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FENCE_LATENCY_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FENCE_LATENCY_MAX_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// One line of cumulative syncobj statistics (waits, spin time, hardware
/// fence latency) for the GPU driver's `/proc/gpudbg` profile section.
pub fn stats_line() -> alloc::string::String {
    let calls = WAIT_CALLS.load(Ordering::Relaxed);
    let landed = FENCES_LANDED.load(Ordering::Relaxed);
    alloc::format!(
        "syncobj waits={} cpu-spin={}us (avg {}us, max {}us, timeouts={}) | hw fences landed={} (submit->land avg {}us, max {}us) timed-out={} pending-now={}",
        calls,
        WAIT_SPIN_US.load(Ordering::Relaxed),
        WAIT_SPIN_US.load(Ordering::Relaxed) / calls.max(1),
        WAIT_MAX_US.load(Ordering::Relaxed),
        WAIT_TIMEOUTS.load(Ordering::Relaxed),
        landed,
        FENCE_LATENCY_US.load(Ordering::Relaxed) / landed.max(1),
        FENCE_LATENCY_MAX_US.load(Ordering::Relaxed),
        FENCES_TIMED_OUT.load(Ordering::Relaxed),
        PENDING_COUNT.load(Ordering::Relaxed),
    )
}

/// Everything [`resolve_locked`] wants done once the lock is dropped.
#[derive(Default)]
struct Deferred {
    notify: Vec<(u32, u64)>,
    timed_out: Vec<PendingFence>,
}

impl Deferred {
    fn run(self) {
        for (h, p) in self.notify {
            notify_signal(h, p);
        }
        let hook = FENCE_TIMEOUT_HOOK.load(Ordering::Relaxed);
        for f in self.timed_out {
            if hook != 0 {
                // SAFETY: stored by `set_fence_timeout_hook` from this type.
                let cb: fn(u32, usize, u32, u32, u64) = unsafe { core::mem::transmute(hook) };
                cb(f.ctx_idx, f.fence_va, f.payload, f.handle, f.point);
            }
        }
    }
}

/// Advance every syncobj whose pending hardware fence has landed (or timed
/// out -- a hung ring must not park its waiters forever; the timeout hook
/// tells the driver, which latches the context wedged so the client's next
/// submit fails honestly). Returns the upcalls to make after unlocking.
fn resolve_locked(table: &mut SyncobjTable) -> Deferred {
    let mut out = Deferred::default();
    if table.pending.is_empty() {
        return out;
    }
    let now = now_us();
    let mut i = 0;
    while i < table.pending.len() {
        let f = table.pending[i];
        let landed = fence_landed(f.fence_va, f.payload);
        let timed_out = !landed && now.wrapping_sub(f.submitted_us) >= FENCE_TIMEOUT_US;
        if !(landed || timed_out) {
            i += 1;
            continue;
        }
        table.pending.swap_remove(i);
        if landed {
            let lat = now.wrapping_sub(f.submitted_us);
            FENCES_LANDED.fetch_add(1, Ordering::Relaxed);
            FENCE_LATENCY_US.fetch_add(lat, Ordering::Relaxed);
            FENCE_LATENCY_MAX_US.fetch_max(lat, Ordering::Relaxed);
        } else {
            FENCES_TIMED_OUT.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(obj) = table.objects.iter_mut().find(|o| o.handle == f.handle) {
            if f.point > obj.point {
                obj.point = f.point;
            }
            out.notify.push((f.handle, obj.point));
        }
        if timed_out {
            out.timed_out.push(f);
        }
    }
    PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
    out
}

/// Resolve pending hardware fences now. Returns how many are still pending.
/// Cheap when nothing is pending (one relaxed load, no lock).
pub fn poll_pending() -> usize {
    if PENDING_COUNT.load(Ordering::Relaxed) == 0 {
        return 0;
    }
    let (deferred, left) = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        (d, table.pending.len())
    };
    deferred.run();
    left
}

/// Whether any hardware fence is still pending (lock-free).
pub fn has_pending() -> bool {
    PENDING_COUNT.load(Ordering::Relaxed) != 0
}

/// Make `handle` reach `point` once the GPU writes `>= payload` into the u32
/// at `fence_va` (a kernel mapping of pinned sysmem that outlives the fence).
/// `fence_gpu_va` is the producer channel's GPU VA of that semaphore
/// (`buf_gpu_va + fence_sem_off`); pass 0 if unknown.
/// Resolves immediately if the fence already landed (a fast GPU). Like a
/// direct signal, this REPLACES an imported `sync_file` fence the object
/// carried. Returns `false` for an unknown handle.
///
/// `binary` distinguishes the two `drm_syncobj` flavours, and it matters:
///
/// * A **timeline** syncobj is a monotonic counter, so a fence for a point at
///   or below the one already reached is moot and is dropped.
/// * A **binary** syncobj is a single fence *slot*, and every signal REPLACES
///   what it held — `drm_syncobj_replace_fence` in Linux. Its point is always
///   1, so dropping "already past that point" was dropping the fence of every
///   submit after the first: the object kept reading signaled while the GPU
///   was still writing, and the next waiter sailed straight through. That is
///   the normal `VkSemaphore` pattern (signal, wait, signal again, wait), so
///   a binary re-signal here un-signals the object back to 0 and re-arms it
///   on the new fence. Only a slot still in binary range is rewound: a handle
///   userspace has already driven past 1 as a timeline takes the timeline
///   branch instead, fence and all, so a stray binary signal cannot un-signal
///   it or discard the timeline fences still in flight on it.
pub fn attach_hw_fence(
    handle: u32,
    point: u64,
    fence_va: usize,
    fence_gpu_va: u64,
    payload: u32,
    ctx_idx: u32,
    binary: bool,
) -> bool {
    if fence_landed(fence_va, payload) {
        return timeline_signal(handle, point);
    }
    let deferred = {
        let mut table = TABLE.lock();
        let Some(obj) = table.objects.iter_mut().find(|o| o.handle == handle) else {
            return false;
        };
        obj.linked = None;
        let cur = obj.point;
        if binary && cur <= 1 {
            // Replace the slot's fence: rewind the point so waiters block
            // until THIS submit lands, and drop the fence the slot carried,
            // which this one supersedes.
            obj.point = 0;
            // Only the fences this binary signal actually supersedes, i.e. the
            // ones inside binary range. Dropping every pending fence on the
            // handle threw away a timeline fence in flight at a higher point --
            // the same bug the test below guards for a handle already driven
            // past binary range, which this arm (cur <= 1) never reached. With
            // its fence gone nothing would ever advance the counter to that
            // point, so a TIMELINE_WAIT on it parked until its deadline, which
            // for NVK's INT64_MAX is forever. Linux cannot hit this at all: a
            // binary replace swaps the single fence slot and cannot delete a
            // `dma_fence_chain` node at a higher seqno.
            table
                .pending
                .retain(|f| !(f.handle == handle && f.point <= 1));
        } else if point <= cur {
            // Already past that point: nothing to wait for.
            //
            // This also catches a *binary* signal on a handle userspace has
            // already driven past 1 as a timeline, which is not a slot in
            // binary range any more. Rewinding it would un-signal a genuine
            // timeline, and purging its pending fences would strand a waiter
            // on a point with nothing left to land and signal it.
            return true;
        }
        table.pending.push(PendingFence {
            handle,
            point,
            fence_va,
            fence_gpu_va,
            payload,
            ctx_idx,
            submitted_us: now_us(),
        });
        PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
        resolve_locked(&mut table)
    };
    deferred.run();
    // The point is *submitted* even if the GPU has not written the landing
    // zone yet. NVK/`QUERY LAST_SUBMITTED` and `WAIT_AVAILABLE` must see it
    // the moment EXEC returns; without this, Mesa's timeline walks a NULL
    // terminator (`libvulkan_nouveau.so+0x9cc48`) because it thinks nothing
    // was queued after the first EXEC.
    notify_signal(handle, point);
    true
}

/// The channel whose landing zone is `fence_va` is going away: every fence
/// still pending on it can never land. Release their waiters as a killed
/// channel's fences would be (signaled, so a compositor waiting on a dead
/// client's buffer moves on) without invoking the timeout hook. Returns how
/// many were abandoned.
pub fn abandon_fences(fence_va: usize) -> usize {
    let notify = {
        let mut table = TABLE.lock();
        let mut notify = Vec::new();
        let mut i = 0;
        while i < table.pending.len() {
            if table.pending[i].fence_va != fence_va {
                i += 1;
                continue;
            }
            let f = table.pending.swap_remove(i);
            if let Some(obj) = table.objects.iter_mut().find(|o| o.handle == f.handle) {
                if f.point > obj.point {
                    obj.point = f.point;
                }
                notify.push((f.handle, obj.point));
            }
        }
        PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
        notify
    };
    let n = notify.len();
    for (h, p) in notify {
        notify_signal(h, p);
    }
    n
}

/// Creates a syncobj, initially at point 0 (or 1 if `signaled`). Returns the
/// new handle.
pub fn create(signaled: bool) -> u32 {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    TABLE.lock().objects.push(Syncobj {
        handle,
        point: if signaled { 1 } else { 0 },
        refs: 1,
        linked: None,
    });
    handle
}

/// Adds one reference to `handle` (an fd export / `dup`). Returns `false` if
/// the handle is unknown.
pub fn add_ref(handle: u32) -> bool {
    let mut table = TABLE.lock();
    let Some(obj) = table.objects.iter_mut().find(|o| o.handle == handle) else {
        return false;
    };
    obj.refs = obj.refs.saturating_add(1);
    true
}

/// Drops one reference to `handle`. If other refs remain the object stays;
/// only the last reference removes it (and its pending fences). Returns
/// `false` if `handle` is unknown.
pub fn destroy(handle: u32) -> bool {
    let mut table = TABLE.lock();
    let Some(pos) = table.objects.iter().position(|o| o.handle == handle) else {
        return false;
    };
    if table.objects[pos].refs > 1 {
        table.objects[pos].refs -= 1;
        return true;
    }
    table.objects.swap_remove(pos);
    table
        .pending
        .retain(|f| !(f.handle == handle && f.point <= 1));
    PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
    true
}

/// If `handle` has an unresolved HW fence that will deliver at least `point`,
/// return `(fence_va_cpu, fence_gpu_va, payload, ctx_idx)`. Used by EXEC to
/// emit a GPU ACQUIRE instead of spinning on the CPU. `fence_gpu_va` is 0
/// when the producer did not publish one.
pub fn pending_hw_fence(handle: u32, point: u64) -> Option<(usize, u64, u32, u32)> {
    let (r, deferred) = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        // Binary waits (point 0/1): the highest pending fence on this handle.
        // Timeline: the lowest pending fence that covers `point`.
        let found = if point <= 1 {
            table
                .pending
                .iter()
                .filter(|f| f.handle == handle)
                .max_by_key(|f| f.point)
                .map(|f| (f.fence_va, f.fence_gpu_va, f.payload, f.ctx_idx))
        } else {
            table
                .pending
                .iter()
                .filter(|f| f.handle == handle && f.point >= point)
                .min_by_key(|f| f.point)
                .map(|f| (f.fence_va, f.fence_gpu_va, f.payload, f.ctx_idx))
        };
        (found, d)
    };
    deferred.run();
    r
}

/// Binary signal (point = 1). Returns `false` if `handle` is unknown.
pub fn signal(handle: u32) -> bool {
    timeline_signal(handle, 1)
}

/// Sets a syncobj's timeline point directly (`SYNCOBJ_TIMELINE_SIGNAL`, and
/// what a driver calls after confirming real GPU completion for `EXEC`'s
/// `sig` list). Monotonic: never moves the point backwards, matching real
/// `drm_syncobj` semantics (a stale/reordered signal can't un-signal a
/// later one). Returns `false` if `handle` is unknown.
pub fn timeline_signal(handle: u32, point: u64) -> bool {
    let (new_point, deferred) = {
        let mut table = TABLE.lock();
        let Some(obj) = table.objects.iter_mut().find(|o| o.handle == handle) else {
            return false;
        };
        if point > obj.point {
            obj.point = point;
        }
        // A direct signal replaces whatever fence the object carried, imported
        // sync_file included — same as real drm_syncobj.
        obj.linked = None;
        let p = obj.point;
        // Pending hardware fences at or below the new point are moot.
        table
            .pending
            .retain(|f| !(f.handle == handle && f.point <= p));
        PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
        (p, resolve_locked(&mut table))
    };
    // Lock released: service any SYNCOBJ_EVENTFD waiters this advance satisfies.
    notify_signal(handle, new_point);
    deferred.run();
    true
}

/// Snapshot of `handle`'s current fence, for
/// `SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE`: the point whose arrival
/// the exported fence stands for. `None` for an unknown handle.
///
/// A real `sync_file` carries the `dma_fence` that was attached to the
/// syncobj at export time, and becomes signaled when that fence does. Here
/// a fence IS "this timeline reached point N", so the snapshot is that N.
/// With the direct-submit path the producer's work may still be in flight
/// when it exports: the snapshot then names the HIGHEST point a pending
/// hardware fence will deliver, so the importer waits for exactly that
/// submission -- the fence that was current at export time, as in Linux.
///
/// The floor at 1 is the UNSIGNALED case: a binary syncobj that has never
/// been signaled (or was reset) sits at point 0, and a raw snapshot of 0
/// would make every import "reached" instantly (`src >= 0` is always true)
/// — the importer's wait would return before the producer ever signaled,
/// which is a premature-signal bug (a compositor reusing a buffer the
/// client still scans out, and the corruption points nowhere near sync).
/// Point 1 is the fence such a syncobj will signal next, so exporting "S
/// reaches 1" keeps the fd honest: pending until the source signals, and
/// identical to the raw snapshot for any already-signaled source. (Linux
/// instead refuses to export a fence-less syncobj with EINVAL; accepting it
/// as the next-signal fence is the closer fit here, where "attached but
/// unsignaled" and "no fence" are the same state.)
pub fn export_snapshot(handle: u32) -> Option<u64> {
    let (r, deferred) = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        let cur = effective_point(&table.objects, handle, LINK_DEPTH);
        let in_flight = table
            .pending
            .iter()
            .filter(|f| f.handle == handle)
            .map(|f| f.point)
            .max()
            .unwrap_or(0);
        (cur.map(|p| p.max(in_flight).max(1)), d)
    };
    deferred.run();
    r
}

/// `SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE`: make `dst` carry the fence
/// captured in a snapshot (`src` reaching `target`). Returns `false` if
/// either handle is unknown.
///
/// Resolved immediately when the snapshot is already satisfied (the common
/// case, see [`export_snapshot`]); otherwise the dependency is recorded and
/// resolves on its own as `src` advances, so a waiter never has to know an
/// import happened.
pub fn import_snapshot(dst: u32, src: u32, target: u64) -> bool {
    let (advanced, deferred) = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        let Some(src_point) = effective_point(&table.objects, src, LINK_DEPTH) else {
            return false;
        };
        let reached = src_point >= target;
        let Some(obj) = table.objects.iter_mut().find(|o| o.handle == dst) else {
            return false;
        };
        let adv = if reached {
            obj.point = obj.point.max(1);
            obj.linked = None;
            Some(obj.point)
        } else {
            // A binary import: `dst_point` is 1, because `IMPORT_SYNC_FILE`
            // really does replace the binary fence.
            obj.linked = Some((src, target, 1));
            None
        };
        (adv, d)
    };
    // Lock released: an already-satisfied import advanced `dst` — wake waiters.
    if let Some(p) = advanced {
        notify_signal(dst, p);
    }
    deferred.run();
    true
}

/// `SYNCOBJ_TRANSFER`: copy the fence "`src` reached `src_point`" onto `dst`
/// at `dst_point`. Both handles must exist (returns `false` otherwise). A
/// `*_point` of 0 selects the object's binary fence (its next signal),
/// matching [`export_snapshot`]'s floor-at-1 treatment of an unsignaled
/// binary syncobj.
///
/// If `src` has already reached the requested point, `dst` is advanced to
/// `dst_point` right away (monotonic — never backwards). If `src` is still
/// waiting on a pending HARDWARE fence covering that point, `dst` gets the
/// same hardware fence at `dst_point` (a timeline-exact transfer, as Linux
/// does with the dma_fence). Otherwise the dependency is recorded so `dst`
/// resolves on its own as `src` catches up, carrying `dst_point` with it: a
/// still-pending software timeline→timeline transfer reaches exactly the point
/// that was asked for, the way `drm_syncobj_add_point` does.
pub fn transfer(dst: u32, dst_point: u64, src: u32, src_point: u64) -> bool {
    let (new_point, deferred) = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        let Some(src_eff) = effective_point(&table.objects, src, LINK_DEPTH) else {
            return false;
        };
        let need = src_point.max(1);
        let reached = src_eff >= need;
        // The lowest pending hardware fence on `src` that covers `need`.
        let hw = table
            .pending
            .iter()
            .filter(|f| f.handle == src && f.point >= need)
            .min_by_key(|f| f.point)
            .copied();
        let Some(obj) = table.objects.iter_mut().find(|o| o.handle == dst) else {
            return false;
        };
        let np = if reached {
            obj.point = obj.point.max(dst_point.max(1));
            obj.linked = None;
            Some(obj.point)
        } else if let Some(f) = hw {
            obj.linked = None;
            let point = dst_point.max(1);
            if point > obj.point {
                table.pending.push(PendingFence {
                    handle: dst,
                    point,
                    fence_va: f.fence_va,
                    fence_gpu_va: f.fence_gpu_va,
                    payload: f.payload,
                    ctx_idx: f.ctx_idx,
                    submitted_us: f.submitted_us,
                });
                PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
            }
            None
        } else {
            obj.linked = Some((src, need, dst_point.max(1)));
            None
        };
        (np, d)
    };
    // Lock released: a satisfied transfer advanced `dst`, so wake its waiters.
    if let Some(p) = new_point {
        notify_signal(dst, p);
    }
    deferred.run();
    true
}

/// Resets a syncobj to point 0 (`SYNCOBJ_RESET`). Returns `false` if
/// `handle` is unknown. Drops any pending hardware fence it carried (the
/// fence is replaced, as in Linux).
pub fn reset(handle: u32) -> bool {
    let mut table = TABLE.lock();
    let Some(obj) = table.objects.iter_mut().find(|o| o.handle == handle) else {
        return false;
    };
    obj.point = 0;
    obj.linked = None;
    table
        .pending
        .retain(|f| !(f.handle == handle && f.point <= 1));
    PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
    true
}

/// Current timeline point (`SYNCOBJ_QUERY`), or `None` if `handle` is
/// unknown. Signaled point only — not in-flight hardware fences.
pub fn query(handle: u32) -> Option<u64> {
    query_inner(handle, false)
}

/// Last *submitted* point (`SYNCOBJ_QUERY` + `LAST_SUBMITTED`): the
/// signaled counter, or the highest pending hardware fence, whichever is
/// larger. EXEC's fast path attaches a fence and returns without waiting;
/// NVK then queries this to learn the timeline value of that submit.
pub fn query_submitted(handle: u32) -> Option<u64> {
    query_inner(handle, true)
}

fn query_inner(handle: u32, last_submitted: bool) -> Option<u64> {
    let (r, deferred) = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        let cur = effective_point(&table.objects, handle, LINK_DEPTH);
        let out = if last_submitted {
            cur.map(|p| {
                let inflight = table
                    .pending
                    .iter()
                    .filter(|f| f.handle == handle)
                    .map(|f| f.point)
                    .max()
                    .unwrap_or(0);
                p.max(inflight)
            })
        } else {
            cur
        };
        (out, d)
    };
    deferred.run();
    r
}

pub enum WaitOutcome {
    /// All (`wait_all`) or at least one required handles reached their
    /// target point. Carries the index into `handles` of the first one
    /// observed signaled, for `drm_syncobj_wait.first_signaled`.
    Signaled { first_signaled_index: u32 },
    /// `deadline` (absolute, `kernel_hal::timer::timer_now()`-comparable --
    /// see the microsecond conversion at the call site) passed first.
    Timeout,
    /// One of `handles` does not exist.
    Invalid,
}

/// Polls `handles` until every one (`wait_all = true`) or any one
/// (`wait_all = false`) reaches its target point, or `deadline_us`
/// (absolute microseconds, same clock as [`crate::bus::drivers_timer_now_as_micros`])
/// passes. `points`, if given, is per-handle target points
/// (`SYNCOBJ_TIMELINE_WAIT`); `None` means "target = 1" for every handle
/// (binary `SYNCOBJ_WAIT`). After the async pre-wait in `sys_ioctl`, this
/// normally returns on the first iteration; the loop is the fallback when
/// called without that sleep (e.g. EXEC's CPU wait path).
pub fn wait(
    handles: &[u32],
    points: Option<&[u64]>,
    wait_all: bool,
    deadline_us: u64,
) -> WaitOutcome {
    wait_inner(handles, points, wait_all, deadline_us, None, false)
}

/// [`wait`] but `WAIT_AVAILABLE`: a pending hardware fence that covers the
/// target counts as satisfied (fence submitted, not necessarily signaled).
pub fn wait_available(
    handles: &[u32],
    points: Option<&[u64]>,
    wait_all: bool,
    deadline_us: u64,
) -> WaitOutcome {
    wait_inner(handles, points, wait_all, deadline_us, None, true)
}

/// [`wait`] for a GPU driver about to submit on channel `ctx_idx`: a handle
/// whose target is covered by a hardware fence PENDING ON THAT SAME CHANNEL
/// counts as satisfied without waiting, because the GPFIFO executes in order
/// -- the new submission cannot run before the fence lands, which is the
/// only guarantee the wait exists to give. Fences on other channels (another
/// process's work, an imported sync_file) are still waited for on the CPU.
/// The syncobj itself stays pending until the fence really lands.
pub fn wait_ordered(
    handles: &[u32],
    points: Option<&[u64]>,
    deadline_us: u64,
    ctx_idx: u32,
) -> WaitOutcome {
    wait_inner(handles, points, true, deadline_us, Some(ctx_idx), false)
}

/// Non-blocking probe for the async sleep loop in `sys_ioctl` (mirrors
/// `WAIT_VBLANK`'s pre-`io_control` sleep). Call [`poll_pending`] first, then
/// this. Returns:
/// - `Some(Ok(first_signaled_index))` if the wait condition is already met
/// - `Some(Err(WaitOutcome::Timeout))` if past `deadline_us` and still unmet
/// - `Some(Err(WaitOutcome::Invalid))` if a handle is unknown
/// - `None` if the caller should sleep (~1 ms or until the deadline) and retry
pub fn wait_ready(
    handles: &[u32],
    points: Option<&[u64]>,
    wait_all: bool,
    deadline_us: u64,
) -> Option<core::result::Result<u32, WaitOutcome>> {
    wait_ready_inner(handles, points, wait_all, deadline_us, false)
}

/// [`wait_ready`] with `WAIT_AVAILABLE`: a pending hardware fence covering the
/// target counts as satisfied.
pub fn wait_available_ready(
    handles: &[u32],
    points: Option<&[u64]>,
    wait_all: bool,
    deadline_us: u64,
) -> Option<core::result::Result<u32, WaitOutcome>> {
    wait_ready_inner(handles, points, wait_all, deadline_us, true)
}

fn wait_ready_inner(
    handles: &[u32],
    points: Option<&[u64]>,
    wait_all: bool,
    deadline_us: u64,
    available_only: bool,
) -> Option<core::result::Result<u32, WaitOutcome>> {
    let mut signaled_count = 0usize;
    let mut first_signaled: Option<u32> = None;
    let deferred = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        for (i, &h) in handles.iter().enumerate() {
            let Some(point) = effective_point(&table.objects, h, LINK_DEPTH) else {
                drop(table);
                d.run();
                return Some(Err(WaitOutcome::Invalid));
            };
            // Point 0 is NOT "already satisfied". Linux resolves a wait point
            // through `drm_syncobj_find_fence()`, and point 0 there means "the
            // syncobj's current fence" -- the wait still blocks until that
            // fence signals. With a bare `point >= 0` an unsignaled, fenceless
            // syncobj reported itself signaled, and Mesa's
            // `vk_drm_syncobj_wait_many` passes wait_value 0 for the BINARY
            // syncs in any batch that also carries a timeline one. So every
            // mixed `vkWaitSemaphores`/`vkQueueSubmit` wait list returned
            // VK_SUCCESS on its binary halves without waiting, and NVK went on
            // to recycle command buffers the GPU was still reading. Flooring to
            // 1 is what the rest of this module already does (`export_snapshot`,
            // `transfer`, the eventfd registry).
            let target = points.map(|p| p[i]).unwrap_or(1).max(1);
            let pending_covers = available_only
                && table
                    .pending
                    .iter()
                    .any(|f| f.handle == h && f.point >= target);
            if point >= target || pending_covers {
                signaled_count += 1;
                if first_signaled.is_none() {
                    first_signaled = Some(i as u32);
                }
            }
        }
        d
    };
    deferred.run();
    let done = if wait_all {
        signaled_count == handles.len()
    } else {
        signaled_count > 0 && !handles.is_empty()
    };
    if done {
        return Some(Ok(first_signaled.unwrap_or(0)));
    }
    if now_us() >= deadline_us {
        return Some(Err(WaitOutcome::Timeout));
    }
    None
}

fn wait_inner(
    handles: &[u32],
    points: Option<&[u64]>,
    wait_all: bool,
    deadline_us: u64,
    ordered_ctx: Option<u32>,
    available_only: bool,
) -> WaitOutcome {
    let start_us = now_us();
    let mut stall_logged = false;
    WAIT_CALLS.fetch_add(1, Ordering::Relaxed);
    let account = |timed_out: bool| {
        let spent = now_us().wrapping_sub(start_us);
        WAIT_SPIN_US.fetch_add(spent, Ordering::Relaxed);
        WAIT_MAX_US.fetch_max(spent, Ordering::Relaxed);
        if timed_out {
            WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        }
    };
    loop {
        let mut signaled_count = 0usize;
        let mut first_signaled: Option<u32> = None;
        let deferred = {
            let mut table = TABLE.lock();
            let d = resolve_locked(&mut table);
            for (i, &h) in handles.iter().enumerate() {
                let Some(point) = effective_point(&table.objects, h, LINK_DEPTH) else {
                    drop(table);
                    d.run();
                    account(false);
                    return WaitOutcome::Invalid;
                };
                // Point 0 is NOT "already satisfied". Linux resolves a wait point
                // through `drm_syncobj_find_fence()`, and point 0 there means "the
                // syncobj's current fence" -- the wait still blocks until that
                // fence signals. With a bare `point >= 0` an unsignaled, fenceless
                // syncobj reported itself signaled, and Mesa's
                // `vk_drm_syncobj_wait_many` passes wait_value 0 for the BINARY
                // syncs in any batch that also carries a timeline one. So every
                // mixed `vkWaitSemaphores`/`vkQueueSubmit` wait list returned
                // VK_SUCCESS on its binary halves without waiting, and NVK went on
                // to recycle command buffers the GPU was still reading. Flooring to
                // 1 is what the rest of this module already does (`export_snapshot`,
                // `transfer`, the eventfd registry).
                let target = points.map(|p| p[i]).unwrap_or(1).max(1);
                let pending_covers = table
                    .pending
                    .iter()
                    .any(|f| f.handle == h && f.point >= target);
                let ordered = match ordered_ctx {
                    Some(ctx) => table
                        .pending
                        .iter()
                        .any(|f| f.handle == h && f.ctx_idx == ctx && f.point >= target),
                    None => false,
                };
                if point >= target || ordered || (available_only && pending_covers) {
                    signaled_count += 1;
                    if first_signaled.is_none() {
                        first_signaled = Some(i as u32);
                    }
                }
            }
            d
        };
        deferred.run();
        let done = if wait_all {
            signaled_count == handles.len()
        } else {
            signaled_count > 0 && !handles.is_empty()
        };
        if done {
            account(false);
            return WaitOutcome::Signaled {
                first_signaled_index: first_signaled.unwrap_or(0),
            };
        }
        let now_us = now_us();
        if now_us >= deadline_us {
            account(true);
            return WaitOutcome::Timeout;
        }
        // Stall reporter: NVK's fence waits pass an effectively infinite
        // absolute deadline (INT64_MAX ns), so a syncobj that never gets
        // signaled parks its caller here FOREVER with nothing in dmesg --
        // the exact shape of the vkcube/eglgears "hangs after device
        // creation" reports. Crossing 2 s with the deadline still far away
        // is that situation, not a normal frame wait; say so once per call
        // (budgeted per boot) with enough to identify the station.
        if !stall_logged && now_us.saturating_sub(start_us) >= 2_000_000 {
            stall_logged = true;
            stall_report(
                handles,
                points,
                wait_all,
                deadline_us.saturating_sub(now_us),
            );
        }
        core::hint::spin_loop();
    }
}

/// One console line for a wait parked past 2 s: every handle with its target
/// point and current point (-1 = handle vanished mid-wait). Budgeted per boot
/// so a session full of legitimately-slow waits cannot storm the UART (klog
/// writes synchronously to it -- an uncapped line on a re-entered path is how
/// the pointer froze once before).
fn stall_report(handles: &[u32], points: Option<&[u64]>, wait_all: bool, remaining_us: u64) {
    static BUDGET: AtomicU32 = AtomicU32::new(0);
    const MAX_REPORTS: u32 = 8;
    let n = BUDGET.fetch_add(1, Ordering::Relaxed);
    if n >= MAX_REPORTS {
        return;
    }
    let list = describe(handles, points);
    crate::klog_warn!(
        "[syncobj] WAIT parked >2s and still unsignaled (wait_all={} deadline in {}s):{} (handle:target/current; report {}/{} this boot)",
        wait_all,
        remaining_us / 1_000_000,
        list,
        n + 1,
        MAX_REPORTS
    );
}

/// `" handle:target/current[+pending]"` for every handle, `-1` for one that
/// does not exist (`+N` = a hardware fence for point N is still in flight).
/// The one line that turns "a wait timed out" into "THIS fence never
/// arrived", so every caller that gives up on a wait should print it — the
/// stall reporter above, and the driver's own `EXEC` timeout, whose 10 s
/// deadline expires long after this reporter's 2 s threshold and used to
/// report nothing but a count.
pub fn describe(handles: &[u32], points: Option<&[u64]>) -> alloc::string::String {
    let mut list = alloc::string::String::new();
    let table = TABLE.lock();
    for (i, &h) in handles.iter().enumerate() {
        let target = points.map(|p| p[i]).unwrap_or(1);
        let cur = effective_point(&table.objects, h, LINK_DEPTH).map_or(-1i64, |p| p as i64);
        let _ = core::fmt::write(&mut list, format_args!(" {:#x}:{}/{}", h, target, cur));
        for f in table.pending.iter().filter(|f| f.handle == h) {
            let _ = core::fmt::write(&mut list, format_args!("+{}(ctx{})", f.point, f.ctx_idx));
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fence landing zone the test drives by hand, standing in for the
    /// sysmem word the GPU's host-semaphore RELEASE writes.
    struct Landing(alloc::boxed::Box<u32>);

    impl Landing {
        fn new() -> Self {
            Self(alloc::boxed::Box::new(0))
        }
        fn va(&self) -> usize {
            &*self.0 as *const u32 as usize
        }
        fn land(&mut self, payload: u32) {
            *self.0 = payload;
        }
    }

    /// The bug this guards: a binary syncobj is a fence SLOT, not a counter.
    /// Signalling it a second time used to hit `point <= obj.point` (its point
    /// is always 1) and drop the new fence on the floor, leaving the object
    /// reading "signaled" while the GPU was still writing — so the next waiter
    /// went straight through and sampled a half-written buffer.
    #[test]
    fn a_second_binary_signal_rearms_the_syncobj_on_the_new_fence() {
        let h = create(false);
        let mut first = Landing::new();
        let mut second = Landing::new();

        // First submit signals it; the fence lands; the object is signaled.
        assert!(attach_hw_fence(h, 1, first.va(), 0, 1, 0, true));
        assert_eq!(query(h), Some(0), "unsignaled until the fence lands");
        first.land(1);
        assert_eq!(query(h), Some(1), "signaled once the GPU wrote the fence");

        // Second submit signals the SAME handle. The object must go back to
        // unsignaled and wait for THIS fence, not report the old one.
        assert!(attach_hw_fence(h, 1, second.va(), 0, 1, 0, true));
        assert_eq!(
            query(h),
            Some(0),
            "a re-signalled binary syncobj must not still read as signaled"
        );
        // ...while LAST_SUBMITTED and WAIT_AVAILABLE still see the submit, and
        // EXEC can still find the fence to turn into a GPU-side ACQUIRE.
        assert_eq!(query_submitted(h), Some(1));
        assert_eq!(
            pending_hw_fence(h, 1).map(|f| f.0),
            Some(second.va()),
            "the pending fence must be the new one"
        );

        second.land(1);
        assert_eq!(query(h), Some(1));

        destroy(h);
    }

    /// The timeline flavour keeps its monotonic-counter semantics: a fence for
    /// a point already reached is genuinely moot and stays dropped.
    #[test]
    fn a_timeline_fence_at_or_below_the_current_point_is_still_dropped() {
        let h = create(false);
        let mut fence = Landing::new();

        assert!(attach_hw_fence(h, 5, fence.va(), 0, 1, 0, false));
        fence.land(1);
        assert_eq!(query(h), Some(5));

        // Point 3 is behind the timeline: nothing to wait for, and the object
        // must not be rewound.
        let stale = Landing::new();
        assert!(attach_hw_fence(h, 3, stale.va(), 0, 7, 0, false));
        assert_eq!(query(h), Some(5));
        assert!(pending_hw_fence(h, 3).is_none());

        destroy(h);
    }

    /// A transfer whose source has not landed yet has to remember WHICH point
    /// it was asked to reach. Landing `dst` at 1 instead looks like a rounding
    /// error and is a hang: wlroots' `linux-drm-syncobj-v1` moves a client's
    /// acquire point into its own timeline at a point it picks, then waits on
    /// that point, so a point that never arrives freezes the surface and, with
    /// the commit waiting on it, the whole output.
    #[test]
    fn a_deferred_transfer_reaches_the_point_it_was_given() {
        let src = create(false);
        let dst = create(false);

        // `src` is nowhere near point 5 yet, so the transfer is deferred.
        assert!(transfer(dst, 9, src, 5));
        assert_eq!(query(dst), Some(0), "nothing has landed yet");

        // `src` arrives. `dst` must now be at 9, not 1.
        assert!(timeline_signal(src, 5));
        assert_eq!(
            query(dst),
            Some(9),
            "a deferred transfer must reach its destination point"
        );
        // And a wait on that point is satisfied, which is the thing that hung.
        assert!(matches!(
            wait(&[dst], Some(&[9]), false, 0),
            WaitOutcome::Signaled { .. }
        ));

        destroy(dst);
        destroy(src);
    }

    /// Mesa's `vk_drm_syncobj_wait_many` uses the TIMELINE wait whenever any
    /// entry in the batch carries a non-zero value, and passes wait_value 0 for
    /// the BINARY syncs sitting in that same batch. Point 0 therefore has to
    /// mean "wait for this syncobj's current fence", as `drm_syncobj_find_fence`
    /// makes it in Linux -- never "already satisfied". It returning signaled is
    /// silent: every caller gets VK_SUCCESS and races the GPU.
    #[test]
    fn a_timeline_wait_on_point_zero_still_waits() {
        let h = create(false);
        assert_eq!(query(h), Some(0));

        // Unsignaled, no fence attached: point 0 must NOT report signaled.
        assert!(matches!(
            wait(&[h], Some(&[0]), false, 0),
            WaitOutcome::Timeout
        ));
        // And the readiness probe must agree: `None` is "sleep and retry",
        // i.e. not satisfied. (A zero deadline would report Timeout instead,
        // which is the same verdict by another name.)
        assert!(wait_ready(&[h], Some(&[0]), false, u64::MAX).is_none());

        // Once it IS signaled, point 0 is satisfied.
        assert!(signal(h));
        assert!(matches!(
            wait(&[h], Some(&[0]), false, 0),
            WaitOutcome::Signaled {
                first_signaled_index: 0
            }
        ));

        destroy(h);
    }

    /// A binary signal on a handle userspace has already driven past 1 as a
    /// timeline must take the timeline branch, not the slot-replacement one.
    /// Rewinding is wrong there (it would un-signal a real timeline), and so
    /// is purging the handle's pending fences: the first cut of the binary
    /// fix dropped them unconditionally, which threw away a timeline fence
    /// still in flight. Nothing was then left to signal its point when the
    /// GPU landed it, so `pending_hw_fence` returned `None`, `query` stayed
    /// below the point, and a waiter on it hung.
    #[test]
    fn a_binary_signal_does_not_strand_a_timeline_fence_in_flight() {
        let h = create(false);
        let mut reached = Landing::new();
        let inflight = Landing::new();

        // Drive the handle to 5 as a timeline.
        assert!(attach_hw_fence(h, 5, reached.va(), 0, 1, 0, false));
        reached.land(1);
        assert_eq!(query(h), Some(5));

        // Point 7 is submitted and still in flight.
        assert!(attach_hw_fence(h, 7, inflight.va(), 0, 3, 0, false));
        assert_eq!(query_submitted(h), Some(7));

        // A stray binary signal arrives on the same handle.
        let stray = Landing::new();
        assert!(attach_hw_fence(h, 1, stray.va(), 0, 1, 0, true));

        assert_eq!(
            query(h),
            Some(5),
            "a binary signal must not rewind a handle already past binary range"
        );
        assert_eq!(
            pending_hw_fence(h, 7).map(|f| f.0),
            Some(inflight.va()),
            "the in-flight timeline fence must survive the binary signal"
        );
        assert_eq!(query_submitted(h), Some(7));

        destroy(h);
    }

    /// The same stranding, one arm over: a handle still INSIDE binary range
    /// (point 0 or 1) with a timeline fence in flight at a higher point. The
    /// binary arm rewinds the counter, which is right, and used to purge every
    /// pending fence on the handle, which is not -- the in-flight one is not
    /// superseded by a binary signal, and with it gone nothing would ever
    /// advance the counter to its point.
    #[test]
    fn a_binary_signal_in_binary_range_spares_a_higher_timeline_fence() {
        let h = create(false);
        let inflight = Landing::new();

        // Point 7 submitted while the handle is still at 0.
        assert!(attach_hw_fence(h, 7, inflight.va(), 0, 3, 0, false));
        assert_eq!(query(h), Some(0));
        assert_eq!(query_submitted(h), Some(7));

        // A binary signal arrives on the same handle.
        let stray = Landing::new();
        assert!(attach_hw_fence(h, 1, stray.va(), 0, 1, 0, true));

        assert_eq!(
            pending_hw_fence(h, 7).map(|f| f.0),
            Some(inflight.va()),
            "the in-flight timeline fence must survive a binary signal"
        );
        assert_eq!(query_submitted(h), Some(7));

        destroy(h);
    }
}
