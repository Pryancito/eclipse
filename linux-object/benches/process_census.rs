//! Reference benchmarks for the process census: the walk that every syscall
//! taking a pid, a process group or a user has to do before it can answer.
//!
//! `kill(-pgid)` and a terminal's Ctrl-C (`send_signal_to_pgrp`), `getpgid`,
//! `getsid`, `setpriority`/`getpriority` on a group or a user,
//! `sched_setaffinity` on somebody else, `setsid`'s own veto
//! (`live_effective_pgids`) and the `/proc` listing all go through
//! `all_live_processes`, and none of them had a single row. Linux keeps an
//! index per pid type (`pid->tasks[PIDTYPE_PGID]`), so each of those walks
//! the group it names and nothing else; here the only index is the job tree,
//! so the census is built first and filtered afterwards -- which makes the
//! cost of the census the cost of all of them, and makes the shape of this
//! family the question that matters: whether a machine pays for the processes
//! it is already running every time one of them calls `kill` or `getpgid`.
//!
//! The family is flat over the number of processes, and the number is a real
//! one: a desktop with Firefox and a terminal open is a few hundred, which is
//! also the shape in which a `for` loop over `kill` turns into something
//! quadratic in the number of processes.
//!
//! `census_of_1` is the control row: the same call against a job with one
//! member, so the rest of the family is read as a difference against the
//! floor of the operation rather than as an absolute.
//!
//! The rows go through [`all_live_processes_in`] rather than
//! `all_live_processes`, because the root job holds whatever else the
//! binary happens to be running -- including the processes the other rows of
//! this very file made. A row has to be able to hold its census still to mean
//! anything.
//!
//! The two `getpgid_*` families are an A/B of one decision rather than a
//! measurement of the code as shipped: `getpgid_by_census` is the walk
//! `get_process_pgid` used to do (build the whole census, keep one), and
//! `getpgid_by_lookup` is what it does now.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the
//! reasons given in `zircon-object/benches/vm.rs`, and every row passes its
//! INPUTS through `test::black_box` as well as its result.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p linux-object --bench process_census --features mock-disk
//! ```

#![feature(test)]

extern crate test;

use linux_object::process::{all_live_processes_in, effective_pgid, LinuxProcess};
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use std::sync::Arc;
use test::Bencher;
use zircon_object::object::KernelObject;
use zircon_object::task::{Job, Process, Thread};

/// Pids are a global table, so this file takes its own range.
static NEXT_PID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(940_000);

/// A job holding `n` live Linux processes, each with a thread, which is the
/// shape of the real tree: every process the census walks has an extension to
/// resolve and a status to read.
fn a_job_with(n: usize) -> (Arc<Job>, u64) {
    let job = zircon_object::task::ROOT_JOB.create_child().unwrap();
    let first = NEXT_PID.fetch_add(n as u64, core::sync::atomic::Ordering::Relaxed);
    for i in 0..n as u64 {
        let proc = Process::create_with_fixed_id_ext(
            &job,
            first + i,
            "bench",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        Thread::create_linux(&proc).unwrap();
    }
    (job, first)
}

macro_rules! census {
    ($name:ident, $n:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let (job, _) = a_job_with($n);
            b.iter(|| test::black_box(all_live_processes_in(test::black_box(&job))).len());
        }
    };
}

census!(census_of_1, 1);
census!(census_of_16, 16);
census!(census_of_64, 64);
census!(census_of_256, 256);

/// `getpgid(pid)` as it was: the whole census, then keep the one asked for.
/// The pid looked up is the LAST of the job's, so the linear search over the
/// census is paid in full -- a pid is as likely to be at one end as the other
/// and the first-hit case would flatter the row.
macro_rules! getpgid_by_census {
    ($name:ident, $n:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let (job, first) = a_job_with($n);
            let pid = first + $n as u64 - 1;
            b.iter(|| {
                let proc = all_live_processes_in(test::black_box(&job))
                    .into_iter()
                    .find(|p| p.id() == test::black_box(pid))
                    .unwrap();
                test::black_box(effective_pgid(&proc))
            });
        }
    };
}

getpgid_by_census!(getpgid_by_census_of_1, 1);
getpgid_by_census!(getpgid_by_census_of_64, 64);
getpgid_by_census!(getpgid_by_census_of_256, 256);

/// `getpgid(pid)` as it is now: ask the job tree for the one process.
macro_rules! getpgid_by_lookup {
    ($name:ident, $n:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let (job, first) = a_job_with($n);
            let pid = first + $n as u64 - 1;
            b.iter(|| {
                let proc = test::black_box(&job)
                    .find_process(test::black_box(pid))
                    .unwrap();
                test::black_box(effective_pgid(&proc))
            });
        }
    };
}

getpgid_by_lookup!(getpgid_by_lookup_of_1, 1);
getpgid_by_lookup!(getpgid_by_lookup_of_64, 64);
getpgid_by_lookup!(getpgid_by_lookup_of_256, 256);

/// The group filter on top of the census, which is what a Ctrl-C does
/// (`send_signal_to_pgrp`) and what `setpriority(PRIO_PGRP)` does. Nothing is
/// signalled: the row is the WALK, and sending would measure the signal path
/// instead. The group asked for is one nobody is in, so the filter is paid in
/// full for every member.
macro_rules! group_filter {
    ($name:ident, $n:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let (job, first) = a_job_with($n);
            let absent = test::black_box(first + 1_000_000);
            b.iter(|| {
                all_live_processes_in(test::black_box(&job))
                    .into_iter()
                    .filter(|p| effective_pgid(p) == absent)
                    .count()
            });
        }
    };
}

group_filter!(group_filter_of_1, 1);
group_filter!(group_filter_of_64, 64);
group_filter!(group_filter_of_256, 256);
