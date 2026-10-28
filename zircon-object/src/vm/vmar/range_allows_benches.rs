//! What `range_allows` adds to a copy: every `UserPtr` read and write asks
//! it now before touching user memory, where it asked `range_is_mapped`.
//!
//! The rows are one word, which is most copies (a `timespec`, a status, a
//! pidfd number), and the 1 MiB buffer of a large `write(2)`. The 1 MiB rows
//! come twice: in a mapping of one protection, and in one that `mprotect`
//! has split into 64 runs of flags, the walk's worst shape, since a read
//! that every run allows looks at each of them. The last row is a word at
//! the start of that split mapping: the runs after the one it falls in are
//! not looked at.
//!
//! What came out, in ns, on the host:
//!
//! | row | `range_is_mapped` | `range_allows` |
//! |---|---|---|
//! | one word | 37.70 | 39.65 |
//! | 1 MiB in one run | 38.82 | 40.07 |
//! | 1 MiB in 64 runs | | 107.31 |
//! | one word in 64 runs | | 41.46 |
//!
//! The last row was 65.36 while the walk looked at every run of the mapping
//! and not only at those up to the range's last page.

use super::*;
use ::test::{black_box, Bencher};

const RW: MMUFlags = MMUFlags::from_bits_truncate(
    MMUFlags::READ.bits() | MMUFlags::WRITE.bits() | MMUFlags::USER.bits(),
);
const RO: MMUFlags = MMUFlags::from_bits_truncate(MMUFlags::READ.bits() | MMUFlags::USER.bits());
const MIB: usize = 1 << 20;

fn a_mapping(len: usize) -> (Arc<VmAddressRegion>, VirtAddr) {
    let vmar = VmAddressRegion::new_root();
    let pages = len / PAGE_SIZE;
    vmar.map_at(0, VmObject::new_paged(pages), 0, len, RW)
        .unwrap();
    let base = vmar.addr();
    (vmar, base)
}

/// 1 MiB whose every other run of 4 pages is read-only: 64 runs.
fn a_split_mapping() -> (Arc<VmAddressRegion>, VirtAddr) {
    let (vmar, base) = a_mapping(MIB);
    for run in (1..64).step_by(2) {
        vmar.protect(base + run * 4 * PAGE_SIZE, 4 * PAGE_SIZE, RO)
            .unwrap();
    }
    (vmar, base)
}

#[bench]
fn range_is_mapped_one_word(b: &mut Bencher) {
    let (vmar, base) = a_mapping(PAGE_SIZE);
    b.iter(|| vmar.range_is_mapped(black_box(base), black_box(8)));
}

#[bench]
fn range_allows_one_word(b: &mut Bencher) {
    let (vmar, base) = a_mapping(PAGE_SIZE);
    b.iter(|| vmar.range_allows(black_box(base), black_box(8), black_box(MMUFlags::WRITE)));
}

#[bench]
fn range_is_mapped_one_mebibyte(b: &mut Bencher) {
    let (vmar, base) = a_mapping(MIB);
    b.iter(|| vmar.range_is_mapped(black_box(base), black_box(MIB)));
}

#[bench]
fn range_allows_one_mebibyte(b: &mut Bencher) {
    let (vmar, base) = a_mapping(MIB);
    b.iter(|| vmar.range_allows(black_box(base), black_box(MIB), black_box(MMUFlags::READ)));
}

#[bench]
fn range_allows_one_mebibyte_in_64_runs(b: &mut Bencher) {
    let (vmar, base) = a_split_mapping();
    b.iter(|| vmar.range_allows(black_box(base), black_box(MIB), black_box(MMUFlags::READ)));
}

#[bench]
fn range_allows_one_word_in_64_runs(b: &mut Bencher) {
    let (vmar, base) = a_split_mapping();
    b.iter(|| vmar.range_allows(black_box(base), black_box(8), black_box(MMUFlags::READ)));
}
