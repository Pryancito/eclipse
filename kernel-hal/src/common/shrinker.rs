//! Reclaim: the caches that can give memory back, and who asks them.
//!
//! Eclipse had none of this. Every cache in the kernel -- the ELF image cache,
//! the dentry cache, the block cache -- had its own fixed cap and nothing that
//! could make it give anything back, so the only answer to memory pressure was
//! to raise a cap: the kernel heap went from 256 MiB to 512 MiB because a
//! desktop session OOMed while its clients were still loading, which is a
//! cache that would not shrink, not a heap that was too small.
//!
//! This is Linux's shrinker interface, with the two calls that matter:
//! `count_objects` (what could be given back) and `scan_objects` (give some
//! back). Linux's `shrink_slab` walks the registered shrinkers and asks each
//! for a share of the target proportional to what it holds; so does [`shrink`].
//!
//! Three things are deliberately NOT like Linux, and each is a decision rather
//! than an omission:
//!
//! 1. **`kswapd` is one worker, and it is the only thing that may be slow.**
//!    Direct reclaim ([`shrink`]) runs on the thread whose allocation just
//!    failed, and can therefore only ask caches that free memory without
//!    allocating, waiting or writing to disk. The caches that cannot promise
//!    that -- the page cache, whose eviction writes dirty pages back through
//!    the filesystem -- register in the DEFERRED registry instead, which only
//!    the reclaim worker walks ([`run_deferred`]). There is no per-node
//!    `kswapd` and no watermark policy in here: the worker lives where the
//!    free-memory figures live, and this module only holds the standing
//!    demand it serves ([`request`]).
//! 2. **No shrinker list in a `Vec`.** The registry is a fixed array of
//!    `&'static` records, because its one caller is an allocation that has
//!    already failed: a `Vec` push on that path is the allocation that cannot
//!    be made.
//! 3. **No NUMA nodes or memcg.** There is one zone and one cgroup here.

use crate::common::cpu_topology::{percpu_slot, PERCPU_SLOTS};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

/// A cache that can give memory back.
///
/// Both calls are plain `fn` pointers, not closures: the record is `'static`
/// and lives in the registering crate's `static`, so there is nothing to own
/// and nothing to allocate when it is registered.
pub struct Shrinker {
    /// For the reclaim log line and for `stats`. A cache nobody can name is a
    /// cache nobody will fix.
    pub name: &'static str,
    /// Bytes this cache could give back right now. Used to decide its share
    /// of a target, so an honest zero means "do not bother asking me".
    pub count: fn() -> usize,
    /// Give back up to `want` bytes and answer how many were actually freed.
    ///
    /// Freeing more than asked is allowed and expected -- a cache whose unit
    /// is a whole mapped image cannot free half of one -- and the caller
    /// counts what it is told.
    pub scan: fn(want: usize) -> usize,
}

/// Shrinkers the registry holds.
///
/// Eight, for the caches that exist plus room: the ELF image cache, the page
/// cache, the dentry cache, the block cache. A ninth registration is refused
/// and says so rather than growing an array on the allocation-failure path.
pub const MAX_SHRINKERS: usize = 8;

/// The least a shrinker is asked for once it is asked at all.
///
/// Without a floor, a cache holding a small share of the total gets a target
/// of a few bytes, frees a whole object anyway, and the proportional split
/// stops meaning anything. One page is the unit every cache here works in.
const MIN_SHARE: usize = 4096;

/// The shrinkers, and what asking them has cost.
///
/// A value rather than a module of statics, for the reason `cpu_topology`'s
/// registry is one: registration is irreversible, so a test that registered
/// into a global would change what every later test sees. The kernel's own
/// instance is [`REGISTRY`]; a test builds its own.
pub struct Registry {
    len: AtomicUsize,
    slots: [AtomicPtr<Shrinker>; MAX_SHRINKERS],
    /// Per-CPU "already reclaiming" flag. A shrinker frees through the same
    /// allocator that called it, so its own work can allocate and fail and
    /// land back here; without this the first real shortage is an infinite
    /// recursion on the failure path instead of an OOM report.
    busy: [AtomicBool; PERCPU_SLOTS],
    calls: AtomicU64,
    freed: AtomicU64,
    reentrant: AtomicU64,
}

impl Registry {
    pub const fn new() -> Self {
        Registry {
            len: AtomicUsize::new(0),
            slots: [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_SHRINKERS],
            busy: [const { AtomicBool::new(false) }; PERCPU_SLOTS],
            calls: AtomicU64::new(0),
            freed: AtomicU64::new(0),
            reentrant: AtomicU64::new(0),
        }
    }

    /// Add `s`, or `false` when the registry is full.
    ///
    /// The slot is claimed before it is filled and published with a `Release`
    /// store, so a reader that sees a non-null pointer sees a complete record.
    /// `len` is raised last for the same reason: a reader walks `0..len` and a
    /// slot still null inside that range is simply skipped.
    pub fn register(&self, s: &'static Shrinker) -> bool {
        let i = self.len.load(Ordering::Acquire);
        if i >= MAX_SHRINKERS {
            return false;
        }
        self.slots[i].store(s as *const Shrinker as *mut Shrinker, Ordering::Release);
        self.len.store(i + 1, Ordering::Release);
        true
    }

    /// How many are registered.
    pub fn registered(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    fn get(&self, i: usize) -> Option<&'static Shrinker> {
        let p = self.slots[i].load(Ordering::Acquire);
        // SAFETY: the only writer is `register`, which stores the address of a
        // `&'static Shrinker`, so a non-null pointer here is a live record
        // that outlives every borrow of it.
        (!p.is_null()).then(|| unsafe { &*(p as *const Shrinker) })
    }

    /// Bytes every registered cache says it could give back.
    pub fn reclaimable(&self) -> usize {
        let mut total = 0usize;
        for i in 0..self.registered() {
            if let Some(s) = self.get(i) {
                total = total.saturating_add((s.count)());
            }
        }
        total
    }

    /// Ask the caches for `want` bytes and answer what they gave.
    ///
    /// `slot` is the per-CPU slot the re-entrancy flag belongs to; the public
    /// [`shrink`] passes this CPU's. Taken as an argument so a test can drive
    /// the flag without being the CPU.
    pub fn shrink_on(&self, slot: usize, want: usize) -> usize {
        if want == 0 || slot >= PERCPU_SLOTS {
            return 0;
        }
        // Not a lock: a failed exchange means THIS CPU is already inside a
        // shrinker, and the answer is to give up rather than wait, because the
        // thing we would be waiting for is ourselves.
        if self.busy[slot]
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.reentrant.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        let freed = self.scan_all(want);
        self.busy[slot].store(false, Ordering::Release);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.freed.fetch_add(freed as u64, Ordering::Relaxed);
        freed
    }

    /// The walk itself, with the flag already held.
    fn scan_all(&self, want: usize) -> usize {
        let n = self.registered();
        // Proportional, like `shrink_slab`: a cache holding most of the
        // reclaimable memory takes most of the target. Asking each for the
        // whole target instead would empty whichever happened to register
        // first and leave the rest untouched.
        let total = self.reclaimable();
        if total == 0 {
            return 0;
        }
        let mut freed = 0usize;
        for i in 0..n {
            let Some(s) = self.get(i) else { continue };
            let held = (s.count)();
            if held == 0 {
                continue;
            }
            let remaining = want.saturating_sub(freed);
            if remaining == 0 {
                break;
            }
            // This cache's share of the target. Multiply first and divide
            // after, so a cache holding a small fraction is not rounded down
            // to nothing; `saturating_mul` because the honest product is what
            // the division wants and a saturated one still divides to
            // something sane. Both factors are bounded by the heap, so on a
            // 64-bit target the product does not come near saturating.
            let share = (want.saturating_mul(held) / total)
                .max(MIN_SHARE)
                .min(remaining);
            freed = freed.saturating_add((s.scan)(share));
        }
        // Second pass: rounding and caches that gave less than their share can
        // leave the target short while another cache still holds plenty. Ask
        // whoever is left for the rest, one at a time.
        for i in 0..n {
            let remaining = want.saturating_sub(freed);
            if remaining == 0 {
                break;
            }
            let Some(s) = self.get(i) else { continue };
            if (s.count)() == 0 {
                continue;
            }
            freed = freed.saturating_add((s.scan)(remaining));
        }
        freed
    }

    /// Reclaim attempts, bytes freed, and attempts refused because this CPU
    /// was already inside a shrinker.
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.calls.load(Ordering::Relaxed),
            self.freed.load(Ordering::Relaxed),
            self.reentrant.load(Ordering::Relaxed),
        )
    }
}

impl Default for Registry {
    /// Only so `new` can stay the name the `static` below uses: a registry has
    /// no meaningful default beyond "empty", and `new` is `const` while this
    /// cannot be, so nothing here calls it.
    fn default() -> Self {
        Self::new()
    }
}

static REGISTRY: Registry = Registry::new();

/// Register a cache that can give memory back. `false` means the registry is
/// full, which is a build-time mistake: raise [`MAX_SHRINKERS`].
pub fn register(s: &'static Shrinker) -> bool {
    REGISTRY.register(s)
}

/// How many caches are registered.
pub fn registered() -> usize {
    REGISTRY.registered()
}

/// Bytes the registered caches say they could give back.
pub fn reclaimable() -> usize {
    REGISTRY.reclaimable()
}

/// Ask the caches for `want` bytes. Answers what they gave, which may be more
/// than asked or nothing at all.
///
/// Safe to call from an allocation that has just failed, which is its only
/// caller: it allocates nothing itself, and a shrinker whose own work lands
/// back here gets a zero rather than a recursion.
pub fn shrink(want: usize) -> usize {
    let freed = REGISTRY.shrink_on(percpu_slot(crate::cpu::cpu_id() as usize), want);
    // What the caches that may answer here could not give is what the worker
    // is for. Raising the demand costs a store, which is all this path can
    // afford, and the allocation in front of it still fails or does not on
    // what `freed` says -- the worker is for the NEXT one.
    request(want.saturating_sub(freed));
    freed
}

/// Reclaim attempts, bytes freed, and attempts refused as re-entrant.
pub fn stats() -> (u64, u64, u64) {
    REGISTRY.stats()
}

/// The caches whose reclaim cannot run in front of a failed allocation.
///
/// Same records, a separate registry, because the contract is the opposite
/// one: a shrinker in here MAY allocate, take filesystem locks and write to
/// disk. Evicting a page-cache entry writes its dirty pages back through
/// `inode.write_at` -- a filesystem write, a `vec![0u8; PAGE_SIZE]`, and the
/// page faults reading the VMO triggers -- which is exactly what an allocation
/// that has already failed cannot do. Registering such a cache in [`REGISTRY`]
/// would trade an OOM for a hang, so it goes here and only [`run_deferred`],
/// on the reclaim worker, ever calls it.
///
/// Its own re-entrancy flags are what make that safe in the other direction
/// too: a deferred scan whose own work fails an allocation lands in [`shrink`],
/// which walks the OTHER registry and finds its flags free.
static DEFERRED: Registry = Registry::new();

/// Bytes the deferred caches have been asked for and have not yet been asked
/// to give. Written by [`request`] and by the shortfall of a direct reclaim,
/// read and cleared by [`run_deferred`].
static DEMAND: AtomicUsize = AtomicUsize::new(0);

/// Register a cache whose reclaim may allocate, wait or write to disk.
/// `false` means the deferred registry is full: raise [`MAX_SHRINKERS`].
pub fn register_deferred(s: &'static Shrinker) -> bool {
    DEFERRED.register(s)
}

/// How many deferred caches are registered.
pub fn registered_deferred() -> usize {
    DEFERRED.registered()
}

/// Bytes the deferred caches say they could give back.
///
/// Runs their `count`, which in the deferred registry may be as slow as their
/// `scan`: for the reclaim worker only.
pub fn deferred_reclaimable() -> usize {
    DEFERRED.reclaimable()
}

/// Ask the worker for `want` bytes, without doing any of the work.
///
/// This is the whole of waking `kswapd`: it stores a figure and returns, so it
/// is safe from a failed allocation and from a frame allocator holding its own
/// lock. The standing demand is the LARGEST request since the last pass, not
/// their sum -- ten failures asking 4 MiB each mean 4 MiB are missing ten
/// times over, not that 40 MiB are.
pub fn request(want: usize) {
    if want > 0 {
        DEMAND.fetch_max(want, Ordering::Relaxed);
    }
}

/// What the deferred caches are currently being asked for.
pub fn pending() -> usize {
    DEMAND.load(Ordering::Relaxed)
}

/// Serve the standing demand: the reclaim worker's one call.
///
/// Takes the demand before scanning, so a request that arrives while this runs
/// is served by the next pass instead of being lost. Answers what was freed.
pub fn run_deferred() -> usize {
    let want = DEMAND.swap(0, Ordering::Relaxed);
    if want == 0 {
        return 0;
    }
    DEFERRED.shrink_on(percpu_slot(crate::cpu::cpu_id() as usize), want)
}

/// Deferred reclaim passes, bytes freed, and passes refused as re-entrant.
pub fn deferred_stats() -> (u64, u64, u64) {
    DEFERRED.stats()
}

#[cfg(test)]
#[path = "shrinker_tests.rs"]
mod shrinker_tests;
