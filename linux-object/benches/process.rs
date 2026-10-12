//! Reference benchmarks for the process object's own bookkeeping: the file
//! descriptor table, and the two range sweeps `close_range(2)` exposes.
//!
//! `benches/vfs.rs` measures the path a descriptor is *looked up* through
//! (`get_file`) and the namespace walk behind an `open`. What it does not
//! measure is the decision that comes first: which NUMBER the new descriptor
//! gets. That is `get_free_fd_from`, and it runs on every `open`, `dup`,
//! `pipe`, `socket` and `accept` a process ever makes -- one of the hottest
//! paths this crate owns, and until now without a single row.
//!
//! The rows are laid out as a flat family over table size, which is the only
//! shape that can answer the question that matters here: whether a process
//! pays for the descriptors it already holds every time it opens one more. A
//! lookup does not (`get_file` is a hash hit, and `benches/vfs.rs` shows it
//! flat from 1 to 256); an allocation might, because the lowest free number
//! has to be FOUND. A family that is flat across the sizes says the search is
//! not walking the table; one that grows with the size says a server with ten
//! thousand sockets open pays ten thousand times over on the next `accept`,
//! and that the cost of filling a table is quadratic in its size.
//!
//! Every family carries its control row. For the allocation family that is
//! `fd_alloc_of_0`, the same call against an empty table: the floor of the
//! operation with none of the searching, so the rest of the family is read as
//! a difference against it rather than as an absolute. For the sweeps it is
//! `close_range_three_of_0`, the same narrow range against a process with
//! nothing open.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the reasons
//! given in `zircon-object/benches/vm.rs`.
//!
//! Every row passes its INPUTS through `test::black_box` as well as its
//! result, because several of these are a handful of instructions over
//! constants the compiler can see; a sub-nanosecond figure here is the
//! signature of that mistake and not of speed.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p linux-object --bench process --features mock-disk
//! ```

#![feature(test)]

extern crate test;

use linux_object::fs::{File, FileDesc, OpenFlags};
use linux_object::process::{LinuxProcess, RLimit, RLIMIT_NOFILE};
use rcore_fs::vfs::{FileType, INode};
use rcore_fs_ramfs::RamFS;
use std::sync::Arc;
use test::Bencher;

/// A process holding `n` open descriptors, numbered 0..n with no holes, which
/// is what a real table looks like: Linux hands out the lowest free number, so
/// a process that has opened `n` files and closed none has exactly this shape.
///
/// `RLIMIT_NOFILE` is raised first. The default soft limit is 1024 and
/// `insert_file` refuses anything past it, so without this the large sizes
/// would not be reachable at all -- and a server that opens ten thousand
/// sockets raises the limit for exactly the same reason.
fn with_open_files(n: usize) -> LinuxProcess {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    proc.rlimit(
        RLIMIT_NOFILE,
        Some(RLimit {
            cur: 1024 * 1024,
            max: 1024 * 1024,
        }),
        true,
    )
    .expect("CAP_SYS_RESOURCE is granted outright here, so the hard limit moves");
    let inode: Arc<dyn INode> = proc
        .root_inode()
        .create("leaf", FileType::File, 0o644)
        .expect("a RamFS takes one file");
    for _ in 0..n {
        proc.add_file(File::new(
            inode.clone(),
            OpenFlags::RDONLY,
            String::from("/leaf"),
        ))
        .expect("the limit was raised above n");
    }
    proc
}

/// The number the next `open` would get, with `n` descriptors already open.
/// This is the search alone, without the insert that follows it, so the family
/// reports the cost of FINDING the hole and nothing else.
fn bench_fd_alloc(b: &mut Bencher, n: usize) {
    let proc = with_open_files(n);
    b.iter(|| test::black_box(proc.get_free_fd()));
}

/// The control row: the same call with nothing open. Whatever the operation
/// costs before it has anything to search through.
#[bench]
fn fd_alloc_of_0(b: &mut Bencher) {
    bench_fd_alloc(b, 0);
}

#[bench]
fn fd_alloc_of_16(b: &mut Bencher) {
    bench_fd_alloc(b, 16);
}

#[bench]
fn fd_alloc_of_256(b: &mut Bencher) {
    bench_fd_alloc(b, 256);
}

/// 1024 is the default soft `RLIMIT_NOFILE`, so this is the largest table an
/// ordinary process reaches without asking for more.
#[bench]
fn fd_alloc_of_1024(b: &mut Bencher) {
    bench_fd_alloc(b, 1024);
}

/// A table the size a browser or a chat client actually runs with. Firefox and
/// halloy both pass four figures of open descriptors on this kernel.
#[bench]
fn fd_alloc_of_8192(b: &mut Bencher) {
    bench_fd_alloc(b, 8192);
}

/// `fcntl(F_DUPFD, start)` with the hole AT `start`: the same search, told
/// where to begin. Paired with `fd_alloc_of_8192`, which searches the same
/// table from zero, it separates the two halves of the cost -- if this row is
/// flat and that one is not, what the family is measuring is the walk up to
/// the hole and not the table's size.
fn bench_fd_alloc_from_end(b: &mut Bencher, n: usize) {
    let proc = with_open_files(n);
    b.iter(|| test::black_box(proc.get_free_fd_from(test::black_box(n))));
}

#[bench]
fn fd_alloc_from_end_of_256(b: &mut Bencher) {
    bench_fd_alloc_from_end(b, 256);
}

#[bench]
fn fd_alloc_from_end_of_8192(b: &mut Bencher) {
    bench_fd_alloc_from_end(b, 8192);
}

/// What filling a table costs end to end: `n` descriptors opened from empty,
/// each one allocating and inserting. One row rather than a family because
/// what it is for is the shape -- against `fd_alloc_of_*` it says whether the
/// total is linear in `n` or quadratic.
fn bench_fd_fill(b: &mut Bencher, n: usize) {
    b.iter(|| test::black_box(with_open_files(test::black_box(n)).get_free_fd()));
}

#[bench]
fn fd_fill_256(b: &mut Bencher) {
    bench_fd_fill(b, 256);
}

#[bench]
fn fd_fill_1024(b: &mut Bencher) {
    bench_fd_fill(b, 1024);
}

/// `close_range(3, 5)` -- the three descriptors a child closes before `exec`,
/// against a table of `n`. The range is three numbers wide whatever `n` is, so
/// a family that grows with `n` is one that walks every descriptor the process
/// holds to find three of them, and the sweep is paying for the table rather
/// than for the range.
fn bench_close_range_three(b: &mut Bencher, n: usize) {
    let proc = with_open_files(n);
    let inode = proc.root_inode().lookup("leaf").expect("created above");
    b.iter(|| {
        // Not `iter`'s subject: the range closed on the previous iteration has
        // to come back, or every pass after the first sweeps a table with three
        // holes in it and measures something else.
        //
        // Put back BY NUMBER (`add_file_at`), never through `add_file`: the
        // allocating path is the subject of the family above, and at eight
        // thousand descriptors three of its calls cost more than the sweep
        // being measured -- which is how the first draft of this row came out
        // reporting the allocator twice over.
        for fd in 3..6 {
            proc.add_file_at(
                FileDesc::from(fd),
                File::new(inode.clone(), OpenFlags::RDONLY, String::from("/leaf")),
            )
            .expect("three numbers were just freed");
        }
        test::black_box(proc.close_range(
            test::black_box(FileDesc::from(3)),
            test::black_box(FileDesc::from(5)),
        ));
    });
}

/// The control row: the same narrow sweep with nothing to sweep.
#[bench]
fn close_range_three_of_0(b: &mut Bencher) {
    bench_close_range_three(b, 0);
}

#[bench]
fn close_range_three_of_256(b: &mut Bencher) {
    bench_close_range_three(b, 256);
}

#[bench]
fn close_range_three_of_8192(b: &mut Bencher) {
    bench_close_range_three(b, 8192);
}

/// `close_range(3, ~0u32, CLOSE_RANGE_CLOEXEC)`, the sweep a shell runs before
/// `exec`: every descriptor from three up marked close-on-exec. This one is
/// honestly linear in the table -- it has to touch every descriptor it marks --
/// and it is here as the comparison that makes the narrow rows above legible:
/// a narrow sweep that costs what this one costs is the finding.
fn bench_set_range_cloexec_all(b: &mut Bencher, n: usize) {
    let proc = with_open_files(n);
    b.iter(|| {
        test::black_box(
            proc.set_range_cloexec(test::black_box(FileDesc::from(3)), FileDesc::from(i32::MAX)),
        )
    });
}

#[bench]
fn set_range_cloexec_all_of_256(b: &mut Bencher) {
    bench_set_range_cloexec_all(b, 256);
}

#[bench]
fn set_range_cloexec_all_of_8192(b: &mut Bencher) {
    bench_set_range_cloexec_all(b, 8192);
}

/// `close_range(3, 5, CLOSE_RANGE_CLOEXEC)`: the same three descriptors as
/// `close_range_three_of_*`, marked instead of closed. The pair exists because
/// this is the half of the sweep that does nothing BUT walk the range -- no
/// files are removed, so no epoll is told anything -- and it is therefore the
/// clean measure of which way round the walk went. Flat across the sizes is
/// the range walk; growing with them is the table walk.
fn bench_set_range_cloexec_three(b: &mut Bencher, n: usize) {
    let proc = with_open_files(n);
    b.iter(|| {
        test::black_box(proc.set_range_cloexec(
            test::black_box(FileDesc::from(3)),
            test::black_box(FileDesc::from(5)),
        ))
    });
}

/// The control row: the same narrow sweep with nothing open to find.
#[bench]
fn set_range_cloexec_three_of_0(b: &mut Bencher) {
    bench_set_range_cloexec_three(b, 0);
}

#[bench]
fn set_range_cloexec_three_of_256(b: &mut Bencher) {
    bench_set_range_cloexec_three(b, 256);
}

#[bench]
fn set_range_cloexec_three_of_8192(b: &mut Bencher) {
    bench_set_range_cloexec_three(b, 8192);
}

/// One `close(2)`, and the `open` that puts the descriptor back. The hottest
/// pair in the file-descriptor family -- every program that reads a file does
/// it -- and the row the `close_range_three_of_*` family points at: what a
/// close costs that is not the removal itself is `epoll_forget_plan`, which
/// asks whether any epoll is watching by walking the whole table, on every
/// close. The reopen is `fd_alloc_of_*`'s subject and is flat, so a family
/// that grows here is growing on the close side.
fn bench_close_and_reopen(b: &mut Bencher, n: usize) {
    let proc = with_open_files(n);
    let inode = proc.root_inode().lookup("leaf").expect("created above");
    let last = FileDesc::from(n.saturating_sub(1));
    b.iter(|| {
        proc.close_file(test::black_box(last)).expect("it is open");
        test::black_box(
            proc.add_file(File::new(
                inode.clone(),
                OpenFlags::RDONLY,
                String::from("/leaf"),
            ))
            .expect("the number just came free"),
        )
    });
}

#[bench]
fn close_and_reopen_of_16(b: &mut Bencher) {
    bench_close_and_reopen(b, 16);
}

#[bench]
fn close_and_reopen_of_256(b: &mut Bencher) {
    bench_close_and_reopen(b, 256);
}

#[bench]
fn close_and_reopen_of_8192(b: &mut Bencher) {
    bench_close_and_reopen(b, 8192);
}
