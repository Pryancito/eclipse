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
    /// The processes holding those references, one entry per reference a
    /// `SYNCOBJ_CREATE` or an opaque `SYNCOBJ_FD_TO_HANDLE` gave out (Linux:
    /// the handle lives in that `drm_file`'s table). Kernel-owned references
    /// (a sync_file's, a merge's, a test's) have no entry. `release_owner`
    /// drops a dead process's entries, and `destroy_for` refuses a process
    /// that holds none.
    holders: alloc::vec::Vec<u64>,
    /// The pending dependencies on other syncobjs, in ascending `dst_point`
    /// order: the object reaches a link's `dst_point` once that link's
    /// sources have reached their targets -- AND every link below it has
    /// resolved, as a node of a `dma_fence_chain` is signaled only once the
    /// nodes before it are. Empty for the normal case.
    ///
    /// One link is set by a `sync_file` import (see [`import_snapshot`],
    /// where `dst_point` is 1, because a binary import really does replace
    /// the binary fence) or by a [`transfer`] whose source has not landed
    /// yet. A binary signal, import or transfer replaces them all, as a real
    /// `drm_syncobj` REPLACES its single fence; a timeline point is ADDED
    /// behind the ones already there (`drm_syncobj_add_point`), which is
    /// what Mesa's many-to-many `vk_drm_syncobj_transfer_payloads` leans
    /// on: it transfers each wait into a temporary timeline at points 1..N
    /// and then transfers the temporary "as a binary" (point 0) into every
    /// signal semaphore, relying on "waiting on a whole chain waits on
    /// everything". With a single link, point N+1 threw away what point N
    /// was still waiting on, and the signal semaphores followed one wait
    /// out of N. A landed hardware fence or a reached transfer at a point
    /// above a link still waiting queues behind it the same way, as an
    /// empty link ([`Syncobj::push_link`]).
    ///
    /// `dst_point` used to be missing, so a software timeline-to-timeline
    /// transfer landed `dst` at 1 whatever point was asked for. Linux allocates
    /// a `dma_fence_chain` node and `drm_syncobj_add_point(dst, chain, fence,
    /// args->dst_point)`, so `dst` reaches exactly that point. The gap was a
    /// hang, not a rounding error: wlroots' `linux-drm-syncobj-v1` moves a
    /// client's acquire point into its own timeline at a point it chooses, then
    /// waits on it, so a point that never arrives freezes that surface.
    links: Vec<Link>,
}

impl Syncobj {
    /// Add a link, keeping [`Self::links`] in chain order (ascending
    /// `dst_point`). An empty `deps` is a point already delivered that
    /// waits only for the links below it.
    fn push_link(&mut self, link: Link) {
        let point = link.dst_point.max(1);
        let at = self
            .links
            .iter()
            .position(|l| l.dst_point.max(1) > point)
            .unwrap_or(self.links.len());
        self.links.insert(at, link);
    }

    /// The links `target` depends on: every one up to and including the
    /// first whose point covers it. `None` when no link promises `target`.
    fn links_to(&self, target: u64) -> Option<&[Link]> {
        let end = self
            .links
            .iter()
            .position(|l| l.dst_point.max(1) >= target)?;
        Some(&self.links[..=end])
    }
}

/// Whether a node of `obj`'s chain below `point` is still waiting -- a link,
/// or a hardware fence of its own in flight above its counter -- so that
/// `point`, delivered now, has to queue behind it.
fn node_below_waiting(table: &SyncobjTable, obj: &Syncobj, point: u64) -> bool {
    obj.links.iter().any(|l| l.dst_point.max(1) < point)
        || table
            .pending
            .iter()
            .any(|g| g.handle == obj.handle && g.point > obj.point && g.point < point)
}

/// A deferred dependency: the object reaches `dst_point` once EVERY entry of
/// `deps` -- "`src` has reached `target`" -- is satisfied.
///
/// One entry is a `sync_file` import or a [`transfer`]. Several entries are
/// a **merged** `sync_file` (`SYNC_IOC_MERGE`, see [`merge_fences`]): Linux
/// builds a `dma_fence_array` that signals when all of its fences have, and
/// Mesa's window-system code leans on it every frame under X11 -- the
/// acquire semaphore of a swapchain image is the merge of "the compositor
/// released the image" and "the previous present of it completed".
struct Link {
    deps: Vec<(u32, u64)>,
    dst_point: u64,
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
fn effective_point(table: &SyncobjTable, handle: u32, depth: u8) -> Option<u64> {
    let obj = table.objects.iter().find(|o| o.handle == handle)?;
    let mut point = obj.point;
    if depth > 0 {
        // The walk of `dma_fence_chain_signaled`: a link's point counts
        // once its own sources AND every node below it -- link or fence
        // in flight -- are reached, so the walk stops at the first one
        // still waiting.
        for link in &obj.links {
            if !link_ready(table, obj, link, depth) {
                break;
            }
            point = point.max(link.dst_point.max(1));
        }
    }
    Some(point)
}

/// Whether `link` (one of `obj`'s) can resolve: every source has reached
/// its target, and no hardware fence of `obj`'s own is still in flight
/// below the link's point. Such a fence is a node of the chain before it
/// (two channels signaling one timeline out of order), and the link waits
/// for it as it waits for a link below it. `depth` bounds the sources'
/// own links, as in [`effective_point`].
fn link_ready(table: &SyncobjTable, obj: &Syncobj, link: &Link, depth: u8) -> bool {
    !node_below_waiting(table, obj, link.dst_point.max(1))
        && link.deps.iter().all(|&(src, target)| {
            effective_point(table, src, depth - 1).is_some_and(|p| p >= target)
        })
}

/// Link-following depth for [`effective_point`]. The X11 route of a
/// swapchain image is five links deep before any driver adds its own: the
/// acquire semaphore -> the merged sync_file -> the surrogate the release
/// point was transferred to -> the release timeline -> the surrogate the
/// compositor imported its render fence into -> that fence's syncobj.
const LINK_DEPTH: u8 = 8;

/// The hardware fences in flight that deliver `target` on `handle`, seen
/// THROUGH the link an import, a deferred transfer or a merge carries.
///
/// A link names its source by handle, so the fence the GPU will write sits
/// in the source's row and never in the importer's -- and the importer is
/// the handle the client waits on. The acquire semaphore of an X11
/// swapchain image is always such an importer, so looked up by its own
/// handle it had no fence, and EXEC parked on the CPU instead of emitting
/// a GPU ACQUIRE for it. The lowest of `handle`'s own fences covering
/// `target` wins, alone; otherwise a link that delivers at least `target`
/// is followed to EVERY source still short of its point, and the list is
/// what a `dma_fence_array` is to Linux's scheduler: one dependency per
/// fence, so a merge of two fences in flight is two ACQUIREs. That merge is
/// the X11 acquire itself -- "the compositor released the image" on the
/// compositor's channel AND "our previous present of it completed" on our
/// own -- and while the client's own half was still running, following
/// only a lone source left the whole wait on the CPU, inside the ioctl,
/// until the compositor's frame had run. A source with nothing submitted
/// has no fence: then the list is empty, and the caller waits on the CPU
/// as before. A chain is followed link by link up to the one that
/// delivers `target`: every one of them has to resolve first.
///
/// Callers must hold the table lock, with pending fences resolved.
fn fences_through_link(
    table: &SyncobjTable,
    handle: u32,
    target: u64,
    depth: u8,
) -> alloc::vec::Vec<PendingFence> {
    let Some(obj) = table.objects.iter().find(|o| o.handle == handle) else {
        return alloc::vec::Vec::new();
    };
    // What delivers `target`: the lowest own fence covering it, or the
    // first link promising it, whichever comes lower -- and, as a node of
    // a `dma_fence_chain` is signaled only once the nodes before it are,
    // everything on the handle below that point too: its own fences still
    // above the counter and its links, each followed to its sources.
    let own_cover = table
        .pending
        .iter()
        .filter(|f| f.handle == handle && f.point >= target)
        .map(|f| f.point)
        .min();
    let link_cover = obj
        .links_to(target)
        .map(|l| l[l.len() - 1].dst_point.max(1));
    let cover = match (own_cover, link_cover) {
        (None, None) => return alloc::vec::Vec::new(),
        (a, b) => a.into_iter().chain(b).min().unwrap_or(target),
    };
    let mut fences: alloc::vec::Vec<PendingFence> = table
        .pending
        .iter()
        .filter(|f| f.handle == handle && f.point > obj.point && f.point <= cover)
        .copied()
        .collect();
    for &(src, t) in obj
        .links
        .iter()
        .filter(|l| l.dst_point.max(1) <= cover)
        .flat_map(|l| l.deps.iter())
    {
        if effective_point(table, src, LINK_DEPTH).is_some_and(|p| p >= t) {
            continue;
        }
        if depth == 0 {
            return alloc::vec::Vec::new();
        }
        let behind = fences_through_link(table, src, t, depth - 1);
        if behind.is_empty() {
            return behind;
        }
        fences.extend(behind);
    }
    fences
}

/// Whether `target` on `handle` is SUBMITTED: reached, covered by a fence
/// in flight, or promised by a link whose every source is itself submitted
/// to its point. This is what `WAIT_AVAILABLE` and `QUERY LAST_SUBMITTED`
/// ask, and an importer is exactly as submitted as its source: Mesa's
/// submit thread waits for its semaphores to be *available* before
/// queueing, so an importer that only counted its own fences kept that
/// thread parked until the source's work had run, not until it was queued.
///
/// Callers must hold the table lock, with pending fences resolved.
fn submitted_locked(table: &SyncobjTable, handle: u32, target: u64, depth: u8) -> bool {
    if effective_point(table, handle, LINK_DEPTH).is_some_and(|p| p >= target) {
        return true;
    }
    if table
        .pending
        .iter()
        .any(|f| f.handle == handle && f.point >= target)
    {
        return true;
    }
    if depth == 0 {
        return false;
    }
    let Some(links) = table
        .objects
        .iter()
        .find(|o| o.handle == handle)
        .and_then(|o| o.links_to(target))
    else {
        return false;
    };
    links
        .iter()
        .flat_map(|l| l.deps.iter())
        .all(|&(src, t)| submitted_locked(table, src, t, depth - 1))
}

/// The point a link on `handle` promises, if it carries one (0 otherwise).
/// `EXPORT_SYNC_FILE` names the fence attached LAST, and on a timeline that
/// still carries a deferred transfer that is the transfer's point, not the
/// counter: exporting the counter handed the importer a fence already
/// reached, signaled before the transferred work had run.
fn promised_point(table: &SyncobjTable, handle: u32) -> u64 {
    table
        .objects
        .iter()
        .find(|o| o.handle == handle)
        .and_then(|o| o.links.last())
        .map_or(0, |l| l.dst_point.max(1))
}

/// Whether some object's link still names `handle` as a source.
fn is_link_source(objects: &[Syncobj], handle: u32) -> bool {
    objects.iter().any(|o| {
        o.links
            .iter()
            .any(|l| l.deps.iter().any(|&(src, _)| src == handle))
    })
}

/// Drop every object whose last reference is gone and that no link names
/// any more (see the orphan rule in [`destroy`]). Cheap when there are none.
fn collect_orphans(table: &mut SyncobjTable) {
    if !table.objects.iter().any(|o| o.refs == 0) {
        return;
    }
    while let Some(pos) = table
        .objects
        .iter()
        .position(|o| o.refs == 0 && !is_link_source(&table.objects, o.handle))
    {
        let gone = table.objects.swap_remove(pos).handle;
        table.pending.retain(|f| f.handle != gone);
        table.errored.retain(|e| e.handle != gone);
    }
    PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
}

/// `drm_syncobj_replace_fence` on a binary syncobj: whatever fence the slot
/// carried is superseded by the one being installed -- a hardware fence
/// still in flight, and the timeout mark of one that was given up on. Only
/// the slot's own range: a timeline fence at a higher point is not this
/// slot's (see [`attach_hw_fence`]). Callers must hold the table lock.
fn drop_binary_fence(table: &mut SyncobjTable, handle: u32) {
    table
        .pending
        .retain(|f| !(f.handle == handle && f.point <= 1));
    table.errored.retain(|e| !(e.handle == handle && e.to <= 1));
    PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
}

/// Whether `target` on `handle` was reached only by a fence that timed out
/// (see [`Errored`]). Callers must hold the table lock, resolved.
fn errored_locked(table: &SyncobjTable, handle: u32, target: u64) -> bool {
    table
        .errored
        .iter()
        .any(|e| e.handle == handle && e.from < target && target <= e.to)
}

/// Record that `handle` went from `from` to `to` on the strength of a
/// timed-out fence (or of a link to one). Nothing to mark when the counter
/// did not move.
fn mark_errored(table: &mut SyncobjTable, handle: u32, from: u64, to: u64) {
    if to > from {
        table.errored.push(Errored { handle, from, to });
    }
}

/// The points of `handle` in `(from, to]` were reached by a fence the GPU
/// never wrote: [`resolve_hw_locked`] gave up on it after
/// [`FENCE_TIMEOUT_US`], and a link that materialised on such a point
/// carries the mark on (a `dma_fence_array` takes its members' error).
///
/// A timed-out fence still advances the counter -- `SYNCOBJ_QUERY` and
/// `SYNCOBJ_WAIT` read it as reached, as Linux reads a fence signaled with
/// an error -- but the driver's EXEC asks [`reached_by_timeout`] before it
/// submits behind such a wait, so its answer is EIO whichever of the two
/// 10 s clocks (this timeout, EXEC's own deadline) ticked first. Cleared by
/// whatever replaces the fence: a signal from the CPU at or past `to`, a
/// binary re-arm, a reset, the object going away.
#[derive(Clone, Copy)]
struct Errored {
    handle: u32,
    from: u64,
    to: u64,
}

struct SyncobjTable {
    objects: Vec<Syncobj>,
    pending: Vec<PendingFence>,
    errored: Vec<Errored>,
}

static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);

lazy_static::lazy_static! {
    static ref TABLE: Mutex<SyncobjTable> = Mutex::new(SyncobjTable {
        objects: Vec::new(),
        pending: Vec::new(),
        errored: Vec::new(),
    });
}

/// Number of pending hardware fences, mirrored outside the lock so the
/// eventfd poller (and [`poll_pending`]'s fast exit) can check it for free.
static PENDING_COUNT: AtomicUsize = AtomicUsize::new(0);

/// A pending fence older than this is a hung ring. Same bound the driver's
/// old synchronous poll used, so the behaviour on a GPU hang is unchanged:
/// the context is latched wedged (via the timeout hook) and the waiter is
/// released instead of parking forever.
pub const FENCE_TIMEOUT_US: u64 = 10_000_000;

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

/// How many times a fence landing zone has been read ([`fence_landed`]).
///
/// The unit this counts is not free: the landing zone is pinned sysmem the
/// GPU writes, mapped uncached, so every read is a round trip off the CPU's
/// caches -- which is why "how often does a frame walk the pending list"
/// is worth measuring at all. Test-only: an atomic on this path would make
/// the measurement change the thing measured.
#[cfg(test)]
pub static FENCE_READS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Whether the GPU has written `payload` (or a later one) into `fence_va`.
#[inline]
fn fence_landed(fence_va: usize, payload: u32) -> bool {
    #[cfg(test)]
    FENCE_READS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `fence_va` is a kernel mapping of pinned sysmem published by
    // the driver for exactly this read (`attach_hw_fence`'s contract).
    let v = unsafe { core::ptr::read_volatile(fence_va as *const u32) };
    (v.wrapping_sub(payload) as i32) >= 0
}

/// How many `spin_loop()` turns [`wait`] burns before its next look at the
/// table, given how many looks it has already taken.
///
/// [`wait`] is the CPU-side wait the GPU driver's `EXEC` falls back to for
/// every fence it cannot hand the hardware as an ACQUIRE -- a cross-process
/// one, which on a desktop means the compositor's release of the client's
/// buffer. It used to re-take [`TABLE`] on EVERY turn of its spin, for up to
/// the whole ten-second bound, with nothing between the turns but one
/// `spin_loop()`.
///
/// That is not merely wasted work: [`TABLE`] is the lock the SIGNALLING side
/// needs too. A waiter that holds it essentially all the time is a waiter
/// starving the very thread that would end its wait, and on a two-CPU
/// desktop that thread is the compositor. The fix is to look less often, not
/// to wait longer: a few hundred spin turns between looks is tens of
/// nanoseconds of extra notice latency and hands the lock back for all of it.
///
/// The first looks have no backoff at all, because the overwhelmingly common
/// case is a fence that is already signalled when the wait begins; the growth
/// after that is what a contended wait pays.
pub fn wait_spin_backoff(probes: u32) -> u32 {
    if probes < WAIT_EAGER_PROBES {
        return 0;
    }
    let doublings = probes - WAIT_EAGER_PROBES;
    (WAIT_SPIN_FLOOR << doublings.min(16)).min(WAIT_SPIN_CAP)
}

/// Looks at the table [`wait`] takes back to back before it starts backing
/// off. A fence already signalled is seen on the first one.
pub const WAIT_EAGER_PROBES: u32 = 2;

/// The first backoff, in `spin_loop()` turns.
pub const WAIT_SPIN_FLOOR: u32 = 64;

/// The longest gap between two looks at the table, in `spin_loop()` turns.
/// Bounded so a wait still notices its fence promptly: this is a pause
/// measured in nanoseconds, not a sleep.
pub const WAIT_SPIN_CAP: u32 = 4_096;

/// What a fence poll should do before its next probe.
///
/// The async fence waits in `linux-object` (`SYNCOBJ_WAIT`, the atomic
/// in-fence, the legacy pre-present fence) cannot be woken by an interrupt:
/// there is no `dma_fence` here, so "the fence landed" is only ever observed
/// by reading it (see this module's docs). What they *can* choose is how soon
/// after submitting they look again, and that choice is the whole latency of
/// a frame that is already done.
///
/// They all used one fixed 1 ms tick. A glxgears frame on a Turing card is
/// well under 200 us of GPU work, so nearly every wait on this path used to
/// sleep out a whole millisecond past the moment its fence had landed, and
/// several such waits stack up in one frame (the client's acquire, the
/// compositor's, the pre-present one). That is a frame rate set by the poll
/// tick rather than by the GPU.
///
/// So: a few yields first, then a sleep that starts at the timer's own floor
/// and backs off to the old tick.
///
/// * The yields cost no timer at all and re-probe as soon as the runtime
///   comes back round, which is the right answer for the fence that lands in
///   tens of microseconds -- the common case, and the one the fixed tick was
///   worst at.
/// * [`FENCE_POLL_FLOOR_US`] is `MIN_ARM_NS` in `kernel-hal`'s timer
///   (200 us): asking to sleep less than that does not wake any sooner, it
///   just reprograms the CPU timer for nothing.
/// * The backoff caps at [`FENCE_POLL_CAP_US`], the tick this replaces, so a
///   fence that is genuinely late (a hung ring waiting out its 100 ms or 10 s
///   bound) costs no more wakeups than it did before -- roughly a hundred for
///   a 100 ms timeout either way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PollStep {
    /// Re-probe as soon as the runtime comes back: no timer is armed.
    Yield,
    /// Sleep this many microseconds (capped by the caller's deadline) and
    /// probe again.
    Sleep { us: u64 },
}

/// Probes made by yielding before the first sleep.
pub const FENCE_POLL_YIELDS: u32 = 4;

/// The shortest sleep worth asking for: `kernel-hal`'s `MIN_ARM_NS`, below
/// which a CPU timer cannot be armed anyway.
pub const FENCE_POLL_FLOOR_US: u64 = 200;

/// The longest gap between probes: the fixed tick this backoff replaces.
pub const FENCE_POLL_CAP_US: u64 = 1_000;

/// What the `probes`-th probe of a fence poll should do before the next one
/// (`probes` counts the probes already made, so the first call passes 0).
pub fn fence_poll_step(probes: u32) -> PollStep {
    if probes < FENCE_POLL_YIELDS {
        return PollStep::Yield;
    }
    let doublings = probes - FENCE_POLL_YIELDS;
    // 200, 400, 800, then the 1000 us cap. The shift count is clamped first:
    // a wait parked for its whole timeout reaches large `probes`, and a
    // shift that runs off the top of the `u64` silently produces ZERO --
    // which would ask for a sleep of no time at all and spin the poll into
    // an unbounded timer storm, the opposite of what this exists for.
    let us = (FENCE_POLL_FLOOR_US << doublings.min(16)).min(FENCE_POLL_CAP_US);
    PollStep::Sleep { us }
}

// --- Counters for /proc/gpudbg -----------------------------------------------
static WAIT_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WAIT_SPIN_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WAIT_MAX_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WAIT_TIMEOUTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Times [`wait`] has taken [`TABLE`] to look at its handles. Against
/// `WAIT_CALLS` it says how hard the CPU-side waits are leaning on the lock
/// the signalling side needs; that ratio is what [`wait_spin_backoff`] is
/// there to keep down.
static WAIT_TABLE_PROBES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// `spin_loop()` turns [`wait`] has burned between those looks -- the time
/// it is NOT holding [`TABLE`]. Only the tests read it: a spin turn costs no
/// time on the test clock, so this is the only way to see from a test that
/// the loop really is backing off.
#[cfg(test)]
static WAIT_SPIN_TURNS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
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
        "syncobj waits={} (table looks={}) cpu-spin={}us (avg {}us, max {}us, timeouts={}) | hw fences landed={} (submit->land avg {}us, max {}us) timed-out={} pending-now={}",
        calls,
        WAIT_TABLE_PROBES.load(Ordering::Relaxed),
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
    /// Fold a second resolve's upcalls into this one, so a function that
    /// resolves twice under one lock still runs everything once unlocked.
    fn extend(&mut self, other: Deferred) {
        self.notify.extend(other.notify);
        self.timed_out.extend(other.timed_out);
    }

    /// Must be called with the [`TABLE`] lock RELEASED (both upcalls re-enter
    /// this module), and must be called on EVERY path out of the block that
    /// built it: [`resolve_locked`] has already taken those fences out of the
    /// table, so a `Deferred` that is dropped instead of run is a lost
    /// wakeup and a lost hang report, with nothing left to retry them.
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
    // What each landing zone said the FIRST time this call looked at it, so
    // the passes below do not ask the same question of the same address
    // again.
    //
    // The fixed point costs a full walk of the pending list per pass, and a
    // walk reads every fence's landing zone -- pinned sysmem the GPU writes,
    // mapped uncached, so each read leaves the CPU. What the later passes are
    // actually re-deciding is not whether a fence LANDED (only the GPU
    // changes that, and it is free to do so a microsecond after this call
    // returns, which is why the next probe exists) but whether a fence that
    // had landed is still HELD behind a node below it. Reading the zone again
    // to answer the second question is the waste: a client parked on its
    // acquire semaphore takes several looks a frame, and each look pays for
    // every pass.
    let mut seen: Vec<(usize, u32, bool)> = Vec::new();
    // To a fixed point: a link resolving can release a landed fence held
    // behind it, and a fence landing can satisfy a link.
    loop {
        let hw = resolve_hw_locked(table, &mut out, &mut seen);
        let links = resolve_links_locked(table, &mut out);
        if !(hw || links) {
            break;
        }
    }
    out
}

/// [`fence_landed`], answered from `seen` when this `resolve_locked` call has
/// already read that landing zone.
///
/// Keyed by the pair the read is made of, because the pending list is walked
/// with `swap_remove` and an index does not name the same fence twice.
fn fence_landed_cached(seen: &mut Vec<(usize, u32, bool)>, fence_va: usize, payload: u32) -> bool {
    if let Some(&(_, _, landed)) = seen
        .iter()
        .find(|&&(va, p, _)| va == fence_va && p == payload)
    {
        return landed;
    }
    let landed = fence_landed(fence_va, payload);
    seen.push((fence_va, payload, landed));
    landed
}

/// The hardware half of [`resolve_locked`]: take every landed (or timed-out)
/// fence out of the table and advance its syncobj.
fn resolve_hw_locked(
    table: &mut SyncobjTable,
    out: &mut Deferred,
    seen: &mut Vec<(usize, u32, bool)>,
) -> bool {
    if table.pending.is_empty() {
        return false;
    }
    let now = now_us();
    let mut any = false;
    let mut i = 0;
    while i < table.pending.len() {
        let f = table.pending[i];
        let landed = fence_landed_cached(seen, f.fence_va, f.payload);
        let timed_out = !landed && now.wrapping_sub(f.submitted_us) >= FENCE_TIMEOUT_US;
        if !(landed || timed_out) {
            i += 1;
            continue;
        }
        // A node of the chain still waiting below this point -- a fence of
        // the same handle in flight (two channels signaling one timeline
        // out of order) or a link -- holds it: the fence stays in the table
        // as landed, and advances the counter once they have resolved, as
        // `dma_fence_chain_signaled` walks every node before it. (A
        // timed-out fence held this way waits for them too.)
        let held = table
            .objects
            .iter()
            .find(|o| o.handle == f.handle)
            .is_some_and(|o| f.point > o.point && node_below_waiting(table, o, f.point));
        if held {
            i += 1;
            continue;
        }
        any = true;
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
            let before = obj.point;
            if f.point > obj.point {
                obj.point = f.point;
            }
            let after = obj.point;
            out.notify.push((f.handle, after));
            if timed_out {
                mark_errored(table, f.handle, before, after);
            }
        }
        if timed_out {
            out.timed_out.push(f);
        }
    }
    PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
    any
}

/// The software half of [`resolve_locked`]: a link whose sources have all
/// reached their targets is MATERIALISED -- the destination's own counter
/// takes the point and the link is dropped -- rather than left to be
/// re-derived from the sources on every read.
///
/// A `sync_file` holds the `dma_fence` that was current when it was
/// exported, and a signaled `dma_fence` never un-signals. A link, by
/// contrast, names its source BY HANDLE and reads the source's *current*
/// point, and a binary source is a fence slot the producer's next submit
/// rewinds to 0 ([`attach_hw_fence`]) and `SYNCOBJ_RESET` clears. Left
/// live, an importer that had read "signaled" read "unsignaled" again the
/// moment its source was re-armed for the next frame -- the acquire
/// semaphore of a swapchain image, whose merged sync_file names the
/// compositor's release syncobj and the previous present, went back to
/// waiting for a release that had already happened, one frame behind, and
/// a frame that waited on itself never came. Materialising at the first
/// resolve after the sources are reached gives the link the fence's
/// semantics: a point once reached is kept, whatever the source does next.
///
/// Runs to a fixed point so a chain (an import of a transfer of an import)
/// collapses in one call, and collects the orphans it stops naming.
fn resolve_links_locked(table: &mut SyncobjTable, out: &mut Deferred) -> bool {
    let mut any = false;
    // The lowest link of an object is the only one that can resolve: a
    // higher one waits for it however ready its own sources are.
    let satisfied = |table: &SyncobjTable| {
        table.objects.iter().position(|o| {
            o.links
                .first()
                .is_some_and(|l| link_ready(table, o, l, LINK_DEPTH))
        })
    };
    while let Some(pos) = satisfied(table) {
        let obj = &mut table.objects[pos];
        let link = obj.links.remove(0);
        let before = obj.point;
        obj.point = obj.point.max(link.dst_point.max(1));
        let (handle, after) = (obj.handle, obj.point);
        out.notify.push((handle, after));
        // A member that was given up on taints the array, as the error of
        // one `dma_fence` taints the `dma_fence_array` it sits in.
        if link
            .deps
            .iter()
            .any(|&(src, t)| errored_locked(table, src, t))
        {
            mark_errored(table, handle, before, after);
        }
        any = true;
    }
    if any {
        collect_orphans(table);
    }
    any
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
        // Resolve what has already landed BEFORE this signal replaces it: a
        // binary re-arm discards the slot's previous fence as superseded,
        // and if that fence had landed with nobody looking (no poll between
        // the GPU's write and this EXEC) its importers still hold it --
        // discarding it unresolved took away the only thing that could ever
        // signal them. Resolving first advances the slot to the landed point,
        // materialises those links, and only then is the slot rewound.
        let mut deferred = resolve_locked(&mut table);
        let Some(obj) = table.objects.iter_mut().find(|o| o.handle == handle) else {
            drop(table);
            deferred.run();
            return false;
        };
        let cur = obj.point;
        // A binary re-arm replaces the slot, links included; a timeline
        // point is a new node of the chain and leaves the links below it
        // waiting (its landing queues behind them, see `resolve_hw_locked`).
        let dropped_link = binary && cur <= 1 && !obj.links.is_empty();
        if binary && cur <= 1 {
            obj.links.clear();
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
            drop_binary_fence(&mut table, handle);
        } else if point <= cur {
            // Already past that point: nothing to wait for.
            //
            // This also catches a *binary* signal on a handle userspace has
            // already driven past 1 as a timeline, which is not a slot in
            // binary range any more. Rewinding it would un-signal a genuine
            // timeline, and purging its pending fences would strand a waiter
            // on a point with nothing left to land and signal it.
            drop(table);
            deferred.run();
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
        if dropped_link {
            collect_orphans(&mut table);
        }
        deferred.extend(resolve_locked(&mut table));
        deferred
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

/// How many objects the table holds, live and orphaned alike.
///
/// Mesa creates and destroys a surrogate syncobj on EVERY frame (the merge of
/// the previous present and the release, imported into the acquire
/// semaphore), and a destroyed surrogate that other links still name stays in
/// the table as an orphan until nothing names it. The table is a `Vec` walked
/// linearly by handle, so an orphan that is never collected is not a leak
/// that shows up as memory -- it is a frame-by-frame slowdown of every
/// syncobj call in the system. Test-only, so a test can pin that it does not
/// happen.
#[cfg(test)]
pub fn table_len() -> usize {
    TABLE.lock().objects.len()
}

/// Creates a syncobj, initially at point 0 (or 1 if `signaled`). Returns the
/// new handle.
pub fn create(signaled: bool) -> u32 {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    TABLE.lock().objects.push(Syncobj {
        handle,
        point: if signaled { 1 } else { 0 },
        refs: 1,
        holders: alloc::vec::Vec::new(),
        links: Vec::new(),
    });
    handle
}

/// Adds one reference to `handle` (an fd export / `dup`). Returns `false` if
/// the handle is unknown.
pub fn add_ref(handle: u32) -> bool {
    let mut table = TABLE.lock();
    let Some(obj) = table
        .objects
        .iter_mut()
        .find(|o| o.handle == handle && o.refs > 0)
    else {
        return false;
    };
    obj.refs = obj.refs.saturating_add(1);
    true
}

/// Drops one reference to `handle`. If other refs remain the object stays;
/// only the last reference removes it (and its pending fences). Returns
/// `false` if `handle` is unknown.
pub fn destroy(handle: u32) -> bool {
    let notify = {
        let mut table = TABLE.lock();
        match destroy_locked(&mut table, handle) {
            Some(notify) => notify,
            None => return false,
        }
    };
    // Lock released: the hook re-enters this module to re-check waiters.
    for (h, p) in notify {
        notify_signal(h, p);
    }
    true
}

/// [`destroy`] with the table already held: `None` for an unknown handle,
/// otherwise the `(handle, point)` signals to deliver once the lock is
/// released.
fn destroy_locked(table: &mut SyncobjTable, handle: u32) -> Option<Vec<(u32, u64)>> {
    let pos = table
        .objects
        .iter()
        .position(|o| o.handle == handle && o.refs > 0)?;
    if table.objects[pos].refs > 1 {
        table.objects[pos].refs -= 1;
        return Some(Vec::new());
    }
    // A fence outlives the syncobj it was taken from. In Linux a
    // dependent holds the `dma_fence` itself, so destroying the source
    // changes nothing for it; here a link names its source BY HANDLE, so
    // an object that other links still depend on, and that can still make
    // progress (a hardware fence in flight, or a link of its own), stays
    // in the table as an ORPHAN: no reference, invisible to `add_ref` and
    // to a second `destroy`, but resolved like any other object until
    // nothing names it ([`collect_orphans`]). Mesa creates exactly this
    // every frame under X11: a surrogate syncobj receives a transfer of a
    // point still in flight, is exported as a sync_file, and is destroyed
    // at once -- releasing its dependents instead (what this function did
    // before) signaled the acquire semaphore of a swapchain image before
    // the compositor had let go of it.
    let can_progress =
        table.pending.iter().any(|f| f.handle == handle) || !table.objects[pos].links.is_empty();
    if can_progress && is_link_source(&table.objects, handle) {
        table.objects[pos].refs = 0;
        return Some(Vec::new());
    }
    table.objects.swap_remove(pos);
    // EVERY fence still in flight on it, timeline points included. The
    // old filter kept anything above binary range, and those entries
    // outlived the object they belonged to: nothing could advance (the
    // resolver looks the handle up and finds nothing), nothing could wait
    // on them, and [`FENCE_TIMEOUT_US`] later they reached the timeout
    // hook, which latches their GPU context WEDGED -- so the client's
    // next submit failed with EIO/device-lost. Closing a syncobj with a
    // submit in flight is not an error, it is what
    // `drm_syncobj_release`/process teardown does to every handle a
    // client owns, so a client that simply exited mid-frame took the
    // context down with it. Linux has nothing to leak here: the syncobj
    // drops its `dma_fence` reference and the fence is just a refcounted
    // object with no back-pointer to it.
    table.pending.retain(|f| f.handle != handle);
    table.errored.retain(|e| e.handle != handle);
    PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
    // A still-deferred import/transfer names its source BY HANDLE (a
    // real one holds the `dma_fence` itself, which outlives the syncobj
    // it came from). With the source gone the link can never resolve, so
    // the destination parks at its old point forever -- and NVK's waits
    // carry an INT64_MAX deadline, so forever is literal. Release them
    // to the point the transfer promised, the same call
    // [`abandon_fences`] makes for a fence that can no longer land: a
    // waiter on a dead producer moves on rather than freezing.
    for link in table.objects.iter_mut().flat_map(|o| o.links.iter_mut()) {
        // A merged fence keeps waiting for its other sources.
        link.deps.retain(|&(src, _)| src != handle);
    }
    // A link left with no source resolves like any other: in chain order,
    // behind the links below it.
    let mut out = Deferred::default();
    resolve_links_locked(table, &mut out);
    collect_orphans(table);
    Some(out.notify)
}

/// `SYNCOBJ_CREATE` from process `pid`: [`create`], with the reference
/// recorded as `pid`'s so that [`release_owner`] can drop it when the
/// process dies. A `pid` of 0 (no process: the kernel, or a test) records
/// nothing, as [`create`] does.
pub fn create_for(pid: u64, signaled: bool) -> u32 {
    let handle = create(signaled);
    if pid != 0 {
        let mut table = TABLE.lock();
        if let Some(obj) = table.objects.iter_mut().find(|o| o.handle == handle) {
            obj.holders.push(pid);
        }
    }
    handle
}

/// An opaque `SYNCOBJ_FD_TO_HANDLE` by process `pid`: [`add_ref`], with the
/// new reference recorded as `pid`'s. A `pid` of 0 records nothing.
pub fn add_ref_for(pid: u64, handle: u32) -> bool {
    let mut table = TABLE.lock();
    let Some(obj) = table
        .objects
        .iter_mut()
        .find(|o| o.handle == handle && o.refs > 0)
    else {
        return false;
    };
    obj.refs = obj.refs.saturating_add(1);
    if pid != 0 {
        obj.holders.push(pid);
    }
    true
}

/// `SYNCOBJ_DESTROY` by process `pid`: gives back one of the references
/// `pid` holds on `handle`. `false` (ENOENT) when it holds none, even if the
/// object exists: a handle is another process's to destroy only in that
/// process's table (Linux), and a client that guesses the compositor's
/// handle numbers must not be able to free its timelines. A `pid` of 0
/// destroys as the kernel would ([`destroy`]).
pub fn destroy_for(pid: u64, handle: u32) -> bool {
    if pid == 0 {
        return destroy(handle);
    }
    let notify = {
        let mut table = TABLE.lock();
        let Some(obj) = table
            .objects
            .iter_mut()
            .find(|o| o.handle == handle && o.refs > 0)
        else {
            return false;
        };
        let Some(i) = obj.holders.iter().position(|&p| p == pid) else {
            return false;
        };
        obj.holders.swap_remove(i);
        // One take of the lock: the reference goes with its holder entry.
        destroy_locked(&mut table, handle).unwrap_or_default()
    };
    for (h, p) in notify {
        notify_signal(h, p);
    }
    true
}

/// Process `pid` is gone: give back every reference it still held
/// (`drm_syncobj_release` on the file's close in Linux). An object another
/// process also holds, or a sync_file carries, stays; one nobody else holds
/// goes the way of [`destroy`] (orphaned while a link still needs it, freed
/// otherwise). Returns how many references were given back. Without this a
/// crashed or killed client's syncobjs stayed in the table for the rest of
/// the boot, and every lookup walks that table.
pub fn release_owner(pid: u64) -> usize {
    if pid == 0 {
        return 0;
    }
    let (given_back, notify) = {
        let mut table = TABLE.lock();
        let mut handles = alloc::vec::Vec::new();
        for obj in table.objects.iter_mut() {
            let before = obj.holders.len();
            obj.holders.retain(|&p| p != pid);
            for _ in obj.holders.len()..before {
                handles.push(obj.handle);
            }
        }
        // One take of the lock for the whole sweep: no DESTROY from another
        // process slips in between the holder entry going and the reference.
        let mut notify = Vec::new();
        for &h in &handles {
            notify.extend(destroy_locked(&mut table, h).unwrap_or_default());
        }
        (handles.len(), notify)
    };
    for (h, p) in notify {
        notify_signal(h, p);
    }
    given_back
}

/// Whether process `pid` holds a reference on `handle`.
pub fn held_by(pid: u64, handle: u32) -> bool {
    TABLE
        .lock()
        .objects
        .iter()
        .any(|o| o.handle == handle && o.refs > 0 && o.holders.contains(&pid))
}

/// Whether process `pid` may name `handle` in an ioctl at all. Linux keeps
/// syncobj handles per `drm_file`, so a handle some other file created is
/// simply not there (`drm_syncobj_find` -> ENOENT) for a SIGNAL, a RESET,
/// a TRANSFER, a QUERY, a WAIT or an EXEC as much as for a DESTROY. Here
/// the handle space is global and consecutive, so the holders recorded per
/// reference stand in for the file: a handle with holders is usable by
/// them alone; a live handle with NO holder (created by the kernel, a
/// sync_file carrier, a merge, a test) belongs to no file and is usable by
/// anyone; and pid 0 (no current thread: the kernel itself) may name any
/// live handle. Before this, `destroy` was the only ioctl that asked, and
/// a client guessing the compositor's handle numbers could signal its
/// release timelines early, reset its acquire fences, or wait on them.
pub fn usable_by(pid: u64, handle: u32) -> bool {
    all_usable_by(pid, &[handle])
}

/// [`usable_by`] for a whole handle array, in ONE take of the lock: the
/// array lookup Linux does first (`drm_syncobj_array_find`), so an ioctl
/// over `[good, bad]` touches neither.
pub fn all_usable_by(pid: u64, handles: &[u32]) -> bool {
    let table = TABLE.lock();
    handles.iter().all(|&h| {
        table.objects.iter().any(|o| {
            o.handle == h
                && o.refs > 0
                && (pid == 0 || o.holders.is_empty() || o.holders.contains(&pid))
        })
    })
}

/// The unresolved HW fences that will deliver at least `point` on `handle`,
/// each as `(fence_va_cpu, fence_gpu_va, payload, ctx_idx)`. Used by EXEC to
/// emit a GPU ACQUIRE per fence instead of spinning on the CPU. `fence_gpu_va`
/// is 0 when the producer did not publish one. The handle's own fence comes
/// alone; an importer, a deferred transfer or a merge has no fence of its
/// own, and the ones behind its link are returned, one per source still in
/// flight ([`fences_through_link`]). Empty means nothing to acquire: either
/// nothing is owed, or a source has nothing submitted yet.
pub fn pending_hw_fences(handle: u32, point: u64) -> alloc::vec::Vec<(usize, u64, u32, u32)> {
    let (r, deferred) = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        // A point the handle has reached (its counter, or a link whose
        // sources all have) has no fence left to deliver it: the wait is
        // over, whatever later fence is still in flight on the handle. Read
        // off the fences alone, a timeline wait on a point already
        // delivered took the NEXT fence pending, and a wait on point 1 the
        // highest, so a consumer that waited for frame N stalled behind
        // frame N+1, in front of pushes nobody asked to hold back. Linux
        // drops a dependency on a signaled fence before the job runs.
        let reached =
            effective_point(&table, handle, LINK_DEPTH).is_some_and(|p| p >= point.max(1));
        // Binary waits (point 0/1): the highest pending fence on this handle.
        // Timeline: every fence and link up to the lowest one that covers
        // `point`, the chain (`fences_through_link`).
        let own = if reached || point > 1 {
            None
        } else {
            table
                .pending
                .iter()
                .filter(|f| f.handle == handle)
                .max_by_key(|f| f.point)
                .copied()
        };
        let found = match own {
            Some(f) => alloc::vec![f],
            None if reached => alloc::vec::Vec::new(),
            None => fences_through_link(&table, handle, point.max(1), LINK_DEPTH),
        };
        (
            found
                .into_iter()
                .map(|f| (f.fence_va, f.fence_gpu_va, f.payload, f.ctx_idx))
                .collect(),
            d,
        )
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
        // A direct signal replaces whatever fence the object carried at or
        // below the point, imported sync_file included -- same as real
        // drm_syncobj. A link ABOVE it is a chain node still waiting for
        // its own sources and stays: dropping it stranded a transfer to a
        // higher point at the counter this signal set.
        let links_before = obj.links.len();
        obj.links.retain(|l| l.dst_point.max(1) > point);
        let dropped_link = obj.links.len() != links_before;
        let p = obj.point;
        // Pending hardware fences at or below the new point are moot, and so
        // is a fence that was given up on there: the CPU's word replaces it.
        table
            .pending
            .retain(|f| !(f.handle == handle && f.point <= p));
        table
            .errored
            .retain(|e| !(e.handle == handle && e.to <= point));
        PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
        if dropped_link {
            collect_orphans(&mut table);
        }
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
        let cur = effective_point(&table, handle, LINK_DEPTH);
        let in_flight = table
            .pending
            .iter()
            .filter(|f| f.handle == handle)
            .map(|f| f.point)
            .max()
            .unwrap_or(0);
        let promised = promised_point(&table, handle);
        (cur.map(|p| p.max(in_flight).max(promised).max(1)), d)
    };
    deferred.run();
    r
}

/// `SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE`: the fence `handle` carries
/// right now, as a syncobj of its own for the `sync_file` to hold. A Linux
/// `sync_file` keeps its own reference to the `dma_fence` that was attached
/// at export time: a RESET of the syncobj (Mesa resets a binary semaphore in
/// the same call that exports its `SYNC_FD`, copy transference) or the
/// re-arm of the next submit replaces the syncobj's fence and leaves the
/// file's alone. A file that read `(handle, point)` live against the source
/// lost the fence with the reset -- `reset` drops the pending fence and
/// zeroes the point -- and waited for the source's NEXT signal, or for ever.
///
/// The carrier starts with one reference, the file's; `destroy` it when the
/// file closes. Its point 1 is the fence: reached already (a timed-out one
/// keeps its mark), the same hardware fence when one is in flight, or the
/// source's next signal when it has none (Linux refuses that export;
/// [`export_snapshot`] explains why it is accepted here). `None` for an
/// unknown handle.
pub fn export_fence(handle: u32) -> Option<u32> {
    let point = export_snapshot(handle)?;
    let carrier = create(false);
    if !transfer(carrier, 1, handle, point) {
        destroy(carrier);
        return None;
    }
    Some(carrier)
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
        let Some(src_point) = effective_point(&table, src, LINK_DEPTH) else {
            drop(table);
            d.run();
            return false;
        };
        let reached = src_point >= target;
        let tainted = reached && errored_locked(&table, src, target);
        // The import REPLACES the destination's fence: one still in flight
        // from an EXEC that signaled it stayed in `pending` beside the new
        // link, and when it landed it advanced the object -- a waiter went
        // through before the source the import stood for had been reached,
        // and EXEC acquired that stale fence instead of the source's.
        drop_binary_fence(&mut table, dst);
        let Some(obj) = table.objects.iter_mut().find(|o| o.handle == dst) else {
            drop(table);
            d.run();
            return false;
        };
        let had_link = !obj.links.is_empty();
        obj.links.clear();
        let adv = if reached {
            let before = obj.point;
            obj.point = obj.point.max(1);
            let after = obj.point;
            if tainted {
                mark_errored(&mut table, dst, before, after);
            }
            Some(after)
        } else {
            // A binary import: `dst_point` is 1, because `IMPORT_SYNC_FILE`
            // really does replace the binary fence -- a signaled slot goes
            // back to waiting, for this source (a `VkSemaphore` imported
            // into again: signal, wait, import, wait).
            if obj.point <= 1 {
                obj.point = 0;
            }
            obj.links.push(Link {
                deps: alloc::vec![(src, target)],
                dst_point: 1,
            });
            None
        };
        if had_link {
            collect_orphans(&mut table);
        }
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
/// `dst_point` of 0 selects the destination's binary fence; a `src_point`
/// of 0 is the fence the source carries NOW -- its highest point reached,
/// in flight or promised, floored at 1 for an unsignaled binary as
/// [`export_snapshot`] floors it -- because `drm_syncobj_find_fence` at
/// point 0 hands back the whole `dma_fence_chain`, and waiting on it waits
/// on everything.
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
        let Some(src_eff) = effective_point(&table, src, LINK_DEPTH) else {
            drop(table);
            d.run();
            return false;
        };
        let need = if src_point == 0 {
            // Read as point 1, this handed over a point long reached while
            // the source still had work pending: Mesa's many-to-many
            // transfer moves its temporary timeline at point 0 into every
            // signal semaphore, and they signaled behind one wait of N.
            let in_flight = table
                .pending
                .iter()
                .filter(|f| f.handle == src)
                .map(|f| f.point)
                .max()
                .unwrap_or(0);
            src_eff
                .max(in_flight)
                .max(promised_point(&table, src))
                .max(1)
        } else {
            src_point
        };
        let reached = src_eff >= need;
        let tainted = reached && errored_locked(&table, src, need);
        // The lowest pending hardware fence on `src` that covers `need` --
        // copied only when it is the WHOLE of what `need` waits for. With
        // another fence or a link below it on the source, the dependency
        // is the chain, and a link to `src` follows every node of it
        // (`fences_through_link`); the lone copy followed one.
        let hw = table
            .pending
            .iter()
            .filter(|f| f.handle == src && f.point >= need)
            .min_by_key(|f| f.point)
            .copied()
            .filter(|f| {
                !table
                    .objects
                    .iter()
                    .find(|o| o.handle == src)
                    .is_some_and(|o| node_below_waiting(&table, o, f.point))
            });
        let binary = dst_point <= 1;
        if binary {
            // A binary transfer replaces the destination's fence, as an
            // import does (above); a timeline point is added to the chain.
            drop_binary_fence(&mut table, dst);
        }
        let point = dst_point.max(1);
        // A timeline point already reached on the source still queues
        // behind the nodes the destination waits on below it.
        let behind_a_node = !binary
            && table
                .objects
                .iter()
                .find(|o| o.handle == dst)
                .is_some_and(|o| node_below_waiting(&table, o, point));
        let Some(obj) = table.objects.iter_mut().find(|o| o.handle == dst) else {
            drop(table);
            d.run();
            return false;
        };
        // The binary replace takes the links with it; a timeline point is a
        // new node behind whatever the destination still waits on
        // (`drm_syncobj_add_point`), which used to be thrown away here.
        let had_link = binary && !obj.links.is_empty();
        if binary {
            obj.links.clear();
        }
        let np = if reached {
            let before = obj.point;
            if behind_a_node {
                obj.push_link(Link {
                    deps: Vec::new(),
                    dst_point: point,
                });
                if tainted {
                    mark_errored(&mut table, dst, before, point);
                }
                None
            } else {
                obj.point = obj.point.max(point);
                let after = obj.point;
                if tainted {
                    mark_errored(&mut table, dst, before, after);
                }
                Some(after)
            }
        } else if let Some(f) = hw {
            if point == 1 && obj.point == 1 {
                // The binary slot is replaced: signaled no more.
                obj.point = 0;
            }
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
            if binary && obj.point == 1 {
                // The binary slot is replaced: signaled no more.
                obj.point = 0;
            }
            obj.push_link(Link {
                deps: alloc::vec![(src, need)],
                dst_point: point,
            });
            None
        };
        if had_link {
            collect_orphans(&mut table);
        }
        (np, d)
    };
    // Lock released: a satisfied transfer advanced `dst`, so wake its waiters.
    if let Some(p) = new_point {
        notify_signal(dst, p);
    }
    deferred.run();
    true
}

/// `SYNC_IOC_MERGE`: a new syncobj that reaches 1 once EVERY `(handle,
/// point)` of `fences` has been reached -- the `dma_fence_array` behind a
/// merged `sync_file`. The caller owns the one reference (the merged fd).
///
/// Mesa's window-system code issues this on every acquire of a swapchain
/// image that has been presented before: the acquire semaphore is "the
/// compositor released the image" AND "its previous present completed",
/// each exported as a sync_file and merged. Without the ioctl the acquire
/// fails, and zink kills the GLX swapchain after the first `image_count`
/// frames -- a window that opens and closes in under a second.
pub fn merge_fences(fences: &[(u32, u64)]) -> u32 {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let deferred = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        // Sources that no longer exist count as reached, the way [`destroy`]
        // releases a dependent whose source is gone for good.
        let deps: Vec<(u32, u64)> = fences
            .iter()
            .copied()
            .filter(|&(src, target)| {
                effective_point(&table, src, LINK_DEPTH).is_some_and(|p| p < target)
            })
            .collect();
        let tainted = deps.is_empty()
            && fences
                .iter()
                .any(|&(src, target)| errored_locked(&table, src, target));
        table.objects.push(Syncobj {
            handle,
            point: if deps.is_empty() { 1 } else { 0 },
            refs: 1,
            holders: alloc::vec::Vec::new(),
            links: if deps.is_empty() {
                Vec::new()
            } else {
                alloc::vec![Link { deps, dst_point: 1 }]
            },
        });
        if tainted {
            mark_errored(&mut table, handle, 0, 1);
        }
        d
    };
    deferred.run();
    handle
}

/// Resets a syncobj to point 0 (`SYNCOBJ_RESET`). Returns `false` if
/// `handle` is unknown. Drops any pending hardware fence it carried (the
/// fence is replaced, as in Linux).
pub fn reset(handle: u32) -> bool {
    let deferred = {
        let mut table = TABLE.lock();
        // As in [`attach_hw_fence`]: a fence that landed unseen is resolved
        // (and the links that hold it materialised) before the reset throws
        // it away. The reset empties THIS object; an importer that already
        // received its fence keeps it.
        let deferred = resolve_locked(&mut table);
        let Some(obj) = table.objects.iter_mut().find(|o| o.handle == handle) else {
            drop(table);
            deferred.run();
            return false;
        };
        obj.point = 0;
        let dropped_link = !obj.links.is_empty();
        obj.links.clear();
        // Every fence, not just the ones inside binary range: `SYNCOBJ_RESET` is
        // `drm_syncobj_replace_fence(syncobj, NULL)` in Linux, i.e. "this object
        // carries no fence at all". A timeline fence left in flight across the
        // reset landed later and drove the counter back up to its point on its
        // own, so a syncobj Mesa had just reset for reuse re-signaled itself
        // behind the application's back -- a premature signal, which reads as
        // corruption nowhere near sync. (This is the opposite case to a binary
        // *signal*, which supersedes only the slot it replaces and must spare a
        // timeline fence in flight; a reset supersedes everything.)
        table.pending.retain(|f| f.handle != handle);
        table.errored.retain(|e| e.handle != handle);
        PENDING_COUNT.store(table.pending.len(), Ordering::Relaxed);
        if dropped_link {
            collect_orphans(&mut table);
        }
        deferred
    };
    deferred.run();
    true
}

/// Whether `handle` names a live syncobj: the lookup EXEC makes of every
/// sig handle before it queues anything (`drm_syncobj_find` in Linux). A
/// plain lookup, on purpose: unlike [`query`] it resolves no landed fence,
/// so refusing a bad list changes nothing about the good handles in it.
pub fn exists(handle: u32) -> bool {
    TABLE.lock().objects.iter().any(|o| o.handle == handle)
}

/// Whether any `(handle, point)` of a wait that [`wait`] reported satisfied
/// got there only because a fence behind it TIMED OUT (see [`Errored`]),
/// through a merge, an import or a transfer as much as on the handle
/// itself. The driver asks this before submitting behind a CPU wait: the
/// release it waited for never happened, and its EXEC contract for that is
/// EIO, not a push behind a buffer nobody let go of. A point of 0 is the
/// binary fence, as everywhere else.
pub fn reached_by_timeout(handles: &[u32], points: Option<&[u64]>) -> bool {
    let (r, deferred) = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        let r = handles.iter().enumerate().any(|(i, &h)| {
            let target = points.map(|p| p[i]).unwrap_or(1).max(1);
            errored_locked(&table, h, target)
        });
        (r, d)
    };
    deferred.run();
    r
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
        let cur = effective_point(&table, handle, LINK_DEPTH);
        let out = if last_submitted {
            cur.map(|p| {
                let inflight = table
                    .pending
                    .iter()
                    .filter(|f| f.handle == handle)
                    .map(|f| f.point)
                    .max()
                    .unwrap_or(0);
                // A link counts once every source has submitted its point.
                let promised = promised_point(&table, handle);
                let promised = if promised > p.max(inflight)
                    && submitted_locked(&table, handle, promised, LINK_DEPTH)
                {
                    promised
                } else {
                    0
                };
                p.max(inflight).max(promised)
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
/// target counts as satisfied (fence submitted, not necessarily signaled),
/// through a link as well ([`submitted_locked`]).
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
/// only guarantee the wait exists to give, whether the fence is the handle's
/// own or every one behind its link ([`fences_through_link`]). Fences on other
/// channels (another process's work) are still waited for on the CPU. The
/// syncobj itself stays pending until the fence really lands.
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
            let Some(point) = effective_point(&table, h, LINK_DEPTH) else {
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
            let pending_covers = available_only && submitted_locked(&table, h, target, LINK_DEPTH);
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
    let mut probes = 0u32;
    loop {
        WAIT_TABLE_PROBES.fetch_add(1, Ordering::Relaxed);
        let mut signaled_count = 0usize;
        let mut first_signaled: Option<u32> = None;
        let deferred = {
            let mut table = TABLE.lock();
            let d = resolve_locked(&mut table);
            for (i, &h) in handles.iter().enumerate() {
                let Some(point) = effective_point(&table, h, LINK_DEPTH) else {
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
                let pending_covers =
                    available_only && submitted_locked(&table, h, target, LINK_DEPTH);
                let ordered = match ordered_ctx {
                    Some(ctx) => {
                        table
                            .pending
                            .iter()
                            .any(|f| f.handle == h && f.ctx_idx == ctx && f.point >= target)
                            || {
                                let behind = fences_through_link(&table, h, target, LINK_DEPTH);
                                !behind.is_empty() && behind.iter().all(|f| f.ctx_idx == ctx)
                            }
                    }
                    None => false,
                };
                if point >= target || ordered || pending_covers {
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
        // Back off before looking again, and hand [`TABLE`] back while we do
        // -- see [`wait_spin_backoff`]. The pump is the same one every other
        // long poll in this tree runs (`gpu_spin`, the RM's `osSpinLoop`):
        // this spin can sit here for ten seconds, and a CPU spinning that
        // long without draining its TLB-shootdown queue starves a peer CPU's
        // shootdown ack and wedges the machine. This was the one long poll
        // that never pumped.
        let turns = wait_spin_backoff(probes);
        probes = probes.saturating_add(1);
        #[cfg(test)]
        WAIT_SPIN_TURNS.fetch_add(turns as u64, Ordering::Relaxed);
        for n in 0..turns {
            if n & 511 == 0 {
                lock::pump();
            }
            core::hint::spin_loop();
        }
        if turns == 0 {
            core::hint::spin_loop();
        }
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
        let cur = effective_point(&table, h, LINK_DEPTH).map_or(-1i64, |p| p as i64);
        let _ = core::fmt::write(&mut list, format_args!(" {:#x}:{}/{}", h, target, cur));
        for f in table.pending.iter().filter(|f| f.handle == h) {
            let _ = core::fmt::write(&mut list, format_args!("+{}(ctx{})", f.point, f.ctx_idx));
        }
    }
    list
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    /// [`pending_hw_fences`] where the caller expects at most one fence: the
    /// handle's own, or the lone source behind its link.
    fn pending_hw_fence(handle: u32, point: u64) -> Option<(usize, u64, u32, u32)> {
        let fences = pending_hw_fences(handle, point);
        assert!(
            fences.len() <= 1,
            "{handle:#x}@{point} has {} fences in flight; ask for the list",
            fences.len()
        );
        fences.first().copied()
    }
    use crate::nvme::nvme_queue::test_clock;
    use core::cell::RefCell;

    /// Every test here drives the one global [`TABLE`] and the two global
    /// hook slots, and the fence clock is per-thread, so a test that winds
    /// its own clock forward would time out another test's pending fences.
    /// Serialise them. (CI runs with `--test-threads=1`, which hides exactly
    /// this class of coupling, so the lock is not optional here.)
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The CPU-side wait hands the table back between its looks.
    ///
    /// `wait` is what `EXEC` falls back to for a fence it cannot hand the
    /// hardware -- on a desktop, the compositor's release of the client's
    /// buffer -- and it used to re-take `TABLE` on every turn of its spin,
    /// for up to ten seconds, with one `spin_loop()` between turns. `TABLE`
    /// is also the lock the SIGNALLING side takes, so that waiter was
    /// starving the thread that would end its own wait.
    ///
    /// What a test here can and cannot see: a `spin_loop()` turn costs
    /// nothing on the test clock, so the number of looks over a fixed span
    /// of virtual microseconds cannot go down here however hard the loop
    /// backs off -- the saving is real time on real hardware, and this
    /// binary has none. What it CAN pin is that the loop really applies the
    /// backoff, per look and in order, and that it stops applying it the
    /// moment there is an answer. Those are the two ways the wiring breaks.
    #[test]
    fn a_cpu_wait_hands_the_table_back_between_its_looks() {
        let _g = test_lock();
        let h = create(false);
        test_clock::set(1_000_000);
        test_clock::set_auto_advance(1);
        const SPAN_US: u64 = 20_000;

        let looks_before = WAIT_TABLE_PROBES.load(Ordering::Relaxed);
        let turns_before = WAIT_SPIN_TURNS.load(Ordering::Relaxed);
        let t0 = now_us();
        assert!(matches!(
            wait(&[h], None, true, t0 + SPAN_US),
            WaitOutcome::Timeout
        ));
        let looks = WAIT_TABLE_PROBES.load(Ordering::Relaxed) - looks_before;
        let turns = WAIT_SPIN_TURNS.load(Ordering::Relaxed) - turns_before;

        assert!(
            looks > 2,
            "the wait did not actually loop ({} looks)",
            looks
        );
        // Every look past the eager ones paused, and long enough to have
        // reached the cap: that pause is the lock handed back.
        assert!(
            turns >= (looks - WAIT_EAGER_PROBES as u64) * WAIT_SPIN_CAP as u64 / 2,
            "{} looks burned only {} spin turns between them",
            looks,
            turns
        );
        // And the wait still ends where it was told to, not later: the
        // backoff is a pause between looks, never an extension of the bound.
        assert!(
            now_us() >= t0 + SPAN_US && now_us() < t0 + SPAN_US * 2,
            "the wait ran to {} for a deadline of {}",
            now_us(),
            t0 + SPAN_US
        );

        // A syncobj already signalled is still seen on the first look and
        // pauses for nothing: the backoff must not cost the common case.
        assert!(signal(h));
        let looks_before = WAIT_TABLE_PROBES.load(Ordering::Relaxed);
        let turns_before = WAIT_SPIN_TURNS.load(Ordering::Relaxed);
        assert!(matches!(
            wait(&[h], None, true, now_us() + SPAN_US),
            WaitOutcome::Signaled { .. }
        ));
        assert_eq!(
            WAIT_TABLE_PROBES.load(Ordering::Relaxed) - looks_before,
            1,
            "a signalled syncobj cost more than one look"
        );
        assert_eq!(
            WAIT_SPIN_TURNS.load(Ordering::Relaxed) - turns_before,
            0,
            "a signalled syncobj paused before answering"
        );

        test_clock::set_auto_advance(0);
        assert!(destroy(h));
    }

    /// The backoff itself: no pause at all for the first looks, then growth,
    /// then a cap. The cap matters as much as the floor -- this is a pause
    /// measured in `spin_loop()` turns, and one that grew without bound
    /// would turn "look less often" into "notice the fence late".
    #[test]
    fn the_cpu_waits_backoff_starts_at_zero_grows_and_settles() {
        assert!(WAIT_EAGER_PROBES > 0, "the first look already pauses");
        for probes in 0..WAIT_EAGER_PROBES {
            assert_eq!(
                wait_spin_backoff(probes),
                0,
                "look {} paused before it looked",
                probes
            );
        }
        assert_eq!(wait_spin_backoff(WAIT_EAGER_PROBES), WAIT_SPIN_FLOOR);
        let mut prev = 0;
        let mut seen_cap = false;
        for probes in WAIT_EAGER_PROBES..WAIT_EAGER_PROBES + 200 {
            let turns = wait_spin_backoff(probes);
            assert!(
                (WAIT_SPIN_FLOOR..=WAIT_SPIN_CAP).contains(&turns),
                "look {} pauses {} turns",
                probes,
                turns
            );
            assert!(
                turns >= prev,
                "look {} pauses less than the one before",
                probes
            );
            prev = turns;
            seen_cap |= turns == WAIT_SPIN_CAP;
        }
        assert!(seen_cap, "the backoff never reaches its cap");
        // A wait parked long enough to run the shift off the top of the `u32`
        // stays at the cap. Unclamped it comes back ZERO, which is the whole
        // spin back again and the lock hammered exactly as before.
        assert_eq!(wait_spin_backoff(u32::MAX), WAIT_SPIN_CAP);
    }

    /// The first probes cost no timer: a fence that lands in tens of
    /// microseconds -- an ordinary glxgears frame -- is seen on a yield,
    /// never a millisecond later.
    #[test]
    fn a_fence_poll_yields_before_it_ever_arms_a_timer() {
        // Spelled out, because a zero here makes the loop below vacuous and
        // the whole test green on a schedule that never yields at all.
        assert!(
            FENCE_POLL_YIELDS > 0,
            "the poll arms a timer on its first probe"
        );
        for probes in 0..FENCE_POLL_YIELDS {
            assert_eq!(
                fence_poll_step(probes),
                PollStep::Yield,
                "probe {} armed a timer",
                probes
            );
        }
        assert_eq!(
            fence_poll_step(FENCE_POLL_YIELDS),
            PollStep::Sleep {
                us: FENCE_POLL_FLOOR_US
            }
        );
    }

    /// No sleep is ever asked for below the timer's arming floor (it would
    /// not wake any sooner) nor above the fixed tick this replaces.
    #[test]
    fn a_fence_poll_sleeps_between_the_timer_floor_and_the_old_tick() {
        let mut seen_cap = false;
        for probes in FENCE_POLL_YIELDS..FENCE_POLL_YIELDS + 200 {
            let PollStep::Sleep { us } = fence_poll_step(probes) else {
                panic!("probe {} still yielding", probes);
            };
            assert!(
                (FENCE_POLL_FLOOR_US..=FENCE_POLL_CAP_US).contains(&us),
                "probe {} sleeps {} us",
                probes,
                us
            );
            seen_cap |= us == FENCE_POLL_CAP_US;
        }
        assert!(seen_cap, "the backoff never reaches its cap");
        // And a wait parked long enough to overflow the shift stays at the
        // cap instead of panicking in a debug build.
        assert_eq!(
            fence_poll_step(u32::MAX),
            PollStep::Sleep {
                us: FENCE_POLL_CAP_US
            }
        );
    }

    /// The backoff only ever moves outward, and it reaches the cap in about
    /// a millisecond and a half, so a genuinely late fence costs no more
    /// wakeups than the fixed tick did.
    #[test]
    fn a_fence_poll_backs_off_monotonically_and_settles_at_the_cap() {
        let mut prev = 0;
        let mut elapsed = 0;
        let mut probes = FENCE_POLL_YIELDS;
        while let PollStep::Sleep { us } = fence_poll_step(probes) {
            assert!(
                us >= prev,
                "probe {} sleeps less than the one before",
                probes
            );
            if us == FENCE_POLL_CAP_US {
                break;
            }
            prev = us;
            elapsed += us;
            probes += 1;
        }
        assert!(
            elapsed <= 2_000,
            "{} us of short probes before the cap",
            elapsed
        );
        // A 100 ms bound (the pre-present fence's) costs about as many
        // wakeups as the old flat 1 ms tick did: 100, give or take the
        // handful of short ones at the front.
        let mut wakes = 0;
        let mut t = 0;
        let mut probes = FENCE_POLL_YIELDS;
        while t < 100_000 {
            let PollStep::Sleep { us } = fence_poll_step(probes) else {
                unreachable!()
            };
            t += us;
            wakes += 1;
            probes += 1;
        }
        assert!(wakes <= 110, "{} wakeups to wait out 100 ms", wakes);
    }

    std::thread_local! {
        static SIGNALS: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };
        /// `(ctx_idx, handle, point)` of each fence the timeout hook was
        /// fired for.
        static TIMEOUTS: RefCell<Vec<(u32, u32, u64)>> = const { RefCell::new(Vec::new()) };
    }

    fn record_signal(handle: u32, point: u64) {
        SIGNALS.with(|s| s.borrow_mut().push((handle, point)));
    }

    fn record_timeout(ctx_idx: u32, _va: usize, _payload: u32, handle: u32, point: u64) {
        TIMEOUTS.with(|s| s.borrow_mut().push((ctx_idx, handle, point)));
    }

    /// Install the upcalls and start from a clean slate. The hooks are the
    /// real registration path (`set_signal_hook` is what `linux-object` calls
    /// at boot to drive `SYNCOBJ_EVENTFD`), so what these tests observe is
    /// what an eventfd waiter would.
    fn arm_hooks() {
        set_signal_hook(record_signal);
        set_fence_timeout_hook(record_timeout);
        SIGNALS.with(|s| s.borrow_mut().clear());
        TIMEOUTS.with(|s| s.borrow_mut().clear());
    }

    fn signals() -> Vec<(u32, u64)> {
        SIGNALS.with(|s| s.borrow().clone())
    }

    fn timeouts() -> Vec<(u32, u32, u64)> {
        TIMEOUTS.with(|s| s.borrow().clone())
    }

    fn pending_now() -> usize {
        PENDING_COUNT.load(Ordering::Relaxed)
    }

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

    /// Two fences of the same channel share one landing zone and are told
    /// apart only by their payload, so the cache [`resolve_locked`] keeps of
    /// what it has already read must be keyed by BOTH.
    ///
    /// This is how a channel really works: the semaphore word is per channel,
    /// and each RELEASE writes a higher payload into it. Keyed by the address
    /// alone, the first fence's "landed" would answer for every later one on
    /// the same channel -- every syncobj of that channel would jump to its
    /// last point the moment the first frame landed, and a client would go on
    /// drawing into a buffer the GPU is still reading.
    #[test]
    fn one_landing_zone_carries_two_payloads_and_they_resolve_apart() {
        let _g = test_lock();
        let early = create(false);
        let late = create(false);
        let mut zone = Landing::new();
        assert!(attach_hw_fence(early, 1, zone.va(), 0, 1, 0, true));
        assert!(attach_hw_fence(late, 1, zone.va(), 0, 5, 0, true));
        assert_eq!(pending_now(), 2);

        // The GPU has written 1: the first fence has landed, the second has
        // not, and one resolve must say exactly that.
        zone.land(1);
        poll_pending();
        assert_eq!(query(early), Some(1), "payload 1 has landed");
        assert_eq!(
            query(late),
            Some(0),
            "payload 5 answered with payload 1's landing"
        );

        zone.land(5);
        poll_pending();
        assert_eq!(query(late), Some(1));
        assert!(destroy(early) && destroy(late));
    }

    /// A fence given up on after [`FENCE_TIMEOUT_US`] still advances the
    /// point (QUERY and WAIT read it as reached, like a `dma_fence` signaled
    /// with an error), but [`reached_by_timeout`] remembers which points got
    /// there that way -- on the handle, and on every import, transfer and
    /// merge that took the point from it -- until the CPU's own signal, a
    /// re-arm or a reset replaces the dead fence.
    #[test]
    fn a_point_reached_by_a_timed_out_fence_is_remembered_and_carried_by_its_links() {
        let _g = test_lock();
        arm_hooks();
        let src = create(false);
        let mut first = Landing::new();
        let dead = Landing::new();
        assert!(attach_hw_fence(src, 2, first.va(), 0, 1, 0, false));
        first.land(1);
        assert_eq!(query(src), Some(2), "point 2 is the GPU's word");
        assert!(attach_hw_fence(src, 5, dead.va(), 0, 2, 0, false));
        // Links made while the fence is still in flight: they materialise
        // when it is given up on.
        let importer = create(false);
        assert!(import_snapshot(importer, src, 5));
        let moved = create(false);
        assert!(transfer(moved, 9, src, 5));
        let merged = merge_fences(&[(src, 5)]);
        assert!(!reached_by_timeout(&[src], Some(&[5])), "still in flight");
        test_clock::advance(FENCE_TIMEOUT_US + 1);
        assert_eq!(query(src), Some(5));
        assert_eq!(
            timeouts().len(),
            2,
            "the fence itself, and its copy the transfer put on `moved`"
        );
        assert!(
            !reached_by_timeout(&[src], Some(&[2])),
            "2 was reached for real"
        );
        assert!(reached_by_timeout(&[src], Some(&[3])));
        assert!(reached_by_timeout(&[src], Some(&[5])));
        assert!(
            !reached_by_timeout(&[src], Some(&[6])),
            "6 is not reached at all, by anything"
        );
        assert_eq!(query(importer), Some(1));
        assert!(reached_by_timeout(&[importer], None));
        assert_eq!(query(moved), Some(9));
        assert!(reached_by_timeout(&[moved], Some(&[9])));
        assert!(
            reached_by_timeout(&[moved], Some(&[0])),
            "0 is the binary fence"
        );
        assert_eq!(query(merged), Some(1));
        assert!(reached_by_timeout(&[merged], Some(&[1])));
        // Links made AFTER the fence died take the point, and the taint,
        // at once.
        let late_import = create(false);
        assert!(import_snapshot(late_import, src, 5));
        assert!(reached_by_timeout(&[late_import], Some(&[1])));
        let late_move = create(false);
        assert!(transfer(late_move, 3, src, 5));
        assert!(reached_by_timeout(&[late_move], Some(&[3])));
        let late_merge = merge_fences(&[(src, 5)]);
        assert!(reached_by_timeout(&[late_merge], Some(&[1])));
        // But not from the good point.
        let good_import = create(false);
        assert!(import_snapshot(good_import, src, 2));
        assert!(!reached_by_timeout(&[good_import], Some(&[1])));
        // The whole list: one dead handle among good ones is enough.
        assert!(reached_by_timeout(&[good_import, late_move], Some(&[1, 3])));
        // What replaces the dead fence clears it: the CPU's own signal at or
        // past the point...
        assert!(timeline_signal(src, 5));
        assert!(!reached_by_timeout(&[src], Some(&[5])));
        assert!(!reached_by_timeout(&[src], Some(&[3])));
        // ...a reset...
        assert!(reset(late_move));
        assert!(!reached_by_timeout(&[late_move], Some(&[3])));
        // ...and a binary re-arm on the slot, which the next landing then
        // makes good.
        let mut again = Landing::new();
        assert!(attach_hw_fence(importer, 1, again.va(), 0, 1, 0, true));
        assert_eq!(query(importer), Some(0));
        assert!(!reached_by_timeout(&[importer], None));
        again.land(1);
        assert_eq!(query(importer), Some(1));
        assert!(!reached_by_timeout(&[importer], None));
        // A signal below the dead point leaves it dead.
        assert!(timeline_signal(moved, 4));
        assert!(reached_by_timeout(&[moved], Some(&[9])));
        // Gone is gone: the mark leaves with the object.
        assert!(destroy(moved));
        assert!(!reached_by_timeout(&[moved], Some(&[9])));
        assert_eq!(
            TABLE
                .lock()
                .errored
                .iter()
                .filter(|e| e.handle == moved)
                .count(),
            0
        );
        for h in [
            src,
            importer,
            merged,
            late_import,
            late_merge,
            good_import,
            late_move,
        ] {
            assert!(destroy(h));
        }
        assert!(TABLE.lock().errored.is_empty(), "nothing left behind");
    }

    /// EXEC turns a wait into an ACQUIRE of the fence that delivers its
    /// point, and a point the counter has already reached has no such
    /// fence: the wait is over. The list looked only at what was still in
    /// flight, so a timeline wait on a point already delivered came back
    /// with the NEXT fence pending on the handle, and a wait on point 1,
    /// read as binary, with the HIGHEST: a consumer that had waited for
    /// frame N stalled behind frame N+1, which nobody asked it to wait for.
    /// Linux drops a dependency on a signaled `dma_fence` before the job
    /// is scheduled.
    #[test]
    fn a_wait_on_a_point_already_reached_has_nothing_to_acquire_whatever_is_still_in_flight() {
        let _g = test_lock();
        arm_hooks();
        let tl = create(false);
        let mut z = Landing::new();
        for point in 1..=3u64 {
            assert!(attach_hw_fence(
                tl,
                point,
                z.va(),
                0,
                point as u32,
                0,
                false
            ));
        }
        z.land(2);
        assert_eq!(query(tl), Some(2), "points 1 and 2 are the GPU's word");
        assert_eq!(
            pending_hw_fences(tl, 2),
            [],
            "point 2 is reached: nothing to acquire, not the fence of point 3"
        );
        assert_eq!(
            pending_hw_fences(tl, 1),
            [],
            "point 1 is reached too, and no binary wait on the highest fence in flight"
        );
        assert_eq!(
            pending_hw_fences(tl, 3),
            [(z.va(), 0, 3, 0)],
            "point 3 is the fence still in flight"
        );
        // A binary syncobj re-armed is at 0 with its fence in flight; one
        // signaled by the CPU has no fence at all. Neither changes.
        let b = create(true);
        assert_eq!(pending_hw_fences(b, 1), []);
        let mut zb = Landing::new();
        assert!(attach_hw_fence(b, 1, zb.va(), 0, 7, 1, true));
        assert_eq!(pending_hw_fences(b, 1), [(zb.va(), 0, 7, 1)]);
        zb.land(7);
        assert_eq!(query(b), Some(1));
        assert_eq!(pending_hw_fences(b, 1), []);
        z.land(3);
        assert_eq!(query(tl), Some(3));
        for h in [tl, b] {
            assert!(destroy(h));
        }
    }

    /// The bug this guards: a binary syncobj is a fence SLOT, not a counter.
    /// Signalling it a second time used to hit `point <= obj.point` (its point
    /// is always 1) and drop the new fence on the floor, leaving the object
    /// reading "signaled" while the GPU was still writing — so the next waiter
    /// went straight through and sampled a half-written buffer.
    #[test]
    fn a_second_binary_signal_rearms_the_syncobj_on_the_new_fence() {
        let _g = test_lock();
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
        let _g = test_lock();
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
        let _g = test_lock();
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
        let _g = test_lock();
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
        let _g = test_lock();
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
        let _g = test_lock();
        let h = create(false);
        let inflight = Landing::new();

        // Point 7 submitted while the handle is still at 0.
        assert!(attach_hw_fence(h, 7, inflight.va(), 0, 3, 0, false));
        assert_eq!(query(h), Some(0));
        assert_eq!(query_submitted(h), Some(7));

        // A binary signal arrives on the same handle.
        let stray = Landing::new();
        assert!(attach_hw_fence(h, 1, stray.va(), 0, 1, 0, true));

        let vas: Vec<usize> = pending_hw_fences(h, 7).into_iter().map(|f| f.0).collect();
        assert!(
            vas.contains(&inflight.va()),
            "the in-flight timeline fence must survive a binary signal"
        );
        // ...and the slot's own fence is the node below it in the chain: 7
        // counts once both have landed.
        assert_eq!(vas.len(), 2);
        assert!(vas.contains(&stray.va()));
        assert_eq!(query_submitted(h), Some(7));

        destroy(h);
    }

    /// Closing a syncobj that still has a submit in flight is the ordinary
    /// end of a frame's semaphore, and process teardown does it to every
    /// handle a client owns (`drm_scheme.rs`'s release path). The fences it
    /// left behind outlived it: the resolver could no longer find the object
    /// to advance, nobody could wait on it, and 10 s later the timeout hook
    /// fired and latched the GPU context WEDGED -- so the client's NEXT
    /// submit (or another process sharing the channel) failed with
    /// EIO/device-lost, for a syncobj that had simply been closed.
    #[test]
    fn destroying_a_syncobj_drops_the_timeline_fences_still_in_flight() {
        let _g = test_lock();
        arm_hooks();
        const CTX: u32 = 0x5e_71;

        let before = pending_now();
        let h = create(false);
        let fences = [Landing::new(), Landing::new(), Landing::new()];
        // Binary range, an ordinary timeline point, and the far end of the
        // counter: which fences go must not depend on the point at all. Only
        // the first of the three was dropped before, and the filter's blind
        // spot is everything above binary range.
        for (i, point) in [1u64, 7, u64::MAX].iter().enumerate() {
            assert!(attach_hw_fence(h, *point, fences[i].va(), 0, 3, CTX, false));
        }
        assert_eq!(pending_now(), before + 3, "three fences in flight");

        assert!(destroy(h));
        assert_eq!(
            pending_now(),
            before,
            "destroying a syncobj must drop every fence in flight on it, not \
             just the ones inside binary range"
        );

        // Past the hang deadline the orphan used to reach the timeout hook.
        test_clock::advance(FENCE_TIMEOUT_US + 1);
        poll_pending();
        assert!(
            timeouts().iter().all(|&(ctx, ..)| ctx != CTX),
            "a closed syncobj must not wedge the context its fence was on"
        );
    }

    /// `SYNCOBJ_RESET` is `drm_syncobj_replace_fence(syncobj, NULL)`: the
    /// object carries no fence afterwards. A timeline fence left in flight
    /// across the reset landed later and drove the counter back up on its
    /// own, so a syncobj Mesa had just reset for reuse re-signaled itself
    /// with nobody having signaled it.
    #[test]
    fn resetting_a_syncobj_drops_the_timeline_fence_it_carried() {
        let _g = test_lock();

        let h = create(false);
        let mut fence = Landing::new();
        assert!(attach_hw_fence(h, 7, fence.va(), 0, 3, 0, false));
        assert_eq!(query_submitted(h), Some(7));

        assert!(reset(h));
        assert_eq!(
            query_submitted(h),
            Some(0),
            "a reset syncobj carries no fence, in flight ones included"
        );

        // The GPU writes the landing zone anyway: the fence was real, it just
        // no longer belongs to anything.
        fence.land(3);
        assert_eq!(
            query(h),
            Some(0),
            "a reset syncobj must not re-signal itself from the fence it had"
        );

        destroy(h);
    }

    /// A deferred transfer names its source BY HANDLE, because there is no
    /// refcounted `dma_fence` here to hand over. So destroying the source --
    /// a client closing its acquire syncobj, or exiting -- left the
    /// destination linked to a handle that no longer resolves: its point
    /// could never advance again. That is wlroots' `linux-drm-syncobj-v1`
    /// timeline, and NVK waits on it with an INT64_MAX deadline, so the
    /// surface (and the output committing it) froze for good.
    #[test]
    fn destroying_a_transfer_source_releases_the_destination_it_promised() {
        let _g = test_lock();
        let src = create(false);
        let dst = create(false);

        // `src` is nowhere near point 5, so the transfer stays deferred.
        assert!(transfer(dst, 9, src, 5));
        assert_eq!(query(dst), Some(0), "nothing has landed yet");

        arm_hooks();
        assert!(destroy(src));
        assert!(
            signals().contains(&(dst, 9)),
            "and an eventfd armed on that point has to be woken -- reading the \
             table is not how a waiter finds out, got {:?}",
            signals()
        );
        assert_eq!(
            query(dst),
            Some(9),
            "the destination of a transfer whose source went away must be \
             released to the point it was promised, not parked forever"
        );
        assert!(matches!(
            wait(&[dst], Some(&[9]), false, 0),
            WaitOutcome::Signaled { .. }
        ));

        destroy(dst);
    }

    /// Every table access resolves the pending fences first, and that
    /// resolution is DESTRUCTIVE: the fence is taken out of the table and the
    /// counter advanced before the caller's own work begins. The upcalls it
    /// produced are handed back to be made once the lock is released. An
    /// ioctl that then bailed out on an unknown handle dropped that parcel on
    /// the floor, and there was nothing left to retry it with: the
    /// `SYNCOBJ_EVENTFD` waiter for the point that had just been reached was
    /// never woken, which for a compositor is a surface that never repaints.
    /// One `SYNCOBJ_FD_TO_HANDLE` with a stale handle -- a race any client
    /// can lose at teardown -- was enough.
    #[test]
    fn a_failed_import_still_delivers_the_signals_it_resolved() {
        let _g = test_lock();
        const GONE: u32 = 0xdead_beef;

        // Bad DESTINATION: the source resolves, the destination does not.
        let h = create(false);
        let mut fence = Landing::new();
        assert!(attach_hw_fence(h, 7, fence.va(), 0, 3, 0, false));
        arm_hooks();
        // Land it WITHOUT touching the table, so the resolution happens
        // inside the failing call.
        fence.land(3);
        assert!(!import_snapshot(GONE, h, 1), "unknown destination handle");
        assert!(
            signals().contains(&(h, 7)),
            "the fence that landed inside the failing import must still wake \
             its waiters, got {:?}",
            signals()
        );

        // Bad SOURCE: the call gives up before it even looks at the
        // destination, one return earlier.
        let h2 = create(false);
        let mut fence2 = Landing::new();
        assert!(attach_hw_fence(h2, 7, fence2.va(), 0, 3, 0, false));
        arm_hooks();
        fence2.land(3);
        assert!(!import_snapshot(h, GONE, 1), "unknown source handle");
        assert!(signals().contains(&(h2, 7)), "got {:?}", signals());

        destroy(h2);
        destroy(h);
    }

    /// [`transfer`] has the same two exits, and loses the same parcel.
    #[test]
    fn a_failed_transfer_still_delivers_the_signals_it_resolved() {
        let _g = test_lock();
        const GONE: u32 = 0xdead_beef;

        let h = create(false);
        let mut fence = Landing::new();
        assert!(attach_hw_fence(h, 7, fence.va(), 0, 3, 0, false));
        arm_hooks();
        fence.land(3);
        assert!(!transfer(GONE, 1, h, 1), "unknown destination handle");
        assert!(signals().contains(&(h, 7)), "got {:?}", signals());

        let h2 = create(false);
        let mut fence2 = Landing::new();
        assert!(attach_hw_fence(h2, 7, fence2.va(), 0, 3, 0, false));
        arm_hooks();
        fence2.land(3);
        assert!(!transfer(h, 1, GONE, 1), "unknown source handle");
        assert!(signals().contains(&(h2, 7)), "got {:?}", signals());

        destroy(h2);
        destroy(h);
    }

    /// The other half of the dropped parcel: a fence that did NOT land within
    /// [`FENCE_TIMEOUT_US`] is a hung ring, and the upcall is how the driver
    /// learns of it -- it latches the context wedged so the next submit fails
    /// honestly instead of the client waiting on a GPU that will never
    /// answer. Losing it turns a reported hang into a silent one.
    #[test]
    fn a_failed_ioctl_still_reports_the_fence_that_timed_out() {
        let _g = test_lock();
        const CTX: u32 = 0x7a_11;
        const GONE: u32 = 0xdead_beef;

        let h = create(false);
        let fence = Landing::new();
        assert!(attach_hw_fence(h, 7, fence.va(), 0, 3, CTX, false));

        test_clock::advance(FENCE_TIMEOUT_US + 1);
        arm_hooks();
        assert!(!import_snapshot(GONE, h, 1), "unknown destination handle");
        assert!(
            timeouts().contains(&(CTX, h, 7)),
            "a hung fence resolved inside a failing ioctl must still reach the \
             timeout hook, got {:?}",
            timeouts()
        );

        destroy(h);
    }

    /// The two fixes have to stay independent: dropping a destroyed handle's
    /// fences must not touch anybody else's, and the links it releases are
    /// only the ones that named it.
    #[test]
    fn destroying_one_syncobj_leaves_the_others_alone() {
        let _g = test_lock();
        let keep = create(false);
        let doomed = create(false);
        let other_src = create(false);
        let linked = create(false);

        let kept_fence = Landing::new();
        let doomed_fence = Landing::new();
        assert!(attach_hw_fence(keep, 7, kept_fence.va(), 0, 3, 0, false));
        assert!(attach_hw_fence(
            doomed,
            7,
            doomed_fence.va(),
            0,
            3,
            0,
            false
        ));
        // `linked` waits on a source that is NOT the one being destroyed.
        assert!(transfer(linked, 9, other_src, 5));

        let before = pending_now();
        assert!(destroy(doomed));
        assert_eq!(before - 1, pending_now(), "only its own fence goes");
        assert_eq!(
            query_submitted(keep),
            Some(7),
            "another handle's fence must survive"
        );
        assert_eq!(
            query(linked),
            Some(0),
            "a link on a different source must not be released"
        );

        // And the link still works once its real source arrives.
        assert!(timeline_signal(other_src, 5));
        assert_eq!(query(linked), Some(9));

        destroy(linked);
        destroy(other_src);
        destroy(keep);
    }

    #[cfg(test)]
    fn object_count() -> usize {
        TABLE.lock().objects.len()
    }

    /// `SYNC_IOC_MERGE`: the merged fence signals only once EVERY source has,
    /// the way the `dma_fence_array` behind a merged sync_file does.
    #[test]
    fn a_merged_fence_waits_for_every_source() {
        let _g = test_lock();
        let a = create(false);
        let b = create(false);
        let m = merge_fences(&[(a, 3), (b, 1)]);
        assert_eq!(query(m), Some(0), "nothing has been reached yet");
        assert!(timeline_signal(a, 3));
        assert_eq!(query(m), Some(0), "one source of two is not enough");
        assert!(matches!(wait(&[m], None, false, 0), WaitOutcome::Timeout));
        assert!(timeline_signal(b, 1));
        assert_eq!(query(m), Some(1), "both sources reached: the merge signals");
        assert!(matches!(
            wait(&[m], None, false, 0),
            WaitOutcome::Signaled { .. }
        ));
        destroy(m);
        destroy(a);
        destroy(b);
    }

    #[test]
    fn a_merge_of_fences_already_reached_is_signaled_at_once() {
        let _g = test_lock();
        let a = create(true);
        let b = create(false);
        assert!(timeline_signal(b, 5));
        let m = merge_fences(&[(a, 1), (b, 5)]);
        assert_eq!(query(m), Some(1));
        destroy(m);
        destroy(a);
        destroy(b);
    }

    /// Mesa's per-frame pattern under X11: a surrogate syncobj receives a
    /// transfer of a point still in flight, is exported as a sync_file, the
    /// sync_file is imported into the acquire semaphore, and the surrogate
    /// is destroyed at once. The semaphore must NOT signal until the fence
    /// lands -- releasing it on the surrogate's destroy handed the GPU an
    /// image the compositor was still reading.
    #[test]
    fn a_link_survives_the_destroy_of_its_source() {
        let _g = test_lock();
        arm_hooks();
        let release = create(false);
        let mut landing = Landing::new();
        assert!(attach_hw_fence(release, 4, landing.va(), 0, 7, 0, false));
        let surrogate = create(false);
        assert!(transfer(surrogate, 0, release, 4));
        let sem = create(false);
        assert!(import_snapshot(sem, surrogate, 1));
        assert!(destroy(surrogate), "destroyed right after the export");
        assert_eq!(
            query(sem),
            Some(0),
            "the fence has not landed: the semaphore must stay unsignaled"
        );
        assert!(
            !signals().iter().any(|&(h, _)| h == sem || h == surrogate),
            "and nobody was woken early, got {:?}",
            signals()
        );
        landing.land(7);
        assert_eq!(
            query(sem),
            Some(1),
            "once the GPU writes the fence the link resolves through the orphan"
        );
        assert!(
            !signals().is_empty(),
            "and the landing is announced, so an eventfd waiter re-checks"
        );
        destroy(sem);
        destroy(release);
    }

    /// Same, one level deeper: the dying source is itself only linked.
    #[test]
    fn a_link_survives_the_destroy_of_a_source_that_is_itself_linked() {
        let _g = test_lock();
        let release = create(false);
        let surrogate = create(false);
        assert!(
            transfer(surrogate, 0, release, 2),
            "deferred: release is at 0"
        );
        let sem = create(false);
        assert!(import_snapshot(sem, surrogate, 1));
        assert!(destroy(surrogate));
        assert_eq!(query(sem), Some(0), "release has not been signaled");
        assert!(timeline_signal(release, 2));
        assert_eq!(query(sem), Some(1));
        destroy(sem);
        destroy(release);
    }

    /// An orphan lives exactly as long as something depends on it.
    #[test]
    fn an_orphaned_source_is_collected_once_nothing_depends_on_it() {
        let _g = test_lock();
        let src = create(false);
        let mut landing = Landing::new();
        assert!(attach_hw_fence(src, 1, landing.va(), 0, 1, 0, true));
        let dst = create(false);
        assert!(import_snapshot(dst, src, 1));
        let objects = object_count();
        let pending = pending_now();
        assert!(destroy(src));
        assert_eq!(
            object_count(),
            objects,
            "still named by dst's link: kept as an orphan"
        );
        assert!(!add_ref(src), "an orphan cannot be revived");
        assert!(!destroy(src), "nor destroyed twice");
        // A direct signal replaces dst's fence, so nothing names the orphan.
        assert!(timeline_signal(dst, 1));
        assert_eq!(object_count(), objects - 1, "the orphan is collected");
        assert_eq!(pending_now(), pending - 1, "and its fence with it");
        landing.land(1);
        destroy(dst);
    }

    /// A merge whose source is gone for good (no fence, no link) keeps
    /// waiting for the sources that are still alive.
    #[test]
    fn a_merge_outlives_a_source_that_can_never_signal() {
        let _g = test_lock();
        let a = create(false);
        let b = create(false);
        let m = merge_fences(&[(a, 1), (b, 1)]);
        assert!(destroy(a));
        assert_eq!(query(m), Some(0), "b has not signaled");
        assert!(timeline_signal(b, 1));
        assert_eq!(query(m), Some(1));
        destroy(m);
        destroy(b);
    }

    // ----- The wait family, abandoned landing zones, exported snapshots -----

    /// `WAIT_AVAILABLE`: a fence that is submitted but has not landed
    /// satisfies the wait (Mesa's "wait for the point to be queued"); a
    /// plain wait still parks on it, and neither flavour signals anything.
    #[test]
    fn wait_available_is_satisfied_by_a_fence_in_flight_and_a_plain_wait_is_not() {
        let _g = test_lock();
        arm_hooks();
        let h = create(false);
        let t = create(false);
        let mut zone = Landing::new();
        // Nothing submitted yet: nothing is available either.
        assert!(matches!(
            wait_available(&[h], None, true, 0),
            WaitOutcome::Timeout
        ));
        assert!(matches!(
            wait_available_ready(&[h], None, true, 0),
            Some(Err(WaitOutcome::Timeout))
        ));
        assert!(attach_hw_fence(h, 1, zone.va(), 0, 1, 0, true));
        assert!(attach_hw_fence(t, 5, zone.va(), 0, 2, 0, false));
        assert!(
            matches!(wait(&[h], None, true, 0), WaitOutcome::Timeout),
            "submitted is not landed"
        );
        assert!(matches!(
            wait_available(&[h], None, true, 0),
            WaitOutcome::Signaled {
                first_signaled_index: 0
            }
        ));
        assert!(matches!(
            wait_ready(&[h], None, true, 0),
            Some(Err(WaitOutcome::Timeout))
        ));
        assert!(matches!(
            wait_available_ready(&[h], None, true, 0),
            Some(Ok(0))
        ));
        // The point in flight is the bound: at it available, past it not.
        assert!(matches!(
            wait_available(&[t], Some(&[5]), true, 0),
            WaitOutcome::Signaled { .. }
        ));
        assert!(matches!(
            wait_available(&[t], Some(&[6]), true, 0),
            WaitOutcome::Timeout
        ));
        assert!(matches!(
            wait_available_ready(&[t], Some(&[6]), true, 0),
            Some(Err(WaitOutcome::Timeout))
        ));
        // Any-of names the first satisfied index; all-of needs every one.
        assert!(matches!(
            wait_available(&[t, h], Some(&[6, 1]), false, 0),
            WaitOutcome::Signaled {
                first_signaled_index: 1
            }
        ));
        assert!(matches!(
            wait_available_ready(&[t, h], Some(&[6, 1]), false, 0),
            Some(Ok(1))
        ));
        assert!(matches!(
            wait_available(&[t, h], Some(&[6, 1]), true, 0),
            WaitOutcome::Timeout
        ));
        // Available is not signaled: the counter reads 0 and the fence stays.
        assert_eq!(query(h), Some(0));
        assert_eq!(query_submitted(h), Some(1));
        assert_eq!(pending_now(), 2);
        assert!(
            signals().iter().all(|&(_, p)| p >= 1),
            "only submits announced"
        );
        // A probe with the deadline ahead says sleep...
        let ahead = now_us() + 1_000_000;
        assert!(wait_ready(&[h], None, true, ahead).is_none());
        // ...until the fence lands, when the plain wait goes through.
        zone.land(2);
        assert!(matches!(wait_ready(&[h], None, true, ahead), Some(Ok(0))));
        assert!(matches!(
            wait(&[h, t], Some(&[1, 5]), true, 0),
            WaitOutcome::Signaled {
                first_signaled_index: 0
            }
        ));
        assert_eq!(pending_now(), 0);
        // An unknown handle is Invalid on every flavour, whatever the others.
        assert!(matches!(
            wait_available(&[h, 0xdead_0000], None, false, 0),
            WaitOutcome::Invalid
        ));
        assert!(matches!(
            wait_available_ready(&[h, 0xdead_0000], None, false, 0),
            Some(Err(WaitOutcome::Invalid))
        ));
        destroy(h);
        destroy(t);
    }

    /// A driver about to submit on a channel may take a fence pending on
    /// THAT channel as already ordered: the ring executes in order, so the
    /// submission cannot overtake it. A fence on any other channel is a real
    /// wait, and an ordered fence is not a signaled one.
    #[test]
    fn wait_ordered_trusts_a_fence_on_its_own_channel_and_waits_for_every_other() {
        let _g = test_lock();
        arm_hooks();
        let same = create(false);
        let other = create(false);
        let plain = create(false);
        let mut zone_a = Landing::new();
        let mut zone_b = Landing::new();
        assert!(attach_hw_fence(same, 3, zone_a.va(), 0, 1, 1, false));
        assert!(attach_hw_fence(other, 1, zone_b.va(), 0, 1, 2, true));
        assert!(timeline_signal(plain, 7));
        assert!(matches!(
            wait_ordered(&[same], Some(&[3]), 0, 1),
            WaitOutcome::Signaled {
                first_signaled_index: 0
            }
        ));
        assert!(
            matches!(
                wait_ordered(&[same], Some(&[4]), 0, 1),
                WaitOutcome::Timeout
            ),
            "only up to the point the ring will land"
        );
        assert!(
            matches!(
                wait_ordered(&[same], Some(&[3]), 0, 2),
                WaitOutcome::Timeout
            ),
            "from another channel the same fence is a wait"
        );
        // Every handle must pass: the other channel's fence holds it up...
        assert!(matches!(
            wait_ordered(&[plain, same, other], Some(&[7, 3, 1]), 0, 1),
            WaitOutcome::Timeout
        ));
        // ...until it lands.
        zone_b.land(1);
        assert!(matches!(
            wait_ordered(&[plain, same, other], Some(&[7, 3, 1]), 0, 1),
            WaitOutcome::Signaled {
                first_signaled_index: 0
            }
        ));
        // Ordered is not signaled: `same` still waits for its own landing.
        assert_eq!(query(same), Some(0));
        assert_eq!(pending_now(), 1);
        assert!(matches!(
            wait(&[same], Some(&[3]), true, 0),
            WaitOutcome::Timeout
        ));
        zone_a.land(1);
        assert_eq!(query(same), Some(3));
        assert!(matches!(
            wait_ordered(&[same, 0xdead_0000], None, 0, 1),
            WaitOutcome::Invalid
        ));
        destroy(same);
        destroy(other);
        destroy(plain);
    }

    /// A channel going away takes its landing zone with it. Every fence
    /// pending on that zone is released as signaled -- its waiters move on,
    /// the eventfd side hears of it, an import of it resolves -- and none is
    /// reported as a timeout; a fence on any other zone is untouched.
    #[test]
    fn abandoning_a_landing_zone_releases_its_fences_as_signaled_and_no_others() {
        let _g = test_lock();
        arm_hooks();
        let a = create(false);
        let b = create(false);
        let c = create(false);
        let dst = create(false);
        let dead = Landing::new();
        let live = Landing::new();
        assert!(attach_hw_fence(a, 3, dead.va(), 0, 1, 1, false));
        assert!(attach_hw_fence(a, 5, dead.va(), 0, 2, 1, false));
        assert!(attach_hw_fence(b, 1, dead.va(), 0, 3, 1, true));
        assert!(attach_hw_fence(c, 1, live.va(), 0, 1, 2, true));
        // The compositor's acquire: a's point 3, imported as a sync_file.
        assert!(import_snapshot(dst, a, 3));
        assert_eq!(query(dst), Some(0));
        assert_eq!(pending_now(), 4);
        assert_eq!(abandon_fences(0xdead_0000), 0, "a zone nobody uses");
        assert_eq!(pending_now(), 4);
        SIGNALS.with(|s| s.borrow_mut().clear());
        assert_eq!(abandon_fences(dead.va()), 3);
        assert_eq!(pending_now(), 1, "the live zone's fence stays");
        assert_eq!(query(a), Some(5));
        assert_eq!(query(b), Some(1));
        assert_eq!(query(c), Some(0));
        assert_eq!(query(dst), Some(1), "a reached 3, so the import is in");
        assert!(matches!(
            wait(&[a, b], Some(&[5, 1]), true, 0),
            WaitOutcome::Signaled { .. }
        ));
        assert!(matches!(wait(&[c], None, true, 0), WaitOutcome::Timeout));
        let heard = signals();
        assert!(heard.contains(&(a, 5)), "announced: {:?}", heard);
        assert!(heard.contains(&(b, 1)), "announced: {:?}", heard);
        assert!(heard.iter().all(|&(h, _)| h != c), "not c: {:?}", heard);
        // Released, not timed out: the ring is not latched wedged for them.
        // The live zone's fence does time out, on its own clock.
        test_clock::advance(FENCE_TIMEOUT_US + 1);
        assert_eq!(query(c), Some(1), "timed out: released too, but reported");
        assert_eq!(timeouts(), [(2, c, 1)]);
        destroy(a);
        destroy(b);
        destroy(c);
        destroy(dst);
    }

    /// `HANDLE_TO_FD` with `EXPORT_SYNC_FILE`: the snapshot names the fence
    /// current at export time -- the highest submission in flight, never
    /// point 0 (an unsignaled binary syncobj exports its next signal), so an
    /// importer waits for exactly that submission and not one less.
    #[test]
    fn an_exported_snapshot_names_the_fence_in_flight_and_never_point_zero() {
        let _g = test_lock();
        arm_hooks();
        assert_eq!(export_snapshot(0xdead_0000), None);
        let h = create(false);
        assert_eq!(export_snapshot(h), Some(1), "unsignaled: its next signal");
        let s = create(true);
        assert_eq!(export_snapshot(s), Some(1));
        let t = create(false);
        assert!(timeline_signal(t, 4));
        assert_eq!(export_snapshot(t), Some(4));
        let mut zone = Landing::new();
        assert!(attach_hw_fence(t, 6, zone.va(), 0, 1, 0, false));
        assert!(attach_hw_fence(t, 9, zone.va(), 0, 2, 0, false));
        assert_eq!(
            export_snapshot(t),
            Some(9),
            "the highest fence in flight, not the counter"
        );
        let dst = create(false);
        assert!(import_snapshot(dst, t, export_snapshot(t).unwrap()));
        zone.land(1);
        assert_eq!(query(t), Some(6));
        assert_eq!(query(dst), Some(0), "the second submission has not landed");
        zone.land(2);
        assert_eq!(query(dst), Some(1));
        assert_eq!(export_snapshot(t), Some(9), "landed: the counter itself");
        // An object that is itself waiting on an import exports its next
        // signal, and a snapshot of it resolves when that import does.
        let src = create(false);
        let linked = create(false);
        assert!(import_snapshot(linked, src, 1));
        assert_eq!(export_snapshot(linked), Some(1));
        let far = create(false);
        assert!(import_snapshot(far, linked, 1));
        assert_eq!(query(far), Some(0));
        assert!(signal(src));
        assert_eq!(query(far), Some(1));
        for x in [h, s, t, dst, src, linked, far] {
            destroy(x);
        }
    }

    /// The one line every timed-out wait prints: each handle with its
    /// target, its current point (-1 for one that does not exist) and the
    /// fences still in flight for it, with the channel they ride.
    #[test]
    fn describe_names_each_handle_with_target_current_and_the_fences_in_flight() {
        let _g = test_lock();
        let h = create(false);
        let t = create(false);
        assert!(timeline_signal(t, 2));
        let zone = Landing::new();
        assert!(attach_hw_fence(t, 7, zone.va(), 0, 1, 3, false));
        assert!(attach_hw_fence(t, 8, zone.va(), 0, 2, 3, false));
        assert_eq!(
            describe(&[h, t, 0xdead_0000], Some(&[1, 7, 1])),
            alloc::format!(" {:#x}:1/0 {:#x}:7/2+7(ctx3)+8(ctx3) 0xdead0000:1/-1", h, t)
        );
        assert_eq!(
            describe(&[t], None),
            alloc::format!(" {:#x}:1/2+7(ctx3)+8(ctx3)", t),
            "no points: binary targets"
        );
        assert_eq!(describe(&[], None), "");
        destroy(h);
        destroy(t);
    }

    // ----- A fence, once signaled, stays signaled -----

    /// A `sync_file` holds the `dma_fence` that was current at export time,
    /// and a signaled `dma_fence` never un-signals. Here an import is a link
    /// to its SOURCE, and the source is a binary slot that the next frame's
    /// submit rewinds to 0 -- so an importer that had read "signaled" read
    /// "unsignaled" again the moment its source was re-armed.
    #[test]
    fn an_importer_stays_signaled_when_its_source_is_rearmed() {
        let _g = test_lock();
        arm_hooks();
        let src = create(false);
        let mut first = Landing::new();
        assert!(attach_hw_fence(src, 1, first.va(), 0, 1, 0, true));
        let dst = create(false);
        assert!(import_snapshot(dst, src, export_snapshot(src).unwrap()));
        first.land(1);
        assert_eq!(
            query(dst),
            Some(1),
            "the fence landed: the importer is signaled"
        );
        // Next frame: the producer re-arms its binary syncobj on a new fence.
        let mut second = Landing::new();
        assert!(attach_hw_fence(src, 1, second.va(), 0, 2, 0, true));
        assert_eq!(query(src), Some(0), "the slot is armed on the new fence");
        assert_eq!(
            query(dst),
            Some(1),
            "but the fence the importer holds has signaled, and stays signaled"
        );
        second.land(2);
        destroy(dst);
        destroy(src);
    }

    /// Same source re-arm, with the first fence landed but not yet seen by
    /// anyone (nobody polled between the GPU's write and the next EXEC): the
    /// re-arm discarded that fence as "superseded", and with it the only
    /// thing that could ever signal the importer.
    #[test]
    fn a_landed_fence_the_source_replaces_before_anyone_looked_still_signals_the_importer() {
        let _g = test_lock();
        arm_hooks();
        let src = create(false);
        let mut first = Landing::new();
        assert!(attach_hw_fence(src, 1, first.va(), 0, 1, 0, true));
        let dst = create(false);
        assert!(import_snapshot(dst, src, export_snapshot(src).unwrap()));
        first.land(1);
        let mut second = Landing::new();
        assert!(attach_hw_fence(src, 1, second.va(), 0, 2, 0, true));
        assert_eq!(query(src), Some(0));
        assert_eq!(
            query(dst),
            Some(1),
            "the fence it imported landed before the replacement"
        );
        second.land(2);
        destroy(dst);
        destroy(src);
    }

    /// `EXPORT_SYNC_FILE` hands out the fence the syncobj carries NOW, and a
    /// `sync_file` holds its own reference to it: resetting or re-arming the
    /// syncobj afterwards does not touch the file. Mesa exports a `SYNC_FD`
    /// and resets the binary semaphore in the same call
    /// (`vk_common_GetSemaphoreFdKHR`, copy transference) while the GPU is
    /// still on its way to the fence. Here the file was `(handle, point)`
    /// read live against the source: the reset dropped the pending fence and
    /// zeroed the point, and the file, its merges and its imports waited for
    /// the source's next signal, or for ever.
    #[test]
    fn an_exported_sync_file_keeps_its_fence_through_a_reset_or_a_re_arm_of_the_source() {
        let _g = test_lock();
        arm_hooks();
        // In flight at export, reset before it lands.
        let src = create(false);
        let mut zone = Landing::new();
        assert!(attach_hw_fence(src, 1, zone.va(), 0x100, 7, 3, true));
        let fd = export_fence(src).expect("a live source");
        assert_ne!(fd, src, "a syncobj of its own");
        assert_eq!(
            pending_hw_fences(fd, 1),
            alloc::vec![(zone.va(), 0x100u64, 7u32, 3u32)],
            "the file carries the same hardware fence"
        );
        assert!(reset(src));
        assert_eq!(query(src), Some(0));
        assert!(
            pending_hw_fences(src, 1).is_empty(),
            "the reset dropped the source's fence"
        );
        assert_eq!(query(fd), Some(0), "not landed yet");
        let dst = create(false);
        assert!(import_snapshot(dst, fd, 1));
        assert!(matches!(
            wait_ready(&[dst], None, true, 0),
            Some(Err(WaitOutcome::Timeout))
        ));
        zone.land(7);
        assert_eq!(query(fd), Some(1), "the fence landed: the file signals");
        assert_eq!(query(dst), Some(1), "and so does its import");
        assert_eq!(query(src), Some(0), "the reset source does not");
        destroy(dst);
        destroy(fd);
        // Re-armed by the next submit before the exported fence lands.
        let mut first = Landing::new();
        assert!(attach_hw_fence(src, 1, first.va(), 0, 8, 3, true));
        let fd = export_fence(src).unwrap();
        let mut second = Landing::new();
        assert!(attach_hw_fence(src, 1, second.va(), 0, 9, 3, true));
        assert_eq!(query(src), Some(0));
        first.land(8);
        assert_eq!(query(fd), Some(1), "the first submission's fence");
        assert_eq!(query(src), Some(0), "the source waits for the second");
        second.land(9);
        assert_eq!(query(src), Some(1));
        destroy(fd);
        // Landed before the export: reached at once, and a reset changes
        // nothing about it.
        let fd = export_fence(src).unwrap();
        assert_eq!(query(fd), Some(1));
        assert!(reset(src));
        assert_eq!(query(fd), Some(1));
        assert!(!reached_by_timeout(&[fd], Some(&[1])));
        destroy(fd);
        // Given up on before the export: the file carries the timeout mark.
        let dead = Landing::new();
        assert!(attach_hw_fence(src, 1, dead.va(), 0, 10, 3, true));
        test_clock::advance(FENCE_TIMEOUT_US + 1);
        assert_eq!(query(src), Some(1), "reached, by the timeout");
        let fd = export_fence(src).unwrap();
        assert_eq!(query(fd), Some(1));
        assert!(reached_by_timeout(&[fd], Some(&[1])), "and marked as such");
        destroy(fd);
        assert!(reset(src));
        // No fence at all: the file is the source's next signal.
        let fd = export_fence(src).unwrap();
        assert_eq!(query(fd), Some(0));
        assert!(signal(src));
        assert_eq!(query(fd), Some(1));
        destroy(fd);
        destroy(src);
        // A timeline: the file is the point in flight, not the counter.
        let tl = create(false);
        assert!(timeline_signal(tl, 3));
        let mut z = Landing::new();
        assert!(attach_hw_fence(tl, 5, z.va(), 0, 11, 3, false));
        let fd = export_fence(tl).unwrap();
        assert_eq!(query(fd), Some(0), "point 5 is still in flight");
        assert!(reset(tl));
        z.land(11);
        assert_eq!(query(fd), Some(1));
        destroy(fd);
        destroy(tl);
        // An unknown handle exports nothing, and leaves nothing behind.
        let before = TABLE.lock().objects.len();
        assert_eq!(export_fence(0xdead_0000), None);
        assert_eq!(TABLE.lock().objects.len(), before);
    }

    /// `IMPORT_SYNC_FILE` and a binary `TRANSFER` REPLACE the destination's
    /// fence (`drm_syncobj_replace_fence` in Linux: the old `dma_fence` may
    /// still signal, but the syncobj no longer carries it). The destination
    /// here kept its previous hardware fence in `pending` next to the new
    /// link, so when that superseded fence landed it advanced the object,
    /// and a waiter went through before the fence the import stood for had
    /// been reached; `pending_hw_fences` handed EXEC the same stale fence to
    /// ACQUIRE. And a slot whose fence had already landed stayed signaled
    /// through the import, so the next wait on it passed at once (a
    /// `VkSemaphore` imported into again: signal, wait, import, wait).
    /// Three shapes, each over a fence in flight and over one landed: an
    /// import, a transfer whose source has nothing submitted (software
    /// link) and a transfer whose source has its own fence in flight (which
    /// moves onto the destination, alone). A timeline transfer ADDS to the
    /// chain and keeps the binary fence; a slot already driven past binary
    /// range is not a slot (as for a stray binary signal).
    #[test]
    fn an_import_or_a_binary_transfer_replaces_the_fence_the_object_was_carrying() {
        let _g = test_lock();
        arm_hooks();
        for (shape, landed_first) in [
            ("import", false),
            ("transfer-link", false),
            ("transfer-hw", false),
            ("import", true),
            ("transfer-link", true),
            ("transfer-hw", true),
        ] {
            let sem = create(false);
            let mut old = Landing::new();
            assert!(attach_hw_fence(sem, 1, old.va(), 0, 7, 3, true));
            if landed_first {
                old.land(7);
                assert_eq!(query(sem), Some(1), "{}: signaled, and seen", shape);
            }
            let src = create(false);
            let mut theirs = Landing::new();
            match shape {
                "import" => assert!(import_snapshot(sem, src, 1)),
                "transfer-link" => assert!(transfer(sem, 0, src, 0)),
                _ => {
                    assert!(attach_hw_fence(src, 1, theirs.va(), 0, 9, 4, true));
                    assert!(transfer(sem, 0, src, 0));
                }
            }
            // (`attach_hw_fence` itself notifies the submitted point, so the
            // eventfd check below starts counting here.)
            let notified = signals().len();
            let fences = pending_hw_fences(sem, 1);
            assert!(
                !fences.iter().any(|f| f.0 == old.va()),
                "{}: EXEC must not acquire the fence the import replaced: {:?}",
                shape,
                fences
            );
            if shape == "transfer-hw" {
                assert_eq!(
                    fences,
                    alloc::vec![(theirs.va(), 0u64, 9u32, 4u32)],
                    "{}: the source's fence, alone",
                    shape
                );
            }
            old.land(7);
            assert_eq!(
                query(sem),
                Some(0),
                "{}: the replaced fence landed ({}), the object waits for its source",
                shape,
                if landed_first { "before" } else { "after" }
            );
            assert!(
                matches!(
                    wait_ready(&[sem], None, true, 0),
                    Some(Err(WaitOutcome::Timeout))
                ),
                "{}: a waiter must not go through",
                shape
            );
            assert!(
                signals()[notified..].iter().all(|&(h, _)| h != sem),
                "{}: no eventfd for the replaced fence",
                shape
            );
            if shape == "transfer-hw" {
                theirs.land(9);
            } else {
                assert!(signal(src));
            }
            assert_eq!(query(sem), Some(1), "{}: reached through the source", shape);
            destroy(sem);
            destroy(src);
        }
        // A fence given up on leaves its timeout mark; the import replaces
        // that too, so the point the source then delivers is a clean one.
        let sem = create(false);
        let dead = Landing::new();
        assert!(attach_hw_fence(sem, 1, dead.va(), 0, 7, 3, true));
        test_clock::advance(FENCE_TIMEOUT_US + 1);
        assert!(reached_by_timeout(&[sem], None));
        let src = create(false);
        assert!(import_snapshot(sem, src, 1));
        assert_eq!(query(sem), Some(0));
        assert!(signal(src));
        assert_eq!(query(sem), Some(1));
        assert!(
            !reached_by_timeout(&[sem], None),
            "the dead fence went with the import"
        );
        destroy(sem);
        destroy(src);
        // A timeline transfer adds a point to the chain: the binary fence in
        // flight on the destination stays.
        let tl = create(false);
        let mut own = Landing::new();
        assert!(attach_hw_fence(tl, 1, own.va(), 0, 7, 3, true));
        let src = create(false);
        assert!(transfer(tl, 5, src, 0));
        assert_eq!(
            pending_hw_fences(tl, 1),
            alloc::vec![(own.va(), 0u64, 7u32, 3u32)],
            "a transfer at point 5 replaces nothing at point 1"
        );
        own.land(7);
        assert_eq!(query(tl), Some(1));
        // A handle already driven past binary range is not a slot: an import
        // does not rewind it (as a stray binary signal does not).
        assert!(timeline_signal(tl, 9));
        assert!(import_snapshot(tl, src, 1));
        assert_eq!(query(tl), Some(9), "a timeline keeps its point");
        assert!(transfer(tl, 0, src, 0));
        assert_eq!(query(tl), Some(9), "through a binary transfer too");
        destroy(tl);
        destroy(src);
    }

    /// Objects still carrying a link (an import or transfer not yet
    /// materialised into their own counter).
    #[cfg(test)]
    fn links_now() -> usize {
        TABLE
            .lock()
            .objects
            .iter()
            .filter(|o| !o.links.is_empty())
            .count()
    }

    /// `SYNCOBJ_RESET` empties the object it names, and nothing else: an
    /// importer that already received the fence keeps it, whether the
    /// landing was seen before the reset or not.
    #[test]
    fn resetting_a_source_does_not_unsignal_the_fence_it_already_delivered() {
        let _g = test_lock();
        arm_hooks();
        for seen_before_reset in [true, false] {
            let src = create(false);
            let mut landing = Landing::new();
            assert!(attach_hw_fence(src, 1, landing.va(), 0, 1, 0, true));
            let dst = create(false);
            assert!(import_snapshot(dst, src, export_snapshot(src).unwrap()));
            landing.land(1);
            if seen_before_reset {
                assert_eq!(query(dst), Some(1));
            }
            assert!(reset(src));
            assert_eq!(query(src), Some(0), "the reset object carries nothing");
            assert_eq!(
                query(dst),
                Some(1),
                "the importer keeps the fence (landing seen first: {})",
                seen_before_reset
            );
            destroy(dst);
            destroy(src);
        }
    }

    /// The X11 acquire path: the acquire semaphore's sync_file is a MERGE of
    /// the compositor's release and the previous present, and both sources
    /// are re-armed every frame. Once merged and signaled it must stay so,
    /// or the acquire waits for a release that already happened.
    #[test]
    fn a_merged_fence_stays_signaled_when_its_sources_are_rearmed() {
        let _g = test_lock();
        arm_hooks();
        let release = create(false);
        let mut first = Landing::new();
        assert!(attach_hw_fence(release, 1, first.va(), 0, 1, 0, true));
        let present = create(false);
        assert!(timeline_signal(present, 6));
        let m = merge_fences(&[(release, 1), (present, 6)]);
        assert_eq!(query(m), Some(0), "the release is still in flight");
        first.land(1);
        assert_eq!(query(m), Some(1), "both sources reached");
        // Next frame: the release slot is re-armed and the present timeline
        // is reset for reuse.
        let mut second = Landing::new();
        assert!(attach_hw_fence(release, 1, second.va(), 0, 2, 0, true));
        assert!(reset(present));
        assert_eq!(query(release), Some(0));
        assert_eq!(query(present), Some(0));
        assert_eq!(
            query(m),
            Some(1),
            "the merged fence signaled once, and that is for good"
        );
        assert!(matches!(
            wait(&[m], None, true, 0),
            WaitOutcome::Signaled { .. }
        ));
        second.land(2);
        destroy(m);
        destroy(present);
        destroy(release);
    }

    /// A chain of links (an import of a transfer of a timeline point)
    /// collapses into plain counters in one resolve once its root is
    /// reached: no object keeps a link to re-derive, and the destination
    /// reached the point the transfer asked for, not just 1.
    #[test]
    fn a_chain_of_links_collapses_into_counters_once_its_root_is_reached() {
        let _g = test_lock();
        arm_hooks();
        let root = create(false);
        let mid = create(false);
        assert!(transfer(mid, 9, root, 3), "deferred: root is at 0");
        let leaf = create(false);
        assert!(import_snapshot(leaf, mid, 9));
        let links = links_now();
        assert!(links >= 2, "two links recorded, got {}", links);
        assert!(timeline_signal(root, 3));
        assert_eq!(links_now(), links - 2, "both links materialised");
        assert_eq!(query(mid), Some(9), "the transfer's own point, kept");
        assert_eq!(query(leaf), Some(1));
        assert!(
            signals().contains(&(mid, 9)) && signals().contains(&(leaf, 1)),
            "each materialisation is announced for its eventfd waiters, got {:?}",
            signals()
        );
        // The root can now do anything without touching them.
        assert!(reset(root));
        assert_eq!(query(mid), Some(9));
        assert_eq!(query(leaf), Some(1));
        destroy(leaf);
        destroy(mid);
        destroy(root);
    }

    /// An orphan (a destroyed source something still links to) is let go as
    /// soon as the link that named it materialises, not only when that link
    /// is replaced by a direct signal.
    #[test]
    fn a_materialised_link_lets_its_orphaned_source_go() {
        let _g = test_lock();
        let src = create(false);
        let mut landing = Landing::new();
        assert!(attach_hw_fence(src, 1, landing.va(), 0, 1, 0, true));
        let dst = create(false);
        assert!(import_snapshot(dst, src, 1));
        let objects = object_count();
        assert!(destroy(src));
        assert_eq!(
            object_count(),
            objects,
            "kept as an orphan while dst links to it"
        );
        landing.land(1);
        assert_eq!(query(dst), Some(1));
        assert_eq!(
            object_count(),
            objects - 1,
            "dst holds its point now: nothing names the orphan"
        );
        assert_eq!(links_now(), 0);
        destroy(dst);
    }

    /// EXEC asks [`pending_hw_fence`] for the hardware fence behind each
    /// wait so it can emit a GPU ACQUIRE instead of parking the ioctl on the
    /// CPU. The acquire semaphore of an X11 swapchain image is always an
    /// import or a merge, whose fence sits in the SOURCE's row: looked up by
    /// the importer's handle it came back empty, so every such wait spun on
    /// the CPU inside EXEC. Seen through the link, it is the source's fence.
    #[test]
    fn an_exec_wait_on_an_importer_finds_the_fence_its_source_waits_for() {
        let _g = test_lock();
        arm_hooks();
        let src = create(false);
        let mut zone = Landing::new();
        assert!(attach_hw_fence(src, 1, zone.va(), 0x4000, 7, 3, true));
        let dst = create(false);
        assert!(import_snapshot(dst, src, export_snapshot(src).unwrap()));
        let fence = Some((zone.va(), 0x4000u64, 7u32, 3u32));
        assert_eq!(pending_hw_fence(dst, 1), fence, "the source's fence");
        assert_eq!(
            pending_hw_fence(dst, 0),
            fence,
            "point 0 is the binary fence"
        );
        assert_eq!(
            pending_hw_fence(dst, 2),
            None,
            "a binary import delivers point 1, nothing higher"
        );
        assert_eq!(
            query_submitted(dst),
            Some(1),
            "submitted as far as its source"
        );
        assert_eq!(query(dst), Some(0));
        // An import of an import: two links deep, same fence.
        let leaf = create(false);
        assert!(import_snapshot(leaf, dst, 1));
        assert_eq!(pending_hw_fence(leaf, 1), fence);
        // A source with nothing submitted has nothing to acquire.
        let idle = create(false);
        let waiter = create(false);
        assert!(import_snapshot(waiter, idle, 1));
        assert_eq!(pending_hw_fence(waiter, 1), None);
        assert_eq!(query_submitted(waiter), Some(0));
        zone.land(7);
        assert_eq!(query(leaf), Some(1));
        assert_eq!(pending_hw_fence(dst, 1), None, "landed: nothing in flight");
        for h in [leaf, dst, src, waiter, idle] {
            assert!(destroy(h));
        }
    }

    /// A transfer recorded before its source was submitted is a link; once
    /// the source's work is in flight, the link shows its fences, and only
    /// up to the point the transfer promised: among several fences on the
    /// source, the lowest one that covers the transferred point and every
    /// one below it (the chain), never the ones above.
    #[test]
    fn a_pending_transfer_shows_its_source_fence_up_to_the_point_it_promises() {
        let _g = test_lock();
        arm_hooks();
        let src = create(false);
        let dst = create(false);
        assert!(
            transfer(dst, 6, src, 4),
            "deferred: nothing submitted on src"
        );
        assert_eq!(pending_hw_fence(dst, 6), None);
        assert!(matches!(
            wait_available(&[dst], Some(&[6]), true, 0),
            WaitOutcome::Timeout
        ));
        assert_eq!(query_submitted(dst), Some(0));
        let mut low = Landing::new();
        let mut high = Landing::new();
        let top = Landing::new();
        assert!(attach_hw_fence(src, 2, low.va(), 0, 5, 1, false));
        assert!(attach_hw_fence(src, 4, high.va(), 0, 11, 2, false));
        assert!(attach_hw_fence(src, 6, top.va(), 0, 13, 2, false));
        let chain = alloc::vec![(low.va(), 0u64, 5u32, 1u32), (high.va(), 0, 11, 2)];
        assert_eq!(
            pending_hw_fences(dst, 6),
            chain,
            "the fence that reaches 4, and the one below it"
        );
        assert_eq!(
            pending_hw_fences(dst, 3),
            chain,
            "any point the transfer covers"
        );
        assert_eq!(pending_hw_fence(dst, 7), None, "past what it promises");
        assert!(matches!(
            wait_available(&[dst], Some(&[6]), true, 0),
            WaitOutcome::Signaled { .. }
        ));
        assert!(matches!(
            wait_available_ready(&[dst], Some(&[6]), true, 0),
            Some(Ok(0))
        ));
        assert!(matches!(
            wait_available(&[dst], Some(&[7]), true, 0),
            WaitOutcome::Timeout
        ));
        assert!(matches!(
            wait_available_ready(&[dst], Some(&[7]), true, 0),
            Some(Err(WaitOutcome::Timeout))
        ));
        assert_eq!(query_submitted(dst), Some(6));
        assert_eq!(query(dst), Some(0), "available is not signaled");
        assert_eq!(export_snapshot(dst), Some(6));
        high.land(11);
        assert_eq!(query(dst), Some(0), "4 landed out of order: it waits for 2");
        low.land(5);
        assert_eq!(query(dst), Some(6));
        assert!(destroy(dst));
        assert!(destroy(src));
        assert_eq!(pending_now(), 0, "src's fence at 6 went with it");
    }

    /// A merged fence is submitted once EVERY source is, and is one GPU
    /// ACQUIRE per source still in flight: with both halves running, the
    /// two fences, in the link's order; once one lands, the other alone.
    #[test]
    fn a_merge_is_available_once_every_source_is_submitted_and_acquires_every_fence_in_flight() {
        let _g = test_lock();
        arm_hooks();
        let a = create(false);
        let b = create(false);
        let mut za = Landing::new();
        let mut zb = Landing::new();
        let m = merge_fences(&[(a, 1), (b, 1)]);
        assert!(matches!(
            wait_available(&[m], None, true, 0),
            WaitOutcome::Timeout
        ));
        assert_eq!(pending_hw_fence(m, 1), None);
        assert!(attach_hw_fence(a, 1, za.va(), 0, 1, 0, true));
        assert!(
            matches!(wait_available(&[m], None, true, 0), WaitOutcome::Timeout),
            "one source not submitted"
        );
        assert_eq!(pending_hw_fence(m, 1), None, "b has nothing to acquire yet");
        assert!(attach_hw_fence(b, 1, zb.va(), 0, 2, 1, true));
        assert!(matches!(
            wait_available(&[m], None, true, 0),
            WaitOutcome::Signaled { .. }
        ));
        assert_eq!(query_submitted(m), Some(1));
        assert_eq!(query(m), Some(0));
        assert_eq!(
            pending_hw_fences(m, 1),
            [(za.va(), 0, 1, 0), (zb.va(), 0, 2, 1)],
            "two fences in flight are two ACQUIREs, a's then b's"
        );
        za.land(1);
        assert_eq!(
            pending_hw_fence(m, 1),
            Some((zb.va(), 0, 2, 1)),
            "a reached: b's fence is the one left"
        );
        zb.land(2);
        assert_eq!(query(m), Some(1));
        for h in [m, a, b] {
            assert!(destroy(h));
        }
    }

    /// The list follows every source of a merge to its own fence, through a
    /// source that is itself a link; a source still short of its point with
    /// nothing submitted empties it, whatever the others hold: half a wait
    /// on the GPU and half nowhere would let the submit run early.
    #[test]
    fn a_merge_lists_the_fence_behind_each_source_or_nothing_when_one_has_none() {
        let _g = test_lock();
        arm_hooks();
        let a = create(false);
        let b = create(false);
        let c = create(false);
        let mut za = Landing::new();
        let mut zb = Landing::new();
        let mut zc = Landing::new();
        assert!(attach_hw_fence(a, 1, za.va(), 0x1000, 1, 1, true));
        assert!(attach_hw_fence(b, 1, zb.va(), 0x2000, 2, 2, true));
        // Mesa's surrogate: a's fence transferred into a binary of its own.
        let via_a = create(false);
        assert!(transfer(via_a, 0, a, 0));
        // An importer of b: a link, not a fence.
        let via_b = create(false);
        assert!(import_snapshot(via_b, b, 1));
        let m = merge_fences(&[(via_a, 1), (via_b, 1), (c, 1)]);
        assert_eq!(
            pending_hw_fences(m, 1),
            [],
            "c has nothing submitted: nothing to acquire"
        );
        assert!(attach_hw_fence(c, 1, zc.va(), 0x3000, 3, 1, true));
        assert_eq!(
            pending_hw_fences(m, 1),
            [
                (za.va(), 0x1000, 1, 1),
                (zb.va(), 0x2000, 2, 2),
                (zc.va(), 0x3000, 3, 1)
            ],
            "one fence per source, in the link's order, through the surrogate and the import"
        );
        assert_eq!(
            pending_hw_fences(m, 2),
            [],
            "a binary merge promises point 1 only"
        );
        zb.land(2);
        assert_eq!(
            pending_hw_fences(m, 1),
            [(za.va(), 0x1000, 1, 1), (zc.va(), 0x3000, 3, 1)],
            "b reached: its fence leaves the list"
        );
        assert!(destroy(via_a));
        assert!(destroy(via_b));
        assert_eq!(
            pending_hw_fences(m, 1),
            [(za.va(), 0x1000, 1, 1), (zc.va(), 0x3000, 3, 1)],
            "the surrogates gone, as Mesa leaves them: the fences are still theirs"
        );
        za.land(1);
        zc.land(3);
        assert_eq!(query(m), Some(1));
        assert_eq!(pending_hw_fences(m, 1), []);
        for h in [m, a, b, c] {
            assert!(destroy(h));
        }
        assert_eq!(pending_now(), 0);
    }

    /// A merge is ordered for a channel only when EVERY fence behind it is
    /// that channel's: two of its own, yes; one of its own and the
    /// compositor's, a wait from either side.
    #[test]
    fn a_merge_is_ordered_only_when_every_fence_behind_it_is_the_channels_own() {
        let _g = test_lock();
        arm_hooks();
        let a = create(false);
        let b = create(false);
        let c = create(false);
        let mut za = Landing::new();
        let mut zb = Landing::new();
        let mut zc = Landing::new();
        assert!(attach_hw_fence(a, 1, za.va(), 0, 1, 1, true));
        assert!(attach_hw_fence(b, 1, zb.va(), 0, 2, 1, true));
        assert!(attach_hw_fence(c, 1, zc.va(), 0, 3, 2, true));
        let ours = merge_fences(&[(a, 1), (b, 1)]);
        let mixed = merge_fences(&[(a, 1), (c, 1)]);
        assert!(matches!(
            wait_ordered(&[ours], None, 0, 1),
            WaitOutcome::Signaled { .. }
        ));
        assert!(matches!(
            wait_ordered(&[ours], None, 0, 2),
            WaitOutcome::Timeout
        ));
        assert!(matches!(
            wait_ordered(&[mixed], None, 0, 1),
            WaitOutcome::Timeout
        ));
        assert!(matches!(
            wait_ordered(&[mixed], None, 0, 2),
            WaitOutcome::Timeout
        ));
        zc.land(3);
        assert!(
            matches!(
                wait_ordered(&[mixed], None, 0, 1),
                WaitOutcome::Signaled { .. }
            ),
            "the compositor's half landed: what is left is ours"
        );
        assert_eq!(query(mixed), Some(0), "ordered is not signaled");
        za.land(1);
        zb.land(2);
        for h in [ours, mixed, a, b, c] {
            assert!(destroy(h));
        }
        assert_eq!(pending_now(), 0);
    }

    /// The in-order guarantee of a channel holds through a link as well: a
    /// submit on the channel whose fence an importer waits for cannot
    /// overtake it.
    #[test]
    fn a_wait_on_its_own_channel_is_ordered_through_a_link_too() {
        let _g = test_lock();
        arm_hooks();
        let src = create(false);
        let zone = Landing::new();
        assert!(attach_hw_fence(src, 1, zone.va(), 0, 1, 5, true));
        let dst = create(false);
        assert!(import_snapshot(dst, src, 1));
        assert!(matches!(
            wait_ordered(&[dst], None, 0, 5),
            WaitOutcome::Signaled { .. }
        ));
        assert!(matches!(
            wait_ordered(&[dst], None, 0, 6),
            WaitOutcome::Timeout
        ));
        assert!(
            matches!(wait(&[dst], None, true, 0), WaitOutcome::Timeout),
            "ordered is not signaled"
        );
        assert_eq!(query(dst), Some(0));
        assert!(destroy(dst));
        assert!(destroy(src));
    }

    /// `EXPORT_SYNC_FILE` takes the fence attached LAST. On a timeline that
    /// still carries a transfer, that is the point the transfer promised,
    /// not the counter: exporting the counter made the importer signaled
    /// before the transferred work had run.
    #[test]
    fn exporting_a_timeline_with_a_transfer_pending_names_the_point_it_promises() {
        let _g = test_lock();
        arm_hooks();
        let src = create(false);
        let t = create(false);
        assert!(timeline_signal(t, 3));
        assert!(transfer(t, 5, src, 2), "deferred: src is at 0");
        assert_eq!(query(t), Some(3));
        assert_eq!(export_snapshot(t), Some(5));
        let imp = create(false);
        assert!(import_snapshot(imp, t, export_snapshot(t).unwrap()));
        assert_eq!(query(imp), Some(0), "not before the transfer lands");
        assert!(matches!(wait(&[imp], None, true, 0), WaitOutcome::Timeout));
        assert!(timeline_signal(src, 2));
        assert_eq!(query(t), Some(5));
        assert_eq!(query(imp), Some(1));
        for h in [imp, t, src] {
            assert!(destroy(h));
        }
    }

    /// Two syncobjs importing each other can never resolve; looking through
    /// their links must give up, not recurse until the stack runs out.
    #[test]
    fn a_cycle_of_links_is_neither_acquirable_nor_available() {
        let _g = test_lock();
        arm_hooks();
        let a = create(false);
        let b = create(false);
        assert!(import_snapshot(a, b, 1));
        assert!(import_snapshot(b, a, 1));
        assert_eq!(pending_hw_fence(a, 1), None);
        assert!(matches!(
            wait_available(&[a, b], None, true, 0),
            WaitOutcome::Timeout
        ));
        assert!(matches!(
            wait_available_ready(&[a], None, true, 0),
            Some(Err(WaitOutcome::Timeout))
        ));
        assert_eq!(query_submitted(a), Some(0));
        assert_eq!(export_snapshot(a), Some(1));
        assert!(matches!(
            wait_ordered(&[a], None, 0, 0),
            WaitOutcome::Timeout
        ));
        // A direct signal breaks the cycle: b's link is then reached.
        assert!(signal(a));
        assert_eq!(query(b), Some(1));
        assert_eq!(links_now(), 0);
        assert!(destroy(a));
        assert!(destroy(b));
    }

    /// `SYNCOBJ_TRANSFER` to a timeline point ADDS a node to the chain
    /// (`drm_syncobj_add_point`): the point counts once its own source and
    /// every node before it have -- and a transfer FROM point 0 takes the
    /// chain the source carries now (`drm_syncobj_find_fence` at 0), not its
    /// point 1. The pair is Mesa's many-to-many
    /// `vk_drm_syncobj_transfer_payloads` (a `vkQueueSubmit` that only
    /// forwards N waits into M signals: NVK's `copy_sync_payloads`): each
    /// wait goes into a temporary timeline at points 1..N, then the
    /// temporary goes at point 0 into every signal, "relying on waiting on
    /// a whole chain waiting on everything". With one link per object,
    /// point 2 threw away what point 1 waited on and point 0 read as point
    /// 1, so every signal semaphore followed ONE wait of N, whichever
    /// order they came in: a semaphore signaled before its producer had
    /// finished, and a buffer reused under the GPU.
    #[test]
    fn a_transfer_to_a_timeline_point_waits_for_the_nodes_before_it_like_a_chain() {
        let _g = test_lock();
        arm_hooks();
        let reached = |h: u32, p: u64| matches!(wait_ready(&[h], Some(&[p]), true, 0), Some(Ok(_)));

        // Mesa's shape, both binary waits still pending when forwarded,
        // signaled in either order.
        for first in 0..2usize {
            let w = [create(false), create(false)];
            let tmp = create(false);
            let (s_bin, s_tl) = (create(false), create(false));
            assert!(transfer(tmp, 1, w[0], 0));
            assert!(transfer(tmp, 2, w[1], 0));
            assert!(transfer(s_bin, 0, tmp, 0));
            assert!(transfer(s_tl, 7, tmp, 0));
            assert!(
                !reached(s_bin, 1) && !reached(s_tl, 7),
                "nothing signaled yet"
            );
            assert!(signal(w[first]));
            assert_eq!(
                query(tmp),
                Some(if first == 0 { 1 } else { 0 }),
                "point 1 is w0's alone; point 2 needs both (w{first} signaled first)"
            );
            assert!(
                !reached(s_bin, 1),
                "one wait of two is not the chain (w{} first)",
                first
            );
            assert!(!reached(s_tl, 7));
            assert!(signal(w[1 - first]));
            assert_eq!(query(tmp), Some(2));
            assert!(reached(s_bin, 1) && reached(s_tl, 7));
            assert_eq!(query(s_tl), Some(7));
            for h in [w[0], w[1], tmp, s_bin, s_tl] {
                assert!(destroy(h));
            }
        }

        // A wait already done when forwarded: the temporary reaches 1 at
        // once, and point 0 is still the whole chain, not that 1.
        {
            let (w0, w1, tmp, s) = (create(true), create(false), create(false), create(false));
            assert!(transfer(tmp, 1, w0, 0));
            assert_eq!(query(tmp), Some(1));
            assert!(transfer(tmp, 2, w1, 0));
            assert!(transfer(s, 0, tmp, 0));
            assert!(
                !reached(s, 1),
                "point 0 of the temporary is point 2, still pending"
            );
            assert!(signal(w1));
            assert!(reached(s, 1));
            for h in [w0, w1, tmp, s] {
                assert!(destroy(h));
            }
        }

        // Both waits in flight on two channels: EXEC behind the signal sees
        // both fences (one ACQUIRE each), and the signal counts only once
        // both have landed, whichever lands first.
        {
            let (w0, w1, tmp, s) = (create(false), create(false), create(false), create(false));
            let (mut l0, mut l1) = (Landing::new(), Landing::new());
            assert!(attach_hw_fence(w0, 1, l0.va(), 0, 1, 0, true));
            assert!(attach_hw_fence(w1, 1, l1.va(), 0, 1, 1, true));
            assert!(transfer(tmp, 1, w0, 0));
            assert!(transfer(tmp, 2, w1, 0));
            assert!(transfer(s, 0, tmp, 0));
            let vas: Vec<usize> = pending_hw_fences(s, 1).into_iter().map(|f| f.0).collect();
            assert_eq!(vas.len(), 2, "both fences of the chain: {:?}", vas);
            assert!(vas.contains(&l0.va()) && vas.contains(&l1.va()));
            l1.land(1);
            assert!(
                !reached(s, 1),
                "w1 landed first: w0 is still the node before it"
            );
            assert_eq!(query(tmp), Some(0));
            l0.land(1);
            assert!(reached(s, 1));
            assert_eq!(query(tmp), Some(2));
            for h in [w0, w1, tmp, s] {
                assert!(destroy(h));
            }
        }

        // The nodes below stay independent: point 1 is reached when its own
        // source is, whatever point 2 still waits on (no over-sync).
        {
            let (a, b, t) = (create(false), create(false), create(false));
            assert!(transfer(t, 1, a, 0));
            assert!(transfer(t, 2, b, 0));
            assert!(signal(a));
            assert_eq!(query(t), Some(1));
            assert!(reached(t, 1) && !reached(t, 2));
            assert!(signal(b));
            assert_eq!(query(t), Some(2));
            for h in [a, b, t] {
                assert!(destroy(h));
            }
        }

        // A transfer already reached at a point above a node still waiting
        // queues behind it too: behind a link (a wait done, forwarded after
        // one that is not) and behind the object's own fence in flight (an
        // EXEC signal at 3, then a done wait forwarded at 5).
        {
            let (a, b, t) = (create(false), create(true), create(false));
            assert!(transfer(t, 1, a, 0));
            assert!(transfer(t, 2, b, 0));
            assert_eq!(
                query(t),
                Some(0),
                "2 is done, 1 is not: the chain stops at 1"
            );
            assert!(!reached(t, 2));
            assert!(signal(a));
            assert_eq!(query(t), Some(2));
            let u = create(false);
            let mut land = Landing::new();
            assert!(attach_hw_fence(u, 3, land.va(), 0, 1, 0, false));
            assert!(transfer(u, 5, b, 0));
            assert_eq!(query(u), Some(0), "5 is done, 3 has not landed");
            assert!(!reached(u, 5));
            land.land(1);
            assert_eq!(query(u), Some(5));
            for h in [a, b, t, u] {
                assert!(destroy(h));
            }
        }

        // A hardware fence landing at a point above a node still waiting
        // queues behind it (EXEC signals 4 while a transfer at 3 is
        // pending), and a CPU signal below a pending transfer leaves the
        // transfer waiting instead of dropping it.
        {
            let (w, t) = (create(false), create(false));
            assert!(transfer(t, 3, w, 0));
            let mut land = Landing::new();
            assert!(attach_hw_fence(t, 4, land.va(), 0, 1, 0, false));
            land.land(1);
            assert_eq!(poll_pending(), 1, "landed, and held behind the node at 3");
            assert_eq!(
                query(t),
                Some(0),
                "4 landed, 3 has not: the chain stops at 3"
            );
            assert!(!reached(t, 3) && !reached(t, 4));
            assert!(signal(w));
            assert_eq!(query(t), Some(4));
            assert_eq!(pending_now(), 0);
            let (a, u) = (create(false), create(false));
            assert!(transfer(u, 5, a, 0));
            assert!(timeline_signal(u, 3));
            assert_eq!(query(u), Some(3));
            assert!(signal(a));
            assert_eq!(
                query(u),
                Some(5),
                "the transfer to 5 survived the signal to 3"
            );
            for h in [w, t, a, u] {
                assert!(destroy(h));
            }
        }
        assert_eq!(links_now(), 0);
    }

    /// Linux finds a syncobj handle in the calling file's table and nowhere
    /// else: another process's handle is ENOENT for every syncobj ioctl.
    /// Here the handle space is global, and with DESTROY the only ioctl
    /// that asked who held the handle, a client guessing the compositor's
    /// (consecutive) handle numbers could signal its release timelines,
    /// reset its acquire fences or wait on them. `usable_by` is the one
    /// answer every arm now asks for, holders standing in for the file.
    #[test]
    fn a_syncobj_is_named_only_by_the_processes_that_hold_it() {
        let _g = test_lock();
        arm_hooks();
        const A: u64 = 92_001;
        const B: u64 = 92_002;
        let a1 = create_for(A, false);
        let b1 = create_for(B, true);
        let kernel = create(false);
        assert!(usable_by(A, a1) && usable_by(B, b1));
        assert!(!usable_by(B, a1), "A's is not B's");
        assert!(!usable_by(A, b1), "B's is not A's");
        assert!(
            usable_by(A, kernel) && usable_by(B, kernel),
            "no file owns a handle nobody holds"
        );
        assert!(
            usable_by(0, a1) && usable_by(0, b1) && usable_by(0, kernel),
            "the kernel itself names any live handle"
        );
        assert!(!usable_by(A, 0xdead_0007) && !usable_by(0, 0xdead_0007));
        // An import (an opaque FD_TO_HANDLE) makes B a holder of a1.
        assert!(add_ref_for(B, a1));
        assert!(usable_by(B, a1) && usable_by(A, a1));
        // The array form is all-or-nothing, like `drm_syncobj_array_find`.
        assert!(all_usable_by(A, &[a1, kernel]));
        assert!(!all_usable_by(A, &[a1, b1]), "one of them is B's alone");
        assert!(!all_usable_by(A, &[a1, 0xdead_0008]));
        assert!(all_usable_by(A, &[]), "nothing to refuse");
        // B gives its reference back: a1 is A's alone again.
        assert!(destroy_for(B, a1));
        assert!(!usable_by(B, a1) && usable_by(A, a1));
        // A destroyed handle is nobody's, not even the kernel's.
        assert!(destroy_for(A, a1));
        assert!(!usable_by(A, a1) && !usable_by(0, a1));
        assert!(destroy_for(B, b1) && destroy(kernel));
    }

    /// A syncobj handle is a `drm_file` thing in Linux: `SYNCOBJ_DESTROY`
    /// frees the caller's reference and nobody else's, and the file's close
    /// (`drm_syncobj_release`) gives back every handle the process still
    /// had. Here the handle space is global and, before this, `destroy` took
    /// whichever reference was there, so a client guessing the compositor's
    /// handle numbers could free its timelines, and a client that crashed
    /// mid-frame left its syncobjs (and any fence in flight on them) in the
    /// table for the rest of the boot.
    #[test]
    fn a_process_that_dies_gives_back_its_syncobjs_and_only_its_own() {
        let _g = test_lock();
        arm_hooks();
        const A: u64 = 91_001;
        const B: u64 = 91_002;
        let live = || TABLE.lock().objects.iter().filter(|o| o.refs > 0).count();
        let before = live();
        let a1 = create_for(A, false);
        let a2 = create_for(A, true);
        let b1 = create_for(B, false);
        let kernel = create(false);
        assert!(held_by(A, a1) && held_by(A, a2) && held_by(B, b1));
        assert!(!held_by(A, b1) && !held_by(B, a1) && !held_by(A, kernel));
        // B imports a1 (an opaque FD_TO_HANDLE): a reference of its own.
        assert!(add_ref_for(B, a1));
        assert!(held_by(B, a1) && held_by(A, a1));
        assert!(!add_ref_for(B, 0xdead_0001), "unknown handle");
        // A imports its own export of a2: two references of A's on it, and
        // its death gives back both.
        assert!(add_ref_for(A, a2));
        // DESTROY from the wrong process is ENOENT and changes nothing.
        assert!(!destroy_for(B, a2), "a2 is A's alone");
        assert!(!destroy_for(A, b1), "b1 is B's alone");
        assert!(!destroy_for(A, kernel), "no process holds the kernel's");
        assert!(exists(a2) && exists(b1) && exists(kernel));
        assert_eq!(query(a2), Some(1));
        // A's fence in flight on a1: B's EXEC acquires it, so it must
        // survive A's death (B holds the object) and still land.
        let mut landing = Landing::new();
        assert!(attach_hw_fence(a1, 1, landing.va(), 0, 7, 0, true));
        assert_eq!(pending_now(), 1);
        // A dies: a1 (its reference; B keeps the object) and a2 (both
        // references, so freed).
        assert_eq!(release_owner(A), 3);
        assert!(exists(a1), "B still holds a1");
        assert!(!held_by(A, a1) && held_by(B, a1));
        assert!(!exists(a2), "nobody held a2 but A");
        assert!(exists(b1) && exists(kernel));
        assert_eq!(pending_now(), 1, "the fence B waits for is still armed");
        assert_eq!(release_owner(A), 0, "a second teardown finds nothing");
        // (The arm itself already announced the fence to EXEC waiters.)
        let announced = signals().iter().filter(|&&s| s == (a1, 1)).count();
        landing.land(7);
        assert_eq!(query(a1), Some(1), "and it lands for B");
        assert_eq!(
            signals().iter().filter(|&&s| s == (a1, 1)).count(),
            announced + 1,
            "the landing is delivered"
        );
        // B's own DESTROY of the imported reference is the last one: gone.
        assert!(destroy_for(B, a1));
        assert!(!exists(a1));
        assert!(!destroy_for(B, a1), "already gone");
        // A fence in flight on a syncobj nobody else holds goes with its
        // process (no waiter, no WEDGE from the timeout hook later).
        let mut orphan = Landing::new();
        assert!(attach_hw_fence(b1, 1, orphan.va(), 0, 3, 0, true));
        assert_eq!(pending_now(), 1);
        assert_eq!(release_owner(B), 1);
        assert!(!exists(b1));
        assert_eq!(pending_now(), 0);
        let announced = signals().iter().filter(|&&s| s == (b1, 1)).count();
        orphan.land(3);
        assert_eq!(poll_pending(), 0);
        assert_eq!(
            signals().iter().filter(|&&s| s == (b1, 1)).count(),
            announced,
            "nothing is delivered for a dead process's fence"
        );
        // pid 0 is the kernel: nothing recorded, nothing released, and
        // `destroy_for(0, ..)` is plain `destroy`.
        let k2 = create_for(0, false);
        assert!(!held_by(0, k2));
        assert!(add_ref_for(0, k2));
        assert_eq!(release_owner(0), 0);
        assert!(exists(k2) && exists(kernel));
        assert!(destroy_for(0, k2), "the extra reference");
        assert!(destroy_for(0, k2), "the last one");
        assert!(!exists(k2));
        assert!(destroy(kernel));
        assert_eq!(live(), before, "nothing of this test is left in the table");
    }
}
