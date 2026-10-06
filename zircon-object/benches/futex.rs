//! Reference benchmarks for the futex objects: the table every futex call
//! looks a word up in, its sweep, and the wake path a `pthread_mutex_unlock`
//! takes when nobody is waiting.
//!
//! Why these. `tools/eclipse-bench --only futex` measures from userspace what
//! a lock costs under contention, and on a machine with fewer CPUs than
//! waiters most of those figures are the woken thread queueing for a CPU
//! rather than anything this crate did. These rows have no syscall, no
//! scheduler and no second thread in them, so they say what the futex
//! bookkeeping itself costs -- the part a code change here can actually move.
//!
//! `FutexTable::get_or_create` is the one to watch. Every futex wait and every
//! wake goes through it, it lives inside a process's `inner` (so it is held
//! under that lock), and it sweeps when it passes a threshold. The `_of_1` /
//! `_of_64` / `_of_512` family is there so the SLOPE can be read: a hit that
//! costs the same whatever the table holds is the hash map doing its job, and
//! one that grows is every futex call on a busy process paying for the words
//! that process has ever used.
//!
//! What is NOT here: `wait`. It returns a future that only completes when
//! something wakes it, which needs a task and an executor, and `libos` has no
//! scheduler of its own. The wait side is measured from userspace instead.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the reasons
//! given in `vm.rs`.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p zircon-object --bench futex --features libos,aspace-separate
//! ```
//!
//! Every row black-boxes its inputs as well as its result: these are a few
//! instructions over values the compiler can see, and black-boxing only the
//! result lets it fold the work away, leaving a sub-nanosecond figure that is
//! the loop overhead and not a measurement.

#![feature(test)]

extern crate test;

use core::sync::atomic::AtomicI32;
use std::sync::Arc;
use test::Bencher;
use zircon_object::signal::{Futex, FutexTable};

/// A futex word with the `'static` lifetime `Futex::new` requires. Leaked on
/// purpose: in the kernel the word lives in a frame the table keeps alive, and
/// a bench that freed it would be handing `Futex` a dangling reference.
fn leaked_word(value: i32) -> &'static AtomicI32 {
    Box::leak(Box::new(AtomicI32::new(value)))
}

/// A table holding `n` futexes, plus the addresses they were keyed by.
///
/// The keys are spaced a page apart rather than packed, so they are the kind
/// of addresses real mutexes have rather than a dense run a hash map might
/// happen to like.
fn populated(n: usize) -> (FutexTable, Vec<usize>, Vec<Arc<Futex>>) {
    let mut table = FutexTable::default();
    let mut keys = Vec::with_capacity(n);
    // The Arcs are KEPT: the table sweeps entries nobody else references, so
    // dropping them here would let the next insert empty the table and a row
    // labelled `of_512` would be measuring a table of one.
    let mut held = Vec::with_capacity(n);
    for i in 0..n {
        let addr = 0x1000 + i * 0x1000;
        let futex = table.get_or_create_dropping_swept(addr, || Futex::new(leaked_word(0)));
        keys.push(addr);
        held.push(futex);
    }
    (table, keys, held)
}

#[bench]
fn futex_new(b: &mut Bencher) {
    let word = leaked_word(0);
    b.iter(|| test::black_box(Futex::new(test::black_box(word))));
}

/// The lookup that hits, against tables of three sizes. Always the LAST key
/// inserted, which is the worst case for anything that scans.
fn bench_hit(b: &mut Bencher, n: usize) {
    let (mut table, keys, _held) = populated(n);
    let target = *keys.last().expect("at least one");
    b.iter(|| {
        test::black_box(
            table.get_or_create_dropping_swept(test::black_box(target), || {
                unreachable!("the key is in the table: this row must not insert")
            }),
        )
    });
}

#[bench]
fn futex_table_hit_of_1(b: &mut Bencher) {
    bench_hit(b, 1);
}

#[bench]
fn futex_table_hit_of_64(b: &mut Bencher) {
    bench_hit(b, 64);
}

#[bench]
fn futex_table_hit_of_512(b: &mut Bencher) {
    bench_hit(b, 512);
}

/// A miss that inserts, with the table below its sweep threshold: the cost of
/// a process using a futex word it has not used before.
#[bench]
fn futex_table_insert_below_sweep(b: &mut Bencher) {
    let word = leaked_word(0);
    let mut table = FutexTable::default();
    let mut addr = 0x1000usize;
    b.iter(|| {
        // Each iteration starts from empty, so the insert never reaches the
        // threshold and this row is the insert alone. Clearing is outside what
        // we are measuring in spirit but inside the clock, so the row is a
        // ceiling on the insert rather than an exact figure -- `clear` on a
        // one-entry map is a few instructions.
        table.clear();
        addr += 0x1000;
        test::black_box(
            table.get_or_create_dropping_swept(test::black_box(addr), || Futex::new(word)),
        )
    });
}

/// Building a table of 512 entries, sweeps included. This is the only row that
/// exercises the sweep, which fires every time the table passes its threshold
/// and is the path the code calls the likeliest of its family to be reached:
/// it walks the map, checks every entry's reference count and idleness, and
/// hands the dead ones back to the caller to drop outside the lock.
///
/// The figure is per BUILD, not per insert, so divide by 512 to compare it
/// with the insert row above. A build that costs much more than 512 inserts is
/// the sweep; one that costs about the same means the threshold doubling is
/// keeping it amortized, which is what it is for.
#[bench]
fn futex_table_build_512_with_sweeps(b: &mut Bencher) {
    let word = leaked_word(0);
    b.iter(|| {
        let mut table = FutexTable::default();
        // No Arcs kept: every entry is sweepable, which is the state a real
        // process is in once its mutexes are unlocked and idle, and the one
        // that makes the sweep do work.
        for i in 0..512usize {
            table.get_or_create_dropping_swept(test::black_box(0x1000 + i * 0x1000), || {
                Futex::new(word)
            });
        }
        test::black_box(table.len())
    });
}

/// `wake` with nobody waiting is issued by every unlock of a mutex that MIGHT
/// have waiters, so on a mostly-uncontended lock it is the whole kernel cost.
/// The C suite reports ~330 ns for it including the syscall; this is the same
/// work with the syscall taken out.
#[bench]
fn futex_wake_one_no_waiters(b: &mut Bencher) {
    let futex = Futex::new(leaked_word(0));
    b.iter(|| test::black_box(futex.wake(test::black_box(1))));
}

/// `wake(0)` is the other path through the same lock: it clears the owner and
/// returns without touching the queue.
#[bench]
fn futex_wake_zero(b: &mut Bencher) {
    let futex = Futex::new(leaked_word(0));
    b.iter(|| test::black_box(futex.wake(test::black_box(0))));
}

/// The value checks a `FUTEX_WAIT` makes before it parks, and the ones the
/// priority-inheritance helpers drive the lock word with.
#[bench]
fn futex_value_eq(b: &mut Bencher) {
    let futex = Futex::new(leaked_word(7));
    b.iter(|| test::black_box(futex.value_eq(test::black_box(7))));
}

#[bench]
fn futex_load(b: &mut Bencher) {
    let futex = Futex::new(leaked_word(7));
    b.iter(|| test::black_box(futex.load()));
}

#[bench]
fn futex_compare_exchange(b: &mut Bencher) {
    let futex = Futex::new(leaked_word(0));
    b.iter(|| {
        // Swap back and forth so every iteration does a real exchange rather
        // than failing after the first.
        let _ = futex.compare_exchange(test::black_box(0), test::black_box(1));
        test::black_box(futex.compare_exchange(test::black_box(1), test::black_box(0)))
    });
}

#[bench]
fn futex_is_idle(b: &mut Bencher) {
    let futex = Futex::new(leaked_word(0));
    b.iter(|| test::black_box(futex.is_idle()));
}
