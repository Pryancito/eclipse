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

/// Hand the window to `lock::fn_slot`, which judges every function-pointer
/// hook slot in the kernel -- klog's emit, the spin pump, the deadlock hooks
/// -- and cannot see this module. Published from here so there is one window
/// and not two to keep in step.
///
/// Indirected so a host test can watch it happen. The real one cannot be
/// driven from a test: the window there is process-wide and **one-way**
/// (`lock::fn_slot` refuses an implausible pair, so `(0, 0)` will not put it
/// back), and a published window turns every host function pointer into a
/// refused slot -- which is precisely what this crate's dmesg tests rely on
/// not happening (`drivers`' recording sink is reached only because nothing
/// publishes a window in a hosted build).
#[cfg(not(test))]
#[inline]
fn publish_fn_slot_window(lo: usize, hi: usize) {
    lock::fn_slot::set_text_range(lo, hi);
}

#[cfg(test)]
fn publish_fn_slot_window(lo: usize, hi: usize) {
    PUBLISHED.with(|c| c.set(Some((lo, hi))));
}

#[cfg(test)]
std::thread_local! {
    /// The pair [`publish_fn_slot_window`] was last handed **on this thread**.
    /// Per thread, like the rest of this crate's host seams, so a test reads
    /// only its own call and nothing else the test binary is running can move
    /// it.
    static PUBLISHED: core::cell::Cell<Option<(usize, usize)>> =
        const { core::cell::Cell::new(None) };
}

/// What this thread last published, or `None` since [`forget_published_window`].
#[cfg(test)]
fn published_window() -> Option<(usize, usize)> {
    PUBLISHED.with(|c| c.get())
}

/// Back to "this thread has published nothing".
#[cfg(test)]
fn forget_published_window() {
    PUBLISHED.with(|c| c.set(None));
}

/// Install the image's real `.text` bounds, from the linker's own symbols.
/// Returns whether they were taken.
pub fn set_kernel_text(lo: u64, hi: u64) -> bool {
    let taken = KERNEL_TEXT.set(lo, hi);
    if taken {
        publish_fn_slot_window(lo as usize, hi as usize);
    }
    taken
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

/// Ring-0 `RIP` that is not kernel `.text`: a `ret`/`call` landed on
/// userspace, a null, or other garbage. The CPU then takes an EXECUTE #PF
/// (or a `#UD`) at that address. Demand-paging it would map a user page and
/// run it in kernel mode.
pub fn kernel_exec_is_corrupt_control_flow(rip: u64) -> bool {
    !is_kernel_text(rip)
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

/// What a machine word found where a kernel code pointer belongs looks like.
///
/// `try_skip_null_execute_call` reads the qword at the faulting `RSP` and has
/// to decide whether it is a return address a `CALL` pushed. It reported that
/// word only in the two shapes it already recognised -- a zero, and a `.text`
/// address that had lost its top half -- and returned **in silence** for every
/// other one. A real capture then came back with
/// `[rsp0]=0x1cb0a4fb0e` and no classification at all: the one word that
/// names the writer class got no line in the report, so the fault was
/// contained with nothing to go on but the `vaddr`.
///
/// The shapes are what this kernel's residue actually looks like, and each
/// points somewhere different: a kernel-half word that is not `.text` is
/// physmap or stack residue (the soft smash this tree is hunting); a user-half
/// word is a userspace pointer that reached a kernel code slot; an `RFLAGS`
/// value is a trap frame read at the wrong offset; a small value is a length
/// or a count written one slot over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordShape {
    /// All zero: the slot was cleared.
    Zero,
    /// Has the shape of an `RFLAGS` value (see [`looks_like_rflags`]).
    Rflags,
    /// Non-zero but below the first page: a length, a count, an index.
    Small,
    /// A `.text` address with its top half gone (see [`truncated_text`]).
    TruncatedText,
    /// A genuine address inside the kernel image's `.text`.
    KernelText,
    /// In the kernel half but not `.text`: physmap, heap or stack residue.
    KernelNonText,
    /// Not canonical: no address at all, and no amount of it is.
    NonCanonical,
    /// A canonical user-half address.
    UserHalf,
}

impl WordShape {
    /// A phrase for the fault report, in the terms whoever reads it is
    /// debugging in.
    pub fn as_str(self) -> &'static str {
        match self {
            WordShape::Zero => "zero (the slot was cleared)",
            WordShape::Rflags => "an RFLAGS value (a trap frame read at the wrong offset)",
            WordShape::Small => "below the first page (a length, count or index)",
            WordShape::TruncatedText => ".text with its top half gone (soft smash)",
            WordShape::KernelText => "kernel .text",
            WordShape::KernelNonText => {
                "kernel-half but not .text (physmap, heap or stack residue)"
            }
            WordShape::NonCanonical => "not a canonical address",
            WordShape::UserHalf => "a user-half address in a kernel code slot",
        }
    }
}

/// Name the shape of `a`, measured against the `.text` window `text`.
///
/// Pure, and ordered from the most specific claim to the least: a word that
/// could be read two ways is reported as the reading that says the most about
/// where it came from.
pub fn classify_word(text: (u64, u64), a: u64) -> WordShape {
    if a == 0 {
        return WordShape::Zero;
    }
    if truncated_text(text, a) {
        return WordShape::TruncatedText;
    }
    if looks_like_rflags(a) {
        return WordShape::Rflags;
    }
    if a < 0x1000 {
        return WordShape::Small;
    }
    if in_text(text, a) {
        return WordShape::KernelText;
    }
    if is_kernel_addr(a) {
        return WordShape::KernelNonText;
    }
    // Canonical: the top seventeen bits all agree. A kernel-half address that
    // is outside this kernel's own window has already been answered above, so
    // what is left here is either nonsense or userspace.
    let top = a >> 47;
    if top != 0 && top != 0x1_ffff {
        return WordShape::NonCanonical;
    }
    WordShape::UserHalf
}

/// [`classify_word`] against the installed window.
pub fn word_shape(a: u64) -> WordShape {
    classify_word(kernel_text(), a)
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

    /// The live capture: kernel #PF EXECUTE at `rip=vaddr=0x1045f0000`
    /// (`[rsp]` also userspace-shaped). That RIP is not `.text`, so the
    /// skip-repair must run and the user VMAR must not demand-page it.
    #[test]
    fn executing_a_userspace_rip_from_the_kernel_is_corrupt_control_flow() {
        assert!(kernel_exec_is_corrupt_control_flow(0x1045f0000));
        assert!(kernel_exec_is_corrupt_control_flow(0x107e990f68));
        assert!(kernel_exec_is_corrupt_control_flow(0));
        assert!(kernel_exec_is_corrupt_control_flow(0x7fff_ffff_f000));
        let (lo, _) = kernel_text();
        assert!(!kernel_exec_is_corrupt_control_flow(lo));
        assert!(!kernel_exec_is_corrupt_control_flow(lo + 0xaa150));
    }

    // ── the window the hook slots are judged against ────────────────────────

    #[test]
    fn a_measured_window_reaches_the_hook_slots() {
        // `lock::fn_slot::live_fn` judges every function-pointer hook in this
        // kernel -- klog's emit, the spin pump, the deadlock hooks -- against
        // a window it keeps itself, and it cannot see the one this module
        // keeps. Handing it over here is the only thing that makes them one
        // window; nothing else in the tree calls `set_text_range`.
        forget_published_window();
        assert!(set_kernel_text(KERNEL_LO, KERNEL_LO + 0x20_0000));
        assert_eq!(
            published_window(),
            Some((KERNEL_LO as usize, (KERNEL_LO + 0x20_0000) as usize)),
            "la ventana llega a los huecos tal cual se midio"
        );
    }

    #[test]
    fn a_refused_window_is_not_published_to_the_hook_slots() {
        // A pair this module refuses must not reach them either. Taking a
        // bogus one there is worse than having none: with no window every
        // slot is `Unchecked` and still called, while a window that holds no
        // real function makes every live hook `Foreign` -- and a refused hook
        // is a call that silently stops happening, which is how a dead klog
        // or a deaf spin pump looks from the outside.
        forget_published_window();
        assert!(!set_kernel_text(0, 0));
        assert_eq!(published_window(), None, "no hay nada que publicar");
    }

    #[test]
    fn every_window_this_module_takes_is_one_the_hook_slots_also_take() {
        // Two filters, written in two crates, and neither can read the other.
        // Were this module's ever the looser of the pair, the hand-over would
        // fail with nobody looking at its answer and the kernel would run on
        // two different windows -- the state the hand-over exists to prevent.
        for (lo, hi) in [
            (KERNEL_LO, KERNEL_LO + 1),
            (KERNEL_LO, KERNEL_HI),
            (KERNEL_HI - 1, KERNEL_HI),
            FALLBACK_TEXT,
        ] {
            assert!(plausible_text_range(lo, hi));
            assert!(
                lock::fn_slot::plausible_range(lo as usize, hi as usize),
                "{:#x}..{:#x} lo toma este modulo y no los huecos",
                lo,
                hi
            );
        }
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
    fn the_reserved_bit_is_bit_one_and_nothing_else() {
        // Bit 1 is the one a live `RFLAGS` always has set. Bit 0 is CF, which
        // is set about half the time and says nothing about what the word is.
        // Asking for "either" instead of "bit 1" exempts every odd `.text`
        // offset below 4 MiB from the residue test -- and the first four
        // megabytes are where a `ret` into the middle of an instruction most
        // often lands, so the exemption would swallow the reports this is for.
        assert!(!looks_like_rflags(0x1));
        assert!(!looks_like_rflags(0x1_0001));
        assert!(!looks_like_rflags(0x3f_fffd));
        assert!(looks_like_rflags(0x3));
    }

    #[test]
    fn an_odd_text_offset_is_residue_and_not_rflags() {
        // The same distinction where it is spent: a `.text` offset with bit 0
        // set and bit 1 clear is residue, and calling it `RFLAGS` is a
        // corruption report that never gets made.
        assert!(!looks_like_rflags(0x7_3bf1));
        assert!(truncated_text(TEXT, 0x7_3bf1));
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

    #[test]
    fn residue_reaches_the_top_of_the_widest_window() {
        // The test for "lost its top half" is that the top half is *gone*,
        // not that the low half is small. A window may be as wide as the
        // kernel half, so a `.text` offset may carry any of the low 32 bits,
        // and a bound drawn anywhere below that quietly stops recognising
        // residue from the far end of the image -- the end a `.text` that has
        // grown adds, and the one no literal window here was ever measured
        // against.
        const WIDEST: (u64, u64) = (KERNEL_LO, KERNEL_HI);
        assert!(plausible_text_range(WIDEST.0, WIDEST.1));
        assert!(truncated_text(WIDEST, 0xffff_ffff));
        assert!(truncated_text(WIDEST, 0x00ff_ffff));
        // ...and one bit above the low half is a word that kept part of its
        // top half, which is not this shape at all.
        assert!(!truncated_text(WIDEST, 0x1_0000_0000));
    }

    // ── the windows the copies used ─────────────────────────────────────────
    //
    // Four places decided "is this word a kernel code pointer" by hand, each
    // with its own literal window and none of them the image's:
    //
    //   zCore/src/handler.rs, truncated-residue probe   +0x1_0000 .. +0x100_0000
    //   zCore/src/handler.rs, `[kchain]` stack scan     +0        .. +0x60_0000
    //   zCore/src/memory_x86_64.rs, `[leaktrace]`       +0x1000   .. +0x100_0000
    //   this module's own FALLBACK_TEXT                 +0x1_0000 .. +0x100_0000
    //
    // `.text` of the build those comments were written against ends at
    // `etext ~= 0x5b_bb27`, so every one of them is wrong in one direction or
    // the other. These tests pin the two ways they are wrong.

    /// `.text` of a real build: from the image base to a measured `etext`.
    const IMAGE: (u64, u64) = (KERNEL_LO, KERNEL_LO + 0x5b_bb27);

    #[test]
    fn a_word_past_etext_is_not_a_return_address_however_low_it_is() {
        // The residue probe accepted anything under 16 MiB, i.e. nearly three
        // times the image. And it does not merely print: inside a timer
        // callback that branch arms the sticky heap-smash flag and halts the
        // machine on purpose, so a word off the end of `.text` -- a small
        // scalar, a low heap offset, an index -- was a deliberate hang.
        let past_etext = 0x80_0000u64;
        assert!(
            (0x1_0000..0x100_0000).contains(&past_etext),
            "the old window took it"
        );
        assert!(!truncated_text(IMAGE, past_etext));
        assert!(!in_text(IMAGE, KERNEL_LO + past_etext));
    }

    #[test]
    fn the_first_page_of_text_is_text() {
        // Two of the copies started their window one page or 64 KiB above the
        // image base, and `.text` starts *at* it (`. = KERNEL_BEGIN; stext =
        // .;`), so a return into the entry code was not a return address.
        for a in [KERNEL_LO, KERNEL_LO + 0x40, KERNEL_LO + 0x800] {
            assert!(in_text(IMAGE, a), "{:#x} is inside the image", a);
            assert!(!in_text((KERNEL_LO + 0x1000, IMAGE.1), a));
            assert!(!in_text(FALLBACK_TEXT, a));
        }
    }

    #[test]
    fn a_window_pinned_to_one_build_loses_the_next_one() {
        // The `[kchain]` scan's upper bound was 6 MiB, chosen as "a
        // conservative upper bound" over that build's `etext` of 5.7 MiB. It
        // is conservative for exactly as long as the image stays smaller than
        // it, and the only symptom of growing past it is a scan that prints
        // nothing and says so in the same breath.
        const SIX_MIB: (u64, u64) = (KERNEL_LO, KERNEL_LO + 0x60_0000);
        const GROWN: (u64, u64) = (KERNEL_LO, KERNEL_LO + 0x70_0000);
        let ret = KERNEL_LO + 0x68_0000;
        assert!(in_text(GROWN, ret));
        assert!(!in_text(SIX_MIB, ret));
    }

    #[test]
    fn an_eight_aligned_address_past_the_kernel_half_is_not_a_stack_qword() {
        // The frame-pointer walks tested `rbp >= KERNEL_LO` and nothing else,
        // so a `saved_rbp` that had been scribbled into a huge value passed
        // the guard and the walk chased it. The bound is half-open at both
        // ends now.
        assert!(is_kernel_stack_qword(KERNEL_HI - 8));
        assert!(!is_kernel_stack_qword(KERNEL_HI));
        assert!(!is_kernel_stack_qword(KERNEL_HI + 0x1000));
        assert!(!is_kernel_stack_qword(u64::MAX & !7));
    }

    #[test]
    fn nothing_but_the_window_decides_whether_a_word_is_text() {
        // The property the four copies each broke: the answer is a function of
        // the installed pair and the address, and of nothing else -- no
        // literal floor, no literal ceiling, no rounding.
        for (lo, hi) in [
            (KERNEL_LO, KERNEL_LO + 0x1000),
            (KERNEL_LO + 0x1000, KERNEL_LO + 0x2_0000),
            IMAGE,
            FALLBACK_TEXT,
        ] {
            assert!(in_text((lo, hi), lo));
            assert!(in_text((lo, hi), hi - 1));
            assert!(!in_text((lo, hi), lo - 1));
            assert!(!in_text((lo, hi), hi));
            // ...and the residue question is the same one asked about a word
            // that has lost its top half, so it moves with the window too.
            assert_eq!(
                truncated_text((lo, hi), hi - 1 - KERNEL_LO),
                !looks_like_rflags(hi - 1 - KERNEL_LO)
            );
        }
    }

    // ── the shape of a word where a code pointer belongs ────────────────────

    #[test]
    fn every_shape_of_residue_this_kernel_has_actually_seen_is_named() {
        for (word, shape, what) in [
            (0u64, WordShape::Zero, "a cleared slot"),
            // The capture that prompted this: reported with no classification
            // at all, because it is none of the two shapes the old report knew.
            (
                0x1c_b0a4_fb0e,
                WordShape::UserHalf,
                "the [rsp0] of the timer-callback #PF",
            ),
            // ...and the fault target of that same capture.
            (
                0x1_076f_0000,
                WordShape::UserHalf,
                "a user-half fn-ptr target",
            ),
            (
                TEXT.0,
                WordShape::KernelText,
                "the first function of the image",
            ),
            (TEXT.1 - 1, WordShape::KernelText, "the last byte of .text"),
            (
                TEXT.0 + 0xa_0000 - KERNEL_LO,
                WordShape::TruncatedText,
                "a .text address that lost its top half",
            ),
            (
                KERNEL_HI - 8,
                WordShape::KernelNonText,
                "physmap or a coroutine stack",
            ),
            (0x10282, WordShape::Rflags, "a live RFLAGS"),
            (
                0x0001_0000_0000_0000,
                WordShape::NonCanonical,
                "no address at all",
            ),
        ] {
            assert_eq!(classify_word(TEXT, word), shape, "{}", what);
        }
    }

    #[test]
    fn a_truncated_text_word_in_the_first_page_is_not_reported_as_merely_small() {
        // `truncated_text` is the more specific claim and has to win: the first
        // functions of the image lose their top half into the same low window a
        // length or an index lives in, and calling one a length loses the whole
        // finding.
        let low = TEXT.0 + 0x40 - KERNEL_LO;
        assert!(
            low < 0x1000,
            "pick an offset that makes this a real question"
        );
        assert_eq!(classify_word(TEXT, low), WordShape::TruncatedText);
    }

    #[test]
    fn a_small_word_is_only_small_where_text_does_not_start_at_the_kernel_base() {
        // With `stext` AT `KERNEL_LO` -- which is what the linker script does
        // -- every low word that is not RFLAGS-shaped raises into `.text`, so
        // the louder reading wins and nothing is "just an index". It becomes a
        // question of its own only under a window that leaves the first pages
        // out, like the literal one the x86_64 probes carried before the
        // linker's symbols were read.
        assert_eq!(classify_word(TEXT, 0x20), WordShape::TruncatedText);
        assert_eq!(classify_word(FALLBACK_TEXT, 0x20), WordShape::Small);
    }

    #[test]
    fn an_rflags_shaped_low_word_is_rflags_and_not_truncated_text() {
        // `truncated_text` already refuses an `RFLAGS` shape, and the order
        // here has to agree with it: a trap frame read at the wrong offset is
        // not a smashed code pointer, and reporting it as one sends the reader
        // hunting a writer that does not exist.
        let flags = 0x1_0282u64;
        assert!(looks_like_rflags(flags));
        assert!(
            in_text(TEXT, KERNEL_LO + flags),
            "a real question only here"
        );
        assert_eq!(classify_word(TEXT, flags), WordShape::Rflags);
    }

    #[test]
    fn the_shape_of_a_word_moves_with_the_installed_window() {
        // The property every hand-written literal in this kernel broke: the
        // answer is a function of the window, not of a constant.
        let a = KERNEL_LO + 0x200_0000;
        assert_eq!(classify_word(TEXT, a), WordShape::KernelNonText);
        assert_eq!(
            classify_word((KERNEL_LO, KERNEL_LO + 0x400_0000), a),
            WordShape::KernelText
        );
    }

    #[test]
    fn every_shape_has_a_phrase_of_its_own() {
        let all = [
            WordShape::Zero,
            WordShape::Rflags,
            WordShape::Small,
            WordShape::TruncatedText,
            WordShape::KernelText,
            WordShape::KernelNonText,
            WordShape::NonCanonical,
            WordShape::UserHalf,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.as_str().is_empty());
            for b in all.iter().skip(i + 1) {
                assert_ne!(a.as_str(), b.as_str(), "two shapes read the same");
            }
        }
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

    #[test]
    fn a_two_byte_tail_naming_a_sib_is_not_a_call_and_is_not_read_past() {
        // `tail` is "up to eight bytes", so two of them is inside the
        // contract -- a return address in the first page of `.text` leaves no
        // more to read behind it. FF /2 with rm=100 names a SIB byte in every
        // mode but register-direct, and that SIB is the byte *after* the two
        // there are: measuring it reads past the slice, inside the #GP
        // handler, with the fault half-diagnosed and the panic path the one
        // thing that must not be taken from there.
        //
        // No CALL in this family is two bytes long once a SIB is named, so
        // the answer is "not a call" either way. The bound is only about how
        // the answer is reached.
        for modrm in [0x14u8, 0x54, 0x94] {
            assert!(!ends_with_call(&[0xff, modrm]), "modrm {:#04x}", modrm);
        }
    }
}
