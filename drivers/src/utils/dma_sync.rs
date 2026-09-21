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
        #[cfg(test)]
        test_flag::note_nt_store();
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
/// Returns `false` — meaning the caller must fall back to a scalar copy —
/// when the CPU lacks the feature, when `src` is not 16-byte aligned, or when
/// a multi-row blit's `src_stride` is not a multiple of 16 (see below).
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
        // `MOVNTDQA` FAULTS on a source that is not 16-byte aligned, and row
        // `r` starts at `src + r * src_stride` — so checking the base alone
        // covers row 0 and nothing else. A 1366-pixel scanline is 5464 bytes,
        // which is 8 mod 16: with an aligned base, row 1 is misaligned and the
        // load is a #GP in kernel mode, not a wrong pixel. Refusing the whole
        // blit (the caller then uses its scalar copy, as the return value
        // documents) beats faulting or giving half the rows the slow path.
        let rows_stay_aligned = height == 1 || src_stride.is_multiple_of(16);
        if HAS_NT_BLIT.load(Ordering::Relaxed)
            && (src as usize).is_multiple_of(16)
            && rows_stay_aligned
        {
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
    ///
    /// Through `test_flag`, because the flag is one bool for the whole process:
    /// setting it by hand here is what another test's probe undoes underneath
    /// us, and vice versa.
    #[test]
    fn the_non_temporal_load_path_still_depends_on_the_probe() {
        {
            let _off = test_flag::pinned(false);
            assert!(!has_nt_blit(), "unprobed means no non-temporal loads");
        }
        let _on = test_flag::pinned(true);
        assert!(has_nt_blit());
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
/// Test-only control of [`HAS_NT_BLIT`].
///
/// The flag is one bool for the whole process, set once at boot by
/// [`probe_cpu_features`]. Under `cargo test` there is no boot and there are
/// several threads: a test that wants it off gets it switched back on under
/// itself by another test's probe, and a test that never asked about it at all
/// silently changes which code path it exercises depending on who ran first.
/// Both showed up here for real.
///
/// So every test that cares takes a guard from this module. It holds a lock
/// for as long as it lives and puts the flag back as it found it, which also
/// means a test module can pin the flag OFF without hiding the fast path from
/// everybody else.
#[cfg(test)]
pub(crate) mod test_flag {
    use super::{probe_cpu_features, HAS_NT_BLIT};
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// How many times [`super::nt_store_rows`] has actually taken the
    /// non-temporal path. A test that compares the fast path against the slow
    /// one is worthless if the fast one quietly declined -- the two agree
    /// perfectly when they are the same code -- so the comparison checks this
    /// moved.
    static NT_STORE_CALLS: AtomicUsize = AtomicUsize::new(0);

    pub(crate) fn note_nt_store() {
        NT_STORE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    /// The count so far. Only meaningful while a [`Scoped`] guard is held,
    /// which is what keeps another thread's blit out of it.
    pub fn nt_store_calls() -> usize {
        NT_STORE_CALLS.load(Ordering::Relaxed)
    }

    lazy_static::lazy_static! {
        static ref LOCK: spin::Mutex<()> = spin::Mutex::new(());
    }

    /// Holds the flag at a chosen value, and the lock that keeps every other
    /// interested test out meanwhile.
    pub struct Scoped {
        _guard: spin::MutexGuard<'static, ()>,
        previous: bool,
        /// What the flag reads while this guard is alive.
        pub nt: bool,
    }

    impl Drop for Scoped {
        fn drop(&mut self) {
            HAS_NT_BLIT.store(self.previous, Ordering::Relaxed);
        }
    }

    /// Pin the flag to `on`, whatever this CPU can actually do.
    pub fn pinned(on: bool) -> Scoped {
        let guard = LOCK.lock();
        let previous = HAS_NT_BLIT.swap(on, Ordering::Relaxed);
        Scoped {
            _guard: guard,
            previous,
            nt: on,
        }
    }

    /// Pin the flag to what this CPU really supports, which is what the boot
    /// path would have left it at. `nt` says whether the fast path is
    /// reachable here at all, so a test can skip rather than pass vacuously.
    pub fn as_detected() -> Scoped {
        let guard = LOCK.lock();
        let previous = HAS_NT_BLIT.load(Ordering::Relaxed);
        probe_cpu_features();
        let nt = HAS_NT_BLIT.load(Ordering::Relaxed);
        Scoped {
            _guard: guard,
            previous,
            nt,
        }
    }
}

/// The non-temporal blit, which only a real write-combining destination
/// walks.
///
/// `blit_from` takes this path when the framebuffer is WC — a UEFI GOP or
/// NVIDIA BAR1 — and the CPU has SSE4.1. QEMU's framebuffer is neither, so
/// every present in the emulator and in CI goes down the `copy_from_slice`
/// fallback instead and none of the pointer arithmetic below is ever
/// executed there. What it gets wrong shows up as visual corruption on the
/// machine with the real card and nowhere else.
///
/// The host runs these for real: the same `MOVNTDQ`/`MOVNTDQA` instructions,
/// on ordinary `Vec<u8>`, where every byte can be read back — including the
/// ones the blit must NOT touch.
///
/// Two deliberate breakages of the 64-byte loop survive these tests and are
/// meant to: running it to `width_bytes` instead of `aligned_end`, and
/// advancing by 48 instead of 64. Neither can change a byte. Both `i` and
/// `aligned_end` stay congruent mod 16, and `width_bytes - aligned_end < 16`,
/// so the first can never fit the extra iteration it asks for; the second just
/// rewrites 16 bytes it already wrote, from the same source to the same
/// destination. They cost instructions, not pixels, so no test should be
/// written to catch them.
#[cfg(all(test, target_arch = "x86_64"))]
mod nt_blit_tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A destination that knows what it looked like before, so a test can ask
    /// not only "did the right bytes arrive" but "did anything else move".
    struct Canvas {
        /// 64 bytes of slack at each end, so a blit that runs off either edge
        /// of the picture lands somewhere observable instead of somewhere
        /// undefined.
        mem: Vec<u8>,
        base: usize,
    }

    const PAD: usize = 64;
    /// What untouched destination bytes hold. Not zero: zero is also what a
    /// bug that writes nothing leaves behind.
    const UNTOUCHED: u8 = 0xCD;

    impl Canvas {
        /// A canvas whose picture starts `misalign` bytes past a 16-byte
        /// boundary, which is what a blit into a sub-rectangle of a real
        /// framebuffer gets.
        fn new(len: usize, misalign: usize) -> Canvas {
            let mut mem = vec![UNTOUCHED; len + 2 * PAD + 32];
            let start = mem.as_ptr() as usize;
            // Slide the base forward until it sits at the requested offset
            // from a 16-byte boundary.
            let aligned = (start + PAD + 15) & !15;
            let base = aligned + misalign - start;
            mem.resize(base + len + PAD, UNTOUCHED);
            Canvas { mem, base }
        }

        fn ptr(&mut self) -> *mut u8 {
            unsafe { self.mem.as_mut_ptr().add(self.base) }
        }

        fn pixel(&self, off: usize) -> u8 {
            self.mem[self.base + off]
        }

        /// Every byte outside `[0, len)` of the picture, which a correct blit
        /// leaves exactly as it found it.
        fn spilled(&self, len: usize) -> Option<usize> {
            let head = (0..self.base).find(|&i| self.mem[i] != UNTOUCHED);
            if head.is_some() {
                return head;
            }
            (self.base + len..self.mem.len()).find(|&i| self.mem[i] != UNTOUCHED)
        }
    }

    /// Source bytes that are distinct enough that a copy landing one row or
    /// one byte off is visible in the value, not just in a count.
    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    /// Check a `height` x `width_bytes` picture at `dst_stride` against a
    /// source at `src_stride`: every visible byte copied, every byte of the
    /// stride padding untouched.
    fn assert_rows_match(
        canvas: &Canvas,
        src: &[u8],
        dst_stride: usize,
        src_stride: usize,
        width_bytes: usize,
        height: usize,
    ) {
        for r in 0..height {
            for c in 0..width_bytes {
                assert_eq!(
                    canvas.pixel(r * dst_stride + c),
                    src[r * src_stride + c],
                    "row {} byte {} did not arrive",
                    r,
                    c
                );
            }
            // The gap after the visible width belongs to the NEXT scanline on
            // a padded framebuffer. Writing into it is how a blit smears one
            // row's tail across the start of the row below.
            for c in width_bytes..dst_stride {
                let off = r * dst_stride + c;
                if off < (height - 1) * dst_stride + width_bytes || r + 1 == height {
                    assert_eq!(
                        canvas.pixel(off),
                        UNTOUCHED,
                        "row {} byte {} is padding and must not be written",
                        r,
                        c
                    );
                }
            }
        }
    }

    #[test]
    fn a_tight_blit_copies_every_byte() {
        let flag = super::test_flag::as_detected();
        if !flag.nt {
            return;
        }
        let (w, h) = (256usize, 4usize);
        let src = pattern(w * h);
        let mut canvas = Canvas::new(w * h, 0);
        let ok = unsafe { nt_store_rows(canvas.ptr(), w, src.as_ptr(), w, w, h) };
        assert!(ok, "an aligned tight blit must take the NT path");
        assert_rows_match(&canvas, &src, w, w, w, h);
        assert_eq!(canvas.spilled(w * h), None, "the blit wrote outside itself");
    }

    #[test]
    fn the_padding_of_a_gop_scanline_is_left_alone() {
        let flag = super::test_flag::as_detected();
        // A GOP reports `PixelsPerScanLine` separately from the width: a
        // 1920-wide mode on a 2048-pixel pitch leaves 512 bytes of padding on
        // every row. Those bytes are off-screen but they are also where the
        // next scanline begins in memory, so a blit that runs into them
        // corrupts the row below.
        if !flag.nt {
            return;
        }
        let (width_bytes, pitch, h) = (1920 * 4, 2048 * 4, 3usize);
        let src = pattern(width_bytes * h);
        let mut canvas = Canvas::new(pitch * h, 0);
        let ok = unsafe {
            nt_store_rows(
                canvas.ptr(),
                pitch,
                src.as_ptr(),
                width_bytes,
                width_bytes,
                h,
            )
        };
        assert!(ok);
        assert_rows_match(&canvas, &src, pitch, width_bytes, width_bytes, h);
        assert_eq!(canvas.spilled(pitch * h), None);
    }

    #[test]
    fn every_destination_misalignment_still_copies_exactly() {
        let flag = super::test_flag::as_detected();
        // The head of a row is copied bytewise until the destination reaches a
        // 16-byte boundary, because `MOVNTDQ` faults on anything else. A blit
        // into a sub-rectangle starts wherever `dst_x * 4` puts it, so all
        // sixteen offsets are reachable.
        if !flag.nt {
            return;
        }
        for misalign in 0..16usize {
            let (w, h) = (100usize, 3usize);
            let src = pattern(w * h);
            let mut canvas = Canvas::new(w * h, misalign);
            assert_eq!(
                canvas.ptr() as usize % 16,
                misalign,
                "the canvas did not honour the requested misalignment"
            );
            let ok = unsafe { nt_store_rows(canvas.ptr(), w, src.as_ptr(), w, w, h) };
            assert!(ok);
            assert_rows_match(&canvas, &src, w, w, w, h);
            assert_eq!(
                canvas.spilled(w * h),
                None,
                "misalignment {} made the blit write outside itself",
                misalign
            );
        }
    }

    #[test]
    fn widths_that_straddle_the_sixty_four_and_sixteen_byte_steps() {
        let flag = super::test_flag::as_detected();
        // The row copy has three stages -- a bytewise head, a 64-byte body, a
        // 16-byte body, a bytewise tail -- and an off-by-one in any of them
        // only shows at a width that ends inside that stage.
        if !flag.nt {
            return;
        }
        for &w in &[
            1usize, 15, 16, 17, 31, 32, 33, 63, 64, 65, 79, 80, 81, 127, 128, 129,
        ] {
            for misalign in [0usize, 1, 8, 15] {
                let h = 2usize;
                let src = pattern(w * h);
                let mut canvas = Canvas::new(w * h, misalign);
                let ok = unsafe { nt_store_rows(canvas.ptr(), w, src.as_ptr(), w, w, h) };
                assert!(ok, "width {} misalign {} refused the NT path", w, misalign);
                assert_rows_match(&canvas, &src, w, w, w, h);
                assert_eq!(
                    canvas.spilled(w * h),
                    None,
                    "width {} misalign {} wrote outside itself",
                    w,
                    misalign
                );
            }
        }
    }

    #[test]
    fn a_laptop_scanline_that_is_not_a_multiple_of_sixteen() {
        let flag = super::test_flag::as_detected();
        // 1366x768 is the commonest laptop panel there is, and 1366 * 4 =
        // 5464 bytes, which is 8 mod 16: every row ends mid-step and leans on
        // the bytewise tail.
        if !flag.nt {
            return;
        }
        let (width_bytes, pitch, h) = (1366 * 4, 1376 * 4, 3usize);
        assert_eq!(
            width_bytes % 16,
            8,
            "the point of this test is the remainder"
        );
        let src = pattern(width_bytes * h);
        let mut canvas = Canvas::new(pitch * h, 0);
        let ok = unsafe {
            nt_store_rows(
                canvas.ptr(),
                pitch,
                src.as_ptr(),
                width_bytes,
                width_bytes,
                h,
            )
        };
        assert!(ok);
        assert_rows_match(&canvas, &src, pitch, width_bytes, width_bytes, h);
        assert_eq!(canvas.spilled(pitch * h), None);
    }

    #[test]
    fn a_sub_rectangle_reads_its_own_rows_and_no_others() {
        let flag = super::test_flag::as_detected();
        // The damage rectangle of a compositor present: a window of a wider
        // source buffer, so `src_stride` is bigger than `width_bytes` and the
        // bytes between them must never be copied.
        if !flag.nt {
            return;
        }
        let (src_stride, width_bytes, h) = (512usize, 200usize, 4usize);
        let src = pattern(src_stride * h);
        let mut canvas = Canvas::new(width_bytes * h, 0);
        let ok = unsafe {
            nt_store_rows(
                canvas.ptr(),
                width_bytes,
                src.as_ptr(),
                src_stride,
                width_bytes,
                h,
            )
        };
        assert!(ok);
        for r in 0..h {
            for c in 0..width_bytes {
                assert_eq!(
                    canvas.pixel(r * width_bytes + c),
                    src[r * src_stride + c],
                    "row {} byte {} came from the wrong source row",
                    r,
                    c
                );
            }
        }
        assert_eq!(canvas.spilled(width_bytes * h), None);
    }

    #[test]
    fn an_empty_blit_writes_nothing_and_says_so() {
        let flag = super::test_flag::as_detected();
        if !flag.nt {
            return;
        }
        for (w, h) in [(0usize, 4usize), (64, 0)] {
            let src = pattern(256);
            let mut canvas = Canvas::new(256, 0);
            let ok = unsafe { nt_store_rows(canvas.ptr(), 64, src.as_ptr(), 64, w, h) };
            assert!(!ok, "a {}x{} blit is nothing to do, not a success", w, h);
            assert_eq!(
                canvas.spilled(0),
                None,
                "a {}x{} blit touched the destination",
                w,
                h
            );
        }
    }

    /// The refusal contract, on the path that can actually refuse.
    ///
    /// `HAS_NT_BLIT` is the **SSE4.1** bit, and SSE4.1 is what `MOVNTDQA` --
    /// a non-temporal *load* -- needs, so it gates `nt_blit_rows` alone.
    /// `nt_store_rows` emits `MOVNTDQ`, which is SSE2 and therefore part of the
    /// x86_64 baseline: there is no such CPU for it to decline on, and it used
    /// to consult this flag only by mistake. So the contract is asserted here
    /// against the loads.
    ///
    /// The contract itself is what matters: the caller reads `false` as "I did
    /// nothing, do it yourself" and runs its own scalar copy. Returning `true`
    /// after copying nothing -- or `false` after copying something -- both
    /// leave the screen wrong.
    #[test]
    fn without_the_cpu_feature_it_refuses_instead_of_copying_half() {
        let _flag = super::test_flag::pinned(false);
        let src = pattern(256 + 16);
        // `nt_blit_rows` wants a 16-byte-aligned source; the refusal must come
        // from the missing feature, not from a rejected argument.
        let off = (16 - (src.as_ptr() as usize % 16)) % 16;
        let aligned_src = unsafe { src.as_ptr().add(off) };
        let mut canvas = Canvas::new(256, 0);
        let ok = unsafe { nt_blit_rows(canvas.ptr(), 64, aligned_src, 64, 64, 4) };
        assert!(!ok, "no NT support must be reported, not assumed");
        assert_eq!(canvas.spilled(0), None, "it copied without saying it had");
    }

    /// And the store path takes no notice of that flag, which is the fix.
    /// Pinned OFF, it must still copy: a machine without SSE4.1 was falling
    /// back to the ~42 MB/s scalar aperture path for every blit, for nothing.
    #[test]
    fn the_store_path_ignores_the_sse41_flag_entirely() {
        let _flag = super::test_flag::pinned(false);
        let (w, h) = (64usize, 4usize);
        let src = pattern(w * h);
        let mut canvas = Canvas::new(w * h, 0);
        let ok = unsafe { nt_store_rows(canvas.ptr(), w, src.as_ptr(), w, w, h) };
        assert!(ok, "MOVNTDQ is SSE2; the SSE4.1 bit has no say over it");
        assert_rows_match(&canvas, &src, w, w, w, h);
        assert_eq!(canvas.spilled(w * h), None);
    }

    #[test]
    fn a_blit_with_non_temporal_loads_matches_a_plain_copy() {
        let flag = super::test_flag::as_detected();
        if !flag.nt {
            return;
        }
        let (w, h) = (256usize, 4usize);
        let src = pattern(w * h + 16);
        // `nt_blit_rows` requires a 16-byte-aligned source, which is what a
        // GEM buffer or a BAR mapping gives it.
        let off = (16 - (src.as_ptr() as usize % 16)) % 16;
        let aligned_src = unsafe { src.as_ptr().add(off) };
        let mut canvas = Canvas::new(w * h, 0);
        let ok = unsafe { nt_blit_rows(canvas.ptr(), w, aligned_src, w, w, h) };
        assert!(ok, "an aligned source must take the NT-load path");
        for i in 0..w * h {
            assert_eq!(canvas.pixel(i), src[off + i], "byte {} differs", i);
        }
        assert_eq!(canvas.spilled(w * h), None);
    }

    #[test]
    fn a_stride_that_would_misalign_the_second_row_is_refused() {
        let flag = super::test_flag::as_detected();
        // `MOVNTDQA` faults on a source that is not 16-byte aligned, and row
        // `r` starts at `src + r * src_stride`. A 1366-pixel scanline is 5464
        // bytes, 8 mod 16: with an aligned base, row 1 is misaligned. Checking
        // only the base -- which is what this did -- makes row 1 a #GP in
        // kernel mode.
        //
        // If this ever regresses the test does not fail, it takes the whole
        // test process down with a SIGSEGV; that is the same instruction
        // faulting, in the only place it can be observed safely.
        if !flag.nt {
            return;
        }
        let src_stride = 1366 * 4;
        assert_eq!(src_stride % 16, 8);
        let h = 3usize;
        let src = pattern(src_stride * h + 16);
        let off = (16 - (src.as_ptr() as usize % 16)) % 16;
        let aligned_src = unsafe { src.as_ptr().add(off) };
        let mut canvas = Canvas::new(src_stride * h, 0);
        let ok = unsafe {
            nt_blit_rows(
                canvas.ptr(),
                src_stride,
                aligned_src,
                src_stride,
                src_stride,
                h,
            )
        };
        assert!(
            !ok,
            "a stride that misaligns row 1 must be refused, not faulted on"
        );
        assert_eq!(
            canvas.spilled(0),
            None,
            "it must leave the destination for the caller's scalar copy"
        );
    }

    #[test]
    fn a_single_row_does_not_care_about_the_stride() {
        let flag = super::test_flag::as_detected();
        // With one row the stride is never added to anything, so refusing an
        // odd stride there would turn away a perfectly good blit -- the cursor
        // overlay and a one-line damage rectangle are both this shape.
        if !flag.nt {
            return;
        }
        let src = pattern(1366 * 4 + 16);
        let off = (16 - (src.as_ptr() as usize % 16)) % 16;
        let aligned_src = unsafe { src.as_ptr().add(off) };
        let w = 1366 * 4;
        let mut canvas = Canvas::new(w, 0);
        let ok = unsafe { nt_blit_rows(canvas.ptr(), w, aligned_src, 1366 * 4, w, 1) };
        assert!(ok, "one row with an odd stride is still one aligned row");
        for i in 0..w {
            assert_eq!(canvas.pixel(i), src[off + i], "byte {} differs", i);
        }
    }

    #[test]
    fn a_misaligned_source_is_refused_outright() {
        let flag = super::test_flag::as_detected();
        // Row 0 itself faults if the base is not aligned, so this one has
        // never been safe to attempt.
        if !flag.nt {
            return;
        }
        let src = pattern(1024);
        let off = (16 - (src.as_ptr() as usize % 16)) % 16;
        let misaligned = unsafe { src.as_ptr().add(off + 1) };
        assert_ne!(misaligned as usize % 16, 0);
        let mut canvas = Canvas::new(512, 0);
        let ok = unsafe { nt_blit_rows(canvas.ptr(), 128, misaligned, 128, 128, 4) };
        assert!(!ok);
        assert_eq!(canvas.spilled(0), None);
    }
}
