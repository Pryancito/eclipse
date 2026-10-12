use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

#[test]
fn a_vram_gem_is_not_a_missing_handle() {
    assert_eq!(
        unresolved_backing_from(Some(GemAperture::Vram)),
        UnresolvedBacking::Vram
    );
}

#[test]
fn an_unknown_handle_and_a_driverless_boot_both_read_as_missing() {
    assert_eq!(
        unresolved_backing_from(Some(GemAperture::Unknown)),
        UnresolvedBacking::Unknown
    );
    // No primary driver to ask: the generic line, not a VRAM claim we
    // have nothing to back.
    assert_eq!(unresolved_backing_from(None), UnresolvedBacking::Unknown);
}

/// The driver claiming a host address while the resolver found none is
/// the OWNERSHIP check refusing a handle this process does not hold --
/// a lifetime bug, which is what the generic wording describes. Calling
/// that one "VRAM-only" would send the reader to the wrong half of the
/// kernel.
#[test]
fn sysmem_that_would_not_resolve_is_an_ownership_bug_not_an_aperture_one() {
    assert_eq!(
        unresolved_backing_from(Some(GemAperture::Sysmem)),
        UnresolvedBacking::Unknown
    );
}

/// The frame path builds the framebuffer on the display card, so the line
/// that explains an unresolvable ADDFB2 handle has to ask that card: `card0`
/// is the primary, and on a two-card box (compute probed last, so first in
/// the registry) that is the compute one, which knows nothing of the
/// swapchain's VRAM objects and called every one of them a missing handle.
#[test]
fn the_scanout_diagnostic_asks_the_display_card_not_card0() {
    const ON_THE_PANEL: u32 = 0xbfff_0040;
    const ON_THE_COMPUTE_CARD: u32 = 0xbfff_0041;
    struct NodeTable;
    impl Drop for NodeTable {
        fn drop(&mut self) {
            clear_gpu_nodes_for_test();
        }
    }
    let screen = kms_emu::attach(16, 16);
    let _table = NodeTable;
    // The registry puts the last driver registered first, and the compute
    // card is probed last.
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_vram_object(ON_THE_PANEL),
    );
    let _compute = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-compute")
            .compute_at(0x65, 0x00)
            .with_vram_object(ON_THE_COMPUTE_CARD),
    );
    build_gpu_nodes();
    assert!(
        get_primary_driver().is_some_and(|d| d.is_compute_gpu()),
        "the box this models: card0 is the compute card"
    );
    assert_eq!(
        unresolved_scanout_backing_reason(ON_THE_PANEL),
        UnresolvedBacking::Vram,
        "a VRAM object of the display card is a VRAM object"
    );
    assert_eq!(
        unresolved_scanout_backing_reason(ON_THE_COMPUTE_CARD),
        UnresolvedBacking::Unknown,
        "the compute card's object is nothing the frame path can scan out"
    );
}
