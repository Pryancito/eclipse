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
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
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

/// The highest node of `obj`'s chain below `point` -- a link's destination
/// point or a hardware fence of its own in flight -- or its counter when
/// there is none: where a point delivered behind the chain starts counting
/// from, so that a tainted one marks only what it delivers itself.
fn highest_node_below(table: &SyncobjTable, obj: &Syncobj, point: u64) -> u64 {
    obj.links
        .iter()
        .map(|l| l.dst_point.max(1))
        .chain(
            table
                .pending
                .iter()
                .filter(|g| g.handle == obj.handle)
                .map(|g| g.point),
        )
        .filter(|&p| p < point)
        .fold(obj.point, u64::max)
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

/// Whether any object in the table carries a chain link right now.
///
/// [`resolve_links_locked`] is the one half of a resolve with no cheap exit of
/// its own: it asks every object whether its lowest link is ready, so a resolve
/// walks the whole object list even when the table holds no links at all. That
/// walk is free with the handful of syncobjs `glxgears` keeps alive and is not
/// free for a real Vulkan client, which holds hundreds and runs a resolve per
/// signal of an `EXEC` that may carry 256 of them: the same O(objects) walk,
/// hundreds of times, inside one ioctl, under the lock the signalling side
/// needs. Nothing in Mesa's ordinary path creates a link -- they come from
/// `SYNCOBJ_TRANSFER` and sync_file import -- so the common table has none.
///
/// Deliberately STICKY, and that is what makes it safe: every site that adds a
/// link sets it, and only [`resolve_links_locked`] clears it, after a full walk
/// has confirmed no object holds one. So it can be true with no links left --
/// which costs exactly the walk this kernel already did -- and it can never be
/// false while a link exists, which is the only way it could change an answer.
/// An object dropped by `collect_orphans` while still holding links therefore
/// needs no bookkeeping here.
static ANY_LINKS: AtomicBool = AtomicBool::new(false);

/// Empties the table and the two mirrors outside it, for
/// `crate::test_globals::LOCK`: every test starts from no objects, no
/// pending hardware fence and no link.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut table = TABLE.lock();
    table.objects.clear();
    table.pending.clear();
    table.errored.clear();
    PENDING_COUNT.store(0, Ordering::Relaxed);
    ANY_LINKS.store(false, Ordering::Relaxed);
}

/// Times [`resolve_links_locked`] got PAST [`ANY_LINKS`] and walked the object
/// list. The seam sits below what the tests check -- the walk still happens,
/// it is only counted -- so a test can tell "the fast path held" from "the
/// answer happened to be the same", which is the half of this that a wrong
/// answer would not show.
#[cfg(test)]
static LINK_WALKS: AtomicUsize = AtomicUsize::new(0);

/// How many times [`resolve_locked`] ran: one per lookup that has to know
/// what landed. Test-only, the same seam as [`LINK_WALKS`]: EXEC's wait
/// validation asked once per wait (256 for a full EXEC) where one
/// resolution answers the whole list.
#[cfg(test)]
pub static RESOLVE_WALKS: AtomicUsize = AtomicUsize::new(0);

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

/// Whether EVERY fence in `fences` has landed, reading each distinct landing
/// zone at most ONCE however many of the fences sit in it.
///
/// For drivers and pre-waits that hold a LIST of fences to wait on -- the
/// render fences of the buffer a present is about to scan out, the fences on
/// a `GEM_CPU_PREP`'d object -- and take that list again on every look of a
/// poll loop. Those lists routinely name one zone twice: a channel has ONE
/// semaphore word and every submit on it lands a later payload in that same
/// word, so a buffer two submits of one ring wrote carries two fences at one
/// address. `fences.iter().all(hw_fence_landed)` read the word once per
/// fence, and the word is pinned sysmem mapped uncached, so those were two
/// trips off the CPU to learn one thing.
///
/// One read answers every payload in its zone, because the word only ever
/// grows: `all(word(va) >= p)` over a zone is `word(va) >= max(p)`. Short
/// circuits on the first fence still in flight, exactly as `all` did.
pub fn hw_fences_landed(fences: &[(usize, u32)]) -> bool {
    // What each zone said this call. Linear: these lists are a handful of
    // entries long (one per ring that wrote the buffer), and a Vec of pairs
    // beats a map at that size.
    let mut words: Vec<(usize, u32)> = Vec::with_capacity(fences.len());
    for &(fence_va, payload) in fences {
        let word = match words.iter().find(|&&(va, _)| va == fence_va) {
            Some(&(_, w)) => w,
            None => {
                let w = read_fence_word(fence_va);
                words.push((fence_va, w));
                w
            }
        };
        if !payload_reached(word, payload) {
            return false;
        }
    }
    true
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

/// The GPU's current value of the semaphore word at `fence_va`.
///
/// The one place a landing zone is read. Split out from [`fence_landed`]
/// because the word is what is expensive and the compare is free: a caller
/// holding several fences in one zone reads the word once and compares every
/// payload against it ([`hw_fences_landed`], [`fence_landed_cached`]).
#[inline]
fn read_fence_word(fence_va: usize) -> u32 {
    #[cfg(test)]
    FENCE_READS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `fence_va` is a kernel mapping of pinned sysmem published by
    // the driver for exactly this read (`attach_hw_fence`'s contract).
    unsafe { core::ptr::read_volatile(fence_va as *const u32) }
}

/// Whether a semaphore word of `word` has reached `payload`.
///
/// Wrapping: the word is a u32 counter that runs past its top and keeps
/// going, so the compare is a signed difference, not `>=`.
#[inline]
fn payload_reached(word: u32, payload: u32) -> bool {
    (word.wrapping_sub(payload) as i32) >= 0
}

/// Whether the GPU has written `payload` (or a later one) into `fence_va`.
#[inline]
fn fence_landed(fence_va: usize, payload: u32) -> bool {
    payload_reached(read_fence_word(fence_va), payload)
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
    #[cfg(test)]
    RESOLVE_WALKS.fetch_add(1, Ordering::Relaxed);
    let mut out = Deferred::default();
    // What each landing zone READ the first time this call looked at it, so
    // neither the passes below nor two fences sharing a zone ask the same
    // address again.
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
    //
    // What is remembered is the WORD, not a landed/not-landed verdict, and
    // that is the difference between one read per fence and one read per
    // zone: a channel has one semaphore word and every submit on it lands a
    // later payload in that same word, so the pending list routinely holds
    // several fences at one address. The word answers all of them, because it
    // only ever grows.
    let mut seen: Vec<(usize, u32)> = Vec::new();
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
/// Keyed by the ADDRESS, and holding the word rather than a verdict, so a
/// second fence in the same zone is answered without a second read. Keying
/// the verdict by the (address, payload) pair instead -- which is what this
/// did first -- made the pending list's two fences on one channel two trips
/// off the CPU for one word. The payload still decides, it just decides
/// against a word already in hand.
fn fence_landed_cached(seen: &mut Vec<(usize, u32)>, fence_va: usize, payload: u32) -> bool {
    let word = match seen.iter().find(|&&(va, _)| va == fence_va) {
        Some(&(_, w)) => w,
        None => {
            let w = read_fence_word(fence_va);
            seen.push((fence_va, w));
            w
        }
    };
    payload_reached(word, payload)
}

/// The hardware half of [`resolve_locked`]: take every landed (or timed-out)
/// fence out of the table and advance its syncobj.
fn resolve_hw_locked(
    table: &mut SyncobjTable,
    out: &mut Deferred,
    seen: &mut Vec<(usize, u32)>,
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
    // No link anywhere in the table: there is nothing for the walk below to
    // find, and saying so costs one relaxed load instead of one pass over
    // every object. See [`ANY_LINKS`] for why a stale `true` is harmless.
    if !ANY_LINKS.load(Ordering::Relaxed) {
        // The whole safety of the fast path is one direction of the invariant:
        // a link may exist with the flag stale-true (which only costs the walk
        // this kernel already did), but never with it false. Asserting it here
        // makes every test in the tree that creates a link and then resolves a
        // check of it, which is the only way a future link-creating path that
        // forgets the store gets caught -- the four that exist today were
        // found by hand.
        //
        // `cfg(test)` and NOT `debug_assert!`: this runs holding `TABLE`, and a
        // panic inside a critical section of this kernel turns into a deadlock
        // rather than a message, so it must not exist in any build that boots.
        // The host suite is where the invariant is checked.
        #[cfg(test)]
        assert!(
            !table.objects.iter().any(|o| !o.links.is_empty()),
            "ANY_LINKS is false while the table still holds a link"
        );
        return false;
    }
    #[cfg(test)]
    LINK_WALKS.fetch_add(1, Ordering::Relaxed);
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
    // The walk has just looked at every object, so this is the one place that
    // can retire the flag without paying for a pass of its own.
    if !table.objects.iter().any(|o| !o.links.is_empty()) {
        ANY_LINKS.store(false, Ordering::Relaxed);
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
/// `pid` holds on `handle`. `false` (EINVAL, `drm_syncobj_destroy`'s
/// `idr_remove` finding nothing) when it holds none, even if the
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
    first_unusable_by(pid, handles.iter().copied()).is_none()
}

/// [`all_usable_by`], but naming the handle that failed, in ONE take of the
/// lock.
///
/// The driver's submit paths want both halves: the whole-list check Linux does
/// before a job is armed, and the offending handle for the line it logs. Doing
/// that with a `find` over per-handle [`usable_by`] -- which is what `EXEC` and
/// `VM_BIND` did -- takes the global table lock once per handle and scans the
/// object list once per handle. An `EXEC` may carry 256 waits and 256 signals
/// (`NVKMD_NOUVEAU_MAX_SYNCS`), and `VM_BIND` coalesces up to 4096 ops with
/// syncs of its own, so a real Vulkan client paid 512 round-trips through the
/// lock the signalling side is waiting for, per submit, before anything was
/// submitted at all. `glxgears` carries one or two and never showed it.
pub fn first_unusable_by(pid: u64, handles: impl IntoIterator<Item = u32>) -> Option<u32> {
    let table = TABLE.lock();
    handles.into_iter().find(|&h| {
        !table.objects.iter().any(|o| {
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

/// Whether `handle` carries a fence for `target` (`drm_syncobj_find_fence`
/// with no `WAIT_FOR_SUBMIT`): its counter is there, a hardware fence for
/// the point is in flight, or a link promises it -- an imported `sync_file`
/// is a fence of its own in Linux, whatever its source has done since, so
/// a link counts whether or not the source is submitted (the difference
/// with [`submitted_locked`], which follows the link to its source).
fn has_fence_locked(table: &SyncobjTable, handle: u32, target: u64) -> bool {
    effective_point(table, handle, LINK_DEPTH).is_some_and(|p| p >= target)
        || table
            .pending
            .iter()
            .any(|f| f.handle == handle && f.point >= target)
        || table
            .objects
            .iter()
            .find(|o| o.handle == handle)
            .is_some_and(|o| o.links_to(target).is_some())
}

/// `drm_syncobj_array_wait_timeout` without `WAIT_FOR_SUBMIT` or
/// `WAIT_AVAILABLE`, and `drm_syncobj_find_fence` with flags 0 as
/// `nouveau_job_add_deps` looks an EXEC's or a VM_BIND's waits up: before
/// anything is waited for, every handle must carry a fence for its point
/// -- a binary syncobj any at all (there is none after `CREATE` without
/// `SIGNALED`, or after `RESET`), a timeline one whose chain reaches the
/// point, signaled, still in flight or imported
/// (`dma_fence_chain_find_seqno`). The index of the first handle without
/// one, which the ioctl answers with EINVAL at once; a wait that simply
/// ran to its deadline on such a handle (as this did) is ETIME or EIO,
/// the codes for "the fence exists and has not signaled".
pub fn without_fence(handles: &[u32], points: Option<&[u64]>) -> Option<usize> {
    let (r, deferred) = {
        let mut table = TABLE.lock();
        let d = resolve_locked(&mut table);
        let r = handles.iter().enumerate().find_map(|(i, &h)| {
            let target = points.map(|p| p[i]).unwrap_or(1).max(1);
            (!has_fence_locked(&table, h, target)).then_some(i)
        });
        (r, d)
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
            ANY_LINKS.store(true, Ordering::Relaxed);
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
    let (outcome, deferred) = {
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
        // What a tainted point queued behind the chain marks: the points
        // from the node below it up to itself. Marked from the counter, it
        // tainted the nodes in between too, which the GPU reaches for real
        // -- and an EXEC that waited on one of them answered EIO.
        let node_below = table
            .objects
            .iter()
            .find(|o| o.handle == dst)
            .map_or(0, |o| highest_node_below(&table, o, point));
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
        // `signaled` = dst advanced (wake waiters on the landed point).
        // `submitted` = a HW fence was attached but has not landed yet — same
        // announcement [`attach_hw_fence`] makes, so SYNCOBJ_EVENTFD /
        // sync_file pollers re-arm when waiters already sit on `dst`. Without
        // it, `export_fence` (GLX/DRI3 SYNC_FD) copied the fence onto the
        // carrier and returned with nobody watching: Mesa's `sync_wait` then
        // parked forever after the first SwapBuffers and glxgears showed a
        // frozen window with no FPS.
        let (signaled, submitted) = if reached {
            let before = obj.point;
            if behind_a_node {
                obj.push_link(Link {
                    deps: Vec::new(),
                    dst_point: point,
                });
                ANY_LINKS.store(true, Ordering::Relaxed);
                if tainted {
                    mark_errored(&mut table, dst, node_below, point);
                }
                (None, None)
            } else {
                obj.point = obj.point.max(point);
                let after = obj.point;
                if tainted {
                    mark_errored(&mut table, dst, before, after);
                }
                (Some(after), None)
            }
        } else if let Some(f) = hw {
            if point == 1 && obj.point == 1 {
                // The binary slot is replaced: signaled no more.
                obj.point = 0;
            }
            let attached = point > obj.point;
            if attached {
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
            (None, attached.then_some(point))
        } else {
            if binary && obj.point == 1 {
                // The binary slot is replaced: signaled no more.
                obj.point = 0;
            }
            obj.push_link(Link {
                deps: alloc::vec![(src, need)],
                dst_point: point,
            });
            ANY_LINKS.store(true, Ordering::Relaxed);
            (None, None)
        };
        if had_link {
            collect_orphans(&mut table);
        }
        ((signaled, submitted), d)
    };
    // Lock released. Resolve first (same order as [`attach_hw_fence`]), then
    // announce: a satisfied transfer wakes waiters on the landed point; a
    // HW-fence copy announces the *submitted* point so SYNCOBJ_EVENTFD /
    // sync_file pollers re-arm when waiters already sit on `dst`.
    deferred.run();
    let (new_point, submitted) = outcome;
    if let Some(p) = new_point {
        notify_signal(dst, p);
    } else if let Some(p) = submitted {
        notify_signal(dst, p);
    }
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
                ANY_LINKS.store(true, Ordering::Relaxed);
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

/// A hardware fence outside the table, as a driver hands it to
/// [`bundle_fence`]: the GPU has passed it once it has written `>= payload`
/// into the u32 at `fence_va` (the same word [`attach_hw_fence`] takes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwFence {
    /// Kernel VA of the fence landing zone.
    pub fence_va: usize,
    /// The same semaphore in the producer channel's GPU VA space, 0 if
    /// unknown.
    pub fence_gpu_va: u64,
    pub payload: u32,
    /// The channel the fence was issued on.
    pub ctx_idx: u32,
}

/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`: one syncobj whose point 1 is reached
/// once every hardware fence in `hw` has landed and every `(syncobj, point)`
/// of `deps` is reached -- the `dma_fence_array` that
/// `dma_resv_get_singleton` builds out of a buffer's reservation, for a
/// `sync_file` to carry. `hw` is what the driver says is still queued on the
/// buffer ([`crate::scheme::drm::DrmScheme::queued_fences`]); `deps` what
/// userspace imported onto it ([`crate::scheme::resv::fences`]). Signaled at
/// once when both are empty, or already past.
///
/// Each hardware fence gets a binary syncobj of its own, attached and then
/// destroyed: one in flight stays in the table as an orphan for as long as
/// the merge names it ([`destroy`]'s orphan rule), so nothing has to own it,
/// and one that has landed already is simply not a dependency. The one
/// reference of the merged object is the caller's to `destroy`.
pub fn bundle_fence(hw: &[HwFence], deps: &[(u32, u64)]) -> u32 {
    let mut sources: Vec<(u32, u64)> = Vec::with_capacity(hw.len() + deps.len());
    let mut own: Vec<u32> = Vec::with_capacity(hw.len());
    for f in hw {
        let carrier = create(false);
        // The handle is fresh: this cannot fail.
        attach_hw_fence(
            carrier,
            1,
            f.fence_va,
            f.fence_gpu_va,
            f.payload,
            f.ctx_idx,
            true,
        );
        own.push(carrier);
        sources.push((carrier, 1));
    }
    sources.extend_from_slice(deps);
    let merged = merge_fences(&sources);
    for carrier in own {
        destroy(carrier);
    }
    merged
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
    let Some(n) = stall_report_number(&BUDGET) else {
        return;
    };
    let list = describe(handles, points);
    crate::klog_warn!(
        "[syncobj] WAIT parked >2s and still unsignaled (wait_all={} deadline in {}s):{} (handle:target/current; report {}/{} this boot)",
        wait_all,
        remaining_us / 1_000_000,
        list,
        n + 1,
        MAX_STALL_REPORTS
    );
}

/// How many stall reports a boot may write: enough to identify the station,
/// not enough to storm the UART.
const MAX_STALL_REPORTS: u32 = 8;

/// Takes one report from `budget`: its number (0-based) while the budget
/// lasts, `None` once it is spent, from then on for the rest of the boot.
fn stall_report_number(budget: &AtomicU32) -> Option<u32> {
    let n = budget.fetch_add(1, Ordering::Relaxed);
    (n < MAX_STALL_REPORTS).then_some(n)
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
mod tests;

#[cfg(test)]
mod benches;
