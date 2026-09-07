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
//! range instead (see `nouveau_uapi.rs`'s `nouveau_gem_next_handle`
//! starting at `0x8000_0001`). Collision is only possible after billions
//! of `CREATE_DUMB` allocations in a single boot, not a real constraint.

use alloc::vec::Vec;
use lock::Mutex;

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
/// thread -- kernel-internal) and an untracked handle answer `true`: the
/// caller's own bookkeeping decides those.
pub fn holds(handle: u32, pid: u64) -> bool {
    if pid == 0 {
        return true;
    }
    match MAPPINGS.lock().iter().find(|e| e.handle == handle) {
        Some(e) => e.holders.contains(&pid),
        None => true,
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
