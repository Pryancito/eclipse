//! Which scanout layout a framebuffer modifier asks for. Out of
//! `drm_scheme.rs` for its size ceiling; its tests are `scanout_modifier_tests`.

use super::*;
use alloc::vec::Vec;

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

/// The id `GETPROPBLOB` serves [`in_formats`] under: one below the EDID
/// range, so it is no connector's and no id the blob store hands out.
pub(super) const IN_FORMATS_BLOB_ID: u32 = EDID_BLOB_BASE - 1;

/// The primary plane's `IN_FORMATS` blob (`struct drm_format_modifier_blob`,
/// version 1), as nvidia-drm builds it for Turing: the block-linear
/// modifiers with the generic page kind from 32 GOBs high down to one, then
/// `LINEAR`, each for every format of [`drm::SCANOUT_FORMATS`]. Each one is
/// a layout [`decode_scanout_modifier`] accepts, so what wlroots picks from
/// here `ADDFB2` takes.
pub(super) fn in_formats() -> Vec<u8> {
    let formats = &drm::SCANOUT_FORMATS;
    let mut modifiers: Vec<u64> = (0..=NVIDIA_MOD_MAX_LOG2_GOBS_Y as u8)
        .rev()
        .map(|h| {
            scanout_layout_modifier(drm::ScanoutLayout::BlockLinear {
                log2_gobs_per_block_y: h,
                page_kind: NVIDIA_SCANOUT_PAGE_KINDS[0],
            })
        })
        .collect();
    modifiers.push(DRM_FORMAT_MOD_LINEAR);
    const HEADER: usize = 24;
    let formats_offset = HEADER;
    // `drm_format_modifier` holds a u64: the array starts 8-aligned.
    let modifiers_offset = (formats_offset + 4 * formats.len()).next_multiple_of(8);
    let mut blob = Vec::with_capacity(modifiers_offset + 24 * modifiers.len());
    for word in [
        1, // version: FORMAT_BLOB_CURRENT
        0, // flags
        formats.len() as u32,
        formats_offset as u32,
        modifiers.len() as u32,
        modifiers_offset as u32,
    ] {
        blob.extend_from_slice(&word.to_ne_bytes());
    }
    for f in formats {
        blob.extend_from_slice(&f.to_ne_bytes());
    }
    blob.resize(modifiers_offset, 0);
    // Every modifier applies to every format: mask bits 0..n from index 0.
    let mask: u64 = (1u64 << formats.len()) - 1;
    for m in modifiers {
        blob.extend_from_slice(&mask.to_ne_bytes());
        blob.extend_from_slice(&0u32.to_ne_bytes()); // offset
        blob.extend_from_slice(&0u32.to_ne_bytes()); // pad
        blob.extend_from_slice(&m.to_ne_bytes());
    }
    blob
}
