//! `lseek(2)` is arithmetic on a number userspace chose, and the result
//! becomes the offset every later read and write uses. A relative seek
//! that goes below zero, or one that overflows, has to be `EINVAL`: if
//! it wraps instead, the file position lands somewhere near 2^64 and the
//! next read or write is aimed at an offset the filesystem was never
//! asked about.

use super::*;
use rcore_fs::vfs::FileSystem;
use rcore_fs_ramfs::RamFS;

/// A writable file of `len` bytes on a fresh ramfs.
fn file(len: usize) -> Arc<File> {
    let fs = RamFS::new();
    let root = fs.root_inode();
    let inode = root.create("f", FileType::File, 0o644).unwrap();
    if len > 0 {
        inode.write_at(0, &alloc::vec![0u8; len]).unwrap();
    }
    File::new(inode, OpenFlags::RDWR, String::from("/f"))
}

/// A directory with `names` in it, open for reading.
fn dir(names: &[&str]) -> Arc<File> {
    let fs = RamFS::new();
    let root = fs.root_inode();
    for name in names {
        root.create(name, FileType::File, 0o644).unwrap();
    }
    File::new(root, OpenFlags::RDONLY, String::from("/"))
}

#[test]
fn an_unread_entry_comes_out_again() {
    let d = dir(&["a", "b", "c"]);
    let first = d.read_entry().unwrap();
    let second = d.read_entry().unwrap();
    let third = d.read_entry().unwrap();
    d.unread_entry();
    assert_eq!(d.read_entry().unwrap(), third);
    d.unread_entry();
    d.unread_entry();
    assert_eq!(d.read_entry().unwrap(), second);
    // Reading on from there is the rest of the directory, once each.
    let mut rest = alloc::vec![d.read_entry().unwrap()];
    while let Ok(name) = d.read_entry() {
        rest.push(name);
    }
    assert_eq!(rest.len() + 2, 5, "\".\", \"..\", a, b and c: {:?}", rest);
    assert!(!rest.contains(&first) && !rest.contains(&second));
}

#[test]
fn the_directory_position_is_where_a_seek_resumes() {
    let d = dir(&["a", "b", "c"]);
    let first = d.read_entry().unwrap();
    let second = d.read_entry().unwrap();
    let after_second = d.dir_position();
    let third = d.read_entry().unwrap();
    assert_ne!(
        after_second, 0,
        "the position after an entry is never the start"
    );
    // Seeking to the position reported after the second entry lands on
    // the third, which is the contract `d_off` and `seekdir` rest on.
    File::seek(&d, SeekFrom::Start(after_second)).unwrap();
    assert_eq!(d.read_entry().unwrap(), third);
    // And the positions climb with the entries.
    File::seek(&d, SeekFrom::Start(0)).unwrap();
    assert_eq!(d.read_entry().unwrap(), first);
    let after_first = d.dir_position();
    assert!(after_first < after_second);
    File::seek(&d, SeekFrom::Start(after_first)).unwrap();
    assert_eq!(d.read_entry().unwrap(), second);
}

#[test]
fn unreading_at_the_start_stays_at_the_start() {
    let d = dir(&["a"]);
    let first = d.read_entry().unwrap();
    d.unread_entry();
    d.unread_entry();
    assert_eq!(d.read_entry().unwrap(), first);
}

#[test]
fn an_absolute_seek_lands_where_it_was_told() {
    let f = file(100);
    assert_eq!(File::seek(&f, SeekFrom::Start(0)).unwrap(), 0);
    assert_eq!(File::seek(&f, SeekFrom::Start(42)).unwrap(), 42);
}

#[test]
fn seeking_past_the_end_is_allowed() {
    // POSIX says so, and it is how every sparse-file writer works: seek
    // out past the end, write, and the hole in between reads as zeroes.
    let f = file(10);
    assert_eq!(
        File::seek(&f, SeekFrom::Start(1_000_000)).unwrap(),
        1_000_000
    );
}

#[test]
fn a_relative_seek_starts_from_where_the_file_is() {
    let f = file(100);
    File::seek(&f, SeekFrom::Start(30)).unwrap();
    assert_eq!(File::seek(&f, SeekFrom::Current(10)).unwrap(), 40);
    assert_eq!(File::seek(&f, SeekFrom::Current(-15)).unwrap(), 25);
}

#[test]
fn a_seek_from_the_end_counts_backwards() {
    // `SeekFrom::End(0)` is how a program asks for the file's size, and
    // a negative offset is the normal way to read a trailer.
    let f = file(100);
    assert_eq!(File::seek(&f, SeekFrom::End(0)).unwrap(), 100);
    assert_eq!(File::seek(&f, SeekFrom::End(-20)).unwrap(), 80);
}

#[test]
fn a_relative_seek_below_zero_is_refused_rather_than_wrapped() {
    // This is the one that matters: without the check the position
    // becomes something near 2^64 and every later read and write is
    // aimed at an offset nobody asked for.
    let f = file(100);
    File::seek(&f, SeekFrom::Start(10)).unwrap();
    assert!(matches!(
        File::seek(&f, SeekFrom::Current(-11)),
        Err(LxError::EINVAL)
    ));
    assert_eq!(
        File::seek(&f, SeekFrom::Current(0)).unwrap(),
        10,
        "a refused seek must not have moved the position"
    );
}

#[test]
fn a_seek_before_the_start_of_the_file_is_refused() {
    let f = file(100);
    assert!(matches!(
        File::seek(&f, SeekFrom::End(-101)),
        Err(LxError::EINVAL)
    ));
}

#[test]
fn a_negative_absolute_seek_is_refused() {
    // `lseek(fd, -1, SEEK_SET)`. The offset is signed in the uAPI and
    // arrives here as an enormous `u64`.
    let f = file(100);
    assert!(matches!(
        File::seek(&f, SeekFrom::Start(u64::MAX)),
        Err(LxError::EINVAL)
    ));
    assert!(matches!(
        File::seek(&f, SeekFrom::Start(1 << 63)),
        Err(LxError::EINVAL)
    ));
}

#[test]
fn a_seek_that_would_overflow_is_refused() {
    // Adding to a position near the top of the range.
    //
    // The `checked_add` in `seek` is belt and braces here rather than
    // the thing doing the work: both operands are non-negative (the
    // stored offset can never have its sign bit set, because this very
    // function refuses one), so a wrap always lands negative and the
    // `new_offset < 0` test below catches it either way. Replacing the
    // `checked_add` with a `wrapping_add` therefore changes nothing
    // observable -- worth knowing before anyone goes looking for a test
    // that tells the two apart.
    let f = file(100);
    File::seek(&f, SeekFrom::Start(i64::MAX as u64)).unwrap();
    assert!(matches!(
        File::seek(&f, SeekFrom::Current(1)),
        Err(LxError::EINVAL)
    ));
    assert!(matches!(
        File::seek(&f, SeekFrom::Current(i64::MAX)),
        Err(LxError::EINVAL)
    ));
}

#[test]
fn the_largest_offset_an_off_t_can_name_is_still_a_valid_position() {
    let f = file(0);
    assert_eq!(
        File::seek(&f, SeekFrom::Start(i64::MAX as u64)).unwrap(),
        i64::MAX as u64
    );
}

#[test]
fn a_seek_of_zero_reports_the_position_without_moving_it() {
    // `lseek(fd, 0, SEEK_CUR)` is how every program asks "where am I",
    // and it must not disturb anything.
    let f = file(100);
    File::seek(&f, SeekFrom::Start(37)).unwrap();
    assert_eq!(File::seek(&f, SeekFrom::Current(0)).unwrap(), 37);
    assert_eq!(File::seek(&f, SeekFrom::Current(0)).unwrap(), 37);
}
