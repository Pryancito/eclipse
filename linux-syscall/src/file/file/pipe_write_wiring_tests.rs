//! The two decisions about a pipe that the module next door settles with
//! pure helpers, run through the syscalls that have to act on them.
//!
//! `pipe_write_tests` says it in its own first line: "the two decisions
//! `write(2)` makes about a pipe or a unix socket that Linux makes and this
//! kernel did not: a blocking writer waits for room, and a writer with no
//! reader gets `SIGPIPE` with its `EPIPE`". Its six tests ask
//! `after_write_error` and `sigpipe_due` what the answer should be, and they
//! are right about all of it.
//!
//! What had never run is the half of each decision that lives in the
//! syscall: the arm that AWAITS room and comes back to write, and the line
//! that puts the signal in the thread's pending set. A kernel that answered
//! `EPIPE` and sent no signal, or that handed a blocking writer the `EAGAIN`
//! only a non-blocking one may see, passed every test in this crate.
//!
//! What this needs that the tree does not have ready: a pipe with NO ROOM
//! and a pipe with NO READER. An fd table can hold neither -- a pipe put in
//! one has both ends open and room to spare -- so these tests keep the ends
//! themselves: the write end goes in the table, and the read end stays here
//! to be shrunk, drained, or dropped.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use linux_object::fs::{File, OpenFlags, Pipe};
use linux_object::process::LinuxProcess;
use linux_object::signal::Signal;
use linux_object::thread::ThreadExt;
use rcore_fs::vfs::INode;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// One page, which is what `fcntl(F_SETPIPE_SZ)` rounds the smallest
/// request up to. A pipe this size counts as writable only when it is
/// empty: readiness means a whole `PIPE_BUF` fits, and `PIPE_BUF` IS the
/// capacity here. That is what makes one write fill it.
const A_SMALL_PIPE: usize = 4096;

/// Short enough to be atomic (`PIPE_BUF` or less), so a full pipe answers
/// "no room for this" instead of taking a few bytes. The waiting arm is only
/// reachable through an all-or-nothing write.
const A_SHORT_WRITE: usize = 100;

/// Enough to leave `A_SMALL_PIPE - A_HEAD_START` of room, so a gather write
/// bigger than `PIPE_BUF` takes what fits and no more.
const A_HEAD_START: usize = 1000;

/// A gather write that cannot fit whatever the pipe does, and is over
/// `PIPE_BUF`, so the pipe takes a prefix rather than refusing outright.
const MORE_THAN_FITS: usize = 8192;

/// `SYSCALL_IO_MAX`, the size of the bounded buffer a gather write copies
/// through, and the default pipe capacity -- the same 64 KiB. Written out
/// here rather than read from the constant under test.
const ONE_WHOLE_CHUNK: usize = 64 * 1024;

/// How long the draining thread leaves the writer alone before freeing the
/// room, and how many of those it waits for the go-ahead. Twenty times
/// longer than the microseconds the writer needs to reach the wait.
const A_MOMENT: Duration = Duration::from_millis(10);
const A_PAUSE: usize = 5;
const A_WAIT_BUDGET: usize = 200;

/// A process holding the WRITE end of a pipe, plus the read end, which stays
/// out of the fd table so these tests decide what the pipe does.
fn a_process_writing_into_a_pipe(
    pid: KoID,
    name: &str,
    how: OpenFlags,
) -> (Arc<Process>, i32, Arc<Pipe>) {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let (read, write) = Pipe::create_pair();
    let fd: i32 = lp
        .add_file(File::new(
            Arc::new(write),
            OpenFlags::WRONLY | how,
            String::from("pipe_w:[]"),
        ))
        .unwrap()
        .into();
    (
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap(),
        fd,
        Arc::new(read),
    )
}

/// The same, with the read end dropped: `read_cnt` falls to zero, which is
/// the state `pipe_write` answers `EPIPE` for.
fn a_process_writing_into_nothing(pid: KoID, name: &str) -> (Arc<Process>, i32) {
    let (proc, fd, read) = a_process_writing_into_a_pipe(pid, name, OpenFlags::empty());
    drop(read);
    (proc, fd)
}

/// A thread that frees the pipe's room once the writer is about to want it,
/// and answers with how many bytes it took out.
///
/// It pauses after the go-ahead rather than racing the writer to the pipe:
/// if it drained first there would be no wait to observe, and the test would
/// pass without the arm under test ever running. The proof that the arm DID
/// run is the mutation lot, not this clock -- a clock cannot witness its own
/// ordering.
fn a_drainer(read_end: Arc<Pipe>, go: Arc<AtomicBool>) -> std::thread::JoinHandle<usize> {
    std::thread::spawn(move || {
        for _ in 0..A_WAIT_BUDGET {
            if go.load(Ordering::SeqCst) {
                for _ in 0..A_PAUSE {
                    std::thread::sleep(A_MOMENT);
                }
                let mut sink = vec![0u8; ONE_WHOLE_CHUNK];
                return read_end.read_at(0, &mut sink).unwrap_or(0);
            }
            std::thread::sleep(A_MOMENT);
        }
        0
    })
}

/// `struct iovec` as userspace lays it out, the same two words
/// `positional_io_tests` builds (and holds against the kernel's layout in
/// `the_iovec_this_module_builds_is_the_one_the_kernel_reads`): what the
/// syscall reads is the memory, and this is that memory.
#[repr(C)]
struct AnIoVec {
    base: usize,
    len: usize,
}

fn an_iovec(buf: &[u8]) -> AnIoVec {
    AnIoVec {
        base: buf.as_ptr() as usize,
        len: buf.len(),
    }
}

/// Whether this thread is holding a pending `SIGPIPE`, which is the whole
/// observable effect of `raise_sigpipe_if_due`.
fn carries_a_sigpipe(sys: &Syscall<'_>) -> bool {
    sys.thread.lock_linux().signals.contains(Signal::SIGPIPE)
}

#[test]
fn a_blocking_writer_on_a_full_pipe_waits_for_room_and_then_writes() {
    // `pipe_write` never hands a blocking writer `EAGAIN`; this kernel's
    // pipe does return it, and the syscall is what turns it into a wait.
    // Without that arm a blocking `write(2)` to a full pipe answers EAGAIN
    // -- an errno a blocking descriptor may never see -- and the caller,
    // which has no reason to retry, loses the bytes.
    let (proc, fd, read_end) =
        a_process_writing_into_a_pipe(49_360, "pipe-full", OpenFlags::empty());
    read_end.set_capacity(A_SMALL_PIPE).unwrap();
    let go = Arc::new(AtomicBool::new(false));
    let drainer = a_drainer(read_end.clone(), go.clone());
    let filler = vec![b'f'; A_SMALL_PIPE];
    let latecomer = vec![b'l'; A_SHORT_WRITE];
    let (filled, waited) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(fd);
            let filled = sys
                .sys_write(fd, UserInPtr::from(filler.as_ptr() as usize), A_SMALL_PIPE)
                .await;
            go.store(true, Ordering::SeqCst);
            let waited = sys
                .sys_write(
                    fd,
                    UserInPtr::from(latecomer.as_ptr() as usize),
                    A_SHORT_WRITE,
                )
                .await;
            (filled, waited)
        })
    });
    let drained = drainer.join().unwrap();
    assert_eq!(filled, Ok(A_SMALL_PIPE), "the pipe did not take a pageful");
    assert_eq!(
        waited,
        Ok(A_SHORT_WRITE),
        "a blocking writer gave up instead of waiting for room"
    );
    assert_eq!(drained, A_SMALL_PIPE, "the drainer never read the pipe");
    assert_eq!(
        read_end.buffered_len(),
        A_SHORT_WRITE,
        "the bytes written after the wait are not in the pipe"
    );
}

#[test]
fn a_writer_with_no_reader_gets_the_signal_with_its_errno() {
    // The errno alone is not the contract: `yes | head -1` ran `yes` for
    // ever because the EPIPE came back with no signal behind it, and the
    // default action of SIGPIPE is what ends such a writer.
    let (gone, fd_gone) = a_process_writing_into_nothing(49_361, "pipe-no-reader");
    let (refused, before, after) = on_a_thread(&gone, move |sys| {
        Box::pin(async move {
            let bytes = vec![b'g'; A_SHORT_WRITE];
            let before = carries_a_sigpipe(sys);
            let refused = sys
                .sys_write(
                    FileDesc::from(fd_gone),
                    UserInPtr::from(bytes.as_ptr() as usize),
                    A_SHORT_WRITE,
                )
                .await;
            (refused, before, carries_a_sigpipe(sys))
        })
    });
    assert_eq!(
        refused,
        Err(LxError::EPIPE),
        "a pipe with no reader took it"
    );
    assert!(!before, "the thread started out holding a SIGPIPE");
    assert!(after, "EPIPE came back with no SIGPIPE behind it");

    // The control: the same write, on a pipe whose reader is still there,
    // must leave nothing pending. Without it a kernel that signalled every
    // writer would pass the assertions above.
    let (live, fd_live, _read_end) =
        a_process_writing_into_a_pipe(49_362, "pipe-reader-there", OpenFlags::empty());
    let (taken, signalled) = on_a_thread(&live, move |sys| {
        Box::pin(async move {
            let bytes = vec![b'l'; A_SHORT_WRITE];
            let taken = sys
                .sys_write(
                    FileDesc::from(fd_live),
                    UserInPtr::from(bytes.as_ptr() as usize),
                    A_SHORT_WRITE,
                )
                .await;
            (taken, carries_a_sigpipe(sys))
        })
    });
    assert_eq!(taken, Ok(A_SHORT_WRITE), "a pipe with a reader refused it");
    assert!(!signalled, "a write that succeeded raised SIGPIPE anyway");
}

#[test]
fn a_non_blocking_gather_write_reports_what_fit_and_stops_there() {
    // POSIX's short write, on the call that gathers: the pipe takes the
    // prefix that fits (a write over `PIPE_BUF` is not atomic) and a
    // non-blocking caller is told how much went in, not that it failed.
    // Reporting the error instead would make it resend bytes the pipe
    // already holds.
    let (proc, fd, read_end) =
        a_process_writing_into_a_pipe(49_363, "pipe-gather-nonblock", OpenFlags::NON_BLOCK);
    read_end.set_capacity(A_SMALL_PIPE).unwrap();
    let room_left = A_SMALL_PIPE - A_HEAD_START;
    let (head_start, gathered) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(fd);
            let head = vec![b'h'; A_HEAD_START];
            let head_start = sys
                .sys_write(fd, UserInPtr::from(head.as_ptr() as usize), A_HEAD_START)
                .await;
            let rest = vec![b'r'; MORE_THAN_FITS];
            let set = [an_iovec(&rest)];
            let gathered = sys
                .sys_writev(fd, UserInPtr::from(set.as_ptr() as usize), 1)
                .await;
            (head_start, gathered)
        })
    });
    assert_eq!(head_start, Ok(A_HEAD_START), "the head start did not go in");
    assert_eq!(
        gathered,
        Ok(room_left),
        "a non-blocking gather write did not report the bytes that fit"
    );
    assert_eq!(
        read_end.buffered_len(),
        A_SMALL_PIPE,
        "the pipe is not full, so the write stopped early for another reason"
    );
}

#[test]
fn a_blocking_gather_write_waits_for_room_instead_of_stopping_short() {
    // The same pipe and the same gather write, with the one difference that
    // decides it: no `O_NONBLOCK`. A blocking caller asked for every byte
    // and gets every byte, however many waits that takes.
    let (proc, fd, read_end) =
        a_process_writing_into_a_pipe(49_364, "pipe-gather-block", OpenFlags::empty());
    read_end.set_capacity(A_SMALL_PIPE).unwrap();
    let go = Arc::new(AtomicBool::new(false));
    let drainer = a_drainer(read_end.clone(), go.clone());
    let (filled, gathered) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(fd);
            let filler = vec![b'f'; A_SMALL_PIPE];
            let filled = sys
                .sys_write(fd, UserInPtr::from(filler.as_ptr() as usize), A_SMALL_PIPE)
                .await;
            let rest = vec![b'r'; A_SHORT_WRITE];
            let set = [an_iovec(&rest)];
            go.store(true, Ordering::SeqCst);
            let gathered = sys
                .sys_writev(fd, UserInPtr::from(set.as_ptr() as usize), 1)
                .await;
            (filled, gathered)
        })
    });
    let drained = drainer.join().unwrap();
    assert_eq!(filled, Ok(A_SMALL_PIPE), "the pipe did not take a pageful");
    assert_eq!(
        gathered,
        Ok(A_SHORT_WRITE),
        "a blocking gather write stopped short instead of waiting"
    );
    assert_eq!(drained, A_SMALL_PIPE, "the drainer never read the pipe");
}

#[test]
fn a_gather_write_past_one_chunk_reports_a_partial_count_and_not_the_error() {
    // Two trips through the bounded buffer, which is what a gather write
    // bigger than `SYSCALL_IO_MAX` takes -- and what xcb sends when it
    // flushes a big X request. The first chunk fills a default-sized pipe
    // exactly; the second cannot start at all. A caller that has already
    // had 64 KiB taken must be told so: handing it the EAGAIN instead says
    // nothing went in, and xcb would resend a request the pipe is holding.
    let (proc, fd, read_end) =
        a_process_writing_into_a_pipe(49_365, "pipe-gather-partial", OpenFlags::NON_BLOCK);
    let gathered = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let first = vec![b'1'; ONE_WHOLE_CHUNK];
            let second = vec![b'2'; ONE_WHOLE_CHUNK];
            let set = [an_iovec(&first), an_iovec(&second)];
            sys.sys_writev(
                FileDesc::from(fd),
                UserInPtr::from(set.as_ptr() as usize),
                2,
            )
            .await
        })
    });
    assert_eq!(
        gathered,
        Ok(ONE_WHOLE_CHUNK),
        "a gather write whose second chunk could not start did not report the first"
    );
    assert_eq!(
        read_end.buffered_len(),
        ONE_WHOLE_CHUNK,
        "the count and what the pipe holds disagree"
    );
}

#[test]
fn a_gather_write_with_no_reader_gets_the_signal_too() {
    // Linux sends it from `unix_stream_sendmsg` and `pipe_write` alike, so
    // it cannot depend on which of the two calls asked. Nothing before this
    // asked `writev` for it.
    let (proc, fd) = a_process_writing_into_nothing(49_366, "pipe-gather-no-reader");
    let (refused, signalled) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let bytes = vec![b'g'; A_SHORT_WRITE];
            let set = [an_iovec(&bytes)];
            let refused = sys
                .sys_writev(
                    FileDesc::from(fd),
                    UserInPtr::from(set.as_ptr() as usize),
                    1,
                )
                .await;
            (refused, carries_a_sigpipe(sys))
        })
    });
    assert_eq!(
        refused,
        Err(LxError::EPIPE),
        "a gather write with no reader was taken"
    );
    assert!(signalled, "writev answered EPIPE with no SIGPIPE behind it");
}

/// Enough to be over `PIPE_BUF`, so the pipe takes the prefix that fits
/// instead of refusing the write outright, and more than
/// `A_SMALL_PIPE - A_HEAD_START` so what fits is only a prefix.
const MORE_THAN_THE_ROOM: usize = 5000;

#[test]
fn a_blocking_gather_write_carries_on_after_a_chunk_the_pipe_only_half_took() {
    // The sibling above never reaches the line this one is about: a pipe
    // with NO room answers "no room for this" and the write goes straight to
    // the wait, so `w < n` never happens. Here the pipe has SOME room, the
    // first write takes a prefix, and the question is what the caller is
    // told: a blocking caller asked for every byte, so a half-taken chunk is
    // a reason to wait for the rest, not to stop and report a short write.
    //
    // Found by the mutation lot, not by reading: dropping `&& !waits` from
    // that line survived every test in the batch until this one existed.
    let (proc, fd, read_end) =
        a_process_writing_into_a_pipe(49_367, "pipe-gather-half", OpenFlags::empty());
    read_end.set_capacity(A_SMALL_PIPE).unwrap();
    let go = Arc::new(AtomicBool::new(false));
    let drainer = a_drainer(read_end.clone(), go.clone());
    let (head_start, gathered) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(fd);
            let head = vec![b'h'; A_HEAD_START];
            let head_start = sys
                .sys_write(fd, UserInPtr::from(head.as_ptr() as usize), A_HEAD_START)
                .await;
            let rest = vec![b'r'; MORE_THAN_THE_ROOM];
            let set = [an_iovec(&rest)];
            go.store(true, Ordering::SeqCst);
            let gathered = sys
                .sys_writev(fd, UserInPtr::from(set.as_ptr() as usize), 1)
                .await;
            (head_start, gathered)
        })
    });
    let drained = drainer.join().unwrap();
    assert_eq!(head_start, Ok(A_HEAD_START), "the head start did not go in");
    assert_eq!(
        gathered,
        Ok(MORE_THAN_THE_ROOM),
        "a blocking gather write reported a short count instead of waiting \
         for the rest of a chunk the pipe only half took"
    );
    assert_eq!(drained, A_SMALL_PIPE, "the drainer never filled its sink");
    assert_eq!(
        read_end.buffered_len(),
        A_HEAD_START + MORE_THAN_THE_ROOM - A_SMALL_PIPE,
        "what the pipe holds does not match what went in minus what came out"
    );
}
