use super::*;

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
