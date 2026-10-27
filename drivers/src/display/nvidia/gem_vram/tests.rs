use super::super::rm_host_shims::{fake_fbmem_offset, reset_fake_rm, FAKE_BAR1_BASE, FAKE_RM};
use super::*;
use crate::test_globals::LOCK;

const GART: u32 = nv::NOUVEAU_GEM_DOMAIN_GART;
const VRAM: u32 = nv::NOUVEAU_GEM_DOMAIN_VRAM;
const MIB: u64 = 1 << 20;

fn target(size: u64) -> Target {
    Target {
        device_instance: 0,
        owner_ctx: 0,
        size,
        handle: 0x8000_0042,
        bar1_phys: FAKE_BAR1_BASE,
        // Enough aperture for every offset the fake hands out.
        bar1_len: 1 << 40,
    }
}

#[test]
fn only_a_vram_or_gart_request_moves_and_only_with_the_hatch() {
    assert_eq!(placement(GART | VRAM, true), Placement::VramMapped);
    assert_eq!(
        placement(GART | VRAM, false),
        Placement::Sysmem,
        "without nvidia.gem_vram GEM_NEW does what it always did"
    );
    assert_eq!(placement(GART, true), Placement::Sysmem);
    assert_eq!(
        placement(VRAM, true),
        Placement::Vram,
        "VRAM-only gets no mapping: BAR1 could not hold NVK's device memory"
    );
    assert_eq!(placement(VRAM, false), Placement::Vram);
    assert_eq!(
        placement(nv::NOUVEAU_GEM_DOMAIN_NO_SHARE, true),
        Placement::Sysmem,
        "neither bit: the old answer (GEM_NEW refuses it before this)"
    );
}

#[test]
fn the_rm_keeps_a_share_of_bar1() {
    assert_eq!(bar1_budget(256 * MIB), 192 * MIB);
    assert_eq!(bar1_budget(128 * MIB), 64 * MIB);
    assert_eq!(bar1_budget(64 * MIB), 32 * MIB, "a small BAR1 is halved");
    assert_eq!(bar1_budget(0), 0);
}

#[test]
fn a_mapped_vram_object_is_contiguous_vram_published_at_its_bar1_address() {
    let _g = LOCK.lock();
    reset_fake_rm();
    let t = target(4 * MIB);
    let asked = core::cell::Cell::new(None);
    let placed = place(&t, Placement::VramMapped, |pa, len| {
        asked.set(Some((pa, len)));
        true
    })
    .unwrap();
    let f = FAKE_RM.lock();
    let h = placed.alloc.h_memory;
    assert_eq!(
        f.alloc_flags,
        [(h, rm_init::GEM_ALLOC_VRAM_CONTIGUOUS)],
        "one allocation, contiguous VRAM"
    );
    assert_eq!(f.bar1_maps, [h], "mapped into BAR1 by the RM");
    assert!(!placed.sysmem && placed.device_memory);
    let off = fake_fbmem_offset(h);
    assert_eq!(placed.phys_addr, Some(FAKE_BAR1_BASE + off));
    assert_eq!(placed.vram_offset, Some(off), "the display flips to this");
    assert_eq!(
        asked.get(),
        Some((FAKE_BAR1_BASE + off, 4 * MIB)),
        "the kernel's window covers the whole object"
    );
    assert_eq!(f.calls, ["gem_alloc", "gem_map_bar1"]);
}

/// Every way the VRAM path can fail ends with the object in host memory, as
/// it always was, and the VRAM allocation (with its mapping) given back.
fn assert_fell_back(placed: &Placed) {
    let f = FAKE_RM.lock();
    assert!(placed.sysmem && !placed.device_memory);
    assert_eq!(placed.vram_offset, None);
    assert_eq!(f.gems.len(), 1, "only the sysmem object is left");
    assert!(f.gems[0].2, "and it is sysmem");
    assert_eq!(placed.phys_addr, f.pa_of(placed.alloc.h_memory));
    assert!(f.bar1_maps.is_empty(), "no BAR1 mapping left behind");
    assert_eq!(f.gem_frees, 1, "the VRAM object was freed");
}

#[test]
fn bar1_full_puts_the_object_in_host_memory() {
    let _g = LOCK.lock();
    reset_fake_rm();
    FAKE_RM.lock().fail_map_bar1 = true;
    let placed = place(&target(MIB), Placement::VramMapped, |_, _| true).unwrap();
    assert_fell_back(&placed);
}

#[test]
fn no_kernel_window_puts_the_object_in_host_memory() {
    let _g = LOCK.lock();
    reset_fake_rm();
    let placed = place(&target(MIB), Placement::VramMapped, |_, _| false).unwrap();
    assert_fell_back(&placed);
}

#[test]
fn a_mapping_outside_this_cards_bar1_is_not_trusted() {
    let _g = LOCK.lock();
    reset_fake_rm();
    let mut t = target(MIB);
    // The PCI scan found BAR1 somewhere else than the RM thinks it is.
    t.bar1_phys = FAKE_BAR1_BASE + (1 << 40);
    let placed = place(&t, Placement::VramMapped, |_, _| true).unwrap();
    assert_fell_back(&placed);
}

#[test]
fn closing_the_object_gives_its_bar1_space_back() {
    let _g = LOCK.lock();
    reset_fake_rm();
    let placed = place(&target(MIB), Placement::VramMapped, |_, _| true).unwrap();
    assert_eq!(FAKE_RM.lock().bar1_maps.len(), 1);
    assert_eq!(rm_init::gem_free(0, 0, placed.alloc.h_memory), 0);
    assert!(FAKE_RM.lock().bar1_maps.is_empty());
}

#[test]
fn without_the_hatch_the_old_placements_are_unchanged() {
    let _g = LOCK.lock();
    reset_fake_rm();
    let sys = place(&target(MIB), Placement::Sysmem, |_, _| unreachable!()).unwrap();
    assert!(sys.sysmem && !sys.device_memory && sys.phys_addr.is_some());
    let vram = place(&target(MIB), Placement::Vram, |_, _| unreachable!()).unwrap();
    assert!(!vram.sysmem && !vram.device_memory);
    assert_eq!(vram.phys_addr, None, "VRAM-only: no CPU address");
    assert_eq!(
        vram.vram_offset,
        Some(fake_fbmem_offset(vram.alloc.h_memory))
    );
    let f = FAKE_RM.lock();
    assert_eq!(
        f.calls,
        ["gem_alloc", "gem_map_cpu", "gem_alloc", "gem_fbmem_offset"]
    );
    assert!(f.bar1_maps.is_empty());
}

#[test]
fn another_cards_vram_buffer_is_not_this_cards_copy_source() {
    let _g = LOCK.lock();
    const OTHER_BAR1: u64 = 0xe0_0000_0000;
    let vram = crate::scheme::gem_mmap::DRIVER_HANDLE_BASE + 0x31_0001;
    let host = crate::scheme::gem_mmap::DRIVER_HANDLE_BASE + 0x31_0002;
    crate::scheme::gem_mmap::register_device_memory(vram, OTHER_BAR1 + 0x80_0000, MIB, 1);
    crate::scheme::gem_mmap::register(host, 0x7_0000_0000, MIB, 1);
    let row = OTHER_BAR1 + 0x80_0000 + 5504 * 3;
    assert!(foreign_bar1_source(FAKE_BAR1_BASE, 256 * MIB, row));
    assert!(
        !foreign_bar1_source(OTHER_BAR1, 256 * MIB, row),
        "its own card copies it"
    );
    assert!(!foreign_bar1_source(
        FAKE_BAR1_BASE,
        256 * MIB,
        0x7_0000_1000
    ));
    assert!(crate::scheme::gem_mmap::unregister(vram));
    assert!(crate::scheme::gem_mmap::unregister(host));
}
