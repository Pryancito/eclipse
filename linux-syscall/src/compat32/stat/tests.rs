//! The layout of `struct stat64`, checked as bytes rather than as fields.
//!
//! Asserting `s.st_size_lo == 7` only says the conversion filled the field
//! this file named `st_size_lo`; it says nothing about where that field lands
//! in the ninety-six bytes a 32-bit program reads. So these tests take the
//! structure's byte image and read each value back out **at the offset from
//! Linux's header**, which is the one thing a program and this kernel have to
//! agree on.

use super::*;
use core::convert::TryInto;
use linux_object::fs::vfs::{FileType, Timespec};

/// The byte image of a `Stat64`: the production path, the one the system call
/// copies to userspace.
fn bytes(s: &Stat64) -> [u8; STAT64_SIZE] {
    s.to_bytes()
}

/// The bridge between the two: the hand-assembled image is byte-for-byte what
/// a struct copy of the same value would have written.
///
/// Without this, the offsets below would only say that `to_bytes` and the
/// tests agree with each other. With it, they say that the compiler's own
/// `repr(C)` layout puts each field where Linux's header does --- and the
/// `const` assertions in the module then stand on something the running code
/// also obeys.
///
/// The destination here is a `[u32; 24]` because a struct copy DOES need the
/// alignment, which is the whole reason the production path does not use one.
#[test]
fn the_hand_assembled_image_is_what_a_struct_copy_would_have_written() {
    let s = Stat64::from(distinctive());
    let mut words = [0u32; STAT64_SIZE / 4];
    let mut out: UserOutPtr<Stat64> = (words.as_mut_ptr() as usize).into();
    out.write(s).unwrap();
    let mut copied = [0u8; STAT64_SIZE];
    for (i, word) in words.iter().enumerate() {
        copied[i * 4..i * 4 + 4].copy_from_slice(&word.to_ne_bytes());
    }
    assert_eq!(s.to_bytes(), copied);
}

/// Read a 32-bit value at a byte offset, the way a 32-bit program's compiler
/// would address the field there.
fn u32_at(b: &[u8; STAT64_SIZE], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

/// Read a 64-bit value at a byte offset. Note that these offsets are not
/// multiples of eight: that is the point.
fn u64_at(b: &[u8; STAT64_SIZE], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

/// A metadata with a different, recognisable value in every field, so that a
/// value read at the wrong offset cannot accidentally be the right one.
fn distinctive() -> Metadata {
    Metadata {
        dev: 0x0000_0a0b,
        inode: 0x0000_0003_1234_5678,
        size: 0x0000_0002_dead_beef,
        blk_size: 4096,
        blocks: 0x0000_0001_0000_0021,
        atime: Timespec {
            sec: 0x1111_2222,
            nsec: 101,
        },
        mtime: Timespec {
            sec: 0x3333_4444,
            nsec: 202,
        },
        ctime: Timespec {
            sec: 0x5555_6666,
            nsec: 303,
        },
        type_: FileType::File,
        mode: 0o644,
        nlinks: 7,
        uid: 1000,
        gid: 1001,
        rdev: 0x0000_0c0d,
    }
}

/// Every field, at the offset `arch/x86/include/uapi/asm/stat.h` gives it.
#[test]
fn every_field_lands_where_the_i386_header_puts_it() {
    let s = Stat64::from(distinctive());
    let b = bytes(&s);

    assert_eq!(u64_at(&b, 0), 0x0a0b, "st_dev");
    assert_eq!(u32_at(&b, 8), 0, "__pad0");
    // The old 32-bit inode, truncated, at the offset `struct stat` kept the
    // inode at.
    assert_eq!(u32_at(&b, 12), 0x1234_5678, "__st_ino");
    assert_eq!(u32_at(&b, 16), 0o100_644, "st_mode: S_IFREG | 0644");
    assert_eq!(u32_at(&b, 20), 7, "st_nlink");
    assert_eq!(u32_at(&b, 24), 1000, "st_uid");
    assert_eq!(u32_at(&b, 28), 1001, "st_gid");
    assert_eq!(u64_at(&b, 32), 0x0c0d, "st_rdev");
    assert_eq!(u32_at(&b, 40), 0, "__pad3");
    // Offset 44, not 48: the field that 8-byte alignment would have moved.
    assert_eq!(u64_at(&b, 44), 0x0000_0002_dead_beef, "st_size");
    assert_eq!(u32_at(&b, 52), 4096, "st_blksize");
    assert_eq!(u64_at(&b, 56), 0x0000_0001_0000_0021, "st_blocks");
    assert_eq!(u32_at(&b, 64), 0x1111_2222, "st_atime");
    assert_eq!(u32_at(&b, 68), 101, "st_atime_nsec");
    assert_eq!(u32_at(&b, 72), 0x3333_4444, "st_mtime");
    assert_eq!(u32_at(&b, 76), 202, "st_mtime_nsec");
    assert_eq!(u32_at(&b, 80), 0x5555_6666, "st_ctime");
    assert_eq!(u32_at(&b, 84), 303, "st_ctime_nsec");
    // And the real inode, all of it, at the end of the structure.
    assert_eq!(u64_at(&b, 88), 0x0000_0003_1234_5678, "st_ino");
}

/// The two fields that hold the same inode: one whole, one truncated.
///
/// They are not redundant and they are not interchangeable. A program built
/// against `struct stat64` reads offset 88; one built against the older
/// `struct stat` reads offset 12 and gets whatever fits. Filling only one of
/// them is the bug this test exists to catch, because an inode small enough
/// to fit in 32 bits --- which is every inode on a small filesystem --- makes
/// the two indistinguishable.
#[test]
fn the_inode_is_in_both_places_and_only_one_of_them_is_whole() {
    let mut meta = distinctive();
    meta.inode = 0x0000_0007_0000_0001;
    let s = Stat64::from(meta);
    assert_eq!(s.ino(), 0x0000_0007_0000_0001, "st_ino keeps all 64 bits");
    assert_eq!(s.__st_ino, 1, "__st_ino keeps the low 32");

    // And an inode that fits loses nothing in either.
    let mut meta = distinctive();
    meta.inode = 42;
    let s = Stat64::from(meta);
    assert_eq!(s.ino(), 42);
    assert_eq!(s.__st_ino, 42);
}

/// A size above 4 GiB survives, which is the entire reason this structure
/// exists rather than the `struct stat` it replaced.
#[test]
fn a_file_larger_than_four_gigabytes_keeps_its_size() {
    let mut meta = distinctive();
    meta.size = 5 * 1024 * 1024 * 1024;
    let s = Stat64::from(meta);
    assert_eq!(s.size(), 5 * 1024 * 1024 * 1024);
    assert_eq!(s.st_size_lo, 0x4000_0000);
    assert_eq!(s.st_size_hi, 1);
}

/// The file type reaches `st_mode`, for each kind a descriptor can be.
///
/// This is the word a 32-bit `S_ISDIR` tests, and it comes from the same
/// `StatMode` table the native `struct stat` is built from --- so if the
/// native one is right, this is too, and a divergence between the two shows
/// up here.
#[test]
fn the_file_type_bits_are_the_same_table_the_native_stat_uses() {
    for (type_, expected) in [
        (FileType::File, crate::file::StatMode::FILE),
        (FileType::Dir, crate::file::StatMode::DIR),
        (FileType::SymLink, crate::file::StatMode::LINK),
        (FileType::CharDevice, crate::file::StatMode::CHAR),
        (FileType::BlockDevice, crate::file::StatMode::BLOCK),
        (FileType::NamedPipe, crate::file::StatMode::FIFO),
        (FileType::Socket, crate::file::StatMode::SOCKET),
    ] {
        let mut meta = distinctive();
        meta.type_ = type_;
        meta.mode = 0o640;
        let s = Stat64::from(meta);
        assert_eq!(
            s.st_mode,
            expected.bits() | 0o640,
            "st_mode for {:?}",
            type_
        );
    }
}

/// The seconds are 32 bits wide, and this test says out loud what that costs
/// rather than leaving it to be discovered.
///
/// Two separate limits, which is why the naive version of this test was
/// wrong. The field is an `unsigned long`, so the **bits** last until 2106;
/// but a 32-bit program reads it into a `time_t`, which is **signed**, so
/// anything past 2038 reaches the program as a negative time. Linux has both
/// limits in the same place for the same reason --- `cp_new_stat64` assigns to
/// an `unsigned long` and the structure has no wider field --- and the answer
/// to both is `statx`, which is why the `*_time64` family exists at all.
#[test]
fn the_seconds_are_32_bits_with_both_of_the_limits_that_implies() {
    // 2039-01-01: the bits fit, so nothing is lost here...
    let mut meta = distinctive();
    meta.mtime = Timespec {
        sec: 2_177_452_800,
        nsec: 0,
    };
    let s = Stat64::from(meta);
    assert_eq!(u64::from(s.st_mtime), 2_177_452_800u64);
    // ...but the program reads the same word as a signed `time_t`, and that
    // is a date in 1901.
    assert!(
        (s.st_mtime as i32) < 0,
        "a 32-bit time_t reads 2039 as negative"
    );

    // 2110-01-01: past 2106, so the value itself is truncated.
    let mut meta = distinctive();
    meta.mtime = Timespec {
        sec: 4_418_323_200,
        nsec: 0,
    };
    let s = Stat64::from(meta);
    assert_ne!(u64::from(s.st_mtime), 4_418_323_200u64);
    assert_eq!(s.st_mtime, 4_418_323_200u64 as u32);

    // And a date this structure can describe comes through untouched, which
    // is the case that must not be broken by either of the above.
    let mut meta = distinctive();
    meta.mtime = Timespec {
        sec: 1_762_000_000,
        nsec: 123_456_789,
    };
    let s = Stat64::from(meta);
    assert_eq!(u64::from(s.st_mtime), 1_762_000_000u64);
    assert_eq!(s.st_mtime_nsec, 123_456_789);
}

/// An unaligned destination is written, not refused.
///
/// This is the bug the byte-pointer copy exists for. `UserPtr::user_bytes`
/// requires the destination to be a multiple of `align_of::<T>()`, so a
/// `UserOutPtr<Stat64>` refuses any address that is not a multiple of four
/// --- and Linux's `copy_to_user` requires nothing of the sort, so a program
/// whose 96-byte buffer happens to sit at an odd address got `EFAULT` from a
/// perfectly good `fstat64`. The second half of this test is what the first
/// half replaced, kept so that the distinction cannot quietly go away.
#[test]
fn an_unaligned_destination_is_written_and_not_refused() {
    let mut buf = [0u8; STAT64_SIZE + 4];
    let base = buf.as_mut_ptr() as usize;
    // An address that is 1 modulo 4, whatever the array happened to land on,
    // and at least one byte into the buffer so the 96 bytes still fit.
    let skip = (1 + 4 - base % 4) % 4;
    let at = base + if skip == 0 { 4 } else { skip };
    assert_eq!(
        at % 4,
        1,
        "the test needs a destination that is not aligned"
    );

    let s = Stat64::from(distinctive());
    let mut out: UserOutPtr<u8> = at.into();
    out.write_array(&s.to_bytes()).unwrap();

    // And the bytes landed there, read back at the same offsets as above.
    let written = &buf[at - base..at - base + STAT64_SIZE];
    assert_eq!(
        u32::from_ne_bytes(written[16..20].try_into().unwrap()),
        0o100_644,
        "st_mode"
    );
    assert_eq!(
        u64::from_ne_bytes(written[44..52].try_into().unwrap()),
        0x0000_0002_dead_beef,
        "st_size"
    );

    // The typed pointer this replaced refuses the same address.
    let mut typed: UserOutPtr<Stat64> = at.into();
    assert!(
        typed.write(s).is_err(),
        "a typed write still requires the alignment, which is why it is not used"
    );
}

/// The padding is zero, not left over.
///
/// `__pad0` and `__pad3` are bytes a program can read; a kernel that leaves
/// them as whatever the buffer held is handing userspace back whatever that
/// page held before. The explicit zeroes in the conversion keep them clean,
/// and this says so.
#[test]
fn the_padding_is_zeroed_and_not_whatever_was_there() {
    let s = Stat64::from(distinctive());
    assert_eq!(s._pad0, 0);
    assert_eq!(s._pad3, 0);
    // And the whole structure is exactly ninety-six bytes, the size of the
    // buffer a program passes.
    assert_eq!(bytes(&s).len(), 96);
}
