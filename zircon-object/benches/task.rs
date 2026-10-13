//! Reference benchmarks for asking a process for its threads.
//!
//! Eight walks in the two Linux crates do this on paths a program hits
//! constantly: choosing which thread a signal goes to, marking a signal
//! pending on all of them, waking the sleepers, clearing a siginfo on
//! `sigaction`, adding up the process's CPU time for `times(2)` and
//! `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`, killing the siblings on
//! `execve`, and two `/proc` files. Every one of them asked for the thread
//! IDS and then looked each id up -- and the lookup (`Process::get_child`)
//! is a linear scan of the very list the ids came from, under the lock
//! again. So walking a process cost one lock and one scan PER THREAD.
//!
//! The family is flat over the number of threads, and the number is a real
//! one: a browser tab process or a `tokio` server runs with dozens to a few
//! hundred threads, which is also the shape in which "send this process a
//! signal" turns into something quadratic in how threaded it is.
//!
//! `*_of_1` is the control row in each family: the same walk over a process
//! with one thread, so the rest is read as a difference against the floor of
//! the operation rather than as an absolute.
//!
//! The two families are an A/B of one decision rather than a measurement of
//! the code as shipped: `threads_by_id` is the walk those eight callers used
//! to do, and `threads_direct` is [`Process::threads`], which is what they do
//! now.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the
//! reasons given in `benches/vm.rs`, and every row passes its INPUTS through
//! `test::black_box` as well as its result.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p zircon-object --bench task --features libos,aspace-separate
//! ```

#![feature(test)]
extern crate test;

use std::sync::Arc;
use test::Bencher;
use zircon_object::object::KernelObject;
use zircon_object::task::{Job, Process, Thread};

/// A process with `n` threads, which is the shape every one of the eight
/// walks meets: each thread has to be found and its `Arc` handed out.
fn a_process_with(n: usize) -> Arc<Process> {
    let proc = Process::create(&Job::root(), "bench").unwrap();
    for _ in 0..n {
        Thread::create(&proc, "bench").unwrap();
    }
    proc
}

macro_rules! by_id {
    ($name:ident, $n:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let proc = a_process_with($n);
            b.iter(|| {
                test::black_box(&proc)
                    .thread_ids()
                    .into_iter()
                    .filter_map(|id| proc.get_child(id).ok())
                    .count()
            });
        }
    };
}
by_id!(threads_by_id_of_1, 1);
by_id!(threads_by_id_of_16, 16);
by_id!(threads_by_id_of_64, 64);
by_id!(threads_by_id_of_256, 256);

macro_rules! direct {
    ($name:ident, $n:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let proc = a_process_with($n);
            b.iter(|| test::black_box(&proc).threads().len());
        }
    };
}
direct!(threads_direct_of_1, 1);
direct!(threads_direct_of_16, 16);
direct!(threads_direct_of_64, 64);
direct!(threads_direct_of_256, 256);
