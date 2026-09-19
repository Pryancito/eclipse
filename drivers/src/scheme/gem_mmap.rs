//! Physical-address registry for driver-private GEM objects that need to be
//! CPU-mmap-able through the same "fake mmap offset" mechanism
//! `linux-object` already uses for `CREATE_DUMB` buffers (`DrmDev::get_vmo`
//! in `linux-object/src/fs/devfs/drm_scheme.rs`, decoding `offset >>
//! PAGE_SHIFT` back into a handle id and looking it up). That mechanism
//! only knows about `linux-object`'s own generic GEM handle table
//! (`drm::get_handle`, populated by `CREATE_DUMB`/PRIME import) -- a lower
//! crate like this one cannot register into it (`drivers` has no
//! dependency on `linux-object`; see `display::nouveau_uapi`'s module doc
//! for the layering constraint this works around). This is the same
//! problem [`crate::scheme::syncobj`] solves for DRM syncobjs: state that
//! both a driver (`nvidia.rs`'s nouveau-uAPI `GEM_NEW`, populating it) and
//! `linux-object`'s ioctl dispatch (`get_vmo`, querying it) need to reach,
//! living in the lower crate both already depend on.
//!
//! # Handle namespace
//!
//! Entries here are looked up by the SAME id space `get_vmo` decodes from
//! `offset >> PAGE_SHIFT` for `linux-object`'s own handle table, so a
//! collision between the two tables would resolve to the wrong physical
//! range -- silently mapping the wrong memory into a process. This module
//! does not enforce disjointness itself (it has no visibility into
//! `linux-object`'s counter); callers must. Convention: `linux-object`'s
//! `DRM_STATE.next_handle_id` starts at 1 and grows sequentially, so
//! driver-private handles registered here use the high half of the `u32`
//! range instead. Collision with that table is only possible after
//! billions of `CREATE_DUMB` allocations in a single boot, not a real
//! constraint.
//!
//! The *other* collision is between GPUs, and it is very real: this table
//! (and `linux-object`'s `NOUVEAU_CPU_VMOS`) is keyed by a bare `u32`, with
//! no GPU in the key, while every `NvidiaGpu` runs its own `GEM_NEW`
//! counter. Two cards handing out the same first handle used to alias each
//! other here, so a `mmap`/PRIME on one node could resolve to the other
//! card's physical range and a `GEM_CLOSE` could free the wrong object.
//! [`alloc_handle_slice`] fixes that at the source: each GPU reserves a
//! disjoint slice of the driver-private half at construction and never
//! hands out an id outside it, which makes the global key unique again.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use lock::Mutex;

/// First id of the driver-private handle range: the high half of `u32`,
/// disjoint from `linux-object`'s own sequential-from-1 handle ids.
pub const DRIVER_HANDLE_BASE: u32 = 0x8000_0000;

/// Handles reserved for one GPU (32Mi). A single boot cannot plausibly run
/// through that many `GEM_NEW`s on one card, and it still leaves room for
/// [`HANDLE_SLICES`] cards.
pub const HANDLES_PER_GPU: u32 = 0x0200_0000;

/// How many GPUs can hold a private slice (64, matching the `/dev/dri` node
/// table's cap). Past that, [`alloc_handle_slice`] refuses rather than
/// wrapping onto another card's ids.
pub const HANDLE_SLICES: u32 = DRIVER_HANDLE_BASE / HANDLES_PER_GPU;

/// Next free slice, in registration order. Boot-time only in practice (one
/// `fetch_add` per `NvidiaGpu::new`).
static NEXT_SLICE: AtomicU32 = AtomicU32::new(0);

/// A GPU's private, half-open range of driver-private GEM handle ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandleSlice {
    base: u32,
    end: u32,
}

impl HandleSlice {
    /// First id this GPU may hand out. `base` itself is skipped for slice 0
    /// so that handle `0x8000_0000` is never used -- historically the
    /// counter started at `0x8000_0001`.
    pub const fn base(&self) -> u32 {
        self.base
    }
    /// One past the last id this GPU may hand out.
    pub const fn end(&self) -> u32 {
        self.end
    }
    /// An empty slice: every id is out of range, so `GEM_NEW` fails cleanly
    /// instead of aliasing another GPU. Handed out once the slices run out.
    pub const fn exhausted() -> Self {
        Self {
            base: u32::MAX,
            end: u32::MAX,
        }
    }
    pub const fn contains(&self, handle: u32) -> bool {
        handle >= self.base && handle < self.end
    }
}

/// Reserve the next per-GPU slice of the driver-private handle range. Called
/// once per GPU, from `NvidiaGpu::new`.
pub fn alloc_handle_slice() -> HandleSlice {
    let slot = NEXT_SLICE.fetch_add(1, Ordering::Relaxed);
    if slot >= HANDLE_SLICES {
        crate::klog_warn!(
            "[gem] more than {} GPUs asked for a GEM handle slice -- GEM_NEW on this one will \
             fail rather than alias another card's handles",
            HANDLE_SLICES
        );
        return HandleSlice::exhausted();
    }
    let base = DRIVER_HANDLE_BASE + slot * HANDLES_PER_GPU;
    HandleSlice {
        // Skip id `0x8000_0000` on the first slice only, so the very first
        // handle stays `0x8000_0001` as it has always been.
        base: if slot == 0 { base + 1 } else { base },
        end: base + HANDLES_PER_GPU,
    }
}

struct MappedGem {
    handle: u32,
    phys_addr: u64,
    size: u64,
    /// Every holder of a reference to this object, BY PID, one entry per
    /// reference: the `GEM_NEW` creator first, then one entry per PRIME
    /// *self-import* (`FD_TO_HANDLE` handing back this ORIGINAL handle, see
    /// `lookup_by_phys`). Real Linux DRM keeps a GEM object alive as long as
    /// any handle or dma-buf references it, and handles are per `drm_file`;
    /// this is the closest a global handle namespace gets. Each `GEM_CLOSE`
    /// drops ONE entry of the closing pid ([`dec_ref`]), a process exit drops
    /// every entry of that pid ([`release_pid`]), and the object is only truly
    /// freed when the list is empty. A pid that is not in the list may not
    /// use, map, bind or close the object ([`holds`]) -- before this the
    /// count was anonymous, so any process could `GEM_CLOSE` or `VM_BIND` the
    /// compositor's buffers by guessing a handle, and a client that died
    /// without closing its own self-imports leaked the buffer until reboot
    /// (its import references were not attributable to it).
    holders: Vec<u64>,
}

/// Outcome of [`dec_ref`], so `GEM_CLOSE` can tell "still shared, keep it" from
/// "last reference gone, free it" from "not one of ours".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecRef {
    /// The reference was dropped but others remain; the value is the number
    /// of references left (> 0). The GEM object and its backing memory MUST
    /// stay alive.
    StillReferenced(u32),
    /// The last reference was dropped; the entry has already been removed from
    /// this table. The caller should now free the GEM object / RM memory.
    Freed,
    /// `handle` was never registered here (a GEM object with no phys mapping,
    /// or an unknown handle). The caller decides what that means.
    NotTracked,
    /// `handle` is tracked but the calling pid holds no reference to it: not
    /// its buffer to close. Nothing was changed.
    NotHolder,
}

lazy_static::lazy_static! {
    static ref MAPPINGS: Mutex<Vec<MappedGem>> = Mutex::new(Vec::new());
}

/// Registers the physical mapping for `handle`, with `owner_pid` as its first
/// holder. If `handle` is already present (re-registration of the same id)
/// only the physical range is updated -- the existing holders are preserved
/// so a stray re-register can never resurrect a freed reference.
pub fn register(handle: u32, phys_addr: u64, size: u64, owner_pid: u64) {
    let mut table = MAPPINGS.lock();
    if let Some(e) = table.iter_mut().find(|e| e.handle == handle) {
        e.phys_addr = phys_addr;
        e.size = size;
    } else {
        table.push(MappedGem {
            handle,
            phys_addr,
            size,
            holders: alloc::vec![owner_pid],
        });
    }
}

/// Adds a PRIME reference to `handle` held by `pid` (a self-import resolved
/// back to this original nouveau handle). Returns the new reference count, or
/// `None` if the handle is not tracked here. Pairs with [`dec_ref`] on
/// `GEM_CLOSE`.
pub fn add_ref(handle: u32, pid: u64) -> Option<u32> {
    let mut table = MAPPINGS.lock();
    table.iter_mut().find(|e| e.handle == handle).map(|e| {
        e.holders.push(pid);
        e.holders.len() as u32
    })
}

/// Whether `pid` holds a reference to `handle`. Pid 0 (a call with no current
/// thread -- kernel-internal) answers `true`. An untracked **nouveau-range**
/// handle (`>= 0x8000_0000`) answers `false` so a guess cannot map/bind/close
/// someone else's GEM; untracked low-range (dumb/generic) handles still
/// answer `true` so `linux-object`'s own table remains authoritative there.
pub fn holds(handle: u32, pid: u64) -> bool {
    if pid == 0 {
        return true;
    }
    match MAPPINGS.lock().iter().find(|e| e.handle == handle) {
        Some(e) => e.holders.contains(&pid),
        None => handle < 0x8000_0000,
    }
}

/// Drops one reference to `handle` held by `pid` (a `GEM_CLOSE`). See
/// [`DecRef`] for the outcomes. Pid 0 drops any one reference. When the last
/// reference goes the entry is removed here BEFORE the caller frees the
/// backing memory, preserving the "no mmap-able mapping can outlive the VRAM
/// it points at" invariant `GEM_CLOSE` has always kept.
pub fn dec_ref(handle: u32, pid: u64) -> DecRef {
    let mut table = MAPPINGS.lock();
    let Some(pos) = table.iter().position(|e| e.handle == handle) else {
        return DecRef::NotTracked;
    };
    let e = &mut table[pos];
    let Some(i) = e.holders.iter().position(|h| pid == 0 || *h == pid) else {
        return DecRef::NotHolder;
    };
    e.holders.swap_remove(i);
    if e.holders.is_empty() {
        table.remove(pos);
        DecRef::Freed
    } else {
        DecRef::StillReferenced(e.holders.len() as u32)
    }
}

/// Drops EVERY reference `pid` holds, across all objects (process exit).
/// Returns each touched handle with `true` when that was its last reference
/// (entry removed, caller frees the object) or `false` when other holders
/// remain.
pub fn release_pid(pid: u64) -> Vec<(u32, bool)> {
    let mut out = Vec::new();
    if pid == 0 {
        return out;
    }
    let mut table = MAPPINGS.lock();
    table.retain_mut(|e| {
        let before = e.holders.len();
        e.holders.retain(|h| *h != pid);
        if e.holders.len() == before {
            return true;
        }
        let freed = e.holders.is_empty();
        out.push((e.handle, freed));
        !freed
    });
    out
}

/// Drops `handle`'s mapping unconditionally, ignoring the share count. Returns
/// whether one existed. A hard reset primitive: unlike [`dec_ref`], it frees the
/// entry no matter how many holders remain, so it must NOT be used where another
/// process may still import the buffer. It is deliberately NOT used by
/// process-exit teardown any more: that path drops one reference with [`dec_ref`]
/// and keeps a still-imported buffer alive (see `NvidiaGpu::nouveau_release_process`),
/// because a client exiting while the compositor still displays its window used
/// to free the buffer out from under the compositor -> its next GEM_INFO/VM_BIND
/// on that handle faulted. Kept as a last-resort primitive; currently unused.
#[allow(dead_code)]
pub fn unregister(handle: u32) -> bool {
    let mut table = MAPPINGS.lock();
    if let Some(pos) = table.iter().position(|e| e.handle == handle) {
        table.remove(pos);
        true
    } else {
        false
    }
}

/// Looks up `handle`'s `(phys_addr, size)`, e.g. from `DrmDev::get_vmo`.
pub fn lookup(handle: u32) -> Option<(u64, u64)> {
    MAPPINGS
        .lock()
        .iter()
        .find(|e| e.handle == handle)
        .map(|e| (e.phys_addr, e.size))
}

/// Like [`lookup`], but only if [`holds`] says `pid` may use `handle`.
pub fn lookup_for(handle: u32, pid: u64) -> Option<(u64, u64)> {
    if !holds(handle, pid) {
        return None;
    }
    lookup(handle)
}

/// Reverse lookup: which driver-private GEM handle owns the object whose
/// backing starts at `phys_addr`? Used by PRIME `FD_TO_HANDLE`: importing a
/// dma-buf that THIS driver exported must hand back the ORIGINAL nouveau
/// handle (real Linux DRM semantics -- self-import resolves to the existing
/// GEM object), because only that handle works with the nouveau-uAPI
/// `GEM_INFO`/`VM_BIND`/`EXEC`. A fresh generic handle over the same memory
/// looks fine to the generic ioctls and then fails every driver-private one.
pub fn lookup_by_phys(phys_addr: u64) -> Option<(u32, u64)> {
    MAPPINGS
        .lock()
        .iter()
        .find(|e| e.phys_addr == phys_addr)
        .map(|e| (e.handle, e.size))
}

#[cfg(test)]
mod handle_slice_tests {
    use super::*;

    /// The property the whole thing exists for: two GPUs never see the same
    /// id. `MAPPINGS` and `linux-object`'s `NOUVEAU_CPU_VMOS` are keyed by a
    /// bare `u32`, so an overlap here is a buffer resolving to the wrong
    /// card's memory.
    #[test]
    fn consecutive_slices_are_disjoint() {
        let a = alloc_handle_slice();
        let b = alloc_handle_slice();

        assert!(a.base() >= DRIVER_HANDLE_BASE);
        assert!(a.base() < a.end());
        // Other tests share the counter, so b may be further along than the
        // next slot -- never before it.
        assert!(a.end() <= b.base());

        assert!(a.contains(a.base()));
        assert!(a.contains(a.end() - 1));
        assert!(!a.contains(a.end()));
        assert!(!a.contains(b.base()));
        assert!(!b.contains(a.end() - 1));
    }

    /// Past the last slice the answer is "no handles", not "someone else's
    /// handles": `GEM_NEW` fails cleanly instead of aliasing another GPU.
    #[test]
    fn the_exhausted_slice_holds_nothing() {
        let s = HandleSlice::exhausted();
        assert!(!s.contains(0));
        assert!(!s.contains(DRIVER_HANDLE_BASE));
        assert!(!s.contains(u32::MAX));
    }

    /// The slices tile the driver-private half exactly: no id in
    /// `DRIVER_HANDLE_BASE..=u32::MAX` belongs to no slice, and the last
    /// slice ends exactly at the top of `u32` rather than wrapping to 0.
    #[test]
    fn the_slices_tile_the_driver_private_half() {
        assert_eq!(HANDLE_SLICES * HANDLES_PER_GPU, DRIVER_HANDLE_BASE);
        let last_end =
            (DRIVER_HANDLE_BASE as u64) + (HANDLE_SLICES as u64) * (HANDLES_PER_GPU as u64);
        assert_eq!(last_end, (u32::MAX as u64) + 1);
    }
}
