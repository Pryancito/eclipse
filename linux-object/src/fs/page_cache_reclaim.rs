//! The page cache, as a cache that can give memory back.
//!
//! `SHARED_FILE_VMOS` is the largest discretionary allocation in the kernel:
//! one VMO per inode, holding every page of the file anything has ever read
//! or mapped, with a STRONG reference. Until now its only eviction rule was
//! "the backing inode is gone", so a file that stays open keeps its pages for
//! as long as the machine runs, and the pages belong to no process -- the
//! `frame_alloc FAILED: 2561 MiB used / 2561 MiB managed` of the XFCE session
//! is this registry.
//!
//! It is registered in the DEFERRED registry, not with the shrinkers direct
//! reclaim walks, and that is the whole reason this module exists separately
//! from `elf_vmo_shrinker`. Evicting an entry runs `finish_eviction`:
//! `inode.write_at` for every dirty page, a `vec![0u8; PAGE_SIZE]`, and the
//! page faults that reading the VMO triggers back into `inode.read_at`. An
//! allocation that has already failed can do none of that, and a filesystem
//! write takes inode locks that `read(2)` already holds coming the other way
//! through `cache_overlay_read` -- a genuine AB-BA. Called from the reclaim
//! worker, on its own thread, with no allocator lock held, all of it is
//! ordinary work.
//!
//! What may be evicted, and why each half matters:
//!
//! * **Nothing maps the VMO** (`Arc::strong_count == 1`, the registry's own).
//!   A mapper holds page table entries pointing at those frames; handing them
//!   back under it is a use-after-free with the MMU as the accomplice.
//! * **Either the entry was never handed out as `MAP_SHARED`, or its file is
//!   still linked.** A `MAP_SHARED` entry whose inode has `nlinks == 0` is a
//!   memfd or an unlinked shm pool: there is no writeback path for it because
//!   the VMO *is* its storage, and evicting it does not free memory, it loses
//!   data. The wl_keyboard keymap lives exactly there.
//!
//! An entry that is still open but unmapped is fair game, which is the point:
//! a clean page is re-read from the inode on its next fault, and a dirty one
//! is written back first. That is what makes this a cache rather than a sink.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use rcore_fs::vfs::INode;
use zircon_object::vm::VmObject;

use super::{finish_eviction, EvictedVmos, SharedVmoMap, PAGE_SIZE, SHARED_FILE_VMOS};

/// A registry entry this pass may be able to evict: its key, the VMO (cloned,
/// so the entry cannot go away underneath), the weak inode handle and whether
/// a `MAP_SHARED` mapping was ever handed out of it.
type Candidate = ((usize, usize), Arc<VmObject>, Weak<dyn INode>, bool);

/// Every entry nothing maps any more, cloned out of the registry.
///
/// Two short lock sections rather than one, so that the only allocation this
/// makes -- the vector -- happens with the registry lock DOWN. That lock is a
/// ticket spinlock held with interrupts off, and a `Vec` growing inside it is
/// the holder parked in the heap allocator while every other CPU spins on
/// `cache_vmo_of`: the KERNEL STOP of the eviction-in-place version.
fn candidates() -> Vec<Candidate> {
    let room = match SHARED_FILE_VMOS.try_lock() {
        Some(registry) => registry.len(),
        None => return Vec::new(),
    };
    let mut out = Vec::with_capacity(room);
    let Some(registry) = SHARED_FILE_VMOS.try_lock() else {
        return out;
    };
    // `take(room)`, not the whole walk: the registry may have grown between
    // the two sections, and a push past the capacity reserved above is a
    // `Vec` growing inside the critical section -- the allocation this split
    // exists to avoid. Entries past it are the next pass's business.
    for (key, (vmo, inode_weak, ever_shared)) in registry.iter().take(room) {
        if Arc::strong_count(vmo) != 1 {
            continue;
        }
        out.push((*key, vmo.clone(), inode_weak.clone(), *ever_shared));
    }
    out
}

/// Bytes of `vmo` that are actually resident, which is what evicting it gives
/// back. A cache VMO is as long as its file; only faulted pages hold frames.
fn committed_bytes(vmo: &Arc<VmObject>) -> usize {
    vmo.committed_pages_in_range(0, vmo.len() / PAGE_SIZE) * PAGE_SIZE
}

/// Whether this entry may be dropped at all -- the `MAP_SHARED`-without-a-file
/// half of the rule. Asks the filesystem, so never under the registry lock.
fn may_evict(inode_weak: &Weak<dyn INode>, ever_shared: bool) -> bool {
    if !ever_shared {
        // Never handed out shared: every page is a clean copy of the file's,
        // because `cache_overlay_write` only mirrors writes that already went
        // to the inode. Nothing to write back, nothing to lose.
        return true;
    }
    match inode_weak.upgrade() {
        // A live, still-linked file: `finish_eviction` writes the dirty pages
        // back to it before the frames go.
        Some(inode) => inode.metadata().map(|m| m.nlinks > 0).unwrap_or(false),
        // Nowhere to write back to, so nothing here may be thrown away. This
        // arm is all but unreachable and no test covers it: the registry's own
        // entry holds the inode through the VMO's `FileFrameFiller`, which is
        // the cycle `take_prunable` is written around, so a weak handle to a
        // registered entry's inode upgrades for as long as the entry exists.
        // It is here because `Weak::upgrade` is allowed to fail and a reclaim
        // that guessed would be guessing about a filesystem write.
        None => false,
    }
}

/// Bytes the page cache could give back right now.
///
/// Counts only entries that pass the eviction rule, and only their resident
/// pages. Promising reclaim memory that cannot be freed is worse than
/// promising none: the retry built on it fails a second time, and the
/// proportional split hands this cache a share of the target that the caches
/// which COULD have given something never get asked for.
pub fn count() -> usize {
    let mut total = 0usize;
    for (_, vmo, inode_weak, ever_shared) in candidates() {
        if may_evict(&inode_weak, ever_shared) {
            total = total.saturating_add(committed_bytes(&vmo));
        }
    }
    total
}

/// Evict unmapped page-cache entries until `want` bytes have been given back.
pub fn scan(want: usize) -> usize {
    // Plan outside the lock: this is where the filesystem is asked and where
    // the resident pages are counted.
    let mut plan: Vec<((usize, usize), Arc<VmObject>, usize)> = Vec::new();
    let mut planned = 0usize;
    for (key, vmo, inode_weak, ever_shared) in candidates() {
        if planned >= want {
            break;
        }
        let bytes = committed_bytes(&vmo);
        if bytes == 0 || !may_evict(&inode_weak, ever_shared) {
            continue;
        }
        planned += bytes;
        plan.push((key, vmo, bytes));
    }
    if plan.is_empty() {
        return 0;
    }

    // Take them out of the registry, under the lock and nothing more. The
    // refcount is re-checked here because the plan above ran unlocked: a
    // mapping created in between raises it, and that entry stays. Two, not
    // one -- the registry's reference and the clone in `plan`.
    let evicted: EvictedVmos = {
        let Some(mut registry) = SHARED_FILE_VMOS.try_lock() else {
            return 0;
        };
        let mut taken = EvictedVmos::with_capacity(plan.len());
        for (key, vmo, _) in plan.iter() {
            if !still_unmapped(&registry, key, vmo) {
                continue;
            }
            if let Some(entry) = registry.remove(key) {
                taken.push(entry);
            }
        }
        taken
    };

    let freed = freed_bytes(&plan, &evicted);
    // Our own clones go first, so that `finish_eviction` holds the last
    // reference and dropping it is what frees the frames.
    drop(plan);
    finish_eviction(evicted);
    freed
}

/// Bytes of `plan` that actually came out of the registry.
///
/// What was freed is what `evicted` holds, never what was planned: the
/// re-check under the lock leaves behind any entry that got mapped while the
/// plan was being made, and counting those would report memory nobody can
/// use -- the retry built on that figure fails a second time.
fn freed_bytes(plan: &[((usize, usize), Arc<VmObject>, usize)], evicted: &EvictedVmos) -> usize {
    plan.iter()
        .filter(|(_, vmo, _)| evicted.iter().any(|(e, _, _)| Arc::ptr_eq(e, vmo)))
        .map(|(_, _, bytes)| *bytes)
        .sum()
}

/// Is `key` still the entry we planned to evict, and does still nobody map it?
fn still_unmapped(registry: &SharedVmoMap, key: &(usize, usize), vmo: &Arc<VmObject>) -> bool {
    match registry.get(key) {
        Some((cached, _, _)) => Arc::ptr_eq(cached, vmo) && Arc::strong_count(cached) == 2,
        None => false,
    }
}

/// The record the reclaim worker walks. DEFERRED, never direct: see the module
/// documentation for what evicting an entry runs.
pub static RECLAIMER: kernel_hal::shrinker::Shrinker = kernel_hal::shrinker::Shrinker {
    name: "page-cache",
    count,
    scan,
};

#[cfg(test)]
#[path = "page_cache_reclaim_tests.rs"]
mod page_cache_reclaim_tests;
