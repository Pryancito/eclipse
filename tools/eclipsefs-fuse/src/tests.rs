//! The translation between EclipseFS and FUSE.
//!
//! Everything this driver decides on its own is in four pure functions and
//! nothing read any of them: the errno a failure reaches the program as, the
//! type `ls` prints, the inode number the kernel is told, and the attributes
//! `stat(2)` returns. The rest of the file is the socket and the device.
//!
//! It matters here more than it looks, because this binary exists to produce
//! numbers: `scripts/eclipsefs-linux-bench.sh` runs fio through it to compare
//! EclipseFS with ext4, xfs and btrfs. A wrong block count or a wrong errno
//! does not crash anything, it just makes the comparison say the wrong thing.

use super::*;
use eclipsefs::cowfmt::{handle, FIRST_INO, FIRST_SUBVOL, FS_TREE};

/// An inode with nothing interesting in it, for the fields a test is not about.
fn stat_of(ino: u64, kind: FileKind, mode: u32) -> InodeStat {
    InodeStat {
        ino,
        kind,
        mode,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        nbytes: 0,
        rdev: 0,
        atime: (0, 0),
        mtime: (0, 0),
        ctime: (0, 0),
    }
}

/// Every failure EclipseFS can raise reaches the program as the errno that
/// means it.
///
/// This is the whole of what a program learns when an operation fails, and
/// the two pairs that share a number do so for a reason: a corrupt image is
/// an I/O error from userspace's point of view, and a bad superblock is as
/// invalid as an invalid argument. What must not blur is the three that tell
/// a caller which retry to make -- `ENOTDIR`, `EISDIR` and `ENOTEMPTY` are
/// what `rmdir` and `unlink` are told apart by.
#[test]
fn every_error_the_filesystem_can_raise_has_its_own_errno() {
    assert_eq!(errno(Error::Io), libc::EIO);
    assert_eq!(errno(Error::Corrupt("any")), libc::EIO);
    assert_eq!(errno(Error::BadSuperblock), libc::EINVAL);
    assert_eq!(errno(Error::Unsupported("any")), libc::EOPNOTSUPP);
    assert_eq!(errno(Error::NotFound), libc::ENOENT);
    assert_eq!(errno(Error::Exists), libc::EEXIST);
    assert_eq!(errno(Error::NotDir), libc::ENOTDIR);
    assert_eq!(errno(Error::IsDir), libc::EISDIR);
    assert_eq!(errno(Error::NotEmpty), libc::ENOTEMPTY);
    assert_eq!(errno(Error::NoSpace), libc::ENOSPC);
    assert_eq!(errno(Error::Invalid), libc::EINVAL);
    assert_eq!(errno(Error::NoKey), libc::ENOKEY);

    // The three a caller acts on differently are three different numbers.
    let three = [
        errno(Error::NotDir),
        errno(Error::IsDir),
        errno(Error::NotEmpty),
    ];
    for (i, a) in three.iter().enumerate() {
        for b in &three[i + 1..] {
            assert_ne!(a, b, "two of ENOTDIR/EISDIR/ENOTEMPTY are the same");
        }
    }
}

/// Every kind of file reaches the kernel as its own type, so `ls -l` prints
/// the right character and a program's `S_ISFIFO` answers.
#[test]
fn every_file_kind_reaches_the_kernel_as_its_own_type() {
    let pairs = [
        (FileKind::Regular, FileType::RegularFile),
        (FileKind::Dir, FileType::Directory),
        (FileKind::Symlink, FileType::Symlink),
        (FileKind::CharDevice, FileType::CharDevice),
        (FileKind::BlockDevice, FileType::BlockDevice),
        (FileKind::Fifo, FileType::NamedPipe),
        (FileKind::Socket, FileType::Socket),
    ];
    for (kind, want) in pairs {
        assert_eq!(file_type(kind), want, "{:?}", kind);
    }
    // And no two kinds collapse onto one type: a socket reported as a fifo
    // would make a program open it and hang rather than refuse.
    for (i, (_, a)) in pairs.iter().enumerate() {
        for (_, b) in &pairs[i + 1..] {
            assert_ne!(a, b, "two file kinds share one FUSE type");
        }
    }
}

/// The root swap, both ways, on a filesystem whose root is not 1.
#[test]
fn the_root_swap_is_a_bijection_both_ways() {
    let root = handle(FS_TREE, FIRST_INO);
    assert_eq!(to_fs_ino(root, fuser::FUSE_ROOT_ID), root);
    assert_eq!(to_fuse_ino(root, root), fuser::FUSE_ROOT_ID);
    // Anything else passes through untouched, in both directions.
    for ino in [
        FIRST_INO + 1,
        FIRST_INO + 7,
        handle(FIRST_SUBVOL, FIRST_INO),
    ] {
        assert_eq!(to_fs_ino(root, ino), ino, "{:#x}", ino);
        assert_eq!(to_fuse_ino(root, ino), ino, "{:#x}", ino);
        assert_eq!(to_fs_ino(root, to_fuse_ino(root, ino)), ino, "{:#x}", ino);
    }
    // A classic image numbers its root 1 and the swap is then the identity.
    assert_eq!(to_fs_ino(1, 1), 1);
    assert_eq!(to_fuse_ino(1, 1), fuser::FUSE_ROOT_ID);
}

/// **The premise that makes that swap safe**, which lives in another crate.
///
/// `to_fuse_ino` reports `FUSE_ROOT_ID` for the root and leaves every other
/// number alone, so a file whose own inode number were 1 would reach the
/// kernel wearing the root's number -- and `to_fs_ino` would then redirect
/// every operation on it to the root directory. Nothing in this binary stops
/// that; what stops it is that EclipseFS never allocates an inode below
/// `FIRST_INO`, and `FIRST_INO` is 256.
///
/// So this test holds the premise and not just the conclusion. Both consts are
/// known at compile time, so the premise itself is asserted in a `const` block
/// and lowering `FIRST_INO` to 1 does not reach a test run at all: the crate
/// stops building, with the message below as the reason. The two runtime
/// assertions after it are the same statement about the numbers the filesystem
/// actually hands out, and are what would fail if `handle` -- which is not a
/// `const fn` -- ever started folding a subvolume into the low bits.
#[test]
fn no_inode_eclipsefs_allocates_can_be_mistaken_for_the_fuse_root() {
    const {
        assert!(
            FIRST_INO > fuser::FUSE_ROOT_ID,
            "an inode of the top subvolume could wear FUSE_ROOT_ID"
        )
    };
    // The top subvolume's handles are its inode numbers, so its lowest is the
    // one that could collide; the others carry their subvolume above bit 40
    // and are larger still.
    assert_ne!(handle(FS_TREE, FIRST_INO), fuser::FUSE_ROOT_ID);
    assert_ne!(handle(FIRST_SUBVOL, FIRST_INO), fuser::FUSE_ROOT_ID);
}

/// The file type does not leak into the permission bits.
///
/// `FileAttr::perm` is twelve bits; `st.mode` is the whole mode, type
/// included. Unmasked, a directory's `S_IFDIR` lands in `perm` and every
/// directory comes out with a nonsense mode.
#[test]
fn the_attributes_keep_the_file_type_out_of_the_permission_bits() {
    let root = handle(FS_TREE, FIRST_INO);
    let dir = attr_of(root, &stat_of(root, FileKind::Dir, 0o040_755));
    assert_eq!(dir.perm, 0o755, "the type bits reached perm");
    assert_eq!(dir.kind, FileType::Directory);
    // The twelve bits that do belong there all survive, setuid included.
    let odd = attr_of(root, &stat_of(300, FileKind::Regular, 0o100_7654));
    assert_eq!(odd.perm, 0o7654);
}

/// `blocks` is in 512-byte units whatever `blksize` says, and rounds up.
///
/// `stat(2)` fixes the unit at 512 and `du` divides by it; reporting 4096-byte
/// blocks would make every file look eight times smaller on disk, which is
/// exactly the number this binary exists to measure.
#[test]
fn the_block_count_is_in_512_byte_units_and_rounds_up() {
    let root = handle(FS_TREE, FIRST_INO);
    for (nbytes, want) in [
        (0u64, 0u64),
        (1, 1),
        (511, 1),
        (512, 1),
        (513, 2),
        (4096, 8),
    ] {
        let mut st = stat_of(300, FileKind::Regular, 0o100_644);
        st.nbytes = nbytes;
        let a = attr_of(root, &st);
        assert_eq!(a.blocks, want, "{} bytes", nbytes);
    }
    // And it is `nbytes`, what the file takes on disk, not `size`: a sparse
    // or compressed file is the whole point of asking EclipseFS.
    let mut sparse = stat_of(300, FileKind::Regular, 0o100_644);
    sparse.size = 1 << 20;
    sparse.nbytes = 4096;
    assert_eq!(attr_of(root, &sparse).blocks, 8);
    assert_eq!(attr_of(root, &sparse).size, 1 << 20);
    // `blksize` is the hint for how much to write at once, and stays 4096.
    assert_eq!(attr_of(root, &sparse).blksize, 4096);
}

/// A timestamp survives the trip out, and a time before the epoch becomes
/// the epoch instead of panicking.
///
/// `SystemTime::duration_since` returns an error going backwards, and the
/// conversion takes `unwrap_or_default()`. A `utimensat` with a negative
/// second is a thing a program can ask for, so the branch is reachable.
#[test]
fn a_timestamp_survives_the_trip_and_a_pre_epoch_one_becomes_the_epoch() {
    assert_eq!(to_time(TimeOrNow::SpecificTime(UNIX_EPOCH)), (0, 0));
    let t = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
    assert_eq!(
        to_time(TimeOrNow::SpecificTime(t)),
        (1_700_000_000, 123_456_789)
    );
    assert_eq!(time((1_700_000_000, 123_456_789)), t);
    // Backwards: the epoch, not a panic.
    let before = UNIX_EPOCH - Duration::from_secs(1);
    assert_eq!(to_time(TimeOrNow::SpecificTime(before)), (0, 0));
}

/// The kernel is allowed to cache for a minute because only this process
/// changes the filesystem. If the mount ever became shared, this is the
/// number that would have to go.
#[test]
fn the_attribute_timeout_is_the_one_the_module_documents() {
    assert_eq!(TTL, Duration::from_secs(60));
}
