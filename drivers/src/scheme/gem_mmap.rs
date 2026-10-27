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
    alloc_handle_slice_from(&NEXT_SLICE)
}

/// [`alloc_handle_slice`] over any counter, so a test can walk a private one
/// to the end: the real counter is reserved from by every GPU the binary
/// builds and never rewinds, and running IT out would hand every GPU built
/// after the test an empty slice.
fn alloc_handle_slice_from(next: &AtomicU32) -> HandleSlice {
    let slot = next.fetch_add(1, Ordering::Relaxed);
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
        // The last slice ends at the top of `u32`, one past `u32::MAX`, which
        // a `u32` cannot hold: `base + HANDLES_PER_GPU` overflowed there (a
        // panic with overflow checks on, and without them a wrap to 0 that
        // made the 64th card's slice contain nothing). Saturating costs that
        // card one id, `u32::MAX`, and nothing else.
        end: base.saturating_add(HANDLES_PER_GPU),
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
    /// Whether this object's host physical address has ever been handed to a
    /// consumer that needs it (mmap, PRIME export, ADDFB2, the cursor). Only
    /// guards double-counting while the entry lives; the running totals are
    /// the two atomics below, so an object that was used and then freed still
    /// counts.
    host_pa_given: bool,
    /// The range is a GPU's VRAM seen through its BAR1 ([`register_device_memory`]),
    /// not host RAM: mapped write-combining, never write-back.
    device_memory: bool,
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
    /// The objects created `NOUVEAU_GEM_DOMAIN_NO_SHARE`, as `(handle,
    /// owner pid)`. Kept apart from [`MAPPINGS`], which only has the objects
    /// with a host physical address: a VRAM-only object has no mapping entry
    /// and is just as private. A record goes when its object goes
    /// ([`clear_no_share`], [`release_pid`]); nothing else about the object
    /// changes it, since a NO_SHARE object can never gain a second holder.
    static ref NO_SHARE: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());
}

/// Forgets every mapping and every NO_SHARE record, for
/// `crate::test_globals::LOCK`: a handle a test never closed would otherwise
/// keep its holders, and `register` of the same handle by the next test
/// keeps existing holders by design.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    MAPPINGS.lock().clear();
    NO_SHARE.lock().clear();
}

/// How many distinct GEM objects have had their host physical address handed
/// out, and their total bytes.
///
/// `/proc/gpudbg` already counts how many GEM objects COULD give one out (a
/// host PA resolved at `GEM_NEW`), which on the nouveau path is every single
/// one -- `GEM_NEW` backs GART, and GART|VRAM, with sysmem precisely so a
/// host PA exists. This counts the ones something actually asked for, which
/// is the number that bears on a VRAM placement path: an object nothing ever
/// needs a host address for could live in VRAM instead of crossing PCIe on
/// every GPU access.
///
/// "Host PA handed out" is deliberately wider than "a client mmap'd it", and
/// the wider reading is the correct one for this question. A GEM that backs
/// an ADDFB2 framebuffer or the hardware cursor is read by the present path
/// straight out of the physmap window, with no userspace mapping anywhere
/// (`linux-object`'s drm module resolves it and `scanout` reads
/// `phys_to_virt(fb.phys_addr)`), and a GEM exported over PRIME is mapped
/// through the dma-buf's own VMO, which never reaches `DrmDev::get_vmo`.
/// Counting only mmap would call all of those VRAM-eligible when they are
/// not. So the callers are the places that actually take the address: mmap,
/// PRIME export, ADDFB2 and the cursor.
static HOST_PA_COUNT: AtomicU32 = AtomicU32::new(0);
static HOST_PA_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Note that `handle`'s host physical address was handed out, counting it
/// once per object however many consumers take it.
///
/// `lookup_for` itself is not the seam: `gem_handle_mappable` and the ADDFB2
/// and cursor validity checks go through it to answer a yes/no question and
/// take no address, so counting there would count probes.
pub fn note_host_pa_handed_out(handle: u32) {
    let mut table = MAPPINGS.lock();
    if let Some(e) = table.iter_mut().find(|e| e.handle == handle) {
        if !e.host_pa_given {
            e.host_pa_given = true;
            let size = e.size;
            HOST_PA_COUNT.fetch_add(1, Ordering::Relaxed);
            HOST_PA_BYTES.fetch_add(size, Ordering::Relaxed);
        }
    }
}

/// `(objects, bytes)` whose host physical address has been handed out since
/// boot.
pub fn host_pa_totals() -> (u32, u64) {
    (
        HOST_PA_COUNT.load(Ordering::Relaxed),
        HOST_PA_BYTES.load(Ordering::Relaxed),
    )
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
    register_as(handle, phys_addr, size, owner_pid, false);
}

/// [`register`] for an object whose range is VRAM behind a GPU's BAR1 rather
/// than host RAM. The address is as real as a sysmem one -- PRIME, ADDFB2 and
/// mmap take it the same way -- but the CPU must reach it uncached or
/// write-combining: a write-back mapping of device memory is undefined
/// behaviour on x86. See [`is_device_memory`].
pub fn register_device_memory(handle: u32, phys_addr: u64, size: u64, owner_pid: u64) {
    register_as(handle, phys_addr, size, owner_pid, true);
}

fn register_as(handle: u32, phys_addr: u64, size: u64, owner_pid: u64, device_memory: bool) {
    let mut table = MAPPINGS.lock();
    if let Some(e) = table.iter_mut().find(|e| e.handle == handle) {
        e.phys_addr = phys_addr;
        e.size = size;
        e.device_memory = device_memory;
    } else {
        table.push(MappedGem {
            handle,
            phys_addr,
            size,
            holders: alloc::vec![owner_pid],
            host_pa_given: false,
            device_memory,
        });
    }
}

/// Whether `handle`'s range is device memory (VRAM through BAR1), which a CPU
/// mapping must not cache. False for an unknown handle.
pub fn is_device_memory(handle: u32) -> bool {
    MAPPINGS
        .lock()
        .iter()
        .any(|e| e.handle == handle && e.device_memory)
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
    // Its NO_SHARE objects go with it: each is its creator's alone.
    NO_SHARE.lock().retain(|&(_, owner)| owner != pid);
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

/// Records `handle`, owned by `owner_pid`, as created with
/// `NOUVEAU_GEM_DOMAIN_NO_SHARE`: a PRIME export of it is refused (Linux
/// `nouveau_gem_prime_export`, EPERM). Whether or not the object has a host
/// physical address: the record was a flag on the mapping entry, so a
/// VRAM-only NO_SHARE object was never marked and its export said the handle
/// did not exist.
pub fn mark_no_share(handle: u32, owner_pid: u64) {
    let mut set = NO_SHARE.lock();
    if !set.iter().any(|&(h, _)| h == handle) {
        set.push((handle, owner_pid));
    }
}

/// Whether `handle` is a NO_SHARE object `pid` holds. The owner is the only
/// holder such an object can have, and pid 0 (no current thread) sees every
/// object, as [`holds`] does; another process's NO_SHARE object answers
/// `false`, so the export path tells it nothing a `drm_gem_object_lookup`
/// in its own file would not.
pub fn no_share(handle: u32, pid: u64) -> bool {
    NO_SHARE
        .lock()
        .iter()
        .any(|&(h, owner)| h == handle && (pid == 0 || owner == pid))
}

/// Drops `handle`'s NO_SHARE record: the object is gone (`GEM_CLOSE` by its
/// one holder). Returns whether there was one.
pub fn clear_no_share(handle: u32) -> bool {
    let mut set = NO_SHARE.lock();
    let before = set.len();
    set.retain(|&(h, _)| h != handle);
    set.len() != before
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
    // The NO_SHARE records are keyed by owner too, so the zombie's follow
    // its references and the teardown of the old pid takes none of the new
    // process's.
    for (_, owner) in NO_SHARE.lock().iter_mut() {
        if *owner == from {
            *owner = to;
            n += 1;
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

/// The handle whose range holds `phys_addr` anywhere inside it, not only at
/// its start (that is [`lookup_by_phys`]): what a present needs to tell a
/// row in the middle of a buffer from host RAM.
pub fn lookup_by_phys_within(phys_addr: u64) -> Option<u32> {
    MAPPINGS
        .lock()
        .iter()
        .find(|e| phys_addr >= e.phys_addr && phys_addr - e.phys_addr < e.size)
        .map(|e| e.handle)
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

    /// The last slice is the one whose end is that top of `u32`, which the
    /// field cannot hold: `base + HANDLES_PER_GPU` used to overflow there. It
    /// now ends at `u32::MAX`, so the 64th card has every id of its range but
    /// the last one. Over a private counter, because the real one belongs to
    /// the whole binary.
    #[test]
    fn the_last_slice_ends_at_the_top_of_u32_instead_of_overflowing() {
        let next = AtomicU32::new(HANDLE_SLICES - 1);
        let s = alloc_handle_slice_from(&next);
        assert_eq!(
            s.base(),
            DRIVER_HANDLE_BASE + (HANDLE_SLICES - 1) * HANDLES_PER_GPU
        );
        assert_eq!(s.end(), u32::MAX);
        assert!(s.contains(s.base()));
        assert!(s.contains(u32::MAX - 1));
        assert!(!s.contains(u32::MAX), "the one id saturating costs");
        // And the first slice over a fresh counter skips `0x8000_0000`.
        let first = alloc_handle_slice_from(&AtomicU32::new(0));
        assert_eq!(first.base(), DRIVER_HANDLE_BASE + 1);
        assert_eq!(first.end(), DRIVER_HANDLE_BASE + HANDLES_PER_GPU);
    }

    /// Past the last slice: the counter keeps counting, the answer is the
    /// empty slice, and dmesg says so, once per card that asked.
    #[test]
    fn the_sixty_fifth_card_gets_the_empty_slice_and_a_line_in_dmesg() {
        let next = AtomicU32::new(HANDLE_SLICES);
        crate::utils::host_hooks::take_dmesg();
        assert_eq!(alloc_handle_slice_from(&next), HandleSlice::exhausted());
        assert_eq!(next.load(Ordering::Relaxed), HANDLE_SLICES + 1);
        assert_eq!(alloc_handle_slice_from(&next), HandleSlice::exhausted());
        let dmesg = crate::utils::host_hooks::take_dmesg();
        let said: Vec<&str> = dmesg
            .iter()
            .map(|(_, l)| l.as_str())
            .filter(|l| l.contains("more than 64 GPUs asked for a GEM handle slice"))
            .collect();
        assert_eq!(said.len(), 2, "{:?}", dmesg);
        assert!(said[0].contains("GEM_NEW on this one will fail"));
    }
}

/// The bookkeeping calls with a trivial answer that nothing above asked:
/// the count of a handle, pid 0 as a process to release or rekey, and
/// unregistering what was never registered.
#[cfg(test)]
mod bookkeeping_edge_tests {
    use super::*;

    const OWNER: u64 = 77_001;
    const OTHER: u64 = 77_002;

    #[test]
    fn pid_zero_is_nobodys_process_to_release_or_rekey() {
        let _g = crate::test_globals::LOCK.lock();
        let handle = DRIVER_HANDLE_BASE + 0x20_0001;
        assert_eq!(
            ref_count(handle),
            None,
            "a handle nobody registered has no count"
        );
        register(handle, 0x5_0000_0000, 0x1000, OWNER);
        assert_eq!(ref_count(handle), Some(1));
        assert_eq!(add_ref(handle, OTHER), Some(2));
        // A reference the kernel itself holds (no current thread).
        assert_eq!(add_ref(handle, 0), Some(3));
        assert_eq!(ref_count(handle), Some(3));
        // Pid 0 is "no current thread", not a process: there is no exit that
        // drops what it holds and no zombie to rekey, and nobody else's
        // references go with it.
        assert!(release_pid(0).is_empty());
        assert_eq!(rekey_pid(0, OTHER), 0);
        assert_eq!(ref_count(handle), Some(3), "pid 0 took a holder with it");
        assert!(holds(handle, OWNER) && holds(handle, OTHER));
        assert!(unregister(handle));
        assert!(!unregister(handle), "unregistered twice");
        assert_eq!(ref_count(handle), None);
    }

    /// A VRAM object behind BAR1 is a range like any other to the lookups,
    /// and the one thing that differs -- it is device memory -- is said by
    /// the handle, for every address inside it.
    #[test]
    fn device_memory_is_known_by_handle_and_by_any_address_inside_it() {
        let _g = crate::test_globals::LOCK.lock();
        let vram = DRIVER_HANDLE_BASE + 0x20_0002;
        let host = DRIVER_HANDLE_BASE + 0x20_0003;
        register_device_memory(vram, 0xd0_0100_0000, 0x40_0000, OWNER);
        register(host, 0x5_0000_0000, 0x1000, OWNER);
        assert!(is_device_memory(vram));
        assert!(!is_device_memory(host), "host RAM is not device memory");
        assert!(!is_device_memory(DRIVER_HANDLE_BASE + 0x20_0004), "unknown");
        assert_eq!(lookup_for(vram, OWNER), Some((0xd0_0100_0000, 0x40_0000)));
        assert_eq!(
            lookup_by_phys_within(0xd0_0100_0000 + 0x3f_ffff),
            Some(vram)
        );
        assert_eq!(lookup_by_phys_within(0xd0_0100_0000 + 0x40_0000), None);
        assert_eq!(lookup_by_phys_within(0x5_0000_0800), Some(host));
        // Re-registering says again what the range is.
        register(vram, 0x6_0000_0000, 0x1000, OWNER);
        assert!(!is_device_memory(vram));
        assert!(unregister(vram) && unregister(host));
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
        let _g = crate::test_globals::LOCK.lock();
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
        let _g = crate::test_globals::LOCK.lock();
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
        let _g = crate::test_globals::LOCK.lock();
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
        let _g = crate::test_globals::LOCK.lock();
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
        let _g = crate::test_globals::LOCK.lock();
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
        let _g = crate::test_globals::LOCK.lock();
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
    /// with each other and with every test that drives it.
    use crate::test_globals::LOCK as SERIAL;

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

#[cfg(test)]
mod no_share_record_tests {
    use super::*;

    /// A NO_SHARE record lives apart from the mapping entry: a handle with
    /// no mapping (VRAM-only) keeps it, only its owner (or the kernel) sees
    /// it, a process exit drops the ones it owned, a rekey moves them, and
    /// unregistering the mapping does not touch it.
    #[test]
    fn a_no_share_record_outlives_the_mapping_and_belongs_to_its_owner() {
        let _g = crate::test_globals::LOCK.lock();
        const VRAM: u32 = 0xbffe_0001;
        const GART: u32 = 0xbffe_0002;
        // Pids of this test's own: 77_001 is the bench module's, whose rows
        // run as tests too and register 256 holders under it, and a rekey
        // caught in the middle of that moved them all (182 of 1).
        const OWNER: u64 = 78_101;
        const OTHER: u64 = 78_102;
        const ZOMBIE: u64 = 78_103;
        mark_no_share(VRAM, OWNER);
        mark_no_share(VRAM, OTHER);
        register(GART, 0x7000_0000, 4096, OWNER);
        mark_no_share(GART, OWNER);
        assert!(no_share(VRAM, OWNER), "no mapping needed");
        assert!(no_share(VRAM, 0), "the kernel sees every object");
        assert!(!no_share(VRAM, OTHER), "the first mark is the owner");
        assert!(no_share(GART, OWNER));
        assert!(unregister(GART));
        assert!(no_share(GART, OWNER), "the mapping is not the object");
        assert!(clear_no_share(GART));
        assert!(!clear_no_share(GART), "once");
        assert!(!no_share(GART, OWNER));
        assert_eq!(rekey_pid(OWNER, ZOMBIE), 1, "the record moved");
        assert!(no_share(VRAM, ZOMBIE) && !no_share(VRAM, OWNER));
        release_pid(ZOMBIE);
        assert!(!no_share(VRAM, 0), "gone with the process");
    }
}

#[cfg(test)]
mod host_pa_counter_tests {
    use super::*;

    /// Pid of the client doing the mapping. In the driver-private range, where
    /// [`holds`] is strict, so nothing here passes for the wrong reason.
    const CLIENT: u64 = 91_001;

    fn drop_handle(handle: u32) {
        while !matches!(dec_ref(handle, 0), DecRef::NotTracked | DecRef::Freed) {}
    }

    /// Every test here reads the running totals, does something, and reads
    /// them again, so two of them overlapping would see each other's bytes in
    /// their own delta. They take turns.
    fn alone_with_the_counters<R>(body: impl FnOnce() -> R) -> R {
        // The nouveau bookkeeping tests hand out host PAs too, under this
        // same lock, so a delta read here is not theirs.
        let _guard = crate::test_globals::LOCK.lock();
        body()
    }

    /// One object taken by several consumers (mmap'd, then exported over
    /// PRIME, then stamped as a framebuffer) must not count its bytes once
    /// per consumer: the number is "how many bytes something needs a host
    /// address for", and double counting would make a VRAM placement look
    /// less worthwhile than it is.
    #[test]
    fn several_consumers_of_one_object_count_it_once() {
        alone_with_the_counters(|| {
            let handle = DRIVER_HANDLE_BASE + 0x20_0001;
            let size = 0x10_0000;
            register(handle, 0x5_0000_0000, size, CLIENT);

            let (n0, b0) = host_pa_totals();
            note_host_pa_handed_out(handle);
            note_host_pa_handed_out(handle);
            note_host_pa_handed_out(handle);
            let (n1, b1) = host_pa_totals();

            assert_eq!(n1 - n0, 1, "three consumers of one object are one object");
            assert_eq!(b1 - b0, size, "and its bytes are counted once");

            drop_handle(handle);
        });
    }

    /// The totals are "since boot", so freeing the object must not take its
    /// bytes back out: a frame that was used and freed is still a frame that
    /// had to live where the CPU could reach it.
    #[test]
    fn the_totals_survive_the_object_being_freed() {
        alone_with_the_counters(|| {
            let handle = DRIVER_HANDLE_BASE + 0x20_0002;
            let size = 0x4_0000;
            register(handle, 0x5_0010_0000, size, CLIENT);
            note_host_pa_handed_out(handle);
            let (n0, b0) = host_pa_totals();

            assert!(unregister(handle), "the entry was there to remove");
            let (n1, b1) = host_pa_totals();

            assert_eq!((n1, b1), (n0, b0), "freeing it does not un-count it");
        });
    }

    /// A handle this table never saw must not be counted: it has no size
    /// here, so counting it would add an object worth zero bytes and make the
    /// average object look smaller than it is.
    #[test]
    fn an_untracked_handle_is_not_counted() {
        alone_with_the_counters(|| {
            let (n0, b0) = host_pa_totals();
            note_host_pa_handed_out(DRIVER_HANDLE_BASE + 0x2f_f001);
            assert_eq!(host_pa_totals(), (n0, b0));
        });
    }

    /// Two distinct objects add up, bytes included -- the obvious case, and
    /// the one that pins that the once-only rule above is per object and not
    /// per table.
    #[test]
    fn two_objects_add_up() {
        alone_with_the_counters(|| {
            let a = DRIVER_HANDLE_BASE + 0x20_0003;
            let b = DRIVER_HANDLE_BASE + 0x20_0004;
            register(a, 0x5_0020_0000, 0x1_0000, CLIENT);
            register(b, 0x5_0030_0000, 0x2_0000, CLIENT);

            let (n0, b0) = host_pa_totals();
            note_host_pa_handed_out(a);
            note_host_pa_handed_out(b);
            let (n1, b1) = host_pa_totals();

            assert_eq!(n1 - n0, 2);
            assert_eq!(b1 - b0, 0x3_0000);

            drop_handle(a);
            drop_handle(b);
        });
    }
}
