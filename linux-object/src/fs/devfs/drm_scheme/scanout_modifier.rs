//! Which scanout layout a framebuffer modifier asks for. Out of
//! `drm_scheme.rs` for its size ceiling; its tests are `scanout_modifier_tests`.

use super::*;

/// The page kinds a block-linear scanout buffer may carry: just
/// `NV_MMU_VER2_PTE_KIND_GENERIC_MEMORY` (0x06), which is what NVK's layout
/// library picks for every uncompressed tiled surface on Turing and what
/// `VM_BIND` programs into the page tables verbatim. The page table and the
/// copy engine have to agree on one kind, and this is it.
///
/// `0xfe`, which the canonicalization above can produce, is the generic kind
/// of the **Fermi..Volta** mapping, so it only ever arrives alongside a `g`
/// this decoder has already refused. It is not listed here, because listing
/// it would mean accepting a Turing-generation modifier whose kind came from
/// another generation's table.
///
/// A COMPRESSED kind is refused rather than downgraded: the comptags that
/// give it meaning are allocated nowhere in this tree, and scanning out
/// compressed bytes as if they were uncompressed is the one way this path
/// paints garbage.
pub(super) const NVIDIA_SCANOUT_PAGE_KINDS: [u8; 1] = [0x06];

/// What layout `modifier` asks for, or `None` if this scanout cannot present
/// it.
///
/// Mirrors `nv_drm_framebuffer_init`'s checks (the lossless-compression field
/// must be zero) and adds the two this tree needs: the GOB generation has to
/// be the one the copy engine's `SET_SRC_BLOCK_SIZE` assumes, and the page
/// kind has to be one `VM_BIND` maps verbatim.
pub(super) fn decode_scanout_modifier(modifier: u64) -> Option<drm::ScanoutLayout> {
    if modifier == DRM_FORMAT_MOD_LINEAR {
        return Some(drm::ScanoutLayout::Linear);
    }
    if modifier == DRM_FORMAT_MOD_INVALID {
        return None;
    }
    if modifier >> 56 != DRM_FORMAT_MOD_VENDOR_NVIDIA {
        return None;
    }
    let m = canonicalize_nvidia_modifier(modifier & 0x00ff_ffff_ffff_ffff);
    // Bit 4 must be 1: without it the value is one of the older
    // non-block-linear NVIDIA modifiers, not a 2D block-linear one.
    if m & 0x10 == 0 {
        return None;
    }
    // Bits 8:5 and 11:9 are reserved and "must be zero"; so is everything
    // from 28 up. Refusing them keeps a future 3D-surface or array-stride
    // modifier from being silently presented as a 2D one.
    if m & 0x00ff_ffff_f000_0fe0 != 0 {
        return None;
    }
    let h = m & 0xf;
    let k = (m >> 12) & 0xff;
    let g = (m >> 20) & 0x3;
    // `s` is bit 22 plus bits 27:26; the high half is already covered by the
    // reserved-bits check above, so only bit 22 is left to read.
    let s = (m >> 22) & 0x1;
    let c = (m >> 23) & 0x7;
    if c != 0 || g != NVIDIA_MOD_GEN_TURING || s != NVIDIA_MOD_SECTOR_DESKTOP {
        return None;
    }
    if h > NVIDIA_MOD_MAX_LOG2_GOBS_Y {
        return None;
    }
    if !NVIDIA_SCANOUT_PAGE_KINDS.contains(&(k as u8)) {
        return None;
    }
    Some(drm::ScanoutLayout::BlockLinear {
        log2_gobs_per_block_y: h as u8,
        page_kind: k as u8,
    })
}
