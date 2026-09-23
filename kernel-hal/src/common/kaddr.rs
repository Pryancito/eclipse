//! Where the kernel image is, and whether a machine word points into it.
//!
//! Every crash probe in this kernel asks the same two questions of a word it
//! found on a stack or in a register — is this a kernel pointer, and is it a
//! pointer into kernel `.text`? The answers decide whether a backtrace keeps
//! walking, whether a zeroed stack slot is reported as corruption, whether a
//! registered callback is still safe to call, and whether a fault is repaired
//! by resuming at an address the stack claims to hold.
//!
//! On x86_64 they were asked with a hand-written literal each time, sixteen of
//! them across three crates, and the literals do not agree: the kernel half
//! ends at sixteen megabytes, or at four gigabytes, depending on which probe
//! is asking. None of them was ever measured, because the files they live in
//! are compiled by bare x86_64 builds only — not the test binaries, not libos,
//! not the other two architectures.
//!
//! The linker knows. `zCore/src/platform/x86/linker.ld` exports `stext` and
//! `etext` around the image's own `.text`, which the aarch64 and riscv64 ports
//! already read to build their kernel page tables — x86_64 was the one
//! architecture guessing. [`set_kernel_text`] installs the real pair at boot,
//! and until it does the old literal window stands, so a probe that runs
//! before it is no worse off than it was.

use core::sync::atomic::{AtomicU64, Ordering};

/// Base of the kernel half: `KERNEL_BEGIN` in the linker script, and the
/// bottom of the physmap window the coroutine stacks live in.
pub const KERNEL_LO: u64 = 0xffff_ff00_0000_0000;
/// Four gigabytes up: the widest of the windows the probes used, and the one
/// that has to hold, since it is the stacks and the physmap it bounds.
pub const KERNEL_HI: u64 = 0xffff_ff01_0000_0000;

/// The literal `.text` window the x86_64 probes carried before the linker's
/// symbols were read. Kept as the answer until [`set_kernel_text`] runs.
///
/// Its low bound is 64 KiB above the image base, and the linker script puts
/// `stext` *at* the image base — so it excluded the first sixty-four kilobytes
/// of the real `.text`. A return address into a function laid out there did
/// not look like a return address, and the repair that depends on recognising
/// one declined it.
pub const FALLBACK_TEXT: (u64, u64) = (KERNEL_LO + 0x1_0000, KERNEL_LO + 0x100_0000);

/// Whether `[lo, hi)` could be this kernel's `.text`.
///
/// A linker symbol that reads back as zero — a build that did not export it, a
/// relocation that did not happen — must not narrow every probe in the kernel
/// to nothing, so an implausible pair is refused and the literal window stands.
pub fn plausible_text_range(lo: u64, hi: u64) -> bool {
    lo >= KERNEL_LO && hi > lo && hi <= KERNEL_HI
}

/// A `.text` window published once by the boot path and read by every probe
/// afterwards, on whatever CPU the fault lands.
pub struct TextRange {
    lo: AtomicU64,
    hi: AtomicU64,
}

impl Default for TextRange {
    fn default() -> Self {
        Self::new()
    }
}

impl TextRange {
    pub const fn new() -> Self {
        Self {
            lo: AtomicU64::new(0),
            hi: AtomicU64::new(0),
        }
    }

    /// Install a measured pair. Returns whether it was taken.
    pub fn set(&self, lo: u64, hi: u64) -> bool {
        if !plausible_text_range(lo, hi) {
            return false;
        }
        self.lo.store(lo, Ordering::Relaxed);
        self.hi.store(hi, Ordering::Release);
        true
    }

    /// `[start, end)` of `.text`, or the literal fallback while none is set.
    ///
    /// `hi` is the published half of the pair, so it is read first and with
    /// `Acquire`: a reader that sees the new `hi` sees the `lo` stored before
    /// it. A reader that sees the old `hi` — zero — gets the fallback, which
    /// is the window it would have used anyway.
    pub fn get(&self) -> (u64, u64) {
        let hi = self.hi.load(Ordering::Acquire);
        let lo = self.lo.load(Ordering::Relaxed);
        if plausible_text_range(lo, hi) {
            (lo, hi)
        } else {
            FALLBACK_TEXT
        }
    }
}

static KERNEL_TEXT: TextRange = TextRange::new();

/// Install the image's real `.text` bounds, from the linker's own symbols.
/// Returns whether they were taken.
pub fn set_kernel_text(lo: u64, hi: u64) -> bool {
    KERNEL_TEXT.set(lo, hi)
}

/// `[start, end)` of the kernel image's `.text`.
pub fn kernel_text() -> (u64, u64) {
    KERNEL_TEXT.get()
}

/// Whether `a` falls in a `.text` window.
pub fn in_text(text: (u64, u64), a: u64) -> bool {
    (text.0..text.1).contains(&a)
}

/// Whether `a` points into the kernel image's `.text`.
pub fn is_kernel_text(a: u64) -> bool {
    in_text(kernel_text(), a)
}

/// Whether `a` points anywhere into the kernel half — the image, the physmap,
/// or a coroutine stack carved out of it.
pub fn is_kernel_addr(a: u64) -> bool {
    (KERNEL_LO..KERNEL_HI).contains(&a)
}

/// Whether `a` is a plausible kernel-stack pointer: in the kernel half and
/// eight-aligned, which is what makes it safe to read a qword from.
pub fn is_kernel_stack_qword(a: u64) -> bool {
    is_kernel_addr(a) && a.is_multiple_of(8)
}

/// Whether `a` has the shape of an `RFLAGS` value rather than a pointer.
///
/// Only ever a question about a word whose top half is *already* gone: RF is
/// bit 16, so a live `RFLAGS` lands in the same low window as `.text`, and bit
/// 1 is the reserved bit that is always set. A word with kernel high bits
/// cannot be one of these, which is why asking this alongside a high-bits test
/// answers nothing — it is the companion "is this a kernel pointer that lost
/// its top half" question that needs it.
///
/// The top half needs no test of its own: nothing above bit 21 may be set, so
/// nothing above bit 31 can be either.
pub fn looks_like_rflags(a: u64) -> bool {
    (a & 0x2) != 0 && (a & !0x3f_ffff) == 0
}

/// Whether `a` is a `.text` address that has lost its top half — the signature
/// of a word half-overwritten rather than replaced.
pub fn truncated_text(text: (u64, u64), a: u64) -> bool {
    (a >> 32) == 0 && !looks_like_rflags(a) && in_text(text, KERNEL_LO + a)
}

/// [`truncated_text`] against the installed window.
pub fn looks_truncated_text(a: u64) -> bool {
    truncated_text(kernel_text(), a)
}

/// A kernel code pointer whose *top byte* was overwritten, and what it was.
///
/// The `ret` corruption this kernel is hunted for lands a saved
/// `0xffff_ff00_00xx_xxxx` on a stack and scribbles the top byte, leaving a
/// non-canonical target: the transfer raises `#GP` with the mangled address in
/// `RIP`. Putting the top byte back is only safe when the result is an address
/// this kernel actually executes from, so the repaired value — not the mangled
/// one — is what gets measured against `.text`. That is the whole test: a
/// plausible window lies inside the kernel half, which pins every bit above
/// the low half, and the repair rewrites only the top byte — so an address
/// whose middle is not a kernel pointer cannot land inside it. A genuine `#GP`
/// on a user address is refused by the same line that bounds the repair.
pub fn unmangle_kernel_text(text: (u64, u64), a: u64) -> Option<u64> {
    let fixed = a | (KERNEL_LO & 0xff00_0000_0000_0000);
    (fixed != a && in_text(text, fixed)).then_some(fixed)
}

/// Whether the bytes ending at a return address are a `CALL`.
///
/// `tail` is up to eight bytes, the last of which is the byte immediately
/// before the return address. This is what separates the two faults the
/// null-execute repair is invoked for, and only one is repairable: a `call`
/// through a corrupt function pointer pushed a true return address, while a
/// `ret` that popped garbage left a stack qword that merely *ranges* like
/// `.text`. Resuming at the second executes from a byte offset the compiler
/// never emitted.
///
/// A false negative leaves the repair declined and the fault reported, which
/// is the safe direction; a false positive needs the exact bytes of a `CALL`
/// immediately before an address the corrupted stack happens to name.
pub fn ends_with_call(tail: &[u8]) -> bool {
    for n in 2..=tail.len() {
        let s = &tail[tail.len() - n..];
        // call rel32: E8 xx xx xx xx
        if n == 5 && s[0] == 0xe8 {
            return true;
        }
        // Indirect near call: FF with ModRM reg-field /2. A REX prefix needs
        // no arm of its own — the CALL it prefixes is a whole instruction
        // ending at the same byte, so the iteration at `n - 1` finds it.
        if s[0] != 0xff {
            continue;
        }
        let modrm = s[1];
        if (modrm >> 3) & 7 != 2 {
            continue; // not /2 = CALL
        }
        let mode = modrm >> 6;
        let rm = modrm & 7;
        let sib = mode != 3 && rm == 4;
        // A SIB whose base field is 5 has no base register under mod=00: a
        // disp32 follows, whatever the mode byte alone suggests. Reading the
        // SIB is the only way to know — measuring the instruction without it
        // makes `call [rax*8 + d32]` three bytes long instead of seven, so a
        // real call site is not recognised and a repairable fault is reported
        // as fatal.
        let no_base = sib && n > 2 && s[2] & 7 == 5;
        let disp: usize = match mode {
            0 if rm == 5 => 4, // RIP-relative disp32
            0 if no_base => 4,
            0 => 0,
            1 => 1,
            2 => 4,
            _ => 0, // register-direct
        };
        if 2 + sib as usize + disp == n {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: (u64, u64) = (KERNEL_LO, KERNEL_LO + 0x100_0000);

    // ── the window itself ───────────────────────────────────────────────────

    #[test]
    fn a_text_range_below_the_kernel_half_is_refused() {
        assert!(!plausible_text_range(0, 0x100_0000));
        assert!(!plausible_text_range(KERNEL_LO - 1, KERNEL_LO + 0x1000));
    }

    #[test]
    fn an_empty_or_inverted_text_range_is_refused() {
        assert!(!plausible_text_range(KERNEL_LO, KERNEL_LO));
        assert!(!plausible_text_range(KERNEL_LO + 0x1000, KERNEL_LO));
    }

    #[test]
    fn a_text_range_running_past_the_kernel_half_is_refused() {
        assert!(!plausible_text_range(KERNEL_LO, KERNEL_HI + 1));
        assert!(plausible_text_range(KERNEL_LO, KERNEL_HI));
    }

    #[test]
    fn an_unset_range_answers_the_literal_window() {
        assert_eq!(TextRange::new().get(), FALLBACK_TEXT);
    }

    #[test]
    fn a_measured_range_replaces_the_literal_window() {
        let r = TextRange::new();
        assert!(r.set(KERNEL_LO, KERNEL_LO + 0x20_0000));
        assert_eq!(r.get(), (KERNEL_LO, KERNEL_LO + 0x20_0000));
    }

    #[test]
    fn a_linker_symbol_that_reads_back_as_zero_leaves_the_window_alone() {
        // Refusing is the whole point: taking `(0, 0)` would narrow every
        // probe in the kernel to nothing, and every crash report with it.
        let r = TextRange::new();
        assert!(!r.set(0, 0));
        assert_eq!(r.get(), FALLBACK_TEXT);
    }

    #[test]
    fn an_implausible_pair_does_not_erase_a_measured_one() {
        let r = TextRange::new();
        assert!(r.set(KERNEL_LO, KERNEL_LO + 0x20_0000));
        assert!(!r.set(0, 0));
        assert_eq!(r.get(), (KERNEL_LO, KERNEL_LO + 0x20_0000));
    }

    #[test]
    fn the_literal_window_excluded_the_first_64k_of_text() {
        // `zCore/src/platform/x86/linker.ld` opens the section with
        // `. = KERNEL_BEGIN; stext = .;`, so `.text` starts *at* the image
        // base and the literal low bound of +64 KiB cut off its first page.
        let early = KERNEL_LO + 0x40;
        assert!(!in_text(FALLBACK_TEXT, early));
        let r = TextRange::new();
        assert!(r.set(KERNEL_LO, KERNEL_LO + 0x100_0000));
        assert!(in_text(r.get(), early));
    }

    #[test]
    fn the_installed_window_is_one_every_probe_can_use() {
        // The global is shared by the whole test binary, so this asserts the
        // shape the probes depend on, never a particular pair.
        let (lo, hi) = kernel_text();
        assert!(plausible_text_range(lo, hi));
        assert!(is_kernel_addr(lo));
        assert!(is_kernel_text(lo));
        assert!(!is_kernel_text(hi));
    }

    // ── the range predicates ────────────────────────────────────────────────

    #[test]
    fn text_is_half_open() {
        assert!(in_text(TEXT, TEXT.0));
        assert!(in_text(TEXT, TEXT.1 - 1));
        assert!(!in_text(TEXT, TEXT.1));
        assert!(!in_text(TEXT, TEXT.0 - 1));
    }

    #[test]
    fn the_kernel_half_is_half_open() {
        assert!(is_kernel_addr(KERNEL_LO));
        assert!(is_kernel_addr(KERNEL_HI - 1));
        assert!(!is_kernel_addr(KERNEL_HI));
        assert!(!is_kernel_addr(KERNEL_LO - 1));
        assert!(!is_kernel_addr(0));
    }

    #[test]
    fn a_stack_qword_must_be_eight_aligned() {
        // Every probe that reads one of these does it with `read_volatile` on
        // a raw pointer, so the alignment is not a nicety.
        assert!(is_kernel_stack_qword(KERNEL_LO + 0x1000));
        for off in 1..8 {
            assert!(
                !is_kernel_stack_qword(KERNEL_LO + 0x1000 + off),
                "offset {} accepted",
                off
            );
        }
    }

    #[test]
    fn a_user_address_is_never_a_stack_qword() {
        assert!(!is_kernel_stack_qword(0));
        assert!(!is_kernel_stack_qword(0x1000));
        assert!(!is_kernel_stack_qword(0x7fff_ffff_f000));
    }

    // ── the residue predicates ──────────────────────────────────────────────

    #[test]
    fn a_live_rflags_with_rf_set_is_not_a_pointer() {
        // RF is bit 16, so a saved RFLAGS lands squarely in the same low
        // window `.text` offsets do. Bit 1 is the reserved bit, always set.
        assert!(looks_like_rflags(0x1_0002));
        assert!(looks_like_rflags(0x2));
        assert!(looks_like_rflags(0x3f_ffff));
    }

    #[test]
    fn rflags_needs_the_reserved_bit() {
        assert!(!looks_like_rflags(0x1_0000));
        assert!(!looks_like_rflags(0));
    }

    #[test]
    fn a_word_with_bits_above_the_flags_register_is_not_rflags() {
        assert!(!looks_like_rflags(0x40_0002));
        assert!(!looks_like_rflags(0x1_0000_0002));
    }

    #[test]
    fn a_word_that_kept_its_kernel_high_bits_is_never_rflags() {
        // Which is why the guard is dead weight next to a high-bits test, and
        // load-bearing only in the companion "lost its top half" question.
        assert!(!looks_like_rflags(KERNEL_LO + 0x1_0002));
        assert!(!looks_like_rflags(KERNEL_LO));
    }

    #[test]
    fn a_text_low_half_on_its_own_is_truncated_residue() {
        assert!(truncated_text(TEXT, 0x7_3bf0));
        assert!(!truncated_text(TEXT, KERNEL_LO + 0x7_3bf0));
    }

    #[test]
    fn a_low_half_above_the_flags_register_is_residue_whatever_its_bits() {
        // The RFLAGS exemption only reaches 0x3f_ffff, so most of `.text` is
        // unambiguous even with bit 1 set — which is the only reason the
        // exemption is affordable at all.
        assert!(truncated_text(TEXT, 0x40_0002));
        assert!(!truncated_text(TEXT, 0x1_0002));
    }

    #[test]
    fn an_rflags_shaped_word_is_not_truncated_residue() {
        // 0x1_0002 ranges like a `.text` offset, and calling it residue is
        // what arms the sticky heap-smash flag and halts the timer path.
        assert!(in_text(TEXT, KERNEL_LO + 0x1_0002));
        assert!(!truncated_text(TEXT, 0x1_0002));
    }

    #[test]
    fn residue_is_measured_against_the_installed_window() {
        let early = 0x40;
        assert!(!truncated_text(FALLBACK_TEXT, early));
        assert!(truncated_text(TEXT, early));
    }

    // ── the #GP top-byte repair ─────────────────────────────────────────────

    #[test]
    fn a_kernel_pointer_with_a_scribbled_top_byte_is_repaired() {
        let good = KERNEL_LO + 0x7_3bf2;
        for top in [0x00u64, 0x01, 0x0a, 0x7f] {
            let mangled = (good & 0x00ff_ffff_ffff_ffff) | (top << 56);
            assert_eq!(
                unmangle_kernel_text(TEXT, mangled),
                Some(good),
                "top byte {:#04x}",
                top
            );
        }
    }

    #[test]
    fn an_intact_kernel_pointer_needs_no_repair() {
        assert_eq!(unmangle_kernel_text(TEXT, KERNEL_LO + 0x7_3bf2), None);
    }

    #[test]
    fn an_address_whose_middle_is_not_a_kernel_pointer_is_not_repaired() {
        // Otherwise a genuine #GP on a user address gets "repaired" into a
        // jump into the kernel.
        assert_eq!(unmangle_kernel_text(TEXT, 0x7_3bf2), None);
        assert_eq!(unmangle_kernel_text(TEXT, 0x0000_1234_0007_3bf2), None);
    }

    #[test]
    fn a_mangled_pointer_landing_outside_text_is_not_repaired() {
        let stack = KERNEL_LO + 0x8000_0000;
        let mangled = (stack & 0x00ff_ffff_ffff_ffff) | (0x01 << 56);
        assert!(is_kernel_addr(stack));
        assert_eq!(unmangle_kernel_text(TEXT, mangled), None);
    }

    // ── the backwards CALL decoder ──────────────────────────────────────────

    #[test]
    fn a_direct_call_rel32_ends_with_a_call() {
        assert!(ends_with_call(&[
            0x90, 0x90, 0x90, 0xe8, 0x11, 0x22, 0x33, 0x44
        ]));
    }

    #[test]
    fn a_call_rel32_that_does_not_end_at_the_return_address_is_not_one() {
        // A return address is by definition preceded by the *whole* CALL, so
        // an E8 four bytes back is some other instruction's operand byte.
        assert!(!ends_with_call(&[0xe8, 0x11, 0x22, 0x33]));
    }

    #[test]
    fn a_register_indirect_call_ends_with_a_call() {
        assert!(ends_with_call(&[0x90, 0xff, 0xd0]));
    }

    #[test]
    fn a_call_through_rsp_carries_no_sib_byte() {
        // `call rsp` is FF D4: rm=100 names a SIB byte in every mode but
        // register-direct, where it names RSP itself and the instruction is
        // two bytes. Measuring a SIB into it puts the CALL one byte earlier
        // than it is.
        assert!(ends_with_call(&[0x90, 0xff, 0xd4]));
        assert!(ends_with_call(&[0x41, 0xff, 0xd4]));
    }

    #[test]
    fn a_rex_prefixed_indirect_call_ends_with_a_call() {
        assert!(ends_with_call(&[0x41, 0xff, 0xd0]));
    }

    #[test]
    fn a_rip_relative_call_ends_with_a_call() {
        // FF /2 with mod=00 rm=101: disp32 follows, six bytes in all.
        assert!(ends_with_call(&[0xff, 0x15, 0x11, 0x22, 0x33, 0x44]));
    }

    #[test]
    fn a_call_through_a_sib_with_a_base_register_ends_with_a_call() {
        // FF /2, mod=00 rm=100, SIB base=rax: three bytes, no displacement.
        assert!(ends_with_call(&[0x90, 0xff, 0x14, 0x18]));
    }

    #[test]
    fn a_call_through_a_scaled_index_with_no_base_register_ends_with_a_call() {
        // `call [rax*8 + disp32]` — FF /2, mod=00 rm=100, SIB base field 5.
        // Mod alone says "no displacement", and measuring it that way makes
        // the instruction three bytes instead of seven: the CALL is not
        // recognised, and a repairable fault is reported as fatal.
        assert!(ends_with_call(&[0xff, 0x14, 0xc5, 0x44, 0x33, 0x22, 0x11]));
    }

    #[test]
    fn a_sib_base_of_five_under_mod_01_is_a_base_register() {
        // Same base field, and here it *is* rbp with a disp8 — four bytes.
        // The no-base rule belongs to mod=00 alone.
        assert!(ends_with_call(&[0x90, 0xff, 0x54, 0x25, 0x10]));
    }

    #[test]
    fn a_call_with_a_disp8_ends_with_a_call() {
        assert!(ends_with_call(&[0x90, 0xff, 0x50, 0x10]));
    }

    #[test]
    fn a_call_with_a_disp32_ends_with_a_call() {
        assert!(ends_with_call(&[0xff, 0x90, 0x11, 0x22, 0x33, 0x44]));
    }

    #[test]
    fn an_indirect_jmp_is_not_a_call() {
        // FF /4 is JMP, FF /6 is PUSH: same opcode byte, and resuming at the
        // address after one of them skips an instruction that never pushed.
        assert!(!ends_with_call(&[0x90, 0xff, 0xe0]));
        assert!(!ends_with_call(&[0x90, 0xff, 0xf0]));
    }

    #[test]
    fn an_indirect_call_that_does_not_end_at_the_return_address_is_not_one() {
        assert!(!ends_with_call(&[0xff, 0xd0, 0x90]));
    }

    #[test]
    fn plain_instruction_bytes_are_not_a_call_at_any_length() {
        assert!(!ends_with_call(&[0x90; 8]));
        assert!(!ends_with_call(&[
            0x48, 0x89, 0xe5, 0x5d, 0xc3, 0x0f, 0x1f, 0x00
        ]));
    }

    #[test]
    fn a_tail_too_short_to_hold_a_call_is_not_one() {
        assert!(!ends_with_call(&[]));
        assert!(!ends_with_call(&[0xff]));
    }
}
