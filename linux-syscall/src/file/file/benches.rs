//! `read` and `write` from the inside, and the argument work of
//! `lseek`, `fcntl`, `fallocate`, `utimensat` and `utimes`.
//!
//! The pipe rows are what `read(2)` and `write(2)` cost once the
//! descriptor is resolved and the bytes are in the kernel: the ring
//! copy and the lock around it, without the user copy or the syscall
//! entry. 64 B / 4 KiB / 64 KiB / 256 KiB is a family -- the slope is
//! the copy, and the intercept is what a call costs however small it
//! is, which is the number that decides whether buffering in libc is
//! worth it.
//!
//! The ring is `PIPE_DEFAULT_CAPACITY`, which is **64 KiB**;
//! `PIPE_BUF` (4096) is only the length POSIX promises to write
//! atomically and is not the ring's size. So the 64 B, 4 KiB and
//! 64 KiB rows are each exactly one write/read pair, and only the
//! 256 KiB row is several ring loads. Every row black-boxes its
//! inputs.

use super::*;
use linux_object::fs::{Pipe, PIPE_BUF, PIPE_DEFAULT_CAPACITY};
use rcore_fs::vfs::INode;
use test::{black_box, Bencher};

/// A pipe's two ends, as the ring itself. `File::read` is `async` and
/// awaits exactly these calls, so this is the work without the executor
/// around it -- putting `block_on` in the loop would measure async-std.
fn pipe_pair() -> (Arc<Pipe>, Arc<Pipe>) {
    let (r, w) = Pipe::create_pair();
    (Arc::new(r), Arc::new(w))
}

/// One `write` of `n` bytes into a pipe and one `read` of them back out:
/// a request and its reply, as a pair of processes trade them. Both
/// halves are in the row because a write alone would fill the ring and
/// then do nothing but refuse.
fn pipe_round_trip(b: &mut Bencher, n: usize) {
    let (r, w) = pipe_pair();
    let out = alloc::vec![0x5au8; n];
    let mut back = alloc::vec![0u8; n];
    b.iter(|| {
        let mut done = 0;
        // A piece at a time, because the ring is smaller than the
        // largest row: this is the loop a blocking `write(2)` runs,
        // minus the parking.
        while done < n {
            let piece = (n - done).min(PIPE_DEFAULT_CAPACITY);
            let wrote = w.write_at(0, black_box(&out[done..done + piece])).unwrap();
            let got = r
                .read_at(0, black_box(&mut back[done..done + wrote]))
                .unwrap();
            done += got;
        }
        black_box(done)
    });
}

/// 64 bytes: a line of a log, or one message of a protocol.
#[bench]
fn pipe_round_trip_of_64b(b: &mut Bencher) {
    pipe_round_trip(b, 64);
}

/// 4 KiB: `PIPE_BUF`, the atomic-write promise, and what a shell
/// pipeline moves per read. Still one pair: the ring is sixteen times
/// this.
#[bench]
fn pipe_round_trip_of_4kib(b: &mut Bencher) {
    pipe_round_trip(b, PIPE_BUF);
}

/// 64 KiB: the ring filled exactly once, which is the largest single
/// write a pipe takes without the writer having to wait for a reader.
#[bench]
fn pipe_round_trip_of_64kib(b: &mut Bencher) {
    pipe_round_trip(b, PIPE_DEFAULT_CAPACITY);
}

/// 256 KiB: four ring loads, so the loop runs four times and the copy
/// should dominate while the per-call cost disappears. This is the only
/// row above that is more than one write/read pair.
#[bench]
fn pipe_round_trip_of_256kib(b: &mut Bencher) {
    pipe_round_trip(b, 4 * PIPE_DEFAULT_CAPACITY);
}

/// A `read` on an empty pipe whose writer is still open: the answer that
/// sends `read(2)` to sleep, and that a non-blocking caller gets as
/// `EAGAIN`. This is the floor every row above pays twice.
#[bench]
fn pipe_read_of_an_empty_ring(b: &mut Bencher) {
    let (r, _w) = pipe_pair();
    let mut back = [0u8; 64];
    b.iter(|| black_box(r.read_at(0, black_box(&mut back))).is_err());
}

/// How much is queued, which `poll`, `select` and `FIONREAD` each ask.
#[bench]
fn pipe_buffered_len(b: &mut Bencher) {
    let (r, w) = pipe_pair();
    w.write_at(0, &[0u8; 128]).unwrap();
    b.iter(|| black_box(black_box(&r).buffered_len()));
}

/// `lseek`'s `whence` and `offset`, turned into a `SeekFrom`.
#[bench]
fn lseek_seek_from(b: &mut Bencher) {
    b.iter(|| black_box(seek_from(black_box(0), black_box(4096))));
}

/// `fcntl(F_SETPIPE_SZ)`: rounded up to a power of two and bounded.
#[bench]
fn fcntl_pipe_size_round(b: &mut Bencher) {
    b.iter(|| black_box(pipe_size_round(black_box(5000))));
}

/// `fallocate`'s `mode`, which is a small set of legal combinations.
#[bench]
fn fallocate_mode(b: &mut Bencher) {
    b.iter(|| black_box(fallocate_op(black_box(0))));
}

/// `posix_fadvise`'s `advice`.
#[bench]
fn fadvise_advice(b: &mut Bencher) {
    b.iter(|| black_box(fadvise_advice_known(black_box(1))));
}

/// `copy_file_range`'s `flags`, which must be zero.
#[bench]
fn copy_file_range_flag_check(b: &mut Bencher) {
    b.iter(|| black_box(copy_file_range_flags(black_box(0))));
}

/// `utimensat` with two explicit times: the `UTIME_NOW`/`UTIME_OMIT`
/// decision, per call.
#[bench]
fn utimensat_explicit_times(b: &mut Bencher) {
    let now = TimeSpec { sec: 1000, nsec: 5 };
    let times = [
        TimeSpec {
            sec: 1_700_000_000,
            nsec: 1,
        },
        TimeSpec {
            sec: 1_700_000_001,
            nsec: 2,
        },
    ];
    b.iter(|| black_box(utimensat_times(black_box(Some(times)), black_box(now))));
}

/// `utimensat(fd, path, NULL, 0)`: the touch, which is the common call.
#[bench]
fn utimensat_touch(b: &mut Bencher) {
    let now = TimeSpec { sec: 1000, nsec: 5 };
    b.iter(|| black_box(utimensat_times(black_box(None), black_box(now))));
}

/// `write`'s decision after the file returned an error, which runs on
/// every short write.
#[bench]
fn write_error_decision(b: &mut Bencher) {
    b.iter(|| {
        black_box(after_write_error(
            black_box(LxError::EAGAIN),
            black_box(true),
            black_box(0),
        ))
    });
}

/// And whether that error owes the caller a `SIGPIPE`.
#[bench]
fn sigpipe_decision(b: &mut Bencher) {
    b.iter(|| black_box(sigpipe_due(black_box(LxError::EPIPE), black_box(true))));
}
