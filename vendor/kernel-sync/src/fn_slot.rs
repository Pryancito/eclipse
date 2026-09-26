//! One question, asked wherever the kernel keeps a `fn` in an `AtomicUsize` and
//! later transmutes the word back into a function and **calls** it.
//!
//! The kernel does that in seven places, because several crates have to hand
//! each other a callback across a dependency edge that only goes one way: the
//! deadlock banner and the spin pump in this crate, the scheduler's reschedule
//! IPI sender, kernel-hal's three `klog` accessors, and its clock observer.
//! Every one of them is the same shape — a slot, a load, a `transmute`, a call
//! — and every one of them is a jump to whatever is in that word.
//!
//! A word is not a function just because a function was stored there. The soft
//! smash this kernel has been chasing leaves plausible residue in exactly such
//! slots: most often a kernel stack pointer, and sometimes a `.text` address
//! with its top half zeroed. So the load has to be judged before the call, and
//! *one* of the seven did judge it — with a high-bits test, which every stack
//! pointer and every physmap address in the kernel half passes, so the
//! commonest residue of all went straight through the guard and was jumped to.
//!
//! Hence one window and one predicate, here: the lowest crate of the three, so
//! the other two can ask it, and pure enough that the host suite can hold it to
//! its answers.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// What a hook slot holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Zero: nothing was ever registered. The normal state of every hook until
    /// its owner installs it, on every boot, so it is not a fault and not
    /// counted as one.
    Empty,
    /// Not zero, with no `.text` window published to judge it against.
    ///
    /// **Allowed.** Refusing here would silence the kernel log, the deadlock
    /// banner and the spin pump for the whole of early boot — the part of the
    /// boot that most needs them — and on the two architectures that never
    /// publish a window it would silence them for good. Counted, so the boot
    /// log can say whether any of these calls was ever actually checked.
    Unchecked,
    /// Inside the published `.text`. Allowed.
    Text,
    /// Not zero and not `.text`. Refused.
    Foreign,
}

/// Whether `[lo, hi)` could be a `.text` window.
///
/// A linker symbol that reads back as zero — a build that did not export it, a
/// relocation that did not happen — must not narrow every hook in the kernel to
/// nothing, so an unusable pair leaves the slots unjudged instead of refused.
pub fn plausible_range(lo: usize, hi: usize) -> bool {
    lo != 0 && hi > lo
}

/// Judge one slot against one window. The whole decision; everything below is
/// storage and counting.
///
/// The zero test comes first because an empty slot is recognisable with no
/// window at all, and the answer for it is not "we could not check": it is
/// "there is nothing to call".
pub fn classify(slot: usize, lo: usize, hi: usize) -> Slot {
    if slot == 0 {
        return Slot::Empty;
    }
    if !plausible_range(lo, hi) {
        return Slot::Unchecked;
    }
    // Half-open, and `lo` is *in*: the linker script opens the section with
    // `. = KERNEL_BEGIN; stext = .;`, so the first function of the image sits
    // at `lo` exactly. kernel-hal's literal fallback window started 64 KiB
    // above the image base and therefore disowned the first sixty-four
    // kilobytes of its own `.text`; a window that excluded `lo` would disown
    // the first function.
    if slot >= lo && slot < hi {
        Slot::Text
    } else {
        Slot::Foreign
    }
}

/// `hi` is the published half of the pair: stored last with `Release`, loaded
/// first with `Acquire`, so a reader that sees the new `hi` sees the `lo`
/// stored before it — and a reader that sees the old `hi` (zero) gets a pair
/// [`plausible_range`] rejects, which is the `Unchecked` it would have had
/// anyway.
static TEXT_LO: AtomicUsize = AtomicUsize::new(0);
static TEXT_HI: AtomicUsize = AtomicUsize::new(0);

/// Calls refused because the slot held something that is not `.text`.
static REFUSED: AtomicU32 = AtomicU32::new(0);
/// Calls let through with no window to judge them against.
static UNJUDGED: AtomicU32 = AtomicU32::new(0);

/// Install the window every hook slot is judged against. Returns whether it was
/// taken.
///
/// An unusable pair is refused **without touching what is already published**:
/// one architecture's boot path reading its symbols back as zero must not undo
/// a window another already installed.
pub fn set_text_range(lo: usize, hi: usize) -> bool {
    if !plausible_range(lo, hi) {
        return false;
    }
    TEXT_LO.store(lo, Ordering::Relaxed);
    TEXT_HI.store(hi, Ordering::Release);
    true
}

/// The window as stored: `(0, 0)` while none has been installed.
///
/// No plausibility filter here — [`set_text_range`] is the only writer and it
/// refuses an unusable pair, so the stored pair is either `(0, 0)` or one
/// [`classify`] can use, and filtering twice would be two places to get one
/// rule wrong.
pub fn text_range() -> (usize, usize) {
    let hi = TEXT_HI.load(Ordering::Acquire);
    let lo = TEXT_LO.load(Ordering::Relaxed);
    (lo, hi)
}

/// The word to `transmute` and call, or `None`.
///
/// The counting is the point of this wrapper: a refusal that leaves no trace is
/// a call that silently stopped happening, which is how a dead `klog` or a
/// deaf spin pump would look.
pub fn live_fn(slot: usize) -> Option<usize> {
    let (lo, hi) = text_range();
    match classify(slot, lo, hi) {
        Slot::Empty => None,
        Slot::Unchecked => {
            bump(&UNJUDGED);
            Some(slot)
        }
        Slot::Text => Some(slot),
        Slot::Foreign => {
            bump(&REFUSED);
            None
        }
    }
}

/// `(refused, unjudged)` since boot.
pub fn slot_stats() -> (u32, u32) {
    (
        REFUSED.load(Ordering::Relaxed),
        UNJUDGED.load(Ordering::Relaxed),
    )
}

/// Saturating, not wrapping: these counts are read to answer "was anything ever
/// refused this boot?", and wrapping past the top would turn "every call was"
/// into "none were".
///
/// A compare-exchange loop rather than `fetch_add`, because that is where the
/// saturation lives -- the loop simply stops when there is nowhere left to go.
fn bump(counter: &AtomicU32) {
    let mut n = counter.load(Ordering::Relaxed);
    while n != u32::MAX {
        match counter.compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(seen) => n = seen,
        }
    }
}

// Test-only windows onto the statics. Early boot -- no window published, and
// the counters near the top of their range -- is a state the kernel passes
// through once and the suite otherwise cannot reach, because publishing a
// window is deliberately one-way.
#[cfg(test)]
pub(crate) fn clear_text_range_for_test() {
    TEXT_HI.store(0, Ordering::Release);
    TEXT_LO.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn set_counters_for_test(refused: u32, unjudged: u32) {
    REFUSED.store(refused, Ordering::Relaxed);
    UNJUDGED.store(unjudged, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plausible kernel image: the base of the kernel half, and a window of
    /// a couple of megabytes above it.
    const TEXT_LO_ADDR: usize = 0xffff_ff00_0000_0000;
    const TEXT_HI_ADDR: usize = 0xffff_ff00_0020_0000;

    /// The globals are shared by the whole test binary, so the few tests that
    /// touch them take this.
    fn global_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Holds that lock and leaves the globals as this boot found them.
    ///
    /// Restoring is not tidiness. A published window refuses every slot outside
    /// it, and the hooks the rest of this crate's tests install are *host*
    /// function pointers -- nowhere near any kernel `.text`. A window left
    /// behind therefore silences their spin pump and their deadlock recorder,
    /// and they fail. Which is the mechanism working exactly as intended, and
    /// the reason the resting state of the suite has to be "no window": with
    /// none published, every hook runs.
    struct Globals(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    fn globals() -> Globals {
        let held = global_lock();
        clear_text_range_for_test();
        set_counters_for_test(0, 0);
        Globals(held)
    }

    impl Drop for Globals {
        fn drop(&mut self) {
            clear_text_range_for_test();
            set_counters_for_test(0, 0);
        }
    }

    // ── the decision, on windows the test names itself ──────────────────────

    #[test]
    fn a_slot_nobody_ever_registered_is_not_a_refusal() {
        assert_eq!(
            classify(0, TEXT_LO_ADDR, TEXT_HI_ADDR),
            Slot::Empty,
            "every hook starts out empty on every boot; that is not a fault"
        );
    }

    #[test]
    fn a_zero_slot_is_recognised_with_no_window_at_all() {
        // The order of the two early returns: an empty slot is not "we could
        // not check", it is "there is nothing to call".
        assert_eq!(classify(0, 0, 0), Slot::Empty);
    }

    #[test]
    fn a_hook_inside_the_text_window_is_called() {
        let f = TEXT_LO_ADDR + 0x1_2340;
        assert_eq!(classify(f, TEXT_LO_ADDR, TEXT_HI_ADDR), Slot::Text);
    }

    #[test]
    fn the_first_byte_of_text_is_a_callable_hook() {
        // `stext` is the image base, so a function really does live at `lo`.
        assert_eq!(
            classify(TEXT_LO_ADDR, TEXT_LO_ADDR, TEXT_HI_ADDR),
            Slot::Text
        );
    }

    #[test]
    fn a_word_at_the_end_of_text_is_past_it() {
        // Half-open: `etext` is the first byte that is not text.
        assert_eq!(
            classify(TEXT_HI_ADDR, TEXT_LO_ADDR, TEXT_HI_ADDR),
            Slot::Foreign
        );
        assert_eq!(
            classify(TEXT_HI_ADDR - 1, TEXT_LO_ADDR, TEXT_HI_ADDR),
            Slot::Text
        );
    }

    #[test]
    fn a_one_byte_window_holds_exactly_its_one_address() {
        assert_eq!(classify(4, 4, 5), Slot::Text);
        assert_eq!(classify(5, 4, 5), Slot::Foreign);
        assert_eq!(classify(3, 4, 5), Slot::Foreign);
    }

    #[test]
    fn a_pointer_whose_top_half_was_zeroed_is_refused() {
        // The residue the soft-smash hunt is named after: a `.text` address
        // that lost its high 32 bits. It is nowhere near the window, and the
        // only reason it needs saying is that it still *reads* like a code
        // address.
        let truncated = (TEXT_LO_ADDR + 0x13_4460) & 0xffff_ffff;
        assert_ne!(truncated, 0, "a truncated .text word is not zero");
        assert_eq!(
            classify(truncated, TEXT_LO_ADDR, TEXT_HI_ADDR),
            Slot::Foreign
        );
    }

    #[test]
    fn a_kernel_stack_pointer_is_refused_although_it_shares_the_top_bits() {
        // This is the case the guard that came before this one let through: it
        // tested the high bits, and every stack and physmap address in the
        // kernel half has the same high bits as `.text`. The coroutine stacks
        // live megabytes above the image.
        let stack = TEXT_LO_ADDR + 0x40_0000;
        assert_eq!(
            stack >> 40,
            TEXT_LO_ADDR >> 40,
            "the point of the test is that a high-bits check cannot tell these apart"
        );
        assert_eq!(classify(stack, TEXT_LO_ADDR, TEXT_HI_ADDR), Slot::Foreign);
    }

    #[test]
    fn a_word_below_the_window_is_refused_like_one_above_it() {
        assert_eq!(
            classify(TEXT_LO_ADDR - 8, TEXT_LO_ADDR, TEXT_HI_ADDR),
            Slot::Foreign
        );
    }

    // ── no window, or an unusable one ───────────────────────────────────────

    #[test]
    fn with_no_window_published_every_hook_still_runs() {
        // Early boot, and both architectures that never install one. Refusing
        // here would mean no kernel log and no deadlock banner precisely where
        // they are the only output there is.
        let f = TEXT_LO_ADDR + 0x1_2340;
        assert_eq!(classify(f, 0, 0), Slot::Unchecked);
    }

    #[test]
    fn a_window_whose_symbols_read_back_as_zero_is_not_a_window() {
        assert!(!plausible_range(0, TEXT_HI_ADDR));
        assert_eq!(classify(TEXT_LO_ADDR + 8, 0, TEXT_HI_ADDR), Slot::Unchecked);
    }

    #[test]
    fn a_window_that_ends_where_it_starts_holds_nothing() {
        assert!(!plausible_range(TEXT_LO_ADDR, TEXT_LO_ADDR));
        assert_eq!(
            classify(TEXT_LO_ADDR, TEXT_LO_ADDR, TEXT_LO_ADDR),
            Slot::Unchecked,
            "an empty window judges nothing rather than refusing everything"
        );
    }

    #[test]
    fn a_window_whose_ends_are_the_wrong_way_round_is_not_a_window() {
        assert!(!plausible_range(TEXT_HI_ADDR, TEXT_LO_ADDR));
        assert_eq!(
            classify(TEXT_LO_ADDR + 8, TEXT_HI_ADDR, TEXT_LO_ADDR),
            Slot::Unchecked
        );
    }

    #[test]
    fn a_reader_that_catches_the_window_half_published_judges_nothing() {
        // `hi` is stored last, so the pair a racing reader can see is
        // `(lo, 0)`. It must read as "no window", not as an empty one that
        // refuses every hook in the kernel.
        assert_eq!(classify(TEXT_LO_ADDR + 8, TEXT_LO_ADDR, 0), Slot::Unchecked);
    }

    #[test]
    fn a_plausible_window_is_one_that_holds_at_least_one_address() {
        assert!(plausible_range(TEXT_LO_ADDR, TEXT_HI_ADDR));
        assert!(plausible_range(4, 5));
    }

    // ── the published window and the counters ───────────────────────────────

    #[test]
    fn a_window_is_only_taken_when_it_could_be_text() {
        let _g = globals();
        assert!(set_text_range(TEXT_LO_ADDR, TEXT_HI_ADDR));
        assert!(!set_text_range(0, TEXT_HI_ADDR));
        assert!(!set_text_range(TEXT_HI_ADDR, TEXT_LO_ADDR));
        assert!(!set_text_range(TEXT_LO_ADDR, TEXT_LO_ADDR));
    }

    #[test]
    fn an_unusable_window_leaves_the_one_already_published_alone() {
        let _g = globals();
        assert!(set_text_range(TEXT_LO_ADDR, TEXT_HI_ADDR));
        assert!(!set_text_range(0, 0));
        assert_eq!(text_range(), (TEXT_LO_ADDR, TEXT_HI_ADDR));
    }

    #[test]
    fn a_second_window_replaces_the_first() {
        let _g = globals();
        assert!(set_text_range(TEXT_LO_ADDR, TEXT_HI_ADDR));
        let wider = TEXT_LO_ADDR + 0x100_0000;
        assert!(set_text_range(TEXT_LO_ADDR, wider));
        assert_eq!(text_range(), (TEXT_LO_ADDR, wider));
    }

    #[test]
    fn the_published_window_is_the_one_the_hooks_are_judged_against() {
        let _g = globals();
        assert!(set_text_range(TEXT_LO_ADDR, TEXT_HI_ADDR));
        let inside = TEXT_LO_ADDR + 0x1000;
        let outside = TEXT_LO_ADDR + 0x40_0000;
        assert_eq!(live_fn(inside), Some(inside));
        assert_eq!(live_fn(outside), None);
    }

    #[test]
    fn a_refused_call_is_counted_and_a_called_one_is_not() {
        let _g = globals();
        assert!(set_text_range(TEXT_LO_ADDR, TEXT_HI_ADDR));
        let (refused_before, unjudged_before) = slot_stats();
        assert_eq!(live_fn(TEXT_LO_ADDR + 0x1000).is_some(), true);
        assert_eq!(slot_stats(), (refused_before, unjudged_before));
        assert!(live_fn(TEXT_LO_ADDR + 0x40_0000).is_none());
        assert_eq!(slot_stats(), (refused_before + 1, unjudged_before));
    }

    #[test]
    fn a_hook_let_through_unjudged_is_counted_apart_from_a_refused_one() {
        let _g = globals();
        // No window: `set_text_range` cannot un-publish one, so drive the
        // distinction through the pure decision and then check that `live_fn`
        // counts the two cases in two different places.
        assert_eq!(classify(8, 0, 0), Slot::Unchecked);
        assert!(set_text_range(TEXT_LO_ADDR, TEXT_HI_ADDR));
        let (refused_before, unjudged_before) = slot_stats();
        assert!(live_fn(TEXT_LO_ADDR + 0x40_0000).is_none());
        let (refused_after, unjudged_after) = slot_stats();
        assert_eq!(refused_after, refused_before + 1);
        assert_eq!(
            unjudged_after, unjudged_before,
            "a word judged and refused is not a word nobody could judge"
        );
    }

    #[test]
    fn an_empty_slot_is_counted_as_neither() {
        let _g = globals();
        assert!(set_text_range(TEXT_LO_ADDR, TEXT_HI_ADDR));
        let before = slot_stats();
        assert_eq!(live_fn(0), None);
        assert_eq!(
            slot_stats(),
            before,
            "an uninstalled hook is the normal state, not a fault to count"
        );
    }

    #[test]
    fn before_any_window_is_published_a_hook_is_called_and_counted_as_unjudged() {
        let _g = globals();
        let f = TEXT_LO_ADDR + 0x1000;
        assert_eq!(live_fn(f), Some(f), "early boot still gets its diagnostics");
        assert_eq!(
            slot_stats(),
            (0, 1),
            "an unjudged call is not a refusal, and is not silent either"
        );
        // Nothing is refused while there is no window: not even the residue a
        // published window would reject.
        let truncated = (TEXT_LO_ADDR + 0x13_4460) & 0xffff_ffff;
        assert_eq!(live_fn(truncated), Some(truncated));
        assert_eq!(slot_stats(), (0, 2));
    }

    #[test]
    fn an_empty_slot_is_not_counted_as_unjudged_either() {
        let _g = globals();
        assert_eq!(live_fn(0), None);
        assert_eq!(
            slot_stats(),
            (0, 0),
            "there was nothing to judge, so nothing went unjudged"
        );
    }

    #[test]
    fn the_counters_stop_at_the_top_instead_of_starting_over() {
        // The counters are read to decide whether anything was ever refused, so
        // wrapping past the top would turn "every call this boot was refused"
        // into "none were".
        let _g = globals();
        assert!(set_text_range(TEXT_LO_ADDR, TEXT_HI_ADDR));
        set_counters_for_test(4_294_967_295, 4_294_967_294);
        assert!(live_fn(TEXT_LO_ADDR + 0x40_0000).is_none());
        assert_eq!(
            slot_stats().0,
            4_294_967_295,
            "the last thing the refusal count says is its top value"
        );
        clear_text_range_for_test();
        assert_eq!(live_fn(8), Some(8));
        assert_eq!(slot_stats().1, 4_294_967_295);
        assert_eq!(live_fn(8), Some(8));
        assert_eq!(slot_stats().1, 4_294_967_295);
    }
}
