//! Repacking a frame into the CE staging buffer for a flat copy-engine
//! present. Out of `drm.rs` for its size ceiling.

use super::*;

/// Repack the frame into the CE staging buffer at `dst_pitch` and return the
/// staging `(phys_addr, bytes)` for a flat CE copy. Returns `None` (caller
/// falls back to the CPU blit) if the staging buffer cannot be allocated.
///
/// The row copies are ordinary cached memory writes (~GB/s), and x86 PCIe DMA
/// snoops the cache, so the CE reads coherent data with no explicit flush.
pub(super) fn ce_repack_to_staging(
    pixels: &[u32],
    src_pa: u64,
    src_stride: usize,
    dst_pitch: usize,
    dst_visible_bytes: usize,
    w: u32,
    h: u32,
) -> Option<(u64, u64)> {
    let t0 = kernel_hal::timer::timer_now();
    let row_bytes = (w as usize).checked_mul(4)?;
    if row_bytes > dst_pitch {
        return None;
    }
    // The caller asks the CE for a FLAT copy of `dst_pitch * h` bytes, so every
    // byte of every destination row is overwritten -- including the
    // `row_bytes..dst_pitch` tail, which the row loop below never writes. When
    // that tail reaches past the visible width it is off-screen padding and
    // harmless; when it starts INSIDE the visible width, the CE paints the
    // previous frame's leftovers (or, on the first frame, whatever
    // `commit_page` left) over real on-screen columns. Decline instead: the
    // caller falls back to the CPU blit, which writes only the columns it has
    // pixels for and leaves the rest of the screen alone.
    if row_bytes < dst_visible_bytes {
        return None;
    }
    let need = dst_pitch.checked_mul(h as usize)?;
    let cached = {
        let slot = CE_STAGING.lock();
        slot.as_ref()
            .filter(|s| s.2 >= need)
            .map(|s| (s.0, s.1, s.3.clone()))
    };
    let (va, pa, _keepalive) = match cached {
        Some(s) => s,
        None => {
            // Allocate OUTSIDE the IRQ-disabling spinlock: a contiguous
            // multi-MB allocation (plus zeroing) is far too heavy to run with
            // interrupts off. A racing double-alloc then has to be resolved
            // when publishing -- see below; it is NOT "harmless (one wins)",
            // as this comment used to claim.
            let vmo = match VmObject::new_contiguous(pages(need), 12) {
                Ok(v) => v,
                Err(_) => {
                    if !CE_STAGING_ALLOC_FAILED_LOGGED.swap(true, Ordering::Relaxed) {
                        kernel_hal::klog_warn!(
                            "[drm] CE repack: staging alloc failed ({} bytes) -- CPU blit",
                            need
                        );
                    }
                    return None;
                }
            };
            let pa = vmo.commit_page(0, MMUFlags::READ).ok()? as u64;
            let va = phys_to_virt(pa as usize);
            // Publish under the lock, and re-check first. Two presents can
            // reach the allocation at once; the store used to overwrite the
            // slot unconditionally, so the LOSER returned a `pa` whose only
            // owner was the local `_keepalive` -- which drops when this
            // function returns, i.e. before the caller hands the address to
            // `ce_present`. The copy engine then DMA'd out of frames already
            // back in the allocator. The loser must adopt the winner's buffer
            // instead and let its own drop.
            let mut slot = CE_STAGING.lock();
            match slot.as_ref().filter(|s| s.2 >= need) {
                Some(s) => (s.0, s.1, s.3.clone()),
                None => {
                    *slot = Some((va, pa, need, vmo.clone()));
                    (va, pa, vmo)
                }
            }
        }
    };
    for r in 0..h as usize {
        let s = r.checked_mul(src_stride)?;
        if s + row_bytes / 4 > pixels.len() {
            break;
        }
        // SAFETY: staging spans `need` bytes and r*dst_pitch + row_bytes <=
        // need; the source slice bound was checked above.
        unsafe {
            core::ptr::copy_nonoverlapping(
                pixels[s..].as_ptr() as *const u8,
                (va + r * dst_pitch) as *mut u8,
                row_bytes,
            );
        }
        // Define the off-screen tail rather than letting the CE carry over
        // whatever a previous, wider repack left in it. The guard above makes
        // this range entirely off-screen padding, so its contents are not
        // visible -- but the CE copies them, and leaving DMA'd bytes
        // undefined is how the stale-column bug looked in the first place.
        // In the common full-screen case the tail is empty and this is a
        // no-op: `blit_w` is capped at the scanout pitch in pixels.
        if row_bytes < dst_pitch {
            // SAFETY: same span as the copy above -- `r * dst_pitch +
            // dst_pitch <= need`, and the region is the staging buffer's own.
            unsafe {
                core::ptr::write_bytes(
                    (va + r * dst_pitch + row_bytes) as *mut u8,
                    0,
                    dst_pitch - row_bytes,
                );
            }
        }
    }
    // The repack is suspected to be the remaining present cost: the GEM
    // source's CPU address comes from the RM's AT_CPU resolution (see
    // nouveau_uapi's NouveauGemObject::phys_addr, "BAR1-relative"), so these
    // reads may be uncached PCIe reads at ~90 MB/s. Log the measured repack
    // time on the usual cadence, plus a one-shot timed re-read of the first
    // 256 KiB of the source: ~25us means cached RAM (repack is NOT the
    // bottleneck), milliseconds mean uncached BAR reads (the CE must learn
    // to read the source directly; no CPU path can be fast).
    let n = CE_REPACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 2 || n.is_multiple_of(64) {
        let repack_us = kernel_hal::timer::timer_now()
            .saturating_sub(t0)
            .as_micros();
        let mut probe_us = 0u128;
        if n == 1 && pixels.len() >= (256 << 10) / 4 {
            let p0 = kernel_hal::timer::timer_now();
            let mut sink = 0u64;
            for px in pixels.iter().take((256 << 10) / 4).step_by(16) {
                sink = sink.wrapping_add(*px as u64);
            }
            probe_us = kernel_hal::timer::timer_now()
                .saturating_sub(p0)
                .as_micros();
            // Keep the probe's reads observable.
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
            let _ = sink;
        }
        kernel_hal::klog_info!(
            "[drm] CE repack #{}: {}us src_pa={:#x} ({} rows x {}B){}",
            n,
            repack_us,
            src_pa,
            h,
            row_bytes,
            if n == 1 {
                alloc::format!(" src read probe 256KiB(step16)={}us", probe_us)
            } else {
                alloc::string::String::new()
            }
        );
    }
    Some((pa, need as u64))
}
