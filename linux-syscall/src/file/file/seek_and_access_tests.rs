//! `lseek` argument checks, `SEEK_DATA`/`SEEK_HOLE` and `access` mode checks.

use super::*;

/// `SeekFrom` does not implement `PartialEq`, so compare by shape.
fn seek(whence: usize, offset: i64) -> Result<SeekFrom, LxError> {
    seek_from(whence, offset)
}

#[test]
fn the_three_whences_translate() {
    assert!(matches!(seek(0, 42), Ok(SeekFrom::Start(42))));
    assert!(matches!(seek(1, -42), Ok(SeekFrom::Current(-42))));
    assert!(matches!(seek(2, -42), Ok(SeekFrom::End(-42))));
}

#[test]
fn a_relative_seek_may_be_negative() {
    // SEEK_CUR and SEEK_END are relative, so Linux checks the *resulting*
    // position, not the argument. Rejecting these would break every
    // `lseek(fd, -n, SEEK_END)`, which is how a reader finds a trailer.
    assert!(matches!(seek(1, i64::MIN), Ok(SeekFrom::Current(i64::MIN))));
    assert!(matches!(seek(2, -1), Ok(SeekFrom::End(-1))));
}

#[test]
fn an_absolute_seek_may_not_be_negative() {
    // `vfs_setpos()` answers EINVAL. Read as `offset as u64` it became a
    // position near 2^64, which leaves the syscall looking like an errno.
    assert_eq!(seek(0, -1).err(), Some(LxError::EINVAL));
    assert_eq!(seek(0, i64::MIN).err(), Some(LxError::EINVAL));
    assert!(matches!(seek(0, 0), Ok(SeekFrom::Start(0))));
}

#[test]
fn an_unknown_whence_is_rejected_instead_of_truncated() {
    // 0x100 has low byte 0, so it used to mean SEEK_SET and the file
    // position quietly moved to `offset`.
    assert_eq!(seek(0x100, 42).err(), Some(LxError::EINVAL));
    assert_eq!(seek(0x102, 42).err(), Some(LxError::EINVAL));
    // SEEK_DATA and SEEK_HOLE never land on one of the three above
    // (`sys_lseek` answers them before it gets here).
    assert_eq!(seek(3, 0).err(), Some(LxError::EINVAL));
    assert_eq!(seek(4, 0).err(), Some(LxError::EINVAL));
}

#[test]
fn seek_data_and_hole_on_a_filesystem_without_holes() {
    const DATA: usize = 3;
    const HOLE: usize = 4;
    assert_eq!(seek_data_hole_generic(100, DATA, 0), Ok(0));
    assert_eq!(seek_data_hole_generic(100, DATA, 42), Ok(42));
    assert_eq!(seek_data_hole_generic(100, HOLE, 42), Ok(100));
    assert_eq!(seek_data_hole_generic(100, DATA, 100), Err(LxError::ENXIO));
    assert_eq!(seek_data_hole_generic(100, HOLE, 100), Err(LxError::ENXIO));
    assert_eq!(seek_data_hole_generic(100, DATA, -1), Err(LxError::ENXIO));
    assert_eq!(seek_data_hole_generic(0, HOLE, 0), Err(LxError::ENXIO));
    assert_eq!(seek(usize::MAX, 0).err(), Some(LxError::EINVAL));
}

#[test]
fn the_access_modes_pass_through() {
    // R_OK | W_OK | X_OK and every combination below them, F_OK included.
    for mode in 0..=0o7usize {
        assert_eq!(
            Syscall::access_mode(mode),
            Ok(mode as u16),
            "mode {:o}",
            mode
        );
    }
}

#[test]
fn a_mode_outside_rwx_is_rejected_instead_of_masked() {
    // `do_faccessat()`: `if (mode & ~S_IRWXO) return -EINVAL;`. Masked
    // instead, `access(path, 8)` asked F_OK and answered "yes, you may"
    // for any file that merely exists.
    assert_eq!(Syscall::access_mode(0o10), Err(LxError::EINVAL));
    assert_eq!(Syscall::access_mode(0o17), Err(LxError::EINVAL));
    assert_eq!(Syscall::access_mode(usize::MAX), Err(LxError::EINVAL));
}
