//! The reports `/proc/perf`, `/proc/<pid>/perf`, `/proc/perf/kernel` and
//! `/proc/perf/top` render, as a reader reads them: the figures a recorded
//! syscall adds show up in its process's table, and the kernel report says
//! whether its figures are of the last window or of the whole uptime.
//!
//! These files are Eclipse's own (Linux has no `/proc/perf`), so the shape
//! asserted is the one `lunarmon` and the docs rely on.

use super::*;
use crate::process::ProcessExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::Process;

fn a_process(pid: u64) -> alloc::sync::Arc<Process> {
    Process::create_linux(&zircon_object::task::ROOT_JOB, RamFS::new(), 0, None, pid).unwrap()
}

/// One table row: the syscall's name, then its calls, total ms and avg us.
fn row<'a>(report: &'a str, name: &str) -> Option<Vec<&'a str>> {
    report
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|w| w.first() == Some(&name))
}

/// A process's table lists what it called, busiest first, with the time
/// columns in ms and us of what it took, and the totals line adds it up.
#[test]
fn a_process_report_counts_what_the_process_called() {
    with_interval(1, || {
        let proc = a_process(910_801);
        let p = proc.linux();
        // getpid three times, at 2 us each; write once, at 1 ms.
        for _ in 0..3 {
            record(p, 39, 2_000, true);
        }
        record(p, 1, 1_000_000, true);
        let report = proc_report(p, 910_801);
        assert!(
            report.starts_with("eclipse perf — pid 910801 (?)\n"),
            "{}",
            report
        );
        assert!(
            report.contains("syscalls total: 4   time in syscalls: 1.006 ms"),
            "{}",
            report
        );
        let getpid = row(&report, &name_of(39)).expect("a getpid row");
        assert_eq!(&getpid[1..], ["3", "0.006", "2.00"], "{}", report);
        let write = row(&report, &name_of(1)).expect("a write row");
        assert_eq!(&write[1..], ["1", "1.000", "1000.00"], "{}", report);
        // Busiest first: three calls before one.
        let at = |n: &str| report.find(&alloc::format!("\n  {} ", n)).unwrap();
        assert!(at(&name_of(39)) < at(&name_of(1)), "{}", report);
        // Nothing it did not call.
        assert!(row(&report, &name_of(0)).is_none(), "{}", report);
        // At interval 1 nothing is estimated, and the table does not say so.
        assert!(!report.contains("estimated"), "{}", report);
    });
}

/// With one call in `n` timed, the table says its times are estimates.
#[test]
fn a_sampled_table_says_its_times_are_estimated() {
    with_interval(8, || {
        let proc = a_process(910_802);
        record(proc.linux(), 39, 1_000, true);
        let report = proc_report(proc.linux(), 910_802);
        assert!(
            report.contains("estimated from 1 call in 8 -- PERFSAMPLE=1"),
            "{}",
            report
        );
    });
}

/// `/proc/perf` is the whole machine's: a call any process records is in
/// its count, and its header gives the uptime and the rate.
#[test]
fn the_global_report_counts_every_process() {
    with_interval(1, || {
        // A syscall number past the stats table is counted here alone, so
        // no other test's calls move it.
        let num = (crate::syscall_stats::MAX_SYSCALL as u32).min(PERF_NR as u32 - 1);
        let before = sys_calls_of(num as usize);
        let proc = a_process(910_803);
        record(proc.linux(), num, 5_000, true);
        record(proc.linux(), num, 5_000, true);
        let report = global_report();
        assert!(
            report.starts_with("eclipse perf — system-wide syscall accounting\n"),
            "{}",
            report
        );
        assert!(report.contains("\nuptime:         "), "{}", report);
        assert!(report.contains("\nsyscalls total: "), "{}", report);
        let r = row(&report, &name_of(num)).expect("the syscall's row");
        let calls: u64 = r[1].parse().unwrap();
        assert!(calls >= before + 2, "{} < {} + 2", calls, before);
    });
}

/// The first read of `/proc/perf/kernel` can only give the lifetime
/// average and says to read again; the next one is of the window between
/// the two and gives the lifetime one beside it.
#[test]
fn the_kernel_report_is_of_the_last_window_from_the_second_read() {
    static ALONE: spin::Mutex<()> = spin::Mutex::new(());
    let _alone = ALONE.lock();
    let first = kernel_report();
    let second = kernel_report();
    for report in [&first, &second] {
        assert!(
            report.starts_with("eclipse perf — kernel runtime stats\n"),
            "{}",
            report
        );
        for line in [
            "cpu temp:     ",
            "uptime:       ",
            "idle naps:    ",
            "cores idle now: ",
            "busy attribution: ",
        ] {
            assert!(report.contains(line), "{}: {}", line, report);
        }
    }
    // Whether the first was the lifetime one depends on whether anything
    // read it before; the second always has a window to report on.
    assert!(second.contains("window over"), "{}", second);
    assert!(second.contains("  lifetime:   idle "), "{}", second);
    assert!(!second.contains("run `cat` again"), "{}", second);
}

/// A tick adds to the histogram `/proc/perf/top` renders, and the report
/// counts it.
#[test]
fn a_tick_is_a_sample_in_the_top_report() {
    let before = SAMPLES.lock().total;
    tick(910_804, 910_804, 0, 0x40_1234);
    let after = SAMPLES.lock().total;
    // `try_lock` on the tick path may drop one under contention; a test
    // ticking alone never contends.
    assert!(after > before, "{} -> {}", before, after);
    let report = top_report();
    assert!(
        report.starts_with("eclipse perf — sampled CPU profile (user space)\n"),
        "{}",
        report
    );
    assert!(!report.contains("no samples yet"), "{}", report);
}
