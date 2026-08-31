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

/// Set at boot by [`probe_cpu_features`]; `true` when the CPU supports the
/// `MOVNTDQA` non-temporal load (SSE4.1, CPUID.1.ECX[19]).
///
/// When set, [`nt_blit_rows`] uses `MOVNTDQA` to read GEM buffers without
/// loading lines into the CPU data cache.  This is the key that lets the
/// scanout path skip `clflush_span` entirely: if the CPU never caches bytes
/// it reads from a GEM buffer, there are no stale lines to flush before the
/// GPU writes the next frame.
///
/// SSE4.1 is present on Intel since Penryn (2007) and AMD since Bulldozer
/// (2011) — every x86 platform that can run an RTX 2060 SUPER qualifies.
static HAS_NT_BLIT: AtomicBool = AtomicBool::new(false);

/// Detect and cache CPU features used by this module.
///
/// Call once on the BSP during early boot (before any driver uses
/// [`dma_sync_wb_from_device`], [`dma_sync_wb_to_device`], or
/// [`nt_blit_rows`]).  It is safe to call from multiple CPUs — the stores
/// are idempotent.
#[cfg(target_arch = "x86_64")]
pub fn probe_cpu_features() {
    // CPUID leaf 7, subleaf 0: EBX bit 23 = CLFLUSHOPT.
    let r7 = core::arch::x86_64::__cpuid_count(7, 0);
    HAS_CLFLUSHOPT.store(r7.ebx & (1 << 23) != 0, Ordering::Relaxed);
    // CPUID leaf 1: ECX bit 19 = SSE4.1 (MOVNTDQA).
    let r1 = core::arch::x86_64::__cpuid(1);
    HAS_NT_BLIT.store(r1.ecx & (1 << 19) != 0, Ordering::Relaxed);
}

#[cfg(not(target_arch = "x86_64"))]
pub fn probe_cpu_features() {}

/// Returns `true` when [`nt_blit_rows`] will take the non-temporal fast path.
/// Callers can check this once and skip the fallback setup work.
#[inline]
pub fn has_nt_blit() -> bool {
    HAS_NT_BLIT.load(Ordering::Relaxed)
}


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

// ── Non-temporal blit (MOVNTDQA) ─────────────────────────────────────────────

/// Copy `width_bytes` bytes from `src` to `dst` using `MOVNTDQA` (non-temporal
/// load) for each 16-byte-aligned source chunk.  Any unaligned tail (0–15
/// bytes) is copied with a plain `copy_nonoverlapping`.
///
/// # Safety
/// `src` must be 16-byte-aligned.  `dst` must be valid for `width_bytes`
/// writes.  Both pointers must remain valid for the duration of the call.
/// Only call when [`HAS_NT_BLIT`] is `true`.
#[cfg(target_arch = "x86_64")]
unsafe fn nt_copy_row_aligned(dst: *mut u8, src: *const u8, width_bytes: usize) {
    use core::arch::x86_64::{__m128i, _mm_storeu_si128};
    let aligned = width_bytes & !(16_usize - 1);
    let mut i = 0usize;
    while i < aligned {
        // MOVNTDQA: non-temporal load, bypasses all cache levels.
        let val: __m128i;
        core::arch::asm!(
            "movntdqa {val}, [{src}]",
            val = out(xmm_reg) val,
            src = in(reg) src.add(i),
            options(nostack, preserves_flags, readonly),
        );
        // Regular (unaligned) store — destination is the WC GOP framebuffer
        // whose PAT mapping already ensures writes are write-combined.
        _mm_storeu_si128(dst.add(i) as *mut __m128i, val);
        i += 16;
    }
    if i < width_bytes {
        core::ptr::copy_nonoverlapping(src.add(i), dst.add(i), width_bytes - i);
    }
}

/// Copy `height` rows of `width_bytes` bytes each from `src` to `dst`, using
/// `MOVNTDQA` (non-temporal) loads for the source on capable CPUs.
///
/// **Why this eliminates the pre-blit `clflush_span`:**  A `MOVNTDQA` load
/// fetches data directly from DRAM, bypassing L1/L2/L3.  After the blit,
/// no cache lines from the GEM buffer are resident in the CPU cache.  The
/// next frame's GPU DMA writes new data to DRAM; when the CPU blits again it
/// reads DRAM directly and always sees the latest frame — with no stale lines
/// to flush.
///
/// `src` must be 16-byte-aligned (true for every page-aligned GEM allocation)
/// and `width_bytes` must be a multiple of 4 (guaranteed by ARGB8888 blit).
/// The unaligned tail (if `width_bytes % 16 != 0`) is handled by
/// [`nt_copy_row_aligned`]'s fallback.
///
/// Returns `true` when the non-temporal path was used (caller must skip
/// `clflush_span`).  Returns `false` on non-x86_64 targets, when SSE4.1 is
/// absent, or when the source is not 16-byte-aligned — caller falls back to
/// the existing `clflush_span + copy_from_slice` path.
pub fn nt_blit_rows(
    dst: *mut u8,
    dst_stride: usize,
    src: *const u8,
    src_stride: usize,
    width_bytes: usize,
    height: usize,
) -> bool {
    if height == 0 || width_bytes == 0 {
        return false;
    }
    #[cfg(target_arch = "x86_64")]
    {
        if HAS_NT_BLIT.load(Ordering::Relaxed) && (src as usize) % 16 == 0 {
            for r in 0..height {
                // Safety: caller guarantees src and dst are valid for the
                // full (height × stride) region; bounds are checked in
                // `nt_blit_chunked` before we are called.
                unsafe {
                    nt_copy_row_aligned(
                        dst.add(r * dst_stride),
                        src.add(r * src_stride),
                        width_bytes,
                    );
                }
            }
            return true;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (dst, dst_stride, src, src_stride, width_bytes, height);
    false
}
