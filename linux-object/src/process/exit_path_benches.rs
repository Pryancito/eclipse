//! What the **return** to user mode costs, per syscall.
//!
//! Every syscall this kernel serves ends in the same place: the loop in
//! `loader/src/linux.rs::run_user`, which before it can re-enter user mode
//! asks three questions of the process and its thread --
//!
//! 1. are there process-directed signals waiting to be pulled onto this
//!    thread (`pull_shared_pending_into`),
//! 2. is a signal pending on the thread itself (`handle_signal`),
//! 3. is the process stopped by job control (`is_job_stopped`)?
//!
//! The answers are "no, no, no" on effectively every syscall of a running
//! program -- a desktop makes millions of them between one Ctrl-C and the
//! next -- and the first and third are read out of
//! [`LinuxProcessInner`][super::LinuxProcessInner], behind the single
//! `Mutex` that also guards the file-descriptor table, the credentials, the
//! working directory and a hundred other fields. Every thread of the process
//! takes that lock twice per syscall return to read two words that are
//! almost always zero.
//!
//! A single-threaded row cannot see that: an uncontended ticket lock on a
//! line this core already owns is a handful of cycles. So the rows come in
//! pairs, alone and with other threads of the *same process* doing ordinary
//! work that takes the same lock, and the difference is the measurement.
//!
//! Read the contended rows with the container in mind: four shared cores,
//! unpinned threads, no work between acquisitions. What they establish is
//! the *shape* -- which lock is on the path and who else wants it -- which
//! is a property of the code, not of this machine.
//!
//! ## What they said, before and after
//!
//! The three-question prologue, at each number of other threads of the same
//! process doing ordinary work:
//!
//! | other threads | from `inner` | from the mirror |
//! |---|---|---|
//! | none | 34.2 ns | **10.8 ns** |
//! | one | 232 ns | **11.1 ns** |
//! | two | 1445 ns | **11.1 ns** |
//! | three | 2390 ns | **11.2 ns** |
//!
//! The first column is a curve and the second is a flat line, which is the
//! whole claim: the cost grew with the number of threads because it *was*
//! the lock, and the questions themselves cost nothing. The row that
//! decides is the one-other-thread row -- two threads sharing a lock is the
//! smallest contention there is, and it is what Moebius's two-core machine
//! can produce -- and it is where the saturation rows would have been
//! useless: 2390 +/- 2021.
//!
//! The pieces:
//!
//! ```text
//! pull_shared_pending_with_nothing_pending                  15.0 -> 0.62
//! pull_shared_pending_while_others_use_the_process_lock     1541 -> 0.64
//! is_job_stopped_on_a_running_process                        8.5 -> 0.64
//! is_job_stopped_while_others_use_the_process_lock           628 -> 0.37
//! handle_signal_with_nothing_pending                        11.2 -> 11.2
//! the_shaped_control                                                 0.31
//! ```
//!
//! Both mirrored rows land on the control, so they are now "a relaxed load
//! and a branch" and nothing else. `handle_signal` does not move and is not
//! supposed to: it takes the **thread's own** lock, which the threads of a
//! process do not share, and it is the whole of the 11 ns that is left. It
//! is the next thing on this path worth a look, and a different change.

extern crate std;

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use test::{black_box, Bencher};

use self::std::sync::atomic::AtomicBool;
use self::std::sync::Arc as StdArc;
use self::std::thread as stdthread;
use self::std::vec::Vec;

/// How many threads stand in for the other CPUs. One less than this host's
/// cores, so the measuring thread has one.
const OTHERS: usize = 3;

/// A process with one thread, nothing pending, not stopped: what a program
/// returning from `read` looks like.
fn a_running_process(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "exit-path",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    (proc, thread)
}

/// Runs `b.iter(body)` with `OTHERS` threads of the same process taking its
/// `inner` lock in a loop.
///
/// `personality()` stands in for "ordinary work": it is a production getter
/// that reads one field under that lock and nothing else, so the row is
/// about the lock and not about what the other threads happen to be doing.
fn with_others_on_the_process_lock(b: &mut Bencher, proc: &Arc<Process>, body: impl FnMut()) {
    with_n_others_on_the_process_lock(b, proc, OTHERS, body)
}

/// The same with the thread count handed in, because the saturation row is
/// the one that decides nothing: a four-core container with three spinning
/// threads reports a point on a curve whose top is the container's, not the
/// code's. The row that decides is the one at the thread count the machine
/// actually has.
fn with_n_others_on_the_process_lock(
    b: &mut Bencher,
    proc: &Arc<Process>,
    others: usize,
    mut body: impl FnMut(),
) {
    let stop = StdArc::new(AtomicBool::new(false));
    let mut threads = Vec::with_capacity(others);
    for _ in 0..others {
        let stop = StdArc::clone(&stop);
        let proc = proc.clone();
        threads.push(stdthread::spawn(move || {
            let lp = proc.linux();
            while !stop.load(core::sync::atomic::Ordering::Relaxed) {
                for _ in 0..256 {
                    black_box(lp.personality());
                }
            }
        }));
    }
    b.iter(&mut body);
    stop.store(true, core::sync::atomic::Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
}

/// Question 1, alone: the process lock, taken to find an empty `Sigset`.
#[bench]
fn pull_shared_pending_with_nothing_pending(b: &mut Bencher) {
    let (proc, thread) = a_running_process(77_001);
    let lp = proc.linux();
    b.iter(|| lp.pull_shared_pending_into(black_box(&thread)));
}

/// The same question with three other threads of the process holding the
/// same lock for their own reasons. This is the row that matters: a browser
/// has dozens of threads and they all return from syscalls.
#[bench]
fn pull_shared_pending_while_others_use_the_process_lock(b: &mut Bencher) {
    let (proc, thread) = a_running_process(77_002);
    let lp = proc.linux();
    with_others_on_the_process_lock(b, &proc, || lp.pull_shared_pending_into(black_box(&thread)));
}

/// Question 3: the same lock again, for one `bool`.
#[bench]
fn is_job_stopped_on_a_running_process(b: &mut Bencher) {
    let (proc, _thread) = a_running_process(77_003);
    let lp = proc.linux();
    b.iter(|| black_box(lp.is_job_stopped()));
}

/// Question 3 with the process's other threads on the lock.
#[bench]
fn is_job_stopped_while_others_use_the_process_lock(b: &mut Bencher) {
    let (proc, _thread) = a_running_process(77_004);
    let lp = proc.linux();
    with_others_on_the_process_lock(b, &proc, || {
        black_box(lp.is_job_stopped());
    });
}

/// Question 2: the per-thread lock and the pending-signal check. Here for
/// scale -- this lock is the thread's own, so the threads of a process do
/// not share it, and it is not what the pairs above are about.
#[bench]
fn handle_signal_with_nothing_pending(b: &mut Bencher) {
    let (_proc, thread) = a_running_process(77_005);
    b.iter(|| black_box(thread.lock_linux().handle_signal()));
}

/// All three questions in the order `run_user` asks them: what a syscall
/// return pays today, with nobody else on the machine.
#[bench]
fn the_whole_prologue_of_a_return_to_user(b: &mut Bencher) {
    let (proc, thread) = a_running_process(77_006);
    let lp = proc.linux();
    b.iter(|| {
        lp.pull_shared_pending_into(black_box(&thread));
        black_box(thread.lock_linux().handle_signal());
        black_box(lp.is_job_stopped())
    });
}

/// The same, with the process's other threads doing ordinary work. The
/// difference against the row above is what the shared lock costs a
/// multithreaded program on every syscall it makes.
#[bench]
fn the_whole_prologue_while_others_use_the_process_lock(b: &mut Bencher) {
    let (proc, thread) = a_running_process(77_007);
    let lp = proc.linux();
    with_others_on_the_process_lock(b, &proc, || {
        lp.pull_shared_pending_into(black_box(&thread));
        black_box(thread.lock_linux().handle_signal());
        black_box(lp.is_job_stopped());
    });
}

/// The curve: the whole prologue with one, then two, other threads of the
/// process on its lock. One is the pair that decides -- two threads sharing
/// a lock is the smallest contention there is, and it is what a two-core
/// machine can produce.
#[bench]
fn the_whole_prologue_with_one_other_thread(b: &mut Bencher) {
    let (proc, thread) = a_running_process(77_009);
    let lp = proc.linux();
    with_n_others_on_the_process_lock(b, &proc, 1, || {
        lp.pull_shared_pending_into(black_box(&thread));
        black_box(thread.lock_linux().handle_signal());
        black_box(lp.is_job_stopped());
    });
}

/// The same with two.
#[bench]
fn the_whole_prologue_with_two_other_threads(b: &mut Bencher) {
    let (proc, thread) = a_running_process(77_010);
    let lp = proc.linux();
    with_n_others_on_the_process_lock(b, &proc, 2, || {
        lp.pull_shared_pending_into(black_box(&thread));
        black_box(thread.lock_linux().handle_signal());
        black_box(lp.is_job_stopped());
    });
}

/// The empty row of the same shape: a closure, two `black_box`es and a
/// clone-free borrow, measuring the harness. Any row that lands here is
/// doing nothing.
#[bench]
fn the_shaped_control(b: &mut Bencher) {
    let (proc, thread) = a_running_process(77_008);
    let lp = proc.linux();
    b.iter(|| {
        black_box(&thread);
        black_box(&lp);
    });
}

/// What is left: where the 11 ns of `handle_signal` go.
///
/// `lock_linux()` is not just a lock. It is `ext()` -> a `downcast_ref` on a
/// `Box<dyn Any>` (a vtable-word compare), then an integrity question about
/// the data word (`ext_drift().data_moved()`, which exists to catch the
/// corruption hunted in `[ext-glitch]`), and only then `Mutex::lock`. These
/// rows say which of the three the syscall return is paying for, so a
/// change aims at the right one.
mod the_thread_lock {
    use super::*;
    use kernel_hal::sync::Mutex;

    /// The whole of question 2, as the return path calls it.
    #[bench]
    fn handle_signal_through_the_thread_lock(b: &mut Bencher) {
        let (_proc, thread) = a_running_process(77_020);
        b.iter(|| black_box(thread.lock_linux().handle_signal()));
    }

    /// The body alone, with the guard taken once outside the loop. The
    /// difference against the row above is the whole of `lock_linux`.
    #[bench]
    fn handle_signal_with_the_lock_already_held(b: &mut Bencher) {
        let (_proc, thread) = a_running_process(77_021);
        let mut guard = thread.lock_linux();
        b.iter(|| black_box(guard.handle_signal()));
    }

    /// Taking and dropping the guard, and nothing else.
    #[bench]
    fn lock_linux_and_drop(b: &mut Bencher) {
        let (_proc, thread) = a_running_process(77_022);
        b.iter(|| {
            black_box(&*thread.lock_linux());
        });
    }

    /// The `downcast_ref` on its own: one vtable-word compare.
    #[bench]
    fn the_downcast_alone(b: &mut Bencher) {
        let (_proc, thread) = a_running_process(77_023);
        b.iter(|| {
            black_box(
                thread
                    .ext()
                    .downcast_ref::<Mutex<crate::thread::LinuxThread>>()
                    .is_some(),
            )
        });
    }

    /// The integrity question on its own: two word compares.
    #[bench]
    fn the_drift_check_alone(b: &mut Bencher) {
        let (_proc, thread) = a_running_process(77_024);
        b.iter(|| black_box(thread.ext_drift().data_moved()));
    }

    /// `Mutex::lock` on its own, on a mutex nobody else wants.
    #[bench]
    fn an_uncontended_mutex_lock(b: &mut Bencher) {
        let m = Mutex::new(0u64);
        b.iter(|| {
            black_box(*m.lock());
        });
    }
}

/// The whole prologue as `run_user` asks it **now**: the two process words
/// from the summary, and the thread's from the one published outside its
/// lock. Compare against `the_whole_prologue_of_a_return_to_user`, which
/// still asks question 2 the old way.
#[bench]
fn the_whole_prologue_as_the_return_path_asks_it(b: &mut Bencher) {
    let (proc, thread) = a_running_process(77_030);
    let lp = proc.linux();
    b.iter(|| {
        lp.pull_shared_pending_into(black_box(&thread));
        if thread.has_deliverable_signal() {
            black_box(thread.lock_linux().handle_signal());
        }
        black_box(lp.is_job_stopped())
    });
}

/// Question 2 alone, the lock-free way.
#[bench]
fn has_deliverable_signal_with_nothing_pending(b: &mut Bencher) {
    let (_proc, thread) = a_running_process(77_031);
    b.iter(|| black_box(thread.has_deliverable_signal()));
}

/// And with a signal actually pending. The reader itself is the same load
/// either way -- this row is here to say so, since a word that answered
/// faster when it was zero would be a branch predicted on the common case
/// rather than a measurement.
#[bench]
fn has_deliverable_signal_with_one_pending(b: &mut Bencher) {
    let (_proc, thread) = a_running_process(77_032);
    thread
        .lock_linux()
        .queue_signal(crate::signal::Signal::SIGWINCH, None);
    b.iter(|| black_box(thread.has_deliverable_signal()));
}
