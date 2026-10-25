//! CPU-mapping a VRAM `GEM_NEW` object through the BAR1 aperture.
//!
//! # The gap this closes
//!
//! `GEM_NEW` honours a VRAM-only request as a true `NV01_MEMORY_LOCAL_USER`
//! allocation, but it cannot hand userspace a CPU address for one:
//! `gem_map_cpu` refuses an `ADDR_FBMEM` descriptor because
//! `memdescGetPhysAddr(AT_CPU)` of a VRAM object is an offset into the
//! framebuffer, not a host physical address, and publishing that offset as if
//! it were one is the hole that once let userspace paint over low kernel RAM.
//! So a VRAM object comes back with `map_handle = 0`, and every request that
//! needs both device-local placement and a CPU mapping -- NVK's
//! `DEVICE_LOCAL | HOST_VISIBLE`, which arrives as `GART | VRAM` -- is served
//! from sysmem instead, where the GPU reaches it across PCIe on every access.
//!
//! BAR1 is the aperture that makes the mapping possible. On TU10x, with the
//! RM's default configuration, it is a linear window onto VRAM starting at
//! offset 0 with no block remapping (`NV_PBUS_BAR1_BLOCK`), which is the same
//! relationship the console blit already relies on in the other direction:
//! `eclipse_rm_init.c` derives the boot framebuffer's VRAM offset as
//! `boot_fb_phys_cpu - bar1_phys`. Inverted, a VRAM object at FBMEM offset
//! `off` is reachable by the CPU at `bar1_phys + off`.
//!
//! # What this module is, and is not
//!
//! It is the arithmetic and the bounds check, with no RM call and no hardware
//! access, so the dangerous part is the part under test. The caller already
//! has both inputs: `bar1_phys` from the PCI scan and the object's FBMEM
//! offset from `gem_fbmem_offset`, which `GEM_NEW` fetches for every VRAM
//! object today and only records.
//!
//! It is **not** a decision to place `GART | VRAM` in VRAM. That changes where
//! a live compositor's buffers land and wants hardware behind it, so it stays
//! out of this file; what lands here is the mapping a VRAM object needs before
//! that question can even be asked. And it is opt-in: with
//! `nvidia.gem_vram_bar1` absent, [`enabled`] is false, the caller skips this
//! path, and `GEM_NEW` behaves byte for byte as before.
//!
//! Never run against real hardware: written in a container with no NVIDIA GPU.
//! The arithmetic is checked by the tests below; that BAR1 really is a linear
//! window at offset 0 on a given board is not something this container can
//! confirm, and a board that remaps BAR1 blocks would make
//! [`host_pa_for_vram_gem`] return a plausible address for the wrong memory.
//! That is the first thing to check on real hardware, and it is why this is
//! behind a flag.

use core::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Called from `zCore`'s cmdline parsing when `nvidia.gem_vram_bar1` is
/// present. Default off: see the module doc.
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The host physical address at which the CPU can reach a VRAM object, or
/// `None` when it cannot be mapped this way and the caller must leave
/// `map_handle` at 0.
///
/// `bar1_phys` and `bar1_len` describe the aperture (the caller's
/// `info.fb_size` is its mapped length, the same bound `drives_boot_display`
/// compares the boot framebuffer against); `fbmem_offset` and `size` describe
/// the object.
///
/// The check is that the object lies **wholly** inside the aperture. A partial
/// fit is refused rather than clamped: a clamped mapping would publish an
/// address whose first bytes are the right memory and whose tail is some other
/// object's, which is the same class of bug as publishing the raw FBMEM offset
/// and harder to see. Every addition is checked, because `bar1_phys` is read
/// from a PCI BAR and the offset comes from the RM, so neither is this
/// module's to trust.
pub(super) fn host_pa_for_vram_gem(
    bar1_phys: u64,
    bar1_len: u64,
    fbmem_offset: u64,
    size: u64,
) -> Option<u64> {
    // A zero-sized object is not a thing GEM_NEW creates, and it has to be
    // refused HERE rather than left to the bound below: `0 + 0` is not past a
    // zero-length aperture, so an empty object would otherwise be "mapped" at
    // the base address of a BAR1 that was never mapped.
    //
    // There is deliberately no separate `bar1_len == 0` arm. An unmapped BAR1
    // with a real object is already refused by the bound -- any `size > 0`
    // makes `end > 0`, and `bar1_len` is 0 -- so that arm was unkillable by
    // any test: removing it left all twelve green. A guard no test can fail is
    // a guard that does nothing, and it reads as if it were load-bearing.
    if size == 0 {
        return None;
    }
    // `end` is one past the object's last byte, inside the aperture's own
    // coordinates. Computed before the base is added so an offset the RM
    // reports wrongly cannot wrap into a small, plausible-looking address.
    let end = fbmem_offset.checked_add(size)?;
    if end > bar1_len {
        return None;
    }
    bar1_phys.checked_add(fbmem_offset)
}

/// What `GEM_NEW` calls: the hatch, the arithmetic and the one log line that
/// says a VRAM object got no CPU mapping and why.
///
/// Lives here rather than at the call site because `nvidia.rs` is one line
/// from its size ceiling, and because keeping the hatch check next to the
/// arithmetic means there is one place to read to know when this runs at all.
/// `sysmem` objects return `None` untouched: their address comes from
/// `gem_map_cpu`, and this is only ever consulted after that returned nothing.
pub(super) fn cpu_mapping_for_vram_gem(
    sysmem: bool,
    bar1_phys: u64,
    bar1_len: u64,
    fbmem_offset: Option<u64>,
    size: u64,
    handle: u32,
) -> Option<u64> {
    if sysmem || !enabled() {
        return None;
    }
    let Some(offset) = fbmem_offset else {
        crate::klog_warn!(
            "[nouveau-uapi] GEM_NEW handle={handle}: VRAM object has no FBMEM offset \
             (gem_fbmem_offset failed) -- no CPU mapping"
        );
        return None;
    };
    let pa = host_pa_for_vram_gem(bar1_phys, bar1_len, offset, size);
    if pa.is_none() {
        crate::klog_warn!(
            "[nouveau-uapi] GEM_NEW handle={handle}: VRAM object at FBMEM offset {offset:#x} \
             size={size} does not fit the BAR1 aperture ({bar1_phys:#x}+{bar1_len:#x}) \
             -- no CPU mapping"
        );
    }
    pa
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 256 MiB of aperture at 16 GiB, which is the shape of a real TU106 BAR1
    /// (the numbers only have to be plausible: nothing here reads hardware).
    const BASE: u64 = 16 << 30;
    const LEN: u64 = 256 << 20;

    #[test]
    fn an_object_at_the_start_of_the_aperture_maps_at_its_base() {
        assert_eq!(host_pa_for_vram_gem(BASE, LEN, 0, 4096), Some(BASE));
    }

    #[test]
    fn an_object_inside_the_aperture_maps_at_its_offset() {
        let off = 64 << 20;
        assert_eq!(
            host_pa_for_vram_gem(BASE, LEN, off, 1 << 20),
            Some(BASE + off)
        );
    }

    /// The boundary that decides whether the check is `>` or `>=`: an object
    /// whose last byte is the aperture's last byte fits, and one byte more
    /// does not. Written as a pair because getting this edge wrong in either
    /// direction is invisible in any other test -- too strict loses the last
    /// page of VRAM, too loose publishes an address past the window.
    #[test]
    fn an_object_ending_exactly_at_the_end_of_the_aperture_fits() {
        let size = 1 << 20;
        assert_eq!(
            host_pa_for_vram_gem(BASE, LEN, LEN - size, size),
            Some(BASE + LEN - size)
        );
    }

    #[test]
    fn an_object_ending_one_byte_past_the_aperture_is_refused() {
        let size = 1 << 20;
        assert_eq!(
            host_pa_for_vram_gem(BASE, LEN, LEN - size + 1, size),
            None,
            "an object whose tail falls outside BAR1 must not be mapped: its \
             last bytes would be another object's memory"
        );
    }

    #[test]
    fn an_offset_at_or_past_the_end_of_the_aperture_is_refused() {
        assert_eq!(host_pa_for_vram_gem(BASE, LEN, LEN, 4096), None);
        assert_eq!(host_pa_for_vram_gem(BASE, LEN, LEN + (1 << 20), 4096), None);
    }

    /// VRAM is bigger than the aperture on these cards (8 GiB of VRAM behind
    /// 256 MiB of BAR1), so an object the RM placed high in the framebuffer
    /// simply has no CPU mapping. The caller leaves `map_handle` at 0, which
    /// is exactly what it does today for every VRAM object.
    #[test]
    fn an_object_beyond_the_aperture_in_a_larger_framebuffer_is_refused() {
        assert_eq!(host_pa_for_vram_gem(BASE, LEN, 4 << 30, 4096), None);
    }

    #[test]
    fn an_unmapped_aperture_maps_nothing() {
        assert_eq!(host_pa_for_vram_gem(BASE, 0, 0, 4096), None);
        assert_eq!(host_pa_for_vram_gem(0, 0, 0, 4096), None);
    }

    #[test]
    fn a_zero_sized_object_maps_nothing() {
        assert_eq!(host_pa_for_vram_gem(BASE, LEN, 0, 0), None);
    }

    /// An offset plus a size that wraps `u64` must not come back as an address
    /// inside the aperture: the wrapped sum is small, so it passes a naive
    /// `end > bar1_len` bound, and the object would be mapped as if it were
    /// tiny.
    ///
    /// **It has to wrap on the SIZE.** The obvious inputs -- a huge offset and
    /// a 4 KiB size -- do not test this function's own `checked_add` at all:
    /// `u64::MAX + 4096` wraps to 4095, which passes the bound, but then
    /// `bar1_phys + u64::MAX` overflows and the SECOND checked addition
    /// returns `None` anyway. So the test passes with the overflow check
    /// removed, which is exactly what it was written to catch. Swapping the
    /// two (offset 1, size `u64::MAX`) leaves the base addition in range and
    /// makes the first check the only thing standing between a wrapped length
    /// and a published mapping. Found by replacing `checked_add` with
    /// `wrapping_add` and watching the earlier version of this test pass.
    #[test]
    fn a_size_that_wraps_when_added_to_the_offset_is_refused() {
        assert_eq!(host_pa_for_vram_gem(BASE, LEN, 1, u64::MAX), None);
        assert_eq!(host_pa_for_vram_gem(BASE, LEN, 4096, u64::MAX - 1), None);
    }

    /// The offset-side pair, kept because it is the shape a bad `gem_fbmem_offset`
    /// would take, with a note on which check actually refuses it.
    #[test]
    fn an_offset_near_the_top_of_the_address_space_is_refused() {
        assert_eq!(host_pa_for_vram_gem(BASE, LEN, u64::MAX, 4096), None);
        assert_eq!(host_pa_for_vram_gem(BASE, LEN, u64::MAX - 2048, 4096), None);
    }

    /// And a base plus an offset that wraps. Unreachable with a real BAR
    /// address, which is why it is checked here instead of being argued about
    /// at the call site.
    #[test]
    fn a_base_that_wraps_when_the_offset_is_added_is_refused() {
        assert_eq!(
            host_pa_for_vram_gem(u64::MAX - 4095, u64::MAX, 4096, 4096),
            None
        );
    }

    #[test]
    fn the_flag_is_off_until_the_cmdline_turns_it_on() {
        // `ENABLED` is a static shared by the whole test binary, so this
        // asserts the initial value and puts it back rather than leaving the
        // flag on for whatever runs next.
        assert!(!enabled(), "nvidia.gem_vram_bar1 must default to off");
        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        assert!(!enabled());
    }

    /// The GEM_NEW path, hatch and arithmetic together: a sysmem object is
    /// never this path's and the hatch off answers nothing, both without a
    /// word; an object the RM gave no FBMEM offset and one that does not fit
    /// are each a warning naming the handle and no mapping; one that fits is
    /// its address and no noise.
    #[test]
    fn the_gem_new_path_maps_only_a_fitting_vram_object_with_the_hatch_on() {
        let _g = crate::test_globals::LOCK.lock();
        let said = || -> alloc::vec::Vec<alloc::string::String> {
            crate::utils::host_hooks::take_dmesg()
                .into_iter()
                .map(|(_, l)| l)
                .collect()
        };
        let was = enabled();
        let _ = said();
        assert_eq!(
            cpu_mapping_for_vram_gem(false, BASE, LEN, Some(0), 4096, 1),
            None,
            "hatch off"
        );
        set_enabled(true);
        assert_eq!(
            cpu_mapping_for_vram_gem(true, BASE, LEN, Some(0), 4096, 2),
            None,
            "sysmem"
        );
        assert!(said().is_empty(), "nothing to say about either");
        assert_eq!(
            cpu_mapping_for_vram_gem(false, BASE, LEN, None, 4096, 3),
            None
        );
        let lines = said();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("handle=3") && l.contains("no FBMEM offset")),
            "{:?}",
            lines
        );
        assert_eq!(
            cpu_mapping_for_vram_gem(false, BASE, LEN, Some(LEN - 4096), 8192, 4),
            None
        );
        let lines = said();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("handle=4") && l.contains("does not fit the BAR1 aperture")),
            "{:?}",
            lines
        );
        assert_eq!(
            cpu_mapping_for_vram_gem(false, BASE, LEN, Some(64 << 20), 1 << 20, 5),
            Some(BASE + (64 << 20))
        );
        assert!(said().is_empty(), "a mapping is not news");
        set_enabled(was);
    }
}
