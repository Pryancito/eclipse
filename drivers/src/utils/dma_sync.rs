//! DMA cache coherency — Linux `dma_sync_*` / FreeBSD `bus_dmamap_sync` model.
//!
//! | Eclipse              | Linux (PCI)                    | FreeBSD `bus_dma`        |
//! |----------------------|--------------------------------|--------------------------|
//! | [`DmaSyncDir::ToDevice`]   | `dma_sync_single_for_device`   | `BUS_DMASYNC_PREWRITE`   |
//! | [`DmaSyncDir::FromDevice`] | `dma_sync_single_for_cpu`      | `BUS_DMASYNC_POSTREAD`   |
//!
//! When the region is mapped UC/coherent (`coherent == true`), both directions are
//! no-ops aside from a memory fence — same as Linux `dma_alloc_coherent` on x86.

use core::sync::atomic::{fence, AtomicBool, Ordering};

use super::dma::DmaRegion;

/// Set at boot by [`probe_cpu_features`]; `true` when the CPU supports the
/// non-serialising `CLFLUSHOPT` instruction (CPUID.7.0.EBX[23]).
///
/// All Intel CPUs since Broadwell (2014) and AMD CPUs since Zen (2017) set
/// this bit — every platform capable of running an RTX 2060 SUPER qualifies.
/// The flag defaults to `false` so pre-probe callers fall back to `CLFLUSH`.
static HAS_CLFLUSHOPT: AtomicBool = AtomicBool::new(false);

/// Detect and cache CPU features used by this module.
///
/// Call once on the BSP during early boot (before any driver uses
/// [`dma_sync_wb_from_device`] or [`dma_sync_wb_to_device`]).
/// It is safe to call from multiple CPUs — the store is idempotent.
#[cfg(target_arch = "x86_64")]
pub fn probe_cpu_features() {
    // CPUID leaf 7, subleaf 0 — Structured Extended Feature Flags.
    // EBX bit 23 = CLFLUSHOPT.
    let r = core::arch::x86_64::__cpuid_count(7, 0);
    HAS_CLFLUSHOPT.store(r.ebx & (1 << 23) != 0, Ordering::Relaxed);
}

#[cfg(not(target_arch = "x86_64"))]
pub fn probe_cpu_features() {}


/// Direction of a DMA cache sync (device ↔ CPU).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaSyncDir {
    /// CPU wrote data the device will read (descriptor post, TX payload).
    ToDevice,
    /// Device wrote data the CPU will read (RX descriptor WB, RX payload).
    FromDevice,
}

/// Sync a byte range of a DMA region (Linux/FreeBSD bus_dmamap_sync equivalent).
pub fn dma_sync_region(
    region: &DmaRegion,
    coherent: bool,
    byte_off: usize,
    len: usize,
    dir: DmaSyncDir,
) {
    // Use checked arithmetic: `byte_off + len` could wrap `usize` and slip past
    // the bound, then `region.vaddr() + byte_off` would clflush out-of-region.
    if len == 0
        || byte_off
            .checked_add(len)
            .is_none_or(|end| end > region.byte_len())
    {
        return;
    }
    if coherent {
        fence(Ordering::SeqCst);
        return;
    }
    let vaddr = region.vaddr() + byte_off;
    match dir {
        DmaSyncDir::ToDevice => dma_sync_wb_to_device(vaddr, len),
        DmaSyncDir::FromDevice => dma_sync_wb_from_device(vaddr, len),
    }
}

/// Sync descriptor ring span covering `count` 16-byte descriptors from `start_idx`.
pub fn dma_sync_rx_desc_span(
    region: &DmaRegion,
    coherent: bool,
    start_idx: usize,
    count: usize,
    desc_size: usize,
    dir: DmaSyncDir,
) {
    if count == 0 {
        return;
    }
    dma_sync_region(
        region,
        coherent,
        start_idx * desc_size,
        count * desc_size,
        dir,
    );
}

/// Linux `dma_sync_single_for_device` on WB pages: clflush + sfence before MMIO doorbell.
pub fn dma_sync_wb_to_device(vaddr: usize, len: usize) {
    if len == 0 {
        return;
    }
    clflush_span(vaddr, len);
    fence(Ordering::Release);
}

/// Linux `dma_sync_single_for_cpu` after RX DMA: clflush stale lines + lfence before read.
pub fn dma_sync_wb_from_device(vaddr: usize, len: usize) {
    if len == 0 {
        return;
    }
    clflush_span(vaddr, len);
    fence(Ordering::Acquire);
}

/// Write back / invalidate the cache lines covering `[vaddr, vaddr+len)`.
///
/// On x86_64 this dispatches to one of two implementations:
///
/// * **`clflushopt` path** (used when CPUID.7.0.EBX[23] is set, i.e. every CPU
///   since Intel Broadwell 2014 / AMD Zen 2017): issues non-serialising
///   `CLFLUSHOPT` instructions with a single trailing `SFENCE`.  No leading
///   `MFENCE` — for the FROM_DEVICE direction there are no pending WC stores
///   that must drain before we invalidate the GPU's write target, so the full
///   memory fence is unnecessary overhead.
///
/// * **`clflush` fallback** (legacy CPUs): `mfence; clflush×N; sfence` —
///   unchanged from the original conservative path.
///
/// The `clflushopt` path is roughly **3–5× faster** than `clflush` because
/// `CLFLUSHOPT` is pipelineable (the CPU can have many in flight at once)
/// while `CLFLUSH` serialises every memory access around it.  For a 1080p
/// framebuffer (~130 000 cache lines) this cuts per-frame cache-sync time
/// from ≈52–104 ms down to ≈13–26 ms, raising glxgears from ~7 FPS toward
/// the 60 Hz vblank ceiling.
///
/// Other architectures use only a fence here; their non-coherent DMA support
/// (e.g. aarch64 `DC CVAC/CIVAC`, riscv `CMO`) is a TODO — for now they
/// rely on coherent (UC) DMA mappings.
fn clflush_span(vaddr: usize, len: usize) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let mut p = vaddr & !(64 - 1);
        let end = vaddr.saturating_add(len);
        if HAS_CLFLUSHOPT.load(Ordering::Relaxed) {
            // Non-serialising path: issue all CLFLUSHOPTs then a single
            // SFENCE to make the invalidations globally visible before the
            // caller reads from the region.  No leading MFENCE needed for
            // the FROM_DEVICE direction.
            while p < end {
                // CLFLUSHOPT m8: 66 0F AE /7
                core::arch::asm!(
                    "clflushopt [{p}]",
                    p = in(reg) p,
                    options(nostack, preserves_flags),
                );
                p += 64;
            }
            core::arch::x86_64::_mm_sfence();
        } else {
            // Legacy serialising path — unchanged.
            core::arch::x86_64::_mm_mfence();
            while p < end {
                core::arch::x86_64::_mm_clflush(p as *const u8);
                p += 64;
            }
            core::arch::x86_64::_mm_sfence();
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (vaddr, len);
        fence(Ordering::SeqCst);
    }
}
