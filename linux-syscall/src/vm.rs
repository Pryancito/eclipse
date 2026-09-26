use super::*;
use alloc::vec::Vec;
use bitflags::bitflags;
use linux_object::error::LxResult;
use linux_object::loader::heap_base;
use zircon_object::vm::{
    pages, round_down_pages, roundup_pages, MMUFlags, VmAddressRegion, VmObject, PAGE_SIZE,
    USER_ASPACE_BASE, USER_ASPACE_SIZE,
};

/// Per-call cap for a single `mmap` / `brk` growth. It bounds how much a single
/// syscall can commit at once (physical frames + per-page VMO metadata).
///
/// This must be larger than the biggest single-call reservation we expect from
/// userspace. Two very different consumers push this up:
///   * the dynamic linker maps a library's whole LOAD span in one call, and
///     `libLLVM.so` (pulled in by `perf`) is ~150 MiB — the original 128 MiB cap
///     rejected it outright with ENOMEM (surfaced by musl as "Out of memory")
///     despite gigabytes being free;
///   * SpiderMonkey (Firefox's JS engine) reserves its whole JIT executable
///     region up front as a single `PROT_NONE` anonymous `mmap`. On x86-64 that
///     reservation is ~1.1 GiB (`MaxCodeBytesPerProcess`), so a 1 GiB cap
///     bounced it — and, crucially, the early-return did so *silently*, which is
///     why `js::jit::InitProcessExecutableMemory() failed` fired with no mmap
///     error in the log.
///
///     2 GiB was not enough either: Firefox 136's SpiderMonkey asks for a
///     single 16 GiB `PROT_NONE` reservation (its WASM huge-memory region) and
///     had it bounced. The user address space is `1 << 47`, so 16 GiB is a
///     rounding error in it; 64 GiB clears that request with room and still
///     bounds a runaway caller to a small fraction of the space per call.
///
/// Because anonymous mappings are now demand-paged (see `sys_mmap`), a large
/// reservation costs only address space plus a sparse per-touched-page frame
/// entry, not committed RAM — so the cap bounds address-space requests, not
/// physical footprint.
const MAX_MMAP_LEN: usize = 64 * 1024 * 1024 * 1024;

/// Linux `vm.mmap_min_addr` (default 64 KiB): floor for kernel-chosen mmap
/// placement. Without it the VMAR's first-fit search can hand a non-FIXED
/// `mmap(NULL, ...)` the address 0 — userspace then treats 0 as a valid
/// pointer and every syscall null-check bounces it with EFAULT (observed:
/// `dd bs=4096` failing "Bad address" because glibc placed its I/O buffer
/// there). Also restores NULL-dereference protection for user processes.
/// MAP_FIXED requests are exempt, matching Linux for privileged tooling.
const MMAP_MIN_ADDR: usize = 0x1_0000;

/// Syscalls for virtual memory.
///
/// # Menu
///
/// - [`mmap`](Self::sys_mmap)
/// - [`mprotect`](Self::sys_mprotect)
/// - [`munmap`](Self::sys_munmap)
///
/// Assert, at the point `mmap` is about to hand `addr` to userspace, that the
/// range it promises is actually installed in the VMAR.
///
/// Userspace is faulting with `[vmar-near] NO mapping within 0x20000 bytes --
/// the address was never mapped` at PAGE-ALIGNED addresses, inside musl
/// mallocng's `m->mem->meta = m` — the allocator writing the FIRST word of a
/// group it just got back from mmap. So either mmap returned a range it never
/// installed, or the range was installed and something removed it afterwards.
/// Those two need completely different fixes, and this check separates them:
/// it fires HERE only in the first case, and stays silent in the second (the
/// crash then proves the range was destroyed later).
///
/// Only the first and last page are probed, so the cost is two lookups, not one
/// per page — and the first page is exactly the word mallocng writes.
fn verify_installed(
    vmar: &alloc::sync::Arc<zircon_object::vm::VmAddressRegion>,
    addr: usize,
    len: usize,
    kind: &str,
) {
    let last = addr + len - PAGE_SIZE;
    let first_ok = vmar.find_mapping(addr).is_some();
    let last_ok = vmar.find_mapping(last).is_some();
    if !first_ok || !last_ok {
        kernel_hal::klog_info!(
            "[mmap-lost] {} mmap returned {:#x} len={:#x} but the range is NOT installed \
             (first_page={}, last_page={})",
            kind,
            addr,
            len,
            if first_ok { "ok" } else { "MISSING" },
            if last_ok { "ok" } else { "MISSING" },
        );
    }
}

impl Syscall<'_> {
    /// Map files or devices into memory
    /// (see [linux man mmap(2)](https://www.man7.org/linux/man-pages/man2/mmap.2.html)).
    ///
    /// `sys_mmap` creates a new mapping in the virtual address space of the calling process.
    ///
    /// The starting address for the new mapping is specified in `addr`.
    ///
    /// The `len` argument specifies the length of the mapping (which must be greater than 0).
    ///
    /// Arguments `fd` and `offset` specifies mapping file descriptor and offset in the file.
    ///
    /// The `prot` argument describes the desired memory protection of the mapping
    /// (and must not conflict with the open mode of the file).
    /// It is either 0 or the bitwise OR of one or more of the following flags:
    ///
    /// - **`MmapProt::READ`**
    ///
    ///   Pages may be read
    ///
    /// - **`MmapProt::WRITE`**
    ///
    ///   Pages may be written
    ///
    /// - **`MmapProt::EXEC`**
    ///
    ///   Pages may be executed
    ///
    /// The `flags` argument determines whether updates to the mapping are visible to other processes mapping the same region,
    /// and whether updates are carried through to the underlying file.
    /// This behavior is determined by including exactly one of the following values:
    ///
    /// - **`MmapFlags::SHARED`**
    ///
    ///   Share this mapping. Updates to the mapping are visible to other processes mapping the same region,
    ///   and (in the case of file-backed mappings) are carried through to the underlying file.
    ///   (To precisely control when updates are carried through to the underlying file requires the use of `msync`,
    ///   which has not been implemented in zcore).
    ///
    /// - **`MmapFlags::PRIVATE`**
    ///
    ///   Create a private copy-on-write mapping.
    ///   Updates to the mapping are not visible to other processes mapping the same file,
    ///   and are not carried through to the underlying file.
    ///   It is unspecified whether changes made to the file after the `sys_mmap` call are visible in the mapped region.
    ///
    /// - **`MmapFlags::FIXED`**
    ///
    ///   Don't interpret `addr` as a hint: place the mapping at exactly that address.
    ///   `addr` must be suitably aligned:
    ///   for most architectures a multiple of the page size is sufficient;
    ///   however, some architectures may impose additional restrictions.
    ///   If the memory region specified by `addr` and `len` overlaps pages of any existing mapping(s),
    ///   then the overlapped part of the existing mapping(s) will be discarded.
    ///   If the specified address cannot be used, `sys_mmap` will fail.
    ///
    /// - **`MmapFlags::ANONYMOUS`**
    ///
    ///   The mapping is not backed by any file; its contents are initialized to zero.
    ///   Both `fd` and `offset` arguments are ignored.
    ///   The use of `MmapFlags::ANONYMOUS` in conjunction with `MmapFlags::SHARED`
    ///   causes an [`EINVAL`](LxError::EINVAL) to be returned.
    pub async fn sys_mmap(
        &self,
        addr: usize,
        len: usize,
        prot: usize,
        flags: usize,
        fd: FileDesc,
        offset: u64,
    ) -> SysResult {
        let prot = MmapProt::from_bits_truncate(prot);
        let shared = mmap_shared(flags, flags & MMAP_ANONYMOUS != 0)?;
        shared_validate_flags(flags)?;
        let flags = MmapFlags::from_bits_truncate(flags);
        info!(
            "mmap: addr={:#x}, size={:#x}, prot={:?}, flags={:?}, fd={:?}, offset={:#x}",
            addr, len, prot, flags, fd, offset
        );
        if let Err(e) = mmap_len_check(len) {
            // Log a refused length, zero (EINVAL) or over the cap (ENOMEM):
            // the early-return is the one mmap failure path with no other
            // trace, and a silently-rejected giant reservation (e.g. a JIT
            // engine's executable pool) otherwise looks like a spontaneous
            // userspace crash with nothing in the kernel log.
            //
            // `error!`, not `warn!`, and the same for every other failure
            // reported in this file: the shipped command line is
            // `LOG=error` (zCore/rboot.conf), so a `warn!` here is not a
            // quieter message -- it is no message. A whole round of chasing
            // Firefox's startup crash went past a possible allocation failure
            // for exactly that reason, with the kernel's own best diagnostic
            // written and switched off. A syscall that fails and takes a
            // process down is not a warning.
            if len == 0 {
                error!(
                    "mmap: rejecting a zero length with {:?} prot={:?} flags={:?} fd={:?}",
                    e, prot, flags, fd
                );
            } else {
                error!(
                    "mmap: rejecting len={:#x} over the cap {:#x} with {:?} prot={:?} flags={:?} fd={:?}",
                    len, MAX_MMAP_LEN, e, prot, flags, fd
                );
            }
            return Err(e);
        }
        // Linux UAPI: `len` is rounded UP to whole pages by the kernel for
        // mmap/munmap/mprotect; only `addr` must be page-aligned (and only
        // for MAP_FIXED). glibc's ld.so depends on this — its zero-fill
        // mmap passes the raw unaligned segment length, and our former
        // aligned-length requirement bounced it with EINVAL, killing every
        // glibc binary at load with "cannot map zero-fill pages" (musl
        // rounds in userspace, which hid the gap for years).
        let placement = mmap_placement(flags);
        if placement != Placement::Hint && !addr.is_multiple_of(PAGE_SIZE) {
            return Err(LxError::EINVAL);
        }
        let len = roundup_pages(len);
        // `offset` is the sixth raw machine word from userspace, and nothing
        // below treats it as anything but a trustworthy byte offset -- see
        // `validate_mmap_offset` for what that cost.
        let offset = validate_mmap_offset(offset, len, flags.contains(MmapFlags::ANONYMOUS))?;
        // hunter W^X: reject (or audit) simultaneously writable+executable maps.
        if !hunter::check_mmap(
            self.zircon_process().id(),
            prot.contains(MmapProt::WRITE),
            prot.contains(MmapProt::EXEC),
        ) {
            return Err(LxError::EACCES);
        }

        let proc = self.zircon_process();
        let pid = proc.id();
        let vmar = proc.vmar();
        let want_write = prot.contains(MmapProt::WRITE);

        // mmap_lock (see `LinuxProcess::aspace_lock`): every layout mutation
        // below runs under it, so a concurrent `fork` can never clone a
        // half-updated mapping list. Taken before any `inner` access (fd
        // lookups in the file-backed arm) — that is the global lock order.
        let _aspace = self.linux_process().aspace_lock().lock();
        // `MAP_FIXED_NOREPLACE` is placed like `MAP_FIXED` and replaces
        // nothing. The range is read under `aspace_lock`, which every layout
        // mutation holds (see above), so nothing can take the hole between
        // this answer and the mapping below.
        //
        // Only a range that IS inside this address space can be "already
        // taken": one that falls outside it is not EEXIST but a placement
        // failure, and `map_ext_min` reports that one a few lines down. Linux
        // orders it the same way -- `get_unmapped_area` runs first and the
        // EEXIST test after it.
        let in_vmar = addr >= vmar.addr()
            && addr
                .checked_add(len)
                .is_some_and(|end| end <= vmar.end_addr());
        if placement == Placement::FixedNoReplace && in_vmar && !vmar.range_is_free(addr, len) {
            return Err(LxError::EEXIST);
        }
        let fixed = placement != Placement::Hint;
        let overwrite = placement == Placement::Fixed;
        if overwrite {
            // hunter: the range is about to be replaced, so drop its W^X
            // writable-history. (The unmap itself now happens INSIDE
            // map_ext_min -- see `overwrite` below.)
            hunter::check_munmap(pid, addr, len);
        }
        // MAP_FIXED must be ATOMIC. This used to call vmar.unmap() here, before
        // trying to place the new mapping: the range was destroyed first, and
        // if the mapping then failed for any reason the process was simply left
        // without the memory it had -- mmap returned an error, userspace kept
        // its own view of the old allocation, and the next touch took a SIGSEGV
        // on an address with no mapping at all. Caught live: xfce4-session
        // faulting at 0x4db2000 with
        //   [vmar-unmapped-by] removed by mmap_fixed_preunmap of [0x4db2000, 0x4f7c000)
        //   [vmar-near] NO mapping within 0x20000 bytes -- never mapped
        // i.e. the pre-unmap ran and nothing replaced it. Linux never does
        // this: on MAP_FIXED failure the old mapping stays untouched.
        //
        // `map_ext_min`'s `overwrite` does the same removal under the SAME VMAR
        // lock, immediately before inserting the replacement, so the range is
        // never left destroyed-and-empty.
        let vmar_offset = fixed.then(|| addr - vmar.addr());
        if flags.contains(MmapFlags::ANONYMOUS) {
            let vmo = VmObject::new_paged(pages(len));
            // MAP_SHARED | MAP_ANONYMOUS: the region must be one object shared
            // with every future child, not a per-process copy. Without the
            // marker, fork privatized it and preforked pools (nginx, postgres,
            // any parent-child counter page) silently stopped sharing.
            if shared {
                vmo.set_share_on_fork();
            }
            // Demand-page anonymous memory (`map_range = false`) instead of
            // committing a zero frame for every page up front. Linux mmap does
            // not commit anonymous pages until first touch, and some programs
            // rely on that: SpiderMonkey reserves ~1.1 GiB of `PROT_NONE` JIT
            // address space in a single call and only ever touches the slices
            // it turns into code — eagerly committing the whole reservation
            // would burn >1 GiB of RAM (and, before the cap was raised, OOM).
            // The permission ceiling is RXW so a later `mprotect` can raise a
            // slice to executable (JIT), matching the file-mapping path. Both
            // user- and kernel-mode faults on these pages resolve through
            // `Vmar::handle_page_fault`, so a lazily-mapped buffer handed to a
            // syscall (e.g. `read`) still faults in correctly on the kernel
            // store.
            let addr = vmar
                .map_ext_min(
                    vmar_offset,
                    vmo.clone(),
                    0,
                    vmo.len(),
                    MMUFlags::RXW | MMUFlags::USER,
                    prot.to_flags(),
                    overwrite,
                    false,
                    false,
                    MMAP_MIN_ADDR,
                )
                .inspect_err(|e| {
                    error!(
                        "mmap(anon) FAILED: {:?} addr={:#x} len={:#x} prot={:?} flags={:?}",
                        e, addr, len, prot, flags
                    );
                })?;
            // hunter P3: remember a writable mapping so a later mprotect(EXEC)
            // over it is recognised as the two-step W^X bypass.
            hunter::record_mapping(pid, addr, len, want_write);
            verify_installed(&vmar, addr, len, "anon");
            Ok(addr)
        } else {
            let file_like = self.linux_process().get_file_like(fd)?;
            mmap_file_access(shared, want_write, file_like.flags())?;
            // MAP_SHARED must hand every mapper of the file the SAME VmObject
            // (stores propagate between processes — the wl_shm pixel path);
            // MAP_PRIVATE keeps the per-call demand-paged snapshot.
            let (vmo, vmo_offset) = if shared {
                let (vmo, off) = file_like
                    .get_vmo_shared(offset, len)
                    .inspect_err(|e| {
                        // Name the failing fd. `File` carries a path (the
                        // device/file node); anything else has none, which
                        // itself pins the kind.
                        let path = file_like
                            .downcast_ref::<linux_object::fs::File>()
                            .map(|f| f.path().clone())
                            .unwrap_or_else(|| alloc::string::String::from("<non-File FileLike>"));
                        // alsa-lib PROBES the mmap of a PCM's status (0x8000_0000)
                        // and control (0x8100_0000) pages, and on any failure
                        // sets `mmap_*_fallbacked` and drives the device through
                        // SNDRV_PCM_IOCTL_SYNC_PTR instead — which this kernel
                        // implements and PulseAudio plays through fine. So that
                        // ENOSYS is an EXPECTED, handled fallback, not a failure:
                        // log it at info so it stops reading as a compositor
                        // crash. Every other shared-mmap ENOSYS is still an error.
                        const PCM_MMAP_STATUS: usize = 0x8000_0000;
                        const PCM_MMAP_CONTROL: usize = 0x8100_0000;
                        let alsa_syncptr_probe = path.contains("/dev/snd/pcm")
                            && (offset == PCM_MMAP_STATUS || offset == PCM_MMAP_CONTROL);
                        if alsa_syncptr_probe {
                            info!(
                                "mmap(pcm status/control) unsupported (proc={} fd={:?} path={} offset={:#x}) — alsa-lib falls back to SYNC_PTR, expected",
                                self.zircon_process().name(),
                                fd,
                                path,
                                offset
                            );
                        } else {
                            error!(
                                "mmap(file,shared) get_vmo_shared FAILED: {:?} proc={} fd={:?} path={} offset={:#x} len={:#x}",
                                e,
                                self.zircon_process().name(),
                                fd,
                                path,
                                offset,
                                len
                            );
                        }
                    })?;
                // Same rule as anonymous MAP_SHARED: fork must share, not copy.
                vmo.set_share_on_fork();
                (vmo, off)
            } else {
                let vmo = file_like.get_vmo(offset, len).inspect_err(|e| {
                    error!(
                        "mmap(file) get_vmo FAILED: {:?} fd={:?} offset={:#x} len={:#x}",
                        e, fd, offset, len
                    );
                })?;
                (vmo, 0)
            };
            // Permission ceiling = full RXW: Linux lets mprotect raise a file
            // mapping to any R/W/X combination, and the dynamic linker relies on
            // it — ld.so mprotect()s a library's *executable* text segment to RW
            // to apply text relocations / DT_TEXTREL and to set up GNU_RELRO
            // (seen with libxul). A W^X-preserving ceiling that strips WRITE from
            // exec maps made that mprotect fail; the syscall layer then swallowed
            // the error, leaving GOT entries unrelocated (== 0) and Firefox
            // crashing on a store through a NULL pointer. W^X is still audited by
            // hunter::check_mprotect (Report mode by default) — it just isn't
            // enforced by capping the mapping here.
            let ceiling = MMUFlags::RXW | MMUFlags::USER;
            // Map without committing the range up front (`map_range = false`):
            // the VMO returned by `get_vmo` is demand-paged from the file, so its
            // pages are read in on the faults that first touch them. Eagerly
            // mapping the whole range here would defeat that and re-introduce the
            // full-file read that froze the machine on `perf` (libLLVM ~150 MiB).
            // For a shared full-file VMO the requested window starts at
            // `vmo_offset` inside it; snapshots bake the offset in and use 0.
            let map_len = file_map_len(len, vmo.len(), vmo_offset);
            let addr = if map_len < len {
                // The file cannot back the whole request (mmap past EOF, or a
                // PT_LOAD segment whose memsz exceeds its filesz). Linux STILL
                // maps the full `len`: the bytes past the file read as zero and
                // the region exists for its entire length. Mapping only
                // `map_len` -- which is what this did -- handed userspace a
                // region that ENDS EARLY. Nothing reports an error: mmap
                // returns success and the caller's own length, so the allocator
                // or loader happily uses the tail, and the first touch past the
                // truncation point takes a SIGSEGV with NOT_FOUND (no VmMapping
                // covers it). Observed as musl mallocng writing through the end
                // of a group, and every XFCE component dying that way.
                //
                // Reserve the full range with an anonymous VMO first, then
                // overwrite its head with the file window, so the tail is
                // demand-zero exactly like Linux.
                let anon = VmObject::new_paged(pages(len));
                let base = vmar
                    .map_ext_min(
                        vmar_offset,
                        anon,
                        0,
                        len,
                        ceiling,
                        prot.to_flags(),
                        overwrite,
                        false,
                        false,
                        MMAP_MIN_ADDR,
                    )
                    .inspect_err(|e| {
                        error!(
                            "mmap(file) reserve FAILED: {:?} len={:#x} map_len={:#x}",
                            e, len, map_len
                        );
                    })?;
                vmar.map_ext_min(
                    Some(base - vmar.addr()),
                    vmo.clone(),
                    vmo_offset,
                    map_len,
                    ceiling,
                    prot.to_flags(),
                    true,
                    false,
                    false,
                    MMAP_MIN_ADDR,
                )
                .inspect_err(|e| {
                    error!(
                        "mmap(file) overlay FAILED: {:?} base={:#x} map_len={:#x} vmo_len={:#x}",
                        e,
                        base,
                        map_len,
                        vmo.len()
                    );
                })?;
                base
            } else {
                vmar.map_ext_min(
                    vmar_offset,
                    vmo.clone(),
                    vmo_offset,
                    map_len,
                    ceiling,
                    prot.to_flags(),
                    overwrite,
                    false,
                    false,
                    MMAP_MIN_ADDR,
                )
                .inspect_err(|e| {
                    error!(
                        "mmap(file) map_ext FAILED: {:?} addr={:#x} len={:#x} vmo_len={:#x} prot={:?} flags={:?} offset={:#x}",
                        e, addr, len, vmo.len(), prot, flags, offset
                    );
                })?
            };
            hunter::record_mapping(pid, addr, len, want_write);
            verify_installed(&vmar, addr, len, "file");
            Ok(addr)
        }
    }

    /// Change the location of the program break
    /// (see [linux man brk(2)](https://www.man7.org/linux/man-pages/man2/brk.2.html)).
    ///
    /// `sys_brk` sets the end of the process data segment (the program break) to `new_brk`.
    /// If `new_brk` is 0, the current break is returned unchanged (query mode).
    /// If `new_brk` is below the current break, the heap is shrunk and the freed pages are
    /// unmapped.  If `new_brk` is above the current break, new anonymous pages are mapped and
    /// the new break value is returned.
    /// On failure the current break is returned (Linux semantics: never returns -1).
    ///
    /// The initial program break is set during ELF loading (end of all loaded segments) and
    /// stored on the process via `LinuxProcess::set_brk`.  A value of 0 means not yet
    /// initialized (e.g. the process called `brk` before any ELF was loaded).
    pub fn sys_brk(&self, new_brk: usize) -> SysResult {
        // mmap_lock: brk grows/shrinks the heap mapping — a layout mutation.
        let _aspace = self.linux_process().aspace_lock().lock();
        // Reserve heap pages in 1 MiB chunks instead of issuing one VMO per
        // user-visible call. Glibc malloc typically calls `brk` in 4–32 KiB
        // increments, which the old code translated into a fresh VMO + VMAR
        // mapping each time — hundreds of VMAR entries for a single arena
        // and a corresponding bump in allocator / TLB-shootdown pressure.
        const BRK_CHUNK: usize = 1 << 20;

        let proc = self.linux_process();
        let current_brk = proc.brk();
        // Lazy init: the loader populates `brk` directly before the first
        // sys_brk, so the mapped end matches it.
        let mapped_brk = {
            let m = proc.mapped_brk();
            if m == 0 {
                roundup_pages(current_brk)
            } else {
                m
            }
        };
        info!(
            "brk: new_brk={:#x}, current_brk={:#x}, mapped_brk={:#x}",
            new_brk, current_brk, mapped_brk
        );

        // brk(0) → return current break unchanged (query).
        if new_brk == 0 {
            return Ok(current_brk);
        }

        let vmar = self.zircon_process().vmar();

        // Below where the heap starts is not a shrink, it is a bad argument,
        // and Linux answers it by returning the old break (`if (brk <
        // mm->start_brk) goto out;`). Without this the shrink branch below
        // accepts it and moves the break into the middle of the loaded image,
        // so the next grow tries to map over the program's own text: the
        // `/oscomp/brk` case does exactly that, because it prints and
        // round-trips the break through a 32-bit int and the heap base is a
        // round power of two whose low 32 bits are zero.
        let Some(plan) = brk_plan(new_brk, current_brk, heap_base(&vmar)) else {
            info!(
                "brk: {:#x} is not a break this address space can take, keeping {:#x}",
                new_brk, current_brk
            );
            return Ok(current_brk);
        };
        // The break the caller asked for is what is stored and returned
        // (`mm->brk = brk; return brk;`); only the mapping is page-aligned.
        // Storing the aligned one made `sbrk(100)` move the break by 4096.
        let new_brk_aligned = plan.mapped_end;

        match plan.moved {
            BrkMove::Shrink => {
                // Shrink: just move the user-visible break. The reserved
                // pages stay mapped until they are reused on the next grow.
                // Linux glibc essentially never shrinks brk, and skipping the
                // unmap avoids the VMAR churn + TLB shootdown for transient
                // shrink/grow patterns.
                proc.set_brk(plan.brk);
                info!("brk: shrunk to {:#x} (mapping kept)", plan.brk);
                return Ok(plan.brk);
            }
            BrkMove::WithinPage => {
                // Same last page as before: bookkeeping only.
                proc.set_brk(plan.brk);
                return Ok(plan.brk);
            }
            BrkMove::Grow => {}
        }
        // Inside the already-reserved heap region — bookkeeping only.
        if new_brk_aligned <= mapped_brk {
            proc.set_brk(plan.brk);
            info!(
                "brk: extended to {:#x} (within reserved mapping up to {:#x})",
                plan.brk, mapped_brk
            );
            return Ok(plan.brk);
        }
        // Extend the mapping. Round the request up to BRK_CHUNK so a
        // burst of small grows is satisfied by a single map_at.
        let want = new_brk_aligned - mapped_brk;
        let size = want
            .checked_next_multiple_of(BRK_CHUNK)
            .unwrap_or(want)
            .max(BRK_CHUNK);
        if size > MAX_MMAP_LEN {
            return Ok(current_brk);
        }
        let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
        // Reserving ahead is only an optimization, so it must never cost a
        // grow that would otherwise have fit. The rounded-up chunk can run
        // into whatever the loader placed after the heap -- glibc's first
        // brk on Firefox asked for a few KiB, got rounded to 1 MiB, and hit
        // the next mapping -- so on failure fall back to the exact amount
        // asked for. Linux reserves nothing ahead and never has this
        // problem; this keeps the batching and its failure mode both.
        let mut last_err = None;
        for size in [size, roundup_pages(want)] {
            let vmo = VmObject::new_paged(pages(size));
            // `map_at` takes an offset into the VMAR, not an address:
            // `addr()` is 0 for a bare-metal user address space but not for
            // the window a libos process gets, where passing the absolute
            // break made every growth INVALID_ARGS. Same conversion
            // `sys_mmap` does for MAP_FIXED.
            match vmar.map_at(mapped_brk - vmar.addr(), vmo, 0, size, flags) {
                Ok(_) => {
                    let new_mapped_brk = mapped_brk + size;
                    proc.set_brk(plan.brk);
                    proc.set_mapped_brk(new_mapped_brk);
                    info!(
                        "brk: extended to {:#x}, mapping reserved up to {:#x}",
                        plan.brk, new_mapped_brk
                    );
                    return Ok(plan.brk);
                }
                Err(e) => last_err = Some((size, e)),
            }
        }
        if let Some((size, e)) = last_err {
            error!(
                "brk: failed to map {:#x} bytes at {:#x}: {:?}",
                size, mapped_brk, e
            );
        }
        // Return current break on failure (Linux semantics).
        Ok(current_brk)
    }

    /// Set protection on a region of memory
    /// (see [linux man mprotect(2)](https://www.man7.org/linux/man-pages/man2/mprotect.2.html)).
    ///
    /// `sys_mprotect` changes the access protections for the calling process's memory pages
    /// containing any part of the address range in the interval `[addr, addr+len-1]`.
    /// `addr` must be aligned to a page boundary.
    ///
    /// If the calling process tries to access memory in a manner that violates the protections,
    /// then the kernel generates a SIGSEGV signal for the process.
    ///
    /// `prot` is a combination of the following access flags:
    /// 0 or a bitwise-or of the other values in the following list:
    ///
    /// - **`MmapProt::READ`**
    ///
    ///   The memory can be read.
    ///
    /// - **`MmapProt::WRITE`**
    ///
    ///   The memory can be modified.
    ///
    /// - **`MmapProt::EXEC`**
    ///
    ///   The memory can be executed.
    ///
    /// If `prot` is 0, the memory cannot be accessed at all.
    pub fn sys_mprotect(&self, addr: usize, len: usize, prot: usize) -> SysResult {
        // mmap_lock: mprotect SPLITS mappings (`cut`) — the exact mutation
        // that, racing a fork's copy loop, gave the child an address space
        // that never existed (llvmpipe's W^X churn vs the Xwayland fork).
        let _aspace = self.linux_process().aspace_lock().lock();
        let prot = mprotect_prot(prot)?;
        info!(
            "mprotect: addr={:#x}, size={:#x}, prot={:?}",
            addr, len, prot
        );
        // Linux UAPI: addr must be page-aligned; len is rounded up to pages.
        let Some(len) = mprotect_args(addr, len)? else {
            // `if (!len) return 0;` (mm/mprotect.c), before Linux has looked at
            // a single VMA. Falling through instead reached
            // `VmAddressRegion::protect`, which refuses a zero length, so an
            // empty `mprotect` logged an INCOMPLETE-transition error -- and
            // under W^X enforcement answered `EINVAL`.
            return Ok(0);
        };
        // hunter W^X: reject (or audit) transitions to writable+executable,
        // including the two-step mmap(W)-then-mprotect(X) bypass (it tracks the
        // ever-writable history of this exact range).
        let pid = self.zircon_process().id();
        if !hunter::check_mprotect(
            pid,
            addr,
            len,
            prot.contains(MmapProt::WRITE),
            prot.contains(MmapProt::EXEC),
        ) {
            return Err(LxError::EACCES);
        }
        let proc = self.zircon_process();
        let vmar = proc.vmar();
        let flags = prot.to_flags();
        // Attempt the real permission change. Normally a range that overlaps
        // sub-regions (Zircon's protect() restriction) is treated as a benign
        // no-op. But under W^X *enforcement* a failed *narrowing* that leaves
        // pages more permissive than requested would be a silent bypass, so we
        // surface the error instead of swallowing it.
        match vmar.protect(addr, len, flags) {
            Ok(()) => Ok(0),
            Err(e) => {
                if hunter::policy::wx_mode() == hunter::Mode::Enforce {
                    error!(
                        "mprotect: addr={:#x} len={:#x} flags={:?} → {:?} (rejected under W^X enforce)",
                        addr, len, flags, e
                    );
                    return Err(LxError::EINVAL);
                }
                // `error!`, not `warn!`: the default cmdline is `LOG=error`
                // (zCore/rboot.conf), so this was invisible on exactly the
                // configuration people run. And it is not a cosmetic failure --
                // swallowing it returns 0 to a caller whose permissions are not
                // what it asked for.
                //
                // Not necessarily unchanged, either: `VmAddressRegion::protect`
                // validates coverage and flags up front, but then applies to
                // overlapping children in a loop with `?`, so a child failing
                // mid-way leaves the ones before it already changed. A failure
                // therefore means the transition is INCOMPLETE -- part of the
                // range may carry the new permissions and part the old -- which
                // is harder to reason about than a clean no-op, not easier.
                //
                // SpiderMonkey's JIT does mmap(PROT_NONE) -> mprotect(RW)
                // -> write code -> mprotect(RX) -> call it; told the last step
                // succeeded, it jumps into a page that is still not executable
                // and takes a user instruction-fetch fault (`err=0x14`, with
                // `rip == rax` from the indirect call). Firefox does that at a
                // fixed address on every boot.
                //
                // The error kind is what picks the cause apart: NOT_FOUND means
                // the range is not fully covered by mappings/children (a hole,
                // or a split this kernel made that Linux would not have),
                // ACCESS_DENIED means a mapping's max permissions forbid the
                // transition. Print it rather than infer it.
                error!(
                    "mprotect: addr={:#x} len={:#x} flags={:?} → {:?} — returning success on an \
                     INCOMPLETE transition; part of the range may still hold the old \
                     permissions, and a caller that now executes or writes it will fault",
                    addr, len, flags, e
                );
                Ok(0)
            }
        }
    }

    /// Unmap files or devices into memory
    /// (see [linux man munmap(2)](https://www.man7.org/linux/man-pages/man2/munmap.2.html)).
    ///
    /// Deletes the mappings for the specified address range, and causes further references to addresses
    /// within the range to generate invalid memory references.
    ///
    /// The `sys_munmap` system call deletes the mappings for the specified address range,
    /// and causes further references to addresses within the range to generate invalid memory references.
    /// The region is also automatically unmapped when the process is terminated.
    /// On the other hand, closing the file descriptor does not unmap the region.
    ///
    /// Both `addr` and `len` must be aligned to the page size, additionally, `len` must greater than 0.
    /// Otherwise, an [`EINVAL`](LxError::EINVAL) is returned.
    pub fn sys_munmap(&self, addr: usize, len: usize) -> SysResult {
        // mmap_lock: removal is a layout mutation (see sys_mmap).
        let _aspace = self.linux_process().aspace_lock().lock();
        info!("munmap: addr={:#x}, size={:#x}", addr, len);
        // Linux UAPI: addr must be page-aligned; len is rounded up to pages.
        let len = munmap_args(addr, len)?;
        let proc = self.thread.proc();
        // hunter P3: the range is gone, so drop its W^X writable-history.
        hunter::check_munmap(proc.id(), addr, len);
        let vmar = proc.vmar();
        // Tagged so the fault dump can tell a real userspace munmap apart
        // from MAP_FIXED's pre-unmap. Two giant unmaps were seen destroying
        // live memory: [0x23cb000,0x4f7e000) (~44 MiB) and
        // [0x23e3000,0x3c3e000) (~24 MiB), the second starting exactly at the
        // address mallocng had just been handed by mmap.
        vmar.unmap_why(addr, len, "sys_munmap")?;
        Ok(0)
    }

    /// Remap a virtual memory address
    /// (see [linux man mremap(2)](https://www.man7.org/linux/man-pages/man2/mremap.2.html)).
    ///
    /// Expands or shrinks the mapping containing `[old_addr, old_addr+old_len)`,
    /// moving it when the space after it is taken (only if `MREMAP_MAYMOVE` is
    /// set, else `ENOMEM`) or to the exact address in `new_addr` with
    /// `MREMAP_FIXED`. Contents are preserved: the kernel re-maps the same
    /// backing object at the new range, and pages added by growth read back as
    /// zero. This is the allocator fast-path (musl and glibc `realloc` try
    /// mremap first for large chunks), so answering it for real — instead of the
    /// former unconditional `ENOMEM` — saves a malloc+memcpy+free per grow.
    ///
    /// `MREMAP_DONTUNMAP` (Linux ≥ 5.7) is not supported and returns `EINVAL`,
    /// which is exactly what a pre-5.7 kernel says.
    pub fn sys_mremap(
        &self,
        old_addr: usize,
        old_len: usize,
        new_len: usize,
        flags: usize,
        new_addr: usize,
    ) -> SysResult {
        const MREMAP_MAYMOVE: usize = 1;
        const MREMAP_FIXED: usize = 2;
        const MREMAP_DONTUNMAP: usize = 4;
        // mmap_lock: remap moves/resizes mappings (see sys_mmap).
        let _aspace = self.linux_process().aspace_lock().lock();
        info!(
            "mremap: old_addr={:#x}, old_len={:#x}, new_len={:#x}, flags={:#x}, new_addr={:#x}",
            old_addr, old_len, new_len, flags, new_addr
        );
        if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP) != 0 {
            return Err(LxError::EINVAL);
        }
        if flags & MREMAP_DONTUNMAP != 0 {
            return Err(LxError::EINVAL);
        }
        if flags & MREMAP_FIXED != 0 && flags & MREMAP_MAYMOVE == 0 {
            return Err(LxError::EINVAL);
        }
        // Linux: old_addr must be page-aligned; lengths round up to pages. A
        // zero old_len (the MAP_SHARED duplication trick) is not supported.
        let (old_len, new_len) = mremap_args(old_addr, old_len, new_len)?;
        if new_len > MAX_MMAP_LEN {
            return Err(LxError::ENOMEM);
        }

        let proc = self.zircon_process();
        let pid = proc.id();
        let vmar = proc.vmar();
        // Whether the range was ever writable, for hunter's W^X history on the
        // range the pages land in. Read before the move invalidates old_addr.
        let writable = vmar
            .find_mapping(old_addr)
            .and_then(|m| m.get_flags(old_addr).ok())
            .map(|f| f.contains(MMUFlags::WRITE))
            .unwrap_or(true);
        let fixed = (flags & MREMAP_FIXED != 0).then_some(new_addr);
        let ret = vmar
            .remap(
                old_addr,
                old_len,
                new_len,
                flags & MREMAP_MAYMOVE != 0,
                fixed,
            )
            .map_err(|e| match e {
                zircon_object::ZxError::NOT_FOUND => LxError::EFAULT,
                zircon_object::ZxError::INVALID_ARGS => LxError::EINVAL,
                _ => LxError::ENOMEM,
            })?;
        if ret == old_addr {
            // Resized in place: retire the dropped tail from hunter's history or
            // record the grown one, depending on the direction.
            if new_len < old_len {
                hunter::check_munmap(pid, old_addr + new_len, old_len - new_len);
            } else if new_len > old_len {
                hunter::record_mapping(pid, old_addr + old_len, new_len - old_len, writable);
            }
        } else {
            hunter::check_munmap(pid, old_addr, old_len);
            hunter::record_mapping(pid, ret, new_len, writable);
        }
        Ok(ret)
    }

    /// Synchronize a file with a memory map
    /// (see [linux man msync(2)](https://www.man7.org/linux/man-pages/man2/msync.2.html)).
    ///
    /// Argument checking follows the man page exactly (`EINVAL` for a misaligned
    /// address, unknown flags or `MS_SYNC|MS_ASYNC` together; `ENOMEM` when the
    /// range touches unmapped pages). No writeback happens beyond that: shared
    /// file mappings hand every mapper the same `VmObject` as the file cache, so
    /// stores are already visible to readers the way `msync` is meant to
    /// guarantee — succeeding here is honest, and it un-breaks software that
    /// treats an `msync` failure as fatal (sqlite, mandb).
    pub fn sys_msync(&self, addr: usize, len: usize, flags: usize) -> SysResult {
        info!(
            "msync: addr={:#x}, len={:#x}, flags={:#x}",
            addr, len, flags
        );
        let Some(len) = msync_args(addr, len, flags)? else {
            return Ok(0);
        };
        // `msync_args` proved this sum fits. It used to be
        // `addr + roundup_pages(len)`: for a `len` in the last page of the
        // address space the round-up wrapped to zero and the walk below ran
        // zero times, so `msync(p, -1, MS_SYNC)` reported success over a range
        // it never looked at; for a `len` just below that the sum itself
        // wrapped, to an `end` under `addr`, with the same result.
        walk_mapped(&self.zircon_process().vmar(), addr, addr + len)
    }

    /// Determine whether pages are resident in memory
    /// (see [linux man mincore(2)](https://www.man7.org/linux/man-pages/man2/mincore.2.html)).
    ///
    /// Writes one byte per page into `vec`; bit 0 set means the page is resident.
    /// Residency is answered from the page table: a demand-paged page that has
    /// never been touched has no PTE and reports non-resident, exactly the
    /// distinction Linux draws. `ENOMEM` when the range includes unmapped pages,
    /// which some allocators use to probe address-space layout.
    pub fn sys_mincore(&self, addr: usize, len: usize, mut vec: UserOutPtr<u8>) -> SysResult {
        info!("mincore: addr={:#x}, len={:#x}", addr, len);
        let pages_count = mincore_args(addr, len)?;
        let residency = mincore_residency(&self.zircon_process().vmar(), addr, pages_count)?;
        vec.write_array(&residency)?;
        Ok(0)
    }

    /// Every page of `[addr, addr+len)` (addr rounded down, len rounded up,
    /// as mlock(2) specifies) must belong to a mapping, else `ENOMEM` — the
    /// address-range validation `mlock`/`munlock` owe their callers.
    fn check_locked_range(&self, addr: usize, len: usize) -> SysResult {
        // The walk is bounded: it stops at the first hole, so it never scans
        // beyond the actually-mapped span plus one page.
        let Some((start, end)) = mlock_args(addr, len)? else {
            return Ok(0);
        };
        walk_mapped(&self.zircon_process().vmar(), start, end)
    }

    /// Lock part of the calling process's memory into RAM (see mlock(2) and
    /// Documentation/mm/unevictable-lru.rst).
    ///
    /// This kernel has no swap and never evicts anonymous pages, so every
    /// mapped page is already permanently resident — after validating the
    /// range the way Linux does, "locked" is the truthful answer. This is what
    /// key-handling software (gpg, ssh-agent) needs from mlock: the guarantee
    /// that secrets never hit backing store.
    pub fn sys_mlock(&self, addr: usize, len: usize) -> SysResult {
        info!("mlock: addr={:#x}, len={:#x}", addr, len);
        self.check_locked_range(addr, len)
    }

    /// `mlock2` = mlock plus a `flags` word; the only defined flag is
    /// `MLOCK_ONFAULT` (see mlock2(2)).
    pub fn sys_mlock2(&self, addr: usize, len: usize, flags: usize) -> SysResult {
        info!(
            "mlock2: addr={:#x}, len={:#x}, flags={:#x}",
            addr, len, flags
        );
        const MLOCK_ONFAULT: usize = 1;
        if flags & !MLOCK_ONFAULT != 0 {
            return Err(LxError::EINVAL);
        }
        self.sys_mlock(addr, len)
    }

    /// Unlock previously locked pages (see mlock(2)); range rules match
    /// `sys_mlock`.
    pub fn sys_munlock(&self, addr: usize, len: usize) -> SysResult {
        info!("munlock: addr={:#x}, len={:#x}", addr, len);
        self.check_locked_range(addr, len)
    }

    /// Lock the whole address space (see mlockall(2)). Flags are validated
    /// exactly as Linux does; with residency permanent here, success follows.
    pub fn sys_mlockall(&self, flags: usize) -> SysResult {
        info!("mlockall: flags={:#x}", flags);
        check_mlockall_flags(flags)
    }

    /// Undo `mlockall` (see munlockall(2)).
    pub fn sys_munlockall(&self) -> SysResult {
        info!("munlockall");
        Ok(0)
    }

    /// Give advice about use of memory
    /// (see [linux man madvise(2)](https://www.man7.org/linux/man-pages/man2/madvise.2.html)).
    ///
    /// `madvise` is advisory: the kernel is free to ignore the hint. zCore does
    /// not yet act on any advice, so a recognised request succeeds without
    /// changing the mapping. An unrecognised advice value, or a start address
    /// that is not page-aligned, is rejected with `EINVAL` as on Linux — which
    /// is stricter (and more correct) than the previous stub that accepted any
    /// value, including garbage, with success.
    pub fn sys_madvise(&self, addr: usize, len: usize, advice: usize) -> SysResult {
        info!(
            "madvise: addr={:#x}, len={:#x}, advice={}",
            addr, len, advice
        );
        if UNHONOURED_MADVISE.contains(&advice) {
            // Loud on purpose: the caller is about to fall back to something
            // slower (a DRBG that reseeds every call, say), and this line is
            // the only place that says why.
            info!(
                "madvise: advice {} is not honoured by this kernel, refused",
                advice
            );
            return Err(LxError::EINVAL);
        }
        let Some(len) = madvise_args(addr, len, advice)? else {
            return Ok(0);
        };
        // MADV_DONTNEED (4) / MADV_FREE (8) must actually DISCARD the pages:
        // Linux guarantees the range reads back as zero on next access. Memory
        // allocators rely on this — they decommit a region with MADV_DONTNEED
        // and later REUSE it assuming it is zeroed. Treating it as a pure no-op
        // leaves STALE data, which corrupts allocator metadata on reuse:
        //   * apk's mimalloc aborts ("trying to free from an invalid arena")
        //     mid-`apk update`, killing every package operation;
        //   * Firefox's mozjemalloc red-black tree corrupts (the earlier crash).
        // We DISCARD the range for real (see `Vmar::madv_dontneed`): unmap the
        // PTEs and drop the VMO frames, so the next touch faults in a fresh zero
        // page. An earlier version only zeroed page-table-visible pages, which
        // silently skipped a page whose frame the VMO still held but whose PTE
        // had been dropped/PROT_NONE'd — mimalloc reused exactly such a page and
        // found its old bytes, hence the deterministic "corrupted free list
        // entry" abort during `apk update`.
        const MADV_DONTNEED: usize = 4;
        const MADV_FREE: usize = 8;
        if advice == MADV_DONTNEED || advice == MADV_FREE {
            // Zero the committed pages in the range THROUGH THE VMO so they read
            // back as zero on next access — including pages whose PTE was dropped
            // or turned PROT_NONE, which a page-table walk cannot see (that gap
            // let mimalloc reuse a stale frame and abort). Done in place, leaving
            // the mapping and frames intact (an earlier unmap+decommit variant
            // turned the abort into a SIGSEGV).
            let proc = self.zircon_process();
            let vmar = proc.vmar();
            vmar.madv_dontneed(addr, len);
        }
        Ok(0)
    }
}

/// How much of a file-backed mapping the file itself can back.
///
/// `get_vmo_shared` promises a VMO that reaches `vmo_offset + len`, but it is a
/// trait method any file-like can implement, so the promise is not enforceable
/// here. A plain `vmo_len - vmo_offset` turns a broken promise into an
/// underflow: a panic in a debug build, and in a release one (no
/// `overflow-checks`) a length near 2^64 that `min` then hands straight to the
/// mapper.
///
/// Saturating instead degrades to `0`, which is the honest reading -- the file
/// backs none of this window -- and takes the caller's demand-zero path,
/// exactly like a mapping that begins past end-of-file.
fn file_map_len(len: usize, vmo_len: usize, vmo_offset: usize) -> usize {
    len.min(vmo_len.saturating_sub(vmo_offset))
}

/// Validate the `offset` argument of `mmap(2)`.
///
/// Two separate rules, because Linux applies them to different calls:
///
/// 1. **Page alignment, for every `mmap`.** x86-64's `SYSCALL_DEFINE6(mmap)`
///    opens with `if (off & ~PAGE_MASK) return -EINVAL;`, before it has even
///    looked at the flags, so an anonymous mapping is held to it too. Nothing
///    that works on Linux can fail this check.
///
/// 2. **`offset + len` must not overflow, for a file-backed `mmap`.** Linux
///    checks the same thing in page units (`do_mmap`: `if ((pgoff + (len >>
///    PAGE_SHIFT)) < pgoff) return -EOVERFLOW;`); this kernel carries the
///    offset in *bytes* all the way down -- `get_vmo(offset, len)` and the page
///    cache both add `offset + len` -- so bytes are the unit that has to be
///    checked here, and the errno is the one Linux uses for it.
///
/// Rule 2 is not theoretical. `offset` arrives as the sixth raw machine word
/// and went unchecked into `FileLike::get_vmo{,_shared}`, so
///
/// ```c
/// mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd, 0xFFFFFFFFFFFFF000);
/// ```
///
/// -- page-aligned, so it passed the one check that did exist -- reached
/// `inode_cache_vmo`, where `offset + len` wraps to 0. In a debug build that
/// addition is a kernel panic from an unprivileged process. In a release build
/// (no `overflow-checks`) it wraps instead, the "does the cache cover this
/// window?" test reads `0 > vmo.len()` and says yes, and the caller then
/// computes `vmo.len() - offset`, which underflows in turn.
///
/// Returns the offset as a `usize`, which is what every layer below wants.
fn validate_mmap_offset(
    offset: u64,
    len: usize,
    anonymous: bool,
) -> linux_object::error::LxResult<usize> {
    if !offset.is_multiple_of(PAGE_SIZE as u64) {
        return Err(LxError::EINVAL);
    }
    // 32-bit targets: an offset a `usize` cannot even name is an overflow by
    // definition. On 64-bit this conversion cannot fail.
    let offset = usize::try_from(offset).map_err(|_| LxError::EOVERFLOW)?;
    if !anonymous && offset.checked_add(len).is_none() {
        return Err(LxError::EOVERFLOW);
    }
    Ok(offset)
}

/// `do_mmap`'s two refusals of a length, in its order: `if (!len) return
/// -EINVAL;` first, then the size cap (`ENOMEM`, what Linux answers when the
/// page-aligned length overflows or the address space cannot hold it).
///
/// A zero length was `ENOMEM`. A reader that maps a file whole (grep and
/// ripgrep, ld.so's `_dl_map_segments` probes) meets an empty file, gets
/// "Cannot allocate memory" instead of `EINVAL`, and the code that takes the
/// empty-file path on `EINVAL` reports an allocation failure instead.
fn mmap_len_check(len: usize) -> LxResult<()> {
    if len == 0 {
        return Err(LxError::EINVAL);
    }
    if len > MAX_MMAP_LEN {
        return Err(LxError::ENOMEM);
    }
    Ok(())
}

/// `madvise(2)` advice values this kernel accepts. All of zCore's target arches
/// (x86_64/aarch64/riscv64) share this (asm-generic) numbering:
///   0 NORMAL, 1 RANDOM, 2 SEQUENTIAL, 3 WILLNEED, 4 DONTNEED, 8 FREE,
///   12 MERGEABLE, 13 UNMERGEABLE, 14 HUGEPAGE, 15 NOHUGEPAGE, 16 DONTDUMP,
///   17 DODUMP, 20 COLD, 21 PAGEOUT, 100 HWPOISON, 101 SOFT_OFFLINE.
/// Each of these is either honoured (4 and 8 discard the pages) or a hint a
/// kernel may ignore without anyone being able to tell.
const KNOWN_MADVISE: &[usize] = &[0, 1, 2, 3, 4, 8, 12, 13, 14, 15, 16, 17, 20, 21, 100, 101];

/// The advice values Linux knows and this kernel cannot honour, refused with
/// `EINVAL` exactly as a kernel from before they existed would (`MADV_REMOVE`
/// 9, `MADV_DONTFORK` 10, `MADV_DOFORK` 11, `MADV_WIPEONFORK` 18,
/// `MADV_KEEPONFORK` 19). They are not hints: each changes what a `fork` or
/// the backing store does afterwards, and a caller that hears 0 relies on it.
///
/// They were accepted and ignored. BoringSSL and AWS-LC (`fork_detect.c`,
/// behind rustls' default backend) ask for `MADV_WIPEONFORK` on a page and,
/// told yes, trust that page to read as zero in a child: here the child kept
/// it, so parent and child went on producing the same DRBG output after a
/// fork, and repeated nonces and keys. Told `EINVAL`, as on a kernel older
/// than 4.14, they fall back to detecting the fork another way.
const UNHONOURED_MADVISE: &[usize] = &[9, 10, 11, 18, 19];

/// Whether `advice` is a `madvise(2)` value this kernel accepts. Pure, so the
/// classification is unit-testable independently of the syscall plumbing.
fn madvise_advice_known(advice: usize) -> bool {
    KNOWN_MADVISE.contains(&advice)
}

/// The first address no user mapping can reach.
///
/// Linux calls it `TASK_SIZE`, and every memory syscall bounds `addr + len`
/// against it before it looks at a single mapping -- `access_ok(start, len)` in
/// `mm/mincore.c`, `len > TASK_SIZE - start` in `mm/mmap.c`. Here it is the
/// span of the root user VMAR, which `VmAddressRegion::new_root` builds from
/// exactly these two constants, so a range that ends past it cannot name a
/// mapping however far it is walked.
const USER_ASPACE_END: usize = (USER_ASPACE_BASE + USER_ASPACE_SIZE) as usize;

/// What the `(addr, len)` pair a memory syscall was handed actually asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserRange {
    /// `len` is zero: the call names no page at all.
    Empty,
    /// `[addr, addr + len)`, with `len` rounded up to whole pages, non-zero,
    /// and wholly inside the user address space.
    Pages(usize),
    /// The range runs off the end of the user address space.
    Beyond,
}

/// Resolve the raw `(addr, len)` of a memory syscall into the range it names.
///
/// Every one of them takes that pair straight from userspace and has to answer
/// the same three questions before it touches a mapping. The only thing that
/// differs between them is the errno each puts on the two bad answers -- that
/// difference is real and comes from the man pages, so it stays with the
/// callers and the rule lives here.
///
/// Two traps, and both were open on every caller:
///
/// * **The round-up wraps.** [`roundup_pages`] is `ceil`, and `ceil` rounds
///   with `wrapping_add` on purpose, so every `len` from `usize::MAX - 4094` to
///   `usize::MAX` comes back as **zero** -- 4095 lengths that each name most of
///   the address space and each arrived looking like an empty range. Linux
///   rounds and then asks the same question out loud: `if (len_in && !len)
///   return -EINVAL;` (`mm/madvise.c`).
/// * **The sum overflows.** For a `len` near `usize::MAX - 4095` the round-up
///   does *not* wrap -- it is `2^64 - 4096` -- and `addr + len` does. The kernel
///   is built without `overflow-checks`, so that sum wraps to an end below
///   `addr` and the page walk behind it runs zero times; a test build panics on
///   the same line instead.
///
/// Both land in the same place: a call that names most of the address space
/// looks like one that names nothing, and the syscall reports success over a
/// range it never looked at.
fn user_range(addr: usize, len: usize) -> UserRange {
    if len == 0 {
        return UserRange::Empty;
    }
    let rounded = roundup_pages(len);
    if rounded == 0 {
        return UserRange::Beyond;
    }
    match addr.checked_add(rounded) {
        Some(end) if end <= USER_ASPACE_END => UserRange::Pages(rounded),
        _ => UserRange::Beyond,
    }
}

/// What `munmap(2)` answers for its `(addr, len)` before it touches a mapping
/// (`do_vmi_munmap`, `mm/mmap.c`): a misaligned address, an empty range and a
/// range past the end of the address space are all `EINVAL`.
fn munmap_args(addr: usize, len: usize) -> LxResult<usize> {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(LxError::EINVAL);
    }
    match user_range(addr, len) {
        UserRange::Pages(len) => Ok(len),
        UserRange::Empty | UserRange::Beyond => Err(LxError::EINVAL),
    }
}

/// What `mprotect(2)` answers (`mm/mprotect.c`): `EINVAL` for a misaligned
/// address, plain success for an empty range, `ENOMEM` for one past the end.
///
/// `Ok(None)` is the empty range, and it is not cosmetic:
/// `VmAddressRegion::protect` refuses a zero length with `INVALID_ARGS`, so
/// `mprotect(p, 0, prot)` -- which Linux answers with 0 before looking at a
/// single VMA -- reached the failure arm of the caller.
fn mprotect_args(addr: usize, len: usize) -> LxResult<Option<usize>> {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(LxError::EINVAL);
    }
    match user_range(addr, len) {
        UserRange::Empty => Ok(None),
        UserRange::Pages(len) => Ok(Some(len)),
        UserRange::Beyond => Err(LxError::ENOMEM),
    }
}

/// The low four bits of `mmap`'s flag word (`MAP_TYPE`), which name the
/// mapping's kind and are not a bitmask: exactly one of three values.
const MAP_TYPE: usize = 0x0f;
/// `MAP_SHARED`: stores reach every other mapper of the object.
const MAP_SHARED: usize = 0x01;
/// `MAP_PRIVATE`: stores stay in this address space.
const MAP_PRIVATE: usize = 0x02;
/// `MAP_SHARED_VALIDATE`: `MAP_SHARED`, but with the rest of the flag word
/// checked rather than ignored. It is the value 3, not a third bit.
const MAP_SHARED_VALIDATE: usize = 0x03;

/// Whether a mapping is shared, or `EINVAL` if the caller named no kind at
/// all -- or one that does not exist.
///
/// `MAP_TYPE` is a small enum living in a flag word, and `do_mmap` reads it
/// with a `switch` whose `default` is `-EINVAL`. Read as a bitmask, which is
/// what `MmapFlags::contains(SHARED)` does, `mmap(..., MAP_ANONYMOUS, ...)`
/// with neither bit set is a private mapping the caller never asked for, and
/// a kind of 4, 5 or 15 is whatever the low bits happen to spell.
///
/// `MAP_SHARED_VALIDATE` is shared for a file mapping and `EINVAL` for an
/// anonymous one: Linux runs a second `switch` in the anonymous arm, and it
/// knows only `MAP_SHARED` and `MAP_PRIVATE`.
fn mmap_shared(flags: usize, anonymous: bool) -> LxResult<bool> {
    match flags & MAP_TYPE {
        MAP_SHARED => Ok(true),
        MAP_PRIVATE => Ok(false),
        MAP_SHARED_VALIDATE if !anonymous => Ok(true),
        _ => Err(LxError::EINVAL),
    }
}

/// The flags `MAP_SHARED_VALIDATE` does not have to validate: Linux's
/// `LEGACY_MAP_MASK`, the flag word as it stood before `MAP_SHARED_VALIDATE`
/// existed. What is outside it (`MAP_SYNC`, `MAP_FIXED_NOREPLACE`, and every
/// bit not yet given a meaning) is exactly what the kind exists to refuse.
const LEGACY_MAP_MASK: usize = MAP_SHARED
    | MAP_PRIVATE
    | MAP_FIXED
    | MMAP_ANONYMOUS
    | MAP_DENYWRITE
    | MAP_EXECUTABLE
    | MAP_GROWSDOWN
    | MAP_LOCKED
    | MAP_NORESERVE
    | MAP_POPULATE
    | MAP_NONBLOCK
    | MAP_STACK
    | MAP_HUGETLB
    | MAP_32BIT
    | MAP_HUGE_2MB
    | MAP_HUGE_1GB;
const MAP_FIXED: usize = 0x10;
const MAP_DENYWRITE: usize = 0x800;
const MAP_EXECUTABLE: usize = 0x1000;
const MAP_GROWSDOWN: usize = 0x100;
const MAP_LOCKED: usize = 0x2000;
const MAP_NORESERVE: usize = 0x4000;
const MAP_POPULATE: usize = 0x8000;
const MAP_NONBLOCK: usize = 0x10000;
const MAP_STACK: usize = 0x20000;
const MAP_HUGETLB: usize = 0x40000;
/// `MAP_32BIT` exists on x86_64 only; elsewhere `<linux/mman.h>` defines it
/// as 0 for this mask, so bit 6 is an unknown flag there.
#[cfg(target_arch = "x86_64")]
const MAP_32BIT: usize = 0x40;
#[cfg(not(target_arch = "x86_64"))]
const MAP_32BIT: usize = 0;
const MAP_HUGE_2MB: usize = 21 << 26;
const MAP_HUGE_1GB: usize = 30 << 26;

/// `do_mmap`'s `MAP_SHARED_VALIDATE` arm: `flags & ~LEGACY_MAP_MASK` is
/// `EOPNOTSUPP`. That answer is the whole point of the kind. A program asks
/// for `MAP_SHARED_VALIDATE | MAP_SYNC` precisely so that a kernel which
/// does not know `MAP_SYNC` (or the file's backing store does not support
/// it) refuses instead of quietly handing back a mapping without the
/// guarantee: with plain `MAP_SHARED` unknown bits are ignored, and the
/// caller cannot tell.
///
/// The kind was accepted as a plain `MAP_SHARED` and the rest of the word
/// never looked at, so PMDK, the DAX users and `MAP_SYNC` probes got a
/// mapping without the persistence they were validating for, and a
/// `MAP_SHARED_VALIDATE | MAP_FIXED_NOREPLACE` got the placement where
/// Linux says the combination is unsupported.
fn shared_validate_flags(flags: usize) -> LxResult<()> {
    if flags & MAP_TYPE == MAP_SHARED_VALIDATE && flags & !LEGACY_MAP_MASK != 0 {
        return Err(LxError::EOPNOTSUPP);
    }
    Ok(())
}

/// `do_mmap`'s check of the descriptor's open mode against the mapping
/// asked for: a shared mapping with `PROT_WRITE` needs the file open for
/// writing (`!(file->f_mode & FMODE_WRITE)` is `EACCES`), and any file
/// mapping, shared or private, needs it open for reading (the `MAP_PRIVATE`
/// arm the shared one falls through to).
///
/// Nothing looked. A file a process could only read (`O_RDONLY` on a
/// root-owned configuration, say) could be mapped `MAP_SHARED|PROT_WRITE`
/// and written through the mapping, and the shared VMO's writeback carried
/// the stores to the file; and an `O_WRONLY` descriptor mapped where Linux
/// says `EACCES`.
fn mmap_file_access(
    shared: bool,
    want_write: bool,
    open: linux_object::fs::OpenFlags,
) -> LxResult<()> {
    if shared && want_write && !open.writable() {
        return Err(LxError::EACCES);
    }
    if !open.readable() {
        return Err(LxError::EACCES);
    }
    Ok(())
}

/// Where `mmap` must put the mapping.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Placement {
    /// `addr` is a hint; the kernel places the mapping where it likes.
    Hint,
    /// `MAP_FIXED`: at exactly `addr`, over whatever is already there.
    Fixed,
    /// `MAP_FIXED_NOREPLACE`: at exactly `addr`, or `EEXIST`.
    FixedNoReplace,
}

/// How the flag word asks for the mapping to be placed.
///
/// `MAP_FIXED_NOREPLACE` used to be dropped by
/// `MmapFlags::from_bits_truncate`, so a caller asking for "this range or
/// nothing" got a hint instead: the mapping landed wherever there was room,
/// the call reported success, and the one thing the flag exists to prevent --
/// silently ending up somewhere else -- is what happened.
///
/// It implies `MAP_FIXED` (`do_mmap` sets the bit itself) and wins over it:
/// with both set the mapping still refuses to replace anything.
fn mmap_placement(flags: MmapFlags) -> Placement {
    if flags.contains(MmapFlags::FIXED_NOREPLACE) {
        Placement::FixedNoReplace
    } else if flags.contains(MmapFlags::FIXED) {
        Placement::Fixed
    } else {
        Placement::Hint
    }
}

/// `PROT_SEM`, which this kernel does not act on but Linux accepts.
const PROT_SEM: usize = 0x8;
/// `PROT_GROWSDOWN`: apply to the whole of a growing-down mapping.
const PROT_GROWSDOWN: usize = 0x0100_0000;
/// `PROT_GROWSUP`: the same, upwards.
const PROT_GROWSUP: usize = 0x0200_0000;

/// The protection `mprotect(2)` was asked for, or `EINVAL`.
///
/// `do_mprotect_pkey` rejects a bit it does not know (`arch_validate_prot`)
/// and the two growth directions together, and only then masks the growth
/// bits off. Truncating instead, which is what this used to do, turned
/// `mprotect(p, len, PROT_READ | 0x40)` into a plain read-only mapping --
/// the caller asked for something this kernel cannot do and was told it had
/// it.
///
/// `mmap` does NOT validate `prot`: `do_mmap` never looks at the unknown
/// bits, so a flag word that `mprotect` refuses is legal there. The two
/// differ on purpose, which is why this is not shared with the mmap path.
fn mprotect_prot(prot: usize) -> LxResult<MmapProt> {
    const KNOWN: usize = MmapProt::READ.bits()
        | MmapProt::WRITE.bits()
        | MmapProt::EXEC.bits()
        | PROT_SEM
        | PROT_GROWSDOWN
        | PROT_GROWSUP;
    if prot & !KNOWN != 0 {
        return Err(LxError::EINVAL);
    }
    if prot & PROT_GROWSDOWN != 0 && prot & PROT_GROWSUP != 0 {
        return Err(LxError::EINVAL);
    }
    Ok(MmapProt::from_bits_truncate(prot))
}

/// `msync(2)` flags (`mm/msync.c`).
const MS_ASYNC: usize = 1;
/// See [`MS_ASYNC`].
const MS_INVALIDATE: usize = 2;
/// See [`MS_ASYNC`].
const MS_SYNC: usize = 4;

/// What `msync(2)` answers for `(addr, len, flags)` before it walks a single
/// mapping (`mm/msync.c`): unknown flags, `MS_SYNC` and `MS_ASYNC` together and
/// a misaligned address are `EINVAL`; an empty range is success (`if (end ==
/// start) goto out;`, reached with `error = 0`); a range past the end of the
/// address space is `ENOMEM`.
fn msync_args(addr: usize, len: usize, flags: usize) -> LxResult<Option<usize>> {
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0
        || (flags & MS_SYNC != 0 && flags & MS_ASYNC != 0)
        || !addr.is_multiple_of(PAGE_SIZE)
    {
        return Err(LxError::EINVAL);
    }
    match user_range(addr, len) {
        UserRange::Empty => Ok(None),
        UserRange::Pages(len) => Ok(Some(len)),
        UserRange::Beyond => Err(LxError::ENOMEM),
    }
}

/// How many pages `mincore(2)` was asked about (`mm/mincore.c`): `EINVAL` for a
/// misaligned address, `ENOMEM` for a range `access_ok` would refuse.
///
/// The upper bound is what keeps the caller's reservation honest. `len` is a
/// raw machine word and the byte vector was sized from it, so
/// `mincore(p, 1 << 63, v)` asked the fixed kernel heap for 2 PiB -- an
/// infallible allocation, and therefore a panic of the whole machine from an
/// unprivileged process.
fn mincore_args(addr: usize, len: usize) -> LxResult<usize> {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(LxError::EINVAL);
    }
    match user_range(addr, len) {
        UserRange::Empty => Ok(0),
        UserRange::Pages(len) => Ok(len / PAGE_SIZE),
        UserRange::Beyond => Err(LxError::ENOMEM),
    }
}

/// What `madvise(2)` answers for `(addr, len, advice)` (`mm/madvise.c`):
/// unrecognised advice and a misaligned address are `EINVAL`, and so is a
/// length whose round-up wrapped -- Linux spells that one out on its own line,
/// `if (len_in && !len) return -EINVAL;`. An empty range succeeds.
fn madvise_args(addr: usize, len: usize, advice: usize) -> LxResult<Option<usize>> {
    if !madvise_advice_known(advice) || !addr.is_multiple_of(PAGE_SIZE) {
        return Err(LxError::EINVAL);
    }
    match user_range(addr, len) {
        UserRange::Empty => Ok(None),
        UserRange::Pages(len) => Ok(Some(len)),
        UserRange::Beyond => Err(LxError::EINVAL),
    }
}

/// The page range `mlock(2)` and `munlock(2)` validate, as `[start, end)`, or
/// `None` when there is nothing to validate.
///
/// These two round the address DOWN and stretch the length to cover the rest of
/// its page (`len = PAGE_ALIGN(len + offset_in_page(start)); start &=
/// PAGE_MASK;`, `mm/mlock.c`), so what has to stay inside the address space is
/// the SUM, not the length: with an unaligned `addr` the range is a page longer
/// than `roundup_pages(len)`.
///
/// Rounding the sum was already the shape here, but in the other order --
/// `addr.checked_add(len).map(roundup_pages)` -- which caught the addition and
/// then let the round-up wrap to zero. An `end` of zero is below every `start`,
/// so the walk ran zero times and `mlock` reported the whole address space
/// locked.
fn mlock_args(addr: usize, len: usize) -> LxResult<Option<(usize, usize)>> {
    if len == 0 {
        return Ok(None);
    }
    let start = round_down_pages(addr);
    let span = (addr - start).checked_add(len).ok_or(LxError::ENOMEM)?;
    match user_range(start, span) {
        UserRange::Empty => Ok(None),
        UserRange::Pages(span) => Ok(Some((start, start + span))),
        UserRange::Beyond => Err(LxError::ENOMEM),
    }
}

/// The two lengths `mremap(2)` works with (`mm/mremap.c`). `old_addr` must be
/// page-aligned, and neither length may be empty or run off the end of the
/// address space. A zero `old_len` is the `MAP_SHARED` duplication trick, which
/// this kernel does not support.
fn mremap_args(old_addr: usize, old_len: usize, new_len: usize) -> LxResult<(usize, usize)> {
    let old_len = match user_range(old_addr, old_len) {
        UserRange::Pages(len) => len,
        UserRange::Empty | UserRange::Beyond => return Err(LxError::EINVAL),
    };
    if !old_addr.is_multiple_of(PAGE_SIZE) {
        return Err(LxError::EINVAL);
    }
    // The new length is not anchored anywhere yet -- `may_move` can place it
    // where it likes -- so it is bounded against the address space as a whole.
    let new_len = match user_range(0, new_len) {
        UserRange::Pages(len) => len,
        UserRange::Empty | UserRange::Beyond => return Err(LxError::EINVAL),
    };
    Ok((old_len, new_len))
}

/// Where `brk(2)` should move the program break, or `None` to leave it where it
/// is -- which is Linux's answer to a break it cannot satisfy (`if (brk <
/// mm->start_brk) goto out;` returns the old one).
///
/// The upper bound is the one that was missing. `roundup_pages` wraps (see
/// [`user_range`]), so a `new_brk` in the last page of the address space came
/// back as **zero**; zero is below the current break, so the shrink branch took
/// it and moved the program's heap end to address 0, inside its own image.
/// `brk(-1)` is one line of C.
fn brk_target(new_brk: usize, heap_base: usize) -> Option<usize> {
    if new_brk < heap_base || new_brk > USER_ASPACE_END {
        return None;
    }
    Some(roundup_pages(new_brk))
}

/// Which way `brk(2)` moves the heap's mapping, page for page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrkMove {
    /// The new break ends on an earlier page.
    Shrink,
    /// The new break ends on a later page.
    Grow,
    /// The new break ends on the same page as the old one: nothing to map
    /// or unmap (`if (oldbrk == newbrk) { mm->brk = brk; goto success; }`).
    WithinPage,
}

/// What `brk(2)` does with a break it can reach, as `__do_sys_brk` does it:
/// the number STORED, and RETURNED, is the one the caller asked for
/// (`mm->brk = brk; ... return brk;`); the page-aligned one only says where
/// the mapping ends. This kernel used to store and return the aligned one,
/// so glibc's `sbrk(100)`, which keeps the syscall's answer as `__curbrk`,
/// moved the break by 4096, and `sbrk(0)` never agreed with `start + n`.
#[derive(Debug, PartialEq, Eq)]
struct BrkPlan {
    /// The break to store and return: what was asked for.
    brk: usize,
    /// Where the heap mapping must end for it: the break rounded up to a
    /// page.
    mapped_end: usize,
    /// How the mapping moves relative to the old break's page.
    moved: BrkMove,
}

fn brk_plan(new_brk: usize, current_brk: usize, heap_base: usize) -> Option<BrkPlan> {
    let mapped_end = brk_target(new_brk, heap_base)?;
    let current_end = roundup_pages(current_brk);
    let moved = match mapped_end.cmp(&current_end) {
        core::cmp::Ordering::Less => BrkMove::Shrink,
        core::cmp::Ordering::Greater => BrkMove::Grow,
        core::cmp::Ordering::Equal => BrkMove::WithinPage,
    };
    Some(BrkPlan {
        brk: new_brk,
        mapped_end,
        moved,
    })
}

/// Every page of `[start, end)` must belong to a mapping, else `ENOMEM`.
///
/// This is the address-range validation `msync(2)` and `mlock(2)`/`munlock(2)`
/// owe their callers, and it was written out twice -- once each -- for a check
/// neither of them varies. The walk stops at the first hole, so it never scans
/// beyond the mapped span plus one page.
fn walk_mapped(vmar: &Arc<VmAddressRegion>, start: usize, end: usize) -> SysResult {
    let mut page = start;
    while page < end {
        if vmar.find_mapping(page).is_none() {
            return Err(LxError::ENOMEM);
        }
        page += PAGE_SIZE;
    }
    Ok(0)
}

/// One byte per page of `[addr, addr + pages * PAGE_SIZE)` for `mincore(2)`,
/// bit 0 set when the page is resident, `ENOMEM` at the first page that is not
/// mapped.
///
/// Resident means "has a page-table entry": a demand-paged page nobody has
/// touched yet reports non-resident, which is exactly the distinction
/// `mincore` draws, and the one allocators use to probe address-space layout.
///
/// `addr + i * PAGE_SIZE` is safe here only because the caller bounded the
/// range first (see [`mincore_args`]); so is the reservation, which Linux
/// keeps to a single page of kernel buffer (`__get_free_page`, mm/mincore.c)
/// rather than one byte per page of a range userspace chose. Sizing it from
/// `len` made it a function of a raw machine word: `mincore(p, 1 << 63, v)`
/// asked the fixed kernel heap for 2 PiB, and that allocation cannot fail,
/// only abort the machine.
fn mincore_residency(vmar: &Arc<VmAddressRegion>, addr: usize, pages: usize) -> LxResult<Vec<u8>> {
    let mut residency = Vec::with_capacity(pages.min(PAGE_SIZE));
    for i in 0..pages {
        let page = addr + i * PAGE_SIZE;
        let mapping = vmar.find_mapping(page).ok_or(LxError::ENOMEM)?;
        residency.push(mapping.query_vaddr(page).is_ok() as u8);
    }
    Ok(residency)
}

#[cfg(test)]
mod mmap_offset_tests {
    //! `mmap`'s sixth argument is a raw machine word that userspace chooses,
    //! and this kernel carries it in bytes into the page cache, where it is
    //! added to the mapping length. An offset that is not page-aligned, or one
    //! whose sum with the length does not fit, has to be refused at the door:
    //! below this point the addition either panics the kernel or wraps, and the
    //! wrap is the worse of the two -- it makes a window the cache does not
    //! cover look like one it does.

    use super::validate_mmap_offset;
    use linux_object::error::LxError;

    const PAGE: usize = 4096;

    #[test]
    fn a_page_aligned_offset_is_accepted_and_comes_back_unchanged() {
        for off in [0u64, 4096, 8192, 1 << 20, 1 << 32] {
            assert_eq!(
                validate_mmap_offset(off, PAGE, false).unwrap(),
                off as usize,
                "offset {:#x} should be accepted verbatim",
                off
            );
        }
    }

    #[test]
    fn an_unaligned_offset_is_einval() {
        // Linux checks this before it looks at anything else:
        // `if (off & ~PAGE_MASK) return -EINVAL;`.
        // 2048 and 6144 are in the list on purpose: they are aligned to half
        // a page, so a check written against the wrong constant still passes
        // every other case here.
        for off in [1u64, 511, 2048, 4095, 4097, 6144, 8191, u64::MAX] {
            assert_eq!(
                validate_mmap_offset(off, PAGE, false),
                Err(LxError::EINVAL),
                "offset {:#x} is not page-aligned and must be EINVAL",
                off
            );
        }
    }

    #[test]
    fn an_unaligned_offset_is_einval_for_an_anonymous_mapping_too() {
        // The man page says `fd` and `offset` are ignored with MAP_ANONYMOUS,
        // and they are -- but Linux still rejects an unaligned one, because the
        // check is in the syscall entry, above the flag. Nothing that runs on
        // Linux can trip over this.
        assert_eq!(validate_mmap_offset(4097, PAGE, true), Err(LxError::EINVAL));
    }

    #[test]
    fn the_offset_that_wrapped_the_page_cache_is_eoverflow() {
        // The reachable one:
        //   mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd, 0xFFFFFFFFFFFFF000)
        // Page-aligned, so the only check that existed let it through, and
        // `offset + len` in `inode_cache_vmo` wraps to exactly 0.
        let offset = u64::MAX - PAGE as u64 + 1;
        assert_eq!(offset % PAGE as u64, 0, "the offset under test is aligned");
        assert_eq!(
            (offset as usize).wrapping_add(PAGE),
            0,
            "this is the offset whose sum wraps to zero"
        );
        assert_eq!(
            validate_mmap_offset(offset, PAGE, false),
            Err(LxError::EOVERFLOW)
        );
    }

    #[test]
    fn the_largest_sum_that_still_fits_is_accepted() {
        // The boundary is exact: `offset + len == usize::MAX + 1` overflows,
        // `offset + len == usize::MAX` does not -- but `usize::MAX` is not
        // page-aligned, so the last acceptable offset for a one-page mapping is
        // two pages below the wrap.
        let last_ok = (usize::MAX - 2 * PAGE + 1) as u64;
        assert_eq!(last_ok % PAGE as u64, 0);
        assert!(validate_mmap_offset(last_ok, PAGE, false).is_ok());
        assert_eq!(
            validate_mmap_offset(last_ok + PAGE as u64, PAGE, false),
            Err(LxError::EOVERFLOW),
            "one page further and the sum no longer fits"
        );
    }

    #[test]
    fn the_overflow_depends_on_the_length_not_just_the_offset() {
        // The same offset is fine for a small mapping and not for a big one:
        // the check is about the window, not about how large the offset looks.
        // Sixteen pages below the wrap, so the window may be fifteen pages
        // long but not sixteen.
        let offset = (usize::MAX - 16 * PAGE + 1) as u64;
        assert_eq!(offset % PAGE as u64, 0);
        assert!(validate_mmap_offset(offset, PAGE, false).is_ok());
        assert!(validate_mmap_offset(offset, 15 * PAGE, false).is_ok());
        assert_eq!(
            validate_mmap_offset(offset, 16 * PAGE, false),
            Err(LxError::EOVERFLOW)
        );
    }

    #[test]
    fn an_anonymous_mapping_keeps_an_offset_linux_would_keep() {
        // Linux's overflow check is in page units, so a huge *aligned* offset
        // survives it; with MAP_ANONYMOUS the offset is then ignored outright
        // and nothing ever adds it to anything. Rejecting it here would fail a
        // call Linux accepts, so the overflow rule is the file-backed path's
        // alone.
        let offset = u64::MAX - PAGE as u64 + 1;
        assert_eq!(
            validate_mmap_offset(offset, PAGE, true).unwrap(),
            offset as usize
        );
    }

    #[test]
    fn the_file_backed_length_never_underflows() {
        use super::file_map_len;
        // The ordinary case: the VMO covers the whole window.
        assert_eq!(file_map_len(2 * PAGE, 8 * PAGE, 0), 2 * PAGE);
        assert_eq!(file_map_len(2 * PAGE, 8 * PAGE, 6 * PAGE), 2 * PAGE);
        // Mapping partly past the end of the file: only the head is backed.
        assert_eq!(file_map_len(4 * PAGE, 8 * PAGE, 6 * PAGE), 2 * PAGE);
        // The offset sits exactly at, or beyond, the end of the VMO. A plain
        // subtraction underflows here; the answer has to be "nothing".
        assert_eq!(file_map_len(PAGE, 8 * PAGE, 8 * PAGE), 0);
        assert_eq!(file_map_len(PAGE, 8 * PAGE, 9 * PAGE), 0);
        assert_eq!(file_map_len(PAGE, 0, usize::MAX), 0);
    }

    #[test]
    fn a_zero_offset_is_always_fine() {
        // What every anonymous mapping and almost every file mapping passes.
        assert_eq!(validate_mmap_offset(0, 0, false).unwrap(), 0);
        assert_eq!(validate_mmap_offset(0, usize::MAX, false).unwrap(), 0);
        assert_eq!(validate_mmap_offset(0, PAGE, true).unwrap(), 0);
    }
}

#[cfg(test)]
mod madvise_tests {
    use super::madvise_advice_known;

    #[test]
    fn known_advice_is_accepted() {
        // NORMAL/RANDOM/SEQUENTIAL/WILLNEED/DONTNEED/FREE and a few higher ones.
        for a in [0usize, 1, 2, 3, 4, 8, 12, 17, 21, 100, 101] {
            assert!(madvise_advice_known(a), "advice {} should be known", a);
        }
    }

    /// An advice this kernel cannot honour is refused, not answered 0: a
    /// `MADV_WIPEONFORK` page that a child would keep is the difference
    /// between a DRBG that reseeds after `fork` and one that repeats.
    #[test]
    fn advice_this_kernel_cannot_honour_is_refused_not_faked() {
        const MADV_REMOVE: usize = 9;
        const MADV_DONTFORK: usize = 10;
        const MADV_DOFORK: usize = 11;
        const MADV_WIPEONFORK: usize = 18;
        const MADV_KEEPONFORK: usize = 19;
        for a in [
            MADV_REMOVE,
            MADV_DONTFORK,
            MADV_DOFORK,
            MADV_WIPEONFORK,
            MADV_KEEPONFORK,
        ] {
            assert!(!madvise_advice_known(a), "advice {} is faked", a);
            assert!(super::UNHONOURED_MADVISE.contains(&a));
        }
        // And the two lists are disjoint: nothing is both honoured and not.
        for a in super::UNHONOURED_MADVISE {
            assert!(!super::KNOWN_MADVISE.contains(a), "{} in both lists", a);
        }
    }

    #[test]
    fn unknown_advice_is_rejected() {
        // Gaps in the numbering (5,6,7) and out-of-range values are unknown.
        for a in [5usize, 6, 7, 22, 99, 102, usize::MAX] {
            assert!(!madvise_advice_known(a), "advice {} should be unknown", a);
        }
    }
}

bitflags! {
    /// for the flag argument in mmap()
    pub struct MmapFlags: usize {
        #[allow(clippy::identity_op)]
        /// Changes are shared.
        const SHARED = 1 << 0;
        /// Changes are private.
        const PRIVATE = 1 << 1;
        /// Place the mapping at the exact address
        const FIXED = 1 << 4;
        /// Place the mapping at the exact address, or fail with `EEXIST`
        /// rather than replace what is already there.
        const FIXED_NOREPLACE = 1 << 20;
        /// The mapping is not backed by any file. (non-POSIX)
        const ANONYMOUS = MMAP_ANONYMOUS;
    }
}

/// MmapFlags `MMAP_ANONYMOUS` depends on arch
#[cfg(target_arch = "mips")]
const MMAP_ANONYMOUS: usize = 0x800;
#[cfg(not(target_arch = "mips"))]
const MMAP_ANONYMOUS: usize = 1 << 5;

bitflags! {
    /// for the prot argument in mmap()
    pub struct MmapProt: usize {
        #[allow(clippy::identity_op)]
        /// Data can be read
        const READ = 1 << 0;
        /// Data can be written
        const WRITE = 1 << 1;
        /// Data can be executed
        const EXEC = 1 << 2;
    }
}

impl MmapProt {
    /// convert MmapProt to MMUFlags
    fn to_flags(self) -> MMUFlags {
        // PROT_NONE (no READ/WRITE/EXEC): USER only. The arch `From<MMUFlags>`
        // conversion refuses to stamp PRESENT without R/W/X, so the PTE
        // stays absent — USER here is a mapping tag, not a hardware bit.
        //
        // Empty flags (the previous choice) broke musl's mallocng / pthread
        // stacks / ld.so: they `mmap(PROT_NONE)` then `mprotect` a slice to
        // RW. `VmMapping::protect` only copies RXW, so the raised pages had
        // READ|WRITE but no USER; the #PF error code is WRITE|USER and
        // `flags.contains(access_flags)` then ACCESS_DENIED — SIGSEGV on
        // the first store (`init` at 0x473000, `[anon]+0x1000`).
        let mut flags = MMUFlags::USER;
        if self.contains(MmapProt::READ) {
            flags |= MMUFlags::READ;
        }
        if self.contains(MmapProt::WRITE) {
            flags |= MMUFlags::WRITE;
        }
        if self.contains(MmapProt::EXEC) {
            flags |= MMUFlags::EXECUTE;
        }
        flags
    }
}

#[cfg(test)]
mod mmap_file_access_tests {
    //! The open mode a file mapping needs, which was never asked.

    use super::*;
    use linux_object::fs::OpenFlags;

    /// `MAP_SHARED|PROT_WRITE` needs a descriptor open for writing; a
    /// private writable mapping (copy on write) does not.
    #[test]
    fn a_shared_writable_mapping_needs_the_file_open_for_writing() {
        assert_eq!(
            mmap_file_access(true, true, OpenFlags::RDONLY),
            Err(LxError::EACCES)
        );
        assert_eq!(mmap_file_access(true, true, OpenFlags::RDWR), Ok(()));
        assert_eq!(
            mmap_file_access(false, true, OpenFlags::RDONLY),
            Ok(()),
            "private: copy on write"
        );
        assert_eq!(
            mmap_file_access(true, false, OpenFlags::RDONLY),
            Ok(()),
            "shared read-only"
        );
    }

    /// Every file mapping needs the descriptor open for reading, `O_WRONLY`
    /// included, whatever the protection asked for.
    #[test]
    fn every_file_mapping_needs_the_file_open_for_reading() {
        for (shared, want_write) in [(false, false), (false, true), (true, false), (true, true)] {
            assert_eq!(
                mmap_file_access(shared, want_write, OpenFlags::WRONLY),
                Err(LxError::EACCES),
                "shared={} write={}",
                shared,
                want_write
            );
        }
        assert_eq!(mmap_file_access(false, false, OpenFlags::RDONLY), Ok(()));
        assert_eq!(
            mmap_file_access(true, true, OpenFlags::RDWR | OpenFlags::APPEND),
            Ok(())
        );
    }
}

#[cfg(test)]
mod mmap_len_tests {
    //! `do_mmap`'s answer to a length it will not map.

    use super::*;

    /// Zero is `EINVAL` (the empty file every whole-file reader meets), the
    /// cap is `ENOMEM`, and everything between maps.
    #[test]
    fn a_zero_length_is_einval_and_only_the_cap_is_enomem() {
        assert_eq!(mmap_len_check(0), Err(LxError::EINVAL));
        assert_eq!(mmap_len_check(1), Ok(()));
        assert_eq!(mmap_len_check(PAGE_SIZE), Ok(()));
        assert_eq!(mmap_len_check(MAX_MMAP_LEN), Ok(()));
        assert_eq!(mmap_len_check(MAX_MMAP_LEN + 1), Err(LxError::ENOMEM));
        assert_eq!(mmap_len_check(usize::MAX), Err(LxError::ENOMEM));
    }
}

#[cfg(test)]
mod mmap_prot_tests {
    use super::*;
    use zircon_object::vm::MMUFlags;

    #[test]
    fn prot_none_is_no_access() {
        let flags = MmapProt::empty().to_flags();
        assert!(
            !flags.contains(MMUFlags::READ) && !flags.contains(MMUFlags::WRITE),
            "PROT_NONE must not install RW (got {:?})",
            flags
        );
        assert!(
            !flags.intersects(MMUFlags::READ | MMUFlags::WRITE | MMUFlags::EXECUTE),
            "PROT_NONE must have no access bits: {:?}",
            flags
        );
        assert!(
            flags.contains(MMUFlags::USER),
            "PROT_NONE must keep USER so a later mprotect(RW) can raise access (got {:?})",
            flags
        );
    }

    #[test]
    fn prot_read_is_user_readable_not_writable() {
        let flags = MmapProt::READ.to_flags();
        assert!(flags.contains(MMUFlags::USER | MMUFlags::READ));
        assert!(!flags.contains(MMUFlags::WRITE));
        assert!(!flags.contains(MMUFlags::EXECUTE));
    }

    #[test]
    fn prot_rx_does_not_imply_write() {
        let flags = (MmapProt::READ | MmapProt::EXEC).to_flags();
        assert!(flags.contains(MMUFlags::USER | MMUFlags::READ | MMUFlags::EXECUTE));
        assert!(!flags.contains(MMUFlags::WRITE));
    }
}

/// mlockall(2) `MCL_CURRENT`: lock all currently mapped pages.
const MCL_CURRENT: usize = 1;
/// mlockall(2) `MCL_FUTURE`: lock everything mapped from now on.
const MCL_FUTURE: usize = 2;
/// mlockall(2) `MCL_ONFAULT`: lock pages as they are faulted in.
const MCL_ONFAULT: usize = 4;

/// Validate an mlockall(2) `flags` word. Pure, so the matrix is unit-testable:
/// empty or unknown flags are `EINVAL`, and `MCL_ONFAULT` is only meaningful
/// alongside `MCL_CURRENT`/`MCL_FUTURE` — the exact rules from the man page.
fn check_mlockall_flags(flags: usize) -> SysResult {
    if flags == 0 || flags & !(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT) != 0 {
        return Err(LxError::EINVAL);
    }
    if flags == MCL_ONFAULT {
        return Err(LxError::EINVAL);
    }
    Ok(0)
}

#[cfg(test)]
mod mlockall_flag_tests {
    use super::*;

    #[test]
    fn valid_combinations_are_accepted() {
        for flags in [
            MCL_CURRENT,
            MCL_FUTURE,
            MCL_CURRENT | MCL_FUTURE,
            MCL_CURRENT | MCL_ONFAULT,
            MCL_FUTURE | MCL_ONFAULT,
            MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT,
        ] {
            assert_eq!(check_mlockall_flags(flags), Ok(0), "{flags:#x}");
        }
    }

    #[test]
    fn empty_lone_onfault_and_unknown_bits_are_einval() {
        for flags in [0, MCL_ONFAULT, 8, MCL_CURRENT | 16, usize::MAX] {
            assert_eq!(
                check_mlockall_flags(flags),
                Err(LxError::EINVAL),
                "{flags:#x}"
            );
        }
    }
}

#[cfg(test)]
mod user_range_tests {
    //! `(addr, len)` is the pair every memory syscall takes straight from
    //! userspace, and `roundup_pages` -- the first thing each of them did with
    //! it -- rounds with a wrapping add. So the lengths that name almost the
    //! whole address space are precisely the ones that arrived looking like an
    //! empty range, and an empty range is what every one of these calls answers
    //! with success.

    use super::{roundup_pages, user_range, UserRange, USER_ASPACE_END};

    const PAGE: usize = 4096;

    #[test]
    fn a_length_of_zero_names_no_page() {
        assert_eq!(user_range(0x1000, 0), UserRange::Empty);
        assert_eq!(user_range(0, 0), UserRange::Empty);
        assert_eq!(user_range(USER_ASPACE_END, 0), UserRange::Empty);
    }

    #[test]
    fn a_length_rounds_up_to_whole_pages() {
        assert_eq!(user_range(0x1000, 1), UserRange::Pages(PAGE));
        assert_eq!(user_range(0x1000, PAGE), UserRange::Pages(PAGE));
        assert_eq!(user_range(0x1000, PAGE + 1), UserRange::Pages(2 * PAGE));
    }

    #[test]
    fn every_length_whose_round_up_wraps_is_out_of_range_and_not_empty() {
        // `ceil` is `x.wrapping_add(align - 1) / align`, so the last page's
        // worth of lengths -- 4095 of them -- all round to zero. Each one names
        // essentially the whole address space, and each one used to walk zero
        // pages and report success.
        for len in (usize::MAX - 4094)..=usize::MAX {
            assert_eq!(roundup_pages(len), 0, "roundup_pages({:#x})", len);
            assert_eq!(
                user_range(0x1000, len),
                UserRange::Beyond,
                "len = {:#x}",
                len
            );
        }
    }

    #[test]
    fn the_largest_length_that_rounds_up_cleanly_still_runs_off_the_end() {
        // One below the wrapping set, the round-up is exact -- 2^64 - 4096 --
        // and it is the addition that goes over. A release kernel wraps it to
        // an end below `addr`; a test build panics on the same line.
        let len = usize::MAX - 4095;
        assert_eq!(roundup_pages(len), len);
        assert_eq!(0x1000usize.checked_add(len), None);
        assert_eq!(user_range(0x1000, len), UserRange::Beyond);
    }

    #[test]
    fn a_range_that_ends_exactly_at_the_top_of_the_address_space_is_taken() {
        assert_eq!(
            user_range(USER_ASPACE_END - PAGE, PAGE),
            UserRange::Pages(PAGE)
        );
    }

    #[test]
    fn a_range_that_ends_one_byte_past_the_top_is_not() {
        assert_eq!(
            user_range(USER_ASPACE_END - PAGE, PAGE + 1),
            UserRange::Beyond
        );
        assert_eq!(user_range(USER_ASPACE_END, PAGE), UserRange::Beyond);
    }

    #[test]
    fn a_length_the_round_up_survives_can_still_be_absurd() {
        // 2^63 rounds cleanly and adds cleanly; what refuses it is the address
        // space. `mincore` sized a `Vec` from exactly this: 2^51 pages, from an
        // allocation that cannot fail, only abort.
        assert_eq!(roundup_pages(1 << 63), 1 << 63);
        assert!(0x1000usize.checked_add(1 << 63).is_some());
        assert_eq!(user_range(0x1000, 1 << 63), UserRange::Beyond);
    }
}

#[cfg(test)]
mod mm_range_tests {
    //! The same hostile `(addr, len)` asked of every memory syscall that takes
    //! one. The errnos differ between them -- that is the man pages, not an
    //! accident -- but "success over a range I never looked at" is not among
    //! them anywhere.

    use super::{
        brk_target, madvise_args, mincore_args, mlock_args, mprotect_args, mremap_args, msync_args,
        munmap_args, roundup_pages, USER_ASPACE_END,
    };
    use alloc::vec::Vec;
    use linux_object::error::LxError;

    const PAGE: usize = 4096;
    const MS_ASYNC: usize = 1;
    const MS_INVALIDATE: usize = 2;
    const MS_SYNC: usize = 4;
    const MADV_NORMAL: usize = 0;
    const MADV_DONTNEED: usize = 4;

    /// The lengths that used to arrive looking like an empty range, plus the
    /// one whose round-up is clean and whose addition is not, plus one that
    /// survives both and is still bigger than the address space.
    const HOSTILE: [usize; 4] = [usize::MAX, usize::MAX - 4094, usize::MAX - 4095, 1 << 63];

    /// Every taker of a `(addr, len)` pair, by name, so a syscall left out of
    /// the rule shows up as a name rather than as a line number.
    fn answers(addr: usize, len: usize) -> Vec<(&'static str, Result<(), LxError>)> {
        vec![
            ("munmap", munmap_args(addr, len).map(|_| ())),
            ("mprotect", mprotect_args(addr, len).map(|_| ())),
            ("msync", msync_args(addr, len, MS_SYNC).map(|_| ())),
            ("mincore", mincore_args(addr, len).map(|_| ())),
            (
                "madvise",
                madvise_args(addr, len, MADV_DONTNEED).map(|_| ()),
            ),
            ("mlock", mlock_args(addr, len).map(|_| ())),
            ("mremap", mremap_args(addr, len, len).map(|_| ())),
        ]
    }

    #[test]
    fn no_memory_syscall_takes_a_length_that_names_the_whole_address_space() {
        for len in HOSTILE {
            for (name, answer) in answers(0x1000, len) {
                assert!(answer.is_err(), "{} accepted len={:#x}", name, len);
            }
        }
    }

    #[test]
    fn each_one_refuses_it_the_way_its_own_man_page_does() {
        let len = usize::MAX;
        assert_eq!(munmap_args(0x1000, len), Err(LxError::EINVAL));
        assert_eq!(mprotect_args(0x1000, len), Err(LxError::ENOMEM));
        assert_eq!(msync_args(0x1000, len, MS_SYNC), Err(LxError::ENOMEM));
        assert_eq!(mincore_args(0x1000, len), Err(LxError::ENOMEM));
        assert_eq!(
            madvise_args(0x1000, len, MADV_DONTNEED),
            Err(LxError::EINVAL)
        );
        assert_eq!(mlock_args(0x1000, len), Err(LxError::ENOMEM));
        assert_eq!(mremap_args(0x1000, len, PAGE), Err(LxError::EINVAL));
        assert_eq!(mremap_args(0x1000, PAGE, len), Err(LxError::EINVAL));
    }

    #[test]
    fn an_empty_range_is_a_bad_argument_only_to_the_two_that_must_move_something() {
        assert_eq!(munmap_args(0x1000, 0), Err(LxError::EINVAL));
        assert_eq!(mremap_args(0x1000, 0, PAGE), Err(LxError::EINVAL));
        assert_eq!(mremap_args(0x1000, PAGE, 0), Err(LxError::EINVAL));
        // The rest answer an honest zero. `mprotect` in particular: Linux
        // returns before it looks at a single VMA, and the VMAR below refuses
        // a zero-length protect, so this is the difference between 0 and a
        // logged incomplete transition -- or a hard EINVAL under W^X enforce.
        assert_eq!(mprotect_args(0x1000, 0), Ok(None));
        assert_eq!(msync_args(0x1000, 0, MS_SYNC), Ok(None));
        assert_eq!(mincore_args(0x1000, 0), Ok(0));
        assert_eq!(madvise_args(0x1000, 0, MADV_DONTNEED), Ok(None));
        assert_eq!(mlock_args(0x1000, 0), Ok(None));
    }

    #[test]
    fn an_unaligned_address_is_einval_everywhere_but_mlock() {
        for (name, answer) in answers(0x1001, PAGE) {
            if name == "mlock" {
                // mlock(2) rounds the address DOWN instead of refusing it.
                assert!(answer.is_ok(), "mlock refused an unaligned address");
                continue;
            }
            assert_eq!(
                answer,
                Err(LxError::EINVAL),
                "{} on an unaligned address",
                name
            );
        }
    }

    #[test]
    fn mlock_rounds_the_address_down_and_stretches_the_length_over_it() {
        // `mlock(0x1001, PAGE)` covers two pages on Linux, not one:
        // `len = PAGE_ALIGN(len + offset_in_page(start))`.
        assert_eq!(mlock_args(0x1001, PAGE), Ok(Some((0x1000, 0x3000))));
        assert_eq!(mlock_args(0x1000, PAGE), Ok(Some((0x1000, 0x2000))));
        assert_eq!(mlock_args(0x1fff, 1), Ok(Some((0x1000, 0x2000))));
    }

    #[test]
    fn mlock_bounds_the_sum_and_not_only_the_length() {
        // The old order rounded AFTER the addition -- `checked_add(len).map(
        // roundup_pages)` -- so a sum of `usize::MAX` gave an end of zero,
        // which is below every start: the walk that proves the range is mapped
        // ran zero times and `mlock` reported the whole space locked.
        let addr = 0x1000;
        assert_eq!(roundup_pages(addr + (usize::MAX - addr)), 0);
        assert_eq!(mlock_args(addr, usize::MAX - addr), Err(LxError::ENOMEM));
    }

    #[test]
    fn the_last_page_of_the_address_space_is_still_usable() {
        let top = USER_ASPACE_END - PAGE;
        assert_eq!(munmap_args(top, PAGE), Ok(PAGE));
        assert_eq!(mprotect_args(top, PAGE), Ok(Some(PAGE)));
        assert_eq!(msync_args(top, PAGE, MS_SYNC), Ok(Some(PAGE)));
        assert_eq!(mincore_args(top, PAGE), Ok(1));
        assert_eq!(madvise_args(top, PAGE, MADV_DONTNEED), Ok(Some(PAGE)));
        assert_eq!(mlock_args(top, PAGE), Ok(Some((top, USER_ASPACE_END))));
        assert_eq!(mremap_args(top, PAGE, PAGE), Ok((PAGE, PAGE)));
    }

    #[test]
    fn mincore_counts_pages_and_not_bytes() {
        assert_eq!(mincore_args(0x1000, 1), Ok(1));
        assert_eq!(mincore_args(0x1000, PAGE), Ok(1));
        assert_eq!(mincore_args(0x1000, PAGE + 1), Ok(2));
    }

    #[test]
    fn msync_keeps_the_flag_rules_it_already_had() {
        assert_eq!(msync_args(0x1000, PAGE, 8), Err(LxError::EINVAL));
        assert_eq!(
            msync_args(0x1000, PAGE, MS_SYNC | MS_ASYNC),
            Err(LxError::EINVAL)
        );
        assert_eq!(msync_args(0x1000, PAGE, 0), Ok(Some(PAGE)));
        assert_eq!(msync_args(0x1000, PAGE, MS_INVALIDATE), Ok(Some(PAGE)));
    }

    #[test]
    fn madvise_keeps_rejecting_advice_it_does_not_know() {
        assert_eq!(madvise_args(0x1000, PAGE, 7), Err(LxError::EINVAL));
        assert_eq!(madvise_args(0x1000, PAGE, MADV_NORMAL), Ok(Some(PAGE)));
    }

    #[test]
    fn brk_keeps_the_old_break_for_one_it_cannot_reach() {
        let heap = 0x40_0000;
        // The whole of it in one line: `roundup_pages(usize::MAX)` is 0, zero
        // is below the current break, so the shrink branch took `brk(-1)` and
        // moved the program's heap end to address 0 -- under its own text.
        assert_eq!(roundup_pages(usize::MAX), 0);
        assert_eq!(brk_target(usize::MAX, heap), None);
        assert_eq!(brk_target(USER_ASPACE_END + 1, heap), None);
        assert_eq!(brk_target(heap - 1, heap), None);
    }

    #[test]
    fn brk_rounds_a_break_it_can_reach_up_to_a_page() {
        let heap = 0x40_0000;
        assert_eq!(brk_target(heap, heap), Some(heap));
        assert_eq!(brk_target(heap + 1, heap), Some(heap + PAGE));
        assert_eq!(brk_target(USER_ASPACE_END, heap), Some(USER_ASPACE_END));
    }
}

#[cfg(test)]
mod brk_plan_tests {
    //! The number `brk(2)` stores and returns, against the page it maps.

    use super::*;

    const HEAP: usize = 0x40_0000;

    /// `sbrk(100)`: the break moves by 100, the mapping by a page.
    #[test]
    fn the_break_stored_is_the_one_asked_for_not_the_page() {
        let plan = brk_plan(HEAP + 100, HEAP, HEAP).unwrap();
        assert_eq!(plan.brk, HEAP + 100);
        assert_eq!(plan.mapped_end, HEAP + PAGE_SIZE);
        assert_eq!(plan.moved, BrkMove::Grow);
    }

    /// A move that stays on the last page, up or down, maps nothing and
    /// still moves the break.
    #[test]
    fn a_move_within_the_last_page_is_bookkeeping_only() {
        let plan = brk_plan(HEAP + 200, HEAP + 100, HEAP).unwrap();
        assert_eq!((plan.brk, plan.moved), (HEAP + 200, BrkMove::WithinPage));
        let plan = brk_plan(HEAP + 50, HEAP + 100, HEAP).unwrap();
        assert_eq!((plan.brk, plan.moved), (HEAP + 50, BrkMove::WithinPage));
        assert_eq!(plan.mapped_end, HEAP + PAGE_SIZE);
        // Exactly on the page boundary, from an unaligned break on it.
        let plan = brk_plan(HEAP + PAGE_SIZE, HEAP + 100, HEAP).unwrap();
        assert_eq!(plan.moved, BrkMove::WithinPage);
    }

    /// Across pages, the mapping shrinks or grows, and the break is still
    /// the number asked for.
    #[test]
    fn across_a_page_the_mapping_moves_with_it() {
        let plan = brk_plan(HEAP + 100, HEAP + 2 * PAGE_SIZE, HEAP).unwrap();
        assert_eq!((plan.brk, plan.moved), (HEAP + 100, BrkMove::Shrink));
        assert_eq!(plan.mapped_end, HEAP + PAGE_SIZE);
        let plan = brk_plan(HEAP + PAGE_SIZE + 1, HEAP + 100, HEAP).unwrap();
        assert_eq!(plan.moved, BrkMove::Grow);
        assert_eq!(plan.mapped_end, HEAP + 2 * PAGE_SIZE);
        assert_eq!(brk_plan(HEAP - 1, HEAP, HEAP), None);
    }
}

#[cfg(test)]
mod mm_walk_tests {
    //! `msync`, `mlock`/`munlock` and `mincore` all answer the same question --
    //! "is every page of this range mapped?" -- and all three walked it
    //! themselves. A real root VMAR fits in a unit test in this crate, so the
    //! walk can be asked rather than reasoned about.

    use super::{mincore_residency, mlock_args, walk_mapped, PAGE_SIZE};
    use alloc::sync::Arc;
    use linux_object::error::LxError;
    use zircon_object::vm::{MMUFlags, VmAddressRegion, VmObject};

    /// A root VMAR with `pages` pages mapped at `base`, and nothing after
    /// them.
    ///
    /// Every caller passes a `base` of its own. Under libos a root VMAR is
    /// backed by the *host* process's address space, so two of these tests
    /// running at once at the same base fight over the same host mapping --
    /// and `cargo test` runs them at once while the CI job pins
    /// `--test-threads=1`, which is the one arrangement where that never
    /// shows. A base is also well clear of `vm.mmap_min_addr`, which is what a
    /// low test mapping runs into on a CI runner and not here.
    fn mapped(base: usize, pages: usize) -> (Arc<VmAddressRegion>, usize) {
        assert!(base >= 0x100_0000 && base.is_multiple_of(PAGE_SIZE));
        let vmar = VmAddressRegion::new_root();
        let addr = vmar.addr() + base;
        // `map_range: false` is what makes this an anonymous `mmap` and not
        // something else: `VmAddressRegion::map`/`map_at` pass `true` and
        // install a PTE for every page up front, so `mincore` over one of
        // those answers "resident" for pages nobody has ever touched and the
        // distinction it exists to draw never appears.
        vmar.map_ext_min(
            Some(base),
            VmObject::new_paged(pages),
            0,
            pages * PAGE_SIZE,
            MMUFlags::RXW,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
            false,
            false,
            false,
            0,
        )
        .unwrap();
        assert!(
            vmar.find_mapping(addr + pages * PAGE_SIZE).is_none(),
            "the page after the mapping is mapped, so the hole tests below \
             would pass for the wrong reason"
        );
        (vmar, addr)
    }

    #[test]
    fn a_range_that_is_mapped_end_to_end_is_accepted() {
        let (vmar, addr) = mapped(0x100_0000, 3);
        assert_eq!(walk_mapped(&vmar, addr, addr + 3 * PAGE_SIZE), Ok(0));
    }

    #[test]
    fn the_walk_stops_at_the_first_page_that_is_not_mapped() {
        let (vmar, addr) = mapped(0x200_0000, 2);
        assert_eq!(
            walk_mapped(&vmar, addr, addr + 3 * PAGE_SIZE),
            Err(LxError::ENOMEM)
        );
        // And a range that starts in the hole, not just one that ends there.
        assert_eq!(
            walk_mapped(&vmar, addr + 2 * PAGE_SIZE, addr + 3 * PAGE_SIZE),
            Err(LxError::ENOMEM)
        );
    }

    #[test]
    fn an_empty_walk_is_vacuously_true() {
        let (vmar, addr) = mapped(0x300_0000, 1);
        assert_eq!(walk_mapped(&vmar, addr, addr), Ok(0));
        // Including one whose end is below its start, which is what the
        // wrapped `addr + roundup_pages(len)` used to hand it.
        assert_eq!(walk_mapped(&vmar, addr, 0), Ok(0));
    }

    #[test]
    fn mlock_validates_the_page_its_own_rounding_added() {
        // One page mapped. `mlock(addr + 1, PAGE_SIZE)` names bytes in two
        // pages, and mlock(2) rounds to cover both -- so the second one, which
        // is a hole, has to make this ENOMEM. Bounding `roundup_pages(len)`
        // instead of the sum would have let it through.
        let (vmar, addr) = mapped(0x400_0000, 1);
        let (start, end) = mlock_args(addr + 1, PAGE_SIZE).unwrap().unwrap();
        assert_eq!((start, end), (addr, addr + 2 * PAGE_SIZE));
        assert_eq!(walk_mapped(&vmar, start, end), Err(LxError::ENOMEM));

        // The same call one byte shorter stays inside the page it started in.
        let (start, end) = mlock_args(addr + 1, PAGE_SIZE - 1).unwrap().unwrap();
        assert_eq!((start, end), (addr, addr + PAGE_SIZE));
        assert_eq!(walk_mapped(&vmar, start, end), Ok(0));
    }

    #[test]
    fn mincore_calls_a_touched_page_resident_and_an_untouched_one_not() {
        // Nothing has been touched yet: the mapping exists, the pages are
        // demand-paged, and `mincore` draws exactly that distinction -- which
        // is what allocators use it for.
        let (vmar, addr) = mapped(0x500_0000, 2);
        assert_eq!(mincore_residency(&vmar, addr, 2), Ok(vec![0, 0]));
        vmar.handle_page_fault(addr, MMUFlags::READ).unwrap();
        assert_eq!(mincore_residency(&vmar, addr, 1), Ok(vec![1]));
        // Its neighbour reads resident too, and that is not a bug: a fault
        // here maps the pages around it (`fault_around`), so one touch really
        // does leave both present, and `mincore` is reporting what the page
        // tables say. The half that matters is the one above -- a mapping
        // nobody has touched reads as absent -- because that is the
        // distinction `mincore` exists to draw and the one allocators probe
        // for.
        assert_eq!(mincore_residency(&vmar, addr, 2), Ok(vec![1, 1]));
    }

    #[test]
    fn mincore_gives_up_at_the_first_page_that_is_not_mapped() {
        let (vmar, addr) = mapped(0x700_0000, 2);
        assert_eq!(mincore_residency(&vmar, addr, 2).map(|v| v.len()), Ok(2));
        assert_eq!(mincore_residency(&vmar, addr, 3), Err(LxError::ENOMEM));
    }

    #[test]
    fn mincore_says_enomem_at_the_first_hole_instead_of_reserving_for_the_rest() {
        // The page count here is what `mincore(0, TASK_SIZE, v)` produces after
        // the range check: 2^35 pages. Reserving one byte per page for it asks
        // the kernel heap for 32 GiB, from an allocation that cannot fail --
        // only abort the machine. The walk gives up on the first page.
        let (vmar, addr) = mapped(0x600_0000, 1);
        assert_eq!(
            mincore_residency(&vmar, addr, 1 << 35),
            Err(LxError::ENOMEM)
        );
    }
}

#[cfg(test)]
mod mmap_flag_tests {
    //! `mmap`'s flag word carries two things that are not bitmasks: the kind
    //! of mapping, which is a small enum in the low four bits, and
    //! `MAP_FIXED_NOREPLACE`, which was not named at all and so was dropped.

    use super::*;

    /// `MAP_FIXED_NOREPLACE` means "exactly here, or fail". Dropped by
    /// `from_bits_truncate`, it left a plain hint: the mapping went wherever
    /// there was room and the call reported success, which is the one
    /// outcome the flag exists to rule out.
    #[test]
    fn fixed_noreplace_is_a_placement_and_not_a_hint() {
        let raw = MmapFlags::from_bits_truncate(0x10_0000);
        assert!(
            raw.contains(MmapFlags::FIXED_NOREPLACE),
            "the bit is being dropped again"
        );
        assert_eq!(mmap_placement(raw), Placement::FixedNoReplace);
    }

    /// It implies `MAP_FIXED` and wins over it: `do_mmap` sets the FIXED bit
    /// itself, and then refuses to replace anything all the same.
    #[test]
    fn fixed_noreplace_wins_over_fixed() {
        assert_eq!(
            mmap_placement(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE),
            Placement::FixedNoReplace
        );
        assert_eq!(mmap_placement(MmapFlags::FIXED), Placement::Fixed);
        assert_eq!(mmap_placement(MmapFlags::empty()), Placement::Hint);
        assert_eq!(mmap_placement(MmapFlags::ANONYMOUS), Placement::Hint);
    }

    /// The bit is the one the uapi headers give it. If it drifts, the flag
    /// goes back to being dropped and nothing else complains.
    #[test]
    fn the_placement_bits_are_the_ones_userspace_sends() {
        assert_eq!(MmapFlags::FIXED.bits(), 0x10);
        assert_eq!(MmapFlags::FIXED_NOREPLACE.bits(), 0x10_0000);
    }

    /// `MAP_TYPE` is an enum, not a bitmask. Read with `contains(SHARED)` a
    /// word naming no kind at all came out private, which is a mapping the
    /// caller never asked for.
    #[test]
    fn a_mapping_that_names_no_kind_is_refused() {
        assert_eq!(mmap_shared(0, true), Err(LxError::EINVAL));
        assert_eq!(mmap_shared(0, false), Err(LxError::EINVAL));
        // ... including when the rest of the word is full of valid flags.
        assert_eq!(
            mmap_shared(MMAP_ANONYMOUS | MmapFlags::FIXED.bits(), true),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn shared_and_private_are_the_two_ordinary_answers() {
        for anonymous in [true, false] {
            assert_eq!(mmap_shared(MAP_SHARED, anonymous), Ok(true));
            assert_eq!(mmap_shared(MAP_PRIVATE, anonymous), Ok(false));
            // The high bits of the word are not part of the kind.
            assert_eq!(
                mmap_shared(MAP_PRIVATE | MMAP_ANONYMOUS | 0x10_0000, anonymous),
                Ok(false)
            );
        }
    }

    /// `MAP_SHARED_VALIDATE` is the value 3, not a third bit, and it is a
    /// file-mapping answer: Linux's anonymous arm is a second `switch` that
    /// knows only `MAP_SHARED` and `MAP_PRIVATE`.
    #[test]
    fn shared_validate_is_a_file_mapping_answer_only() {
        assert_eq!(mmap_shared(MAP_SHARED_VALIDATE, false), Ok(true));
        assert_eq!(mmap_shared(MAP_SHARED_VALIDATE, true), Err(LxError::EINVAL));
    }

    /// `MAP_SHARED_VALIDATE` validates: a flag outside `LEGACY_MAP_MASK`
    /// (`MAP_SYNC`, `MAP_FIXED_NOREPLACE`, a bit nobody has defined) is
    /// `EOPNOTSUPP`, which is the answer the kind exists to give and the one
    /// a `MAP_SYNC` probe reads to learn the mapping would not be what it
    /// asked for.
    #[test]
    fn shared_validate_refuses_the_flags_it_does_not_know() {
        const MAP_SYNC: usize = 0x80000;
        const MAP_FIXED_NOREPLACE: usize = 0x100000;
        for unknown in [MAP_SYNC, MAP_FIXED_NOREPLACE, 1 << 25] {
            assert_eq!(
                shared_validate_flags(MAP_SHARED_VALIDATE | unknown),
                Err(LxError::EOPNOTSUPP),
                "{:#x}",
                unknown
            );
            // Plain MAP_SHARED and MAP_PRIVATE ignore the same bits, as they
            // always did: only the validating kind looks.
            assert_eq!(shared_validate_flags(MAP_SHARED | unknown), Ok(()));
            assert_eq!(shared_validate_flags(MAP_PRIVATE | unknown), Ok(()));
        }
    }

    /// The legacy flag word passes under `MAP_SHARED_VALIDATE` whole: the
    /// kind must not turn an ordinary `MAP_SHARED|MAP_FIXED|MAP_POPULATE`
    /// into `EOPNOTSUPP`, or the programs that validate get nothing at all.
    #[test]
    fn shared_validate_takes_the_whole_legacy_word() {
        assert_eq!(shared_validate_flags(MAP_SHARED_VALIDATE), Ok(()));
        assert_eq!(
            shared_validate_flags(MAP_SHARED_VALIDATE | LEGACY_MAP_MASK),
            Ok(())
        );
        for legacy in [
            MAP_FIXED,
            MAP_POPULATE,
            MAP_NORESERVE,
            MAP_HUGETLB | MAP_HUGE_2MB,
            MAP_LOCKED | MAP_STACK,
        ] {
            assert_eq!(
                shared_validate_flags(MAP_SHARED_VALIDATE | legacy),
                Ok(()),
                "{:#x}",
                legacy
            );
        }
    }

    /// A kind in the low four bits that names nothing is `EINVAL`, not
    /// whatever its bits happen to spell.
    #[test]
    fn a_kind_that_does_not_exist_is_refused() {
        for kind in 4..=15usize {
            assert_eq!(
                mmap_shared(kind, false),
                Err(LxError::EINVAL),
                "MAP_TYPE {}",
                kind
            );
        }
    }
}

#[cfg(test)]
mod mprotect_prot_tests {
    //! `mprotect` validates its protection word and `mmap` does not. Both
    //! used to truncate, so `mprotect` accepted a protection this kernel
    //! cannot give and told the caller it had it.

    use super::*;

    #[test]
    fn the_three_ordinary_protections_come_through() {
        assert_eq!(mprotect_prot(0), Ok(MmapProt::empty()));
        assert_eq!(mprotect_prot(1), Ok(MmapProt::READ));
        assert_eq!(
            mprotect_prot(7),
            Ok(MmapProt::READ | MmapProt::WRITE | MmapProt::EXEC)
        );
    }

    /// A bit `arch_validate_prot` does not know is `EINVAL`. Truncating
    /// turned `PROT_READ | 0x40` into a read-only mapping.
    #[test]
    fn a_protection_bit_that_does_not_exist_is_refused() {
        for stray in [0x10usize, 0x40, 1 << 30, 1 << 62] {
            assert_eq!(mprotect_prot(stray), Err(LxError::EINVAL), "{:#x}", stray);
            assert_eq!(
                mprotect_prot(stray | 1),
                Err(LxError::EINVAL),
                "{:#x} with PROT_READ",
                stray
            );
        }
    }

    /// `PROT_SEM` is accepted and does nothing here; the growth bits are
    /// accepted one at a time and refused together, which is the check Linux
    /// runs before it masks them off.
    #[test]
    fn the_flags_linux_accepts_but_does_not_act_on_are_accepted() {
        assert_eq!(mprotect_prot(PROT_SEM | 1), Ok(MmapProt::READ));
        assert_eq!(mprotect_prot(PROT_GROWSDOWN | 1), Ok(MmapProt::READ));
        assert_eq!(mprotect_prot(PROT_GROWSUP | 1), Ok(MmapProt::READ));
        assert_eq!(
            mprotect_prot(PROT_GROWSDOWN | PROT_GROWSUP | 1),
            Err(LxError::EINVAL)
        );
    }
}
