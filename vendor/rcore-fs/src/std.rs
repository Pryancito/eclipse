use crate::vfs::*;
#[cfg(windows)]
use filetime::FileTime;
use std::io::Error;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
use winapi::shared::minwindef::DWORD;
#[cfg(windows)]
use winapi::um::winnt;

impl std::error::Error for FsError {}

impl From<std::io::Error> for FsError {
    fn from(e: Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => FsError::EntryNotFound,
            // We do not have permission in our fs, just ignore the file
            ErrorKind::PermissionDenied => FsError::EntryNotFound,
            ErrorKind::AlreadyExists => FsError::EntryExist,
            ErrorKind::WouldBlock => FsError::Again,
            ErrorKind::InvalidInput => FsError::InvalidParam,
            ErrorKind::InvalidData => FsError::InvalidParam,
            // The host fs is the device here
            _ => FsError::DeviceError,
        }
    }
}

#[cfg(unix)]
impl From<std::fs::Metadata> for Metadata {
    fn from(m: std::fs::Metadata) -> Self {
        Metadata {
            dev: m.dev() as usize,
            inode: m.ino() as usize,
            size: m.size() as usize,
            blk_size: m.blksize() as usize,
            blocks: m.blocks() as usize,
            atime: Timespec {
                sec: m.atime(),
                nsec: m.atime_nsec() as i32,
            },
            mtime: Timespec {
                sec: m.mtime(),
                nsec: m.mtime_nsec() as i32,
            },
            ctime: Timespec {
                sec: m.ctime(),
                nsec: m.ctime_nsec() as i32,
            },
            type_: match (m.mode() & 0xf000) as _ {
                libc::S_IFCHR => FileType::CharDevice,
                libc::S_IFBLK => FileType::BlockDevice,
                libc::S_IFDIR => FileType::Dir,
                libc::S_IFREG => FileType::File,
                libc::S_IFLNK => FileType::SymLink,
                libc::S_IFSOCK => FileType::Socket,
                _ => unimplemented!("unknown file type"),
            },
            mode: m.mode() as u16 & 0o777,
            nlinks: m.nlink() as usize,
            uid: m.uid() as usize,
            gid: m.gid() as usize,
            rdev: m.rdev() as usize,
        }
    }
}

#[cfg(windows)]
impl From<std::fs::Metadata> for Metadata {
    fn from(m: std::fs::Metadata) -> Self {
        Metadata {
            dev: 0,
            inode: 0,
            size: m.file_size() as usize,
            blk_size: 0,
            blocks: 0,
            atime: {
                let atime = FileTime::from_last_access_time(&m);
                Timespec {
                    sec: atime.unix_seconds(),
                    nsec: atime.nanoseconds() as i32,
                }
            },
            mtime: {
                let mtime = FileTime::from_last_modification_time(&m);
                Timespec {
                    sec: mtime.unix_seconds(),
                    nsec: mtime.nanoseconds() as i32,
                }
            },
            ctime: {
                let mtime = FileTime::from_last_modification_time(&m);
                Timespec {
                    sec: mtime.unix_seconds(),
                    nsec: mtime.nanoseconds() as i32,
                }
            },
            type_: {
                let attr = m.file_attributes() as DWORD;
                if (attr & winnt::FILE_ATTRIBUTE_NORMAL) != 0 {
                    FileType::File
                } else if (attr & winnt::FILE_ATTRIBUTE_DIRECTORY) != 0 {
                    FileType::Dir
                } else if (attr & winnt::FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
                    FileType::SymLink
                } else {
                    unimplemented!("unknown file type")
                }
            },
            mode: 0,
            nlinks: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt;

    /// A path as the C string `libc` wants.
    fn c_path(path: &std::path::Path) -> std::ffi::CString {
        std::ffi::CString::new(path.to_str().expect("a temporary path that is not UTF-8"))
            .expect("a temporary path with a NUL in it")
    }

    /// What this file makes of the host's own metadata for `path`.
    fn of(path: &std::path::Path) -> Metadata {
        Metadata::from(fs::metadata(path).expect("stat of a file this test just made"))
    }

    /// Each host error kind this maps by hand, and what it has to come out as.
    ///
    /// `PermissionDenied` is the one a reader second-guesses, and it is
    /// deliberate: a host directory the image build cannot read is reported as
    /// absent, so the build walks past it instead of dying on it.
    #[test]
    fn each_named_host_error_keeps_the_meaning_it_was_given() {
        let named = [
            (ErrorKind::NotFound, FsError::EntryNotFound),
            (ErrorKind::PermissionDenied, FsError::EntryNotFound),
            (ErrorKind::AlreadyExists, FsError::EntryExist),
            (ErrorKind::WouldBlock, FsError::Again),
            (ErrorKind::InvalidInput, FsError::InvalidParam),
            (ErrorKind::InvalidData, FsError::InvalidParam),
        ];
        for (kind, want) in named {
            assert_eq!(
                FsError::from(Error::from(kind)),
                want,
                "io::ErrorKind::{:?}",
                kind
            );
        }
    }

    /// Everything else is the host filesystem failing the way a device does.
    #[test]
    fn a_kind_nobody_named_is_a_device_error() {
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::BrokenPipe,
            ErrorKind::TimedOut,
            ErrorKind::Interrupted,
            ErrorKind::Other,
        ] {
            assert_eq!(
                FsError::from(Error::from(kind)),
                FsError::DeviceError,
                "io::ErrorKind::{:?}",
                kind
            );
        }
    }

    #[test]
    fn a_files_size_mode_and_link_count_come_across() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("five");
        fs::write(&path, b"hello").expect("writing five bytes");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod 640");
        let md = of(&path);
        assert_eq!(md.size, 5);
        // `& 0o777` and nothing wider: the host keeps the file type in the high
        // bits of the same word (0o100000 for a regular file), and those are
        // not permissions.
        assert_eq!(md.mode, 0o640);
        assert_eq!(md.type_, FileType::File);
        assert_eq!(md.nlinks, 1);
        assert_eq!(md.rdev, 0, "a regular file with a device number");
        assert!(
            md.blk_size >= 512,
            "a preferred block size of {} bytes",
            md.blk_size
        );
    }

    #[test]
    fn a_hard_link_is_counted_on_both_names() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        fs::write(&one, b"x").expect("writing the first name");
        fs::hard_link(&one, &two).expect("a second name for it");
        assert_eq!(of(&one).nlinks, 2);
        assert_eq!(of(&two).nlinks, 2);
    }

    #[test]
    fn a_directory_a_symlink_and_a_device_node_are_told_apart() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        assert_eq!(of(dir.path()).type_, FileType::Dir);
        let target = dir.path().join("target");
        fs::write(&target, b"x").expect("the link's target");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("a symlink to it");
        // `fs::metadata` follows the link; only `symlink_metadata` sees it.
        let itself = Metadata::from(fs::symlink_metadata(&link).expect("lstat of a symlink"));
        assert_eq!(itself.type_, FileType::SymLink);
        assert_eq!(
            of(&link).type_,
            FileType::File,
            "stat of a symlink to a file"
        );
        let null = Metadata::from(fs::metadata("/dev/null").expect("stat of /dev/null"));
        assert_eq!(null.type_, FileType::CharDevice);
        assert_ne!(null.rdev, 0, "a device node with no device number");
    }

    /// Three timestamps of the same shape, so one read out of another's place
    /// is invisible unless they hold different instants: atime and mtime are
    /// set to two fixed ones, and ctime cannot be set at all, which leaves it
    /// at roughly now and tells all three apart.
    #[test]
    fn the_three_timestamps_do_not_get_mixed_up() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("dated");
        fs::write(&path, b"x").expect("writing the file");
        // Microseconds is all `utimes` can ask for, and both ext4 and tmpfs
        // keep more than that.
        let times = [
            libc::timeval {
                tv_sec: 1_000_000,
                tv_usec: 250_000,
            },
            libc::timeval {
                tv_sec: 2_000_000,
                tv_usec: 750_000,
            },
        ];
        let name = c_path(&path);
        assert_eq!(
            unsafe { libc::utimes(name.as_ptr(), times.as_ptr()) },
            0,
            "utimes: {}",
            Error::last_os_error()
        );
        let md = of(&path);
        assert_eq!(md.atime.sec, 1_000_000);
        assert_eq!(md.atime.nsec, 250_000_000);
        assert_eq!(md.mtime.sec, 2_000_000);
        assert_eq!(md.mtime.nsec, 750_000_000);
        assert!(
            md.ctime.sec > 1_700_000_000,
            "ctime came out of one of the other two: {}",
            md.ctime.sec
        );
    }

    #[test]
    fn two_files_in_one_directory_share_a_device_and_not_an_inode() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        fs::write(&one, b"1").expect("the first file");
        fs::write(&two, b"22").expect("the second file");
        let a = of(&one);
        let b = of(&two);
        assert_eq!(a.dev, b.dev, "two files of one directory on two devices");
        assert_ne!(a.inode, b.inode, "two files sharing an inode number");
        assert_eq!(a.inode, of(&one).inode, "one file with two inode numbers");
        assert_eq!(a.size, 1);
        assert_eq!(b.size, 2);
    }

    #[test]
    fn the_owner_is_the_user_running_the_test() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("mine");
        fs::write(&path, b"x").expect("writing the file");
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let md = of(&path);
        assert_eq!(md.uid, uid as usize);
        assert_eq!(md.gid, gid as usize);
        // While those two are equal -- as they are for root -- one read out of
        // the other's place is invisible, so hand the file to another group.
        // Only root may, so where the host says no the two above are all there
        // is.
        let other = if gid == 0 { 1 } else { 0 };
        let name = c_path(&path);
        if unsafe { libc::chown(name.as_ptr(), uid, other) } == 0 {
            let md = of(&path);
            assert_eq!(md.uid, uid as usize, "the owner changed with the group");
            assert_eq!(md.gid, other as usize);
        }
    }
}
