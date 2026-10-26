//! Comptags: honouring a compressed PTE kind instead of downgrading it.
//!
//! # Why there is nothing to allocate
//!
//! `pte_kind_is_supported` in `nouveau_uapi.rs` has the long version. In
//! short: on Turing the comptag line is not drawn from a pool, it is a slice
//! of the surface's own physical address. The RM's Turing HAL,
//! `memmgrDetermineComptag_TU102`, is `1 + physAddr[35:16]`, and
//! `kmemsysAllocComprResources` allocates no tags either. So "this driver has
//! no comptag allocator" was never the reason compression was missing.
//!
//! # What this module decides
//!
//! Whether a given `VM_BIND` may pass a compressible kind through to the PTEs
//! rather than let the C glue swap in its uncompressed pair. Getting that
//! wrong does not return an error: it hands the engine a tag line that
//! describes other bytes, or a surface the engine reads as compressed when
//! nothing wrote compressed data there. So the decision is made here, where it
//! is under test, and the C side only obeys
//! (`eclipse_rm_vm_bind_map`'s `bHonourCompressedKind`).
//!
//! Three mistakes this is shaped to prevent:
//!
//! 1. **There are three comptag modes, not two.** One-to-one and raw are the
//!    ones where the line comes from the physical address, and the RM's own
//!    asserts accept only those. `bUseOneToFourComptagLineAllocation` exists
//!    and is neither, so [`ComptagCaps::one_to_four`] is carried explicitly:
//!    "not one-to-one, therefore raw" is exactly the inference that would put
//!    a wrong line in a PTE on such a board.
//! 2. **The page has to be at least a comptag line wide.** One line covers
//!    `1 << 16` bytes, so a 4 KiB page cannot carry its own.
//! 3. **Scrub-on-free is not bookkeeping.** The RM refuses a compressible
//!    allocation without it because a line inherited from a freed surface
//!    describes bytes that are gone.
//!
//! Opt-in: with `nvidia.gem_comptags` absent, [`enabled`] is false, every
//! bind is asked for with the flag clear, and the C glue downgrades exactly as
//! it did before -- byte for byte the old behaviour.
//!
//! Never run against real hardware: written in a container with no NVIDIA GPU,
//! where [`ComptagCaps`] cannot even be read (the query needs a GPU, so it
//! fails and compression stays off). The derivation is quoted from the RM's
//! own Turing HAL and the decision is covered by the tests below; neither is
//! the same claim as having seen a compressed surface come out right. That is
//! why this is behind a flag.

use core::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Called from `zCore`'s cmdline parsing when `nvidia.gem_comptags` is
/// present. Default off: see the module doc.
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One comptag line covers `1 << 16` bytes, from the low bit of
/// `NV_MMU_PTE_COMPTAGLINE_BITS_FROM_SPA` (`35:16` in tu102
/// `dev_fb_addendum.h`). This is why a compressed surface needs a 64 KiB page.
pub const COMPTAG_PAGE_SHIFT: u32 = 16;

/// The high bit of that same field.
const SPA_HI_BIT: u32 = 35;

/// How many bits the derived line has before the `+ 1`: 20, from `35:16`.
const SPA_FIELD_BITS: u32 = SPA_HI_BIT + 1 - COMPTAG_PAGE_SHIFT;

/// The widest line the PTE can hold. `NV_MMU_VER2_PTE_COMPTAGLINE` is
/// `(20+35):36`, so 20 bits -- the same width as the field the line is derived
/// *from*, which is what makes the `+ 1` a boundary worth checking.
const MAX_PTE_COMPTAG_LINE: u32 = (1 << SPA_FIELD_BITS) - 1;

/// What the GPU reports about comptags, from `eclipse_rm_comptag_caps`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ComptagCaps {
    /// `bOneToOneComptagLineAllocation`: line from the physical address.
    pub one_to_one: bool,
    /// `bUseOneToFourComptagLineAllocation`: the third mode, in which the
    /// derivation below does NOT hold. Carried so it can be refused by name.
    pub one_to_four: bool,
    /// `bUseRawModeComptaglineAllocation`: hardware manages the tags.
    pub raw_mode: bool,
    /// `bDisableCompbitBacking`: the compression bit backing store is off, so
    /// there is nowhere for a tag line to live whatever the mode says.
    pub compbit_backing_off: bool,
    /// `bDisablePostL2Compression`.
    pub post_l2_off: bool,
    /// `memmgrIsScrubOnFreeEnabled`.
    pub scrub_on_free: bool,
    /// `memmgrUseVasForCeMemoryOps`, which the RM asserts in the one-to-one
    /// branch specifically.
    pub vas_for_ce: bool,
    /// `comprPageShift`, `log2(comprPageSize)`. Checked rather than assumed:
    /// the whole derivation is built on this being
    /// [`COMPTAG_PAGE_SHIFT`].
    pub compr_page_shift: u32,
}

/// The comptag line the hardware will use for a surface at host physical
/// address `pa`, or `None` when `pa` cannot be described by one.
///
/// This is `memmgrDetermineComptag_TU102` and nothing else. Both refusals are
/// real rather than defensive:
///
/// - A `pa` with any bit above 35 set has those bits **dropped** by the field,
///   so two surfaces 64 GiB apart would be handed the same line. That is one
///   surface's compression state describing another's bytes, and it must not
///   be papered over by truncation.
/// - The top line does not fit the PTE. The field the line is read from and
///   the field it is written to are both 20 bits, and the derivation adds one,
///   so a `pa` in the last representable comptag page yields `0x10_0000`,
///   which is 21 bits. Unreachable on any real Turing board -- it needs a
///   physical address within 64 KiB of 64 GiB -- but it is the arithmetic's
///   actual edge, and `pa` comes from the RM rather than from this module.
pub(super) fn comptag_line_for_pa(pa: u64) -> Option<u32> {
    if pa >> (SPA_HI_BIT + 1) != 0 {
        return None;
    }
    let line = 1 + ((pa >> COMPTAG_PAGE_SHIFT) as u32 & MAX_PTE_COMPTAG_LINE);
    if line > MAX_PTE_COMPTAG_LINE {
        return None;
    }
    Some(line)
}

/// Whether a compressible kind may reach the PTEs for a surface in `domain`
/// of `size` bytes, given what the GPU reports.
///
/// `size` is the GEM object's recorded size, which `GEM_NEW` has already
/// rounded up to its GPU page (see the `GEM_INFO` row of
/// `docs/README-nouveau-uapi.md`), so comparing it against a comptag line is
/// a check on the page and not on what the client asked for.
///
/// Everything here is a refusal except the two modes, because every unknown
/// should end as "downgrade", which is the behaviour that has been running.
pub(super) fn may_honour_compressed_kind(caps: ComptagCaps, vram: bool, size: u64) -> bool {
    // Compression lives in the L2 tag store; a sysmem page has no line in it.
    if !vram {
        return false;
    }
    // A line inherited from a freed surface describes bytes that are gone.
    if !caps.scrub_on_free {
        return false;
    }
    // No backing store for the tag bits, so the mode is beside the point.
    if caps.compbit_backing_off {
        return false;
    }
    // The derivation IS this shift. A board that reports another one is not a
    // board this module has read the HAL for.
    if caps.compr_page_shift != COMPTAG_PAGE_SHIFT {
        return false;
    }
    if size < (1u64 << COMPTAG_PAGE_SHIFT) {
        return false;
    }
    // The third mode, refused by name rather than by falling off the end: in
    // one-to-four the line is not `1 + pa[35:16]`, so honouring a kind here
    // would put a line in the PTE that belongs to other memory.
    if caps.one_to_four {
        return false;
    }
    if caps.raw_mode {
        return true;
    }
    caps.one_to_one && caps.vas_for_ce
}

/// The GPU's comptag caps, read once at init. `None` until
/// [`set_caps`] runs, and `None` is a refusal: a failed query (which is what
/// happens with no GPU) leaves compression off rather than guessing.
static CAPS: spin::Mutex<Option<ComptagCaps>> = spin::Mutex::new(None);

/// Called once per GPU bring-up with what `eclipse_rm_comptag_caps` returned,
/// or with `None` when the query failed. Cached because a game binds dozens of
/// surfaces a second and each bind would otherwise take the RM lock to ask a
/// question whose answer is static after state load.
pub fn set_caps(caps: Option<ComptagCaps>) {
    *CAPS.lock() = caps;
}

/// True the first time it is called and false ever after, so the caller asks
/// the RM for the caps once rather than on every bind. Separate from
/// [`set_caps`] because a FAILED query must also count as asked: otherwise a
/// GPU-less boot would re-enter the RM on every single bind.
pub(super) fn claim_caps_query() -> bool {
    !CAPS_ASKED.swap(true, Ordering::Relaxed)
}

static CAPS_ASKED: AtomicBool = AtomicBool::new(false);

/// What is cached, for the tests of the reader next door.
#[cfg(test)]
pub(super) fn caps() -> Option<ComptagCaps> {
    *CAPS.lock()
}

/// Puts the one-shot back, so a test can watch the claim happen.
#[cfg(test)]
pub(super) fn reset_caps_asked() {
    CAPS_ASKED.store(false, Ordering::Relaxed);
}

/// Everything `VM_BIND` needs to decide, in one call: the hatch, whether the
/// kind is compressible at all, the GPU's caps and the surface's placement.
///
/// `true` means the C glue is asked to leave the kind alone so the RM derives
/// the comptag line itself. Anything unknown returns `false`, which is the
/// downgrade that has been running all along.
///
/// `vram_offset` is only used to say, once per honoured bind, which line the
/// hardware will use -- that number is the first thing worth checking against
/// a real board, and there is nowhere else to read it from.
pub(super) fn honour_for_bind(
    kind: u32,
    obj: &super::super::nouveau_uapi::NouveauGemObject,
) -> bool {
    if !enabled() || !kind_is_compressible(kind) {
        return false;
    }
    let Some(caps) = *CAPS.lock() else {
        return false;
    };
    let vram = obj.domain & super::super::nouveau_uapi::NOUVEAU_GEM_DOMAIN_VRAM != 0;
    if !may_honour_compressed_kind(caps, vram, obj.size) {
        return false;
    }
    let (vram_offset, handle) = (obj.vram_offset, obj.handle);
    // The line the RM will put in the PTEs, for the one place a human can
    // check it. `None` here does not block the bind: the RM derives the line
    // from the PTE's own address, and this is only our reading of it.
    match vram_offset.and_then(comptag_line_for_pa) {
        Some(line) => crate::klog_info!(
            "[nouveau-uapi] VM_BIND handle={handle}: honouring compressed kind \
             {kind:#04x}, comptag line {line} (FBMEM offset {:#x})",
            vram_offset.unwrap_or(0)
        ),
        None => crate::klog_info!(
            "[nouveau-uapi] VM_BIND handle={handle}: honouring compressed kind \
             {kind:#04x}, comptag line not readable from here (FBMEM offset \
             {:?})",
            vram_offset
        ),
    }
    true
}

/// The Turing COMPRESSIBLE kind range, `0x08..=0x0F`
/// (`FB_IS_KIND_COMPRESSIBLE` in the RM; `0x0f` is one of the table's invalid
/// entries but is in the range the RM tests).
pub(super) const fn kind_is_compressible(kind: u32) -> bool {
    matches!(kind, 0x08..=0x0F)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plausible VRAM physical address on a 2060 SUPER: a little way into an
    /// 8 GiB framebuffer.
    const PA: u64 = 1 << 30;
    const BIG: u64 = 1 << COMPTAG_PAGE_SHIFT;

    #[test]
    fn the_line_is_the_address_field_plus_one() {
        // Spelled out rather than rebuilt from the constants, so a change to
        // COMPTAG_PAGE_SHIFT has to come past this test: 1 GiB >> 16 is
        // 0x4000, and the derivation adds one.
        assert_eq!(comptag_line_for_pa(PA), Some(0x4001));
    }

    #[test]
    fn address_zero_gets_line_one_because_line_zero_means_uncompressed() {
        assert_eq!(
            comptag_line_for_pa(0),
            Some(1),
            "the `1 +` exists so that no surface is handed line 0",
        );
    }

    /// Every address inside one 64 KiB comptag page shares its line, and the
    /// next byte starts the next. This is the property that makes the 64 KiB
    /// page requirement a consequence rather than a rule to remember.
    #[test]
    fn one_line_covers_exactly_sixty_four_kibibytes() {
        let base = comptag_line_for_pa(PA).unwrap();
        assert_eq!(comptag_line_for_pa(PA + 1), Some(base));
        assert_eq!(comptag_line_for_pa(PA + BIG - 1), Some(base));
        assert_eq!(comptag_line_for_pa(PA + BIG), Some(base + 1));
    }

    /// The refusal that matters: bits above 35 are dropped by the field, so
    /// without this check two surfaces 64 GiB apart would be told to use the
    /// same compression state.
    #[test]
    fn an_address_past_the_field_is_refused_not_truncated() {
        assert_eq!(
            comptag_line_for_pa(PA + (1u64 << (SPA_HI_BIT + 1))),
            None,
            "an address past the field must be refused, never truncated into \
             another surface's line",
        );
    }

    #[test]
    fn the_last_address_the_field_can_describe_still_works() {
        // One comptag page below the end of the representable range: field
        // value 0xF_FFFE, line 0xF_FFFF, the widest the PTE holds.
        let pa = ((MAX_PTE_COMPTAG_LINE as u64 - 1) << COMPTAG_PAGE_SHIFT) + 0x1234;
        assert_eq!(comptag_line_for_pa(pa), Some(MAX_PTE_COMPTAG_LINE));
    }

    /// And the one past it, which is the `+ 1` boundary. Unreachable on real
    /// hardware; it is here because the two fields being the same width is
    /// exactly the kind of thing that reads as safe and is not.
    #[test]
    fn the_top_comptag_page_does_not_fit_the_pte_field() {
        assert_eq!(
            comptag_line_for_pa((MAX_PTE_COMPTAG_LINE as u64) << COMPTAG_PAGE_SHIFT),
            None,
        );
        assert_eq!(comptag_line_for_pa(u64::MAX), None, "and nothing above it");
    }

    /// A GPU in one-to-one mode with every precondition met. Every other caps
    /// test takes this and spoils one thing, so that each asserts one reason.
    fn caps_ok() -> ComptagCaps {
        ComptagCaps {
            one_to_one: true,
            one_to_four: false,
            raw_mode: false,
            compbit_backing_off: false,
            post_l2_off: false,
            scrub_on_free: true,
            vas_for_ce: true,
            compr_page_shift: COMPTAG_PAGE_SHIFT,
        }
    }

    #[test]
    fn one_to_one_with_its_preconditions_may_compress() {
        assert!(may_honour_compressed_kind(caps_ok(), true, BIG));
    }

    #[test]
    fn sysmem_may_never_compress_however_good_the_caps() {
        assert!(
            !may_honour_compressed_kind(caps_ok(), false, BIG),
            "a sysmem page has no line in the L2 tag store",
        );
    }

    #[test]
    fn a_page_smaller_than_a_comptag_line_may_not_compress() {
        assert!(!may_honour_compressed_kind(caps_ok(), true, 4096));
        // The boundary, both sides.
        assert!(!may_honour_compressed_kind(caps_ok(), true, BIG - 1));
        assert!(may_honour_compressed_kind(caps_ok(), true, BIG));
    }

    #[test]
    fn scrub_on_free_is_required_in_both_modes() {
        let mut caps = caps_ok();
        caps.scrub_on_free = false;
        assert!(!may_honour_compressed_kind(caps, true, BIG));
        caps.raw_mode = true;
        assert!(
            !may_honour_compressed_kind(caps, true, BIG),
            "raw mode does not excuse a stale tag line",
        );
    }

    #[test]
    fn disabled_compbit_backing_refuses_in_both_modes() {
        let mut caps = caps_ok();
        caps.compbit_backing_off = true;
        assert!(!may_honour_compressed_kind(caps, true, BIG));
        caps.raw_mode = true;
        assert!(
            !may_honour_compressed_kind(caps, true, BIG),
            "with no backing store there is nowhere for the tag bits to live",
        );
    }

    /// The third mode. This is the test that exists because the obvious shape
    /// of this function -- "raw, else one-to-one" -- is wrong on a board that
    /// reports one-to-four: it is neither, and must be refused by name rather
    /// than by falling off the end of the two branches.
    #[test]
    fn the_one_to_four_mode_is_refused_even_alongside_another_mode() {
        let mut caps = caps_ok();
        caps.one_to_four = true;
        assert!(
            !may_honour_compressed_kind(caps, true, BIG),
            "in one-to-four the line is not 1 + pa[35:16]",
        );
        // And it wins over both of the modes this module does understand,
        // because a board reporting two modes is a board this module has not
        // read the HAL for.
        caps.raw_mode = true;
        assert!(!may_honour_compressed_kind(caps, true, BIG));
    }

    /// The asymmetry between the two modes, which is why this is a function
    /// and not one `&&`: the RM asserts VAS-for-CE in the one-to-one branch
    /// only.
    #[test]
    fn vas_for_ce_gates_one_to_one_but_not_raw_mode() {
        let mut caps = caps_ok();
        caps.vas_for_ce = false;
        assert!(
            !may_honour_compressed_kind(caps, true, BIG),
            "one-to-one without VAS-for-CE is the state the RM refuses",
        );
        caps.raw_mode = true;
        assert!(
            may_honour_compressed_kind(caps, true, BIG),
            "raw mode has no such precondition",
        );
    }

    /// The shift is checked, not assumed: the derivation is built on it.
    #[test]
    fn a_gpu_reporting_another_compression_page_shift_is_refused() {
        let mut caps = caps_ok();
        caps.compr_page_shift = 17;
        assert!(!may_honour_compressed_kind(caps, true, BIG));
        caps.compr_page_shift = 0;
        assert!(
            !may_honour_compressed_kind(caps, true, BIG),
            "zero is what the caps struct defaults to when the query failed",
        );
    }

    #[test]
    fn a_gpu_in_no_known_mode_may_not_compress() {
        let mut caps = caps_ok();
        caps.one_to_one = false;
        assert!(!may_honour_compressed_kind(caps, true, BIG));
        assert!(
            !may_honour_compressed_kind(ComptagCaps::default(), true, BIG),
            "and the default -- which is also what a failed query leaves -- is \
             the safe one",
        );
    }

    #[test]
    fn only_the_compressible_kind_range_is_compressible() {
        for kind in 0x00u32..=0x07 {
            assert!(!kind_is_compressible(kind), "{:#04x} is uncompressed", kind);
        }
        for kind in 0x08u32..=0x0F {
            assert!(kind_is_compressible(kind), "{:#04x} is compressible", kind);
        }
        assert!(!kind_is_compressible(0x10), "past the table");
        assert!(!kind_is_compressible(u32::MAX));
    }

    /// The hatch and the caps are AND-ed, and either alone must not be enough.
    /// Run serially with the flag put back, like the flag test below.
    /// A VRAM object big enough to carry its own comptag line.
    fn vram_obj() -> super::super::super::nouveau_uapi::NouveauGemObject {
        super::super::super::nouveau_uapi::NouveauGemObject {
            handle: 1,
            h_memory: 2,
            owner_pid: 3,
            owner_ctx: 0,
            size: BIG,
            phys_addr: None,
            vram_offset: Some(PA),
            domain: super::super::super::nouveau_uapi::NOUVEAU_GEM_DOMAIN_VRAM,
            valid_domains: super::super::super::nouveau_uapi::NOUVEAU_GEM_DOMAIN_VRAM,
            tile_mode: 0,
            tile_flags: 0,
        }
    }

    #[test]
    fn a_bind_is_refused_without_both_the_hatch_and_the_caps() {
        let _g = crate::test_globals::LOCK.lock();
        let obj = vram_obj();
        set_caps(None);
        set_enabled(false);
        assert!(!honour_for_bind(0x08, &obj), "hatch off, no caps");
        set_enabled(true);
        assert!(
            !honour_for_bind(0x08, &obj),
            "hatch on but the caps query never succeeded -- the no-GPU case",
        );
        set_caps(Some(caps_ok()));
        assert!(
            !honour_for_bind(0x06, &obj),
            "an uncompressed kind is never 'honoured'",
        );
        assert!(
            honour_for_bind(0x08, &obj),
            "hatch on, caps good, kind compressible",
        );
        set_enabled(false);
        assert!(
            !honour_for_bind(0x08, &obj),
            "and the hatch alone takes it back off",
        );
        set_caps(None);
    }

    /// A sysmem object with the same everything else: the decision is the
    /// object's, not the hatch's.
    #[test]
    fn a_sysmem_object_is_refused_with_the_hatch_on_and_the_caps_good() {
        let _g = crate::test_globals::LOCK.lock();
        let mut obj = vram_obj();
        obj.domain = super::super::super::nouveau_uapi::NOUVEAU_GEM_DOMAIN_GART;
        set_enabled(true);
        set_caps(Some(caps_ok()));
        assert!(!honour_for_bind(0x08, &obj));
        set_enabled(false);
        set_caps(None);
    }

    /// The query is claimed exactly once. It matters in the failing case: with
    /// no GPU the caps read never succeeds, so without this every single
    /// `VM_BIND` would take the RM lock to be told so again.
    ///
    /// `CAPS_ASKED` is a one-shot static shared by the whole test binary;
    /// `reset_caps_asked` puts it back, here and in `comptag_caps`'s tests,
    /// under the one lock, so the order the tests run in does not matter.
    #[test]
    fn the_caps_query_is_claimed_once_and_never_again() {
        let _serialised = crate::test_globals::LOCK.lock();
        reset_caps_asked();
        assert!(
            claim_caps_query(),
            "the first bind that needs the caps must be allowed to ask",
        );
        assert!(!claim_caps_query(), "no second bind may re-enter the RM");
        assert!(!claim_caps_query());
        reset_caps_asked();
    }

    #[test]
    fn the_flag_is_off_until_the_cmdline_turns_it_on() {
        let _g = crate::test_globals::LOCK.lock();
        // `ENABLED` is a static shared by the whole test binary, so this
        // asserts the initial value and puts it back rather than leaving the
        // flag on for whatever runs next.
        assert!(!enabled(), "nvidia.gem_comptags must default to off");
        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        assert!(!enabled());
    }

    /// The line is only our reading of what the RM will put in the PTEs, so
    /// an object whose line cannot be derived here -- no FBMEM offset
    /// recorded, or one past the address field -- is honoured all the same,
    /// with a log line that says the line was not readable instead of a
    /// number.
    #[test]
    fn a_bind_is_honoured_even_when_the_line_cannot_be_read_here() {
        let _g = crate::test_globals::LOCK.lock();
        let said = || -> alloc::vec::Vec<alloc::string::String> {
            crate::utils::host_hooks::take_dmesg()
                .into_iter()
                .map(|(_, l)| l)
                .collect()
        };
        set_caps(Some(caps_ok()));
        set_enabled(true);
        let _ = said();
        for (offset, why) in [
            (None, "no FBMEM offset"),
            (Some(u64::MAX), "past the field"),
        ] {
            let mut obj = vram_obj();
            obj.vram_offset = offset;
            assert!(honour_for_bind(0x08, &obj), "{}", why);
            let lines = said();
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("comptag line not readable from here")),
                "{}: {:?}",
                why,
                lines
            );
        }
        set_enabled(false);
        set_caps(None);
    }
}
