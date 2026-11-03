//! The memory syscalls: `mmap`, `munmap`, `mprotect`, `madvise`,
//! `msync`, `mincore`, `mlockall` and `brk`.
//!
//! Two kinds of row. The `_args` and flag rows are the work every one of
//! these calls does before it touches a mapping -- rounding a length,
//! bounding a range, narrowing a flag word -- and they are the reason
//! those helpers exist, since each one was a wrong answer once. The
//! `mincore_` and `msync_` rows walk a real VMAR a page at a time, which
//! is work proportional to the range, and they come in families so the
//! slope can be read.
//!
//! Every row black-boxes its inputs. Two cycles (~0.6 ns) is `b.iter`'s
//! own loop; a row there with an opaque input is a real compare and
//! branch, a row there with a constant input is a call that was folded
//! away and never ran.

use super::*;
use test::{black_box, Bencher};

fn user_rw() -> MMUFlags {
    MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER
}

/// A root VMAR with one mapping of `pages` pages at `base`, plus the
/// address it landed at. One mapping rather than many: `mincore` walks
/// pages, not mappings, and this is the shape its loop meets in a heap.
///
/// `base` is a parameter, and every row below passes a different one, for
/// the same two reasons the test helper of the same name spells out:
/// under `libos` these mappings are backed by the *host* process's
/// address space, so two rows sharing a base fight over one host mapping;
/// and a low base runs into `vm.mmap_min_addr`, which on a CI runner is
/// 65536 and here is 4096 -- mapping at 0, as an earlier version of this
/// helper did, is an `EPERM` from the host `mmap` that only CI sees.
/// Bases start past the test helper's range so the two never collide.
fn mapped(base: usize, pages: usize) -> (Arc<VmAddressRegion>, usize) {
    assert!(base >= 0x1000_0000 && base.is_multiple_of(PAGE_SIZE));
    let vmar = VmAddressRegion::new_root();
    let vmo = VmObject::new_paged(pages);
    let addr = vmar
        .map_at(base, vmo, 0, pages * PAGE_SIZE, user_rw())
        .expect("a fresh root VMAR has room for this");
    (vmar, addr)
}

/// `mincore` over one page: the per-call cost, since the loop runs once.
#[bench]
fn mincore_residency_of_1_page(b: &mut Bencher) {
    let (vmar, addr) = mapped(0x1000_0000, 1);
    b.iter(|| {
        black_box(mincore_residency(
            black_box(&vmar),
            black_box(addr),
            black_box(1),
        ))
    });
}

/// 64 pages, a quarter of a megabyte.
#[bench]
fn mincore_residency_of_64_pages(b: &mut Bencher) {
    let (vmar, addr) = mapped(0x1100_0000, 64);
    b.iter(|| {
        black_box(mincore_residency(
            black_box(&vmar),
            black_box(addr),
            black_box(64),
        ))
    });
}

/// 512 pages, which is the chunk `sys_mincore` hands over at a time, so
/// this row is one full chunk of a `mincore` over a large heap: the
/// figure an allocator probing address-space layout pays per call.
#[bench]
fn mincore_residency_of_512_pages(b: &mut Bencher) {
    let (vmar, addr) = mapped(0x1200_0000, 512);
    b.iter(|| {
        black_box(mincore_residency(
            black_box(&vmar),
            black_box(addr),
            black_box(512),
        ))
    });
}

/// The distinct objects under a range, which `msync(MS_SYNC)` needs
/// before it can write any of them back. It looks a mapping up per page
/// and keeps a set, so its slope is the question: a `msync` of a mapped
/// file is one object however large the range.
///
/// `distinct_vmos` takes an absolute `end`, not a length. A row that
/// passed the length read 11 ns flat at both 64 and 512 pages, because
/// `end` was below `start` and the loop never ran once -- the same
/// signature as a folded call, and the reason a flat family is worth
/// doubting before it is believed.
#[bench]
fn msync_distinct_vmos_over_64_pages(b: &mut Bencher) {
    let (vmar, addr) = mapped(0x1300_0000, 64);
    b.iter(|| {
        black_box(distinct_vmos(
            black_box(&vmar),
            black_box(addr),
            black_box(addr + 64 * PAGE_SIZE),
        ))
    });
}

/// The same over 512 pages of the same single object.
#[bench]
fn msync_distinct_vmos_over_512_pages(b: &mut Bencher) {
    let (vmar, addr) = mapped(0x1400_0000, 512);
    b.iter(|| {
        black_box(distinct_vmos(
            black_box(&vmar),
            black_box(addr),
            black_box(addr + 512 * PAGE_SIZE),
        ))
    });
}

/// The `(addr, len)` of every memory syscall, resolved into the range it
/// names. This is the rounding whose wrap made a call naming most of the
/// address space look like one naming nothing.
#[bench]
fn user_range_resolve(b: &mut Bencher) {
    b.iter(|| black_box(user_range(black_box(0x1000_0000), black_box(1 << 20))));
}

/// The same for the length that wraps, which is the arm that was missing.
#[bench]
fn user_range_resolve_wrapping_len(b: &mut Bencher) {
    b.iter(|| black_box(user_range(black_box(0x1000_0000), black_box(usize::MAX))));
}

/// `munmap`'s arguments: alignment, then the range.
#[bench]
fn munmap_arg_check(b: &mut Bencher) {
    b.iter(|| black_box(munmap_args(black_box(0x1000_0000), black_box(1 << 20))));
}

/// `mprotect`'s, which differ from `munmap`'s on both bad answers.
#[bench]
fn mprotect_arg_check(b: &mut Bencher) {
    b.iter(|| black_box(mprotect_args(black_box(0x1000_0000), black_box(1 << 20))));
}

/// `mprotect`'s `prot`, which unlike `mmap`'s is validated: an unknown
/// bit is `EINVAL`, and the two growth directions together are too.
#[bench]
fn mprotect_prot_word(b: &mut Bencher) {
    b.iter(|| black_box(mprotect_prot(black_box(0x1 | 0x2))));
}

/// And the answer for a `prot` with a bit this kernel does not name.
#[bench]
fn mprotect_prot_unknown_bit(b: &mut Bencher) {
    b.iter(|| black_box(mprotect_prot(black_box(0x1 | 0x40))));
}

/// `mmap`'s length, bounded before anything is reserved.
#[bench]
fn mmap_len_arg_check(b: &mut Bencher) {
    b.iter(|| black_box(mmap_len_check(black_box(1 << 20))));
}

/// `mmap`'s sixth argument, whose sum with the length must fit.
#[bench]
fn mmap_offset_validation(b: &mut Bencher) {
    b.iter(|| {
        black_box(validate_mmap_offset(
            black_box(1 << 20),
            black_box(4096),
            black_box(false),
        ))
    });
}

/// `MAP_SHARED` vs `MAP_PRIVATE`, which are not a bitmask but exactly
/// one of three values in the low four bits.
#[bench]
fn mmap_shared_flag(b: &mut Bencher) {
    b.iter(|| black_box(mmap_shared(black_box(MAP_PRIVATE), black_box(true))));
}

/// Where `mmap` may put the mapping: a hint, fixed, or fixed and
/// refusing to replace.
#[bench]
fn mmap_placement_decision(b: &mut Bencher) {
    b.iter(|| black_box(mmap_placement(black_box(MmapFlags::empty()))));
}

/// `MAP_HUGETLB`, which this kernel refuses rather than silently
/// ignoring.
#[bench]
fn mmap_hugetlb_check(b: &mut Bencher) {
    b.iter(|| black_box(mmap_no_hugetlb(black_box(MAP_PRIVATE))));
}

/// How much of a file a mapping covers, given the object's length and
/// the offset into it.
#[bench]
fn mmap_file_map_len(b: &mut Bencher) {
    b.iter(|| {
        black_box(file_map_len(
            black_box(1 << 20),
            black_box(1 << 22),
            black_box(4096),
        ))
    });
}

/// `madvise`'s `advice`: a known one, one this kernel accepts and does
/// not act on, and one it refuses.
#[bench]
fn madvise_advice_lookup(b: &mut Bencher) {
    b.iter(|| {
        black_box((
            madvise_advice_known(black_box(4)),
            madvise_advice_known(black_box(9)),
            madvise_advice_known(black_box(77)),
        ))
    });
}

/// `msync`'s arguments, which carry a flag word the other calls do not.
#[bench]
fn msync_arg_check(b: &mut Bencher) {
    b.iter(|| {
        black_box(msync_args(
            black_box(0x1000_0000),
            black_box(1 << 20),
            black_box(4),
        ))
    });
}

/// `mincore`'s, which bound the per-call chunk.
#[bench]
fn mincore_arg_check(b: &mut Bencher) {
    b.iter(|| black_box(mincore_args(black_box(0x1000_0000), black_box(1 << 20))));
}

/// `mlockall`'s flags.
#[bench]
fn mlockall_flag_check(b: &mut Bencher) {
    b.iter(|| black_box(check_mlockall_flags(black_box(1))));
}
