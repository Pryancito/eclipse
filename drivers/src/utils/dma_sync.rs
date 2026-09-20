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
/// non-serialising `CLFLUSHOPT` instruction (CPUID.7.0.EBX[23]). Only the
/// x86_64 `clflush_span` consults it; other arches never touch it, and with
/// `#![deny(warnings)]` an unused static fails their clippy.
#[cfg(target_arch = "x86_64")]
static HAS_CLFLUSHOPT: AtomicBool = AtomicBool::new(false);

/// Set at boot by [`probe_cpu_features`]; `true` when the CPU supports the
/// `MOVNTDQA` non-temporal LOAD (SSE4.1, CPUID.1.ECX[19]).
///
/// This gates [`nt_blit_rows`] only. It used to gate [`nt_store_rows`] as well,
/// which was wrong in a way that cost throughput silently: that path emits
/// `MOVDQU`/`MOVNTDQ`, both **SSE2**, and SSE2 is architecturally guaranteed on
/// every x86_64 part. A CPU without SSE4.1 was therefore pushed onto the scalar
/// aperture path (the documented ~42 MB/s) for no reason at all. See
/// [`has_nt_store`].
static HAS_NT_BLIT: AtomicBool = AtomicBool::new(false);

/// Detect and cache CPU features used by this module.
///
/// Call once on the BSP during early boot (before any driver uses
/// [`dma_sync_wb_from_device`], [`dma_sync_wb_to_device`],
/// [`nt_store_rows`], or [`nt_blit_rows`]).  It is safe to call from
/// multiple CPUs — the stores are idempotent.
#[cfg(target_arch = "x86_64")]
pub fn probe_cpu_features() {
    let r7 = core::arch::x86_64::__cpuid_count(7, 0);
    HAS_CLFLUSHOPT.store(r7.ebx & (1 << 23) != 0, Ordering::Relaxed);
    let r1 = core::arch::x86_64::__cpuid(1);
    HAS_NT_BLIT.store(r1.ecx & (1 << 19) != 0, Ordering::Relaxed);
}

#[cfg(not(target_arch = "x86_64"))]
pub fn probe_cpu_features() {}

/// Returns `true` when [`nt_blit_rows`] takes its non-temporal **load** path
/// (`MOVNTDQA`, SSE4.1). Not a precondition for [`nt_store_rows`] -- see
/// [`has_nt_store`].
#[inline]
pub fn has_nt_blit() -> bool {
    HAS_NT_BLIT.load(Ordering::Relaxed)
}

/// Returns `true` when [`nt_store_rows`] takes its non-temporal **store** path.
///
/// Unconditionally true on x86_64 and false everywhere else: the path emits
/// only `MOVDQU` and `MOVNTDQ`, which are SSE2, and the x86_64 ABI guarantees
/// SSE2. No CPUID probe is needed, and none is consulted -- which is the point,
/// because this used to read the SSE4.1 bit and quietly disable itself.
#[inline]
pub const fn has_nt_store() -> bool {
    cfg!(target_arch = "x86_64")
}

/// Drain the CPU's write-combining store buffers (`SFENCE` on x86).
///
/// Every ordinary store into a write-combining aperture -- the GOP surface or
/// BAR1 -- sits in a combine buffer until the CPU decides to flush it, and the
/// CPU is under no obligation to do that before the scanout engine reads the
/// same bytes. The non-temporal path already ends with an `SFENCE` "so scanout /
/// cursor overlay cannot observe a torn last line"; the scalar paths
/// (`fill_rect`, `copy_rect`, `blit_argb_over`, and `blit_from`'s
/// `copy_from_slice` fallback) had no barrier of any kind, and
/// `DisplayScheme::need_flush` is `false` for both write-combining backends, so
/// nothing downstream supplied one either. The last line of a blit could
/// therefore reach the panel a frame late, or half-written.
///
/// Cheap enough to call once per primitive: `SFENCE` orders stores already
/// issued and does not wait on memory.
#[inline]
pub fn wc_store_drain() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a plain fence with no memory operand.
    unsafe {
        core::arch::x86_64::_mm_sfence()
    };
    // Other arches: the compiler fence keeps the preceding stores from being
    // sunk past the caller's "the frame is on screen now" point. The bare-metal
    // targets here have no write-combining framebuffer aperture to drain.
    #[cfg(not(target_arch = "x86_64"))]
    fence(Ordering::Release);
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
fn clflush_span(vaddr: usize, len: usize) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let mut p = vaddr & !(64 - 1);
        let end = vaddr.saturating_add(len);
        if p >= end {
            return;
        }
        if HAS_CLFLUSHOPT.load(Ordering::Relaxed) {
            // MFENCE on BOTH sides of the *span*, not a trailing SFENCE and
            // not an MFENCE per line. CLFLUSHOPT is ordered only by store
            // fences, and SFENCE does not order later LOADS -- without the
            // trailing MFENCE the consumer's reads (the present blit /
            // CE-staging repack pulling GPU-rendered pixels) can execute
            // BEFORE the invalidate completes and keep serving stale lines
            // from the previous frame. Linux `clflushopt_cache_range` is
            // the same shape (`mb(); loop; mb();`). The flushes themselves
            // still overlap between the fences, which is the entire win
            // over the serializing CLFLUSH path below.
            //
            // A 4.2 MB GEM buffer is ~65k lines: one fence each side plus
            // an 8-line unroll keeps the invalidate concurrent instead of
            // serializing every 64 B behind an MFENCE (several ms).
            core::arch::x86_64::_mm_mfence();
            let end8 = end.saturating_sub(511);
            while p < end8 {
                core::arch::asm!(
                    "clflushopt [{p}]",
                    "clflushopt [{p} + 64]",
                    "clflushopt [{p} + 128]",
                    "clflushopt [{p} + 192]",
                    "clflushopt [{p} + 256]",
                    "clflushopt [{p} + 320]",
                    "clflushopt [{p} + 384]",
                    "clflushopt [{p} + 448]",
                    p = in(reg) p,
                    options(nostack, preserves_flags),
                );
                p += 512;
            }
            while p < end {
                core::arch::asm!(
                    "clflushopt [{p}]",
                    p = in(reg) p,
                    options(nostack, preserves_flags),
                );
                p += 64;
            }
            core::arch::x86_64::_mm_mfence();
        } else {
            // CLFLUSH is itself serializing (no per-line fence needed).
            // One trailing SFENCE keeps later stores ordered; the leading
            // MFENCE the CLFLUSHOPT path needs would only add latency here.
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

/// Copy `width_bytes` bytes from `src` to `dst` using `MOVNTDQ` stores.
///
/// Head/tail bytes that are not 16-byte-aligned in `dst` use ordinary
/// copies; the aligned middle is written with non-temporal stores so a
/// write-combining GOP/BAR1 mapping fills 64-byte PCIe bursts instead of
/// one transaction per `copy_from_slice` store (~42 MB/s on UC BAR1).
///
/// # Safety
/// `src`/`dst` must be valid for `width_bytes` reads/writes.  Only call
/// when [`has_nt_blit`] is `true`.  Clobbers `xmm0`.
#[cfg(target_arch = "x86_64")]
unsafe fn nt_store_row(dst: *mut u8, src: *const u8, width_bytes: usize) {
    if width_bytes == 0 {
        return;
    }
    let dst_mis = (dst as usize) & 15;
    let mut i = 0usize;
    if dst_mis != 0 {
        let head = (16 - dst_mis).min(width_bytes);
        core::ptr::copy_nonoverlapping(src, dst, head);
        i = head;
    }
    let aligned = (width_bytes - i) & !15;
    let aligned_end = i + aligned;
    // 64-byte body fills one WC combine buffer per trip.
    while i + 64 <= aligned_end {
        core::arch::asm!(
            "movdqu xmm0, [{src}]",
            "movntdq [{dst}], xmm0",
            "movdqu xmm0, [{src} + 16]",
            "movntdq [{dst} + 16], xmm0",
            "movdqu xmm0, [{src} + 32]",
            "movntdq [{dst} + 32], xmm0",
            "movdqu xmm0, [{src} + 48]",
            "movntdq [{dst} + 48], xmm0",
            src = in(reg) src.add(i),
            dst = in(reg) dst.add(i),
            options(nostack, preserves_flags),
        );
        i += 64;
    }
    while i + 16 <= aligned_end {
        core::arch::asm!(
            "movdqu xmm0, [{src}]",
            "movntdq [{dst}], xmm0",
            src = in(reg) src.add(i),
            dst = in(reg) dst.add(i),
            options(nostack, preserves_flags),
        );
        i += 16;
    }
    if i < width_bytes {
        core::ptr::copy_nonoverlapping(src.add(i), dst.add(i), width_bytes - i);
    }
}

/// Copy `width_bytes` bytes from `src` to `dst` using `MOVNTDQA` for each
/// 16-byte-aligned source chunk.
///
/// # Safety
/// `src` must be 16-byte-aligned.  `dst` must be valid for `width_bytes`
/// writes.  Only call when [`has_nt_blit`] is `true`.
#[cfg(target_arch = "x86_64")]
unsafe fn nt_copy_row_aligned(dst: *mut u8, src: *const u8, width_bytes: usize) {
    let aligned = width_bytes & !(16_usize - 1);
    let mut i = 0usize;
    while i < aligned {
        // xmm0 is hardcoded scratch: the kernel target is `-sse,+soft-float`
        // (`zCore/x86_64.json`), so `out(xmm_reg)` and SSE intrinsics are
        // rejected.  Safe because LLVM never allocates XMM under `-sse` and
        // callers hold IRQs off for the blit (trap path saves GPRs only).
        core::arch::asm!(
            "movntdqa xmm0, [{src}]",
            "movdqu [{dst}], xmm0",
            src = in(reg) src.add(i),
            dst = in(reg) dst.add(i),
            options(nostack, preserves_flags),
        );
        i += 16;
    }
    if i < width_bytes {
        core::ptr::copy_nonoverlapping(src.add(i), dst.add(i), width_bytes - i);
    }
}

/// Copy `height` rows of `width_bytes` bytes each from `src` to `dst` using
/// non-temporal **stores** (`MOVNTDQ`).
///
/// Call this when the destination is write-combining (UEFI GOP / NVIDIA
/// BAR1). Regular stores to that aperture are ~42 MB/s; NT stores combine
/// into 64-byte PCIe writes. A trailing `SFENCE` drains the WC buffers so
/// scanout / cursor overlay cannot observe a torn last line.
///
/// Returns `false` when the CPU lacks the NT-blit feature; the caller must
/// then use a scalar copy. Does **not** skip a `clflush_span` of a WB
/// source — NT stores do not make GPU-written WB lines coherent.
///
/// # Safety
/// `src` must be valid for `height` rows of `width_bytes` reads at
/// `src_stride` spacing and `dst` for the same writes at `dst_stride`; the
/// two must not overlap.
pub unsafe fn nt_store_rows(
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
        // No CPUID gate: `MOVDQU`/`MOVNTDQ` are SSE2, guaranteed on x86_64.
        let mut xmm0_save = [0u8; 16];
        unsafe {
            core::arch::asm!(
                "movdqu [{buf}], xmm0",
                buf = in(reg) xmm0_save.as_mut_ptr(),
                options(nostack, preserves_flags),
            );
            for r in 0..height {
                nt_store_row(
                    dst.add(r * dst_stride),
                    src.add(r * src_stride),
                    width_bytes,
                );
            }
            core::arch::x86_64::_mm_sfence();
            core::arch::asm!(
                "movdqu xmm0, [{buf}]",
                buf = in(reg) xmm0_save.as_ptr(),
                options(nostack, preserves_flags),
            );
        }
        true
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (dst, dst_stride, src, src_stride, width_bytes, height);
        false
    }
}

/// Copy `height` rows of `width_bytes` bytes each from `src` to `dst`, using
/// non-temporal loads for the source on capable CPUs.
///
/// NOTE: `MOVNTDQA` only bypasses the cache on WC-mapped memory (Intel SDM
/// vol. 1 §12.10.3); on an ordinary write-back mapping it behaves as a plain
/// cached load, so it does NOT make a preceding `clflush_span` of a WB source
/// skippable, and it cannot speed up a present whose bottleneck is the store
/// side. The scanout CPU fallback uses [`nt_store_rows`] (NT *stores* into
/// WC GOP/BAR1) instead.
///
/// # Safety
/// Same contract as [`nt_store_rows`]: `src`/`dst` valid for `height` rows
/// at their strides, non-overlapping.
pub unsafe fn nt_blit_rows(
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
        if HAS_NT_BLIT.load(Ordering::Relaxed) && (src as usize).is_multiple_of(16) {
            // The row copies clobber xmm0, which belongs to the interrupted
            // USER context: this soft-float kernel never saves vector state on
            // syscall entry, so without a save/restore the caller returns to
            // userspace with a corrupted xmm0 (Mesa's SSE code then fails in
            // ways that look nothing like the real cause).
            let mut xmm0_save = [0u8; 16];
            unsafe {
                core::arch::asm!(
                    "movdqu [{buf}], xmm0",
                    buf = in(reg) xmm0_save.as_mut_ptr(),
                    options(nostack, preserves_flags),
                );
                for r in 0..height {
                    nt_copy_row_aligned(
                        dst.add(r * dst_stride),
                        src.add(r * src_stride),
                        width_bytes,
                    );
                }
                core::arch::asm!(
                    "movdqu xmm0, [{buf}]",
                    buf = in(reg) xmm0_save.as_ptr(),
                    options(nostack, preserves_flags),
                );
            }
            return true;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (dst, dst_stride, src, src_stride, width_bytes, height);
    false
}

/// Tests for the CPU-feature gating of the non-temporal paths and for the
/// write-combining drain. Both are about the framebuffer aperture: what reaches
/// it, and when the CPU is obliged to let it go.
#[cfg(test)]
mod wc_tests {
    use super::*;

    /// The regression this guards. `nt_store_rows` emits `MOVDQU` and `MOVNTDQ`
    /// -- both SSE2, which the x86_64 ABI guarantees -- but it was gated on
    /// `HAS_NT_BLIT`, probed from the **SSE4.1** bit CPUID.1.ECX[19]. On a
    /// machine without SSE4.1 every framebuffer blit silently fell back to the
    /// scalar aperture path (the documented ~42 MB/s), for no reason. Worse, the
    /// flag starts out `false`, so the fast path was also unavailable to anything
    /// running before `probe_cpu_features()`.
    ///
    /// The store path must therefore not consult any probe at all. Asserted
    /// without calling `probe_cpu_features` first, which is the case that used
    /// to fail regardless of the host CPU.
    #[test]
    fn the_non_temporal_store_path_needs_no_cpuid_probe() {
        assert_eq!(
            has_nt_store(),
            cfg!(target_arch = "x86_64"),
            "MOVNTDQ is SSE2; on x86_64 it is always available"
        );
        // It is a `const fn` precisely so it cannot grow a runtime probe.
        const _: bool = has_nt_store();
    }

    /// And the load path keeps its probe, because `MOVNTDQA` really is SSE4.1.
    /// The two must not be the same flag again.
    #[test]
    fn the_non_temporal_load_path_still_depends_on_the_probe() {
        HAS_NT_BLIT.store(false, Ordering::Relaxed);
        assert!(!has_nt_blit(), "unprobed means no non-temporal loads");
        HAS_NT_BLIT.store(true, Ordering::Relaxed);
        assert!(has_nt_blit());
        // Restore whatever the real probe would say on this host.
        #[cfg(target_arch = "x86_64")]
        {
            let r1 = core::arch::x86_64::__cpuid(1);
            HAS_NT_BLIT.store(r1.ecx & (1 << 19) != 0, Ordering::Relaxed);
        }
    }

    /// `nt_store_rows` copies correctly with no probe having run -- the
    /// behavioural half of the first test.
    #[test]
    fn non_temporal_stores_copy_every_row_without_a_probe() {
        const W: usize = 64; // bytes, four whole WC lines
        const H: usize = 4;
        let src: alloc::vec::Vec<u8> = (0..W * H).map(|n| (n % 251) as u8).collect();
        let mut dst = alloc::vec![0u8; W * H];
        // SAFETY: both buffers hold `H` rows of `W` bytes at stride `W`, and
        // they do not overlap.
        let took_nt = unsafe { nt_store_rows(dst.as_mut_ptr(), W, src.as_ptr(), W, W, H) };
        assert_eq!(took_nt, cfg!(target_arch = "x86_64"));
        if took_nt {
            assert_eq!(dst, src, "every row must land, byte for byte");
        }
    }

    /// The drain has no observable result to assert -- an `SFENCE` returns
    /// nothing and orders stores already issued. What can be pinned is that it
    /// is callable from anywhere, cheaply and repeatedly, with nothing to
    /// initialise: that is what lets every 2D primitive end with one, which is
    /// the actual fix (the scalar paths had no barrier at all, and
    /// `need_flush()` is false for both write-combining backends so nothing
    /// above supplied one either).
    #[test]
    fn the_write_combining_drain_is_always_callable() {
        for _ in 0..3 {
            wc_store_drain();
        }
    }
}
