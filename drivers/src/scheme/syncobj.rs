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
//! [`wait`] is a bounded, CPU-spinning poll of that counter, not a real
//! wait-queue: `linux-object`'s `io_control` (where these ioctls are
//! dispatched) is a synchronous, non-async function, so there is no
//! lower-cost way to block here without deeper scheduler surgery. This
//! matches the spin-poll idiom already used throughout this codebase for
//! bounded hardware waits (e.g. `nvidia.rs` `gmmu_flush`, `eclipse_rm_init.c`
//! step18's semaphore poll) -- consistent, but real: a long wait pegs the
//! CPU core handling the ioctl for its whole duration.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use lock::Mutex;

struct Syncobj {
    handle: u32,
    point: u64,
    /// A pending `sync_file` import (see [`import_snapshot`]): this object
    /// also counts as signaled — binary point 1 — once `src` reaches
    /// `target`. `None` for the normal case, and cleared whenever the object
    /// is signaled or reset directly, mirroring how a real `drm_syncobj`
    /// REPLACES its fence on those operations rather than accumulating them.
    linked: Option<(u32, u64)>,
}

/// "`handle` reaches `point` once the GPU has written a value >= `payload`
/// into the u32 at `fence_va`" -- the lazy form of a hardware completion.
/// `fence_va` is a kernel virtual address of pinned sysmem (the channel's
/// own scratch buffer, see `eclipse_rm_exec_fast_prepare`); `payload` is a
/// per-context monotonic sequence, so landing is a wrapping `>=`, which
/// stays correct when several fences share one landing zone.
#[derive(Clone, Copy)]
struct PendingFence {
    handle: u32,
    point: u64,
    fence_va: usize,
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
    if let (Some((src, target)), true) = (obj.linked, depth > 0) {
        if let Some(src_point) = effective_point(objects, src, depth - 1) {
            if src_point >= target {
                point = point.max(1);
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
const FENCE_TIMEOUT_US: u64 = 1_000_000;

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
/// Resolves immediately if the fence already landed (a fast GPU). Like a
/// direct signal, this REPLACES an imported `sync_file` fence the object
/// carried. Returns `false` for an unknown handle.
pub fn attach_hw_fence(
    handle: u32,
    point: u64,
    fence_va: usize,
    payload: u32,
    ctx_idx: u32,
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
        if point <= obj.point {
            // Already past that point: nothing to wait for.
            return true;
        }
        table.pending.push(PendingFence {
            handle,
            point,
            fence_va,
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
        linked: None,
    });
    handle
}

/// Destroys a syncobj. Returns `false` if `handle` is unknown.
pub fn destroy(handle: u32) -> bool {
    let mut table = TABLE.lock();
    let len_before = table.objects.len();
    table.objects.retain(|o| o.handle != handle);
    table.pending.retain(|f| f.handle != handle);
    PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
    table.objects.len() != len_before
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
            obj.linked = Some((src, target));
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
/// does with the dma_fence). Otherwise the dependency is recorded like a
/// `sync_file` import so `dst` resolves on its own as `src` catches up --
/// that branch can only carry a binary dependency, so a still-pending
/// software timeline→timeline transfer lands `dst` at point 1 rather than
/// an arbitrary `dst_point`.
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
                    payload: f.payload,
                    ctx_idx: f.ctx_idx,
                    submitted_us: f.submitted_us,
                });
                PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
            }
            None
        } else {
            obj.linked = Some((src, need));
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
    table.pending.retain(|f| f.handle != handle);
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
/// (binary `SYNCOBJ_WAIT`). Spin-polls -- see the module doc for why.
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
                let target = points.map(|p| p[i]).unwrap_or(1);
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
/// stall reporter above, and the driver's own `EXEC` timeout, whose 1 s
/// deadline expires long before this reporter's 2 s threshold and used to
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
