//! `sync_file_range(2)`'s three flags, which used to be declared inside the
//! syscall method.
//!
//! Nothing could reach them there: the constants had no name outside the
//! body, and the check around them needed a `Syscall`, which needs a live
//! process. Every number below is written out from `<linux/fs.h>` rather
//! than read back out of the tree.

use super::{
    sync_file_range_writes_back, LxError, SYNC_FILE_RANGE_WAIT_AFTER, SYNC_FILE_RANGE_WAIT_BEFORE,
    SYNC_FILE_RANGE_WRITE,
};

#[test]
fn the_sync_file_range_flags_are_the_numbers_linux_fs_h_gives() {
    // #define SYNC_FILE_RANGE_WAIT_BEFORE 1
    // #define SYNC_FILE_RANGE_WRITE       2
    // #define SYNC_FILE_RANGE_WAIT_AFTER  4
    assert_eq!(SYNC_FILE_RANGE_WAIT_BEFORE, 1);
    assert_eq!(SYNC_FILE_RANGE_WRITE, 2);
    assert_eq!(SYNC_FILE_RANGE_WAIT_AFTER, 4);
    // They are three separate bits, and their union is the header's
    // SYNC_FILE_RANGE_WRITE_AND_WAIT, which is what fdatasync-style callers
    // pass. Two of them sharing a bit would make one of the three
    // unaskable while every flag word still looked valid.
    assert_eq!(
        SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER,
        7,
        "SYNC_FILE_RANGE_WRITE_AND_WAIT"
    );
}

#[test]
fn every_combination_of_the_three_is_accepted_and_nothing_else_is() {
    // Raw numbers: 0 through 7 is the whole of the valid flag space.
    for flags in 0usize..=7 {
        assert_eq!(
            sync_file_range_writes_back(flags),
            Ok(flags != 0),
            "flags {:#x}",
            flags
        );
    }
    // A bit the call does not have is EINVAL -- that is how a caller finds
    // out this kernel is older than the flag it wanted -- including beside a
    // bit it does have, so the stray is not quietly dropped and the rest
    // honoured.
    for stray in [8usize, 16, 7 | 8, 1 << 31, 1 << 63, usize::MAX] {
        assert_eq!(
            sync_file_range_writes_back(stray),
            Err(LxError::EINVAL),
            "flags {:#x}",
            stray
        );
    }
}

/// An empty flag word is legal and asks for nothing: `sync_file_range(fd,
/// off, len, 0)` is a documented no-op that returns success. A kernel that
/// read it the other way round would sync on the one call that asked for
/// nothing and skip the ones that asked.
#[test]
fn an_empty_flag_word_is_legal_and_asks_for_no_write_back() {
    assert_eq!(sync_file_range_writes_back(0), Ok(false));
    for flags in 1usize..=7 {
        assert_eq!(
            sync_file_range_writes_back(flags),
            Ok(true),
            "flags {:#x}",
            flags
        );
    }
}
