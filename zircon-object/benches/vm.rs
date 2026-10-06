//! Reference benchmarks for the address-space objects: VMO creation, mapping
//! into a VMAR, the lookup every page fault pays, and the copies every syscall
//! that touches userspace memory pays.
//!
//! Why these and not a frame rate: `tools/eclipse-bench` already measures what
//! a *process* pays for `mmap`, `munmap` and a page fault, from userspace, in
//! wall-clock nanoseconds. What it cannot do is say how much of that figure is
//! this crate and how much is the syscall entry, the scheduler and the
//! emulator it ran under. These rows do: same machine, same run, no syscall,
//! no guest. Read them as the floor the userspace numbers sit on.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), which needs
//! nightly. This tree already pins a nightly toolchain in
//! `rust-toolchain.toml`, and `smoltcp/benches/bench.rs` is the existing
//! precedent, so no benchmarking dependency enters the kernel's `Cargo.lock`
//! for this. `criterion` or `divan` would give confidence intervals and stored
//! baselines on stable; the trade was made for a zero-dependency build, and
//! the bodies below would port unchanged.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p zircon-object --bench vm --features libos,aspace-separate
//! ```
//!
//! Each `_1`/`_64`/`_512` triple exists so the SLOPE can be read, not an
//! absolute: a single row cannot tell an O(1) lookup from an O(n) one, and
//! which of those `find_mapping` is decides what a page fault costs on a
//! process with a thousand mappings — a browser, a compositor, anything
//! dynamically linked.

#![feature(test)]

extern crate test;

use std::sync::Arc;
use test::Bencher;
use zircon_object::vm::{MMUFlags, VmAddressRegion, VmObject, PAGE_SIZE};

/// Flags every ordinary userspace mapping carries.
fn user_rw() -> MMUFlags {
    MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER
}

/// A root VMAR with `n` single-page mappings already in it, plus the stride
/// between them, so a caller can aim at one.
///
/// The mappings are spaced two pages apart rather than packed: packed
/// neighbours can be coalesced, and a benchmark that silently measures one
/// giant mapping instead of `n` of them would report the O(1) answer whatever
/// the lookup does.
fn populated(n: usize) -> (Arc<VmAddressRegion>, usize) {
    let vmar = VmAddressRegion::new_root();
    let stride = PAGE_SIZE * 2;
    for i in 0..n {
        let vmo = VmObject::new_paged(1);
        vmar.map_at(i * stride, vmo, 0, PAGE_SIZE, user_rw())
            .expect("a fresh root VMAR has room for these");
    }
    (vmar, stride)
}

#[bench]
fn vmo_new_paged_1_page(b: &mut Bencher) {
    b.iter(|| test::black_box(VmObject::new_paged(1)));
}

/// 256 pages is a megabyte: what a loader asks for per ELF segment, so this is
/// the per-`mmap` object cost a process start pays several times over.
#[bench]
fn vmo_new_paged_256_pages(b: &mut Bencher) {
    b.iter(|| test::black_box(VmObject::new_paged(256)));
}

/// Map one page and take it back out again: the pair of operations behind
/// `mmap`/`munmap` of a single page, with no syscall around them.
#[bench]
fn vmar_map_unmap_one_page(b: &mut Bencher) {
    let vmar = VmAddressRegion::new_root();
    b.iter(|| {
        let vmo = VmObject::new_paged(1);
        let addr = vmar
            .map_at(0, vmo, 0, PAGE_SIZE, user_rw())
            .expect("offset 0 of a root VMAR is free again every iteration");
        vmar.unmap(addr, PAGE_SIZE)
            .expect("unmap what we just mapped");
        test::black_box(addr)
    });
}

/// The same single mapping, inserted into a VMAR that already holds 512 of
/// them. Against the row above, the difference is what maintaining the mapping
/// list costs as an address space fills up — which is the shape a long-running
/// process has and a benchmark starting from empty never sees.
#[bench]
fn vmar_map_unmap_with_512_mappings(b: &mut Bencher) {
    let (vmar, stride) = populated(512);
    let free = 512 * stride;
    b.iter(|| {
        let vmo = VmObject::new_paged(1);
        let addr = vmar
            .map_at(free, vmo, 0, PAGE_SIZE, user_rw())
            .expect("the slot past the populated range is free");
        vmar.unmap(addr, PAGE_SIZE)
            .expect("unmap what we just mapped");
        test::black_box(addr)
    });
}

/// `find_mapping` is the page-fault path: given a faulting address, which
/// mapping owns it. Benched at 1, 64 and 512 mappings, always hitting the LAST
/// one — the worst case for a linear scan and no harder than any other for a
/// tree. A flat triple says the lookup is sub-linear; a triple that grows with
/// the count says every fault in a big address space pays for its size.
fn bench_find_last(b: &mut Bencher, n: usize) {
    let (vmar, stride) = populated(n);
    let target = vmar.addr() + (n - 1) * stride;
    b.iter(|| test::black_box(vmar.find_mapping(target)));
}

#[bench]
fn vmar_find_mapping_of_1(b: &mut Bencher) {
    bench_find_last(b, 1);
}

#[bench]
fn vmar_find_mapping_of_64(b: &mut Bencher) {
    bench_find_last(b, 64);
}

#[bench]
fn vmar_find_mapping_of_512(b: &mut Bencher) {
    bench_find_last(b, 512);
}

/// A miss, on a populated VMAR: the address belongs to nothing. An
/// implementation that returns early on a hit can still walk everything before
/// admitting it has nothing, and a fault on an unmapped address is the common
/// case for a growing heap or stack, so it gets its own row.
#[bench]
fn vmar_find_mapping_miss_of_512(b: &mut Bencher) {
    let (vmar, stride) = populated(512);
    // One page past the last mapping, inside the gap the stride leaves.
    let target = vmar.addr() + (512 - 1) * stride + PAGE_SIZE;
    b.iter(|| test::black_box(vmar.find_mapping(target)));
}

/// `read_memory` / `write_memory` are what a syscall uses to copy a userspace
/// buffer in or out. 4 KiB is one page: the common size for a `read`, a
/// `stat` struct is far smaller and a pipe write far larger, so this is the
/// per-page rate the copies run at, with the mapping lookup included because a
/// syscall pays that too.
#[bench]
fn vmar_read_memory_4_kib(b: &mut Bencher) {
    let vmar = VmAddressRegion::new_root();
    let vmo = VmObject::new_paged(1);
    let addr = vmar
        .map_at(0, vmo, 0, PAGE_SIZE, user_rw())
        .expect("one page into a fresh root VMAR");
    let mut buf = vec![0u8; PAGE_SIZE];
    b.iter(|| {
        let n = vmar
            .read_memory(addr, &mut buf)
            .expect("the page is mapped");
        test::black_box(n)
    });
}

#[bench]
fn vmar_write_memory_4_kib(b: &mut Bencher) {
    let vmar = VmAddressRegion::new_root();
    let vmo = VmObject::new_paged(1);
    let addr = vmar
        .map_at(0, vmo, 0, PAGE_SIZE, user_rw())
        .expect("one page into a fresh root VMAR");
    let buf = vec![0x5au8; PAGE_SIZE];
    b.iter(|| {
        let n = vmar.write_memory(addr, &buf).expect("the page is mapped");
        test::black_box(n)
    });
}

/// `fork_copy` is the copy-on-write snapshot `fork` takes of every private
/// mapping. `tools/eclipse-bench` reports `fork cost per MiB resident` from
/// userspace; this is the same work with the process creation taken out, so
/// the two bracket it.
#[bench]
fn vmo_fork_copy_256_pages(b: &mut Bencher) {
    let vmo = VmObject::new_paged(256);
    b.iter(|| test::black_box(vmo.fork_copy().expect("a paged VMO can be forked")));
}
