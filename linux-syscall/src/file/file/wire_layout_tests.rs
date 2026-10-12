//! The layout of the two structures this file hands across the kernel's edge.
//!
//! `statfs(2)` and `fstatfs(2)` write a `StatFs` straight into a buffer the
//! caller sized itself, and `F_GETLK` fills in the caller's `Flock` the same
//! way. The order of the fields is the ABI: a 64-bit libc reads `f_bsize` at
//! offset 8 and nothing tells it otherwise.
//!
//! `StatFs` was not unguarded -- there is a
//! `const_assert_eq!(120, size_of::<StatFs>())` right next to the
//! declaration -- and that is the point: a size is not a layout. Swapping any
//! two same-typed neighbours keeps the 120 bytes exactly and moves every
//! value one slot along, so `f_bsize` comes out carrying the filesystem
//! magic and `df` reports that as the block size. Measured over the crate's
//! whole suite: the nine swaps the two structures allow all survived.

use super::*;
use core::mem::{align_of, offset_of, size_of};

/// `struct statfs` as the generic 64-bit uapi fixes it, against
/// `include/uapi/asm-generic/statfs.h`: seven longs, the fsid pair, three
/// more longs and four spare words, 120 bytes in all.
///
/// x86_64 has no 64-bit `asm/statfs.h` of its own, so the generic header is
/// what a 64-bit libc compiles against -- which is the thing to establish
/// before trusting a header.
#[test]
fn statfs_is_the_generic_64_bit_struct_statfs_field_by_field() {
    assert_eq!(size_of::<StatFs>(), 120);
    assert_eq!(align_of::<StatFs>(), 8);
    assert_eq!(offset_of!(StatFs, f_type), 0);
    assert_eq!(offset_of!(StatFs, f_bsize), 8);
    assert_eq!(offset_of!(StatFs, f_blocks), 16);
    assert_eq!(offset_of!(StatFs, f_bfree), 24);
    assert_eq!(offset_of!(StatFs, f_bavail), 32);
    assert_eq!(offset_of!(StatFs, f_files), 40);
    assert_eq!(offset_of!(StatFs, f_ffree), 48);
    assert_eq!(offset_of!(StatFs, f_fsid), 56);
    assert_eq!(offset_of!(StatFs, f_namelen), 64);
    assert_eq!(offset_of!(StatFs, f_frsize), 72);
    assert_eq!(offset_of!(StatFs, f_flags), 80);
    assert_eq!(offset_of!(StatFs, f_spare), 88);
    // Which accounts for every one of the 120 bytes: the spare array is four
    // words long and ends exactly at the end of the struct.
    assert_eq!(offset_of!(StatFs, f_spare) + 4 * 8, 120);
}

/// `f_fsid` is `__kernel_fsid_t`, which the header writes as `int val[2]`.
///
/// It is a Rust tuple here, and a tuple is `repr(Rust)`: nothing promises
/// which half comes first. Both halves are the same type and the same width,
/// so no caller can tell them apart -- but the eight bytes, and the place
/// they sit in, are the ABI, and those are what is pinned.
#[test]
fn the_statfs_fsid_is_the_eight_bytes_of_kernel_fsid_t() {
    assert_eq!(size_of::<(i32, i32)>(), 8);
    assert_eq!(
        offset_of!(StatFs, f_fsid) + 8,
        offset_of!(StatFs, f_namelen)
    );
}

/// `struct flock` as a 64-bit libc lays it out, against
/// `include/uapi/asm-generic/fcntl.h`: the two shorts, four bytes of
/// padding, the two offsets, the pid and four bytes of tail padding.
#[test]
fn flock_is_the_64_bit_struct_flock_field_by_field() {
    assert_eq!(size_of::<Flock>(), 32);
    assert_eq!(align_of::<Flock>(), 8);
    assert_eq!(offset_of!(Flock, l_type), 0);
    assert_eq!(offset_of!(Flock, l_whence), 2);
    // Bytes 4..8 are padding: `l_start` is eight wide and has to be aligned.
    assert_eq!(offset_of!(Flock, l_start), 8);
    assert_eq!(offset_of!(Flock, l_len), 16);
    assert_eq!(offset_of!(Flock, l_pid), 24);
    // And bytes 28..32 are the tail padding that rounds the struct up.
    assert_eq!(offset_of!(Flock, l_pid) + 4 + 4, 32);
}
