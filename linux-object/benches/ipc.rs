//! Reference benchmarks for the blocking IPC path: what a pipe round trip
//! costs in *software*, with no scheduler, no context switch and no guest.
//!
//! The C suite's `SCHEDULER / IPC` section reports a pipe round trip as one
//! number -- 186 us between two processes on real hardware against Linux's 6 --
//! and that number contains everything: two syscall entries, two wakes, two
//! context switches, the emulator or the machine underneath. It cannot say how
//! much of it is this crate deciding what to do. These rows can, and the
//! answer decides where the work goes: if parking and waking a blocked reader
//! is a few hundred nanoseconds here, then the 186 us is the scheduler and the
//! switch, and nothing in `fs/pipe.rs` or `fs/file.rs` would move it; if it is
//! tens of microseconds, the opposite.
//!
//! The shape is a ladder, each row adding one layer over the one above, so the
//! reading is the DIFFERENCES and not the absolutes:
//!
//! 1. `pipe_read_at_with_data` / `pipe_write_at_one_byte` -- the queue itself,
//!    `INode` level: a lock, a `memcpy` of one byte, the event bus publish.
//!    This is the floor, and the control row for everything below.
//! 2. `file_read_with_data` -- the same read through `FileLike`, which is what
//!    `sys_read` actually calls: an `#[async_trait]` boxed future per call,
//!    the descriptor's offset bookkeeping, the readiness check.
//! 3. `pipe_async_poll_park_and_drop` -- what a read that finds the pipe empty
//!    pays before it sleeps: `INode::async_poll`'s `Box::pin`, the bus
//!    subscription, and the unsubscribe its `Drop` owes.
//! 4. `file_read_park_then_wake` -- the whole software half of a round trip:
//!    poll the blocking read until it parks, write the byte from the other
//!    end (which fires the waker), poll again until it answers. Two of these
//!    are one round trip, so the C suite's figure minus twice this row is what
//!    the scheduler and the switch cost.
//!
//! What is deliberately NOT in row 4, because the host cannot have it: the
//! `process::interruptible` wrapper `File::read` puts around the park. With no
//! Linux thread on the CPU it awaits the inner future directly, so the
//! `SignalPark` -- two `Arc` clones, a `signal_clear`, a `wait_signal`
//! subscription and a `select` between two futures, per pass -- is skipped.
//! Row 4 is therefore a LOWER BOUND on the real software cost: on the machine
//! that wrapper is built, polled and dropped once per blocked read on top of
//! everything these rows do measure.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the reasons
//! given in `zircon-object/benches/vm.rs`.
//!
//! Every row passes its INPUTS through `test::black_box` as well as its
//! result: these are short paths over values the compiler can see through, and
//! black-boxing only the result lets it fold the work away and leaves the row
//! reporting the loop. A sub-nanosecond figure here is that mistake, not
//! speed.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p linux-object --bench ipc --features mock-disk
//! ```

#![feature(test)]

extern crate test;

use linux_object::error::LxError;
use linux_object::fs::{File, FileLike, OpenFlags, Pipe};
use rcore_fs::vfs::INode;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use test::{black_box, Bencher};

/// A waker that only counts, so a row measures the wake PUBLICATION and not
/// whatever a real executor would do with it. The count is read by the rows
/// that need to prove a wake actually happened -- a park that is never woken
/// would otherwise measure a much cheaper nothing.
struct CountingWaker(AtomicUsize);

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn counting_waker() -> (Arc<CountingWaker>, Waker) {
    let arc = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let waker = Waker::from(arc.clone());
    (arc, waker)
}

/// A read/write pipe pair at the `INode` level.
fn raw_pair() -> (Pipe, Pipe) {
    Pipe::create_pair()
}

/// The same pair behind the two descriptors `sys_read`/`sys_write` would see.
/// `RDONLY`/`WRONLY` without `O_NONBLOCK`: the blocking path is the subject.
fn file_pair() -> (Arc<File>, Arc<File>) {
    let (r, w) = Pipe::create_pair();
    (
        File::new(Arc::new(r), OpenFlags::RDONLY, String::from("pipe:[r]")),
        File::new(Arc::new(w), OpenFlags::WRONLY, String::from("pipe:[w]")),
    )
}

// ── 1. the queue itself ─────────────────────────────────────────────────────

/// One byte out of a pipe that has one: the lock, the copy, the drain.
#[bench]
fn pipe_read_at_with_data(b: &mut Bencher) {
    let (r, w) = raw_pair();
    let mut buf = [0u8; 1];
    b.iter(|| {
        w.write_at(0, black_box(&[0xa5u8])).unwrap();
        let n = r.read_at(0, black_box(&mut buf)).unwrap();
        black_box(n)
    });
}

/// One byte into a pipe with room, including the `READABLE` publish and the
/// room republish that a parked writer would be woken by. The read half of
/// the pair above is the same two calls; this row isolates the write so the
/// publish cost is attributable.
#[bench]
fn pipe_write_at_one_byte(b: &mut Bencher) {
    let (r, w) = raw_pair();
    let mut sink = [0u8; 64];
    b.iter(|| {
        for _ in 0..64 {
            w.write_at(0, black_box(&[0xa5u8])).unwrap();
        }
        // Asserted, not assumed: a short read would leave bytes behind every
        // iteration, and the row would quietly stop being "a write into a pipe
        // with room" and become a measurement of back-pressure.
        let n = r.read_at(0, &mut sink).unwrap();
        assert_eq!(
            n, 64,
            "the sink is as big as the burst, so one read drains it"
        );
        black_box(n)
    });
}

/// The control for every row below: an empty `b.iter` body that only moves
/// the same byte through `black_box`. A row that is not clear of this floor
/// is measuring the harness.
#[bench]
fn harness_floor(b: &mut Bencher) {
    let byte = 0xa5u8;
    b.iter(|| black_box(byte));
}

// ── 2. the same read through `FileLike`, which is what `sys_read` calls ─────

/// `FileLike::read` on a descriptor whose pipe already has the byte: the
/// `#[async_trait]` box, the offset bookkeeping, the readiness check. The
/// future completes on its first poll, so nothing parks.
#[bench]
fn file_read_with_data(b: &mut Bencher) {
    let (reader, writer) = file_pair();
    let (_count, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    let mut buf = [0u8; 1];
    b.iter(|| {
        FileLike::write(&*writer, black_box(&[0xa5u8])).unwrap();
        let mut fut = FileLike::read(&*reader, black_box(&mut buf));
        let out = match Pin::new(&mut fut).poll(&mut cx) {
            Poll::Ready(r) => r.unwrap(),
            Poll::Pending => unreachable!("the byte is already queued"),
        };
        black_box(out)
    });
}

// ── 3. what a read pays to go to sleep ──────────────────────────────────────

/// `INode::async_poll` on an empty pipe: the `Box::pin`, the subscription on
/// the pipe's event bus, and the unsubscribe its `Drop` owes. Every blocking
/// read that finds the pipe empty pays this before it sleeps, and
/// `poll`/`epoll` pay it per fd per rescan.
#[bench]
fn pipe_async_poll_park_and_drop(b: &mut Bencher) {
    let (r, _w) = raw_pair();
    let (_count, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    b.iter(|| {
        let mut fut = black_box(&r).async_poll();
        let parked = matches!(fut.as_mut().poll(&mut cx), Poll::Pending);
        drop(fut);
        black_box(parked)
    });
}

/// The wake publication alone, with one waiter parked: the write that fires
/// `READABLE` into a bus that has a subscriber to retire. Read next to
/// `pipe_write_at_one_byte`, whose bus has nobody on it: the difference is
/// what a waiting reader adds to the writer's side.
#[bench]
fn pipe_write_waking_one_parked_reader(b: &mut Bencher) {
    let (r, w) = raw_pair();
    let (count, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    let mut buf = [0u8; 1];
    b.iter(|| {
        let mut fut = r.async_poll();
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        w.write_at(0, black_box(&[0xa5u8])).unwrap();
        drop(fut);
        // The byte has to leave again, or the queue fills and the row turns
        // into a measurement of a full pipe.
        black_box(r.read_at(0, &mut buf).unwrap())
    });
    assert!(
        count.0.load(Ordering::Relaxed) > 0,
        "the write must have woken the parked reader"
    );
}

// ── 4. the whole software half of a round trip ──────────────────────────────

/// Park a blocking `FileLike::read` on an empty pipe, write the byte from the
/// other end, and poll the read through to its answer. This is one half of a
/// round trip with the scheduler and the switch taken out: everything
/// `fs/file.rs` and `fs/pipe.rs` do between "the reader asked and there was
/// nothing" and "the reader has its byte".
///
/// Twice this row is what a round trip costs in software. The C suite's
/// `pipe round trip (2 thrds)` minus that is the scheduler's share.
#[bench]
fn file_read_park_then_wake(b: &mut Bencher) {
    let (reader, writer) = file_pair();
    let (count, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    let before = count.0.load(Ordering::Relaxed);
    b.iter(|| {
        let mut buf = [0u8; 1];
        let mut fut = FileLike::read(&*reader, black_box(&mut buf));
        // First poll: the pipe is empty, so this parks on its event bus.
        assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
        FileLike::write(&*writer, black_box(&[0xa5u8])).unwrap();
        // Second poll: the byte is there and the read completes. A real
        // executor would have been handed the wake and would arrive here on
        // another CPU; the difference between the two is the switch, which is
        // what this row exists to leave out.
        let out = match Pin::new(&mut fut).poll(&mut cx) {
            Poll::Ready(r) => r.unwrap(),
            Poll::Pending => unreachable!("the byte was just written"),
        };
        black_box(out)
    });
    assert!(
        count.0.load(Ordering::Relaxed) > before,
        "every pass must have parked and been woken"
    );
}

/// The same half round trip on a descriptor that is `O_NONBLOCK`, driven the
/// way an event loop drives it: a read that answers `EAGAIN`, then a read
/// that finds the byte. No parking, no subscription, no boxed `async_poll`.
///
/// This is the control row for `file_read_park_then_wake`: both move one byte
/// through the same pipe and the same descriptor layer, and the difference is
/// exactly what going to sleep and being woken costs in software.
#[bench]
fn file_read_nonblocking_eagain_then_data(b: &mut Bencher) {
    let (r, w) = Pipe::create_pair();
    let reader = File::new(
        Arc::new(r),
        OpenFlags::RDONLY | OpenFlags::NON_BLOCK,
        String::from("pipe:[r]"),
    );
    let writer = File::new(Arc::new(w), OpenFlags::WRONLY, String::from("pipe:[w]"));
    let (_count, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    b.iter(|| {
        let mut buf = [0u8; 1];
        let mut again = FileLike::read(&*reader, black_box(&mut buf));
        let answer = match Pin::new(&mut again).poll(&mut cx) {
            Poll::Ready(r) => r,
            Poll::Pending => unreachable!("a non-blocking read never parks"),
        };
        // The specific error, not merely "an error": any other failure here
        // would take a different path out of `File::read` and this row is the
        // control the parking rows are compared against.
        assert_eq!(
            answer,
            Err(LxError::EAGAIN),
            "an empty non-blocking pipe must answer EAGAIN"
        );
        drop(again);
        FileLike::write(&*writer, black_box(&[0xa5u8])).unwrap();
        let mut fut = FileLike::read(&*reader, black_box(&mut buf));
        let out = match Pin::new(&mut fut).poll(&mut cx) {
            Poll::Ready(r) => r.unwrap(),
            Poll::Pending => unreachable!("the byte was just written"),
        };
        black_box(out)
    });
}

// ── 5. the wrapper the host can only have with a thread on the CPU ──────────
//
// `File::read` does not park on `INode::async_poll` directly: it parks on
// `process::interruptible(..)`, which adds, per pass, a `SignalPark` (two
// `Arc` clones), a `signal_clear` on the thread object (a lock and a walk of
// its callback list), a `check_signals_of` (the thread's state, the process
// status, the shared-pending pull and the per-thread signal mask -- four more
// locks, one of them the process's), a `wait_signal` subscription (an `Arc`, a
// boxed closure pushed onto the thread object) and a `select` between the two
// futures. With no Linux thread on the CPU all of that is skipped, which is
// why the rows above are a lower bound.
//
// So these rows put a thread on the CPU. They are the difference between
// Eclipse's pipe and its AF_UNIX socketpair, which parks on its own future
// with none of this around it -- and the C suite reports the socketpair round
// trip at 54 us against the pipe's 163.

/// A Linux process with one Linux thread, registered as the thread running on
/// this CPU, exactly as `run_user` leaves it.
///
/// On the host the HAL keeps "the thread on this CPU" in an async-std task
/// local, so the registration -- and therefore every row that needs it -- only
/// works inside a task. That is what [`with_current_linux_thread`] is for:
/// `block_on` ONCE around the whole `b.iter`, never per iteration, or the row
/// would be measuring the executor it was driven by.
fn a_current_linux_thread() -> Arc<zircon_object::task::Thread> {
    use linux_object::process::LinuxProcess;
    use linux_object::thread::ThreadExt;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::{Process, Thread, ROOT_JOB};

    // Pids are a global table; this file takes its own range so a parallel
    // bench target cannot collide with it.
    static NEXT_PID: AtomicUsize = AtomicUsize::new(960_000);
    let job = ROOT_JOB.create_child().unwrap();
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed) as u64;
    let proc =
        Process::create_with_fixed_id_ext(&job, pid, "bench", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    kernel_hal::thread::set_current_thread(Some(thread.clone()));
    thread
}

/// Run `body` with a Linux thread registered on this CPU, inside the one task
/// context the host HAL's registration needs.
fn with_current_linux_thread<R>(body: impl FnOnce(&Arc<zircon_object::task::Thread>) -> R) -> R {
    async_std::task::block_on(async move {
        let thread = a_current_linux_thread();
        let out = body(&thread);
        // Hand the registration back. On the host it lives in an async-std
        // task local, so this task's copy dies with the task -- but a row that
        // leaves a thread registered reads as deliberate to the next person,
        // and the rows that must run WITHOUT one would then be measuring
        // something else entirely.
        kernel_hal::thread::set_current_thread(None);
        out
    })
}

/// One pass of the interruptible park's bookkeeping, without the park: the
/// `SignalPark`, the `prepare` that clears the wake bits on the thread
/// object, and the `check_signals_of` that decides whether a signal beat the
/// condition. A blocked read runs this at least twice -- once on the way to
/// sleep, once on the way back.
#[bench]
fn signal_park_prepare_and_check(b: &mut Bencher) {
    use linux_object::process::{check_signals_of, SignalPark};
    with_current_linux_thread(|thread| {
        let thread = thread.clone();
        b.iter(move || {
            let park = SignalPark::new(black_box(&thread));
            park.prepare();
            black_box(check_signals_of(black_box(&thread)).is_ok())
        })
    });
}

/// `process::interruptible` around a future that is ready on its first poll:
/// what every *non-blocking* trip through the wrapper costs. Nothing parks,
/// so this is the floor of the wrapper and the control row for the one below.
#[bench]
fn interruptible_over_a_ready_future(b: &mut Bencher) {
    with_current_linux_thread(|_thread| {
        let (_count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        b.iter(|| {
            let mut fut = Box::pin(linux_object::process::interruptible(async {
                black_box(1usize)
            }));
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(out) => black_box(out.unwrap()),
                Poll::Pending => unreachable!("the inner future is ready at once"),
            }
        })
    });
}

/// `file_read_park_then_wake` with a Linux thread on the CPU, so the real
/// wrapper is in the path: the whole software half of a pipe round trip as
/// the machine runs it. The difference against `file_read_park_then_wake` is
/// what the signal wrapper costs a blocking read, and the difference against
/// the C suite's `pipe round trip (2 thrds)` is the scheduler's share.
#[bench]
fn file_read_park_then_wake_with_thread(b: &mut Bencher) {
    with_current_linux_thread(|_thread| {
        let (reader, writer) = file_pair();
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let before = count.0.load(Ordering::Relaxed);
        b.iter(|| {
            let mut buf = [0u8; 1];
            let mut fut = FileLike::read(&*reader, black_box(&mut buf));
            assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
            FileLike::write(&*writer, black_box(&[0xa5u8])).unwrap();
            let out = match Pin::new(&mut fut).poll(&mut cx) {
                Poll::Ready(r) => r.unwrap(),
                Poll::Pending => unreachable!("the byte was just written"),
            };
            black_box(out)
        });
        assert!(
            count.0.load(Ordering::Relaxed) > before,
            "every pass must have parked and been woken"
        );
    });
}
