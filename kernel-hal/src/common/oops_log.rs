//! In-memory record of contained kernel faults, for `/proc/oops`.
//!
//! Once a fault is *contained* (see `zcore::oops`) the machine keeps running —
//! which is exactly when the report matters and exactly when it is easiest to
//! lose: the `[isolate]` lines go to the serial console, and a box with no
//! serial cable (or a user who scrolled past) has nothing left to read.
//!
//! # Why not write `/var/log/oops.log` directly
//!
//! Because the writer runs on the fault path: interrupts are off, the heap is
//! possibly the thing that just got smashed, and `oops` only proceeds when **no
//! kernel lock is held** — its central safety condition. Going through the VFS
//! would allocate, take filesystem and block-device locks, and block on I/O;
//! any one of those re-faults or deadlocks, turning a contained fault back into
//! a dead machine. That is the whole failure mode this subsystem exists to
//! prevent.
//!
//! So this module is the kernel half of the split Linux uses (`printk` ring →
//! `/proc/kmsg` → syslogd → `/var/log`): the fault path appends to a fixed
//! `static` byte buffer with **no allocation, no locks and no I/O**, and
//! userspace drains `/proc/oops` into `/var/log/oops.log` at its leisure. The
//! kernel survives the fault; the log survives the scrollback.
//!
//! Deliberately **fills and stops** rather than wrapping: the budget is bounded
//! anyway (`MAX_CONTAINED`), and a log that silently eats its own beginning is
//! worse than one that says it is full. Contents are lost on reboot — this
//! records faults the kernel *survived*, and userspace has seconds to minutes
//! to pick them up.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Room for well past `MAX_CONTAINED` events.
///
/// Sixty-four kilobytes of `.bss`, not sixteen, because the record now carries
/// the *diagnosis* and not only the `[isolate]` verdict: the fault header, the
/// stack attribution and the `[kfault-bt]` stack scan come to about a kilobyte
/// an event, and `MAX_CONTAINED` is sixteen of them. At the old size a machine
/// that contained its budget filled the buffer and dropped the tail -- and
/// this module exists precisely so that the box with no serial cable keeps the
/// report.
const CAP: usize = 64 * 1024;

/// The record itself. `AtomicU8` per byte so appends from a faulting CPU need
/// no lock — the one thing the fault path cannot take.
static BUF: [core::sync::atomic::AtomicU8; CAP] =
    [const { core::sync::atomic::AtomicU8::new(0) }; CAP];

/// Bytes claimed so far. Reservations are `fetch_add`, so two CPUs containing
/// at once get disjoint ranges instead of interleaving mid-line.
static LEN: AtomicUsize = AtomicUsize::new(0);

/// Set once the buffer fills, so readers know the tail is missing.
static OVERFLOWED: AtomicBool = AtomicBool::new(false);

/// A `fmt::Write` sink that appends into [`BUF`].
struct Sink;

impl Write for Sink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            return Ok(());
        }
        // Reserve first, then fill: the reservation is what makes concurrent
        // appends safe without a lock.
        let start = LEN.fetch_add(bytes.len(), Ordering::AcqRel);
        if start >= CAP {
            // Undo the runaway so `LEN` stays a usable length, and latch the
            // truncation for the reader.
            LEN.store(CAP, Ordering::Release);
            OVERFLOWED.store(true, Ordering::Release);
            return Ok(());
        }
        let end = (start + bytes.len()).min(CAP);
        if end < start + bytes.len() {
            OVERFLOWED.store(true, Ordering::Release);
            LEN.store(CAP, Ordering::Release);
        }
        for (i, b) in bytes.iter().take(end - start).enumerate() {
            BUF[start + i].store(*b, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Append a formatted line to the in-memory oops record.
///
/// Callable from the fault path: allocation-free, lock-free, and it never
/// touches a device. Failure to format is ignored — losing a log line must
/// never be able to escalate into a second fault.
pub fn record(args: fmt::Arguments) {
    let _ = Sink.write_fmt(args);
}

/// Print to the serial console AND append to the record `/proc/oops` exposes.
///
/// The fault path used to call [`crate::console::serial_write_fmt_spin`]
/// directly for everything but the `[isolate]` verdict, so a real
/// `/var/log/oops.log` read back as two lines: "a fault was contained" and the
/// culprit heuristic -- no faulting address, no stack attribution, no
/// backtrace. Every one of those lines had been written; they went to a
/// console nobody was capturing. The split this module documents is only worth
/// having if the diagnosis is on the durable side of it.
pub fn report(args: fmt::Arguments<'_>) {
    crate::console::serial_write_fmt_spin(args);
    record(args);
}

/// [`report`] for a plain string.
pub fn report_str(s: &str) {
    crate::console::serial_write_str(s);
    record(format_args!("{}", s));
}

/// Whether anything has been recorded since boot.
pub fn is_empty() -> bool {
    LEN.load(Ordering::Acquire) == 0
}

/// Copy the record out as bytes. Called from ordinary process context by
/// `/proc/oops`, where allocating is fine.
pub fn snapshot() -> alloc::vec::Vec<u8> {
    let len = LEN.load(Ordering::Acquire).min(CAP);
    let mut out = alloc::vec::Vec::with_capacity(len + 96);
    for b in BUF.iter().take(len) {
        out.push(b.load(Ordering::Relaxed));
    }
    if OVERFLOWED.load(Ordering::Acquire) {
        out.extend_from_slice(
            b"\n[oops-log] record full; later events were dropped (raise CAP in \
              kernel-hal/src/common/oops_log.rs)\n",
        );
    }
    out
}

/// Empty the record so one test's lines are not another's.
///
/// `LEN` only ever grows in the shipped kernel -- deliberately: the buffer
/// fills and stops rather than eating its own beginning. That is also why this
/// module had no tests at all while being the one place a contained fault's
/// report survives.
#[cfg(test)]
fn reset_for_tests() {
    LEN.store(0, Ordering::SeqCst);
    OVERFLOWED.store(false, Ordering::SeqCst);
    for b in BUF.iter() {
        b.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> alloc::string::String {
        alloc::string::String::from_utf8_lossy(&snapshot()).into_owned()
    }

    #[test]
    fn a_recorded_line_comes_back_out() {
        reset_for_tests();
        assert!(is_empty());
        record(format_args!("[isolate] contained {}/{}\n", 1, 16));
        assert!(!is_empty());
        assert!(text().contains("[isolate] contained 1/16"));
    }

    /// The regression this module was failing silently: a diagnosis line that
    /// went to the console only. A real `/var/log/oops.log` came back as the
    /// `[isolate]` verdict and nothing else -- no faulting address, no stack
    /// attribution, no backtrace -- because every one of those lines was
    /// written straight to the serial writer. `report` is the sink that is on
    /// the durable side of the split this module documents.
    #[test]
    fn a_reported_line_is_on_the_durable_side_and_not_only_the_console() {
        reset_for_tests();
        report(format_args!(
            "[KERNEL PAGE FAULT] vaddr={:#x} flags=EXECUTE\n",
            0x1000
        ));
        report_str("[kfault-bt] raw stack scan from rsp:\n");
        let out = text();
        assert!(out.contains("vaddr=0x1000"), "the header was lost: {}", out);
        assert!(out.contains("[kfault-bt] raw stack scan"), "{}", out);
    }

    #[test]
    fn a_full_record_stops_rather_than_eating_its_own_beginning() {
        reset_for_tests();
        // The first line has to survive: it is the one that names the fault.
        record(format_args!("FIRST\n"));
        for _ in 0..(CAP / 8 + 64) {
            record(format_args!("xxxxxxx\n"));
        }
        let out = text();
        assert!(out.starts_with("FIRST\n"), "the beginning was eaten");
        assert!(
            out.contains("record full; later events were dropped"),
            "a truncated record did not say so"
        );
        // `LEN` is clamped to something a reader can use, not left runaway.
        assert!(LEN.load(Ordering::Acquire) <= CAP);
    }

    /// A whole contained-fault report has to fit, which is what the capacity
    /// is for: the header, the stack attribution and a `[kfault-bt]` scan come
    /// to roughly a kilobyte, and the fault budget is `MAX_CONTAINED` = 16.
    #[test]
    fn the_capacity_holds_a_full_report_for_every_fault_the_budget_allows() {
        const PER_EVENT: usize = 1024;
        const MAX_CONTAINED: usize = 16;
        assert!(
            CAP >= PER_EVENT * MAX_CONTAINED,
            "{} B cannot hold {} reports of {} B",
            CAP,
            MAX_CONTAINED,
            PER_EVENT
        );
        reset_for_tests();
        for n in 0..MAX_CONTAINED {
            for _ in 0..(PER_EVENT / 8) {
                record(format_args!("ev{:05}\n", n));
            }
        }
        assert!(
            !OVERFLOWED.load(Ordering::Acquire),
            "the budget's worth of reports did not fit"
        );
        let out = text();
        assert!(out.contains("ev00000"), "the first event was lost");
        assert!(out.contains("ev00015"), "the last event was lost");
    }
}
