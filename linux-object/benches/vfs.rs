//! Reference benchmarks for the Linux-personality layer: the path walk, the
//! descriptor table, and the procfs reports every tool on the machine reads.
//!
//! These are the in-kernel halves of rows `tools/eclipse-bench` already
//! measures from userspace. The C suite says what a *process* pays for an
//! `open`, a `read` of `/proc/self/status` or a path nine components deep,
//! including the syscall entry, the scheduler and the emulator it ran under.
//! It cannot say how much of that was this crate. These rows can: same
//! machine, same run, no syscall and no guest, so the pair brackets the cost
//! from both ends.
//!
//! The filesystem under them is a RamFS, deliberately: the subject is path
//! resolution and descriptor bookkeeping, and a real block device would bury
//! both under I/O that the `disk` section of the C suite already measures.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the reasons
//! given in `zircon-object/benches/vm.rs`: nightly is already pinned in
//! `rust-toolchain.toml`, `smoltcp/benches/bench.rs` is the existing
//! precedent, and no benchmarking dependency enters the kernel's `Cargo.lock`.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p linux-object --bench vfs --features mock-disk
//! ```
//!
//! The `_1`/`_9` and `_1`/`_256` pairs exist so a SLOPE can be read. One row
//! cannot tell a lookup that costs the same at any depth from one that pays
//! per component, nor a descriptor table that is a map from one that is a
//! list — and on a process holding a thousand descriptors, which of those it
//! is decides what every syscall taking an fd costs.
//!
//! The lookup rows come in three flavours on purpose: a cache HIT, a COLD walk
//! with the path cache invalidated, and a MISS. Benching only the hit would
//! report that path depth is free, which is true of a repeated `open` and false
//! of everything a build or an installer does, because every namespace
//! mutation empties that cache.

#![feature(test)]

extern crate test;

use linux_object::fs::{File, FileDesc, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileType, INode};
use rcore_fs_ramfs::RamFS;
use std::sync::Arc;
use test::Bencher;

/// A process on a RamFS, with `depth` nested directories and a file at the
/// bottom of the chain. Returns the process and the path to that file.
fn with_depth(depth: usize) -> (LinuxProcess, String) {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let mut dir: Arc<dyn INode> = proc.root_inode().clone();
    let mut path = String::new();
    for i in 0..depth {
        let name = format!("d{}", i);
        dir = dir
            .create(&name, FileType::Dir, 0o755)
            .expect("a RamFS takes as many directories as we ask for");
        path.push('/');
        path.push_str(&name);
    }
    dir.create("leaf", FileType::File, 0o644)
        .expect("one file at the bottom");
    path.push_str("/leaf");
    (proc, path)
}

/// Absolute lookups are served from a global path cache (`dcache`), so the
/// pair below is the HIT path: what the second and every later `open` of a
/// library, a font or a config file pays. The depth makes no difference to a
/// hit, which is the point of the pair — a flat result is the cache working,
/// and a result that grows with depth would mean it is not being consulted.
#[bench]
fn lookup_inode_cached_1_component(b: &mut Bencher) {
    let (proc, path) = with_depth(0);
    let _ = proc.lookup_inode(&path); // seed the cache, outside the clock
    b.iter(|| test::black_box(proc.lookup_inode(&path).expect("the file is there")));
}

#[bench]
fn lookup_inode_cached_9_components(b: &mut Bencher) {
    let (proc, path) = with_depth(8);
    let _ = proc.lookup_inode(&path);
    b.iter(|| test::black_box(proc.lookup_inode(&path).expect("the file is there")));
}

/// The cold walk: the cache is invalidated before each iteration, so every
/// lookup resolves every component. This is not an artificial case — the
/// cache is coherent through a single global epoch that EVERY namespace
/// mutation bumps, so a build, an installer or anything else creating files
/// puts most of its own lookups on this path.
///
/// The invalidation inside the timed loop is one atomic increment; the map
/// clear it triggers happens lazily inside the lookup, which is part of what
/// a post-mutation lookup really pays and belongs in the figure.
///
/// The difference between these two rows, divided by eight, is this crate's
/// own per-component cost — the in-kernel half of the C suite's
/// `path cost per component` row.
#[bench]
fn lookup_inode_cold_1_component(b: &mut Bencher) {
    let (proc, path) = with_depth(0);
    b.iter(|| {
        linux_object::fs::dcache_invalidate();
        test::black_box(proc.lookup_inode(&path).expect("the file is there"))
    });
}

#[bench]
fn lookup_inode_cold_9_components(b: &mut Bencher) {
    let (proc, path) = with_depth(8);
    b.iter(|| {
        linux_object::fs::dcache_invalidate();
        test::black_box(proc.lookup_inode(&path).expect("the file is there"))
    });
}

/// A walk that fails at the last component. Every `exec` of a bare command
/// name does one of these per `PATH` entry before the one that hits, and a
/// failed lookup is never cached, so these rows are a cold walk with no
/// invalidation needed. Their difference over eight components is a second,
/// independent reading of the per-component cost.
#[bench]
fn lookup_inode_miss_1_component(b: &mut Bencher) {
    let (proc, path) = with_depth(0);
    let missing = path.replace("leaf", "absent");
    b.iter(|| test::black_box(proc.lookup_inode(&missing).err()));
}

#[bench]
fn lookup_inode_miss_9_components(b: &mut Bencher) {
    let (proc, path) = with_depth(8);
    let missing = path.replace("leaf", "absent");
    b.iter(|| test::black_box(proc.lookup_inode(&missing).err()));
}

/// Resolving a relative name against a directory descriptor. Compare it with
/// the COLD absolute rows, not the cached ones: the path cache covers absolute
/// lookups only, so this route resolves its component every time however often
/// it is called. That is what the C suite's `openat / open` ratio is really
/// comparing on this kernel, and the fd lookup below (~30 ns) is the only part
/// of it that is table work rather than walking.
#[bench]
fn lookup_inode_at_dirfd(b: &mut Bencher) {
    let (proc, _) = with_depth(1);
    let dir = proc
        .lookup_inode("/d0")
        .expect("the first directory of the chain");
    let fd = proc
        .add_file(File::new(dir, OpenFlags::RDONLY, String::from("/d0")))
        .expect("an empty table has room for one");
    b.iter(|| test::black_box(proc.lookup_inode_at(fd, "leaf", true).expect("it is there")));
}

/// Open a descriptor and give it back: the table bookkeeping behind every
/// `open`/`close` pair, with no filesystem work in the loop (the inode is
/// resolved once, before the clock starts).
#[bench]
fn fd_add_then_close(b: &mut Bencher) {
    let (proc, path) = with_depth(0);
    let inode = proc.lookup_inode(&path).expect("the file is there");
    b.iter(|| {
        let fd = proc
            .add_file(File::new(
                inode.clone(),
                OpenFlags::RDONLY,
                String::from("/leaf"),
            ))
            .expect("the table never grows: we close every iteration");
        proc.close_file(fd).expect("close what we just opened");
        test::black_box(fd)
    });
}

/// Fill the table with `n` descriptors and look the LAST one up: the lookup
/// every syscall that takes an fd performs. Benched at 1 and 256 so the slope
/// says whether a process holding many files pays for holding them.
fn bench_get_file(b: &mut Bencher, n: usize) {
    let (proc, path) = with_depth(0);
    let inode = proc.lookup_inode(&path).expect("the file is there");
    let mut last = FileDesc::from(0);
    for _ in 0..n {
        last = proc
            .add_file(File::new(
                inode.clone(),
                OpenFlags::RDONLY,
                String::from("/leaf"),
            ))
            .expect("the table takes this many");
    }
    b.iter(|| test::black_box(proc.get_file(last).expect("the descriptor is open")));
}

#[bench]
fn fd_get_file_of_1(b: &mut Bencher) {
    bench_get_file(b, 1);
}

#[bench]
fn fd_get_file_of_256(b: &mut Bencher) {
    bench_get_file(b, 256);
}

/// `/proc/<pid>/perf`: the per-process syscall table the `net` and `fs`
/// sections of the C suite read to pair their rows with the kernel's own
/// account. It is formatted on every read, and this benchmark exists partly to
/// keep the measuring instrument honest — a report that costs more than the
/// operation it reports on would distort what it is used to measure.
#[bench]
fn perf_proc_report(b: &mut Bencher) {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    b.iter(|| test::black_box(linux_object::perf::proc_report(&proc, 4242)));
}

/// `/proc/perf/kernel`: the kernel-wide counter report the `psched` section
/// reads before and after every probe. Twice per probe, on a suite with dozens
/// of them, so its cost lands inside measurements it is supposed to explain.
#[bench]
fn perf_kernel_report(b: &mut Bencher) {
    b.iter(|| test::black_box(linux_object::perf::kernel_report()));
}

/// `/proc/perf`: the system-wide syscall table, which is the same formatting
/// over a table that has every syscall in it rather than one process's.
#[bench]
fn perf_global_report(b: &mut Bencher) {
    b.iter(|| test::black_box(linux_object::perf::global_report()));
}
