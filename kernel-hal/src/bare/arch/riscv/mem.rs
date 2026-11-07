use alloc::vec::Vec;
use core::ops::Range;

use crate::addr::{align_down, align_up};
use crate::utils::init_once::InitOnce;
use crate::{PhysAddr, KCONFIG, PAGE_SIZE};

fn cut_off(total: Range<PhysAddr>, cut: &Range<PhysAddr>) -> Vec<Range<PhysAddr>> {
    let mut regions = Vec::new();
    // no overlap at all
    if cut.end <= total.start || total.end <= cut.start {
        regions.push(total);
    } else {
        // no overlap on the left
        if total.start < cut.start {
            regions.push(total.start..align_down(cut.start));
        }
        // no overlap on the right
        if cut.end < total.end {
            regions.push(align_up(cut.end)..total.end);
        }
    }
    regions
}

pub fn free_pmem_regions() -> Vec<Range<PhysAddr>> {
    extern "C" {
        fn end();
    }

    static FREE_PMEM_REGIONS: InitOnce<Vec<Range<PhysAddr>>> = InitOnce::new();
    FREE_PMEM_REGIONS.init_once(|| {
        let initrd = super::INITRD_REGION.as_ref();
        let dtb = Range::<PhysAddr> {
            start: KCONFIG.dtb_paddr,
            end: KCONFIG.dtb_paddr + KCONFIG.dtb_size,
        };
        let min_start =
            align_up(end as *const () as usize + PAGE_SIZE) - KCONFIG.phys_to_virt_offset;

        let mut regions = Vec::new();
        for r in super::MEMORY_REGIONS.iter() {
            let base = align_up(r.start.max(min_start))..align_down(r.end);
            let mut no_dtb = cut_off(base, &dtb);
            if let Some(initrd) = initrd {
                for range in no_dtb {
                    let mut no_initrd = cut_off(range, initrd);
                    regions.append(&mut no_initrd);
                }
            } else {
                regions.append(&mut no_dtb);
            }
        }
        regions
    });
    FREE_PMEM_REGIONS.clone()
}

/// Write one page of a frame back from every cache and drop it, before the
/// frame changes cache policy (`zx_vmo_set_cache_policy`) and the next
/// access goes to memory directly.
///
/// RISC-V has no base cache-maintenance instruction: `cbo.flush` is the
/// Zicbom extension, and whether the harts have it, and how many bytes one
/// instruction covers, comes from the device tree (`cpu::init_cbom`). Without
/// it this is the `fence` it always amounted to on a `virt` machine, whose
/// memory is coherent anyway. This used to be `unimplemented!()`, so the
/// first `set_cache_policy` on a VMO with pages (`VmoTestCase.Cache` is one)
/// was a kernel panic that took the whole guest with it.
pub fn frame_flush(target: PhysAddr) {
    let block = super::cpu::cbom_block_size();
    if block != 0 {
        let base = crate::mem::phys_to_virt(target);
        crate::common::cache_maint::for_each_line(base, PAGE_SIZE, block, |addr| {
            // SAFETY: `cbo.flush` takes a virtual address inside a block to
            // flush; `addr` is in the direct map of `target`, a frame the
            // caller owns. The device tree said every hart has Zicbom.
            unsafe {
                core::arch::asm!(
                    ".option push",
                    ".option arch, +zicbom",
                    "cbo.flush 0({0})",
                    ".option pop",
                    in(reg) addr,
                    options(nostack, preserves_flags)
                );
            }
        });
    }
    // SAFETY: a full memory fence has no preconditions.
    unsafe { core::arch::asm!("fence rw, rw", options(nostack, preserves_flags)) };
}
