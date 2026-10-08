//! Physical frame allocation and kernel heap on x86_64 bare metal.
//!
//! Two pools on purpose:
//! - **Kernel heap** (`GlobalAlloc`): fixed BSS buddy pool for `Vec`/strings/smoltcp.
//! - **Frame allocator** (bitmap): UEFI free RAM for DMA pages and process VM.
//!
//! Do not `transfer()` raw UEFI regions into the kernel heap at early boot: the buddy
//! allocator touches those pages and can hang before the kernel reaches 60% progress.

use bitmap_allocator::BitAlloc;
use core::ops::Range;
#[cfg(feature = "mem-debug")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicUsize, Ordering};
use kernel_hal::sync::Mutex;
use kernel_hal::PhysAddr;

static TOTAL_MEMORY: AtomicUsize = AtomicUsize::new(0);
static HEAP_USED: AtomicUsize = AtomicUsize::new(0);
static FRAMES_USED: AtomicUsize = AtomicUsize::new(0);

type FrameAlloc = bitmap_allocator::BitAlloc16M; // max 64G

const PAGE_BITS: usize = 12;
const MAX_MANAGED_PADDR_EXCLUSIVE: PhysAddr = 1usize << (PAGE_BITS + 24); // 64GiB

static FRAME_ALLOCATOR: Mutex<FrameAlloc> = Mutex::new(FrameAlloc::DEFAULT);

// ─── DEBUG: detector de doble-uso / use-after-free de frames físicos ───────────
//
// Bitset "frame actualmente asignado", paralelo al bitmap allocator. Cubre hasta
// 4 GiB (suficiente para el bench de 1 GiB). Empieza todo a 0 (libre) porque el
// allocator solo reparte frames libres y solo marcamos al repartir.
//   - DOUBLE ALLOC: el allocator devuelve un frame que aún teníamos marcado como
//     asignado -> dos dueños del mismo frame físico (la causa de la corrupción).
//   - DOUBLE/UNTRACKED FREE: se libera un frame que no estaba asignado ->
//     liberación prematura / doble free.
//
// Gated (feature `mem-debug`): un RMW SeqCst por frame en alloc Y free, en la
// ruta de commit de página — el camino más caliente del sistema de memoria.
#[cfg(feature = "mem-debug")]
const TRACK_MAX_FRAMES: usize = 1 << 20; // 4 GiB / 4 KiB
#[cfg(feature = "mem-debug")]
const TRACK_WORDS: usize = TRACK_MAX_FRAMES / 64;
#[cfg(feature = "mem-debug")]
static FRAME_ALLOCATED: [AtomicU64; TRACK_WORDS] = [const { AtomicU64::new(0) }; TRACK_WORDS];

#[cfg(feature = "mem-debug")]
fn track_mark_alloc(idx: usize) {
    if idx >= TRACK_MAX_FRAMES {
        return;
    }
    let bit = 1u64 << (idx % 64);
    let prev = FRAME_ALLOCATED[idx / 64].fetch_or(bit, Ordering::SeqCst);
    if prev & bit != 0 {
        crate::klog_warn!(
            "[frametrack] DOUBLE ALLOC frame_idx={:#x} paddr={:#x} (dos dueños del mismo frame)",
            idx,
            idx << PAGE_BITS
        );
    }
}

#[cfg(feature = "mem-debug")]
fn track_mark_free(idx: usize) {
    if idx >= TRACK_MAX_FRAMES {
        return;
    }
    let bit = 1u64 << (idx % 64);
    let prev = FRAME_ALLOCATED[idx / 64].fetch_and(!bit, Ordering::SeqCst);
    if prev & bit == 0 {
        crate::klog_warn!(
            "[frametrack] DOUBLE/UNTRACKED FREE frame_idx={:#x} paddr={:#x} (liberación prematura)",
            idx,
            idx << PAGE_BITS
        );
    }
}

#[inline]
fn phys_addr_to_frame_idx(addr: PhysAddr) -> usize {
    addr >> PAGE_BITS
}

#[inline]
fn frame_idx_to_phys_addr(idx: usize) -> PhysAddr {
    idx << PAGE_BITS
}

/// The frames wholly inside `region`, clamped to what the bitmap can address.
///
/// Both ends round **inward**, and that is the whole point: a frame the
/// allocator hands out must be free for its whole length. Rounding the start
/// down (which `addr >> PAGE_BITS` does by itself) or the end up gives away a
/// page that straddles the region's edge -- so the page holding the tail of the
/// kernel image, or the head of whatever reserved range comes next, becomes
/// ordinary RAM and gets a second owner. Which is the bug shape this file's own
/// `reserve_active_page_table_frames` exists to undo.
///
/// On x86_64 every UEFI descriptor is page-aligned, so today this changes
/// nothing; the clamping right below it is equally defensive, and a guard that
/// rounds the wrong way is worse than no guard.
fn frames_inside(region: &Range<PhysAddr>) -> Range<usize> {
    const PAGE_SIZE: usize = 1 << PAGE_BITS;
    // Frame 0 is never handed out: a null physical address is how every
    // "allocation failed" is spelled elsewhere.
    let start = region.start.clamp(PAGE_SIZE, MAX_MANAGED_PADDR_EXCLUSIVE);
    let end = region.end.min(MAX_MANAGED_PADDR_EXCLUSIVE);
    let frame_start = phys_addr_to_frame_idx(start + PAGE_SIZE - 1);
    let frame_end = phys_addr_to_frame_idx(end);
    // An empty answer is always `0..0` and never a backwards range: callers ask
    // `is_empty()`, and a `2..1` handed to anything that iterates is a trap.
    // This also covers a backwards or empty `region`, since `start` is at least
    // one page and an `end` below it cannot reach `frame_start`.
    if frame_start >= frame_end {
        return 0..0;
    }
    frame_start..frame_end
}

pub fn insert_regions(regions: &[Range<PhysAddr>]) {
    debug!("init_frame_allocator regions: {regions:x?}");
    let mut ba = FRAME_ALLOCATOR.lock();
    for region in regions {
        let frames = frames_inside(region);
        if frames.is_empty() {
            continue;
        }
        let range_start = frame_idx_to_phys_addr(frames.start);
        let range_end = frame_idx_to_phys_addr(frames.end);
        if range_end != region.end {
            crate::klog_warn!(
                "memory: frame allocator region clipped (>64GiB): {:#x?} -> {:#x?}",
                region,
                range_start..range_end
            );
        }
        ba.insert(frames.clone());
        TOTAL_MEMORY.fetch_add(range_end - range_start, Ordering::Relaxed);
        crate::klog_info!(
            "memory: free RAM range {:#x}..{:#x} ({} MiB)",
            range_start,
            range_end,
            (range_end - range_start) / (1024 * 1024)
        );
    }
    let (frames_used, frames_total) = frame_stats();
    crate::klog_info!(
        "memory: frame allocator ready ({} MiB managed, {} KiB used)",
        frames_total / (1024 * 1024),
        frames_used / 1024
    );
}

pub fn frame_alloc(frame_count: usize, align_log2: usize) -> Option<PhysAddr> {
    // Single-frame allocations (the page-fault/commit hot path — every
    // demand-paged page in the system) MUST use the cascade fast path.
    // `alloc_contiguous(1, 0)` goes through `find_contiguous`, which probes
    // linearly FROM INDEX 0 across the already-allocated region on every call:
    // O(allocated) per allocation, quadratic overall. Measured on hardware:
    // fork()'s per-page commit degraded 114 -> 240 us/page as allocation grew,
    // turning a 107 MiB mapping into 6.4 s of frame allocation (and the fork's
    // tail into an apparent freeze). `alloc()` descends the bitmap cascade via
    // trailing_zeros — O(log) — restoring ~us-scale commits.
    let start_idx = if frame_count == 1 && align_log2 == 0 {
        FRAME_ALLOCATOR.lock().alloc()
    } else {
        FRAME_ALLOCATOR
            .lock()
            .alloc_contiguous(frame_count, align_log2)
    };
    if let Some(idx) = start_idx {
        FRAMES_USED.fetch_add(frame_count << PAGE_BITS, Ordering::Relaxed);
        #[cfg(feature = "mem-debug")]
        for i in 0..frame_count {
            track_mark_alloc(idx + i);
        }
        #[cfg(not(feature = "mem-debug"))]
        let _ = idx;
    } else {
        // DIAGNOSTIC: a frame allocation failed. For a single-frame request
        // (the paged-VMO commit path) this means physical RAM is genuinely
        // exhausted; for a multi-frame request it may instead be fragmentation
        // (no contiguous run). Printed unconditionally so we can tell a real
        // shortage apart from a code path bug when something reports ENOMEM.
        let used = FRAMES_USED.load(Ordering::Relaxed);
        let total = TOTAL_MEMORY.load(Ordering::Relaxed);
        // error! (console), not klog: physical-RAM exhaustion during a fork's
        // eager copy surfaces as a mysterious ENOMEM/stall with nothing on
        // screen — this line is the difference between diagnosing it from a
        // photo and chasing ghosts.
        log::error!(
            "frame_alloc FAILED: count={} align_log2={} | {} MiB used / {} MiB managed",
            frame_count,
            align_log2,
            used / (1024 * 1024),
            total / (1024 * 1024),
        );
    }
    let ret = start_idx.map(frame_idx_to_phys_addr);
    trace!(
        "frame_alloc_contiguous(): {ret:x?} ~ {end_ret:x?}, align_log2={align_log2}",
        // The end of the RANGE, so the count has to become bytes first. It used
        // to be `x + frame_count`, a frame count added to a physical address:
        // for the single-frame requests that are most of this line's traffic
        // the end read one byte past the start, and for a 512-frame run it read
        // 512 bytes past it instead of 2 MiB. This line is what one reads to
        // see which runs the allocator is handing out.
        end_ret = ret.map(|x| x + (frame_count << PAGE_BITS)),
    );
    ret
}

pub fn frame_dealloc(target: PhysAddr) {
    trace!("frame_dealloc(): {target:x}");
    let idx = phys_addr_to_frame_idx(target);
    // Marcar libre ANTES de devolverlo al allocator: si es un doble-free, lo
    // detectamos antes de reinsertarlo.
    #[cfg(feature = "mem-debug")]
    track_mark_free(idx);
    // DEBUG (feature `mem-debug`): envenenar el frame con 0x5A al liberarlo. Si
    // un proceso lee este patrón en su memoria (fault a una dirección
    // ~0x5a5a5a5a...), está leyendo un frame YA LIBERADO -> PTE rancia
    // (free-sin-unmap / use-after-free).
    //
    // Resolver la dirección del frame con `phys_to_virt` en lugar de la base
    // physmap hardcodeada: en bare-metal es el mismo offset, pero en libos la
    // "memoria física" es un mmap del proceso anfitrión en otra base — escribir
    // a `0xffff_8000_…` allí provoca un SIGSEGV del propio anfitrión.
    //
    // Coste sin la feature: nada — el memset de 4 KiB por frame liberado
    // duplicaba el tráfico de memoria de todo ciclo alloc/free (el commit de
    // página ya rellena a cero al asignar).
    #[cfg(feature = "mem-debug")]
    unsafe {
        core::ptr::write_bytes(
            kernel_hal::mem::phys_to_virt(target) as *mut u8,
            0x5A,
            1 << PAGE_BITS,
        );
    }
    FRAMES_USED.fetch_sub(1 << PAGE_BITS, Ordering::Relaxed);
    FRAME_ALLOCATOR.lock().dealloc(idx);
}

pub fn frame_stats() -> (usize, usize) {
    (
        FRAMES_USED.load(Ordering::Relaxed),
        TOTAL_MEMORY.load(Ordering::Relaxed),
    )
}

/// The next table down from page-table entry `e` at `level`, or `None` when
/// there is none to descend into.
///
/// Three reasons there is none, and the third was a hole. The entry may be
/// absent (bit 0 clear). It may be a huge leaf: bit 7 is `PS` in a PDPTE
/// (1 GiB) or a PDE (2 MiB), so a set `PS` there means the entry maps memory
/// rather than naming a table -- but in the PML4 bit 7 is reserved and in a PTE
/// it is `PAT`, which is why the question is asked only below level 4 and why
/// the walk stops before reading a PTE at all. And **the child address may be
/// zero**: the walk guarded its *reservation* with `table_pa != 0` and then
/// dereferenced `phys_to_virt(table_pa)` regardless, so a present entry naming
/// frame 0 -- which nothing legitimately does, and which is exactly the shape
/// of a half-overwritten entry -- had the walk reading 4 KiB of frame 0 as 512
/// page-table entries and recursing into whatever they said. In the one
/// function whose entire job is to stop a recycled page table from triple
/// faulting the machine.
///
/// `level` is 4 for the PML4 down to 1 for a PT.
fn table_child(e: u64, level: u8) -> Option<usize> {
    const PRESENT: u64 = 1;
    /// Bit 7: `PS` in a PDPTE or PDE, `PAT` in a PTE, reserved in a PML4E.
    const HUGE: u64 = 1 << 7;
    /// Bits 51:12 of an entry are the physical address of what it names.
    const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
    if e & PRESENT == 0 {
        return None;
    }
    if level < 4 && e & HUGE != 0 {
        return None;
    }
    let child = (e & ADDR_MASK) as usize;
    if child == 0 {
        return None;
    }
    Some(child)
}

/// Reserve every frame of the CURRENTLY ACTIVE page-table tree so the frame
/// allocator can never hand them out as ordinary RAM.
///
/// Root cause of the desktop's escalating corruption: on x86_64 the kernel
/// keeps running on the page tables the BOOTLOADER built — and UEFI marks
/// that memory as reclaimable boot-services data, so `insert_regions` fed the
/// LIVE page-table frames into the allocator as free RAM. Nothing failed
/// until memory pressure (waybar's GTK heap) finally reused one of those
/// frames: translations then rotted progressively (kernel #GPs whose
/// registers held ELF file bytes, an all-idle wakeup wedge) and, once the
/// root PML4 itself was recycled, the next `activate_kernel_paging()` loaded
/// a CR3 whose tree was user data — instruction fetch of the kernel faulted,
/// the #DF handler was unfetchable for the same reason, and the machine
/// TRIPLE-FAULTED with no banner (QEMU `-d int` was needed to see it).
///
/// Must run right after `insert_regions`, before any frame allocation.
pub fn reserve_active_page_table_frames() {
    let root = kernel_hal::vm::current_vmtoken();
    if root == 0 {
        return; // libos / no paging context: nothing to reserve
    }
    let mut ba = FRAME_ALLOCATOR.lock();
    let mut reserved = 0usize;
    // Depth-first walk of the 4-level tree. Huge-page entries (PS bit) have no
    // lower table; entry bit 0 = present. Physical addresses are masked to 52
    // bits and page-aligned.
    fn walk(ba: &mut FrameAlloc, table_pa: usize, level: u8, reserved: &mut usize) {
        let idx = table_pa >> PAGE_BITS;
        if table_pa < MAX_MANAGED_PADDR_EXCLUSIVE {
            // `remove` marks the frame used regardless of current state.
            ba.remove(idx..idx + 1);
            *reserved += 1;
        }
        if level == 1 {
            return;
        }
        // SAFETY: `table_pa` is a live page-table frame -- the root from CR3,
        // or a child `table_child` accepted -- and the physmap covers every
        // page-table frame. `table_child` is what guarantees it is not zero.
        let entries = unsafe {
            core::slice::from_raw_parts(kernel_hal::mem::phys_to_virt(table_pa) as *const u64, 512)
        };
        for &e in entries {
            if let Some(child) = table_child(e, level) {
                walk(ba, child, level - 1, reserved);
            }
        }
    }
    walk(&mut ba, root, 4, &mut reserved);
    drop(ba);
    crate::klog_info!(
        "memory: reserved {} live page-table frame(s) of the boot tree (root {:#x})",
        reserved,
        root
    );
}

/// Combined usage for `/proc/meminfo` and diagnostics.
pub fn stats() -> (usize, usize) {
    let heap_used = HEAP_USED.load(Ordering::Relaxed);
    let frames_used = FRAMES_USED.load(Ordering::Relaxed);
    (
        heap_used + frames_used,
        heap_total() + TOTAL_MEMORY.load(Ordering::Relaxed),
    )
}

/// Kernel heap bytes currently allocated (diagnostics / OOM handler).
#[allow(dead_code)]
pub fn heap_used() -> usize {
    HEAP_USED.load(Ordering::Relaxed)
}

cfg_if! {
    if #[cfg(not(feature = "libos"))] {
        use buddy_system_allocator::Heap;
        // Only the bare-metal build has an impl of this trait (kernel-sync
        // gates it on target_os = "none"), and its one call site is inside
        // this same block, so importing it at file scope made the libos
        // build fail #![deny(warnings)] with an unused import.
        use lock::HeldByCurrentCpu;
        use core::{
            alloc::{GlobalAlloc, Layout},
            ops::Deref,
            ptr::NonNull,
        };

        /// Kernel heap — separate from the physical frame pool.
        ///
        /// 512 MiB: the full desktop session (labwc + lunarbg + foot + waybar,
        /// each mapping dozens of glibc shared objects through the SFS block
        /// cache) OOMed the previous 256 MiB pool while the clients were still
        /// loading. The heap is a BSS static, so this costs nothing on disk and
        /// only reserves (zero-fill) RAM at boot.
        const KERNEL_HEAP_SIZE: usize = 512 * 1024 * 1024; // 512 MiB
        const ORDER: usize = 32;

        // Invariants: `init()` carves the heap into word-sized slots
        // (`HEAP_BLOCK = KERNEL_HEAP_SIZE / size_of::<usize>()`), so the size
        // must divide evenly by the machine word — otherwise the backing static
        // would silently under-provision the allocator. It must also be
        // page-aligned, since the region is handed to the allocator wholesale.
        const _: () = assert!(KERNEL_HEAP_SIZE.is_multiple_of(core::mem::size_of::<usize>()));
        const _: () = assert!(KERNEL_HEAP_SIZE.is_multiple_of(4096));

        #[global_allocator]
        static HEAP_ALLOCATOR: LockedHeap<ORDER> = LockedHeap::<ORDER>::new();

        /// Whether this CPU may allocate right now.
        ///
        /// For fault and panic paths ONLY. A kernel fault taken INSIDE the
        /// allocator leaves the heap lock held by this very CPU, and the
        /// ticket mutex is not reentrant: anything on the fault path that
        /// allocates (a `String` from `Thread::name`, a formatted `Vec`) then
        /// waits forever for a lock it already owns. The deadlock detector
        /// named it exactly:
        ///
        ///     cpu=5 at memory_x86_64.rs:766     <- alloc, waiting
        ///     HOLDER cpu=5 at memory_x86_64.rs:849  <- dealloc, holding
        ///
        /// one CPU, both ends. `try_lock` fails in precisely that case (and
        /// while a peer holds it, where allocating is merely slow, not fatal),
        /// so a `false` here means "print only what needs no heap".
        ///
        /// No caller at the moment: the fault reporter stopped printing object
        /// names altogether rather than gating on this, which is strictly safer
        /// (it cannot allocate even when the heap happens to be free). Kept
        /// because the question is the right one for the next reporter to ask,
        /// and its non-x86 twin in `memory.rs` is kept the same way — without
        /// the attribute this breaks the build, since the crate denies warnings.
        #[allow(dead_code)]
        pub fn heap_available() -> bool {
            match HEAP_ALLOCATOR.try_lock() {
                Some(guard) => {
                    drop(guard);
                    true
                }
                None => false,
            }
        }

        /// Whether THIS cpu is already inside the kernel heap's critical
        /// section.
        ///
        /// The single most important fact about a kernel fault, and nothing
        /// printed it. The heap lock is an IRQ-off ticket mutex, so a fault
        /// taken while holding it cannot be recovered from on this CPU and
        /// cannot be reported through anything that allocates: every other CPU
        /// that reaches the allocator then spins until the deadlock detector
        /// gives up eight seconds later, which is what
        ///
        /// ```text
        /// DEADLOCK: spinlock(s) stuck >8s  cpu=7 at zCore/src/memory_x86_64.rs:1507
        /// HOLDER cpu=11 at zCore/src/memory_x86_64.rs:1507, now at
        ///   ZcoreKernelHandler::handle_page_fault+0x1002
        /// ```
        ///
        /// is: line 1507 is `self.0.lock()` in `dealloc`, so cpu 11 took the
        /// buddy's lock to free a block, faulted inside the buddy -- its free
        /// lists are intrusive, a wild write into them is a fault on the next
        /// walk -- and went into the fault handler still holding it. Reading
        /// that off three reports and a line number took a build with matching
        /// sources; the fault path can just say so.
        pub fn heap_held_by_current_cpu() -> bool {
            HEAP_ALLOCATOR.0.held_by_current_cpu()
        }

        /// The heap lock was found held **by this very CPU** at the moment we
        /// were about to block on it.
        ///
        /// The heap mutex disables interrupts for its whole critical section,
        /// so no IRQ can re-enter it; the only way back round to it on one CPU
        /// is a fault (#PF, NMI) taken *inside* `alloc`/`dealloc`, on a path
        /// that then allocates. A ticket mutex is not re-entrant, so blocking
        /// there waits, with IRQs off, for a release only this CPU could
        /// perform — the machine stops dead. That is the shape behind every
        /// "OS freezes, serial included" report in this hunt:
        ///
        ///     cpu=5 at zCore/src/memory_x86_64.rs:791        <- alloc, waiting
        ///     HOLDER cpu=5 at zCore/src/memory_x86_64.rs:874 <- dealloc, holding
        ///
        /// Refusing costs an allocation failure (or a leaked block on the free
        /// side); blocking costs the machine. Print the re-entrant call chain
        /// while we are still standing on it — those frames ARE the fault-path
        /// code that must be made allocation-free — then let the caller bail.
        /// How many allocations this guard has refused, all time. The refusal
        /// hands the caller a null pointer, so it surfaces later as
        /// `alloc_error` ("memory allocation of N bytes failed") — which reads
        /// exactly like a genuine out-of-memory. The OOM report prints this
        /// count so the two can be told apart at a glance.
        static HEAP_REENTRANCY_EVENTS: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);

        /// Allocations refused by the re-entrancy guard so far.
        pub fn heap_reentrancy_events() -> u32 {
            HEAP_REENTRANCY_EVENTS.load(core::sync::atomic::Ordering::Relaxed)
        }

        /// Both consoles: on a box with only a monitor, a serial-only report
        /// is invisible exactly when it is needed. `graphic_console_write_fmt_spin`
        /// is best-effort try_lock, so it cannot deadlock this path.
        fn emit(args: core::fmt::Arguments<'_>) {
            kernel_hal::console::serial_write_fmt_spin(args);
            kernel_hal::console::graphic_console_write_fmt_spin(args);
        }

        /// `[heap-grow]` / `[heap-reentrant]` banners. The elastic growth and
        /// the re-entrancy refuse still run; only the console chatter is off.
        const HEAP_GROW_LOG: bool = false;
        const HEAP_REENTRANT_LOG: bool = false;

        /// Blocks the allocator refused, by [`heap_regions::check`].
        static HEAP_WILD_BLOCKS: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);

        /// Wild blocks refused so far. Non-zero is a diagnosis, not a warning:
        /// something in this kernel handed the allocator a block that is not
        /// the allocator's.
        pub fn heap_wild_blocks() -> u32 {
            HEAP_WILD_BLOCKS.load(core::sync::atomic::Ordering::Relaxed)
        }

        /// Name the code that handed the allocator a block it must not touch.
        ///
        /// The backtrace is the whole value here. `what` says which side
        /// caught it, but a free list only ever reveals its corruption later,
        /// in another subsystem, on another CPU -- the call chain on this
        /// stack right now is the only place the culprit appears.
        #[cold]
        #[inline(never)]
        fn report_wild_block(
            what: &str,
            fault: heap_regions::BlockFault,
            ptr: usize,
            sz: usize,
            align: usize,
        ) {
            use core::sync::atomic::{AtomicU32, Ordering};
            HEAP_WILD_BLOCKS.fetch_add(1, Ordering::Relaxed);
            static REPORTED: AtomicU32 = AtomicU32::new(0);
            if REPORTED.fetch_add(1, Ordering::Relaxed) >= 8 {
                return;
            }
            emit(format_args!(
                "\n[heap-wild] {} ptr={:#x} size={:#x} align={} -- {}. The heap owns \
                 {} region(s){}. Leaking this block rather than splicing a wild \
                 address into the free lists; the call chain below is the code \
                 that produced it:\n",
                what,
                ptr,
                sz,
                align,
                fault.why(),
                heap_regions::registered(),
                if heap_regions::unregistered() > 0 {
                    " (and some that did not fit the registry)"
                } else {
                    ""
                },
            ));
            let mut rbp: usize;
            unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };
            for _ in 0..24 {
                if !kernel_hal::kaddr::is_kernel_stack_qword(rbp as u64) {
                    break;
                }
                let ret = unsafe { core::ptr::read_volatile((rbp + 8) as *const usize) };
                let next = unsafe { core::ptr::read_volatile(rbp as *const usize) };
                if ret == 0 {
                    break;
                }
                emit(format_args!(
                    "[heap-wild]   ret={}\n",
                    kernel_hal::ksyms::Addr(ret as u64)
                ));
                if next <= rbp {
                    break;
                }
                rbp = next;
            }
        }

        /// Free blocks the front cache found written after their free, or
        /// freed twice. See [`free_lists`].
        static HEAP_WRITTEN_AFTER_FREE: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);

        /// Broken free blocks caught so far. Like [`heap_wild_blocks`], a
        /// diagnosis: each one is code that kept using memory it had freed.
        pub fn heap_written_after_free() -> u32 {
            HEAP_WRITTEN_AFTER_FREE.load(core::sync::atomic::Ordering::Relaxed)
        }

        /// The return addresses above `dealloc`, for the ring of frees.
        ///
        /// Frame pointers only, and only while each frame lies in the window
        /// above this stack pointer and above the frame before it: a free must
        /// never fault on a chain it is only recording. A coroutine starts with
        /// `rbp = 0` and an entry from userspace leaves a user-half `rbp`
        /// below every kernel frame, so either ends the walk.
        #[cfg(not(feature = "mem-debug"))]
        #[inline(always)]
        fn caller_chain() -> [usize; free_ring::DEPTH] {
            const WINDOW: usize = 64 * 1024;
            let mut rets = [0; free_ring::DEPTH];
            let (mut rbp, rsp): (usize, usize);
            unsafe {
                core::arch::asm!(
                    "mov {}, rbp",
                    "mov {}, rsp",
                    out(reg) rbp,
                    out(reg) rsp,
                    options(nomem, nostack, preserves_flags),
                )
            };
            for r in rets.iter_mut() {
                if rbp < rsp || rbp - rsp > WINDOW - 16 || !rbp.is_multiple_of(8) {
                    break;
                }
                *r = unsafe { core::ptr::read((rbp + 8) as *const usize) };
                let next = unsafe { core::ptr::read(rbp as *const usize) };
                if next <= rbp {
                    break;
                }
                rbp = next;
            }
            rets
        }

        /// What a word found in a free block looks like, for the report.
        #[cfg(not(feature = "mem-debug"))]
        fn word_phrase(word: usize) -> &'static str {
            let clock = kernel_hal::kaddr::clock_shape(
                word as u64,
                kernel_hal::deadline::duration_to_ns(kernel_hal::timer::timer_now()),
            );
            if clock != kernel_hal::kaddr::ClockShape::NotAClock {
                return clock.as_str();
            }
            kernel_hal::kaddr::word_shape(word as u64).as_str()
        }

        /// Name the block the front cache refused, and the code that freed it.
        ///
        /// Nobody who wrote into a free block is on this stack: the write
        /// happened some time after the free, and this is the next allocation
        /// or free of that size, wherever it came from. What can be named is
        /// the block, what was written into it, and -- from the ring of frees
        /// -- the code that freed it. That code's object is the one still in
        /// use somewhere after its free.
        #[cfg(not(feature = "mem-debug"))]
        #[cold]
        #[inline(never)]
        fn report_broken_free_block(broken: free_lists::Broken, size: usize) {
            use core::sync::atomic::{AtomicU32, Ordering};
            use free_lists::{Broken, LinkFault};
            HEAP_WRITTEN_AFTER_FREE.fetch_add(1, Ordering::Relaxed);
            static REPORTED: AtomicU32 = AtomicU32::new(0);
            if REPORTED.fetch_add(1, Ordering::Relaxed) >= 8 {
                return;
            }
            let block = broken.block();
            match broken {
                Broken::Tag { found, lost, .. } => emit(format_args!(
                    "\n[heap-uaf] free {}-byte block {:#x} was WRITTEN AFTER ITS FREE: its \
                     second word, the free mark, now holds {:#x} ({}). Leaked it and the {} \
                     free block(s) behind it instead of handing them out.\n",
                    size,
                    block,
                    found,
                    word_phrase(found),
                    lost,
                )),
                Broken::Link {
                    link, lost, why, ..
                } => {
                    let what = match why {
                        LinkFault::Outside => "which is no block of that size the heap owns",
                        LinkFault::NotFree => {
                            "a block of the heap that is not free: either this link was \
                             overwritten with a pointer to a live block, or that block was \
                             written after its own free"
                        }
                        LinkFault::Zeroed => "zero, with free blocks still counted behind it",
                        LinkFault::PastEnd => "although it is the last free block of its size",
                    };
                    emit(format_args!(
                        "\n[heap-uaf] free {}-byte block {:#x} was WRITTEN AFTER ITS FREE: its \
                         link to the next free block is {:#x} ({}), {}. Leaked it and the {} \
                         free block(s) behind it instead of following the link.\n",
                        size,
                        block,
                        link,
                        word_phrase(link),
                        what,
                        lost,
                    ));
                    if why == LinkFault::NotFree {
                        report_freer(link);
                    }
                }
                Broken::DoubleFree { .. } => emit(format_args!(
                    "\n[heap-uaf] {}-byte block {:#x} FREED TWICE: it is already in the free \
                     list, so it is not freed again. The second free is this call chain:\n",
                    size, block,
                )),
            }
            if let Broken::DoubleFree { .. } = broken {
                let mut rbp: usize;
                unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };
                for _ in 0..16 {
                    if !kernel_hal::kaddr::is_kernel_stack_qword(rbp as u64) {
                        break;
                    }
                    let ret = unsafe { core::ptr::read_volatile((rbp + 8) as *const usize) };
                    let next = unsafe { core::ptr::read_volatile(rbp as *const usize) };
                    if ret == 0 {
                        break;
                    }
                    emit(format_args!(
                        "[heap-uaf]   ret={}\n",
                        kernel_hal::ksyms::Addr(ret as u64)
                    ));
                    if next <= rbp {
                        break;
                    }
                    rbp = next;
                }
            }
            report_freer(block);
        }

        /// The last free of `block` the ring still holds.
        #[cfg(not(feature = "mem-debug"))]
        fn report_freer(block: usize) {
            match slab::FREES.find(block) {
                Some((rets, age)) => {
                    emit(format_args!(
                        "[heap-uaf] {:#x} was freed {} cached free(s) ago, by:\n",
                        block, age,
                    ));
                    for ret in rets.iter().take_while(|r| **r != 0) {
                        emit(format_args!(
                            "[heap-uaf]   ret={}\n",
                            kernel_hal::ksyms::Addr(*ret as u64)
                        ));
                    }
                }
                None => emit(format_args!(
                    "[heap-uaf] {:#x}: its free is no longer in the ring of the last {} frees\n",
                    block,
                    slab::FREES.len(),
                )),
            }
        }

        #[cold]
        #[inline(never)]
        fn report_heap_reentrancy(what: &str, sz: usize) {
            use core::sync::atomic::{AtomicU32, Ordering};
            HEAP_REENTRANCY_EVENTS.fetch_add(1, Ordering::Relaxed);
            if HEAP_REENTRANT_LOG {
                static REPORTED: AtomicU32 = AtomicU32::new(0);
                if REPORTED.fetch_add(1, Ordering::Relaxed) < 4 {
                    emit(format_args!(
                        "\n[heap-reentrant] {} size={:#x} while THIS cpu already holds the \
                         heap lock — a fault was taken inside the allocator and the fault \
                         path allocated. Refusing instead of wedging the machine. The \
                         re-entrant call chain below is the code that must not allocate:\n",
                        what, sz,
                    ));
                    let mut rbp: usize;
                    unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };
                    for _ in 0..24 {
                        if !kernel_hal::kaddr::is_kernel_stack_qword(rbp as u64) {
                            break;
                        }
                        let ret =
                            unsafe { core::ptr::read_volatile((rbp + 8) as *const usize) };
                        let next = unsafe { core::ptr::read_volatile(rbp as *const usize) };
                        if ret == 0 {
                            break;
                        }
                        emit(format_args!(
                            "[heap-reentrant]   ret={}\n",
                            kernel_hal::ksyms::Addr(ret as u64)
                        ));
                        if next <= rbp {
                            break;
                        }
                        rbp = next;
                    }
                    if !kernel_hal::ksyms::available() {
                        emit(format_args!(
                            "[heap-reentrant] no in-kernel symbol table — symbolize with \
                             `make sym ADDRS=\"...\"` where this kernel was built\n"
                        ));
                    }
                }
            } else {
                let _ = (what, sz);
            }
        }

        /// One-shot report that the buddy allocator just dispensed a block
        /// overlapping a live coroutine stack — the double-alloc every
        /// null-range crash has been chasing. Runs inside `alloc`, on the
        /// allocating call chain, so its backtrace names the writer's path.
        #[cold]
        #[inline(never)]
        fn report_stack_double_alloc(ptr: usize, sz: usize, stack_base: usize) {
            use core::sync::atomic::{AtomicBool, Ordering};
            executor::note_heap_smash_suspected();
            static REPORTED: AtomicBool = AtomicBool::new(false);
            if REPORTED.swap(true, Ordering::SeqCst) {
                return;
            }
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[double-alloc] BUDDY HANDED OUT A LIVE COROUTINE STACK: \
                 alloc ptr={:#x} size={:#x} overlaps live stack alloc_base={:#x}. \
                 This allocation is the wild zero-writer; its call chain is below \
                 (diag rev 4).\n",
                ptr, sz, stack_base,
            ));
            // Frame-pointer backtrace of the ALLOCATING path — the code about
            // to zero-fill this block over a live stack. Bounded and guarded so
            // a corrupt chain cannot fault this report.
            let mut rbp: usize;
            unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };
            for _ in 0..20 {
                if !kernel_hal::kaddr::is_kernel_stack_qword(rbp as u64) {
                    break;
                }
                let ret = unsafe { core::ptr::read_volatile((rbp + 8) as *const usize) };
                let next = unsafe { core::ptr::read_volatile(rbp as *const usize) };
                if ret == 0 {
                    break;
                }
                kernel_hal::console::serial_write_fmt_spin(format_args!(
                    "[double-alloc]   ret={}\n",
                    kernel_hal::ksyms::Addr(ret as u64)
                ));
                if next <= rbp {
                    break;
                }
                rbp = next;
            }
            if !kernel_hal::ksyms::available() {
                kernel_hal::console::serial_write_str(
                    "[double-alloc] no in-kernel symbol table — symbolize with \
                     `make sym ADDRS=\"...\"` where this kernel was built\n",
                );
            }
        }

        /// Live big blocks (≥2 MiB), with the call site that asked for each.
        ///
        /// Attribution for the OOM in #1135: the heap dump was five blocks in
        /// the 64 MiB class and fifty in the 4 MiB class — ~520 MiB of a
        /// 512 MiB heap — and nothing said who held them. This table keeps the
        /// live large blocks and their allocating frames so `/proc/kheap` can
        /// name the holders while the machine still runs.
        ///
        /// Costs nothing on the hot path: only allocations already ≥2 MiB touch
        /// it, and the walk is the same frame-pointer chain the reporters use.
        const BIG_ALLOC_MIN: usize = 2 * 1024 * 1024;
        const BIG_TRACK_SLOTS: usize = 128;
        const BIG_TRACK_FRAMES: usize = 3;

        struct BigSlot {
            ptr: AtomicUsize,
            size: AtomicUsize,
            site: [AtomicUsize; BIG_TRACK_FRAMES],
        }

        static BIG_TRACK: [BigSlot; BIG_TRACK_SLOTS] = [const {
            BigSlot {
                ptr: AtomicUsize::new(0),
                size: AtomicUsize::new(0),
                site: [const { AtomicUsize::new(0) }; BIG_TRACK_FRAMES],
            }
        }; BIG_TRACK_SLOTS];

        /// Big blocks that found no free slot: the table is a sample, and a
        /// non-zero count here says so instead of quietly under-reporting.
        static BIG_TRACK_MISSED: AtomicUsize = AtomicUsize::new(0);

        /// Record a live big block and the frames that asked for it.
        fn track_big_alloc(ptr: usize, sz: usize) {
            let mut site = [0usize; BIG_TRACK_FRAMES];
            let mut rbp: usize;
            unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };
            for slot in site.iter_mut() {
                if !kernel_hal::kaddr::is_kernel_stack_qword(rbp as u64) {
                    break;
                }
                let ret = unsafe { core::ptr::read_volatile((rbp + 8) as *const usize) };
                let next = unsafe { core::ptr::read_volatile(rbp as *const usize) };
                if ret == 0 {
                    break;
                }
                *slot = ret;
                if next <= rbp {
                    break;
                }
                rbp = next;
            }
            for e in BIG_TRACK.iter() {
                if e.ptr
                    .compare_exchange(0, ptr, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    e.size.store(sz, Ordering::Relaxed);
                    for (i, f) in site.iter().enumerate() {
                        e.site[i].store(*f, Ordering::Relaxed);
                    }
                    return;
                }
            }
            BIG_TRACK_MISSED.fetch_add(1, Ordering::Relaxed);
        }

        /// Drop a big block from the table when it is freed.
        fn untrack_big_alloc(ptr: usize) {
            for e in BIG_TRACK.iter() {
                if e.ptr.load(Ordering::Relaxed) == ptr
                    && e.ptr
                        .compare_exchange(ptr, 0, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                {
                    e.size.store(0, Ordering::Relaxed);
                    return;
                }
            }
        }

        /// The live big blocks, biggest first: `(size, [call frames])`.
        /// Entries with size 0 are empty slots.
        pub fn heap_big_blocks() -> ([(usize, [usize; BIG_TRACK_FRAMES]); BIG_TRACK_SLOTS], usize) {
            let mut out = [(0usize, [0usize; BIG_TRACK_FRAMES]); BIG_TRACK_SLOTS];
            for (o, e) in out.iter_mut().zip(BIG_TRACK.iter()) {
                if e.ptr.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                o.0 = e.size.load(Ordering::Relaxed);
                for (i, f) in o.1.iter_mut().enumerate() {
                    *f = e.site[i].load(Ordering::Relaxed);
                }
            }
            out.sort_unstable_by_key(|a| core::cmp::Reverse(a.0));
            (out, BIG_TRACK_MISSED.load(Ordering::Relaxed))
        }

        /// Heap pressure watermarks, in percent of `KERNEL_HEAP_SIZE`.
        ///
        /// The heap filling up used to be silent until the machine died in
        /// `alloc_error`, with the attribution printed only at the funeral. A
        /// march from 20% to 100% takes minutes of desktop use; say so while
        /// there is still a machine to say it on, once per level crossed
        /// upward, with the same by-size-class attribution.
        const HEAP_PRESSURE_LEVELS: [usize; 4] = [60, 75, 85, 95];
        static HEAP_PRESSURE_REPORTED: AtomicUsize = AtomicUsize::new(0);

        /// One compare on the alloc path; the report itself is `#[cold]`.
        #[inline]
        fn note_heap_pressure(used: usize) {
            let reported = HEAP_PRESSURE_REPORTED.load(Ordering::Relaxed);
            if reported >= HEAP_PRESSURE_LEVELS.len() {
                return;
            }
            let pct = used / (HEAP_TOTAL.load(Ordering::Relaxed) / 100).max(1);
            if pct >= HEAP_PRESSURE_LEVELS[reported] {
                report_heap_pressure(reported, used, pct);
            }
        }

        #[cold]
        #[inline(never)]
        fn report_heap_pressure(level: usize, used: usize, pct: usize) {
            // Claim the level: whoever wins prints, everyone else moves on.
            if HEAP_PRESSURE_REPORTED
                .compare_exchange(level, level + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                return;
            }
            emit(format_args!(
                "\n[heap-pressure] {}% of the kernel heap in use ({} of {} MiB). Live by size class:\n",
                pct,
                used >> 20,
                HEAP_TOTAL.load(Ordering::Relaxed) >> 20,
            ));
            for (i, count) in HEAP_LIVE.iter().enumerate() {
                let live = count.load(Ordering::Relaxed);
                let size = 1usize << i;
                // Only classes that could matter: a MiB or more if every
                // block sat at the class bound.
                if live > 0 && (live * size) >> 20 > 0 {
                    emit(format_args!(
                        "[heap-pressure]   <={:>9}B x {:<8} (<= {} MiB)\n",
                        size,
                        live,
                        (live * size) >> 20,
                    ));
                }
            }
            #[cfg(feature = "linux")]
            {
                let (created, live, bytes) = linux_object::fs::memfd_stats();
                emit(format_args!(
                    "[heap-pressure]   memfd created={} live={} live_bytes={} MiB\n",
                    created,
                    live,
                    bytes >> 20,
                ));
            }
            emit(format_args!(
                "[heap-pressure] `cat /proc/kheap` names the holders of every block >= 2 MiB\n"
            ));
        }

        /// Bytes the heap manages RIGHT NOW: the static arena plus every
        /// chunk [`try_grow_heap`] has taken from physical RAM.
        static HEAP_TOTAL: AtomicUsize = AtomicUsize::new(KERNEL_HEAP_SIZE);

        pub fn heap_total() -> usize {
            HEAP_TOTAL.load(Ordering::Relaxed)
        }

        // ── Elastic heap ────────────────────────────────────────────────────
        //
        // The arena is a fixed `static mut HEAP: [usize; _]` in .bss, and
        // running it out ends the machine. That ceiling is arbitrary: the box
        // has GiBs of RAM the frame allocator is managing, while a desktop
        // client can put more than 512 MiB into the heap on its own — memfd
        // and tmpfs file content lives here, and one browser's shm pools
        // already carry 534 MiB of (sparse) logical size.
        //
        // So when the heap gets tight, take physical frames and hand them to
        // the buddy (`add_to_heap`, which is address-agnostic — the free
        // lists are intrusive and hold absolute addresses). The heap becomes
        // bounded by RAM instead of by a constant, and every consumer
        // benefits, not just the one that happened to trip it.
        const HEAP_GROW_AT_PCT: usize = 70;
        /// Tried in order: see the fallback in `grow_heap_once`.
        const HEAP_GROW_CHUNKS: [usize; 3] =
            [32 * 1024 * 1024, 8 * 1024 * 1024, 2 * 1024 * 1024];
        const HEAP_MAX_TOTAL: usize = 2 * 1024 * 1024 * 1024;
        // The wild-block registry must be able to hold every region this
        // policy can ever create -- the static arena plus one per growth, all
        // of them at the SMALLEST fallback chunk. If it cannot, a machine
        // under memory pressure silently turns the guard off, which is the one
        // machine that needs it. Shrinking the fallback chunk or raising the
        // ceiling fails the build here rather than in the field.
        // Written as a strict `>` rather than `>= 1 + n`: the arena itself is
        // the one extra region beyond the growths, and `clippy::int_plus_one`
        // rejects the other spelling.
        const _: () = assert!(
            heap_regions::MAX_REGIONS
                > (HEAP_MAX_TOTAL - KERNEL_HEAP_SIZE)
                    / HEAP_GROW_CHUNKS[HEAP_GROW_CHUNKS.len() - 1]
        );
        /// Never take the machine's last quarter of RAM for the kernel heap:
        /// user pages must still be commitable, or we trade an OOM here for a
        /// worse one in the page-fault path.
        const HEAP_LEAVE_FREE_PCT: usize = 25;

        /// Set for the duration of a growth attempt. Growth runs from INSIDE
        /// `alloc`, and `frame_alloc`'s failure path logs (which allocates),
        /// so without this flag a failed growth would recurse into itself.
        static HEAP_GROWING: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        static HEAP_GROW_REFUSED: AtomicUsize = AtomicUsize::new(0);

        /// Grow the heap when it passes [`HEAP_GROW_AT_PCT`]. One relaxed
        /// compare on the alloc path; everything else is `#[cold]`.
        #[inline]
        fn try_grow_heap(used: usize) {
            let total = HEAP_TOTAL.load(Ordering::Relaxed);
            if used < total / 100 * HEAP_GROW_AT_PCT {
                return;
            }
            grow_heap_guarded(used, total);
        }

        /// Run one growth attempt under the re-entrancy flag. Returns whether
        /// the heap actually got bigger.
        fn grow_heap_guarded(used: usize, total: usize) -> bool {
            if HEAP_GROWING.swap(true, Ordering::AcqRel) {
                return false;
            }
            let grew = grow_heap_once(used, total);
            HEAP_GROWING.store(false, Ordering::Release);
            grew
        }

        /// Last resort: the buddy just refused an allocation. Grow **without**
        /// consulting the watermark and tell the caller whether to retry.
        ///
        /// [`try_grow_heap`] alone is not enough to keep the machine alive,
        /// for two compounding reasons. It only ran from the *success* path of
        /// `alloc` — a failing allocation fell straight through to
        /// `alloc_error`, which panics, so the elastic heap could never save
        /// the one allocation that needed it. And the watermark it consults is
        /// `HEAP_USED`, which counts the *requested* size while the buddy
        /// rounds every block up to a power of two; the desktop session was
        /// observed exhausting the pool with `HEAP_USED` reading ~54%, i.e.
        /// well under the 70% trigger, so growth was never even considered.
        /// Growing here needs no accounting to be accurate: the buddy saying
        /// "no" is the ground truth.
        #[cold]
        #[inline(never)]
        fn grow_heap_on_failure() -> bool {
            let total = HEAP_TOTAL.load(Ordering::Relaxed);
            let used = HEAP_USED.load(Ordering::Relaxed);
            grow_heap_guarded(used, total)
        }

        #[cold]
        #[inline(never)]
        fn grow_heap_once(used: usize, total: usize) -> bool {
            if !HEAP_GROW_LOG {
                let _ = used;
            }
            if total >= HEAP_MAX_TOTAL {
                if HEAP_GROW_REFUSED.fetch_add(1, Ordering::Relaxed) == 0 && HEAP_GROW_LOG {
                    emit(format_args!(
                        "\n[heap-grow] REFUSED: already at the {} MiB ceiling ({} MiB in use)\n",
                        total >> 20,
                        used >> 20,
                    ));
                }
                return false;
            }
            // The heap lock is free at the call site (the allocation that
            // brought us here has already released it), but an IRQ can land
            // here on a CPU that holds it. Asking is exact and costs nothing.
            if HEAP_ALLOCATOR.0.held_by_current_cpu() {
                return false;
            }
            // Leave the machine room to commit user pages.
            let ram = TOTAL_MEMORY.load(Ordering::Relaxed);
            let ram_used = FRAMES_USED.load(Ordering::Relaxed);
            let reserve = ram / 100 * HEAP_LEAVE_FREE_PCT;
            if ram == 0 || ram_used + HEAP_GROW_CHUNKS[HEAP_GROW_CHUNKS.len() - 1] + reserve > ram
            {
                if HEAP_GROW_REFUSED.fetch_add(1, Ordering::Relaxed) == 0 && HEAP_GROW_LOG {
                    emit(format_args!(
                        "\n[heap-grow] REFUSED: {} MiB of {} MiB RAM already committed; \
                         the heap stays at {} MiB\n",
                        ram_used >> 20,
                        ram >> 20,
                        total >> 20,
                    ));
                }
                return false;
            }
            // A fragmented machine can have GiBs free and no 32 MiB run left:
            // the second growth on the very first test boot was refused for
            // exactly that. Step down instead of giving up.
            let mut got = None;
            for chunk in HEAP_GROW_CHUNKS {
                if ram_used + chunk + reserve > ram {
                    continue;
                }
                // align_log2 is in FRAMES, not bytes: 21 would demand an 8 GiB
                // alignment and fail on any real machine (the first test boot
                // only succeeded because the run happened to land on 8 GiB).
                // The buddy needs no more than usize alignment.
                if let Some(pa) = frame_alloc(chunk >> PAGE_BITS, 0) {
                    got = Some((pa, chunk));
                    break;
                }
            }
            let Some((pa, chunk)) = got else {
                if HEAP_GROW_REFUSED.fetch_add(1, Ordering::Relaxed) == 0 && HEAP_GROW_LOG {
                    emit(format_args!(
                        "\n[heap-grow] REFUSED: no contiguous run of frames left, down to {} MiB\n",
                        HEAP_GROW_CHUNKS[HEAP_GROW_CHUNKS.len() - 1] >> 20,
                    ));
                }
                return false;
            };
            let va = kernel_hal::mem::phys_to_virt(pa);
            // Register BEFORE the buddy is told about the region, never after.
            // The guard from `.lock()` below dies at the end of its own
            // statement, so registering afterwards leaves a window in which
            // another CPU can allocate out of the new chunk and free it again
            // while the registry still says those addresses are not the
            // heap's -- and a perfectly sound block would be leaked and
            // reported as corruption. Registering first cannot misfire in the
            // other direction: it only ever widens the set of addresses the
            // heap may own, and nothing can be freed out of a region the buddy
            // has not handed anything out of yet.
            heap_regions::register(va, va + chunk);
            // SAFETY: these frames were just allocated to us, are inside the
            // kernel's linear map, and overlap nothing the heap manages.
            unsafe {
                HEAP_ALLOCATOR.0.lock().add_to_heap(va, va + chunk);
            }
            HEAP_TOTAL.fetch_add(chunk, Ordering::Relaxed);
            // The vtable-liveness ceiling (`set_vtable_max`) is deliberately
            // LEFT ALONE here. It rests on "the image links .rodata below
            // .bss, so no real vtable is at or above the static heap base" —
            // and a grown region lives in the linear map (0xffff_8000_…),
            // BELOW the kernel image (0xffff_ff00_…). Lowering the bound to it
            // would classify every genuine vtable in the image as
            // heap-resident, and `dyn_fat_ptr_live` would then refuse every
            // dyn dispatch in the system. The cost is that the heuristic does
            // not cover blocks in grown regions; refusing to dispatch anything
            // is not a trade. (Caught on the first forced-growth boot: the
            // region landed at 0xffff_8002_0000_0000.)
            if HEAP_GROW_LOG {
                emit(format_args!(
                    "\n[heap-grow] +{} MiB from physical RAM at {:#x} (heap {} -> {} MiB, {} MiB in use)\n",
                    chunk >> 20,
                    va,
                    total >> 20,
                    (total + chunk) >> 20,
                    used >> 20,
                ));
            }
            true
        }

        pub fn init() {
            const MACHINE_ALIGN: usize = core::mem::size_of::<usize>();
            const HEAP_BLOCK: usize = KERNEL_HEAP_SIZE / MACHINE_ALIGN;
            static mut HEAP: [usize; HEAP_BLOCK] = [0; HEAP_BLOCK];
            let heap_start = (&raw const HEAP).cast::<u8>() as usize;
            // Tell the allocator which addresses are actually its own, so a
            // free of anything else is refused instead of corrupting the free
            // lists. Before `init`, for the same reason as in
            // `grow_heap_once`: the registry must never lag the buddy.
            heap_regions::register(heap_start, heap_start + HEAP_BLOCK * MACHINE_ALIGN);
            unsafe {
                HEAP_ALLOCATOR
                    .lock()
                    .init(heap_start, HEAP_BLOCK * MACHINE_ALIGN);
            }
            // Teach the fat-pointer liveness gate where the heap begins. A real
            // vtable lives in `.rodata`, always below this address; any dyn
            // pointer whose vtable word lands at or above it is a use-after-free
            // and must be leaked, not dispatched. See
            // `zcore_drivers::utils::fat_ptr`.
            kernel_hal::drivers::utils::set_vtable_max(heap_start);
            crate::klog_info!(
                "memory: kernel heap ready ({} KiB @ {:#x}), vtable ceiling registered",
                KERNEL_HEAP_SIZE / 1024,
                heap_start
            );
        }

        pub struct LockedHeap<const ORDER: usize>(Mutex<Heap<ORDER>>);

        impl<const ORDER: usize> LockedHeap<ORDER> {
            pub const fn new() -> Self {
                LockedHeap(Mutex::new(Heap::<ORDER>::new()))
            }
        }

        impl<const ORDER: usize> Deref for LockedHeap<ORDER> {
            type Target = Mutex<Heap<ORDER>>;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        // Sin redzone: el canario de depuración (16 bytes tras cada asignación)
        // ya cumplió su función (la corrupción de heap que perseguía está
        // arreglada) y tenía un coste oculto brutal: el buddy allocator
        // redondea cada asignación a la potencia de dos superior, así que una
        // asignación de 4096 bytes (la clase dominante: caché de bloques del
        // SFS, buffers de pipe) pasaba a consumir 8192 reales. La sesión de
        // escritorio agotaba el pool con `HEAP_USED` marcando solo ~54%, porque
        // la contabilidad cuenta `sz` y no el bloque redondeado.

        // Histograma de asignaciones VIVAS por clase de tamaño (log2). El OOM
        // del escritorio (499/512 MiB usados) necesita atribución: qué clase
        // de tamaño retiene el heap. Coste: un fetch_add relajado por
        // alloc/dealloc. `heap_live_histogram` lo vuelca el alloc_error
        // handler sin asignar memoria.
        const HEAP_BUCKETS: usize = 32;
        static HEAP_LIVE: [AtomicUsize; HEAP_BUCKETS] = [const { AtomicUsize::new(0) }; HEAP_BUCKETS];

        #[inline]
        fn bucket_of(size: usize) -> usize {
            (usize::BITS - size.max(1).leading_zeros()) as usize % HEAP_BUCKETS
        }

        /// Live-allocation counts per power-of-two size class, for the OOM
        /// report. Index i counts allocations with size in (2^(i-1), 2^i].
        pub fn heap_live_histogram() -> [usize; HEAP_BUCKETS] {
            let mut out = [0usize; HEAP_BUCKETS];
            for (i, slot) in HEAP_LIVE.iter().enumerate() {
                out[i] = slot.load(Ordering::Relaxed);
            }
            out
        }

        // Exact-size tracking for OOM attribution. Track live counts for up to 32
        // distinct allocation sizes across the heap, first-come-first-served.
        const HOT_SLOTS: usize = 32;
        static HOT_SIZE: [AtomicUsize; HOT_SLOTS] = [const { AtomicUsize::new(0) }; HOT_SLOTS];
        static HOT_LIVE: [AtomicUsize; HOT_SLOTS] = [const { AtomicUsize::new(0) }; HOT_SLOTS];

        fn hot_track(size: usize, delta: isize) {
            for i in 0..HOT_SLOTS {
                let cur = HOT_SIZE[i].load(Ordering::Relaxed);
                let claimed = cur == size
                    || (cur == 0
                        && HOT_SIZE[i]
                            .compare_exchange(0, size, Ordering::Relaxed, Ordering::Relaxed)
                            .map_or_else(|racer| racer == size, |_| true));
                if claimed {
                    if delta > 0 {
                        let live = HOT_LIVE[i].fetch_add(1, Ordering::Relaxed) + 1;
                        // Leak hunt: the desktop OOMs (512 MiB heap) with ~2M live
                        // 8 B and 96 B blocks -- a per-event allocation that is never
                        // freed. Dump the allocating call chain a few times as each
                        // suspect class climbs, so the leaking site can be symbolized
                        // from the printed return addresses. Fires at most 3x/class.
                        //
                        // 512 B joined them from an OOM report taken on hardware:
                        // `512B x 131156 (~64 MiB)`, the largest exact class on the
                        // screen, against a heap that then could not find 4 MiB. No
                        // call site in the tree allocates exactly 512 bytes often
                        // enough to explain 131k live blocks, and nothing bounded
                        // holds that many (the console's scrollback caps at 1000
                        // lines per VT, and there are 7 VTs), so it is either a leak
                        // or a cache with no ceiling -- and either way the only way
                        // to name it is to catch it allocating. Thresholds are set
                        // below what the report showed so the dump lands before the
                        // heap is gone.
                        let hunt = match size {
                            8 | 96 => matches!(live, 100_000 | 400_000 | 800_000),
                            512 => matches!(live, 20_000 | 60_000 | 120_000),
                            _ => false,
                        };
                        if (size == 4096 && (live == 50_000 || live == 90_000)) || hunt {
                            leak_trace_dump(size, live);
                        }
                    } else {
                        HOT_LIVE[i].try_update(
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                            |v| Some(v.saturating_sub(1)),
                        ).ok();
                    }
                    return;
                }
            }
        }

        /// Print kernel-.text-looking words found on the current stack (the
        /// return-address chain of whoever is allocating), via the no-alloc
        /// spin serial writer. Reads stay inside the mapped kernel heap /
        /// physmap, so over-scanning past the coroutine stack top is safe.
        #[cold]
        fn leak_trace_dump(size: usize, live: usize) {
            let mut rsp: usize;
            unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp) };
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[leaktrace] {}B live={} stack-scan:",
                size, live
            ));
            // `kaddr`, against the image's own `stext`..`etext`. These were
            // two more literals, a third window again (one page in, 16 MiB
            // out) that agreed with neither of the two in `handler.rs` nor
            // with the image: `.text` starts at its first page and ends
            // around 5.7 MiB, so this both dropped real return addresses and
            // printed heap-shaped words as if they were code.
            let mut printed = 0;
            let mut p = rsp;
            while printed < 24 && p < rsp + 32 * 1024 {
                let v = unsafe { core::ptr::read_volatile(p as *const usize) };
                if kernel_hal::kaddr::is_kernel_text(v as u64) {
                    kernel_hal::console::serial_write_fmt_spin(format_args!(" {:#x}", v));
                    printed += 1;
                }
                p += 8;
            }
            kernel_hal::console::serial_write_fmt_spin(format_args!("\n"));
        }

        /// (size, live-count) pairs for the exact-size tracker (0 = unused slot).
        pub fn heap_hot_sizes() -> [(usize, usize); HOT_SLOTS] {
            let mut out = [(0usize, 0usize); HOT_SLOTS];
            for i in 0..HOT_SLOTS {
                out[i] = (
                    HOT_SIZE[i].load(Ordering::Relaxed),
                    HOT_LIVE[i].load(Ordering::Relaxed),
                );
            }
            out
        }

        // Heap-corruption forensics (feature `mem-debug`), reinstated after the
        // desktop soak kept dying on #GPs whose registers held ELF-magic bytes
        // (a freed heap block reused as a file buffer while its old owner
        // still points at it). Two tools:
        //  * REDZONE canary after every allocation: linear overflows panic at
        //    dealloc naming the clobbered block. (Hidden cost: the buddy's
        //    power-of-two rounding means the +16 doubles the real footprint of
        //    the dominant 4 KiB class — SFS block cache, pipe buffers.)
        //  * Poison-on-free (0xA5): a use-after-free READ yields the
        //    unmistakable 0xa5a5... pattern in crash registers instead of
        //    whatever the next owner wrote, separating "read after free" from
        //    "read after free AND reuse". (Hidden cost: dealloc becomes
        //    O(size) — freeing a 64 KiB I/O buffer memsets all 64 KiB.)
        //
        // Default build: no redzone, no poison — alloc/dealloc are the buddy
        // op plus two relaxed counters.
        #[cfg(feature = "mem-debug")]
        const REDZONE: usize = 16;
        #[cfg(not(feature = "mem-debug"))]
        const REDZONE: usize = 0;
        #[cfg(feature = "mem-debug")]
        const CANARY: u8 = 0xAB;
        #[cfg(feature = "mem-debug")]
        const POISON: u8 = 0xA5;

        // ── Front cache: per-size-class free lists over the buddy allocator ──
        //
        // The buddy's `dealloc` coalesces by SCANNING `free_list[class]` for the
        // sibling block (buddy_system_allocator 0.8 lib.rs:161) — O(free-list
        // length). A fork storm allocates and frees thousands of same-sized
        // objects (VMObjectPaged inner, VmMapping, VmMappingInner) whose buddies
        // are still live, so those class free lists grow long and *every*
        // dealloc pays O(n): the fork loop is O(n²). HEAPPROF confirmed it —
        // dealloc climbed from ~18 K to ~284 K cyc/call across a 512-map loop.
        //
        // This cache keeps recently-freed small blocks in per-class LIFO stacks
        // (the next pointer lives in the freed block's first word, like the
        // buddy's own list) and serves allocations from them in O(1). The common
        // alloc/free pair never touches the buddy, so the buddy free lists stay
        // short and the O(n) scan never fires on the hot path. The buddy is
        // consulted only to refill an empty class or to absorb frees past the
        // per-class cap.
        //
        // Safety hinges on one buddy invariant: every block of class `c` is
        // 2^c-aligned. The buddy carves its region by the address' low bit and
        // splits power-of-two blocks in halves (lib.rs:87, 117), so a class-`c`
        // block is always 2^c-aligned. Since `class_size ≥ layout.align()`, any
        // cached class-`c` block satisfies the alignment of *any* allocation that
        // maps to class `c` — blocks in a class are freely interchangeable.
        //
        // The lists themselves are `free_lists`, which checks every link and
        // free mark before trusting it; this module is the lock around them.
        //
        // Only enabled in the default build. `mem-debug` wants real buddy
        // round-trips for its canary/poison forensics, so it bypasses the cache.
        #[cfg(not(feature = "mem-debug"))]
        mod slab {
            use super::{
                free_lists::{self, SlabCache},
                free_ring, heap_regions, Mutex,
            };
            use core::alloc::Layout;

            static SLAB: Mutex<SlabCache> = Mutex::new(SlabCache::new());

            /// Who freed each cached block, newest last. See [`free_ring`].
            pub static FREES: free_ring::FreeRing<4096> = free_ring::FreeRing::new();

            /// Whether a link may be followed: a block of `size` bytes, aligned
            /// to it, inside the heap. Before `init`, or once the registry has
            /// lost track of a growth, [`heap_regions::check`] has no opinion
            /// and only the alignment is left: an aligned wild link is then
            /// followed, as every link used to be.
            fn owns(addr: usize, size: usize) -> bool {
                heap_regions::check(addr, size, size).is_none()
            }

            /// What [`try_free`] did with a block.
            pub enum Freed {
                /// In the cache.
                Cached,
                /// The caller must hand it to the buddy.
                ToBuddy,
                /// Already free: reported and not freed again. The caller must
                /// not count it either -- its first free already did.
                Refused,
            }

            /// Serve `layout` from the front cache, or null on a miss. A pure
            /// block-provider — the caller owns the live-heap accounting.
            ///
            /// Uses `try_lock` instead of `lock` so that re-entrant callers
            /// (e.g. an interrupt that fires while the same CPU already holds
            /// SLAB) get a cache miss and fall through to the buddy allocator
            /// rather than spinning forever on a lock they already own.
            #[inline]
            pub fn try_alloc(layout: Layout) -> *mut u8 {
                let Some(i) = free_lists::cache_slot(layout) else {
                    return core::ptr::null_mut();
                };
                let Some(mut c) = SLAB.try_lock() else {
                    return core::ptr::null_mut();
                };
                match unsafe { c.pop(i, owns) } {
                    Ok(Some(block)) => block as *mut u8,
                    Ok(None) => core::ptr::null_mut(),
                    Err(broken) => {
                        // Off the lock first: the report formats, and whatever
                        // it allocates must find the cache usable.
                        drop(c);
                        super::report_broken_free_block(broken, free_lists::class_size(i));
                        core::ptr::null_mut()
                    }
                }
            }

            /// Return a freed block to the front cache, leaving `rets` in
            /// [`FREES`] when it is cached. `ToBuddy` means the class is out of
            /// range, the cache is full, or the lock is already held -- in all
            /// cases the caller must hand the block to the buddy.
            ///
            /// Uses `try_lock` instead of `lock` for the same re-entrancy safety
            /// reason as `try_alloc`.
            #[inline]
            pub fn try_free(ptr: *mut u8, layout: Layout, rets: [usize; free_ring::DEPTH]) -> Freed {
                let Some(i) = free_lists::cache_slot(layout) else {
                    return Freed::ToBuddy;
                };
                let Some(mut c) = SLAB.try_lock() else {
                    return Freed::ToBuddy;
                };
                match unsafe { c.push(i, ptr as usize) } {
                    Ok(true) => {
                        // Under the lock: nothing can pop the block, write into
                        // it and be caught before its freer is on record.
                        FREES.note(ptr as usize, rets);
                        Freed::Cached
                    }
                    Ok(false) => Freed::ToBuddy,
                    Err(broken) => {
                        drop(c);
                        super::report_broken_free_block(broken, free_lists::class_size(i));
                        Freed::Refused
                    }
                }
            }
        }

        unsafe impl<const ORDER: usize> GlobalAlloc for LockedHeap<ORDER> {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let sz = layout.size();
                let ext = Layout::from_size_align_unchecked(sz + REDZONE, layout.align());
                // See `kernel_hal::kstats::heap_prof_enabled`: off by default,
                // and when off this is one relaxed load on the kernel's hottest
                // path.
                let prof = kernel_hal::kstats::heap_prof_enabled();
                let t0 = if prof { core::arch::x86_64::_rdtsc() } else { 0 };
                // Front cache first (default build); a miss falls through to the
                // buddy. The cache serves the common small alloc/free pair in
                // O(1), keeping the buddy's class free lists short so its O(n)
                // coalescing scan never fires on the fork hot path.
                #[cfg(not(feature = "mem-debug"))]
                let p = {
                    let cached = slab::try_alloc(ext);
                    if cached.is_null() {
                        // See `report_heap_reentrancy`: asked BEFORE a ticket is
                        // drawn, because a drawn ticket can never be given back.
                        if self.0.held_by_current_cpu() {
                            report_heap_reentrancy("alloc", sz);
                            core::ptr::null_mut::<u8>()
                        } else {
                            self.0
                                .lock()
                                .alloc(ext)
                                .ok()
                                .map_or(core::ptr::null_mut::<u8>(), |a| a.as_ptr())
                        }
                    } else {
                        cached
                    }
                };
                #[cfg(feature = "mem-debug")]
                let p = if self.0.held_by_current_cpu() {
                    report_heap_reentrancy("alloc", sz);
                    core::ptr::null_mut::<u8>()
                } else {
                    self.0
                        .lock()
                        .alloc(ext)
                        .ok()
                        .map_or(core::ptr::null_mut::<u8>(), |a| a.as_ptr())
                };
                // The buddy refused. Before the caller turns that into
                // `alloc_error` (which panics and ends the machine), take
                // frames from physical RAM, hand them to the buddy and ask
                // once more — see `grow_heap_on_failure` for why the
                // watermark-driven growth on the success path below cannot
                // catch this case. `held_by_current_cpu` means the null came
                // from the re-entrancy refusal, not from an empty pool, so
                // growing would not help and re-locking would deadlock.
                let p = if p.is_null() && !self.0.held_by_current_cpu() && grow_heap_on_failure() {
                    self.0
                        .lock()
                        .alloc(ext)
                        .ok()
                        .map_or(core::ptr::null_mut::<u8>(), |a| a.as_ptr())
                } else {
                    p
                };
                if !p.is_null() {
                    // [diag] Double-alloc tripwire. Every crash zeroes a live
                    // coroutine stack; if the buddy hands out a block that
                    // overlaps one, this is the writer's own allocation and its
                    // call chain is on the stack right now. Report once with a
                    // frame-pointer backtrace and latch the smash flag so the
                    // timer path stops running on the doomed stack. Cheap: a
                    // scan of the small live-executor set per allocation.
                    if let Some(base) = executor::alloc_overlaps_live_stack(p as usize, sz + REDZONE)
                    {
                        report_stack_double_alloc(p as usize, sz, base);
                    }
                    let used = HEAP_USED.fetch_add(sz, Ordering::Relaxed) + sz;
                    HEAP_LIVE[bucket_of(sz)].fetch_add(1, Ordering::Relaxed);
                    hot_track(sz, 1);
                    try_grow_heap(used);
                    note_heap_pressure(used);
                    if sz >= BIG_ALLOC_MIN {
                        track_big_alloc(p as usize, sz);
                    }
                    #[cfg(feature = "mem-debug")]
                    {
                        let cz = p.add(sz);
                        for i in 0..REDZONE {
                            core::ptr::write_volatile(cz.add(i), CANARY);
                        }
                    }
                }
                kernel_hal::kstats::note_heap_alloc(if prof {
                    core::arch::x86_64::_rdtsc().wrapping_sub(t0)
                } else {
                    0
                });
                p
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                let prof = kernel_hal::kstats::heap_prof_enabled();
                let t0 = if prof { core::arch::x86_64::_rdtsc() } else { 0 };
                let sz = layout.size();
                // BEFORE anything touches this block, and before the front
                // cache gets a chance to keep it. The buddy's free lists are
                // intrusive -- a freed block's own first words become the
                // links -- so freeing a pointer the heap never handed out is
                // not a failed free, it is a wild write into the allocator's
                // bookkeeping. It surfaces much later, on another CPU, as the
                // allocator walking into memory that is not its own, with
                // nothing left on screen to name the code that did it.
                //
                // Leaking the block is the right refusal: a leak is bytes, and
                // the alternative is the machine.
                if let Some(fault) =
                    heap_regions::check(ptr as usize, sz + REDZONE, layout.align())
                {
                    report_wild_block("dealloc", fault, ptr as usize, sz, layout.align());
                    return;
                }
                let ext = Layout::from_size_align_unchecked(sz + REDZONE, layout.align());
                // The front cache absorbs the free in O(1); only an
                // out-of-range size or a cap-overflow reaches the buddy
                // (default build only). Asked before anything counts this
                // free: a block the cache finds already free was counted by
                // its first free, and counting it again would lie about the
                // heap in every report that reads these counters.
                #[cfg(not(feature = "mem-debug"))]
                let to_buddy = match slab::try_free(ptr, ext, caller_chain()) {
                    slab::Freed::Cached => false,
                    slab::Freed::ToBuddy => true,
                    slab::Freed::Refused => return,
                };
                #[cfg(feature = "mem-debug")]
                let to_buddy = true;
                hot_track(sz, -1);
                #[cfg(feature = "mem-debug")]
                {
                    let cz = ptr.add(sz);
                    for i in 0..REDZONE {
                        if core::ptr::read_volatile(cz.add(i)) != CANARY {
                            panic!(
                                "[heapcanary] HEAP OVERFLOW: ptr={:#x} size={} align={} clobbered at +{} (val={:#x})",
                                ptr as usize,
                                sz,
                                layout.align(),
                                i,
                                core::ptr::read_volatile(cz.add(i))
                            );
                        }
                    }
                    // Poison the payload before returning it to the buddy so a
                    // stale reader sees 0xa5a5... instead of plausible data.
                    core::ptr::write_bytes(ptr, POISON, sz);
                }
                HEAP_USED.fetch_sub(sz, Ordering::Relaxed);
                HEAP_LIVE[bucket_of(sz)].fetch_sub(1, Ordering::Relaxed);
                if sz >= BIG_ALLOC_MIN {
                    untrack_big_alloc(ptr as usize);
                }
                if to_buddy {
                    // Same refusal as `alloc`, with the only outcome a free can
                    // have: the block is leaked. A leaked block is recoverable
                    // (and this path runs only after a fault inside the
                    // allocator, i.e. once the kernel is already reporting a
                    // bug); a wedged CPU holding the heap lock with IRQs off is
                    // not.
                    if self.0.held_by_current_cpu() {
                        report_heap_reentrancy("dealloc (block leaked)", sz);
                    } else {
                        self.0.lock().dealloc(NonNull::new_unchecked(ptr), ext);
                    }
                }
                kernel_hal::kstats::note_heap_dealloc(if prof {
                    core::arch::x86_64::_rdtsc().wrapping_sub(t0)
                } else {
                    0
                });
            }
        }
    } else {
        pub fn init() {}

        pub fn heap_total() -> usize {
            0
        }

        /// No kernel heap of our own under `libos`: the host allocator is not
        /// a lock this kernel can be inside. Unused there -- the fault paths
        /// that ask are bare-metal only -- and kept so the two arms of the
        /// `cfg_if` expose the same surface.
        #[allow(dead_code)]
        pub fn heap_held_by_current_cpu() -> bool {
            false
        }
    }
}

#[cfg(feature = "hypervisor")]
mod rvm_extern_fn {
    use super::*;

    #[rvm::extern_fn(alloc_frame)]
    fn rvm_alloc_frame() -> Option<usize> {
        hal_frame_alloc()
    }

    #[rvm::extern_fn(dealloc_frame)]
    fn rvm_dealloc_frame(paddr: usize) {
        hal_frame_dealloc(&paddr)
    }

    #[rvm::extern_fn(phys_to_virt)]
    fn rvm_phys_to_virt(paddr: usize) -> usize {
        paddr + PHYSICAL_MEMORY_OFFSET
    }

    #[cfg(target_arch = "x86_64")]
    #[rvm::extern_fn(is_host_timer_interrupt)]
    fn rvm_is_host_timer_interrupt(vector: u8) -> bool {
        vector == 32
    }

    #[cfg(target_arch = "x86_64")]
    #[rvm::extern_fn(is_host_serial_interrupt)]
    fn rvm_is_host_serial_interrupt(vector: u8) -> bool {
        vector == 36
    }
}

/// The physical frame allocator's arithmetic.
///
/// `insert_regions` decides what RAM the kernel may hand out, and nothing had
/// ever compiled this file as a test target. `frames_inside` is pure, so the
/// interesting half is testable without a bitmap; the shared `FRAME_ALLOCATOR`
/// is a process global, so the tests that use it take a turnstile.
#[cfg(test)]
mod frame_tests {
    use super::*;

    const PAGE: usize = 1 << PAGE_BITS;

    /// The whole allocator lives in one `static`, and `insert` is not
    /// idempotent, so two tests inserting overlapping ranges see each other's
    /// frames. Serialise them.
    #[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
    fn alone_with_the_allocator() -> spin::MutexGuard<'static, ()> {
        static GUARD: spin::Mutex<()> = spin::Mutex::new(());
        GUARD.lock()
    }

    // --- the two conversions ---------------------------------------------

    #[test]
    fn a_frame_index_and_its_address_are_the_same_thing() {
        for idx in [0usize, 1, 0xff, 0x1_0000, (1 << 24) - 1] {
            assert_eq!(phys_addr_to_frame_idx(frame_idx_to_phys_addr(idx)), idx);
        }
        // Any address inside a frame names that frame.
        assert_eq!(phys_addr_to_frame_idx(0x1000), 1);
        assert_eq!(phys_addr_to_frame_idx(0x1fff), 1);
        assert_eq!(phys_addr_to_frame_idx(0x2000), 2);
    }

    /// The bitmap and the address cap have to agree: a `remove`/`insert` past
    /// the bitmap's last bit is what `MAX_MANAGED_PADDR_EXCLUSIVE` exists to
    /// prevent, and the two are written as unrelated literals.
    #[test]
    fn the_address_cap_is_exactly_what_the_bitmap_can_hold() {
        assert_eq!(
            phys_addr_to_frame_idx(MAX_MANAGED_PADDR_EXCLUSIVE),
            FrameAlloc::CAP,
            "MAX_MANAGED_PADDR_EXCLUSIVE and the BitAlloc type have drifted apart"
        );
    }

    // --- frames_inside ----------------------------------------------------

    #[test]
    fn an_aligned_region_is_taken_whole() {
        assert_eq!(frames_inside(&(0x10_0000..0x20_0000)), 0x100..0x200);
    }

    /// The bug. Both ends used to round OUTWARD -- the start implicitly (a bare
    /// `addr >> PAGE_BITS`) and the end explicitly (`idx(end - 1) + 1`) -- so an
    /// unaligned region handed out the page straddling each of its edges. That
    /// page belongs to whatever is on the other side: the tail of the kernel
    /// image, or the head of the next reserved range, now with two owners.
    #[test]
    fn an_unaligned_region_gives_up_the_partial_page_at_each_end() {
        // 0x1800..0x4800 covers all of frame 2 and part of frames 1 and 4.
        assert_eq!(frames_inside(&(0x1800..0x4800)), 2..4);
    }

    #[test]
    fn a_region_too_small_to_hold_one_whole_page_yields_nothing() {
        assert!(frames_inside(&(0x1800..0x1900)).is_empty());
        assert!(frames_inside(&(0x1001..0x2000)).is_empty());
        assert!(frames_inside(&(0x1000..0x1fff)).is_empty());
        // ...and exactly one page does yield it.
        assert_eq!(frames_inside(&(0x1000..0x2000)), 1..2);
    }

    #[test]
    fn an_empty_or_backwards_region_yields_nothing() {
        assert!(frames_inside(&(0x2000..0x2000)).is_empty());
        assert!(frames_inside(&(0x4000..0x2000)).is_empty());
    }

    /// Frame 0 is never handed out: a physical address of 0 is how every
    /// "allocation failed" is spelled, so a real frame at 0 would be
    /// indistinguishable from a failure.
    #[test]
    fn frame_zero_is_never_handed_out() {
        assert_eq!(frames_inside(&(0..0x4000)), 1..4);
        assert!(frames_inside(&(0..0x1000)).is_empty());
    }

    #[test]
    fn a_region_past_the_bitmap_is_clipped_to_it() {
        let cap = MAX_MANAGED_PADDR_EXCLUSIVE;
        assert_eq!(
            frames_inside(&(cap - 2 * PAGE..cap + 0x1_0000)),
            FrameAlloc::CAP - 2..FrameAlloc::CAP,
        );
        assert!(
            frames_inside(&(cap..cap + 0x1_0000)).is_empty(),
            "a region entirely above the cap is not ours to manage"
        );
    }

    // --- insert_regions and the allocator --------------------------------

    /// What `insert_regions` promises: every frame it reports as managed can be
    /// allocated, and none outside the region can.
    #[test]
    fn the_frames_a_region_contributes_are_the_ones_it_hands_out() {
        let _alone = alone_with_the_allocator();
        let base = 0x40_0000; // frame 0x400, well away from the other tests
        insert_regions(&[base..base + 3 * PAGE]);
        let mut got = alloc::vec::Vec::new();
        for _ in 0..3 {
            let addr = frame_alloc(1, 0).expect("three frames went in");
            assert!(
                (base..base + 3 * PAGE).contains(&addr),
                "{addr:#x} is outside the region that was inserted"
            );
            got.push(addr);
        }
        got.sort_unstable();
        got.dedup();
        assert_eq!(got.len(), 3, "the same frame was handed out twice");
        for addr in got {
            frame_dealloc(addr);
        }
    }

    /// The accounting `/proc/meminfo` reads: bytes in, bytes out, and back to
    /// where it started.
    #[test]
    fn allocating_and_freeing_leaves_the_counters_where_they_were() {
        let _alone = alone_with_the_allocator();
        let base = 0x50_0000;
        let (used_before, total_before) = frame_stats();
        insert_regions(&[base..base + 4 * PAGE]);
        assert_eq!(
            frame_stats().1 - total_before,
            4 * PAGE,
            "four pages of managed memory"
        );
        let addr = frame_alloc(2, 0).expect("two contiguous frames");
        assert_eq!(frame_stats().0 - used_before, 2 * PAGE);
        frame_dealloc(addr);
        frame_dealloc(addr + PAGE);
        assert_eq!(frame_stats().0, used_before);
    }

    #[test]
    fn a_contiguous_request_comes_back_contiguous_and_aligned() {
        let _alone = alone_with_the_allocator();
        let base = 0x60_0000;
        insert_regions(&[base..base + 16 * PAGE]);
        let addr = frame_alloc(4, 2).expect("four frames aligned to four");
        assert_eq!(addr % (4 * PAGE), 0, "{addr:#x} is not 4-page aligned");
        assert!((base..base + 16 * PAGE).contains(&addr));
        for i in 0..4 {
            frame_dealloc(addr + i * PAGE);
        }
    }

    /// The managed total is the bytes that went INTO the allocator, not the
    /// span of the descriptor they came from: reporting more than was inserted
    /// is how `/proc/meminfo` ends up with free RAM nobody can allocate.
    #[test]
    fn only_the_whole_pages_of_a_region_are_counted_as_managed() {
        let _alone = alone_with_the_allocator();
        let total_before = frame_stats().1;
        // Four pages of span, three whole pages inside it.
        insert_regions(&[0x80_0800..0x80_4800]);
        assert_eq!(frame_stats().1 - total_before, 3 * PAGE);
    }

    /// Inserting nothing is not an error, and it must not move the counters --
    /// this is the path an all-reserved memory map takes.
    #[test]
    fn a_region_that_contributes_no_whole_page_is_not_counted() {
        let _alone = alone_with_the_allocator();
        let before = frame_stats();
        insert_regions(&[0x7000_0001..0x7000_0fff, 0x8000..0x8000]);
        assert_eq!(frame_stats(), before);
    }

    /// libos has no paging context, so this is the branch that must do nothing
    /// rather than walk a page table that is not there.
    #[test]
    fn reserving_the_boot_page_tables_is_a_no_op_without_paging() {
        let _alone = alone_with_the_allocator();
        assert_eq!(
            kernel_hal::vm::current_vmtoken(),
            0,
            "libos reports no page-table root; this test would walk a null tree"
        );
        reserve_active_page_table_frames();
    }
}

/// Which page-table entries have a table under them.
///
/// The region arithmetic beside this already has its `frame_tests`; the walk
/// that keeps the live boot tree out of the frame pool did not, and its
/// per-entry decision is where a wrong answer is worst: descend into a huge
/// leaf and you read 4 KiB of somebody's data as 512 page-table entries; refuse
/// to descend and a live page table goes into the pool as ordinary RAM, which
/// is the corruption this function exists to prevent.
///
/// The heap half below is `#[cfg(not(feature = "libos"))]` and **cannot** be
/// opened the way `lang.rs` and `oops.rs` were: `buddy_system_allocator` and
/// `bitmap-allocator` are `target_os = "none"` dependencies, and `LockedHeap`'s
/// mutex comes from kernel-sync, which gates its impl on the same cfg -- so it
/// has no `lock()` on a host build. Opening it means changing a vendored crate,
/// not a cfg.
#[cfg(test)]
mod table_walk_tests {
    use super::table_child;

    /// Bits 51:12 are the address; the low twelve bits and bit 63 are flags.
    fn entry(child_pa: usize, flags: u64) -> u64 {
        child_pa as u64 | flags
    }

    #[test]
    fn an_absent_entry_has_nothing_under_it() {
        for level in 2..=4u8 {
            assert_eq!(table_child(entry(0x1000, 0), level), None, "level {level}");
            assert_eq!(
                table_child(entry(0x1000, 1 << 1 | 1 << 2), level),
                None,
                "level {level}: writable and user, but not present"
            );
        }
    }

    #[test]
    fn a_present_entry_names_the_table_under_it_with_its_flags_stripped() {
        // present | writable | user | accessed | dirty | NX
        let flags = 1 | 1 << 1 | 1 << 2 | 1 << 5 | 1 << 6 | 1 << 63;
        for level in 2..=4u8 {
            assert_eq!(
                table_child(entry(0x1234_5000, flags), level),
                Some(0x1234_5000),
                "level {level}"
            );
        }
    }

    /// Bit 7 is `PS` in a PDPTE (1 GiB) and a PDE (2 MiB): the entry maps memory
    /// instead of naming a table.
    #[test]
    fn a_huge_leaf_has_no_table_under_it() {
        let huge = 1 | 1 << 7;
        assert_eq!(table_child(entry(0x4000_0000, huge), 3), None, "1 GiB page");
        assert_eq!(table_child(entry(0x20_0000, huge), 2), None, "2 MiB page");
    }

    /// ...but in the PML4 bit 7 is reserved, not `PS`. Reading it as `PS` there
    /// would skip a whole 512 GiB quarter of the tree, live page tables and all,
    /// and every one of those frames would go into the pool as ordinary RAM.
    #[test]
    fn bit_seven_in_the_top_table_is_not_a_huge_page() {
        assert_eq!(
            table_child(entry(0x1000, 1 | 1 << 7), 4),
            Some(0x1000),
            "the PML4 has no PS bit"
        );
    }

    /// The hole. The walk guarded its *reservation* with `table_pa != 0` and
    /// then dereferenced `phys_to_virt(table_pa)` anyway, so a present entry
    /// naming frame 0 -- the shape of a half-overwritten entry, and nothing
    /// legitimate -- sent it reading frame 0 as a page table and recursing into
    /// whatever it found there. In the one function whose whole job is to stop a
    /// recycled page table from triple faulting the machine.
    #[test]
    fn a_present_entry_naming_frame_zero_is_not_followed() {
        for level in 2..=4u8 {
            assert_eq!(table_child(entry(0, 1), level), None, "level {level}");
            assert_eq!(
                table_child(entry(0, 1 | 1 << 1 | 1 << 5), level),
                None,
                "level {level} with flags"
            );
        }
    }
}

/// Which addresses the kernel heap owns, and the question the allocator has to
/// answer before it touches a block.
///
/// A buddy allocator's free lists are *intrusive*: a freed block's own first
/// words become the list links. So a `dealloc` of a pointer the heap never
/// handed out does not fail — it splices a wild address into the free list,
/// and the damage surfaces later, somewhere else, as the allocator walking
/// into memory that is not its own. That is what a KERNEL STOP on real
/// hardware showed: a read of `0xffff_ffff_ffff_ffff` from inside
/// `GlobalAlloc::dealloc`, with the heap lock held and never released, and
/// three unrelated subsystems faulting on near-null pointers behind it. By
/// then nothing on the screen named the code that did it.
///
/// This module is what lets the allocator refuse. It is deliberately separate
/// from the allocator itself (which is `cfg(not(libos))` and so compiles for
/// no `cargo test` at all) so the judgement has tests.
// The allocator this serves is `cfg(not(libos))`, so in a libos build nothing
// here has a caller -- and `#![deny(warnings)]` makes that an error rather than
// the quiet dead code it is. The host test suite (which builds libos) does
// exercise it.
#[cfg_attr(feature = "libos", allow(dead_code))]
pub mod heap_regions {
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Regions the registry can hold: the static arena, plus one per elastic
    /// growth.
    ///
    /// This has to cover the allocator's *worst* case, not its typical one. A
    /// growth takes 32 MiB when it can, but under memory pressure it falls
    /// back to 8 MiB and then 2 MiB, so the heap can reach its 2 GiB ceiling
    /// from a 512 MiB arena in 768 small steps. An overflow does not merely
    /// lose a region: `check` then stops answering `Outside` at all, so the
    /// whole guard would switch itself off exactly on the machine that is
    /// short of memory. A `const` assertion next to the growth policy ties
    /// this number to those chunk sizes, so it cannot silently fall behind.
    ///
    /// At two words a region this is 16 KiB of `.bss`, which is the cheapest
    /// part of the bargain.
    pub const MAX_REGIONS: usize = 1024;

    static STARTS: [AtomicUsize; MAX_REGIONS] = [const { AtomicUsize::new(0) }; MAX_REGIONS];
    static ENDS: [AtomicUsize; MAX_REGIONS] = [const { AtomicUsize::new(0) }; MAX_REGIONS];
    /// Slots handed out. Always `>= COUNT`: a slot is reserved here, written,
    /// and only then published through `COUNT`.
    static RESERVED: AtomicUsize = AtomicUsize::new(0);
    /// Slots that are fully written. Readers never look past this.
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    static UNREGISTERED: AtomicUsize = AtomicUsize::new(0);
    /// The region that answered last. Every free runs through [`contains`], so
    /// the common case -- block after block out of the same region -- is two
    /// comparisons instead of a walk over every region the heap has grown.
    static HINT: AtomicUsize = AtomicUsize::new(0);

    /// Why a block must not be handed to the allocator.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BlockFault {
        /// A null pointer.
        Null,
        /// Not aligned to what its own layout asked for.
        Unaligned,
        /// `ptr + size` wraps the address space.
        Wraps,
        /// Inside no region the heap owns.
        Outside,
    }

    impl BlockFault {
        /// One phrase for a report.
        pub fn why(self) -> &'static str {
            match self {
                BlockFault::Null => "null pointer",
                BlockFault::Unaligned => "not aligned to its own layout",
                BlockFault::Wraps => "ptr + size wraps the address space",
                BlockFault::Outside => "outside every region the kernel heap owns",
            }
        }
    }

    /// Record a region the heap owns, `[start, end)`.
    ///
    /// Called from `init` for the static arena and from the elastic growth
    /// right where it hands the frames to the buddy. A region that does not
    /// fit is counted, not dropped: see [`check`].
    pub fn register(start: usize, end: usize) {
        if end <= start {
            return;
        }
        let slot = RESERVED.fetch_add(1, Ordering::SeqCst);
        if slot >= MAX_REGIONS {
            RESERVED.store(MAX_REGIONS, Ordering::SeqCst);
            UNREGISTERED.fetch_add(1, Ordering::SeqCst);
            return;
        }
        // End first: a reader that sees the start must see a complete pair.
        ENDS[slot].store(end, Ordering::SeqCst);
        STARTS[slot].store(start, Ordering::SeqCst);
        // Publish last, and in slot order. A reader that saw `COUNT` cover a
        // slot still being written would find it empty and call a perfectly
        // sound block wild -- which leaks it and prints a false accusation.
        // Growth is serialised behind the heap lock, so this never spins in
        // practice; it is here so that the invariant does not rest on that.
        while COUNT
            .compare_exchange(slot, slot + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    /// Whether `[addr, addr + size)` lies inside one region.
    ///
    /// One region, not several: the heap's regions are separate allocations
    /// and a block never straddles two.
    pub fn contains(addr: usize, size: usize) -> bool {
        let Some(end) = addr.checked_add(size) else {
            return false;
        };
        let n = COUNT.load(Ordering::SeqCst).min(MAX_REGIONS);
        let hint = HINT.load(Ordering::Relaxed);
        if hint < n && in_region(hint, addr, end) {
            return true;
        }
        for i in 0..n {
            if i != hint && in_region(i, addr, end) {
                HINT.store(i, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    fn in_region(i: usize, addr: usize, end: usize) -> bool {
        let start = STARTS[i].load(Ordering::SeqCst);
        start != 0 && addr >= start && end <= ENDS[i].load(Ordering::SeqCst)
    }

    /// Regions on record.
    pub fn registered() -> usize {
        COUNT.load(Ordering::SeqCst).min(MAX_REGIONS)
    }

    /// Regions the heap owns that did not fit in the registry.
    pub fn unregistered() -> usize {
        UNREGISTERED.load(Ordering::SeqCst)
    }

    /// Judge a block the allocator is about to take back, or has just handed
    /// out. `None` means "nothing to say" — which is not the same as "sound".
    ///
    /// Two deliberate silences, both so that a healthy kernel is never
    /// slandered. Before `init` there are no regions and no basis for an
    /// opinion; and once a growth has failed to fit in the registry, the
    /// registry no longer knows the whole heap, so `Outside` stops being
    /// evidence. The cheap checks — null, alignment, wrap — hold either way,
    /// because they need no map of the heap at all.
    pub fn check(ptr: usize, size: usize, align: usize) -> Option<BlockFault> {
        if ptr == 0 {
            return Some(BlockFault::Null);
        }
        if align != 0 && !ptr.is_multiple_of(align) {
            return Some(BlockFault::Unaligned);
        }
        if ptr.checked_add(size).is_none() {
            return Some(BlockFault::Wraps);
        }
        if registered() == 0 || unregistered() > 0 {
            return None;
        }
        if contains(ptr, size) {
            return None;
        }
        Some(BlockFault::Outside)
    }

    /// Forget every region. Tests only — the heap's regions are never given
    /// back while the kernel runs.
    #[cfg(test)]
    pub(super) fn reset() {
        for i in 0..MAX_REGIONS {
            STARTS[i].store(0, Ordering::SeqCst);
            ENDS[i].store(0, Ordering::SeqCst);
        }
        COUNT.store(0, Ordering::SeqCst);
        RESERVED.store(0, Ordering::SeqCst);
        UNREGISTERED.store(0, Ordering::SeqCst);
        HINT.store(0, Ordering::SeqCst);
    }
}

/// The front cache's free lists, kept apart from the lock and the statics so
/// the host can test them over an ordinary buffer.
///
/// A cached block's first word is the link to the next free block of its
/// class, and nothing used to look at it: `pop` read the word and made it the
/// new head. So a single write into a freed block -- a stale pointer, a double
/// free, an object freed while something still held it -- became, one
/// allocation later, an allocation AT whatever value had been written, and
/// the next owner of that class wrote its fields through it, on some other
/// CPU, in code that had done nothing wrong. That is the shape of what the
/// captures keep showing after the direction flag was fixed: a
/// `BTreeMap<usize, PageState>` writing through a clock value, a buddy walk
/// faulting on `0x6cffffff47`, a `TaskCollection` whose `Vec` length is a
/// heap pointer. One bad write, and every report blamed whoever came next.
///
/// So the lists stop trusting their own words:
///
/// * `push` writes [`tag_of`] into the second word of every block of 16 bytes
///   or more, and `pop` refuses a block whose mark is gone;
/// * `pop` refuses a link that is not a free block of this class the heap
///   owns, a zero with blocks still counted behind it, and a link out of the
///   last block;
/// * `push` refuses a block that already carries its mark, or is already the
///   head: freed twice. Pushing it again would put it in the list twice and
///   hand it to two owners.
///
/// A refused block is leaked with the rest of its list, never handed out:
/// whoever wrote into it may still hold it, nothing behind it can be reached
/// without trusting the word that was just caught, and a leak is bytes where
/// the alternative is the machine. The refusal is reported at the block, so
/// the report names the block that was written after its free instead of the
/// code that allocated it next.
// `mem-debug` belongs here for the same reason `libos` does: the `slab` module
// that uses these lists is `cfg(not(feature = "mem-debug"))` -- memory
// debugging wants real buddy round-trips for its canary -- so with the feature
// on nothing calls them and `deny(warnings)` counts them as dead code.
//
// Without this, `MEM_DEBUG=1` did not compile at all: 18 `never used` errors.
// That is why the feature sat here since it was written with no way to turn it
// on -- no Makefile switch, and no job compiling it.
#[cfg_attr(any(feature = "libos", feature = "mem-debug"), allow(dead_code))]
pub mod free_lists {
    use core::alloc::Layout;

    // Cache buddy classes 2^3 (8 B, the buddy minimum) .. 2^12 (4 KiB).
    // That covers every hot fork object and the general small-allocation
    // churn; larger buffers (I/O, 1 MiB readahead) churn rarely and would
    // pin too much idle RAM, so they go straight to the buddy.
    pub const MIN_CLASS: usize = 3; // 2^3 = 8 B
    pub const MAX_CLASS: usize = 12; // 2^12 = 4 KiB
    pub const NUM_CLASSES: usize = MAX_CLASS - MIN_CLASS + 1;
    // Per-class cap. Worst case is the top class: 4 KiB * 1024 = 4 MiB;
    // the full table pins ≈8 MiB out of a 512 MiB heap. Bounding it keeps
    // the buddy from starving of large contiguous blocks and stops idle
    // RAM accumulating in the cache. A freed block past the cap is handed
    // to the buddy (where its buddy may coalesce) instead of cached.
    pub const CAP: u32 = 1024;

    /// What a free block of the cache holds in its second word. Mixed with
    /// the block's own address, so a value copied out of one free block into
    /// another live object can never pass for the tag of the second.
    const FREE_TAG: usize = 0x5a1b_f4ee_b10c_c0de;

    pub fn tag_of(block: usize) -> usize {
        FREE_TAG ^ block
    }

    /// Bytes in a block of class `i`.
    pub const fn class_size(i: usize) -> usize {
        1 << (i + MIN_CLASS)
    }

    /// Cache-array index for `layout`, or `None` when it is larger than
    /// the top cached class. The class formula matches
    /// buddy_system_allocator exactly, so a cached block is byte-for-byte
    /// what the buddy would have handed out.
    #[inline]
    pub fn cache_slot(layout: Layout) -> Option<usize> {
        // Guard before `next_power_of_two` so an oversize request can
        // never overflow it (and matches the buddy's own bypass of the
        // cache for large blocks).
        if layout.size() > (1 << MAX_CLASS) || layout.align() > (1 << MAX_CLASS) {
            return None;
        }
        let size = core::cmp::max(
            layout.size().next_power_of_two(),
            core::cmp::max(layout.align(), core::mem::size_of::<usize>()),
        );
        // size ∈ [8, 4096] given the guard, so class ∈ [3, 12].
        Some(size.trailing_zeros() as usize - MIN_CLASS)
    }

    /// What was wrong with a free block's link.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LinkFault {
        /// Not a block of this class the heap owns.
        Outside,
        /// A block of the heap's, but one that does not carry the free mark.
        NotFree,
        /// Zero, with more free blocks still counted behind it.
        Zeroed,
        /// Non-zero, from the last free block of its class.
        PastEnd,
    }

    /// A block the cache refused, and what was found in it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Broken {
        /// `block`'s link to the next free block is `link`, which is wrong in
        /// the way `why` says: something wrote over the block's first word
        /// after it was freed. `lost` more blocks were behind it.
        Link {
            block: usize,
            link: usize,
            lost: u32,
            why: LinkFault,
        },
        /// `block`'s second word holds `found` instead of its free mark:
        /// something wrote into the block after it was freed. `lost` more
        /// blocks were behind it.
        Tag {
            block: usize,
            found: usize,
            lost: u32,
        },
        /// `block` was freed while already free.
        DoubleFree { block: usize },
    }

    impl Broken {
        pub fn block(self) -> usize {
            match self {
                Broken::Link { block, .. }
                | Broken::Tag { block, .. }
                | Broken::DoubleFree { block } => block,
            }
        }
    }

    pub struct SlabCache {
        // head[i]: first free block of class MIN_CLASS+i (0 = empty). The
        // block's first word holds the next pointer while it is cached.
        head: [usize; NUM_CLASSES],
        count: [u32; NUM_CLASSES],
    }

    impl SlabCache {
        pub const fn new() -> Self {
            SlabCache {
                head: [0; NUM_CLASSES],
                count: [0; NUM_CLASSES],
            }
        }

        /// Blocks cached in class `i`.
        #[cfg(test)]
        pub fn count(&self, i: usize) -> u32 {
            self.count[i]
        }

        /// Take a block of class `i`; `Ok(None)` when the class is empty.
        ///
        /// `owns(addr, size)` answers whether `[addr, addr + size)` is a block
        /// of the heap's, aligned to `size`.
        ///
        /// # Safety
        /// Every block in the lists must have been given to [`push`] and be
        /// memory this cache may read and write.
        pub unsafe fn pop(
            &mut self,
            i: usize,
            owns: impl Fn(usize, usize) -> bool,
        ) -> Result<Option<usize>, Broken> {
            let block = self.head[i];
            if block == 0 {
                return Ok(None);
            }
            let size = class_size(i);
            let word = core::mem::size_of::<usize>();
            let tagged = size >= 2 * word;
            // Whatever is refused below, the rest of the list goes with it:
            // nothing behind this block can be reached without trusting a
            // word that was just shown to have been written by someone else.
            let lost = self.count[i].saturating_sub(1);
            if tagged {
                let found = unsafe { core::ptr::read((block + word) as *const usize) };
                if found != tag_of(block) {
                    self.drop_class(i);
                    return Err(Broken::Tag { block, found, lost });
                }
            }
            let link = unsafe { core::ptr::read(block as *const usize) };
            let why = if link == 0 {
                (lost > 0).then_some(LinkFault::Zeroed)
            } else if !owns(link, size) {
                Some(LinkFault::Outside)
            } else if tagged
                // A link that lands on a block of the heap's can still be
                // wrong -- a heap pointer is exactly what a stale owner
                // stores -- so the block it names must be free as well.
                && unsafe { core::ptr::read((link + word) as *const usize) } != tag_of(link)
            {
                Some(LinkFault::NotFree)
            } else if lost == 0 {
                Some(LinkFault::PastEnd)
            } else {
                None
            };
            if let Some(why) = why {
                self.drop_class(i);
                return Err(Broken::Link {
                    block,
                    link,
                    lost,
                    why,
                });
            }
            if tagged {
                // A live block must not carry the mark: `push` reads it as
                // "already free".
                unsafe { core::ptr::write((block + word) as *mut usize, 0) };
            }
            self.head[i] = link;
            self.count[i] -= 1;
            Ok(Some(block))
        }

        /// Forget class `i`'s list, leaking every block in it.
        fn drop_class(&mut self, i: usize) {
            self.head[i] = 0;
            self.count[i] = 0;
        }

        /// Cache a freed block of class `i`. `Ok(false)` means the class is
        /// full and the caller must hand the block to the buddy.
        ///
        /// # Safety
        /// `block` must be a block of class `i` that the caller is freeing.
        pub unsafe fn push(&mut self, i: usize, block: usize) -> Result<bool, Broken> {
            let size = class_size(i);
            let tagged = size >= 2 * core::mem::size_of::<usize>();
            let tag = (block + core::mem::size_of::<usize>()) as *mut usize;
            if block == self.head[i] || (tagged && unsafe { core::ptr::read(tag) } == tag_of(block))
            {
                return Err(Broken::DoubleFree { block });
            }
            if self.count[i] >= CAP {
                return Ok(false);
            }
            // Push: stash the old head in the block's first word.
            unsafe { core::ptr::write(block as *mut usize, self.head[i]) };
            if tagged {
                unsafe { core::ptr::write(tag, tag_of(block)) };
            }
            self.head[i] = block;
            self.count[i] += 1;
            Ok(true)
        }
    }
}

/// Who freed a block, for the report that finds it written after the free.
///
/// The heap cannot see the write itself; it sees the damage when the block
/// comes round again, and by then the code that freed it is long gone from
/// every stack. So every free leaves its caller's return addresses here, and
/// the report looks the block up. A ring: old entries are overwritten, which
/// a report says rather than naming somebody else.
// Same as `free_lists`: under `mem-debug` the `slab` that feeds this ring is
// not compiled, so the ring is left with no callers.
#[cfg_attr(any(feature = "libos", feature = "mem-debug"), allow(dead_code))]
pub mod free_ring {
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Return addresses kept per free.
    pub const DEPTH: usize = 6;

    struct Slot {
        block: AtomicUsize,
        rets: [AtomicUsize; DEPTH],
    }

    impl Slot {
        const fn new() -> Self {
            Slot {
                block: AtomicUsize::new(0),
                rets: [const { AtomicUsize::new(0) }; DEPTH],
            }
        }
    }

    pub struct FreeRing<const N: usize> {
        next: AtomicUsize,
        slots: [Slot; N],
    }

    impl<const N: usize> FreeRing<N> {
        pub const fn new() -> Self {
            FreeRing {
                next: AtomicUsize::new(0),
                slots: [const { Slot::new() }; N],
            }
        }

        /// Frees the ring remembers.
        pub const fn len(&self) -> usize {
            N
        }

        /// Record that `block` was freed from `rets`.
        pub fn note(&self, block: usize, rets: [usize; DEPTH]) {
            let n = self.next.fetch_add(1, Ordering::Relaxed);
            let slot = &self.slots[n % N];
            // Unpublish, fill, publish: a reader that sees `block` before and
            // after reading the addresses read this free's addresses.
            slot.block.store(0, Ordering::Release);
            for (r, v) in slot.rets.iter().zip(rets) {
                r.store(v, Ordering::Relaxed);
            }
            slot.block.store(block, Ordering::Release);
        }

        /// The latest free of `block` still in the ring: its return addresses,
        /// and how many frees anywhere in the heap came after it.
        pub fn find(&self, block: usize) -> Option<([usize; DEPTH], usize)> {
            if block == 0 {
                return None;
            }
            let newest = self.next.load(Ordering::Relaxed);
            for age in 0..N.min(newest) {
                let slot = &self.slots[(newest - 1 - age) % N];
                if slot.block.load(Ordering::Acquire) != block {
                    continue;
                }
                let mut rets = [0; DEPTH];
                for (v, r) in rets.iter_mut().zip(&slot.rets) {
                    *v = r.load(Ordering::Relaxed);
                }
                if slot.block.load(Ordering::Acquire) == block {
                    return Some((rets, age));
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod heap_region_tests {
    //! A wild `dealloc` is not a failed free: the buddy's lists are intrusive,
    //! so it is a wild WRITE into the allocator's own bookkeeping, and what
    //! surfaces afterwards is unrelated code faulting on pointers that make no
    //! sense. These tests are about never being wrong in the direction that
    //! would make the allocator refuse a sound block.

    use super::heap_regions::{self, BlockFault};

    /// The registry is one `static`; serialise.
    #[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
    fn alone() -> spin::MutexGuard<'static, ()> {
        static GUARD: spin::Mutex<()> = spin::Mutex::new(());
        GUARD.lock()
    }

    const ARENA: usize = 0xffff_ff00_1000_0000;
    const ARENA_LEN: usize = 512 * 1024 * 1024;

    fn with_arena() {
        heap_regions::reset();
        heap_regions::register(ARENA, ARENA + ARENA_LEN);
    }

    /// The whole point: a pointer the heap never handed out is named, instead
    /// of being spliced into a free list.
    #[test]
    fn a_pointer_from_outside_the_heap_is_refused() {
        let _alone = alone();
        with_arena();
        assert_eq!(
            heap_regions::check(0xffff_8000_0000_0000, 64, 8),
            Some(BlockFault::Outside)
        );
    }

    /// ... and every ordinary block inside it is not. A check that cried wolf
    /// would leak the whole heap one block at a time.
    #[test]
    fn every_block_inside_the_arena_passes() {
        let _alone = alone();
        with_arena();
        for off in [0usize, 8, 4096, ARENA_LEN / 2, ARENA_LEN - 64] {
            assert_eq!(
                heap_regions::check(ARENA + off, 64, 8),
                None,
                "off={off:#x}"
            );
        }
    }

    /// A block that starts inside and ends past the end is outside: it is the
    /// shape a size that no longer matches its allocation takes.
    #[test]
    fn a_block_running_past_the_end_of_its_region_is_refused() {
        let _alone = alone();
        with_arena();
        assert_eq!(
            heap_regions::check(ARENA + ARENA_LEN - 32, 64, 8),
            Some(BlockFault::Outside)
        );
    }

    /// A grown region is as much the heap as the static arena.
    #[test]
    fn a_block_in_a_grown_region_passes() {
        let _alone = alone();
        with_arena();
        const GROWN: usize = 0xffff_8002_0000_0000;
        assert_eq!(
            heap_regions::check(GROWN + 128, 64, 8),
            Some(BlockFault::Outside),
            "not registered yet"
        );
        heap_regions::register(GROWN, GROWN + 32 * 1024 * 1024);
        assert_eq!(heap_regions::check(GROWN + 128, 64, 8), None);
    }

    /// The hint is a fast path, never an answer. Blocks arriving from two
    /// regions in turn must all pass: a hint that shortcut the walk without
    /// re-checking would refuse every other free.
    #[test]
    fn alternating_between_two_regions_passes_every_time() {
        let _alone = alone();
        heap_regions::reset();
        const A: usize = 0x1000_0000;
        const B: usize = 0x9000_0000;
        heap_regions::register(A, A + 0x10_0000);
        heap_regions::register(B, B + 0x10_0000);
        for i in 0..8usize {
            assert_eq!(heap_regions::check(A + i * 64, 64, 8), None, "A round {i}");
            assert_eq!(heap_regions::check(B + i * 64, 64, 8), None, "B round {i}");
        }
        assert_eq!(
            heap_regions::check(A + 0x20_0000, 64, 8),
            Some(heap_regions::BlockFault::Outside),
            "the gap between the two regions is not the heap"
        );
    }

    /// A block never straddles two regions, because they are separate
    /// allocations -- so one that appears to is a corrupt size, not a merge.
    #[test]
    fn a_block_straddling_two_regions_is_refused() {
        let _alone = alone();
        heap_regions::reset();
        heap_regions::register(0x1_0000, 0x2_0000);
        heap_regions::register(0x2_0000, 0x3_0000);
        assert_eq!(
            heap_regions::check(0x1_fff0, 0x40, 8),
            Some(BlockFault::Outside)
        );
    }

    /// Null, misalignment and a wrapping end need no map of the heap, so they
    /// are answered even before `init` has registered anything.
    #[test]
    fn the_cheap_faults_are_answered_with_no_regions_on_record() {
        let _alone = alone();
        heap_regions::reset();
        assert_eq!(heap_regions::check(0, 64, 8), Some(BlockFault::Null));
        assert_eq!(
            heap_regions::check(0x1004, 64, 8),
            Some(BlockFault::Unaligned)
        );
        assert_eq!(
            heap_regions::check(usize::MAX - 8, 64, 1),
            Some(BlockFault::Wraps)
        );
    }

    /// With nothing registered there is no basis for `Outside`, and guessing
    /// would make the allocator refuse every block the kernel frees before
    /// `memory::init` runs.
    #[test]
    fn before_init_nothing_is_called_outside_the_heap() {
        let _alone = alone();
        heap_regions::reset();
        assert_eq!(heap_regions::check(0xdead_0000, 64, 8), None);
    }

    /// Once a region has not fitted, the registry no longer knows the whole
    /// heap -- so it stops claiming a block is outside it. Getting this wrong
    /// turns a machine that merely grew its heap a lot into one that leaks
    /// every free.
    #[test]
    fn a_registry_that_overflowed_stops_claiming_blocks_are_outside() {
        let _alone = alone();
        heap_regions::reset();
        for i in 0..heap_regions::MAX_REGIONS + 1 {
            let base = 0x10_0000 + i * 0x10_0000;
            heap_regions::register(base, base + 0x1000);
        }
        assert_eq!(heap_regions::unregistered(), 1);
        assert_eq!(heap_regions::registered(), heap_regions::MAX_REGIONS);
        assert_eq!(heap_regions::check(0xffff_8000_0000_0000, 64, 8), None);
        // ... and the cheap ones still hold.
        assert_eq!(heap_regions::check(0, 64, 8), Some(BlockFault::Null));
    }

    /// An empty or inverted region is not recorded: it would otherwise take a
    /// slot and push a real region into the overflow count.
    /// The capacity is not a round number picked by feel: a 512 MiB arena
    /// growing to the 2 GiB ceiling in 2 MiB fallback steps is 768 growths,
    /// and if the registry cannot hold them it silently stops answering
    /// `Outside` on the machine that is short of memory. The `const`
    /// assertion beside the growth policy enforces this for the real
    /// constants; this pins the arithmetic so the number cannot be lowered
    /// here without a test saying why.
    #[test]
    fn the_registry_holds_every_region_the_growth_policy_can_create() {
        let worst_case = 1 + (2 * 1024 * 1024 * 1024 - 512 * 1024 * 1024) / (2 * 1024 * 1024);
        assert_eq!(worst_case, 769);
        assert!(
            heap_regions::MAX_REGIONS >= worst_case,
            "{} slots cannot hold {} regions",
            heap_regions::MAX_REGIONS,
            worst_case
        );
    }

    /// And filling it to the brim keeps the verdicts sound: the last region
    /// registered is still recognised, and nothing has been declared wild.
    #[test]
    fn a_full_registry_still_answers_for_its_last_region() {
        let _alone = alone();
        heap_regions::reset();
        for i in 0..heap_regions::MAX_REGIONS {
            let base = ARENA + i * 0x10_0000;
            heap_regions::register(base, base + 0x1000);
        }
        assert_eq!(heap_regions::registered(), heap_regions::MAX_REGIONS);
        assert_eq!(heap_regions::unregistered(), 0);
        let last = ARENA + (heap_regions::MAX_REGIONS - 1) * 0x10_0000;
        assert_eq!(heap_regions::check(last, 64, 8), None);
        assert_eq!(
            heap_regions::check(last + 0x8000, 64, 8),
            Some(BlockFault::Outside)
        );
    }

    #[test]
    fn an_empty_region_is_not_recorded() {
        let _alone = alone();
        heap_regions::reset();
        heap_regions::register(0x1000, 0x1000);
        heap_regions::register(0x2000, 0x1000);
        assert_eq!(heap_regions::registered(), 0);
    }
}

#[cfg(test)]
mod free_list_tests {
    //! The front cache's lists used to follow whatever a freed block's first
    //! word said. These tests write into freed blocks the way a stale owner
    //! does -- a clock value, a small integer, a pointer to a live block, a
    //! zero -- and require that the cache names the block and never hands out
    //! anything it was not given.

    use super::free_lists::{class_size, tag_of, Broken, LinkFault, SlabCache, CAP};
    use super::free_ring::{FreeRing, DEPTH};
    use std::alloc::Layout;

    /// 64-byte blocks: a tagged class.
    const C64: usize = 3;
    /// 8-byte blocks: one word, no room for the free mark.
    const C8: usize = 0;

    /// Real memory for the lists to write their links and marks into.
    struct Arena {
        base: usize,
        layout: Layout,
    }

    impl Arena {
        fn new(len: usize) -> Self {
            let layout = Layout::from_size_align(len, 4096).unwrap();
            let base = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
            assert_ne!(base, 0);
            Arena { base, layout }
        }

        /// The `n`th block of class `i`.
        fn block(&self, i: usize, n: usize) -> usize {
            assert!((n + 1) * class_size(i) <= self.layout.size());
            self.base + n * class_size(i)
        }

        fn owns(&self) -> impl Fn(usize, usize) -> bool + '_ {
            move |addr, size| {
                addr >= self.base
                    && addr + size <= self.base + self.layout.size()
                    && addr.is_multiple_of(size)
            }
        }

        fn write(&self, addr: usize, value: usize) {
            assert!(addr >= self.base && addr + 8 <= self.base + self.layout.size());
            unsafe { core::ptr::write(addr as *mut usize, value) }
        }

        fn read(&self, addr: usize) -> usize {
            assert!(addr >= self.base && addr + 8 <= self.base + self.layout.size());
            unsafe { core::ptr::read(addr as *const usize) }
        }
    }

    impl Drop for Arena {
        fn drop(&mut self) {
            unsafe { std::alloc::dealloc(self.base as *mut u8, self.layout) }
        }
    }

    /// The word the ninth capture found where a `BTreeMap` node pointer
    /// belonged: an instant, not an address.
    const CLOCK: usize = 0x0000_0012_a05f_2000;

    #[test]
    fn blocks_come_back_last_in_first_out_without_their_mark() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let (a, b, d) = (
            arena.block(C64, 0),
            arena.block(C64, 1),
            arena.block(C64, 2),
        );
        for x in [a, b, d] {
            assert_eq!(unsafe { c.push(C64, x) }, Ok(true));
        }
        assert_eq!(c.count(C64), 3);
        for x in [d, b, a] {
            assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(x)));
            // A live block carrying the mark would read as "already free" to
            // the next `push` of it.
            assert_eq!(arena.read(x + 8), 0);
        }
        assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(None));
        assert_eq!(c.count(C64), 0);
    }

    /// The ninth capture's shape: a clock value where a free block's link
    /// was. The block that holds it is named, and the clock value is never
    /// handed out as an allocation.
    #[test]
    fn a_link_overwritten_with_a_clock_value_is_caught_at_that_block() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let (a, b, d) = (
            arena.block(C64, 0),
            arena.block(C64, 1),
            arena.block(C64, 2),
        );
        for x in [a, b, d] {
            unsafe { c.push(C64, x) }.unwrap();
        }
        arena.write(b, CLOCK);
        assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(d)));
        assert_eq!(
            unsafe { c.pop(C64, arena.owns()) },
            Err(Broken::Link {
                block: b,
                link: CLOCK,
                lost: 1,
                why: LinkFault::Outside,
            })
        );
        // The rest of the list is gone, not followed.
        assert_eq!(c.count(C64), 0);
        assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(None));
    }

    /// A heap pointer is exactly what a stale owner stores, and it passes
    /// every address test. The block it names is live, so it has no mark.
    #[test]
    fn a_link_overwritten_with_a_live_block_is_caught() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let (a, b, live) = (
            arena.block(C64, 0),
            arena.block(C64, 1),
            arena.block(C64, 5),
        );
        for x in [a, b] {
            unsafe { c.push(C64, x) }.unwrap();
        }
        arena.write(b, live);
        assert_eq!(
            unsafe { c.pop(C64, arena.owns()) },
            Err(Broken::Link {
                block: b,
                link: live,
                lost: 1,
                why: LinkFault::NotFree,
            })
        );
    }

    /// A write that misses the link lands on the mark.
    #[test]
    fn a_write_over_the_free_mark_is_caught() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let (a, b) = (arena.block(C64, 0), arena.block(C64, 1));
        for x in [a, b] {
            unsafe { c.push(C64, x) }.unwrap();
        }
        arena.write(b + 8, 0x71);
        assert_eq!(
            unsafe { c.pop(C64, arena.owns()) },
            Err(Broken::Tag {
                block: b,
                found: 0x71,
                lost: 1,
            })
        );
    }

    /// Zero is a valid link only from the last block. Anywhere else it is the
    /// zero-writer this hunt began with, and it would have leaked the rest of
    /// the list without a word.
    #[test]
    fn a_zeroed_link_with_blocks_behind_it_is_caught() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let (a, b) = (arena.block(C64, 0), arena.block(C64, 1));
        for x in [a, b] {
            unsafe { c.push(C64, x) }.unwrap();
        }
        arena.write(b, 0);
        assert_eq!(
            unsafe { c.pop(C64, arena.owns()) },
            Err(Broken::Link {
                block: b,
                link: 0,
                lost: 1,
                why: LinkFault::Zeroed,
            })
        );
    }

    /// One-word blocks have no mark, so a link to another block of the heap
    /// passes the address test -- but not out of the last block.
    #[test]
    fn a_link_out_of_the_last_block_is_caught() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let (a, other) = (arena.block(C8, 0), arena.block(C8, 9));
        unsafe { c.push(C8, a) }.unwrap();
        arena.write(a, other);
        assert_eq!(
            unsafe { c.pop(C8, arena.owns()) },
            Err(Broken::Link {
                block: a,
                link: other,
                lost: 0,
                why: LinkFault::PastEnd,
            })
        );
    }

    /// Before: the second free put the block in the list twice, and two
    /// allocations later two owners shared it.
    #[test]
    fn a_block_freed_twice_is_refused_and_the_list_stays_sound() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let (a, b) = (arena.block(C64, 0), arena.block(C64, 1));
        for x in [a, b] {
            unsafe { c.push(C64, x) }.unwrap();
        }
        assert_eq!(
            unsafe { c.push(C64, a) },
            Err(Broken::DoubleFree { block: a })
        );
        assert_eq!(c.count(C64), 2);
        assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(b)));
        assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(a)));
        assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(None));
    }

    /// Without a mark, the head is the one double free a one-word class can
    /// still see -- and the one that would make the list point at itself.
    #[test]
    fn a_one_word_block_freed_twice_in_a_row_is_refused() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let a = arena.block(C8, 0);
        unsafe { c.push(C8, a) }.unwrap();
        assert_eq!(
            unsafe { c.push(C8, a) },
            Err(Broken::DoubleFree { block: a })
        );
        assert_eq!(c.count(C8), 1);
    }

    /// The everyday case must stay quiet: a block that was handed out and
    /// comes back is a free, not a double free.
    #[test]
    fn a_block_handed_out_and_freed_again_is_not_a_double_free() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let a = arena.block(C64, 0);
        unsafe { c.push(C64, a) }.unwrap();
        assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(a)));
        assert_eq!(unsafe { c.push(C64, a) }, Ok(true));
    }

    /// The mark is mixed with the block's address, so a live object holding
    /// a copy of another block's mark -- a struct copied out of freed memory
    /// -- is not taken for a free block.
    #[test]
    fn another_blocks_mark_in_a_live_block_is_not_a_double_free() {
        let arena = Arena::new(4096);
        let mut c = SlabCache::new();
        let (a, live) = (arena.block(C64, 0), arena.block(C64, 1));
        unsafe { c.push(C64, a) }.unwrap();
        arena.write(live + 8, tag_of(a));
        assert_eq!(unsafe { c.push(C64, live) }, Ok(true));
    }

    /// A full class hands the block back without writing into it: the buddy
    /// keeps its own links there.
    #[test]
    fn a_full_class_leaves_the_block_to_the_buddy_untouched() {
        let arena = Arena::new((CAP as usize + 1) * class_size(C64));
        let mut c = SlabCache::new();
        for n in 0..CAP as usize {
            assert_eq!(unsafe { c.push(C64, arena.block(C64, n)) }, Ok(true));
        }
        let extra = arena.block(C64, CAP as usize);
        arena.write(extra, 0x1111);
        arena.write(extra + 8, 0x2222);
        assert_eq!(unsafe { c.push(C64, extra) }, Ok(false));
        assert_eq!(arena.read(extra), 0x1111);
        assert_eq!(arena.read(extra + 8), 0x2222);
        assert_eq!(c.count(C64), CAP);
    }

    /// Whatever is written into free blocks, and wherever, `pop` hands out
    /// only blocks that were freed and are still free: never a written value,
    /// never a block twice.
    #[test]
    fn whatever_is_written_into_free_blocks_pop_hands_out_only_free_blocks() {
        const BLOCKS: usize = 64;
        let arena = Arena::new(BLOCKS * class_size(C64));
        let mut c = SlabCache::new();
        let mut free: Vec<usize> = Vec::new();
        let mut live: Vec<usize> = (0..BLOCKS).map(|n| arena.block(C64, n)).collect();
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        let (mut caught, mut handed) = (0, 0);
        for _ in 0..20_000 {
            match next() % 8 {
                0..=2 if !live.is_empty() => {
                    let b = live.swap_remove(next() % live.len());
                    if unsafe { c.push(C64, b) } == Ok(true) {
                        free.push(b);
                    }
                }
                3..=5 => match unsafe { c.pop(C64, arena.owns()) } {
                    Ok(Some(b)) => {
                        let at = free.iter().position(|&f| f == b);
                        assert!(at.is_some(), "handed out {b:#x}, which is not free");
                        free.swap_remove(at.unwrap());
                        live.push(b);
                        handed += 1;
                    }
                    Ok(None) => assert_eq!(c.count(C64), 0),
                    Err(_) => {
                        // Leaked with its list: neither free nor ever handed
                        // out again.
                        caught += 1;
                        assert_eq!(c.count(C64), 0);
                        free.clear();
                    }
                },
                6 if !free.is_empty() => {
                    // A stale owner writes into a free block.
                    let b = free[next() % free.len()];
                    let value = match next() % 5 {
                        0 => 0,
                        1 => 0x71,
                        2 => CLOCK + next() % 1000,
                        3 if !live.is_empty() => live[next() % live.len()],
                        _ => free[next() % free.len()],
                    };
                    arena.write(b + 8 * (next() % 2), value);
                }
                _ => {}
            }
        }
        assert!(caught > 0 && handed > 0, "caught={caught} handed={handed}");
    }

    #[test]
    fn the_ring_names_the_latest_free_of_a_block_and_how_long_ago() {
        let ring: FreeRing<8> = FreeRing::new();
        let rets = |x| [x; DEPTH];
        ring.note(0xa0, rets(1));
        ring.note(0xb0, rets(2));
        ring.note(0xa0, rets(3));
        assert_eq!(ring.find(0xa0), Some((rets(3), 0)));
        assert_eq!(ring.find(0xb0), Some((rets(2), 1)));
        assert_eq!(ring.find(0xc0), None);
        assert_eq!(ring.find(0), None);
    }

    /// A free the ring has overwritten is "no longer in the ring", never the
    /// free that took its slot.
    #[test]
    fn a_free_older_than_the_ring_is_not_attributed_to_anyone() {
        let ring: FreeRing<4> = FreeRing::new();
        ring.note(0xa0, [1; DEPTH]);
        for n in 1..=4 {
            ring.note(0xa0 + n * 0x10, [2; DEPTH]);
        }
        assert_eq!(ring.find(0xa0), None);
        assert_eq!(ring.find(0xe0), Some(([2; DEPTH], 0)));
        assert_eq!(ring.len(), 4);
    }
}
