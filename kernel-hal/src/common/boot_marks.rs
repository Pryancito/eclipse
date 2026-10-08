//! Boot timeline — when the kernel reached each of its boot progress marks.
//!
//! The boot already walks a progress bar from 52% to 100% ([`crate::console::
//! early_progress_bar`]), with marks at the ends of the expensive stretches:
//! memory init, the HAL's early init, the device probe, the PCI scan, the
//! handoff to userspace. Those marks were drawn on the framebuffer and
//! forgotten. Here they are also *timestamped*, so the same boot that drew the
//! bar can afterwards say how long each stretch took.
//!
//! This is deliberately a recorder and nothing else: no new call sites, no
//! decisions taken from the numbers, no cost beyond one relaxed store per mark
//! (there are fewer than twenty in a whole boot). "Make the boot faster" has
//! to start from a measurement, and before this there was none for the kernel
//! half — `dmesg` timestamps every line it prints, which tells you when the
//! kernel *said* something, not where it spent the time between two lines.
//!
//! Two caveats the table prints for itself, because a reader who does not know
//! them will draw the wrong conclusion:
//!
//!  * **Marks below [`TSC_TRUSTED_FROM`] are timed with the *provisional* TSC
//!    frequency.** The real one is measured against the ACPI PM timer at the
//!    start of the device probe, and on a machine whose provisional guess was
//!    off those early stamps are off by the same factor (the recalibration
//!    line in `dmesg` says by how much).
//!  * **A gap is wall time, not work.** A stretch that waits on firmware or on
//!    a device looks exactly like a stretch that computes.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

/// One slot per possible progress percentage, so a mark is recorded by
/// indexing rather than by searching, from any context, with no lock: these
/// stores happen on the boot path, some of them before the heap is usable.
const SLOTS: usize = 101;

/// Uptime nanoseconds at which each mark was reached; `0` means "not reached".
///
/// First write wins ([`mark`]): a progress percentage is meant to be passed
/// once per boot, and if one is ever passed twice the *first* arrival is the
/// one that bounds the stretch before it.
static AT_NS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// The first mark whose timestamp is taken with a TSC frequency that has been
/// checked against the ACPI PM timer (`recalibrate_tsc_hz`, the first thing the
/// x86 device probe does). Anything below this is timed with the provisional
/// frequency and is only as good as that guess.
const TSC_TRUSTED_FROM: u32 = 81;

/// What each mark means, for the table. A percentage with no entry prints as
/// itself — an unknown mark is a readable row, not a panic, so adding a
/// `early_progress_bar` call site never has to come here first (though a new
/// mark that brackets something expensive is worth naming).
fn label(progress: u32) -> &'static str {
    match progress {
        52 => "entered the kernel (arch entry)",
        53 => "logging up",
        54 => "kernel heap and frame allocator",
        55 => "HAL early init (paging, serial, TSC guess)",
        60 => "boot options parsed",
        70 => "boot banner and cmdline switches",
        80 => "HAL init (interrupts, timer, SMP)",
        81 => "TSC checked against the ACPI PM timer",
        82 => "local APIC",
        83 => "IRQ controller and UART IRQs",
        84 => "framebuffer and display registered",
        87 => "PCI scan",
        88 => "PCI devices registered, MSI finished",
        90 => "drivers up, HAL init, hunter",
        91 => "root filesystem mounted",
        95 => "handing over to userspace",
        100 => "init(1) running",
        _ => "",
    }
}

/// Record that the boot reached `progress`.
///
/// Called from [`crate::console::early_progress_bar`], so every existing mark
/// is timed with no new call site.
pub fn mark(progress: u32) {
    let Some(slot) = AT_NS.get(progress as usize) else {
        return;
    };
    let now = crate::timer::timer_now().as_nanos() as u64;
    // `max(1)`: a stamp of exactly 0 ns is indistinguishable from "not
    // reached", and the very first mark can genuinely land on a zero counter
    // (under QEMU the TSC really does start at 0). One nanosecond early is
    // not a lie anyone can measure.
    let _ = slot.compare_exchange(0, now.max(1), Ordering::Relaxed, Ordering::Relaxed);
}

/// The marks reached so far, in the order they were reached, as
/// `(progress, at_ns)`.
///
/// Ordered by *time* and not by percentage, because the percentages are only
/// as monotonic as the call sites are: if a stretch is ever moved and the
/// numbers end up out of order, a timeline sorted by time still reads
/// correctly, and the percentage column shows what happened.
pub fn reached() -> Vec<(u32, u64)> {
    let mut rows: Vec<(u32, u64)> = AT_NS
        .iter()
        .enumerate()
        .filter_map(|(p, at)| match at.load(Ordering::Relaxed) {
            0 => None,
            ns => Some((p as u32, ns)),
        })
        .collect();
    rows.sort_by_key(|(_, ns)| *ns);
    rows
}

/// The boot timeline as plain text: one row per mark, with the gap since the
/// previous one.
///
/// The gap is the figure to read: it is the cost of the stretch that *ends* at
/// this mark. `at` is there to line the row up against a `dmesg` timestamp.
pub fn render() -> String {
    let rows = reached();
    let mut out = String::new();
    if rows.is_empty() {
        let _ = writeln!(out, "boot timeline:  no progress mark has been reached");
        return out;
    }
    let total = rows.last().map(|(_, ns)| *ns).unwrap_or(0);
    let _ = writeln!(
        out,
        "boot timeline — {} marks, {} to the last one",
        rows.len(),
        fmt_ms(total)
    );
    let _ = writeln!(
        out,
        "  gap = time spent in the stretch ENDING at that mark;"
    );
    let _ = writeln!(
        out,
        "  marks below {}% are timed with the provisional TSC frequency.",
        TSC_TRUSTED_FROM
    );
    // Ten wide, not eight: a stamp past a hundred milliseconds is nine
    // characters with the unit on it ("136.739ms"), and on a real boot most of
    // the column is past that -- at eight the numbers pushed the `stretch`
    // column out of line on exactly the rows worth reading.
    let _ = writeln!(out, "   mark          at         gap  stretch");
    let mut prev = 0u64;
    for (progress, at) in &rows {
        let gap = at.saturating_sub(prev);
        prev = *at;
        let _ = writeln!(
            out,
            "   {:>3}%  {:>10}  {:>10}  {}{}",
            progress,
            fmt_ms(*at),
            fmt_ms(gap),
            label(*progress),
            if *progress < TSC_TRUSTED_FROM {
                " (*)"
            } else {
                ""
            },
        );
    }
    out
}

/// Milliseconds with three decimals, so the column lines up and a sub-
/// millisecond stretch is still visible rather than printing as `0`.
fn fmt_ms(ns: u64) -> String {
    let mut s = String::new();
    let _ = write!(s, "{}.{:03}ms", ns / 1_000_000, (ns % 1_000_000) / 1_000);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_of_zero_nanoseconds_still_counts_as_reached() {
        // The first mark can land on a counter that really reads 0 (QEMU's TSC
        // starts there), and `0` is this table's "not reached". Without the
        // `max(1)` the earliest, most interesting mark would be the one that
        // vanished.
        AT_NS[0].store(0, Ordering::Relaxed);
        let _ = AT_NS[0].compare_exchange(0, 0u64.max(1), Ordering::Relaxed, Ordering::Relaxed);
        assert_eq!(AT_NS[0].load(Ordering::Relaxed), 1);
        AT_NS[0].store(0, Ordering::Relaxed);
    }

    #[test]
    fn the_timeline_is_ordered_by_time_not_by_percentage() {
        // The percentages are only as monotonic as the call sites: a stretch
        // moved without renumbering must still read as a timeline.
        AT_NS[70].store(5_000_000, Ordering::Relaxed);
        AT_NS[60].store(9_000_000, Ordering::Relaxed);
        let rows = reached();
        AT_NS[70].store(0, Ordering::Relaxed);
        AT_NS[60].store(0, Ordering::Relaxed);
        assert_eq!(rows, vec![(70, 5_000_000), (60, 9_000_000)]);
    }

    #[test]
    fn an_unnamed_mark_is_a_row_and_not_a_panic() {
        // Adding an `early_progress_bar` call site must not have to come here
        // first.
        assert_eq!(label(53), "logging up");
        assert_eq!(label(7), "");
    }

    #[test]
    fn a_gap_is_the_stretch_that_ends_at_the_mark() {
        AT_NS[54].store(2_000_000, Ordering::Relaxed);
        AT_NS[55].store(7_500_000, Ordering::Relaxed);
        let text = render();
        AT_NS[54].store(0, Ordering::Relaxed);
        AT_NS[55].store(0, Ordering::Relaxed);
        // 55% is charged 5.5 ms, the distance from 54%, not its own 7.5 ms.
        assert!(text.contains("5.500ms"), "{text}");
        assert!(text.contains("7.500ms"), "{text}");
    }

    #[test]
    fn sub_millisecond_stretches_are_still_visible() {
        assert_eq!(fmt_ms(125_000), "0.125ms");
        assert_eq!(fmt_ms(0), "0.000ms");
    }
}
