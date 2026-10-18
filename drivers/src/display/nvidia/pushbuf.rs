//! `GEM_PUSHBUF`: the arithmetic of a real nvc0 submission.
//!
//! # What this is for
//!
//! `GEM_PUSHBUF` is the submission path of the classic **nvc0 Gallium**
//! driver -- Mesa's OpenGL for Turing, which is what the shipped image
//! actually runs. Until now the ioctl parsed the whole request, logged its
//! anatomy and returned `EOPNOTSUPP`: honest, but no OpenGL client ever got a
//! triangle out of it. NVK has the other uAPI (`VM_BIND` + `EXEC`) and is
//! unaffected.
//!
//! Two things stood between the parse and a submission, and both are here:
//! every listed buffer needs a GPU VA in the channel's VAS so a push can name
//! one, and the relocs have to patch those VAs into the pushbuffer words. The
//! ring submission itself already exists (`eclipse_rm_exec_submit`, which
//! `EXEC` drives), and **the 3D class was never missing**: every channel the
//! glue builds gets `TURING_A` (0xc597).
//!
//! # What lives here, and why
//!
//! The parts that are wrong silently. A reloc writes a 32-bit word into a
//! buffer the client will hand straight to the GPU; the wrong word is not an
//! error return, it is a method stream addressing memory that belongs to
//! something else. So the word, the bounds and the ranges are computed here,
//! FFI-free and under test, and the ioctl does the mapping and the stores.
//!
//! Three details that are easy to get wrong and are pinned by tests below:
//!
//! 1. **`LOW` wins over `HIGH`.** `nouveau_gem_pushbuf_reloc_apply` is an
//!    `if LOW / else if HIGH / else`, so a reloc with both bits gets the low
//!    half, not the high one and not a combination.
//! 2. **`OR` picks `vor` or `tor` by the buffer's domain**, and it is an
//!    addition to whichever word the first step produced, not a replacement.
//! 3. **An unaligned `reloc_bo_offset` is refused here**, where Linux lets it
//!    through. Linux stores with `nouveau_bo_wr32(nvbo, off >> 2, data)`, so
//!    an offset of 6 silently patches the word at 4 -- a different method's
//!    operand. Refusing is the divergence; patching the wrong word is not
//!    something to be bug-compatible about.
//!
//! Opt-in: with `nvidia.gem_pushbuf` absent, [`enabled`] is false and the
//! ioctl parses, logs and returns `EOPNOTSUPP` exactly as it did before.
//!
//! Never run against real hardware: written in a container with no NVIDIA
//! GPU, so no method stream from this path has ever reached an engine. The
//! arithmetic is checked below and the uAPI semantics are read off
//! `nouveau_gem_ioctl_pushbuf`; neither is the same claim as having seen a
//! frame come out. That is why it is behind a flag.

use core::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Called from `zCore`'s cmdline parsing when `nvidia.gem_pushbuf` is
/// present. Default off: see the module doc.
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// `NOUVEAU_GEM_RELOC_LOW`: patch the low 32 bits of the buffer's VA.
pub(super) const NOUVEAU_GEM_RELOC_LOW: u32 = 1 << 0;
/// `NOUVEAU_GEM_RELOC_HIGH`: patch the high 32 bits.
pub(super) const NOUVEAU_GEM_RELOC_HIGH: u32 = 1 << 1;
/// `NOUVEAU_GEM_RELOC_OR`: OR in `vor` or `tor` depending on the domain the
/// buffer ended up in.
pub(super) const NOUVEAU_GEM_RELOC_OR: u32 = 1 << 2;

/// The 32-bit word a reloc patches into the pushbuffer, from
/// `nouveau_gem_pushbuf_reloc_apply`.
///
/// `bo_va` is the GPU VA the *referenced* buffer was mapped at (not the
/// buffer being patched), and `bo_in_vram` says which of `vor`/`tor` the `OR`
/// step uses.
///
/// The sum is taken in 64 bits and then sliced, and it is the HIGH half that
/// needs it. Truncation commutes with addition, so the low word is the same
/// whether the add happens in 32 or 64 bits -- an earlier version of this
/// comment claimed otherwise, and a mutation that computed the low half as
/// `(bo_va as u32).wrapping_add(data)` survived every test, which is how the
/// claim was caught. The high half is the one that can be got wrong: shifting
/// first and adding after (`(bo_va >> 32) as u32 + data`) both drops the
/// carry out of the low word and adds a byte offset to a word count, so a
/// reloc pair whose offset crosses a 4 GiB boundary would name an address
/// 4 GiB away from the buffer.
pub(super) fn reloc_word(
    flags: u32,
    data: u32,
    vor: u32,
    tor: u32,
    bo_va: u64,
    bo_in_vram: bool,
) -> u32 {
    // `if LOW / else if HIGH / else`, in that order, because that is what the
    // kernel does: a reloc with both bits set gets the low half.
    let mut word = if flags & NOUVEAU_GEM_RELOC_LOW != 0 {
        bo_va.wrapping_add(data as u64) as u32
    } else if flags & NOUVEAU_GEM_RELOC_HIGH != 0 {
        (bo_va.wrapping_add(data as u64) >> 32) as u32
    } else {
        data
    };
    if flags & NOUVEAU_GEM_RELOC_OR != 0 {
        word |= if bo_in_vram { vor } else { tor };
    }
    word
}

/// Where in the patched buffer the word goes, as a byte offset, or `None`
/// when the reloc does not name a word of it.
///
/// Both refusals matter, and only one of them is Linux's:
///
/// - The word has to lie wholly inside the buffer (`off + 4 <= size`), which
///   is `nouveau_gem_pushbuf_reloc_apply`'s own check. Note it must be the
///   whole word: a reloc two bytes before the end passes `off < size` and
///   then writes past it.
/// - The offset has to be word-aligned. Linux stores through
///   `nouveau_bo_wr32(nvbo, off >> 2, ...)`, which rounds an unaligned offset
///   DOWN to the previous word and patches a different method's operand with
///   no error anywhere. See the module doc.
pub(super) fn reloc_target_offset(reloc_bo_offset: u32, reloc_bo_size: u64) -> Option<u64> {
    if !reloc_bo_offset.is_multiple_of(4) {
        return None;
    }
    let off = reloc_bo_offset as u64;
    if off.checked_add(4)? > reloc_bo_size {
        return None;
    }
    Some(off)
}

/// A push's GPU address and length, or `None` when the push does not name a
/// range of its buffer.
///
/// `offset` and `length` are the client's, so neither is this module's to
/// trust: the sum is checked, the range has to fit the buffer, and the length
/// has to fit the `u32` the ring submission takes. A zero-length push is
/// refused rather than submitted as an empty ring entry, and both ends have
/// to be word-aligned because the GPFIFO addresses 32-bit method words -- an
/// unaligned push makes the engine read a method stream shifted by a byte,
/// which is not a recoverable kind of wrong.
pub(super) fn push_gpu_range(
    bo_va: u64,
    bo_size: u64,
    offset: u64,
    length: u64,
) -> Option<(u64, u32)> {
    if length == 0 || !offset.is_multiple_of(4) || !length.is_multiple_of(4) {
        return None;
    }
    let end = offset.checked_add(length)?;
    if end > bo_size {
        return None;
    }
    if length > u32::MAX as u64 {
        return None;
    }
    Some((bo_va.checked_add(offset)?, length as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plausible GPU VA: NVK and nvc0 both live well inside the 47-bit VAS,
    /// and this one is deliberately NOT 4 GiB-aligned so a lost carry shows.
    const VA: u64 = 0x7F_FFFF_F000;

    #[test]
    fn a_reloc_with_neither_half_passes_its_data_through() {
        assert_eq!(reloc_word(0, 0xDEAD_BEEF, 0, 0, VA, false), 0xDEAD_BEEF);
    }

    #[test]
    fn the_low_reloc_is_the_low_half_of_the_address_plus_the_offset() {
        assert_eq!(
            reloc_word(NOUVEAU_GEM_RELOC_LOW, 0x100, 0, 0, VA, false),
            (VA + 0x100) as u32
        );
    }

    #[test]
    fn the_high_reloc_is_the_high_half() {
        assert_eq!(
            reloc_word(NOUVEAU_GEM_RELOC_HIGH, 0x100, 0, 0, VA, false),
            ((VA + 0x100) >> 32) as u32
        );
    }

    /// The pair has to describe ONE address. `VA + 0x2000` carries out of the
    /// low word, so a high half computed by shifting before adding would come
    /// back 4 GiB below where the low half points. (It is only the high half
    /// this pins: the low word is the same in 32-bit or 64-bit arithmetic --
    /// see the note on [`reloc_word`].)
    #[test]
    fn a_pair_whose_offset_carries_out_of_the_low_word_still_names_one_address() {
        let data = 0x2000;
        let lo = reloc_word(NOUVEAU_GEM_RELOC_LOW, data, 0, 0, VA, false);
        let hi = reloc_word(NOUVEAU_GEM_RELOC_HIGH, data, 0, 0, VA, false);
        assert_eq!(
            ((hi as u64) << 32) | lo as u64,
            VA + data as u64,
            "the two halves of a reloc pair must reassemble into the VA the \
             client asked for, carry included",
        );
    }

    /// `if LOW / else if HIGH`: both bits set is the LOW case, not the high
    /// one and not a mix. Worth a test of its own because either other
    /// reading looks just as plausible in the flags.
    #[test]
    fn low_wins_when_a_reloc_asks_for_both_halves() {
        assert_eq!(
            reloc_word(
                NOUVEAU_GEM_RELOC_LOW | NOUVEAU_GEM_RELOC_HIGH,
                0x100,
                0,
                0,
                VA,
                false
            ),
            (VA + 0x100) as u32,
        );
    }

    #[test]
    fn the_or_bit_picks_vor_in_vram_and_tor_everywhere_else() {
        let f = NOUVEAU_GEM_RELOC_OR;
        assert_eq!(reloc_word(f, 0, 0xA000, 0x5000, VA, true), 0xA000);
        assert_eq!(reloc_word(f, 0, 0xA000, 0x5000, VA, false), 0x5000);
    }

    /// `OR` adds to whatever the first step produced; it does not replace it.
    #[test]
    fn the_or_bit_is_added_to_the_address_half_not_instead_of_it() {
        let f = NOUVEAU_GEM_RELOC_LOW | NOUVEAU_GEM_RELOC_OR;
        assert_eq!(
            reloc_word(f, 0, 0x7, 0, VA, true),
            (VA as u32) | 0x7,
            "the tiling/compression bits ride on the address, they do not \
             overwrite it",
        );
    }

    #[test]
    fn a_word_inside_the_buffer_is_patched_where_the_reloc_says() {
        assert_eq!(reloc_target_offset(0, 4), Some(0));
        assert_eq!(reloc_target_offset(64, 4096), Some(64));
    }

    /// The edge that decides `>` versus `>=`: the last word of the buffer is
    /// patchable and one word further is not.
    #[test]
    fn the_last_word_of_the_buffer_is_patchable_and_the_next_is_not() {
        assert_eq!(reloc_target_offset(4092, 4096), Some(4092));
        assert_eq!(reloc_target_offset(4096, 4096), None);
    }

    /// Not `off < size`: a reloc two bytes before the end passes that and
    /// then writes two bytes past the buffer.
    #[test]
    fn a_word_that_only_partly_fits_is_refused_not_clipped() {
        assert_eq!(
            reloc_target_offset(4088, 4090),
            None,
            "a word whose tail falls outside the buffer must not be written \
             at all",
        );
    }

    /// The divergence from Linux, which rounds down and patches the previous
    /// word instead.
    #[test]
    fn an_unaligned_reloc_offset_is_refused_rather_than_rounded_down() {
        assert_eq!(reloc_target_offset(6, 4096), None);
        assert_eq!(reloc_target_offset(1, 4096), None);
    }

    #[test]
    fn a_reloc_offset_near_the_top_of_the_address_space_is_refused() {
        // Aligned, so it gets past the first check and has to be refused by
        // the bound rather than by wrapping into a small offset.
        assert_eq!(reloc_target_offset(u32::MAX - 3, 4096), None);
    }

    #[test]
    fn a_push_inside_its_buffer_becomes_a_va_and_a_length() {
        assert_eq!(push_gpu_range(VA, 4096, 256, 128), Some((VA + 256, 128)));
    }

    #[test]
    fn a_push_filling_its_buffer_exactly_fits_and_one_word_more_does_not() {
        assert_eq!(push_gpu_range(VA, 4096, 0, 4096), Some((VA, 4096)));
        assert_eq!(push_gpu_range(VA, 4096, 4, 4096), None);
    }

    #[test]
    fn an_empty_push_is_refused_rather_than_submitted() {
        assert_eq!(push_gpu_range(VA, 4096, 0, 0), None);
    }

    /// Both ends, because an unaligned length leaves the NEXT push's VA
    /// unaligned even when this one's offset was fine.
    #[test]
    fn an_unaligned_push_is_refused_at_either_end() {
        assert_eq!(push_gpu_range(VA, 4096, 2, 128), None);
        assert_eq!(push_gpu_range(VA, 4096, 0, 130), None);
    }

    /// **Every other check has to be stepped around for this to test its own
    /// one.** A length past `u32::MAX` is refused by the cap below it, and a
    /// huge OFFSET is refused by the `bo_va + offset` addition under that, so
    /// both of the obvious inputs pass with `offset.checked_add` removed --
    /// which is exactly what this was written to catch. What is left is a
    /// word-aligned offset near the top of the range with a small length, and
    /// a base of 0 so the last addition stays in range: then the wrapped sum
    /// (4) is inside the buffer, the length fits a `u32`, and the first check
    /// is the only thing between a wrapped range and a submitted push. Found
    /// by replacing it with `wrapping_add` and watching the earlier version of
    /// this test pass.
    #[test]
    fn a_length_that_wraps_when_added_to_the_offset_is_refused() {
        assert_eq!(push_gpu_range(0, 4096, u64::MAX - 3, 8), None);
    }

    /// A length past `u32` cannot be handed to the ring submission, which
    /// takes a `u32` byte count. Aligned and non-wrapping, so only the
    /// conversion can refuse it.
    #[test]
    fn a_push_longer_than_the_ring_submission_can_describe_is_refused() {
        let big = 1u64 << 32;
        assert_eq!(push_gpu_range(VA, big + 4096, 0, big), None);
    }

    #[test]
    fn a_base_that_wraps_when_the_offset_is_added_is_refused() {
        assert_eq!(push_gpu_range(u64::MAX - 4095, u64::MAX, 4096, 4), None);
    }

    #[test]
    fn the_flag_is_off_until_the_cmdline_turns_it_on() {
        // `ENABLED` is a static shared by the whole test binary, so this
        // asserts the initial value and puts it back rather than leaving the
        // flag on for whatever runs next.
        assert!(!enabled(), "nvidia.gem_pushbuf must default to off");
        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        assert!(!enabled());
    }
}
