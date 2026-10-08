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

/// The slice a test-built GPU (`NvidiaGpu::for_test`) hands out from: a
/// fixed one past slot 0, not a reservation. The test suite builds a GPU
/// per test and the reservations are 64 for the whole binary, so building
/// one more test than that turned every later slice `exhausted` and failed
/// tests that never asked for a handle. Tests run one at a time under
/// their own lock, so two test GPUs never hold this slice at once.
#[cfg(test)]
pub fn test_handle_slice() -> HandleSlice {
    HandleSlice {
        base: DRIVER_HANDLE_BASE + HANDLES_PER_GPU,
        end: DRIVER_HANDLE_BASE + 2 * HANDLES_PER_GPU,
    }
}

struct MappedGem {
    handle: u32,
    phys_addr: u64,
    size: u64,
    /// Every holder of a reference to this object, BY PID, one entry per
    /// reference: the `GEM_NEW` creator first, then one entry per PRIME
    /// *self-import* (`FD_TO_HANDLE` handing back this ORIGINAL handle, see
    /// `lookup_by_phys`), plus one entry per live dma-buf exported from it
    /// ([`DMABUF_HOLDER`]) and per KMS framebuffer that pins it. Real Linux
    /// DRM keeps a GEM object alive as long as any handle or dma-buf
    /// references it, and handles are per `drm_file`; this is the closest a
    /// global handle namespace gets. Each `GEM_CLOSE` drops ONE entry of the
    /// closing pid ([`dec_ref`]), a process exit drops every entry of that
    /// pid ([`release_pid`]), and the object is only truly freed when the
    /// list is empty. A pid that is not in the list may not use, map, bind
    /// or close the object ([`holds`]) -- before this the count was
    /// anonymous, so any process could `GEM_CLOSE` or `VM_BIND` the
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

/// Holder id a live dma-buf fd records against a nouveau GEM object.
///
/// Real Linux keeps a GEM object alive for as long as any handle **or dma-buf**
/// references it. Our export path used to wrap the physical range in a
/// dma-buf without taking a `gem_mmap` reference,
/// so the exporter's subsequent `GEM_CLOSE` (the normal DRI3 dance: export fd,
/// hand it to Xwayland, close the local handle) freed the object while the
/// dma-buf fd was still in flight. `lookup_by_phys` then missed on the
/// importer side → generic handle → `GEM_INFO` ENOENT. Wayland-native clients
/// usually keep their GEM handle open until `wl_buffer.release`, so the same
/// bug was invisible there; the X11/Xwayland chain of three processes is what
/// surfaces it. `u64::MAX - 1` is never a real pid (and is distinct from the
/// KMS framebuffer sentinel `u64::MAX` in `linux-object`'s drm module).
pub const DMABUF_HOLDER: u64 = u64::MAX - 1;

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

/// How many references `handle` has, or `None` if it is not tracked here.
pub fn ref_count(handle: u32) -> Option<u32> {
    MAPPINGS
        .lock()
        .iter()
        .find(|e| e.handle == handle)
        .map(|e| e.holders.len() as u32)
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
    holds_entry(
        MAPPINGS.lock().iter().find(|e| e.handle == handle),
        handle,
        pid,
    )
}

/// The ownership decision [`holds`] makes, given the entry a scan already
/// found (or `None` for an untracked handle). Factored out so a caller that
/// has to walk the table anyway -- [`lookup_for`], which every
/// driver-private mmap, PRIME export and `ADDFB` goes through -- can decide
/// and read the range in ONE pass instead of two. The table is a linear scan
/// and the second walk cost as much as the first: 120.9 ns against 58.6 for
/// `lookup` alone at 256 live objects (`benches::look_up_for_a_holder_in_a_
/// table_of_256` against `benches::lookup_one_of_256`).
///
/// `pid == 0` is the caller's to handle before calling this, because it is
/// decided without the table at all.
fn holds_entry(entry: Option<&MappedGem>, handle: u32, pid: u64) -> bool {
    match entry {
        Some(e) => e.holders.contains(&pid),
        None => handle < DRIVER_HANDLE_BASE,
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

/// Moves every reference `from` holds to `to`, across all objects: a zombie
/// context whose pid the pool has handed to a new process
/// (`NvidiaGpu::rekey_zombie_wearing`), so [`release_pid`] of the zombie's
/// teardown later drops the zombie's references and none of the new
/// process's. Returns how many moved.
pub fn rekey_pid(from: u64, to: u64) -> usize {
    if from == 0 {
        return 0;
    }
    let mut n = 0;
    for e in MAPPINGS.lock().iter_mut() {
        for h in e.holders.iter_mut() {
            if *h == from {
                *h = to;
                n += 1;
            }
        }
    }
    n
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
///
/// A refusal here is SILENT to the caller -- PRIME export, the driver-private
/// mmap and `ADDFB` all just see `None` and answer the same ENOENT/EINVAL they
/// would for a handle that does not exist. That is the right answer for a
/// prober and a terrible one to debug: if some legitimate holder is ever
/// missing from `holders`, a real client (NVK under Xwayland, where the buffer
/// crosses client -> Xwayland -> compositor) loses a buffer it owns and hangs
/// with nothing in the log to say why.
///
/// So say it, loudly and at most a few times per boot: an entry that EXISTS but
/// is not held by the caller is the only case worth reporting -- an unknown
/// handle is an ordinary miss, not an ownership decision.
pub fn lookup_for(handle: u32, pid: u64) -> Option<(u64, u64)> {
    // ONE pass: the scan that decides ownership is the scan that reads the
    // range. This used to call `holds` and then `lookup`, walking the table
    // twice for every accepted handle, which at 256 live objects was 120.9 ns
    // where one walk is 58.6 (see `holds_entry`). The guard is dropped before
    // anything is logged -- a temporary lock guard left in an `if` condition
    // would be held across the log call below.
    let (allowed, range, tracked) = {
        let table = MAPPINGS.lock();
        let entry = table.iter().find(|e| e.handle == handle);
        let range = entry.map(|e| (e.phys_addr, e.size));
        let allowed = pid == 0 || holds_entry(entry, handle, pid);
        (allowed, range, entry.is_some())
    };
    if allowed {
        return range;
    }
    {
        use core::sync::atomic::{AtomicU32, Ordering};
        static DENIED: AtomicU32 = AtomicU32::new(0);
        if tracked {
            let n = DENIED.fetch_add(1, Ordering::Relaxed);
            if n < 8 {
                log::error!(
                    "[gem] handle={:#x} refused to pid={} -- it is tracked but that pid holds no \
                     reference (denial {}/8 this boot). If a working client just lost a buffer, \
                     this is why: the holder list is missing whoever legitimately imported it.",
                    handle,
                    pid,
                    n + 1
                );
            }
        }
    }
    None
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

/// The buffer-sharing chain an X11 GL client actually walks, which is one hop
/// longer than a Wayland one and is the reason it breaks on its own.
///
/// A native Wayland client hands its buffer to the compositor: two processes,
/// one export and one import. An X11 client under Xwayland hands it to
/// Xwayland, which hands it on to the compositor: **three** processes, and the
/// middle one both imports and re-exports a buffer it did not allocate. Every
/// ownership rule at the uAPI edge has to let that middle hop through, and a
/// rule that is correct for two processes can be wrong for three — which is
/// invisible in QEMU, where there is no nouveau GEM object in the first place
/// and the whole path is never walked.
///
/// These drive the pid-parameterised layer directly (`lookup_for`, `add_ref`,
/// `dec_ref`, `release_pid`) because the uAPI entry points above read the
/// caller from the current thread, which a host test does not have.
#[cfg(test)]
mod one_pass_lookup_tests {
    use super::*;

    /// `lookup_for` reads the range in the same pass that decides ownership,
    /// where it used to call `holds` and then `lookup` and walk the table
    /// twice. The two have to answer the same thing in every case the old
    /// pair did, so this walks the matrix: tracked or not, in the
    /// driver-private range or below it, held by the caller or not, and the
    /// pid-0 arm that is decided without the table at all.
    #[test]
    fn one_pass_answers_what_the_two_passes_did() {
        /// The arrangement the old code made, spelled out: ownership first
        /// through `holds`, then the range through `lookup`.
        fn two_passes(handle: u32, pid: u64) -> Option<(u64, u64)> {
            if !holds(handle, pid) {
                return None;
            }
            lookup(handle)
        }

        const OWNER: u64 = 99_101;
        const STRANGER: u64 = 99_102;
        // Clear of every other module's range in this file.
        let tracked_hi = DRIVER_HANDLE_BASE + 0x0200_0001;
        let untracked_hi = DRIVER_HANDLE_BASE + 0x0200_0002;
        let untracked_lo = 0x42u32;
        let phys = 0xa0_0000_0000u64;
        register(tracked_hi, phys, 0x2_0000, OWNER);

        for (handle, pid) in [
            (tracked_hi, OWNER),
            (tracked_hi, STRANGER),
            (tracked_hi, 0),
            (untracked_hi, OWNER),
            (untracked_hi, 0),
            (untracked_lo, OWNER),
            (untracked_lo, 0),
        ] {
            assert_eq!(
                lookup_for(handle, pid),
                two_passes(handle, pid),
                "handle {:#x} pid {}",
                handle,
                pid
            );
        }
        // And the answers are the ones that matter, not just equal to each
        // other: the owner gets the range, a stranger gets nothing from a
        // tracked driver-private handle, and an untracked low handle is not
        // this table's to refuse.
        assert_eq!(lookup_for(tracked_hi, OWNER), Some((phys, 0x2_0000)));
        assert_eq!(lookup_for(tracked_hi, STRANGER), None);
        assert_eq!(lookup_for(untracked_hi, OWNER), None);
        assert_eq!(lookup_for(untracked_lo, OWNER), None, "nothing registered");
        assert!(holds(untracked_lo, OWNER), "low handles stay permissive");

        while !matches!(dec_ref(tracked_hi, 0), DecRef::NotTracked | DecRef::Freed) {}
    }
}

#[cfg(test)]
mod xwayland_chain_tests {
    use super::*;

    /// Handles must be in the driver-private range: [`holds`] is deliberately
    /// strict there and permissive below it, so a low id would pass every
    /// assertion here for the wrong reason.
    const CLIENT: u64 = 88_001;
    const XWAYLAND: u64 = 88_002;
    const COMPOSITOR: u64 = 88_003;
    const STRANGER: u64 = 88_004;

    fn drop_handle(handle: u32) {
        while !matches!(dec_ref(handle, 0), DecRef::NotTracked | DecRef::Freed) {}
    }

    /// Client allocates and exports, Xwayland imports and re-exports, the
    /// compositor imports. Every hop must resolve the buffer for the process
    /// making it.
    #[test]
    fn a_buffer_survives_the_client_xwayland_compositor_chain() {
        let handle = DRIVER_HANDLE_BASE + 0x10_0001;
        let phys = 0x4_0000_0000;
        register(handle, phys, 0x10_0000, CLIENT);

        // 1. The client exports it (PRIME_HANDLE_TO_FD).
        assert!(
            lookup_for(handle, CLIENT).is_some(),
            "the allocator can export its own buffer"
        );

        // 2. Xwayland imports it. A self-import resolves back to the original
        //    nouveau handle — only that handle works with GEM_INFO/VM_BIND —
        //    and takes a reference for the importer.
        let (resolved, _) = lookup_by_phys(phys).expect("self-import finds the original handle");
        assert_eq!(resolved, handle);
        assert_eq!(add_ref(resolved, XWAYLAND), Some(2));

        // 3. Xwayland re-exports it to the compositor. THIS is the hop a
        //    Wayland client never makes, and the one an ownership rule
        //    written for "the allocator" alone refuses.
        assert!(
            lookup_for(handle, XWAYLAND).is_some(),
            "Xwayland can re-export a buffer it imported but did not allocate"
        );

        // 4. The compositor imports it and can reach it too.
        assert_eq!(add_ref(handle, COMPOSITOR), Some(3));
        assert!(
            lookup_for(handle, COMPOSITOR).is_some(),
            "the compositor can scan out what reached it through Xwayland"
        );

        drop_handle(handle);
    }

    /// An X11 client exiting is routine — the window closes — and must not
    /// pull the buffer out from under Xwayland or the compositor, which are
    /// still holding references to it.
    #[test]
    fn the_client_exiting_does_not_pull_the_buffer_from_xwayland() {
        let handle = DRIVER_HANDLE_BASE + 0x10_0002;
        let phys = 0x4_0010_0000;
        register(handle, phys, 0x10_0000, CLIENT);
        add_ref(handle, XWAYLAND);
        add_ref(handle, COMPOSITOR);

        let freed = release_pid(CLIENT);
        assert!(
            !freed
                .iter()
                .any(|(h, was_freed)| *h == handle && *was_freed),
            "the client's exit drops its own reference only"
        );
        assert!(
            lookup_for(handle, XWAYLAND).is_some(),
            "Xwayland still holds the buffer after the client is gone"
        );
        assert!(
            lookup_for(handle, COMPOSITOR).is_some(),
            "so does the compositor"
        );
        assert!(
            lookup_for(handle, CLIENT).is_none(),
            "the process that left no longer holds it"
        );

        drop_handle(handle);
    }

    /// The other half of the same rule: passing through Xwayland must not turn
    /// the buffer into something any process can name. A process that never
    /// imported it gets nothing, even while three others hold it.
    #[test]
    fn a_process_that_never_imported_cannot_reach_the_buffer() {
        let handle = DRIVER_HANDLE_BASE + 0x10_0003;
        register(handle, 0x4_0020_0000, 0x10_0000, CLIENT);
        add_ref(handle, XWAYLAND);
        add_ref(handle, COMPOSITOR);

        assert!(
            lookup_for(handle, STRANGER).is_none(),
            "an unrelated process cannot resolve the handle"
        );
        assert!(
            matches!(dec_ref(handle, STRANGER), DecRef::NotHolder),
            "nor close it out from under the three that hold it"
        );
        assert!(
            lookup_for(handle, XWAYLAND).is_some(),
            "and the refused close changed nothing"
        );

        drop_handle(handle);
    }

    /// Each hop's `GEM_CLOSE` drops exactly one reference; the backing memory
    /// is freed only when the last one goes. A chain one process longer than
    /// the Wayland case is one more chance to free it too early.
    #[test]
    fn the_buffer_is_freed_only_after_the_last_hop_closes_it() {
        let handle = DRIVER_HANDLE_BASE + 0x10_0004;
        register(handle, 0x4_0030_0000, 0x10_0000, CLIENT);
        add_ref(handle, XWAYLAND);
        add_ref(handle, COMPOSITOR);

        assert!(matches!(
            dec_ref(handle, CLIENT),
            DecRef::StillReferenced(2)
        ));
        assert!(matches!(
            dec_ref(handle, XWAYLAND),
            DecRef::StillReferenced(1)
        ));
        assert!(matches!(dec_ref(handle, COMPOSITOR), DecRef::Freed));
        assert!(
            lookup(handle).is_none(),
            "the entry goes before the VRAM behind it is released"
        );
    }

    /// The regression this guards. An X11 GL client exports a dma-buf, hands
    /// the fd to Xwayland, and closes its local GEM handle — that close must
    /// NOT free the object while the dma-buf is still live. Without a
    /// `DMABUF_HOLDER` reference taken at export, `lookup_by_phys` misses on
    /// the importer and the whole DRI3 chain collapses.
    #[test]
    fn a_dmabuf_keeps_the_buffer_alive_after_the_exporter_closes() {
        let handle = DRIVER_HANDLE_BASE + 0x10_0005;
        let phys = 0x4_0040_0000;
        register(handle, phys, 0x10_0000, CLIENT);

        // PRIME_HANDLE_TO_FD: the dma-buf takes its own reference.
        assert_eq!(add_ref(handle, DMABUF_HOLDER), Some(2));

        // Client GEM_CLOSE after export — the DRI3 dance.
        assert!(
            matches!(dec_ref(handle, CLIENT), DecRef::StillReferenced(1)),
            "closing the exporter's handle must leave the dma-buf's reference"
        );
        assert!(
            lookup_by_phys(phys).is_some(),
            "self-import on the receiving end still finds the original handle"
        );

        // Xwayland (or the compositor) imports the still-live dma-buf.
        assert_eq!(add_ref(handle, XWAYLAND), Some(2));
        assert!(lookup_for(handle, XWAYLAND).is_some());

        // Last close of the dma-buf fd drops its holder; Xwayland still owns it.
        assert!(matches!(
            dec_ref(handle, DMABUF_HOLDER),
            DecRef::StillReferenced(1)
        ));
        assert!(lookup_for(handle, XWAYLAND).is_some());

        drop_handle(handle);
    }
}

/// Native `#[bench]` rows for the GEM handle registry — the table every
/// driver-private GEM ioctl goes through.
///
/// `MAPPINGS` is a `Vec<MappedGem>` behind one mutex and every operation on it
/// is a linear scan, with each entry's `holders` a `Vec<u64>` scanned in turn.
/// That is the right shape for a handful of buffers and the question these
/// rows answer is where "a handful" stops: a compositor with Xwayland and a
/// browser under it holds one object per surface, per texture upload staging
/// buffer and per KMS framebuffer, and every `GEM_CLOSE`, `GEM_INFO`,
/// `VM_BIND`, driver-private mmap, PRIME export and `ADDFB` pays a scan.
///
/// Inline rather than in `benches/`, like the rest of this crate: `MAPPINGS`
/// and `MappedGem` are private to this module, so a separate target could not
/// build a table to measure.
///
/// Nothing here touches hardware: the registry is a `Vec` of integers and a
/// mutex, so these are the real figures. The rows take a lock of their own and
/// use a handle range no test uses, and each one empties what it registered
/// before it returns, so a row's table size does not leak into the next row's
/// or into a test's scans.
///
/// There is no empty-equivalent row here and the lookup family does not need
/// one: a floor says whether a *flat* family is measuring work, and this
/// family is not flat — it scales with the table, a miss costs exactly twice
/// a hit in the middle, and the slope is the evidence. (A non-inlined control
/// taking the same lock reads 13.5 ns, above the one-object row it would be
/// bounding, because `lookup` inlines and the control pays a call it does
/// not: a floor built out of the wrong shape measures the control.)
///
/// `cargo +nightly bench -p zcore-drivers --features graphic,virtio,xhci-usb-hid`
#[cfg(test)]
mod benches {
    use super::*;
    use test::{black_box, Bencher};

    /// These rows write `MAPPINGS`, which is process-wide, so they take turns
    /// with each other.
    static SERIAL: lock::Mutex<()> = lock::Mutex::new(());

    /// A handle range of this module's own, clear of every test's.
    const BASE: u32 = DRIVER_HANDLE_BASE + 0x0100_0000;
    const PID: u64 = 77_001;

    /// A physical base far from any test's, so `lookup_by_phys` cannot match
    /// one of their entries.
    const PHYS: u64 = 0x90_0000_0000;

    /// Register `count` objects, each held by one pid, and hand back the
    /// handle in the middle of the table — the average a scan walks to.
    fn populate(count: u32) -> u32 {
        for i in 0..count {
            register(BASE + i, PHYS + (i as u64) * 0x10_0000, 0x10_0000, PID);
        }
        BASE + count / 2
    }

    fn depopulate(count: u32) {
        for i in 0..count {
            while !matches!(dec_ref(BASE + i, 0), DecRef::NotTracked | DecRef::Freed) {}
        }
    }

    // --- the scan every GEM ioctl pays ---

    fn bench_lookup(b: &mut Bencher, count: u32) {
        let _g = SERIAL.lock();
        let middle = populate(count);
        b.iter(|| black_box(lookup(black_box(middle))));
        depopulate(count);
    }

    /// One live object: the floor, and what the table looks like in a VM with
    /// nothing but the console on it.
    #[bench]
    fn lookup_one_of_1(b: &mut Bencher) {
        bench_lookup(b, 1);
    }

    /// Sixteen, about a bare compositor.
    #[bench]
    fn lookup_one_of_16(b: &mut Bencher) {
        bench_lookup(b, 16);
    }

    /// Sixty-four.
    #[bench]
    fn lookup_one_of_64(b: &mut Bencher) {
        bench_lookup(b, 64);
    }

    /// Two hundred and fifty-six: a compositor, Xwayland and a couple of
    /// clients with their swapchains and staging buffers.
    #[bench]
    fn lookup_one_of_256(b: &mut Bencher) {
        bench_lookup(b, 256);
    }

    /// A thousand, which a browser with many tabs reaches on its own. If the
    /// scan is the cost, this row is a thousand times the first.
    #[bench]
    fn lookup_one_of_1024(b: &mut Bencher) {
        bench_lookup(b, 1024);
    }

    /// A handle that is not in the table at all: the scan runs to the end
    /// without matching, which is the worst case of the same walk and what a
    /// generic (dumb) handle costs on every call that asks here first.
    #[bench]
    fn miss_in_a_table_of_1024(b: &mut Bencher) {
        let _g = SERIAL.lock();
        populate(1024);
        b.iter(|| black_box(lookup(black_box(BASE + 0x0010_0000))));
        depopulate(1024);
    }

    // --- the ownership check, which rides the lookup's own scan ---

    /// `holds`: the same scan plus a walk of the entry's holder list.
    #[bench]
    fn check_the_holder_of_1_in_a_table_of_256(b: &mut Bencher) {
        let _g = SERIAL.lock();
        let middle = populate(256);
        b.iter(|| black_box(holds(black_box(middle), black_box(PID))));
        depopulate(256);
    }

    /// `lookup_for` is what the driver-private mmap, PRIME export and `ADDFB`
    /// actually call, and it is the row this batch's fix came out of: on the
    /// accept path it used to scan the table **twice**, once inside `holds`
    /// and once inside `lookup`, and read **120.9 ns** here where one walk is
    /// 58.1. It now decides ownership over the entry its single scan already
    /// found, so the row should land **level with `lookup_one_of_256`** — the
    /// check free on top of the walk it already needed. A figure near twice
    /// that one again would mean the second walk is back.
    #[bench]
    fn look_up_for_a_holder_in_a_table_of_256(b: &mut Bencher) {
        let _g = SERIAL.lock();
        let middle = populate(256);
        b.iter(|| black_box(lookup_for(black_box(middle), black_box(PID))));
        depopulate(256);
    }

    /// An object held by a long holder list — the Xwayland chain plus a
    /// dma-buf and a KMS framebuffer is several, and a compositor that
    /// re-imports per frame without closing would be many. The entry is first
    /// in the table so this row is the holder walk and not the table walk.
    #[bench]
    fn check_the_holder_of_64_in_a_table_of_1(b: &mut Bencher) {
        let _g = SERIAL.lock();
        register(BASE, PHYS, 0x10_0000, PID);
        for i in 1..64u64 {
            add_ref(BASE, PID + i).expect("the object is tracked");
        }
        // The last holder added, so the walk runs the whole list.
        b.iter(|| black_box(holds(black_box(BASE), black_box(PID + 63))));
        depopulate(1);
    }

    /// The reverse lookup PRIME `FD_TO_HANDLE` does on every self-import: the
    /// same linear walk, keyed on the physical address instead of the handle.
    #[bench]
    fn reverse_look_up_by_phys_in_a_table_of_256(b: &mut Bencher) {
        let _g = SERIAL.lock();
        populate(256);
        let middle_phys = PHYS + 128 * 0x10_0000;
        b.iter(|| black_box(lookup_by_phys(black_box(middle_phys))));
        depopulate(256);
    }

    // --- registering and closing ---

    /// `register` of a handle already present (a re-register): the scan finds
    /// it and updates the range in place.
    #[bench]
    fn re_register_into_a_table_of_256(b: &mut Bencher) {
        let _g = SERIAL.lock();
        let middle = populate(256);
        b.iter(|| {
            register(
                black_box(middle),
                black_box(PHYS),
                black_box(0x10_0000),
                black_box(PID),
            )
        });
        depopulate(256);
    }

    /// A `GEM_CLOSE` that leaves other references: the scan, the holder walk
    /// and a `swap_remove`. The reference is put back inside the loop so the
    /// row measures the same work every iteration, which means it also carries
    /// one `add_ref` — read it against `check_the_holder_of_1_in_a_table_of_256`
    /// rather than as the cost of a close alone.
    #[bench]
    fn close_one_reference_of_two_in_a_table_of_256(b: &mut Bencher) {
        let _g = SERIAL.lock();
        let middle = populate(256);
        add_ref(middle, PID + 1).expect("the object is tracked");
        b.iter(|| {
            let r = black_box(dec_ref(black_box(middle), black_box(PID + 1)));
            add_ref(middle, PID + 1);
            r
        });
        depopulate(256);
    }

    // --- process exit, which is where the two scans multiply ---

    /// `release_pid` is the process-exit teardown, and it is the one operation
    /// here that walks **every** object and **every** holder of each: a
    /// `retain_mut` over the table with a `retain` over each holder list
    /// inside it.
    ///
    /// Measured for a pid that holds nothing, because that call leaves the
    /// table alone and so can run in a loop over one table. The case where
    /// the exiting pid owns everything cannot be isolated this way — the call
    /// empties the table, so the row would have to rebuild it, and rebuilding
    /// is itself quadratic (see the `register` rows) and swamps what is being
    /// measured. What it adds on top of these two rows is a `Vec::retain` and
    /// one `swap_remove` per owned object.
    #[bench]
    fn release_a_pid_holding_nothing_of_64_objects(b: &mut Bencher) {
        bench_release(b, 64);
    }

    /// The same walk over 256 objects.
    #[bench]
    fn release_a_pid_holding_nothing_of_256_objects(b: &mut Bencher) {
        bench_release(b, 256);
    }

    fn bench_release(b: &mut Bencher, count: u32) {
        let _g = SERIAL.lock();
        populate(count);
        let stranger = PID + 999;
        b.iter(|| black_box(release_pid(black_box(stranger))));
        depopulate(count);
    }

    // --- registering, which is the same scan and is paid per GEM_NEW ---

    /// `register` of a fresh handle into an empty table: the floor of the
    /// family below.
    #[bench]
    fn register_a_fresh_handle_into_an_empty_table(b: &mut Bencher) {
        let _g = SERIAL.lock();
        b.iter(|| {
            register(
                black_box(BASE),
                black_box(PHYS),
                black_box(0x10_0000),
                black_box(PID),
            );
            dec_ref(BASE, PID)
        });
        depopulate(1);
    }

    /// The claim: `register` scans the whole table before appending, so
    /// filling the table is quadratic. This row registers one fresh handle
    /// into a table of 256 and closes it again, so the figure is one full
    /// walk plus a push — and 256 of those is what a compositor's startup
    /// pays to get there.
    #[bench]
    fn register_a_fresh_handle_into_a_table_of_256(b: &mut Bencher) {
        let _g = SERIAL.lock();
        populate(256);
        let fresh = BASE + 0x0010_0000;
        b.iter(|| {
            register(
                black_box(fresh),
                black_box(PHYS),
                black_box(0x10_0000),
                black_box(PID),
            );
            dec_ref(fresh, PID)
        });
        depopulate(256);
    }

    /// `rekey_pid`, the zombie-context handover: it walks every holder of
    /// every object unconditionally, with no early exit, because a pid may
    /// appear in any number of them.
    #[bench]
    fn rekey_a_pid_across_256_objects(b: &mut Bencher) {
        let _g = SERIAL.lock();
        populate(256);
        let mut from = PID;
        b.iter(|| {
            let to = from + 1;
            let n = black_box(rekey_pid(black_box(from), black_box(to)));
            from = to;
            n
        });
        // The holders now carry whatever pid the last iteration moved them to.
        for i in 0..256u32 {
            while !matches!(dec_ref(BASE + i, 0), DecRef::NotTracked | DecRef::Freed) {}
        }
    }
}
