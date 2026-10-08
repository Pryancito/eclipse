use super::*;

/// The fields a Turing framebuffer carries: no compression, the desktop
/// sector layout, the Turing+ page-kind generation, and the generic
/// uncompressed colour kind.
const C_NONE: u64 = 0;
const S_DESKTOP: u64 = 1;
const G_TURING: u64 = 2;
const K_GENERIC_TURING: u64 = 0x06;

fn block_linear(h: u64) -> u64 {
    nvidia_block_linear_2d(C_NONE, S_DESKTOP, G_TURING, K_GENERIC_TURING, h)
}

/// The one modifier the present path can actually read, and the zero a
/// client leaves behind when it sends no modifier at all.
#[test]
fn linear_is_the_zero_modifier() {
    assert_eq!(decode_scanout_modifier(0), Some(drm::ScanoutLayout::Linear));
}

/// Every block height the hardware defines, and nothing above it:
/// `SET_SRC_BLOCK_SIZE_HEIGHT` stops at `_THIRTYTWO_GOBS` (h = 5), so a
/// larger one would be programmed as a masked-off value and the copy
/// engine would walk the surface with the wrong stride.
#[test]
fn the_six_block_heights_decode_and_the_seventh_does_not() {
    for h in 0..=5u64 {
        assert_eq!(
            decode_scanout_modifier(block_linear(h)),
            Some(drm::ScanoutLayout::BlockLinear {
                log2_gobs_per_block_y: h as u8,
                page_kind: K_GENERIC_TURING as u8,
            }),
            "h={} is a defined block height",
            h
        );
    }
    for h in 6..=15u64 {
        assert_eq!(
            decode_scanout_modifier(block_linear(h)),
            None,
            "h={} is past _THIRTYTWO_GOBS",
            h
        );
    }
}

/// The older `DRM_FORMAT_MOD_NVIDIA_16BX2_BLOCK(v)` spelling is
/// `(0, 0, 0, 0, v)`: GOB generation 0 and sector layout 0, which is the
/// Fermi..Volta / Tegra arrangement, NOT Turing's. It names a different
/// bit layout in memory, so presenting it as if it were ours would paint
/// garbage, and it is refused on that ground rather than on its page
/// kind.
///
/// What the canonicalization is for is the other case: a client that
/// sends Turing's generation but leaves `k` at 0. Kind 0 means
/// "pitch/linear", which a block-linear surface cannot be, so
/// `drm_fourcc_canonicalize_nvidia_format_mod` remaps it to 0xfe -- a
/// Fermi..Volta kind, which this decoder then refuses rather than
/// quietly presenting under a Turing modifier.
#[test]
fn the_old_16bx2_spelling_and_a_zero_page_kind_are_both_refused() {
    assert_eq!(
        decode_scanout_modifier(nvidia_block_linear_2d(0, 0, 0, 0, 2)),
        None,
        "16BX2_BLOCK names the Fermi..Volta layout, not Turing's"
    );
    assert_eq!(
        decode_scanout_modifier(nvidia_block_linear_2d(C_NONE, S_DESKTOP, G_TURING, 0, 2)),
        None,
        "kind 0 canonicalizes to 0xfe, which is not Turing's generic kind"
    );
}

/// The three fields that describe a layout we would read wrong, each
/// refused on its own so a later edit cannot drop one silently.
#[test]
fn compression_the_wrong_gob_generation_and_the_wrong_sector_layout_are_refused() {
    // c != 0: lossless compression, whose comptags nothing in this tree
    // allocates. Scanning the bytes out uncompressed is garbage, which
    // is why nv_drm_framebuffer_init refuses it too.
    for c in 1..=7u64 {
        assert_eq!(
            decode_scanout_modifier(nvidia_block_linear_2d(
                c,
                S_DESKTOP,
                G_TURING,
                K_GENERIC_TURING,
                0
            )),
            None,
            "compression type {} must not be presented",
            c
        );
    }
    // g = 1 is "Gob Height 4, G80 - GT2XX": a different GOB shape
    // entirely. g = 0 is Fermi..Volta, whose page-kind mapping differs
    // from the one VM_BIND programs.
    for g in [0u64, 1, 3] {
        assert_eq!(
            decode_scanout_modifier(nvidia_block_linear_2d(
                C_NONE,
                S_DESKTOP,
                g,
                K_GENERIC_TURING,
                0
            )),
            None,
            "GOB generation {} is not Turing's",
            g
        );
    }
    // s = 0 is the Tegra sector layout; the bits below the page kind are
    // arranged differently and the surface cannot be shared.
    assert_eq!(
        decode_scanout_modifier(nvidia_block_linear_2d(
            C_NONE,
            0,
            G_TURING,
            K_GENERIC_TURING,
            0
        )),
        None
    );
}

/// A page kind VM_BIND does not program verbatim is refused rather than
/// downgraded: the page tables and the copy engine have to agree on the
/// same kind, and a depth or compressible surface is not something this
/// scanout should be putting on a panel at all.
#[test]
fn only_turings_generic_uncompressed_colour_kind_is_accepted() {
    for k in 0x01..=0xffu64 {
        let want = k == 0x06;
        assert_eq!(
            decode_scanout_modifier(nvidia_block_linear_2d(C_NONE, S_DESKTOP, G_TURING, k, 0))
                .is_some(),
            want,
            "page kind {:#04x}",
            k
        );
    }
}

/// Anything that is not an NVIDIA block-linear modifier at all.
#[test]
fn foreign_reserved_and_invalid_modifiers_are_refused() {
    // DRM_FORMAT_MOD_INVALID.
    assert_eq!(decode_scanout_modifier(0x00ff_ffff_ffff_ffff), None);
    // Another vendor's (Intel's Y-tiling is vendor 1).
    assert_eq!(decode_scanout_modifier((1u64 << 56) | 2), None);
    // NVIDIA vendor, but bit 4 clear: not a 2D block-linear modifier.
    assert_eq!(decode_scanout_modifier(3u64 << 56), None);
    // The reserved fields "must be zero": 8:5, 11:9 and everything from
    // 28 up. A future 3D-surface modifier sets one of these and must not
    // be presented as if it were 2D.
    for bit in [5u64, 8, 9, 11, 28, 40, 55] {
        let m = block_linear(0) | (1u64 << bit);
        assert_eq!(
            decode_scanout_modifier(m),
            None,
            "reserved bit {} must refuse the modifier",
            bit
        );
    }
}

/// The size arithmetic a block-linear framebuffer needs, which differs
/// from `pitch * height` in both terms: the pitch counts 64-byte blocks,
/// and the height is padded up to a whole block.
#[test]
fn a_block_linear_surface_is_measured_in_blocks_and_padded_to_one() {
    // 1920 pixels of 4 bytes is 7680 bytes, which is 120 blocks.
    // h = 4 means blocks are 8 << 4 = 128 rows tall, so 1080 rows pad up
    // to 1152.
    assert_eq!(
        drm::block_linear_size(120, 1080, 4),
        Some(120 * 64 * 1152),
        "the last block row is addressed whole; a size from 1080 would \
         let a present read past the buffer"
    );
    // Exactly one block tall: no padding to add.
    assert_eq!(drm::block_linear_size(1, 8, 0), Some(64 * 8));
    // One row past it: a second whole block.
    assert_eq!(drm::block_linear_size(1, 9, 0), Some(2 * 64 * 8));
    // The arithmetic must not wrap: a pitch and height a client is free
    // to send have to come back as None, not as a small size that would
    // pass the buffer check.
    assert_eq!(drm::block_linear_size(u32::MAX, u32::MAX, 5), None);
}
