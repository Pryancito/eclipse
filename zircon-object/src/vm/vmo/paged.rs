use {
    super::*,
    crate::util::block_range::BlockIter,
    alloc::collections::BTreeMap,
    alloc::collections::VecDeque,
    alloc::sync::{Arc, Weak},
    alloc::vec::Vec,
    core::cell::{Ref, RefCell, RefMut, UnsafeCell},
    core::ops::Range,
    core::sync::atomic::*,
    kernel_hal::{
        mem::{phys_to_virt, PhysFrame},
        PAGE_SIZE,
    },
    lock::{Mutex, MutexGuard},
};

// ---------------------------------------------------------------------------
// Copy-on-write tree instrumentation
// ---------------------------------------------------------------------------
//
// A copy-on-write `fork` inserts a *hidden* node above every mapping's VMO, to
// hold the frames both sides now share. When the child exits, dropping its
// snapshot collapses that node again (`remove_child`) and the tree returns to
// what it was.
//
// If it does. Measured under QEMU with 256 mappings, the FIRST fork of a
// process costs 5 826 us — indistinguishable from Linux's 10 053 us — and every
// later one costs 40 000 to 90 000 us. It does not keep growing, so nothing is
// accumulating without bound; it is a step, which means some state the first
// fork established is never undone. These counters exist to say whether that
// state is the hidden nodes: if `live` returns to zero between forks the tree
// does collapse and the cost is elsewhere, and if it does not, this is it.
//
// Plain relaxed atomics on paths that already take locks and allocate, so the
// measurement cannot plausibly perturb what it measures.
static VMO_PAGED_CREATED: AtomicU64 = AtomicU64::new(0);
static VMO_PAGED_DROPPED: AtomicU64 = AtomicU64::new(0);
static VMO_HIDDEN_CREATED: AtomicU64 = AtomicU64::new(0);
static VMO_HIDDEN_DROPPED: AtomicU64 = AtomicU64::new(0);
static VMO_COW_CHILDREN: AtomicU64 = AtomicU64::new(0);

// Length of the per-VMO mapping list, sampled where a fork walks it.
//
// `Drop for VmMapping` does not call `VMObjectPaged::remove_mapping`, so a
// mapping that goes away leaves its `Weak` behind; only an explicit
// `remove_mapping` (or another `retain` pass) ever prunes them. Every
// `create_child` then walks the whole list, dead entries included. That shape
// matches what the fork measurements show — nearly linear on a process's FIRST
// fork and quadratic on every one after, resetting when `exec` replaces the
// address space — so these say whether the list is in fact filling up.
//
// `dead` is the count that failed to upgrade: pure waste, and the number that
// should be zero.
static MAP_LIST_SCANS: AtomicU64 = AtomicU64::new(0);
static MAP_LIST_ENTRIES: AtomicU64 = AtomicU64::new(0);
static MAP_LIST_DEAD: AtomicU64 = AtomicU64::new(0);
static MAP_LIST_MAX: AtomicU64 = AtomicU64::new(0);

struct DeferredRangeChange {
    map: Arc<VmMapping>,
    offset: usize,
    len: usize,
    op: RangeChangeOp,
}

impl DeferredRangeChange {
    fn new(map: Arc<VmMapping>, offset: usize, len: usize, op: RangeChangeOp) -> Self {
        Self {
            map,
            offset,
            len,
            op,
        }
    }

    fn apply(self) {
        self.map
            .range_change_blocking(self.offset, self.len, self.op);
    }
}

struct CowFaultSeqGuard<'a> {
    seq: &'a AtomicU64,
}

impl<'a> CowFaultSeqGuard<'a> {
    fn try_begin(seq: &'a AtomicU64) -> Option<Self> {
        let mut current = seq.load(Ordering::Acquire);
        loop {
            if (current & 1) != 0 {
                return None;
            }
            match seq.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self { seq }),
                Err(next) => current = next,
            }
        }
    }
}

impl Drop for CowFaultSeqGuard<'_> {
    fn drop(&mut self) {
        let prev = self.seq.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(prev & 1, 1);
    }
}

/// Marks a `decommit` as in flight on one object, from before the first frame
/// is released until after the last stale PTE has been unmapped.
///
/// Unlike [`CowFaultSeqGuard`] this never has to wait: the two counters stay
/// ordered (`begin >= end`) however many decommits overlap, and a reader only
/// needs to tell "some decommit was active during my window" from "none was".
struct DecommitGuard<'a> {
    end: &'a AtomicU64,
}

impl<'a> DecommitGuard<'a> {
    fn begin(vmo: &'a VMObjectPaged) -> Self {
        vmo.decommit_begin.fetch_add(1, Ordering::AcqRel);
        Self {
            end: &vmo.decommit_end,
        }
    }
}

impl Drop for DecommitGuard<'_> {
    fn drop(&mut self) {
        self.end.fetch_add(1, Ordering::AcqRel);
    }
}

fn apply_deferred_range_changes(range_changes: Vec<DeferredRangeChange>) {
    for range_change in range_changes {
        range_change.apply();
    }
}

/// Per-VMO mapping-list census: `(scans, entries walked, dead entries, longest
/// list seen)`.
pub fn mapping_list_stats() -> (u64, u64, u64, u64) {
    (
        MAP_LIST_SCANS.load(Ordering::Relaxed),
        MAP_LIST_ENTRIES.load(Ordering::Relaxed),
        MAP_LIST_DEAD.load(Ordering::Relaxed),
        MAP_LIST_MAX.load(Ordering::Relaxed),
    )
}

/// Copy-on-write tree census: `(paged live, hidden live, snapshots taken)`.
///
/// The two "live" figures are create-minus-drop, so a hidden count that does not
/// fall back to its pre-fork value is a tree that did not collapse.
pub fn cow_tree_stats() -> (u64, u64, u64) {
    let paged = VMO_PAGED_CREATED
        .load(Ordering::Relaxed)
        .saturating_sub(VMO_PAGED_DROPPED.load(Ordering::Relaxed));
    let hidden = VMO_HIDDEN_CREATED
        .load(Ordering::Relaxed)
        .saturating_sub(VMO_HIDDEN_DROPPED.load(Ordering::Relaxed));
    (paged, hidden, VMO_COW_CHILDREN.load(Ordering::Relaxed))
}

enum VMOType {
    /// The original node.
    Origin,
    /// A snapshot of the parent node.
    Snapshot,
    /// Internal non-leaf node for snapshot.
    ///
    /// ```text
    ///    v---create_child
    ///    O       H <--- hidden node
    ///   /   =>  / \
    ///  S       O   S
    /// ```
    Hidden {
        /// The left child.
        left: WeakRef,
        /// The right child.
        right: WeakRef,
    },
}

/// [fossil-hunt] One budgeted error line per distinct collapse anomaly (8 per
/// boot): these paths silently degrade the hidden-node merge and are the
/// standing suspects for a fork child resolving a page to a frame frozen many
/// writer-generations in the past.
fn fossil_trap(what: &str) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static BUDGET: AtomicU32 = AtomicU32::new(0);
    if BUDGET.fetch_add(1, Ordering::Relaxed) < 8 {
        log::error!("[cow-collapse] {}", what);
    }
}

impl VMOType {
    fn get_tag_and_other(&self, child: &WeakRef) -> (PageStateTag, WeakRef) {
        match self {
            VMOType::Hidden { left, right, .. } => {
                if left.ptr_eq(child) {
                    (PageStateTag::LeftSplit, right.clone())
                } else if right.ptr_eq(child) {
                    (PageStateTag::RightSplit, left.clone())
                } else {
                    (PageStateTag::Owned, Weak::new())
                }
            }
            _ => (PageStateTag::Owned, Weak::new()),
        }
    }

    fn is_hidden(&self) -> bool {
        matches!(self, VMOType::Hidden { .. })
    }
}

/// The main VM object type, holding a list of pages.
pub struct VMObjectPaged {
    /// The lock that protected the `inner`
    /// This lock is shared between objects in the same clone tree to avoid deadlock
    lock: Arc<Mutex<()>>,
    inner: RefCell<VMObjectPagedInner>,
    /// Shared object (see `VMObjectTrait::set_shared`): anonymous memory
    /// shared across `fork`, or a file/memfd page cache.
    shared: core::sync::atomic::AtomicBool,
    /// Mappings of BORROWERS of this page cache, each with the cache page
    /// its page 0 borrows. Their PTEs point at this cache's frames, so a
    /// decommit here must unmap them too; the cache otherwise knows nothing
    /// about its borrowers. Leaf lock: taken briefly, never with another
    /// lock taken inside it.
    borrower_maps: Mutex<Vec<(Weak<VmMapping>, usize)>>,
    /// Even when quiescent; odd while `create_child` has dropped the family
    /// lock to write-protect alias PTEs before publishing the hidden-node
    /// reshape. Write faults that see a change (or an odd value) retry rather
    /// than installing a writable PTE onto a frame that may have become shared.
    cow_fault_seq: AtomicU64,
    /// Decommit generation, as a pair of counters that only ever grow:
    /// `decommit_begin` goes up before `decommit` releases its first frame and
    /// `decommit_end` only once the last PTE pointing at those frames is gone,
    /// so `decommit_begin == decommit_end` means "no frame of this object is
    /// being released right now". A page fault snapshots the pair before
    /// `commit_page` and re-reads it while publishing its PTE; see
    /// `VmObject::decommit_snapshot`. Two counters rather than one odd/even
    /// sequence because concurrent decommits of different ranges must not
    /// serialize on each other.
    decommit_begin: AtomicU64,
    decommit_end: AtomicU64,
    /// The page cache this leaf borrows from (the same object as
    /// `inner.cache.0`), held OUTSIDE the `RefCell` so the fault path can read
    /// the cache's decommit generation without taking this object's family
    /// lock. A borrower's `commit_page` hands back the *cache's* frames, so a
    /// decommit over there frees pages this object's faults are publishing,
    /// and the borrower's own counters would never notice. The `cache` field's
    /// doc explains why re-locking across families on the fault path is not an
    /// option.
    cache_ref: Option<Arc<VmObject>>,
}

/// We always lock the lock before access to the Refcell, so it is actually sync
#[allow(unsafe_code)]
unsafe impl Sync for VMObjectPaged {}

type WeakRef = Weak<VMObjectPaged>;

/// The mutable part of `VMObjectPaged`.
struct VMObjectPagedInner {
    /// Owner identifier.
    owner: u64,
    type_: VMOType,
    /// Parent node.
    parent: Option<Arc<VMObjectPaged>>,
    /// The offset from parent.
    parent_offset: usize,
    /// The range limit from parent.
    parent_limit: usize,
    /// The size in bytes.
    size: usize,
    /// Physical frames of this VMO.
    frames: BTreeMap<usize, PageState>,
    /// All mappings to this VMO.
    mappings: Vec<Weak<VmMapping>>,
    /// Cache Policy
    cache_policy: CachePolicy,
    /// Is contiguous
    contiguous: bool,
    /// A weak reference to myself.
    self_ref: WeakRef,
    /// Sum of pin_count
    pin_count: usize,
    /// Optional lazy backing source (demand paging). When set, a page that is
    /// committed for the first time on this (root) node is filled from the
    /// source instead of being left zero. Carried to the hidden parent on
    /// `create_child` (fork) so it keeps resolving for both children.
    source: Option<Arc<dyn FrameFiller>>,
    /// Shared page cache this leaf BORROWS clean pages from, with the byte
    /// offset of this VMO's page 0 inside it.
    ///
    /// This is what gives `MAP_PRIVATE` file mappings a page cache: a read
    /// fault resolves to the cache VMO's own frame (mapped read-only by the
    /// fault handler), so N processes mapping the same library share one set
    /// of frames; the first write fault copies that single page into `frames`
    /// (copy-up) and the mapping goes on privately. Deliberately NOT the
    /// hidden-node snapshot tree: the borrower never reshapes the cache, is
    /// invisible to it, and holds it alive by plain `Arc` -- the snapshot
    /// machinery is the one piece of this file that mis-replicated address
    /// spaces when COW fork used it, so the cache path shares nothing with it.
    ///
    /// The third element is the cache VMO's length, captured at borrow time.
    /// A file page cache is never resizable, so this is immutable — and caching
    /// it here means the per-page commit bounds check does not call
    /// `cache.len()`, which would re-acquire the cache's family lock *while this
    /// borrower's family lock is held*. Under a heavy concurrent-exec workload
    /// (many processes faulting the same library through one shared cache) that
    /// extra cross-family acquisition serialized every commit on the cache lock
    /// and tripped the >8 s deadlock detector as pathological contention. The
    /// bounds check now reads this stored length, holding only one lock.
    cache: Option<(Arc<VmObject>, usize, usize)>,
}

/// Page state in VMO.
struct PageState {
    frame: PhysFrame,
    tag: PageStateTag,
    pin_count: u8,
}

/// The owner tag of pages in the node.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum PageStateTag {
    /// If the node is hidden, the page is shared by its 2 children.
    /// Otherwise, the page is owned by the node.
    Owned,
    /// The page is split to the left child and now owned by the right child.
    LeftSplit,
    /// The page is split to the right child and now owned by the left child.
    RightSplit,
}

impl PageStateTag {
    fn negate(self) -> Self {
        match self {
            PageStateTag::LeftSplit => PageStateTag::RightSplit,
            PageStateTag::RightSplit => PageStateTag::LeftSplit,
            PageStateTag::Owned => unreachable!(),
        }
    }
    fn is_split(self) -> bool {
        self != PageStateTag::Owned
    }
}

impl PageState {
    fn new(frame: PhysFrame) -> Self {
        VMO_PAGE_ALLOC.add(1);
        PageState {
            frame,
            tag: PageStateTag::Owned,
            pin_count: 0,
        }
    }
    #[allow(unsafe_code)]
    fn take(self) -> PhysFrame {
        let frame = unsafe { core::mem::transmute_copy(&self.frame) };
        VMO_PAGE_DEALLOC.add(1);
        core::mem::forget(self);
        frame
    }
    fn swap(&mut self, t: &mut Self) {
        core::mem::swap(&mut self.frame, &mut t.frame);
        core::mem::swap(&mut self.pin_count, &mut t.pin_count);
    }
}

impl Drop for PageState {
    fn drop(&mut self) {
        VMO_PAGE_DEALLOC.add(1);
    }
}

impl VMObjectPaged {
    /// Create a new VMO backing on physical memory allocated in pages.
    pub fn new(pages: usize) -> Arc<Self> {
        VMObjectPaged::wrap(
            VMObjectPagedInner {
                owner: new_owner_id(),
                type_: VMOType::Origin,
                parent: None,
                parent_offset: 0usize,
                parent_limit: 0usize,
                size: pages * PAGE_SIZE,
                frames: BTreeMap::new(),
                mappings: Vec::new(),
                cache_policy: CachePolicy::Cached,
                contiguous: false,
                self_ref: Default::default(),
                pin_count: 0,
                source: None,
                cache: None,
            },
            None,
        )
    }

    /// Create a new paged VMO demand-paged from `source` (see [`FrameFiller`]).
    pub fn new_with_source(pages: usize, source: Arc<dyn FrameFiller>) -> Arc<Self> {
        VMObjectPaged::wrap(
            VMObjectPagedInner {
                owner: new_owner_id(),
                type_: VMOType::Origin,
                parent: None,
                parent_offset: 0usize,
                parent_limit: 0usize,
                size: pages * PAGE_SIZE,
                frames: BTreeMap::new(),
                mappings: Vec::new(),
                cache_policy: CachePolicy::Cached,
                contiguous: false,
                self_ref: Default::default(),
                pin_count: 0,
                source: Some(source),
                cache: None,
            },
            None,
        )
    }

    /// Create a leaf that BORROWS clean pages from `cache` (a shared per-file
    /// page cache), starting at byte `base` inside it. See the `cache` field
    /// for the semantics; `pages` is this VMO's own size.
    pub fn new_borrowing(pages: usize, cache: Arc<VmObject>, base: usize) -> Arc<Self> {
        assert!(page_aligned(base));
        // Capture the cache length now, outside any family lock: a file page
        // cache is not resizable, so this stays valid for the borrower's life
        // and spares the per-commit bounds check a re-lock of the cache (see
        // the `cache` field doc).
        let cache_len = cache.len();
        VMObjectPaged::wrap(
            VMObjectPagedInner {
                owner: new_owner_id(),
                type_: VMOType::Origin,
                parent: None,
                parent_offset: 0usize,
                parent_limit: 0usize,
                size: pages * PAGE_SIZE,
                frames: BTreeMap::new(),
                mappings: Vec::new(),
                cache_policy: CachePolicy::Cached,
                contiguous: false,
                self_ref: Default::default(),
                pin_count: 0,
                source: None,
                cache: Some((cache, base, cache_len)),
            },
            None,
        )
    }

    /// Independent copy for Linux `fork`: committed pages are copied eagerly,
    /// but pages never touched by the parent stay uncommitted in the child and
    /// keep resolving lazily — from the same backing source (file mapping) when
    /// there is one, or as demand-zero otherwise. This is what makes forking a
    /// process with hundreds of MiB of demand-paged libraries cheap: only the
    /// resident pages are copied, instead of faulting the whole file in from
    /// disk just to duplicate it.
    ///
    /// Only the plain origin-node case is supported; anything with a parent,
    /// pinned/contiguous frames or a special cache policy must use the caller's
    /// eager fallback.
    fn fork_copy(&self) -> ZxResult<Arc<Self>> {
        let inner = self.get_inner();
        if !matches!(inner.type_, VMOType::Origin)
            || inner.parent.is_some()
            || inner.contiguous
            || inner.pin_count != 0
            || inner.cache_policy != CachePolicy::Cached
        {
            return Err(ZxError::NOT_SUPPORTED);
        }
        let mut frames = BTreeMap::new();
        for (&idx, state) in inner.frames.iter() {
            let frame = PhysFrame::new().ok_or(ZxError::NO_MEMORY)?;
            kernel_hal::mem::pmem_copy(frame.paddr(), state.frame.paddr(), PAGE_SIZE);
            frames.insert(idx, PageState::new(frame));
        }
        Ok(VMObjectPaged::wrap(
            VMObjectPagedInner {
                owner: new_owner_id(),
                type_: VMOType::Origin,
                parent: None,
                parent_offset: 0,
                parent_limit: 0,
                size: inner.size,
                frames,
                mappings: Vec::new(),
                cache_policy: CachePolicy::Cached,
                contiguous: false,
                self_ref: Default::default(),
                pin_count: 0,
                source: inner.source.clone(),
                // A fork's child keeps borrowing from the same page cache: its
                // untouched pages must go on reading file bytes, exactly as
                // `source` is carried for the demand-paged case.
                cache: inner.cache.clone(),
            },
            None,
        ))
    }

    /// Create a new VMO backing on contiguous pages.
    ///
    /// `align_log2` is an ABSOLUTE log2 alignment, so its floor is
    /// `PAGE_SIZE_LOG2` and 0 does not mean "no alignment": the frame
    /// allocator counts in pages, so what it is given is
    /// `align_log2 - PAGE_SIZE_LOG2`, and anything smaller made that
    /// subtraction underflow -- a panic in a debug kernel and, in release, a
    /// colossal alignment handed to `PhysFrame::new_contiguous`. The only
    /// guard for it lived in `sys_vmo_create_contiguous`, two crates away,
    /// where the contract is not written down either.
    pub fn new_contiguous(pages: usize, align_log2: usize) -> ZxResult<Arc<Self>> {
        if align_log2 < PAGE_SIZE_LOG2 {
            return Err(ZxError::INVALID_ARGS);
        }
        let vmo = Self::new(pages);
        let mut frames = PhysFrame::new_contiguous(pages, align_log2 - PAGE_SIZE_LOG2);
        if frames.is_empty() {
            return Err(ZxError::NO_MEMORY);
        }
        {
            let mut inner = vmo.get_inner_mut();
            inner.contiguous = true;
            for (i, f) in frames.drain(0..).enumerate() {
                kernel_hal::mem::pmem_zero(f.paddr(), PAGE_SIZE);
                let mut state = PageState::new(f);
                state.pin_count += 1;
                inner.frames.insert(i, state);
            }
        }
        Ok(vmo)
    }

    /// Internal: Wrap an inner struct to object.
    fn wrap(inner: VMObjectPagedInner, lock_ref: Option<Arc<Mutex<()>>>) -> Arc<Self> {
        VMO_PAGED_CREATED.fetch_add(1, Ordering::Relaxed);
        if inner.type_.is_hidden() {
            VMO_HIDDEN_CREATED.fetch_add(1, Ordering::Relaxed);
        }
        let cache_ref = inner.cache.as_ref().map(|(cache, _, _)| cache.clone());
        let obj = Arc::new(VMObjectPaged {
            lock: lock_ref.unwrap_or_else(|| Arc::new(Mutex::new(()))),
            inner: RefCell::new(inner),
            shared: core::sync::atomic::AtomicBool::new(false),
            borrower_maps: Mutex::new(Vec::new()),
            cow_fault_seq: AtomicU64::new(0),
            decommit_begin: AtomicU64::new(0),
            decommit_end: AtomicU64::new(0),
            cache_ref,
        });
        obj.inner.borrow_mut().self_ref = Arc::downgrade(&obj);
        obj
    }

    /// A leaf whose pages other mappings or `read(2)` can observe: shared
    /// across `fork`, or a file/memfd page cache (both marked by
    /// `set_shared`). Borrowers and private file snapshots keep their
    /// copy-on-read / zero-page semantics.
    fn is_shared_leaf(&self, _inner: &VMObjectPagedInner) -> bool {
        self.shared.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// On a shared leaf a read fault must commit a REAL page. The plain path
    /// hands a read of an uncommitted page the global zero frame without
    /// recording anything; the PTE then points at the zero frame for good,
    /// and the first write -- from another mapping, another process after
    /// `fork`, or `pwrite(2)` -- lands in a fresh frame this mapping never
    /// sees (`firefox-probe`: "parent sees the child's writes to shared
    /// anonymous pages it had only READ" and the grown-memfd cases). Asking
    /// for WRITE on a root leaf allocates and inserts the page (filled from
    /// the source when inside it) and returns it; it does not change how the
    /// fault handler maps it.
    fn shared_commit_flags(&self, inner: &VMObjectPagedInner, flags: MMUFlags) -> MMUFlags {
        if self.is_shared_leaf(inner) {
            flags | MMUFlags::WRITE
        } else {
            flags
        }
    }

    /// get the reference to inner by lock the shared lock
    ///
    /// Take the family lock, then borrow `inner`.
    ///
    /// DROP-ORDER CONTRACT (the source of the twice-recurring "RefCell
    /// already borrowed" panic at this file's borrow sites): the RefCell
    /// borrow must die BEFORE the family mutex is released, or another CPU
    /// can take the mutex and borrow_mut() while our borrow is still alive
    /// for a few instructions. A `(guard, borrow)` tuple could not enforce
    /// this: destructuring drops bindings in REVERSE order (borrow first —
    /// correct) but an intact temporary like `self.get_inner().size` drops
    /// tuple fields in FORWARD order (guard first — the mutex was released
    /// while the borrow lived, which is exactly the cross-CPU race that
    /// kept coming back, last seen as a fork-time panic on real 8-core
    /// hardware). These guard newtypes make the order structural: struct
    /// fields drop in declaration order, `inner` is declared before `guard`,
    /// so the borrow ALWAYS dies before the mutex releases, no matter how a
    /// call site uses the value.
    #[track_caller]
    fn get_inner(&self) -> InnerGuard<'_> {
        reentry_check(self.lock_ptr());
        let guard = self.lock.lock();
        reentry_enter(self.lock_ptr());
        let drain = StashDrain::open();
        InnerGuard {
            inner: self.inner.borrow(),
            _guard: guard,
            _drain: drain,
            _reentry: ReentryPop(self.lock_ptr()),
        }
    }

    /// Identity of this VMO's family lock, for the re-entrancy tripwire.
    #[inline]
    fn lock_ptr(&self) -> usize {
        Arc::as_ptr(&self.lock) as usize
    }

    /// Mutable variant of [`get_inner`] — same structural drop-order contract.
    // `track_caller` on both accessors so the ticket lock's holder snapshot —
    // and a deadlock banner built from it — records the REAL call site
    // (`commit_page`, `create_child`, `Drop`…), not this wrapper. Every
    // family-lock banner to date has read `paged.rs:391` on both the waiter
    // and the holder line, which names the lobby and not the room.
    #[track_caller]
    fn get_inner_mut(&self) -> InnerGuardMut<'_> {
        reentry_check(self.lock_ptr());
        let guard = self.lock.lock();
        reentry_enter(self.lock_ptr());
        let drain = StashDrain::open();
        InnerGuardMut {
            inner: self.inner.borrow_mut(),
            _guard: guard,
            _drain: drain,
            _reentry: ReentryPop(self.lock_ptr()),
        }
    }

    /// Drop every PTE that points into `start_page..start_page + pages` of
    /// this object, borrowers of its pages included.
    ///
    /// The rule every operation that hands frames back to the allocator has
    /// to keep -- no PTE may go on pointing at them -- written once. Three
    /// operations release frames (`decommit`, `set_len` and `zero`) and only
    /// `decommit` used to keep it: a shrink or a whole-page zero left
    /// userspace with a live PTE onto a frame the allocator had taken back,
    /// and a whole-page zero left it reading the bytes it had just asked to
    /// have zeroed.
    ///
    /// `mappings` is what the caller collected while it still held the family
    /// lock. The passes below run WITHOUT it, so the mapping locks can be
    /// taken in blocking mode and no stale PTE is skipped on contention.
    fn unmap_released(&self, mappings: Vec<Arc<VmMapping>>, start_page: usize, pages: usize) {
        if pages == 0 {
            return;
        }
        for map in mappings {
            map.range_change_blocking(start_page, pages, RangeChangeOp::Unmap);
        }
        // A borrower's PTEs point at THIS object's frames and it has no way
        // of learning that they went; only the cache knows who borrows it.
        let borrowers: Vec<(Arc<VmMapping>, usize)> = self
            .borrower_maps
            .lock()
            .iter()
            .filter_map(|(m, base)| m.upgrade().map(|m| (m, *base)))
            .collect();
        for (map, base) in borrowers {
            let s = start_page.max(base);
            let e = (start_page + pages).max(base);
            if e > s {
                map.range_change_blocking(s - base, e - s, RangeChangeOp::Unmap);
            }
        }
    }

    /// The mappings of this object, for an unmap pass that runs once the
    /// family lock is back down.
    fn live_mappings(&self) -> Vec<Arc<VmMapping>> {
        self.get_inner()
            .mappings
            .iter()
            .filter_map(|map| map.upgrade())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Per-cpu deferred-drop stash
// ---------------------------------------------------------------------------
//
// The standing invariant is "no `Arc` of a COW family dies while the family
// lock is held" — an in-scope death can be the last reference, and the
// re-entrant `Drop` then re-acquires the non-re-entrant ticket lock on the
// same cpu: the self-deadlock this file has now produced from three different
// functions (`remove_child`, `replace_child`, `commit_page_internal`).
// Threading a `deferred` vector through signatures worked for the first two,
// but the commit path fans out through closures (`commit_pages_with`,
// `for_each_page`) where threading is invasive and easy to miss on the next
// edit.
//
// So the guards themselves enforce it. Code under a family guard parks any
// possibly-last `Arc` in this per-cpu stash; the guard's third field — dropped
// last, i.e. after the RefCell borrow AND after the lock guard — drains every
// entry pushed while it was open. Safe without atomics because the family
// lock's `push_off` keeps IRQs off: nothing else runs on this cpu while
// entries are pushed, and nested guards drain down to their own watermark, so
// an inner drain never steals an outer frame's entries.
// Sized to the kernel's real per-cpu ceiling, not an arbitrary local literal:
// on bare-metal x86_64, SMP bring-up (`register_cpu`) and every IRQ-off
// primitive that indexes this array (`mycpu`, via push_off/pop_off) already
// assert `cpu_id() < MAX_CORE_NUM` before any core can reach this code, so a
// >16-logical-cpu box (unremarkable on modern desktop/server silicon or a
// KVM guest with many vCPUs) previously aliased two or more distinct,
// concurrently-running cores onto the SAME slot -- a data race on a
// `Vec<Arc<VMObjectPaged>>`'s {ptr,len,cap} triple despite each core
// believing (correctly, for ITS OWN slot) that IRQs-off made it exclusive.
// `stash_cpu()`'s `.min(STASH_CPUS - 1)` clamp is deliberately left as-is:
// with STASH_CPUS == MAX_CORE_NUM it is provably dead code on bare metal,
// but it remains the only thing stopping libos/test builds -- where
// cpu_id() is a truncated host thread id, unrelated to MAX_CORE_NUM -- from
// skipping a stash_defer() a caller depends on to avoid a reentrant-lock
// deadlock (see the stash mechanism's own doc comment above).
const STASH_CPUS: usize = kernel_hal::config::MAX_CORE_NUM;
const STASH_CAP: usize = 128;

struct StashSlot(UnsafeCell<Vec<Arc<VMObjectPaged>>>);
// SAFETY: each slot is only touched by its own cpu, with IRQs off.
unsafe impl Sync for StashSlot {}

struct DropStashes([StashSlot; STASH_CPUS]);

static DROP_STASH: DropStashes =
    DropStashes([const { StashSlot(UnsafeCell::new(Vec::new())) }; STASH_CPUS]);

fn stash_cpu() -> usize {
    (kernel_hal::cpu::cpu_id() as usize).min(STASH_CPUS - 1)
}

// ── Re-entrancy tripwire for the family lock ─────────────────────────────────
//
// The family lock is a non-reentrant ticket mutex SHARED across a COW family
// (create_child hands children `lock_ref.clone()`). Calling a *public* method
// of any family member (`len()`, `commit_page()`, …) while already holding the
// lock re-acquires it and self-deadlocks — the failure reproduced live at
// commit_page(:678)/len(:660). Static reading has not pinned which call does
// it, so this records, per cpu, the lock(s) this cpu holds; the next re-entrant
// acquisition prints the caller location and a frame-pointer backtrace BEFORE
// deadlocking on the mutex, naming the exact re-entrant path. Best-effort and
// diagnostic: per-cpu, cleared on guard drop.
const REENTRY_DEPTH: usize = 16;
struct HeldSlot(UnsafeCell<[usize; REENTRY_DEPTH]>);
// SAFETY: each cpu only ever touches its own slot; the family lock's push_off
// keeps IRQs off across a held region, so pushes/pops are not preempted.
unsafe impl Sync for HeldSlot {}
static HELD_LOCKS: [HeldSlot; STASH_CPUS] =
    [const { HeldSlot(UnsafeCell::new([0usize; REENTRY_DEPTH])) }; STASH_CPUS];

#[track_caller]
fn reentry_check(lock_ptr: usize) {
    let held = unsafe { &*HELD_LOCKS[stash_cpu()].0.get() };
    if !held.contains(&lock_ptr) {
        return;
    }
    let loc = core::panic::Location::caller();
    kernel_hal::console::serial_write_fmt_spin(format_args!(
        "\n[vmo-reentry] SELF-DEADLOCK about to happen: cpu re-acquires family \
         lock {:#x} it already holds, at {}:{} — call chain:\n",
        lock_ptr,
        loc.file(),
        loc.line(),
    ));
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        let mut rbp: usize;
        unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };
        for _ in 0..24 {
            if rbp == 0 || rbp & 0x7 != 0 || rbp < 0xffff_ff00_0000_0000 {
                break;
            }
            let ret = unsafe { core::ptr::read_volatile((rbp + 8) as *const usize) };
            let next = unsafe { core::ptr::read_volatile(rbp as *const usize) };
            if ret == 0 {
                break;
            }
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "[vmo-reentry]   ret={:#x}\n",
                ret
            ));
            if next <= rbp {
                break;
            }
            rbp = next;
        }
        kernel_hal::console::serial_write_str(
            "[vmo-reentry] symbolize: llvm-addr2line -e <zcore.elf> -fCi <ret ...>\n",
        );
    }
}

fn reentry_enter(lock_ptr: usize) {
    let held = unsafe { &mut *HELD_LOCKS[stash_cpu()].0.get() };
    if let Some(slot) = held.iter_mut().find(|p| **p == 0) {
        *slot = lock_ptr;
    }
}

/// Pops one matching `lock_ptr` off this cpu's held-set on drop.
struct ReentryPop(usize);
impl Drop for ReentryPop {
    fn drop(&mut self) {
        let held = unsafe { &mut *HELD_LOCKS[stash_cpu()].0.get() };
        if let Some(slot) = held.iter_mut().rev().find(|p| **p == self.0) {
            *slot = 0;
        }
    }
}

/// Park `arc` until the current family guard releases the lock. MUST be called
/// with a family guard held (IRQs are then off, making the per-cpu access
/// exclusive).
fn stash_defer(arc: Arc<VMObjectPaged>) {
    // SAFETY: per-cpu, IRQs off under the family lock; see `DropStashes`.
    let v = unsafe { &mut *DROP_STASH.0[stash_cpu()].0.get() };
    if v.len() < STASH_CAP {
        v.push(arc);
    } else {
        // Overflow: leaking is recoverable and diagnosable; dropping under the
        // lock is the deadlock this exists to prevent. Never expected — depth
        // here is the nesting of COW operations, single digits.
        error!("vmo drop stash overflow; leaking one Arc");
        core::mem::forget(arc);
    }
}

/// Watermark recorded when a guard opens; dropping it drains every stash entry
/// pushed since. Declared LAST in the guard structs so it runs after the lock
/// guard's own drop — the entries die with the family lock released.
struct StashDrain {
    mark: usize,
}

impl StashDrain {
    fn open() -> Self {
        // SAFETY: called while constructing a guard, family lock just taken.
        let v = unsafe { &*DROP_STASH.0[stash_cpu()].0.get() };
        StashDrain { mark: v.len() }
    }
}

impl Drop for StashDrain {
    fn drop(&mut self) {
        loop {
            // SAFETY: IRQs are on again here (the lock guard dropped first),
            // but only this cpu's own frames push/pop this stash and we pop
            // one entry at a time, re-reading the length each round: a nested
            // Drop triggered by the pop pushes and drains at ITS OWN deeper
            // watermark before returning here.
            let arc = {
                let v = unsafe { &mut *DROP_STASH.0[stash_cpu()].0.get() };
                if v.len() <= self.mark {
                    break;
                }
                v.pop()
            };
            drop(arc);
        }
    }
}

/// Shared borrow of a `VMObjectPaged`'s inner state, holding the family lock.
/// Field order is load-bearing: `inner` (the RefCell borrow) drops first,
/// `_guard` (the family mutex) last. See `get_inner` for the full contract.
struct InnerGuard<'a> {
    inner: Ref<'a, VMObjectPagedInner>,
    _guard: MutexGuard<'a, ()>,
    /// Pops this lock off the per-cpu held-set (drops after the mutex releases,
    /// before the drain — so the drain's Arc drops don't self-report).
    _reentry: ReentryPop,
    /// Drops LAST (declaration order): drains the per-cpu deferred-drop stash
    /// with the family lock already released. See `stash_defer`.
    _drain: StashDrain,
}

impl core::ops::Deref for InnerGuard<'_> {
    type Target = VMObjectPagedInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Exclusive borrow of a `VMObjectPaged`'s inner state, holding the family
/// lock. Same structural drop-order guarantee as [`InnerGuard`].
struct InnerGuardMut<'a> {
    inner: RefMut<'a, VMObjectPagedInner>,
    _guard: MutexGuard<'a, ()>,
    /// Pops this lock off the per-cpu held-set. See `InnerGuard::_reentry`.
    _reentry: ReentryPop,
    /// Drops LAST: drains the deferred-drop stash after the lock releases.
    _drain: StashDrain,
}

impl core::ops::Deref for InnerGuardMut<'_> {
    type Target = VMObjectPagedInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl core::ops::DerefMut for InnerGuardMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl VMObjectTrait for VMObjectPaged {
    fn read(&self, offset: usize, buf: &mut [u8]) -> ZxResult {
        let mut range_changes = Vec::new();
        let ret = {
            let mut inner = self.get_inner_mut();
            if inner.cache_policy != CachePolicy::Cached {
                Err(ZxError::BAD_STATE)
            } else {
                inner.for_each_page(
                    offset,
                    buf.len(),
                    MMUFlags::READ,
                    &mut range_changes,
                    |paddr, buf_range| {
                        kernel_hal::mem::pmem_read(paddr, &mut buf[buf_range]);
                    },
                )
            }
        };
        apply_deferred_range_changes(range_changes);
        ret
    }

    fn write(&self, offset: usize, buf: &[u8]) -> ZxResult {
        let mut range_changes = Vec::new();
        let ret = {
            let mut inner = self.get_inner_mut();
            if inner.cache_policy != CachePolicy::Cached {
                Err(ZxError::BAD_STATE)
            } else {
                inner.for_each_page(
                    offset,
                    buf.len(),
                    MMUFlags::WRITE,
                    &mut range_changes,
                    |paddr, buf_range| {
                        kernel_hal::mem::pmem_write(paddr, &buf[buf_range]);
                    },
                )
            }
        };
        apply_deferred_range_changes(range_changes);
        ret
    }

    fn zero(&self, offset: usize, len: usize) -> ZxResult {
        let mut range_changes = Vec::new();
        // The whole-page path below drops frames, so it releases memory just
        // like `decommit` does and takes the same window. See `set_len`.
        let _decommit_guard = DecommitGuard::begin(self);
        // First page dropped, and how many: the blocks a `zero` walks are
        // consecutive, so the pages it drops are one run.
        let mut released: Option<(usize, usize)> = None;
        let ret = (|| {
            let mut inner = self.get_inner_mut();
            // Checked, because `zx_vmo_op_range(ZX_VMO_OP_ZERO)` hands both
            // numbers through from userspace without a bound of its own: a
            // wrapping sum passed this test in a release build and then drove
            // `BlockIter` over pages this object does not have.
            let end = offset.checked_add(len).ok_or(ZxError::OUT_OF_RANGE)?;
            if end > inner.size {
                return Err(ZxError::OUT_OF_RANGE);
            }
            let iter = BlockIter {
                begin: offset,
                end,
                block_size_log2: PAGE_SIZE_LOG2 as u8,
            };
            let mut unwanted = VecDeque::new();
            for block in iter {
                //let paddr = self.commit_page(block.block, MMUFlags::READ)?;
                if block.len() == PAGE_SIZE && !inner.is_contiguous() {
                    let _ = inner.commit_page(block.block, MMUFlags::WRITE, &mut range_changes)?;
                    unwanted.push_back(block.block + inner.parent_offset / PAGE_SIZE);
                    inner.frames.remove(&block.block);
                    released = Some(match released {
                        None => (block.block, 1),
                        Some((first, _)) => (first, block.block + 1 - first),
                    });
                } else if inner.committed_pages_in_range(block.block, block.block + 1) != 0 {
                    // check whether this page is initialized, otherwise nothing should be done
                    let paddr =
                        inner.commit_page(block.block, MMUFlags::WRITE, &mut range_changes)?;
                    kernel_hal::mem::pmem_zero(paddr + block.begin, block.len());
                }
            }
            inner.release_unwanted_pages_in_parent(unwanted);
            Ok(())
        })();
        apply_deferred_range_changes(range_changes);
        // A dropped frame is a dropped frame whichever operation dropped it:
        // without this the next read through a mapping came back with the
        // bytes the caller had just asked to have zeroed, off a frame the
        // allocator had taken back. Runs even when the loop gave up partway,
        // because the pages it got through are gone either way.
        if let Some((first, pages)) = released {
            self.unmap_released(self.live_mappings(), first, pages);
        }
        ret
    }

    fn len(&self) -> usize {
        self.get_inner().size
    }

    fn set_len(&self, len: usize) -> ZxResult {
        assert!(page_aligned(len));
        let mut deferred: Vec<Arc<VMObjectPaged>> = Vec::new();
        // Shrinking hands frames back to the allocator, so this opens the
        // decommit window too: a fault that is between `commit_page` and
        // publishing its PTE gives up and retries instead of mapping a frame
        // that is on its way out. Taken for a grow as well, which releases
        // nothing -- the window then closes a few instructions later, and a
        // guard that depends on which way the object is about to be resized
        // would have to read `size` before taking the lock that decides it.
        let _decommit_guard = DecommitGuard::begin(self);
        let released = {
            let mut inner = self.get_inner_mut();
            if inner.pin_count > 0 {
                return Err(ZxError::BAD_STATE);
            }
            let old_len = inner.size;
            inner.resize(len, &mut deferred);
            old_len.saturating_sub(len) / PAGE_SIZE
        };
        drop(deferred);
        // `resize` dropped the frames of every page past `len`; the mappings
        // that still point at them are this object's to tear down, exactly as
        // in `decommit`. Collected after the family lock is back down: a
        // mapping that arrives after this point faults for its PTEs, and the
        // frames are already gone by then.
        self.unmap_released(self.live_mappings(), len / PAGE_SIZE, released);
        Ok(())
    }

    fn commit_page(&self, page_idx: usize, flags: MMUFlags) -> ZxResult<PhysAddr> {
        let mut range_changes = Vec::new();
        let ret = {
            let mut inner = self.get_inner_mut();
            // The page-fault path arrives here, and a mapping outlives the
            // pages it covers: `zx_vmo_set_size` can shrink the object under a
            // mapping that was within it when it was made. A write fault past
            // the end used to demand-allocate a frame at an index the object
            // does not have -- one that `committed_pages_in_range`, which
            // clamps to `size`, could never report again. Out of range is an
            // error here, and the fault becomes an exception, which is what a
            // read or a write of that same address already answers.
            if page_idx >= inner.size / PAGE_SIZE {
                return Err(ZxError::OUT_OF_RANGE);
            }
            let flags = self.shared_commit_flags(&inner, flags);
            inner.commit_page(page_idx, flags, &mut range_changes)
        };
        apply_deferred_range_changes(range_changes);
        ret
    }

    fn commit_pages_with(
        &self,
        f: &mut dyn FnMut(&mut dyn FnMut(usize, MMUFlags) -> ZxResult<PhysAddr>) -> ZxResult,
    ) -> ZxResult {
        let mut range_changes = Vec::new();
        let ret = {
            let mut inner = self.get_inner_mut();
            let shared = self.is_shared_leaf(&inner);
            f(&mut |page_idx, flags| {
                let flags = if shared {
                    flags | MMUFlags::WRITE
                } else {
                    flags
                };
                inner.commit_page(page_idx, flags, &mut range_changes)
            })
        };
        apply_deferred_range_changes(range_changes);
        ret
    }

    fn commit(&self, offset: usize, len: usize) -> ZxResult {
        let start_page = offset / PAGE_SIZE;
        let pages = len / PAGE_SIZE;
        let mut range_changes = Vec::new();
        let ret = (|| {
            let mut inner = self.get_inner_mut();
            // The other half of the bound `zero` carries. Past the end,
            // `commit_page` demand-allocated real frames at page indices this
            // object does not have -- and `committed_pages_in_range` clamps to
            // `size`, so nothing that reports what a VMO holds could see them:
            // one `zx_vmo_op_range(COMMIT)` made a one-page object hold
            // sixty-four while every counter still said one.
            let end = offset.checked_add(len).ok_or(ZxError::OUT_OF_RANGE)?;
            if end > inner.size {
                return Err(ZxError::OUT_OF_RANGE);
            }
            for i in 0..pages {
                inner.commit_page(start_page + i, MMUFlags::WRITE, &mut range_changes)?;
            }
            Ok(())
        })();
        apply_deferred_range_changes(range_changes);
        ret
    }

    fn decommit(&self, offset: usize, len: usize) -> ZxResult {
        let start_page = offset / PAGE_SIZE;
        let pages = len / PAGE_SIZE;
        // Opens the decommit window and closes it when this function returns,
        // i.e. after the unmap passes below. A page fault that is somewhere
        // between `commit_page` and publishing its PTE while this window is
        // open gives up and retries instead of mapping a frame we are about to
        // hand back to the allocator -- it cannot be reached through the unmap
        // passes, because its PTE does not exist yet. See `decommit_seq`.
        let _decommit_guard = DecommitGuard::begin(self);
        let mappings = {
            let mut inner = self.get_inner_mut();
            if inner.parent.is_some() {
                return Err(ZxError::NOT_SUPPORTED);
            }
            // Same bound `zero` carries, and the one `VMObjectSlice` already
            // wrote down for this very operation: `zx_vmo_op_range` hands
            // `offset` and `len` through from userspace with nothing but an
            // alignment check of its own.
            let end = offset.checked_add(len).ok_or(ZxError::OUT_OF_RANGE)?;
            if end > inner.size {
                return Err(ZxError::OUT_OF_RANGE);
            }
            // A pinned page is one a device may be doing DMA into right now:
            // handing its frame back to the allocator is a use-after-free that
            // nothing downstream can notice, and it leaves `unpin` looking for
            // a frame that is no longer there. `set_len` and `set_cache_policy`
            // already refuse for exactly this reason; this is the third of the
            // three, and it was the one that did not. Asked first, over the
            // whole range, so a refused decommit leaves every page alone.
            for i in 0..pages {
                let pinned = inner
                    .frames
                    .get(&(start_page + i))
                    .is_some_and(|frame| frame.pin_count != 0);
                if pinned {
                    return Err(ZxError::BAD_STATE);
                }
            }
            for i in 0..pages {
                inner.decommit(start_page + i);
            }
            inner
                .mappings
                .iter()
                .filter_map(|map| map.upgrade())
                .collect::<Vec<_>>()
        };
        // The frames are gone; no PTE may keep pointing at them.
        self.unmap_released(mappings, start_page, pages);
        Ok(())
    }

    fn create_child(&self, offset: usize, len: usize) -> ZxResult<Arc<dyn VMObjectTrait>> {
        assert!(page_aligned(offset));
        assert!(page_aligned(len));
        let (mappings, dead, cow_fault_guard) = loop {
            let inner = self.get_inner();
            inner.validate_create_child()?;
            if let Some(cow_fault_guard) = CowFaultSeqGuard::try_begin(&self.cow_fault_seq) {
                let len_before = inner.mappings.len() as u64;
                MAP_LIST_SCANS.fetch_add(1, Ordering::Relaxed);
                MAP_LIST_ENTRIES.fetch_add(len_before, Ordering::Relaxed);
                MAP_LIST_MAX.fetch_max(len_before, Ordering::Relaxed);
                let (mappings, dead) = inner.collect_live_mappings();
                break (mappings, dead, cow_fault_guard);
            }
            drop(inner);
            core::hint::spin_loop();
        };
        if dead != 0 {
            MAP_LIST_DEAD.fetch_add(dead, Ordering::Relaxed);
        }
        // Fork correctness differs from deferred Unmap: the pages are still
        // live. Mark the VMO's fork window in `cow_fault_seq`, then drop WRITE
        // from every alias mapping with blocking locks OUTSIDE the family lock.
        // Any write fault that races this odd sequence retries after
        // `commit_page(WRITE)` instead of re-installing a writable PTE on the
        // old tree; once the PTEs are read-only, publishing the hidden-node
        // reshape below is safe.
        for map in mappings {
            map.range_change_blocking(pages(offset), pages(len), RangeChangeOp::RemoveWrite);
        }
        let mut inner = self.get_inner_mut();
        let child = inner.create_child(offset, len, &self.lock);
        drop(inner);
        drop(cow_fault_guard);
        Ok(child?)
    }

    fn append_mapping(&self, mapping: Weak<VmMapping>) {
        let mut inner = self.get_inner_mut();
        inner.mappings.push(mapping.clone());
        // A borrower's mapping holds the cache's frames: let the cache know,
        // so a decommit there unmaps it. (Order: this leaf's family lock,
        // then the cache's borrower list -- a leaf lock -- never a family
        // lock inside another.)
        if let Some((cache, base, _)) = &inner.cache {
            cache.register_borrower_mapping(mapping, base / PAGE_SIZE);
        }
    }

    fn is_file_backed(&self) -> bool {
        self.get_inner().source.is_some()
    }

    fn is_shared(&self) -> bool {
        self.shared.load(core::sync::atomic::Ordering::Relaxed)
    }

    fn cow_fault_seq(&self) -> u64 {
        self.cow_fault_seq.load(Ordering::Acquire)
    }

    fn decommit_seq(&self) -> (u64, u64) {
        // Read `end` BEFORE `begin`, here and in the cache below. Both
        // counters only ever grow and `begin >= end` holds at every instant,
        // so in this order `begin == end` proves that nothing was in flight at
        // the moment `begin` was read; the opposite order could miss a
        // decommit that started between the two loads.
        //
        // The cache's counters are summed into ours rather than returned
        // separately: monotone sums are equal only if each part is equal, so
        // one pair still answers both "is anything in flight" and "did
        // anything happen since". A borrower therefore inherits its cache's
        // decommits -- the frames its faults publish belong to the cache.
        let (cache_begin, cache_end) = match &self.cache_ref {
            Some(cache) => cache.decommit_seq(),
            None => (0, 0),
        };
        let end = self.decommit_end.load(Ordering::Acquire);
        let begin = self.decommit_begin.load(Ordering::Acquire);
        (begin + cache_begin, end + cache_end)
    }

    fn register_borrower_mapping(&self, mapping: Weak<VmMapping>, base_page: usize) {
        let mut list = self.borrower_maps.lock();
        list.retain(|(m, _)| m.strong_count() > 0);
        list.push((mapping, base_page));
    }

    fn is_borrower(&self) -> bool {
        self.get_inner().cache.is_some()
    }

    fn set_shared(&self) {
        self.shared
            .store(true, core::sync::atomic::Ordering::Relaxed);
    }

    fn remove_mapping(&self, mapping: Weak<VmMapping>) {
        // Drop the target mapping and any dead weak refs in a single in-place
        // pass, without taking and rebuilding the vector.
        self.get_inner_mut()
            .mappings
            .retain(|x| x.strong_count() > 0 && !Weak::ptr_eq(x, &mapping));
    }

    fn complete_info(&self, info: &mut VmoInfo) {
        let inner = self.get_inner();
        info.flags |= VmoInfoFlags::TYPE_PAGED;
        inner.complete_info(info);
    }

    fn cache_policy(&self) -> CachePolicy {
        let inner = self.get_inner();
        inner.cache_policy
    }

    fn set_cache_policy(&self, policy: CachePolicy) -> ZxResult {
        // conditions for allowing the cache policy to be set:
        // 1) vmo either has no pages committed currently or is transitioning from being cached
        // 2) vmo has no pinned pages
        // 3) vmo has no mappings
        // 4) vmo has no children (TODO)
        // 5) vmo is not a child
        let mut inner = self.get_inner_mut();
        if !inner.frames.is_empty() && inner.cache_policy != CachePolicy::Cached {
            return Err(ZxError::BAD_STATE);
        }
        inner.clear_invalild_mappings();
        if !inner.mappings.is_empty() {
            return Err(ZxError::BAD_STATE);
        }
        if inner.parent.is_some() {
            return Err(ZxError::BAD_STATE);
        }
        if inner.pin_count != 0 {
            return Err(ZxError::BAD_STATE);
        }
        if inner.cache_policy == CachePolicy::Cached && policy != CachePolicy::Cached {
            for value in inner.frames.values() {
                kernel_hal::mem::frame_flush(value.frame.paddr());
            }
        }
        inner.cache_policy = policy;
        Ok(())
    }

    fn committed_pages_in_range(&self, start_idx: usize, end_idx: usize) -> usize {
        let inner = self.get_inner();
        inner.committed_pages_in_range(start_idx, end_idx)
    }

    fn pin(&self, offset: usize, len: usize) -> ZxResult {
        let mut inner = self.get_inner_mut();
        if offset + len > inner.size {
            return Err(ZxError::OUT_OF_RANGE);
        }
        if len == 0 {
            return Ok(());
        }
        let start_page = offset / PAGE_SIZE;
        let end_page = pages(offset + len);
        for i in start_page..end_page {
            // Not `unwrap`: `zx_bti_pin` commits the range and then pins it,
            // and those are two turns of this lock. A `zx_vmo_op_range`
            // decommit in between -- which nothing refuses, because the pages
            // are not pinned yet -- left this looking for a frame that is no
            // longer there, and that was a kernel panic.
            let frame = inner.frames.get(&i).ok_or(ZxError::BAD_STATE)?;
            if frame.pin_count == VM_PAGE_OBJECT_MAX_PIN_COUNT {
                return Err(ZxError::UNAVAILABLE);
            }
        }
        // Every frame in the range was there a moment ago and this lock has
        // not been let go of since.
        for i in start_page..end_page {
            inner.frames.get_mut(&i).unwrap().pin_count += 1;
            inner.pin_count += 1;
        }
        Ok(())
    }

    fn unpin(&self, offset: usize, len: usize) -> ZxResult {
        let mut inner = self.get_inner_mut();
        if offset + len > inner.size {
            return Err(ZxError::OUT_OF_RANGE);
        }
        if len == 0 {
            return Ok(());
        }
        let start_page = offset / PAGE_SIZE;
        let end_page = pages(offset + len);
        for i in start_page..end_page {
            let frame = inner.frames.get(&i).ok_or(ZxError::BAD_STATE)?;
            if frame.pin_count == 0 {
                return Err(ZxError::UNAVAILABLE);
            }
        }
        if inner.pin_count == 0 {
            // The loop above established that every frame in the range is
            // pinned, and the object's own count is their sum, so this says
            // the two disagree. It was an `assert_ne!`: no guard at all in
            // release, where the `-= 1` below wraps instead, and in debug a
            // kernel panic raised from wherever the unpin came from -- which
            // is usually `PinnedMemoryToken::drop`, so it aborted rather than
            // unwound. An answer the caller can read is better than both.
            error!(
                "unpinning {:#x}..{:#x} of a vmo whose own pin count is zero",
                offset,
                offset + len,
            );
            return Err(ZxError::BAD_STATE);
        }
        for i in start_page..end_page {
            inner.frames.get_mut(&i).unwrap().pin_count -= 1;
            inner.pin_count -= 1;
        }
        Ok(())
    }

    fn is_contiguous(&self) -> bool {
        self.get_inner().is_contiguous()
    }

    fn is_paged(&self) -> bool {
        true
    }

    fn committed_paddr(&self, page_idx: usize) -> Option<PhysAddr> {
        let inner = self.get_inner();
        inner.frames.get(&page_idx).map(|s| s.frame.paddr())
    }

    fn is_demand_paged(&self) -> bool {
        self.get_inner().source.is_some()
    }

    fn fork_copy(&self) -> ZxResult<Arc<dyn VMObjectTrait>> {
        VMObjectPaged::fork_copy(self).map(|v| v as Arc<dyn VMObjectTrait>)
    }

    fn as_mut_buf(&self) -> ZxResult<(MutexGuard<'_, ()>, &mut [u8])> {
        // This is the one site that hands the family lock OUT to the caller
        // (the returned slice aliases the contiguous frames, so the lock must
        // outlive it). Take the lock manually and keep the RefCell borrow as a
        // statement-scoped temporary: it dies at the end of the second
        // statement, strictly before the guard leaves this function — the same
        // borrow-dies-before-mutex-releases contract the guard newtypes enforce.
        let guard = self.lock.lock();
        let res = self.inner.borrow_mut().as_mut_buf();
        res.map(|(addr, size)| {
            (guard, unsafe {
                core::slice::from_raw_parts_mut(addr as *mut u8, size)
            })
        })
    }

    fn unset_contiguous(&self) {
        let mut inner = self.get_inner_mut();
        if inner.contiguous {
            inner.contiguous = false;
            for frame in inner.frames.values_mut() {
                frame.pin_count -= 1;
            }
        }
    }
}

enum CommitResult {
    /// A reference to existing page.
    Ref(PhysAddr),
    /// A new page copied-on-write.
    /// the bool value indicate should we unmap the page after the copy
    CopyOnWrite(PhysFrame, bool),
    /// A new zero page.
    NewPage(PhysFrame),
}

impl VMObjectPagedInner {
    fn validate_create_child(&self) -> ZxResult {
        // clone contiguous vmo is no longer permitted
        // https://fuchsia.googlesource.com/fuchsia/+/e6b4c6751bbdc9ed2795e81b8211ea294f139a45
        if self.is_contiguous() {
            return Err(ZxError::INVALID_ARGS);
        }
        if self.cache_policy != CachePolicy::Cached || self.pin_count != 0 {
            return Err(ZxError::BAD_STATE);
        }
        // A page-cache borrower must never enter the hidden-node tree: the
        // hidden parent would take over page resolution and knows nothing of
        // the cache, so the child's clean pages would silently read as zeros.
        // Refusing here makes COW-fork (`try_cow_child`) fall back to the
        // eager `fork_copy`, which carries the borrow correctly.
        if self.cache.is_some() {
            return Err(ZxError::NOT_SUPPORTED);
        }
        Ok(())
    }

    /// Helper function to split range into sub-ranges within pages.
    ///
    /// All covered pages will be committed implicitly.
    ///
    /// ```text
    /// VMO range:
    /// |----|----|----|----|----|
    ///
    /// buf:
    ///            [====len====]
    /// |--offset--|
    ///
    /// sub-ranges:
    ///            [===]
    ///                [====]
    ///                     [==]
    /// ```
    ///
    /// `f` is a function to process in-page ranges.
    /// It takes 2 arguments:
    /// * `paddr`: the start physical address of the in-page range.
    /// * `buf_range`: the range in view of the input buffer.
    fn for_each_page(
        &mut self,
        offset: usize,
        buf_len: usize,
        flags: MMUFlags,
        range_changes: &mut Vec<DeferredRangeChange>,
        mut f: impl FnMut(PhysAddr, Range<usize>),
    ) -> ZxResult {
        // The range has to be inside this object. Without this, `read` and
        // `write` -- the only two callers -- committed pages past the end:
        // a `read` at four pages into a one-page VMO answered `Ok(())` after
        // resolving page 4 through the demand-zero path, and the matching
        // `write` would have allocated a frame at an index `len()` says does
        // not exist, where nothing ever frees it. The bound belongs here and
        // not in the two callers because `zero` below has its own, spelled the
        // same way. `zx_vmo_read`/`zx_vmo_write` do check their arguments, so
        // this is the object defending an invariant of its own rather than a
        // second copy of the syscall's check: `VmMapping::read_memory`
        // reaches the same method with an offset it derives from a mapping.
        let end = offset.checked_add(buf_len).ok_or(ZxError::OUT_OF_RANGE)?;
        if end > self.size {
            return Err(ZxError::OUT_OF_RANGE);
        }
        let iter = BlockIter {
            begin: offset,
            end,
            block_size_log2: PAGE_SIZE_LOG2 as u8,
        };
        for block in iter {
            let paddr = self.commit_page(block.block, flags, range_changes)?;
            let buf_range = block.origin_begin() - offset..block.origin_end() - offset;
            f(paddr + block.begin, buf_range);
        }
        Ok(())
    }

    fn commit_page(
        &mut self,
        page_idx: usize,
        flags: MMUFlags,
        range_changes: &mut Vec<DeferredRangeChange>,
    ) -> ZxResult<PhysAddr> {
        let ret = match self.commit_page_internal(page_idx, flags, &Weak::new(), range_changes)? {
            CommitResult::Ref(paddr) => Ok(paddr),
            _ => unreachable!(),
        };
        // Validate physical contiguity only in debug builds; this runs on
        // every page commit and is O(committed pages) for contiguous VMOs.
        debug_assert!(self.check_contig());
        ret
    }

    /// Commit a page recursively.
    fn commit_page_internal(
        &mut self,
        page_idx: usize,
        flags: MMUFlags,
        child: &WeakRef,
        range_changes: &mut Vec<DeferredRangeChange>,
    ) -> ZxResult<CommitResult> {
        // special case
        let no_parent = self.parent.is_none();
        let no_frame = !self.frames.contains_key(&page_idx);
        let out_of_range = if self.type_.is_hidden() || self.parent.is_none() {
            page_idx >= self.size / PAGE_SIZE
        } else {
            (self.parent_offset + page_idx * PAGE_SIZE) >= self.parent_limit
        };
        let mut need_unmap = false;
        if no_frame {
            // if out_of_range
            if out_of_range || no_parent {
                // Page-cache borrow (MAP_PRIVATE file mappings). A read fault
                // resolves to the CACHE's frame -- the fault handler installs
                // it read-only precisely because the access was a read, which
                // is the same write-protection COW has always relied on here.
                // A write fault copies that one page into a private frame
                // (copy-up) and falls through to the common tail, which hands
                // the private frame back. Pages beyond the cache window (the
                // BSS tail of a mapping) fall through to ordinary demand-zero.
                //
                // Lock order: this runs under OUR family lock and takes the
                // CACHE's family lock inside `commit_page`. The two are
                // different objects in different families, and a cache never
                // references a borrower, so the order cannot invert.
                if !out_of_range {
                    if let Some((cache, base, cache_len)) = &self.cache {
                        let cache_byte = base + page_idx * PAGE_SIZE;
                        if cache_byte < *cache_len {
                            let src = cache.commit_page(cache_byte / PAGE_SIZE, MMUFlags::READ)?;
                            if !flags.contains(MMUFlags::WRITE) {
                                return Ok(CommitResult::Ref(src));
                            }
                            let target_frame = PhysFrame::new().ok_or(ZxError::NO_MEMORY)?;
                            kernel_hal::mem::pmem_copy(target_frame.paddr(), src, PAGE_SIZE);
                            self.frames.insert(page_idx, PageState::new(target_frame));
                        }
                    }
                }
                // A copy-up above already landed the private page; only a page
                // still missing takes the demand-zero / demand-file path.
                if !self.frames.contains_key(&page_idx) {
                    // Demand paging: if this root node is file-backed and the page
                    // lies within the source, it must be filled from the file — even
                    // on a read fault, since a shared zero page would expose zeros
                    // instead of the file's contents.
                    let from_source = self
                        .source
                        .as_ref()
                        .is_some_and(|s| page_idx * PAGE_SIZE < s.source_len());
                    if !flags.contains(MMUFlags::WRITE) && !from_source {
                        // read-only, just return zero frame
                        return Ok(CommitResult::Ref(kernel_hal::mem::ZERO_FRAME.paddr()));
                    }
                    // lazy allocate zero frame
                    // 这里会调用HAL层的hal_frame_alloc, 请注意实现该函数时参数要一样
                    let target_frame = PhysFrame::new_zero().ok_or(ZxError::NO_MEMORY)?;
                    if from_source {
                        // Read the page from the backing file into the fresh frame.
                        // The frame is already zeroed, so bytes past end-of-file stay
                        // zero (BSS tail of the mapping).
                        let source = self.source.as_ref().unwrap();
                        let vaddr = phys_to_virt(target_frame.paddr());
                        let buf =
                            unsafe { core::slice::from_raw_parts_mut(vaddr as *mut u8, PAGE_SIZE) };
                        source.fill_page(page_idx * PAGE_SIZE, buf);
                    }
                    if out_of_range {
                        // can never be a hidden vmo
                        assert!(!self.type_.is_hidden());
                    }
                    if self.type_.is_hidden() {
                        return Ok(CommitResult::NewPage(target_frame));
                    }
                    self.frames.insert(page_idx, PageState::new(target_frame));
                }
            } else {
                // recursively find a frame in parent
                let mut parent = self.parent.as_ref().unwrap().inner.borrow_mut();
                let parent_idx = page_idx + self.parent_offset / PAGE_SIZE;
                match parent.commit_page_internal(
                    parent_idx,
                    flags,
                    &self.self_ref,
                    range_changes,
                )? {
                    CommitResult::NewPage(frame) if !self.type_.is_hidden() => {
                        self.frames.insert(page_idx, PageState::new(frame));
                    }
                    CommitResult::CopyOnWrite(frame, unmap) => {
                        let mut new_frame = PageState::new(frame);
                        // Cloning a contiguous vmo: original frames are stored in hidden parent nodes.
                        // In order to make sure original vmo (now is a child of hidden parent)
                        // owns physically contiguous frames, swap the new frame with the original
                        if self.contiguous {
                            if let Some(par_frame) = parent.frames.get_mut(&parent_idx) {
                                par_frame.swap(&mut new_frame);
                            }
                            let sibling = parent.type_.get_tag_and_other(&self.self_ref).1;
                            if let Some(arc_sibling) = sibling.upgrade() {
                                {
                                    let sibling_inner = arc_sibling.inner.borrow();
                                    sibling_inner.collect_range_change(
                                        parent_idx * PAGE_SIZE,
                                        (parent_idx + 1) * PAGE_SIZE,
                                        RangeChangeOp::Unmap,
                                        range_changes,
                                    );
                                }
                                // Possibly-last ref, held under the family
                                // lock: defer its drop (see `stash_defer`).
                                stash_defer(arc_sibling);
                            }
                        } else {
                            need_unmap = need_unmap || unmap;
                        }
                        self.frames.insert(page_idx, new_frame);
                    }
                    r => return Ok(r),
                }
            }
        }
        // now the page must hit on this VMO
        let (child_tag, other_child) = self.type_.get_tag_and_other(child);
        // Trap (budgeted): a HIDDEN node asked to commit by a caller it does
        // not recognize as either child gets `Owned` here — and the write
        // path below then degrades to handing back the SHARED frame as a
        // plain `Ref`, giving the faulting side a writable PTE onto memory
        // its sibling still reads: silent cross-fork corruption. This should
        // be impossible (the family lock serializes reshapes); if this line
        // ever fires, it is the smoking gun for the torn-snapshot pages the
        // fork hammer caught.
        if self.type_.is_hidden() && child_tag == PageStateTag::Owned {
            use core::sync::atomic::{AtomicU32, Ordering};
            static TRAP_BUDGET: AtomicU32 = AtomicU32::new(0);
            if TRAP_BUDGET.fetch_add(1, Ordering::Relaxed) < 8 {
                log::error!(
                    "[cow] hidden node got commit_page from an UNKNOWN child (page_idx={}, write={}) -- write path would hand back the SHARED frame",
                    page_idx,
                    flags.contains(MMUFlags::WRITE)
                );
            }
        }
        if self.type_.is_hidden() {
            if let Some(arc_other) = other_child.upgrade() {
                let early_return = {
                    let other_inner = arc_other.inner.borrow();
                    let in_range = {
                        let start = other_inner.parent_offset / PAGE_SIZE;
                        let end = other_inner.parent_limit / PAGE_SIZE;
                        page_idx >= start && page_idx < end
                    };
                    if !in_range {
                        Some(self.frames.remove(&page_idx).unwrap().take())
                    } else {
                        if need_unmap {
                            other_inner.collect_range_change(
                                page_idx * PAGE_SIZE,
                                (1 + page_idx) * PAGE_SIZE,
                                RangeChangeOp::Unmap,
                                range_changes,
                            );
                        }
                        None
                    }
                };
                // Defer BEFORE the possible early return: on both paths this is
                // a possibly-last ref dying under the family lock.
                stash_defer(arc_other);
                if let Some(frame) = early_return {
                    return Ok(CommitResult::CopyOnWrite(frame, need_unmap));
                }
            }
            // Sibling VMO already dropped (fork/COW tree reshaped): skip hidden
            // sibling bookkeeping and fall through to the leaf commit path below.
        }
        if need_unmap {
            self.collect_mapping_range_changes(page_idx, 1, RangeChangeOp::Unmap, range_changes);
        }
        let frame = self.frames.get_mut(&page_idx).unwrap();
        if frame.tag.is_split() {
            // has split, take out
            let target_frame = self.frames.remove(&page_idx).unwrap().take();
            return Ok(CommitResult::CopyOnWrite(target_frame, need_unmap));
        } else if flags.contains(MMUFlags::WRITE) && child_tag.is_split() {
            // copy-on-write
            let target_frame = PhysFrame::new().ok_or(ZxError::NO_MEMORY)?;
            kernel_hal::mem::pmem_copy(target_frame.paddr(), frame.frame.paddr(), PAGE_SIZE);
            frame.tag = child_tag;
            return Ok(CommitResult::CopyOnWrite(target_frame, true));
        }
        // otherwise already committed
        Ok(CommitResult::Ref(frame.frame.paddr()))
    }

    fn decommit(&mut self, page_idx: usize) {
        self.frames.remove(&page_idx);
    }

    fn collect_mapping_range_changes(
        &self,
        offset: usize,
        len: usize,
        op: RangeChangeOp,
        range_changes: &mut Vec<DeferredRangeChange>,
    ) -> u64 {
        let mut dead = 0;
        for map in self.mappings.iter() {
            if let Some(map) = map.upgrade() {
                range_changes.push(DeferredRangeChange::new(map, offset, len, op));
            } else {
                dead += 1;
            }
        }
        dead
    }

    fn collect_live_mappings(&self) -> (Vec<Arc<VmMapping>>, u64) {
        let mut dead = 0;
        let mut mappings = Vec::with_capacity(self.mappings.len());
        for map in self.mappings.iter() {
            if let Some(map) = map.upgrade() {
                mappings.push(map);
            } else {
                dead += 1;
            }
        }
        (mappings, dead)
    }

    fn collect_range_change(
        &self,
        parent_offset: usize,
        parent_limit: usize,
        op: RangeChangeOp,
        range_changes: &mut Vec<DeferredRangeChange>,
    ) {
        let mut start = self.parent_offset.max(parent_offset);
        let mut end = self.parent_limit.min(parent_limit);
        if start >= end {
            return;
        }
        start -= self.parent_offset;
        end -= self.parent_offset;
        self.collect_mapping_range_changes(
            pages(start),
            pages(end) - pages(start),
            op,
            range_changes,
        );
        if let VMOType::Hidden { left, right, .. } = &self.type_ {
            for child in &[left, right] {
                if let Some(child) = child.upgrade() {
                    child
                        .inner
                        .borrow()
                        .collect_range_change(start, end, op, range_changes);
                    // `child` was obtained by upgrading a Weak in `type_`, so it
                    // is a POSSIBLY-LAST strong ref: if a concurrent drop just
                    // released the tree's other strong ref, letting `child` die
                    // in-scope here re-enters the shared family lock via
                    // VMObjectPaged::drop and self-deadlocks. Reproduced live as
                    //   commit_page (:773, family lock held)
                    //     -> range_change (:1231, this drop)
                    //       -> VMObjectPaged::drop (:1803, re-acquire) — DEADLOCK.
                    // Defer the drop to the family guard's StashDrain, exactly as
                    // commit_page_internal already does for arc_sibling/arc_other.
                    // Sound because every caller of this method holds the family
                    // lock (1136/1170 in commit_page_internal, plus this
                    // recursion), which is stash_defer's precondition.
                    stash_defer(child);
                }
            }
        }
    }

    /// Count committed pages of the VMO.
    fn committed_pages_in_range(&self, start_idx: usize, end_idx: usize) -> usize {
        // Clamp to this object rather than assert. A question about pages past
        // the end has an answer -- nothing is committed there -- and the two
        // assertions this replaces were a kernel panic on a counting path:
        // `VmMapping::fill_task_stats`, which is what reading
        // `/proc/<pid>/status` runs, carries a hand-written clamp and an
        // early return purely to stay out of them, and `VMObjectSlice`
        // forwards indices here after shifting them by its own offset.
        let pages = self.size / PAGE_SIZE;
        let start_idx = start_idx.min(pages);
        let end_idx = end_idx.min(pages);
        let mut count = 0;
        for i in start_idx..end_idx {
            if self.frames.contains_key(&i) {
                count += 1;
                continue;
            }
            if self.parent_limit <= i * PAGE_SIZE {
                continue;
            }
            let mut current = self.parent.clone();
            let mut current_idx = i + self.parent_offset / PAGE_SIZE;
            while let Some(vmop) = current {
                let inner = vmop.inner.borrow();
                if let Some(frame) = inner.frames.get(&current_idx) {
                    if frame.tag.is_split() || inner.owner == self.owner {
                        count += 1;
                        break;
                    }
                }
                if inner.owner != self.owner {
                    break;
                }
                current_idx += inner.parent_offset / PAGE_SIZE;
                if current_idx >= inner.parent_limit / PAGE_SIZE {
                    break;
                }
                current = inner.parent.clone();
            }
        }
        count
    }

    /// Remove one child and contract hidden node.
    ///
    /// ```text
    ///    |         |
    ///    H         |
    ///   / \    =>  |
    ///  A   B       B
    ///  ^remove
    /// ```
    /// Returns the surviving sibling's `Arc`, which the caller MUST drop only
    /// after releasing the family lock.
    ///
    /// Every caller of this function holds the family lock, and the upgrade
    /// below takes a strong reference to the sibling. If that reference dies
    /// here and another CPU has concurrently dropped the sibling's last other
    /// `Arc` (a parent munmapping while its exited child's snapshots are torn
    /// down — `fork + exit` in a loop manufactures exactly this), then dropping
    /// ours runs `Drop for VMObjectPaged` on THIS cpu, which re-enters
    /// `get_inner_mut` on the ticket lock this cpu already holds. A ticket lock
    /// is not re-entrant: that is a self-deadlock, observed as
    /// `DEADLOCK: cpu=N at paged.rs:391` (the lock acquire in `get_inner_mut`)
    /// with the same cpu as holder. `set_len` already handles its parent `Arc`
    /// this way ("pass it to caller who can drop it after unlocking"); this is
    /// the same rule applied to the reference this function itself creates.
    /// Every `Arc` this path upgrades or clones lands in `deferred`, which the
    /// outermost frame drops only after releasing the family lock.
    fn remove_child(&mut self, child: &WeakRef, deferred: &mut Vec<Arc<VMObjectPaged>>) {
        drop_crumb(2); // remove_child entered
                       // a child slice do not have to belong to a hidden parent
        if !self.type_.is_hidden() {
            drop_crumb(3); // parent not hidden: early return
            return;
        }
        let (tag, other_child) = self.type_.get_tag_and_other(child);
        // [fossil-hunt] Budgeted traps on the two silent degradations of the
        // hidden-node collapse. Both leave the tree in a shape the merge
        // conditions below were never written for -- prime suspects for the
        // fork-hammer's "child sees a frame from many generations ago".
        if tag == PageStateTag::Owned {
            // The dying child is not one of this hidden node's two children:
            // the merge below would use a MEANINGLESS side tag and migrate /
            // skip the wrong frames.
            fossil_trap("remove_child: dying child UNKNOWN to hidden parent");
        }
        let Some(arc_child) = other_child.upgrade() else {
            drop_crumb(4); // sibling dead: early return
                           // The tree stays UNCOLLAPSED: this hidden node keeps its frames
                           // and its dangling child ref. Resolution still walks it, but its
                           // split-tag bookkeeping is now frozen mid-transaction.
            fossil_trap("remove_child: sibling already dead, tree left uncollapsed");
            return;
        };
        drop_crumb(5); // sibling upgraded
        let mut child = arc_child.inner.borrow_mut();
        let start = child.parent_offset / PAGE_SIZE;
        let end = child.parent_limit / PAGE_SIZE;
        // merge nodes to the child
        for (key, mut value) in core::mem::take(&mut self.frames) {
            if key < start || key >= end {
                continue;
            }
            if self.contiguous && !child.contiguous && value.pin_count >= 1 {
                value.pin_count -= 1;
            }
            let idx = key - start;
            if !child.frames.contains_key(&idx) && value.tag != tag.negate() {
                value.tag = PageStateTag::Owned;
                child.frames.insert(idx, value);
            }
        }
        // connect child to my parent
        child.parent_offset += self.parent_offset;
        child.parent_limit += self.parent_offset;
        if let Some(parent) = &self.parent {
            parent.inner.borrow_mut().replace_child(
                &self.self_ref,
                self.owner,
                other_child,
                Some((child.parent_offset, child.parent_limit)),
                deferred,
            );
        }
        // The surviving child takes over as root; carry the demand-paging source
        // along so its still-uncommitted pages keep resolving from the file.
        if child.source.is_none() {
            child.source = self.source.take();
        }
        drop_crumb(6); // about to overwrite sibling.parent
                       // The sibling's old parent Arc (this hidden node) is kept alive by the
                       // dropping child's own field until its fields drop, but route it
                       // through `deferred` anyway: the invariant is "no Arc of this family
                       // dies under the family lock", not a per-site survivability proof.
        if let Some(old_parent) = child.parent.take() {
            deferred.push(old_parent);
        }
        child.parent = self.parent.take();
        drop(child);
        deferred.push(arc_child);
    }

    /// Create a snapshot child VMO.
    fn create_child(
        &mut self,
        offset: usize,
        len: usize,
        lock_ref: &Arc<Mutex<()>>,
    ) -> ZxResult<Arc<VMObjectPaged>> {
        self.validate_create_child()?;
        // create child VMO
        let child = VMObjectPaged::wrap(
            VMObjectPagedInner {
                owner: new_owner_id(),
                type_: VMOType::Snapshot,
                parent: None, // set later
                parent_offset: offset,
                parent_limit: (offset + len).min(self.size),
                size: len,
                frames: BTreeMap::new(),
                mappings: Vec::new(),
                cache_policy: CachePolicy::Cached,
                contiguous: false,
                self_ref: Default::default(),
                pin_count: 0,
                // The new snapshot child resolves uncommitted pages through the
                // shared hidden parent (which carries the source), not directly.
                source: None,
                cache: None,
            },
            Some(lock_ref.clone()),
        );
        // construct a hidden VMO as shared parent
        let hidden = VMObjectPaged::wrap(
            VMObjectPagedInner {
                owner: self.owner,
                type_: VMOType::Hidden {
                    left: self.self_ref.clone(),
                    right: Arc::downgrade(&child),
                },
                parent: self.parent.clone(),
                parent_offset: self.parent_offset,
                parent_limit: self.parent_limit,
                size: self.size,
                frames: core::mem::take(&mut self.frames),
                mappings: Vec::new(),
                cache_policy: CachePolicy::Cached,
                contiguous: self.contiguous,
                self_ref: Default::default(),
                pin_count: self.pin_count,
                // The hidden node becomes the shared root for both children, so
                // it must keep demand-paging the file: hand the source over to it.
                source: self.source.take(),
                cache: None,
            },
            Some(lock_ref.clone()),
        );
        // update parent's child
        if let Some(parent) = self.parent.take() {
            if let VMOType::Hidden { left, right, .. } = &mut parent.inner.borrow_mut().type_ {
                if left.ptr_eq(&self.self_ref) {
                    *left = Arc::downgrade(&hidden);
                } else if right.ptr_eq(&self.self_ref) {
                    *right = Arc::downgrade(&hidden);
                } else {
                    panic!();
                }
            }
        }
        // update children's parent
        self.parent = Some(hidden.clone());
        self.parent_offset = 0;
        self.parent_limit = self.size;
        child.inner.borrow_mut().parent = Some(hidden);
        Ok(child)
    }

    /// Replace a child of the hidden node.
    /// `new_start` and `new_end` are in bytes
    /// The captured wedge lived here: this function upgrades the grandparent's
    /// OTHER child, and its owner-propagation walk clones an `Arc` per level —
    /// and every one of them used to die in-scope, under the family lock. With
    /// a concurrent teardown dropping the last other reference, that in-scope
    /// death ran `Drop for VMObjectPaged` re-entrantly on the lock-holding cpu:
    /// the self-deadlock the breadcrumb mask pinned between crumbs 5 and 6.
    /// Everything upgraded or cloned here now goes to `deferred` instead.
    fn replace_child(
        &mut self,
        old: &WeakRef,
        old_id: KoID,
        new: WeakRef,
        new_range: Option<(usize, usize)>,
        deferred: &mut Vec<Arc<VMObjectPaged>>,
    ) {
        let (tag, other) = self.type_.get_tag_and_other(old);
        let Some(arc_other_child) = other.upgrade() else {
            if let VMOType::Hidden { left, right, .. } = &mut self.type_ {
                if left.ptr_eq(old) {
                    *left = new;
                } else if right.ptr_eq(old) {
                    *right = new;
                }
            }
            return;
        };
        let mut other_child = arc_other_child.inner.borrow_mut();
        let mut unwanted = VecDeque::<usize>::new();
        if let Some((new_start, new_end)) = new_range {
            let other_start = other_child.parent_offset / PAGE_SIZE;
            let other_end = other_child.parent_limit / PAGE_SIZE;
            let start = new_start / PAGE_SIZE;
            let end = new_end / PAGE_SIZE;
            for i in 0..self.size / PAGE_SIZE {
                let not_in_range =
                    !((start <= i && end > i) || (other_start <= i && other_end > i));
                if not_in_range {
                    // if not in this node's range
                    if self.frames.contains_key(&i) {
                        // if the frame is in our, remove it
                        assert!(self.frames.remove(&i).is_some());
                    } else {
                        // or it is in our ancestor, tell them we do not need it.
                        unwanted.push_back(i + self.parent_offset / PAGE_SIZE);
                    }
                } else {
                    // if in this node's range, check if it can be moved
                    if let Some(frame) = self.frames.get(&i) {
                        if frame.tag.is_split() {
                            let mut new_frame = self.frames.remove(&i).unwrap();
                            if self.contiguous
                                && !other_child.contiguous
                                && new_frame.pin_count >= 1
                            {
                                new_frame.pin_count -= 1;
                            }
                            if new_frame.tag == tag && other_start <= i && other_end > i {
                                new_frame.tag = PageStateTag::Owned;
                                let new_key = i - other_child.parent_offset / PAGE_SIZE;
                                other_child.frames.insert(new_key, new_frame);
                            }
                        }
                    }
                }
            }
        }

        self.release_unwanted_pages_in_parent(unwanted);

        if old_id == self.owner {
            let mut option_parent = self.parent.clone();
            let mut child = self.self_ref.clone();
            let mut skip_owner = old_id;
            while let Some(parent) = option_parent {
                let mut parent_inner = parent.inner.borrow_mut();
                if parent_inner.owner == old_id {
                    let (_, other) = parent_inner.type_.get_tag_and_other(&child);
                    let Some(arc) = other.upgrade() else {
                        drop(parent_inner);
                        deferred.push(parent);
                        break;
                    };
                    let new_owner = arc.inner.borrow().owner;
                    deferred.push(arc);
                    child = parent_inner.self_ref.clone();
                    assert_ne!(new_owner, skip_owner);
                    parent_inner.owner = new_owner;
                    skip_owner = new_owner;
                    option_parent = parent_inner.parent.clone();
                    drop(parent_inner);
                    deferred.push(parent);
                } else {
                    drop(parent_inner);
                    deferred.push(parent);
                    break;
                }
            }
        }

        self.owner = other_child.owner;
        drop(other_child);
        deferred.push(arc_other_child);
        match &mut self.type_ {
            VMOType::Hidden { left, right, .. } => {
                if left.ptr_eq(old) {
                    *left = new;
                } else if right.ptr_eq(old) {
                    *right = new;
                } else {
                    panic!();
                }
            }
            _ => panic!(),
        }
    }

    fn complete_info(&self, info: &mut VmoInfo) {
        if let VMOType::Snapshot = self.type_ {
            info.flags |= VmoInfoFlags::IS_COW_CLONE;
        }
        if self.is_contiguous() {
            info.flags |= VmoInfoFlags::CONTIGUOUS;
        }
        // info.num_children = if self.type_.is_hidden() { 2 } else { 0 };
        info.num_mappings = self.mappings.len() as u64; // FIXME remove weak ptr
        info.share_count = self.mappings.len() as u64; // FIXME share_count should be the count of unique aspace
        info.committed_bytes =
            (self.committed_pages_in_range(0, self.size / PAGE_SIZE) * PAGE_SIZE) as u64;
        info.populated_bytes = info.committed_bytes;
        info.committed_private_bytes = info.committed_bytes;
        info.populated_private_bytes = info.populated_bytes;
        info.committed_scaled_bytes = info.committed_bytes;
        info.populated_scaled_bytes = info.populated_bytes;
    }

    fn release_unwanted_pages_in_parent(&mut self, mut unwanted: VecDeque<usize>) {
        let mut option_parent = self.parent.clone();
        let mut child = self.self_ref.clone();
        while let Some(parent) = option_parent {
            let mut parent_inner = parent.inner.borrow_mut();
            let (tag, other) = parent_inner.type_.get_tag_and_other(&child);
            let Some(arc_other) = other.upgrade() else {
                // `parent` (and any earlier ancestor still in `option_parent`)
                // is a possibly-last ref about to die under the family lock.
                drop(parent_inner);
                stash_defer(parent);
                break;
            };
            let mut other_inner = arc_other.inner.borrow_mut();
            let start = other_inner.parent_offset / PAGE_SIZE;
            let end = other_inner.parent_limit / PAGE_SIZE;
            for _ in 0..unwanted.len() {
                let idx = unwanted.pop_front().unwrap();
                // if the frame is in other_inner's range, check if it can be move to other_inner
                if start <= idx && idx < end {
                    if parent_inner.frames.contains_key(&idx) {
                        let mut to_insert = parent_inner.frames.remove(&idx).unwrap();
                        if parent_inner.contiguous
                            && !other_inner.contiguous
                            && to_insert.pin_count >= 1
                        {
                            to_insert.pin_count -= 1;
                        }
                        if to_insert.tag != tag.negate() {
                            to_insert.tag = PageStateTag::Owned;
                            other_inner.frames.insert(idx - start, to_insert);
                        }
                        unwanted.push_back(idx + parent_inner.parent_offset / PAGE_SIZE);
                    }
                } else {
                    // otherwise, if it exists in our frames, remove it; if not, push_back it again
                    if parent_inner.frames.contains_key(&idx) {
                        parent_inner.frames.remove(&idx);
                    } else {
                        unwanted.push_back(idx + parent_inner.parent_offset / PAGE_SIZE);
                    }
                }
            }
            child = parent_inner.self_ref.clone();
            option_parent = parent_inner.parent.clone();
            drop(parent_inner);
            drop(other_inner);
            // Both `arc_other` and this iteration's `parent` are possibly-last
            // refs; park them rather than let them die under the lock.
            stash_defer(arc_other);
            stash_defer(parent);
        }
    }

    fn resize(&mut self, new_size: usize, deferred: &mut Vec<Arc<VMObjectPaged>>) {
        if new_size == 0 && new_size < self.size {
            self.frames.clear();
            if let Some(parent) = self.parent.as_ref() {
                parent
                    .inner
                    .borrow_mut()
                    .remove_child(&self.self_ref, deferred);
            }
            // We cannot drop the parent Arc here since we are holding the lock
            // pass it to caller who can drop it after unlocking the lock
            if let Some(p) = self.parent.take() {
                deferred.push(p);
            }
            self.parent_offset = 0;
            self.parent_limit = 0;
        } else if new_size < self.size {
            let mut unwanted = VecDeque::<usize>::new();
            let parent_end = (self.parent_limit - self.parent_offset) / PAGE_SIZE;
            for i in new_size / PAGE_SIZE..self.size / PAGE_SIZE {
                self.decommit(i);
                if parent_end > i {
                    unwanted.push_back(i + self.parent_offset / PAGE_SIZE);
                }
            }
            self.release_unwanted_pages_in_parent(unwanted);
            if new_size < self.parent_limit - self.parent_offset {
                self.parent_limit = self.parent_offset + new_size;
            }
        }
        self.size = new_size;
    }

    fn is_contiguous(&self) -> bool {
        self.contiguous
    }

    fn clear_invalild_mappings(&mut self) {
        for x in core::mem::take(&mut self.mappings) {
            if x.strong_count() > 0 {
                self.mappings.push(x);
            }
        }
    }

    /// Check whether it is not physically contiguous when it should be
    fn check_contig(&self) -> bool {
        if !self.contiguous {
            return true;
        }
        let mut base = 0;
        for (key, ps) in self.frames.iter() {
            let new_base = ps.frame.paddr() - key * PAGE_SIZE;
            if base == 0 || new_base == base {
                base = new_base;
            } else {
                return false;
            }
        }
        true
    }

    fn as_mut_buf(&mut self) -> ZxResult<(usize, usize)> {
        if self.contiguous {
            let mut range_changes = Vec::new();
            let addr =
                phys_to_virt(self.commit_page(0, MMUFlags::WRITE, &mut range_changes)?) as usize;
            debug_assert!(range_changes.is_empty());
            let size = self.size;
            return Ok((addr, size));
        }
        Err(ZxError::UNAVAILABLE)
    }
}

/// Per-cpu nesting depth of `Drop for VMObjectPaged`, plus what the OUTER drop
/// was, so a re-entrant drop can name the pair.
///
/// The instant-re-entrancy detector proved a cpu re-acquires the family lock
/// from `Drop` nested inside `Drop` — both acquire sites are the same line, so
/// the lock can no longer narrow it. This runs BEFORE the nested acquire would
/// wedge, and prints which two objects (type, owner) are involved: the fact
/// that decides between the candidate re-entry vectors, none of which static
/// analysis has managed to confirm.
// Same real-ceiling sizing as STASH_CPUS above; these three arrays are
// atomics-only diagnostics (no ownership/Drop hazard), so unlike STASH_CPUS
// there is no clamp-vs-hard-bound distinction to worry about here.
const DROP_TRACK_CPUS: usize = kernel_hal::config::MAX_CORE_NUM;
static DROP_DEPTH: [AtomicU64; DROP_TRACK_CPUS] = [const { AtomicU64::new(0) }; DROP_TRACK_CPUS];
static DROP_OUTER: [AtomicU64; DROP_TRACK_CPUS] = [const { AtomicU64::new(0) }; DROP_TRACK_CPUS];
/// Breadcrumb bitmask of the points the OUTER drop's lock scope has passed, so
/// the re-entry line can say exactly BETWEEN which two points the inner Drop
/// began. Set with relaxed stores; reset on each outer entry.
static DROP_CRUMBS: [AtomicU64; DROP_TRACK_CPUS] = [const { AtomicU64::new(0) }; DROP_TRACK_CPUS];

/// Mark breadcrumb `bit` for the current cpu's in-flight outer Drop.
fn drop_crumb(bit: u64) {
    let cpu = (kernel_hal::cpu::cpu_id() as usize).min(DROP_TRACK_CPUS - 1);
    DROP_CRUMBS[cpu].fetch_or(1 << bit, Ordering::Relaxed);
}

fn drop_tag(inner: &VMObjectPagedInner) -> u64 {
    let ty = match inner.type_ {
        VMOType::Origin => 1u64,
        VMOType::Snapshot => 2,
        VMOType::Hidden { .. } => 3,
    };
    (ty << 60) | (inner.owner & 0x0fff_ffff_ffff_ffff)
}

impl Drop for VMObjectPaged {
    fn drop(&mut self) {
        VMO_PAGED_DROPPED.fetch_add(1, Ordering::Relaxed);
        let cpu = (kernel_hal::cpu::cpu_id() as usize).min(DROP_TRACK_CPUS - 1);
        // `borrow()` without the family lock is safe here only for reading the
        // tag: this object is at strong=0, nobody else holds a live borrow of
        // ITS RefCell (borrows require going through get_inner*, which needs an
        // Arc). The PARENT's RefCell is not touched.
        let my_tag = drop_tag(&self.inner.borrow());
        let depth = DROP_DEPTH[cpu].fetch_add(1, Ordering::Relaxed);
        if depth > 0 {
            let outer = DROP_OUTER[cpu].load(Ordering::Relaxed);
            let crumbs = DROP_CRUMBS[cpu].load(Ordering::Relaxed);
            // Serial, lock-free, before the nested family-lock acquire wedges:
            // this line must escape even though the machine is about to stop.
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[VMO-DROP-REENTRY cpu={} outer=(ty{} owner{}) inner=(ty{} owner{}) crumbs={:#x}]\n",
                cpu,
                outer >> 60,
                outer & 0x0fff_ffff_ffff_ffff,
                my_tag >> 60,
                my_tag & 0x0fff_ffff_ffff_ffff,
                crumbs,
            ));
        } else {
            DROP_OUTER[cpu].store(my_tag, Ordering::Relaxed);
            DROP_CRUMBS[cpu].store(0, Ordering::Relaxed);
        }
        // Scoped so the family lock is released before `deferred` — the
        // sibling `Arc` that `remove_child` upgraded — is dropped. Dropping it
        // under the lock is the re-entrant self-deadlock described on
        // `remove_child`.
        let mut deferred: Vec<Arc<VMObjectPaged>> = Vec::new();
        {
            let mut inner = self.get_inner_mut();
            drop_crumb(0); // scope entered, family lock held
            if inner.type_.is_hidden() {
                VMO_HIDDEN_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
            // remove self from parent
            if let Some(parent) = &inner.parent {
                drop_crumb(1); // parent present, about to borrow it
                parent
                    .inner
                    .borrow_mut()
                    .remove_child(&inner.self_ref, &mut deferred);
                drop_crumb(7); // remove_child returned, parent RefMut dropped
            }
            drop_crumb(8); // entering the frame pin sweep
            let is_conti = inner.is_contiguous();
            for frame in inner.frames.iter_mut() {
                if is_conti {
                    // WARN: In fact we do not need this `if`.
                    // If this vmo is a child of a contiguous vmo,
                    // its pages should also be pinned.
                    if frame.1.pin_count >= 1 {
                        frame.1.pin_count -= 1;
                    }
                }
                if frame.1.pin_count != 0 {
                    // A frame still pinned here means a device was told it
                    // could do DMA into a page that is on its way back to the
                    // allocator, and the token that made that promise is
                    // already gone. Worth saying loudly; not worth a panic.
                    // This is a destructor, and a panic in one aborts instead
                    // of unwinding, so the report costs the whole machine --
                    // and it is where `PinnedMemoryToken`'s own `Drop` would
                    // land one frame later, now that a refused `unpin` there
                    // is reported rather than asserted. Same hole, one report.
                    error!(
                        "vmo dropped with page {} still pinned {} time(s)",
                        frame.0, frame.1.pin_count,
                    );
                }
            }
            drop_crumb(9); // scope tail: about to release the family lock
        }
        // Depth closes BEFORE the deferred Arcs drop: those drops run with the
        // family lock released and may legitimately nest more Drops — that is
        // the deferral working, not the bug. Only a Drop that begins while the
        // lock-holding scope above is still open is the wedge.
        DROP_DEPTH[cpu].fetch_sub(1, Ordering::Relaxed);
        drop(deferred);
    }
}

/// Generate a owner ID.
fn new_owner_id() -> u64 {
    static OWNER_ID: AtomicU64 = AtomicU64::new(1);
    OWNER_ID.fetch_add(1, Ordering::SeqCst)
}

const VM_PAGE_OBJECT_MAX_PIN_COUNT: u8 = 31;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write() {
        let vmo = VmObject::new_paged(2);
        super::super::tests::read_write(&*vmo);
    }

    #[test]
    fn create_child() {
        let vmo = VmObject::new_paged(1);
        let child_vmo = vmo.create_child(false, 0, PAGE_SIZE).unwrap();

        // write to parent and make sure clone doesn't see it
        vmo.test_write(0, 1);
        assert_eq!(vmo.test_read(0), 1);
        assert_eq!(child_vmo.test_read(0), 0);

        // write to clone and make sure parent doesn't see it
        child_vmo.test_write(0, 2);
        assert_eq!(vmo.test_read(0), 1);
        assert_eq!(child_vmo.test_read(0), 2);
    }

    #[test]
    fn fork_copy_lazy() {
        struct Filler;
        impl FrameFiller for Filler {
            fn source_len(&self) -> usize {
                2 * PAGE_SIZE
            }
            fn fill_page(&self, offset: usize, buf: &mut [u8]) {
                buf.fill(if offset == 0 { 0xaa } else { 0xbb });
            }
        }
        let vmo = VmObject::new_paged_with_source(2, Arc::new(Filler));
        // Touch (and modify) only page 0; page 1 stays uncommitted.
        vmo.commit_page(0, MMUFlags::WRITE).unwrap();
        vmo.test_write(0, 1);

        let child = vmo.fork_copy().unwrap();
        // The committed page was copied with the parent's modification…
        assert_eq!(child.test_read(0), 1);
        // …and further parent writes don't leak into the child (independent frame).
        vmo.test_write(0, 7);
        assert_eq!(child.test_read(0), 1);
        // The untouched page still demand-fills from the shared source.
        let mut buf = [0u8; 1];
        child.read(PAGE_SIZE, &mut buf).unwrap();
        assert_eq!(buf[0], 0xbb);
    }

    #[test]
    #[ignore] // FIXME
    fn zero_page_write() {
        let vmo0 = VmObject::new_paged(1);
        let vmo1 = vmo0.create_child(false, 0, PAGE_SIZE).unwrap();
        let vmo2 = vmo0.create_child(false, 0, PAGE_SIZE).unwrap();
        let vmos = [vmo0, vmo1, vmo2];
        let origin = vmo_page_bytes();

        // no committed pages
        for vmo in &vmos {
            assert_eq!(vmo.get_info().committed_bytes, 0);
        }

        // copy-on-write
        for i in 0..3 {
            vmos[i].test_write(0, i as u8);
            for j in 0..3 {
                assert_eq!(vmos[j].test_read(0), if j <= i { j as u8 } else { 0 });
                assert_eq!(
                    vmos[j].get_info().committed_bytes as usize,
                    if j <= i { PAGE_SIZE } else { 0 }
                );
            }
            assert_eq!(vmo_page_bytes() - origin, (i + 1) * PAGE_SIZE);
        }
    }

    #[test]
    fn overflow() {
        let vmo0 = VmObject::new_paged(2);
        vmo0.test_write(0, 1);
        let vmo1 = vmo0.create_child(false, 0, 2 * PAGE_SIZE).unwrap();
        vmo1.test_write(1, 2);
        let vmo2 = vmo1.create_child(false, 0, 3 * PAGE_SIZE).unwrap();
        vmo2.test_write(2, 3);
        assert_eq!(vmo0.get_info().committed_bytes as usize, PAGE_SIZE);
        assert_eq!(vmo1.get_info().committed_bytes as usize, PAGE_SIZE);
        assert_eq!(vmo2.get_info().committed_bytes as usize, PAGE_SIZE);
    }

    #[test]
    fn many_clones() {
        const N: usize = 4;
        let old: u8 = 0xa;
        let new: u8 = 0xb;
        let permutations = [
            [0, 1, 2, 3],
            [0, 1, 3, 2],
            [0, 2, 1, 3],
            [0, 2, 3, 1],
            [0, 3, 1, 2],
            [0, 3, 2, 1],
            [1, 0, 2, 3],
            [1, 0, 3, 2],
            [1, 2, 0, 3],
            [1, 2, 3, 0],
            [1, 3, 0, 2],
            [1, 3, 2, 0],
            [2, 1, 0, 3],
            [2, 1, 3, 0],
            [2, 0, 1, 3],
            [2, 0, 3, 1],
            [2, 3, 1, 0],
            [2, 3, 0, 1],
            [3, 1, 2, 0],
            [3, 1, 0, 2],
            [3, 2, 1, 0],
            [3, 2, 0, 1],
            [3, 0, 1, 2],
            [3, 0, 2, 1],
        ];
        for i in 0..24 {
            let vmo0 = VmObject::new_paged(1);
            vmo0.write(0, &[old]).unwrap();
            let vmo1 = vmo0.create_child(false, 0, PAGE_SIZE).unwrap();
            let vmo2 = vmo0.create_child(false, 0, PAGE_SIZE).unwrap();
            let vmo3 = vmo1.create_child(false, 0, PAGE_SIZE).unwrap();
            let vmos: [Arc<VmObject>; 4] = [vmo0.clone(), vmo1.clone(), vmo2.clone(), vmo3.clone()];
            let mut write: [bool; 4] = [false; 4];
            let perm = permutations[i];
            println!("{:?}", perm);
            for j in 0..N {
                println!("j = {}, write = {}", j, perm[j]);
                vmos[perm[j]].write(0, &[new]).unwrap();
                write[perm[j]] = true;
                let mut buf: [u8; 1] = [0];
                for k in 0..N {
                    vmos[k].read(0, &mut buf).unwrap();
                    println!("vmo[{}] = {:x}", k, buf[0]);
                    if write[k] {
                        assert!(buf[0] == new);
                    } else {
                        assert!(buf[0] == old);
                    }
                    buf[0] = 0;
                }
            }
        }
    }

    impl VmObject {
        pub fn test_write(&self, page: usize, value: u8) {
            self.write(page * PAGE_SIZE, &[value]).unwrap();
        }

        pub fn test_read(&self, page: usize) -> u8 {
            let mut buf = [0; 1];
            self.read(page * PAGE_SIZE, &mut buf).unwrap();
            buf[0]
        }
    }

    /// `read` and `write` had no bound at all: a read four pages into a
    /// one-page object answered `Ok(())` after resolving page 4 through the
    /// demand-zero path, and the matching write would have allocated a frame
    /// at an index `len()` says does not exist, where nothing ever frees it.
    #[test]
    fn a_range_outside_the_object_is_refused_by_the_object() {
        let vmo = VmObject::new_paged(1);
        let mut buf = [0u8; 8];
        assert_eq!(vmo.len(), PAGE_SIZE);
        assert_eq!(vmo.read(PAGE_SIZE - 8, &mut buf), Ok(()));

        assert_eq!(
            vmo.read(4 * PAGE_SIZE, &mut buf),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(
            vmo.read(PAGE_SIZE - 7, &mut buf),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(vmo.write(PAGE_SIZE, &[1u8; 1]), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(
            vmo.committed_pages_in_range(0, 1),
            0,
            "a refused read must not have committed anything"
        );
    }

    /// The three range operations share one shape of bound, and all three of
    /// them have to survive a sum that wraps -- `zx_vmo_op_range(ZX_VMO_OP_
    /// ZERO)` passes `offset` and `len` through from userspace untouched.
    #[test]
    fn a_range_whose_end_wraps_is_refused_too() {
        let vmo = VmObject::new_paged(2);
        let mut buf = [0u8; 8];
        assert_eq!(
            vmo.read(usize::MAX - 3, &mut buf),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(
            vmo.write(usize::MAX - 3, &[0u8; 8]),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(vmo.zero(usize::MAX - 3, 8), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(
            vmo.zero(usize::MAX - PAGE_SIZE + 1, 2 * PAGE_SIZE),
            Err(ZxError::OUT_OF_RANGE)
        );
        // The whole object is still in range.
        assert_eq!(vmo.zero(0, 2 * PAGE_SIZE), Ok(()));
    }

    /// `align_log2` is an absolute log2, so 0 is not "page alignment": the
    /// frame allocator is handed `align_log2 - PAGE_SIZE_LOG2`, and anything
    /// below the page shift made that underflow. `sys_vmo_create_contiguous`
    /// maps 0 to `PAGE_SIZE_LOG2` and rejects the rest, but it is the only
    /// caller that knows, and it is two crates away.
    #[test]
    fn a_contiguous_vmo_takes_an_absolute_log2_alignment() {
        assert!(VmObject::new_contiguous(1, PAGE_SIZE_LOG2).is_ok());
        assert!(VmObject::new_contiguous(1, PAGE_SIZE_LOG2 + 1).is_ok());
        assert_eq!(
            VmObject::new_contiguous(1, 0).err(),
            Some(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            VmObject::new_contiguous(1, PAGE_SIZE_LOG2 - 1).err(),
            Some(ZxError::INVALID_ARGS)
        );
    }

    /// Counting pages is a question, not a command: asking about pages the
    /// object does not have answers zero. It used to assert, and the caller
    /// that runs on every `/proc/<pid>/status` read carries a hand-written
    /// clamp and an early return purely to stay out of that assertion.
    #[test]
    fn counting_pages_past_the_end_answers_zero_instead_of_panicking() {
        let vmo = VmObject::new_paged(2);
        vmo.commit_page(0, MMUFlags::WRITE).unwrap();
        assert_eq!(vmo.committed_pages_in_range(0, 2), 1);
        assert_eq!(vmo.committed_pages_in_range(0, 64), 1);
        assert_eq!(vmo.committed_pages_in_range(2, 64), 0);
        assert_eq!(vmo.committed_pages_in_range(64, 65), 0);
        assert_eq!(vmo.committed_pages_in_range(64, 8), 0);
    }
}

#[cfg(test)]
mod pin_tests {
    //! Pinning is the promise that a page stays where it is because a device
    //! is about to do DMA into it. Three operations take pages away from a
    //! VMO; two of them asked whether anything was pinned first and the third
    //! did not, so the frame went back to the allocator with the IOMMU still
    //! pointing at it.
    use super::*;

    /// A four-page object with every page committed.
    fn committed() -> Arc<VmObject> {
        let vmo = VmObject::new_paged(4);
        vmo.commit(0, 4 * PAGE_SIZE).unwrap();
        assert_eq!(held(&vmo), [true; 4]);
        vmo
    }

    /// Holds a pin for as long as it is alive.
    ///
    /// `Drop for VMObjectPaged` asserts that no frame is left pinned, so a
    /// test that fails an assertion with a pin still held panics a second
    /// time on its way out and aborts the whole binary: the suite goes red
    /// with no test name anywhere in it, which is most of what a CI failure
    /// is for. Nothing reachable from userspace can trip that assert -- the
    /// token holds the `Arc` -- but a test can, and letting go from a `Drop`
    /// runs on the unwinding path too.
    struct Pinned<'a> {
        vmo: &'a Arc<VmObject>,
        offset: usize,
        len: usize,
        held: bool,
    }

    impl Pinned<'_> {
        /// Let go of the pin, and say whether the kernel agreed.
        fn release(mut self) -> ZxResult {
            self.held = false;
            self.vmo.unpin(self.offset, self.len)
        }
    }

    impl Drop for Pinned<'_> {
        fn drop(&mut self) {
            if self.held {
                let _ = self.vmo.unpin(self.offset, self.len);
            }
        }
    }

    /// Pins a range and hands back the handle that lets go of it.
    fn pin(vmo: &Arc<VmObject>, offset: usize, len: usize) -> Pinned<'_> {
        vmo.pin(offset, len).unwrap();
        Pinned {
            vmo,
            offset,
            len,
            held: true,
        }
    }

    /// Which of the four pages the object is still holding a frame for.
    fn held(vmo: &Arc<VmObject>) -> [bool; 4] {
        let mut out = [false; 4];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = vmo.committed_pages_in_range(i, i + 1) == 1;
        }
        out
    }

    #[test]
    /// A pinned page is not handed back to the allocator. `zx_vmo_op_range`
    /// passes its offset and length straight through from userspace, so this
    /// used to hand a frame a device was about to write into back for someone
    /// else to use, and answer `Ok`.
    fn a_pinned_page_is_not_handed_back_to_the_allocator() {
        let vmo = committed();
        // Two pages, so a pin that only marked the first one shows up here.
        let pinned = pin(&vmo, 0, 2 * PAGE_SIZE);
        assert_eq!(vmo.decommit(0, PAGE_SIZE), Err(ZxError::BAD_STATE));
        assert_eq!(vmo.decommit(PAGE_SIZE, PAGE_SIZE), Err(ZxError::BAD_STATE));
        assert_eq!(held(&vmo), [true; 4], "a page went under the pin");
        // And with the pin released they go as usual.
        pinned.release().unwrap();
        vmo.decommit(0, 2 * PAGE_SIZE).unwrap();
        assert_eq!(held(&vmo), [false, false, true, true]);
    }

    #[test]
    /// The refusal is per page, not per object: the rest of a mostly-pinned
    /// VMO can still be decommitted.
    fn decommitting_around_a_pinned_page_still_works() {
        let vmo = committed();
        let pinned = pin(&vmo, PAGE_SIZE, PAGE_SIZE);
        vmo.decommit(0, PAGE_SIZE).unwrap();
        vmo.decommit(2 * PAGE_SIZE, 2 * PAGE_SIZE).unwrap();
        assert_eq!(vmo.decommit(PAGE_SIZE, PAGE_SIZE), Err(ZxError::BAD_STATE));
        assert_eq!(held(&vmo), [false, true, false, false]);
        pinned.release().unwrap();
    }

    #[test]
    /// And it is all or nothing: a range with one pinned page in it leaves
    /// every page alone, including the ones before the pinned one.
    fn a_refused_decommit_leaves_every_page_alone() {
        let vmo = committed();
        let pinned = pin(&vmo, 3 * PAGE_SIZE, PAGE_SIZE);
        assert_eq!(vmo.decommit(0, 4 * PAGE_SIZE), Err(ZxError::BAD_STATE));
        assert_eq!(held(&vmo), [true; 4]);
        pinned.release().unwrap();
    }

    #[test]
    /// Taking pages away is three operations, and all three answer the same
    /// thing while something is pinned. Two of them always did.
    fn the_three_ways_of_taking_pages_away_all_refuse_a_pinned_object() {
        let vmo = VmObject::new_paged_with_resizable(true, 4);
        vmo.commit(0, 4 * PAGE_SIZE).unwrap();
        let pinned = pin(&vmo, 0, PAGE_SIZE);
        assert_eq!(vmo.set_len(PAGE_SIZE), Err(ZxError::BAD_STATE));
        assert_eq!(
            vmo.set_cache_policy(CachePolicy::Uncached),
            Err(ZxError::BAD_STATE),
        );
        assert_eq!(vmo.decommit(0, PAGE_SIZE), Err(ZxError::BAD_STATE));

        pinned.release().unwrap();
        vmo.decommit(0, PAGE_SIZE).unwrap();
        vmo.set_len(PAGE_SIZE).unwrap();
    }

    #[test]
    /// Pinning a page the object does not have is an error, not a panic.
    /// `zx_bti_pin` commits the range and then pins it, and those are two
    /// turns of the object's lock: a decommit in between is allowed, because
    /// the pages are not pinned yet, and this is where it lands.
    fn pinning_a_page_that_is_not_there_is_an_error_not_a_panic() {
        let vmo = VmObject::new_paged(4);
        assert_eq!(vmo.pin(0, PAGE_SIZE), Err(ZxError::BAD_STATE));

        let vmo = committed();
        vmo.decommit(2 * PAGE_SIZE, PAGE_SIZE).unwrap();
        assert_eq!(vmo.pin(0, 4 * PAGE_SIZE), Err(ZxError::BAD_STATE));
        // Nothing was half-pinned on the way to that answer: the pages before
        // the missing one are still free to go.
        vmo.decommit(0, PAGE_SIZE).unwrap();
        assert_eq!(held(&vmo), [false, true, false, true]);
    }

    #[test]
    /// Unpinning one is the same answer rather than the same panic.
    fn unpinning_a_page_that_is_not_there_is_an_error_not_a_panic() {
        let vmo = VmObject::new_paged(4);
        assert_eq!(vmo.unpin(0, PAGE_SIZE), Err(ZxError::BAD_STATE));

        // A page that is there but was never pinned is a different answer.
        let vmo = committed();
        assert_eq!(vmo.unpin(0, PAGE_SIZE), Err(ZxError::UNAVAILABLE));
    }

    #[test]
    /// A VMO that goes away with a page still pinned is a hole worth saying
    /// out loud, and saying it used to be an `assert_eq!` inside a
    /// destructor. That costs the machine to report a leak, and in a test
    /// binary it costs every test after this one: a panic raised while
    /// something else is already unwinding does not unwind, it aborts, so
    /// the run ends with no test name in it at all. It is also exactly
    /// where a refused `unpin` in `PinnedMemoryToken`'s own `Drop` lands one
    /// frame later, so the two have to agree.
    fn a_vmo_that_goes_away_still_pinned_is_reported_not_a_panic() {
        let vmo = committed();
        vmo.pin(0, PAGE_SIZE).unwrap();
        drop(vmo);
    }

    #[test]
    /// A zero-length pin or unpin is nothing at all, whatever it names.
    fn nothing_is_pinned_by_a_zero_length_range() {
        let vmo = committed();
        vmo.pin(0, 0).unwrap();
        vmo.unpin(0, 0).unwrap();
        vmo.decommit(0, PAGE_SIZE).unwrap();
        assert_eq!(held(&vmo), [false, true, true, true]);
    }
}

#[cfg(test)]
mod range_bound_tests {
    //! One question -- is this page inside the object? -- asked by six
    //! operations that all take an offset and a length straight from
    //! userspace, and answered by four of them.
    //!
    //! `zx_vmo_op_range` checks that both numbers are page-aligned and hands
    //! them through; `VMObjectSlice` wrote the bound down for its own
    //! `commit`/`decommit` ("hands `offset` and `len` through unbounded"), and
    //! `zero` carries it too. The object underneath did not, so a commit past
    //! the end demand-allocated real frames at page indices the object does
    //! not have -- and `committed_pages_in_range` clamps to `size`, so no
    //! counter that reports what a VMO is holding could see them afterwards.
    use super::*;

    /// A one-page object, so page 1 is the first one outside it.
    fn one_page() -> Arc<VmObject> {
        VmObject::new_paged(1)
    }

    #[test]
    /// All six give the same answer for the same range. Two of them did not.
    fn every_range_operation_answers_the_same_bound() {
        let vmo = one_page();
        let mut buf = [0u8; 1];
        assert_eq!(vmo.read(PAGE_SIZE, &mut buf), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(vmo.write(PAGE_SIZE, &[9]), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(vmo.zero(PAGE_SIZE, PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(vmo.pin(PAGE_SIZE, PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(vmo.commit(PAGE_SIZE, PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(
            vmo.decommit(PAGE_SIZE, PAGE_SIZE),
            Err(ZxError::OUT_OF_RANGE)
        );
    }

    #[test]
    /// And the frames are the point: `zx_vmo_op_range(ZX_VMO_OP_COMMIT)` over
    /// sixty-four pages of a one-page object really allocated sixty-four
    /// frames, while `zx_object_get_info` went on reporting one page
    /// committed. Every accounting and memory-pressure figure in the kernel
    /// reads those clamped counters, so an unprivileged process could hold
    /// physical memory that nothing could account for.
    fn a_commit_past_the_end_allocates_nothing() {
        let vmo = one_page();
        let before = vmo_page_bytes();
        assert_eq!(
            vmo.commit(0, 64 * PAGE_SIZE),
            Err(ZxError::OUT_OF_RANGE),
            "a one-page object accepted a sixty-four-page commit"
        );
        assert_eq!(
            vmo_page_bytes(),
            before,
            "the refused commit still allocated frames"
        );
        assert_eq!(vmo.get_info().committed_bytes, 0);
    }

    #[test]
    /// All or nothing, like the pinned-page refusal next door: a range with
    /// one page outside the object leaves the pages inside it alone, rather
    /// than committing as far as it can and then failing.
    fn a_partly_out_of_range_commit_leaves_the_object_alone() {
        let vmo = one_page();
        assert_eq!(
            vmo.commit(0, 2 * PAGE_SIZE),
            Err(ZxError::OUT_OF_RANGE),
            "a two-page commit on a one-page object"
        );
        assert_eq!(
            vmo.committed_pages_in_range(0, 1),
            0,
            "the page inside the object was committed by a refused call"
        );
    }

    #[test]
    /// A sum that wraps is out of range, not a short range that happens to
    /// fit. `zx_vmo_op_range` page-aligns both numbers and nothing else, so
    /// the pair below arrives exactly as written.
    fn a_range_that_wraps_is_out_of_range() {
        let vmo = one_page();
        let high = usize::MAX - PAGE_SIZE + 1;
        assert_eq!(vmo.commit(high, PAGE_SIZE * 2), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(
            vmo.decommit(high, PAGE_SIZE * 2),
            Err(ZxError::OUT_OF_RANGE)
        );
    }

    #[test]
    /// The bound is on the end of the range, so an empty range that starts
    /// exactly at the end of the object is inside it -- the same answer a
    /// zero-length pin already gave.
    fn an_empty_range_at_the_very_end_is_allowed() {
        let vmo = one_page();
        vmo.commit(PAGE_SIZE, 0).unwrap();
        vmo.decommit(PAGE_SIZE, 0).unwrap();
        vmo.pin(PAGE_SIZE, 0).unwrap();
        vmo.zero(PAGE_SIZE, 0).unwrap();
        // One past it is not.
        assert_eq!(
            vmo.commit(PAGE_SIZE + PAGE_SIZE, 0),
            Err(ZxError::OUT_OF_RANGE)
        );
    }

    #[test]
    /// The whole object still commits, which is what the bound must not cost.
    fn committing_the_whole_object_still_works() {
        let vmo = VmObject::new_paged(4);
        vmo.commit(0, 4 * PAGE_SIZE).unwrap();
        assert_eq!(vmo.committed_pages_in_range(0, 4), 4);
        vmo.decommit(0, 4 * PAGE_SIZE).unwrap();
        assert_eq!(vmo.committed_pages_in_range(0, 4), 0);
    }
}
