//! File status
//!
//! - stat
//! - lstat
//! - fstat(at)

use super::*;
use linux_object::fs::vfs::{FileType, Metadata};

impl Syscall<'_> {
    /// Works exactly like the stat syscall, but if the file in question is a symbolic link,
    /// information on the link is returned rather than its target.
    /// - `path` – full path to file
    /// - `stat_ptr` – pointer to stat buffer
    pub fn sys_lstat(&self, path: UserInPtr<u8>, stat_ptr: UserOutPtr<Stat>) -> SysResult {
        self.sys_fstatat(
            FileDesc::CWD,
            path,
            stat_ptr,
            AtFlags::SYMLINK_NOFOLLOW.bits(),
        )
    }

    /// Works exactly like the stat syscall except a file descriptor (fd) is provided instead of a path.
    /// - `fd` – file descriptor
    /// - `stat_ptr` – pointer to stat buffer
    pub fn sys_fstat(&self, fd: FileDesc, mut stat_ptr: UserOutPtr<Stat>) -> SysResult {
        info!("fstat: fd={:?}, stat_ptr={:?}", fd, stat_ptr);

        // Every descriptor can be stat'ed, not just the ones backed by an
        // inode: `get_file_like` covers sockets (S_IFSOCK) and the anonymous
        // objects (eventfd, epoll, timerfd...) too. Restricting this to
        // regular files made `fstat` on a socket fail -- and musl implements
        // `fstat` as `fstatat(fd, "", AT_EMPTY_PATH)`, so it failed for every
        // caller, Firefox's IPC channel checks among them.
        let meta = self.linux_process().get_file_like(fd)?.metadata()?;
        stat_ptr.write(meta.into())?;
        Ok(0)
    }

    /// get file status relative to a directory file descriptor
    pub fn sys_fstatat(
        &self,
        dirfd: FileDesc,
        path: UserInPtr<u8>,
        mut stat_ptr: UserOutPtr<Stat>,
        flags: usize,
    ) -> SysResult {
        let flags = at_flags(flags, FSTATAT_FLAGS)?;
        let path = path.as_c_str()?;
        info!(
            "fstatat: dirfd={:?}, path={:?}, stat_ptr={:?}, flags={:?}",
            dirfd, path, stat_ptr, flags
        );

        let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
        let proc = self.linux_process();
        // AT_EMPTY_PATH with an empty path names `dirfd` itself. This is not a
        // corner case: musl implements `fstat(fd, ...)` exactly this way, so
        // without it `fstat` fails on every descriptor that is not a regular
        // file — sockets included, which is what Firefox stats on the handles
        // it receives over its IPC channels.
        let stat = if flags.contains(AtFlags::EMPTY_PATH) && path.is_empty() {
            if dirfd == FileDesc::CWD {
                proc.root_inode()
                    .lookup(&proc.current_working_directory())?
                    .metadata()?
            } else {
                proc.get_file_like(dirfd)?.metadata()?
            }
        } else {
            proc.lookup_inode_at(dirfd, path, follow)?.metadata()?
        };
        stat_ptr.write(stat.into())?;
        Ok(0)
    }

    /// Returns information about a file in a structure named stat.
    /// - `path` – pointer to the name of the file
    /// - `stat_ptr` –  pointer to the structure to receive file information
    pub fn sys_stat(&self, path: UserInPtr<u8>, stat_ptr: UserOutPtr<Stat>) -> SysResult {
        self.sys_fstatat(FileDesc::CWD, path, stat_ptr, 0)
    }

    /// statx system call
    pub fn sys_statx(
        &self,
        dirfd: FileDesc,
        pathname: UserInPtr<u8>,
        flags: usize,
        _mask: u32,
        mut statxbuf: UserOutPtr<Statx>,
    ) -> SysResult {
        let flags = at_flags(flags, STATX_FLAGS)?;
        let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
        let proc = self.linux_process();
        let meta = if flags.contains(AtFlags::EMPTY_PATH)
            && (pathname.is_null() || pathname.as_c_str().map(|s| s.is_empty()).unwrap_or(false))
        {
            if dirfd == FileDesc::CWD {
                proc.root_inode()
                    .lookup(&proc.current_working_directory())?
                    .metadata()?
            } else {
                // AT_EMPTY_PATH names the descriptor itself, whatever kind it
                // is — this is the path musl's `fstat` takes.
                proc.get_file_like(dirfd)?.metadata()?
            }
        } else {
            let path = pathname.as_c_str()?;
            proc.lookup_inode_at(dirfd, path, follow)?.metadata()?
        };

        statxbuf.write(meta.into())?;
        Ok(0)
    }
}

/// `struct timespec` as `struct stat` carries it.
///
/// `tv_sec` is a **signed** `long` in the kernel ABI, and this used to be
/// `linux_object::time::TimeSpec`, whose `sec` is a `usize`. Same sixteen
/// bytes, opposite reading of the top bit: a timestamp before 1970 -- which a
/// filesystem stores signed, and which arrives here signed -- came out of
/// `stat` as roughly 584 billion years in the future. `statx`, in this same
/// file, always had it right (`stx_atime.tv_sec` is an `i64`), which is how
/// the two answers to the same question came to disagree.
#[cfg(not(target_arch = "mips"))]
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct TimeSpec {
    /// seconds, signed: times before the epoch are negative
    pub sec: i64,
    /// nanoseconds
    pub nsec: i64,
}

#[cfg(not(target_arch = "mips"))]
impl From<linux_object::fs::vfs::Timespec> for TimeSpec {
    fn from(t: linux_object::fs::vfs::Timespec) -> Self {
        TimeSpec {
            sec: t.sec,
            nsec: i64::from(t.nsec),
        }
    }
}

#[cfg(target_arch = "mips")]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct TimeSpec {
    pub sec: i32,
    pub nsec: i32,
}

#[cfg(target_arch = "mips")]
impl From<linux_object::fs::vfs::TimeSpec> for TimeSpec {
    fn from(t: TimeSpec) -> Self {
        TimeSpec {
            sec: t.sec as _,
            nsec: t.nsec as _,
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// ID of device containing file
    dev: u64,
    /// inode number
    ino: u64,
    /// number of hard links
    nlink: u64,

    /// file type and mode
    mode: StatMode,
    /// user ID of owner
    uid: u32,
    /// group ID of owner
    gid: u32,
    /// padding
    _pad0: u32,
    /// device ID (if special file)
    rdev: u64,
    /// total size, in bytes
    size: u64,
    /// blocksize for filesystem I/O
    blksize: u64,
    /// number of 512B blocks allocated
    blocks: u64,

    /// last access time
    atime: TimeSpec,
    /// last modification time
    mtime: TimeSpec,
    /// last status change time
    ctime: TimeSpec,
}

#[cfg(target_arch = "mips")]
#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// ID of device containing file
    dev: u64,
    /// padding
    _pad0: u64,
    /// inode number
    ino: u64,
    /// file type and mode
    mode: StatMode,
    /// number of hard links
    nlink: u32,

    /// user ID of owner
    uid: u32,
    /// group ID of owner
    gid: u32,
    /// device ID (if special file)
    rdev: u64,
    /// padding
    _pad1: u64,
    /// total size, in bytes
    size: u64,

    /// last access time
    atime: TimeSpec,
    /// last modification time
    mtime: TimeSpec,
    /// last status change time
    ctime: TimeSpec,

    /// blocksize for filesystem I/O
    blksize: u32,
    /// padding
    _pad2: u32,
    /// number of 512B blocks allocated
    blocks: u64,
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "mips")))]
#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// ID of device containing file
    dev: u64,
    /// inode number
    ino: u64,
    /// file type and mode
    mode: StatMode,
    /// number of hard links
    nlink: u32,

    /// user ID of owner
    uid: u32,
    /// group ID of owner
    gid: u32,
    /// device ID (if special file)
    rdev: u64,
    /// padding
    _pad0: u64,
    /// total size, in bytes
    size: u64,
    /// blocksize for filesystem I/O
    blksize: u32,
    /// padding
    _pad1: u32,
    /// number of 512B blocks allocated
    blocks: u64,

    /// last access time
    atime: TimeSpec,
    /// last modification time
    mtime: TimeSpec,
    /// last status change time
    ctime: TimeSpec,
}

impl From<Metadata> for Stat {
    fn from(info: Metadata) -> Self {
        Stat {
            dev: info.dev as _,
            ino: info.inode as _,
            mode: StatMode::from_type_mode(info.type_, info.mode as _),
            nlink: info.nlinks as _,
            uid: info.uid as _,
            gid: info.gid as _,
            rdev: info.rdev as _,
            size: info.size as _,
            blksize: info.blk_size as _,
            blocks: info.blocks as _,
            atime: info.atime.into(),
            mtime: info.mtime.into(),
            ctime: info.ctime.into(),
            _pad0: 0,
            #[cfg(not(target_arch = "x86_64"))]
            _pad1: 0,
            #[cfg(target_arch = "mips")]
            _pad2: 0,
        }
    }
}

bitflags! {
    pub struct StatMode: u32 {
        /// Type
        const TYPE_MASK = 0o170_000;
        /// FIFO
        const FIFO  = 0o010_000;
        /// character device
        const CHAR  = 0o020_000;
        /// directory
        const DIR   = 0o040_000;
        /// block device
        const BLOCK = 0o060_000;
        /// ordinary regular file
        const FILE  = 0o100_000;
        /// symbolic link
        const LINK  = 0o120_000;
        /// socket
        const SOCKET = 0o140_000;

        /// Set-user-ID on execution.
        const SET_UID = 0o4_000;
        /// Set-group-ID on execution.
        const SET_GID = 0o2_000;
        /// Restricted deletion flag (`S_ISVTX`), the sticky bit.
        const STICKY = 0o1_000;

        /// Read, write, execute/search by owner.
        const OWNER_MASK = 0o700;
        /// Read permission, owner.
        const OWNER_READ = 0o400;
        /// Write permission, owner.
        const OWNER_WRITE = 0o200;
        /// Execute/search permission, owner.
        const OWNER_EXEC = 0o100;

        /// Read, write, execute/search by group.
        const GROUP_MASK = 0o70;
        /// Read permission, group.
        const GROUP_READ = 0o40;
        /// Write permission, group.
        const GROUP_WRITE = 0o20;
        /// Execute/search permission, group.
        const GROUP_EXEC = 0o10;

        /// Read, write, execute/search by others.
        const OTHER_MASK = 0o7;
        /// Read permission, others.
        const OTHER_READ = 0o4;
        /// Write permission, others.
        const OTHER_WRITE = 0o2;
        /// Execute/search permission, others.
        const OTHER_EXEC = 0o1;
    }
}

impl StatMode {
    /// Every permission bit `st_mode` can carry: the twelve of `0o7777`.
    ///
    /// `S_ISVTX` (0o1000), the sticky bit, had no flag in this type at all,
    /// and `from_bits_truncate` drops what it does not know -- so the sticky
    /// bit **vanished from every stat in the system**. `/tmp` in mode 1777
    /// read back as 777, on the way out of a filesystem that stores the bit
    /// faithfully and a `chmod` that writes it. That is what `ls -ld` shows
    /// and what a program checks before it will use a shared directory.
    pub const PERMISSION_MASK: u32 = 0o7777;

    /// `st_mode`: the file type from `type_` and the permission bits from
    /// `mode`, which is the only place either may come from.
    ///
    /// The mask is not decoration. `Metadata::mode` is a `u16`, wide enough
    /// to hold `S_IFMT` bits, and a filesystem that leaves them in there --
    /// several store `st_mode` whole -- would otherwise have them read as a
    /// **type** and ORed into the answer, so one inode could come back as two
    /// kinds of file at once.
    fn from_type_mode(type_: FileType, mode: u16) -> Self {
        let type_ = match type_ {
            FileType::File => StatMode::FILE,
            FileType::Dir => StatMode::DIR,
            FileType::SymLink => StatMode::LINK,
            FileType::CharDevice => StatMode::CHAR,
            FileType::BlockDevice => StatMode::BLOCK,
            FileType::Socket => StatMode::SOCKET,
            FileType::NamedPipe => StatMode::FIFO,
        };
        let mode = StatMode::from_bits_truncate(u32::from(mode) & Self::PERMISSION_MASK);
        type_ | mode
    }
}

const STATX_BASIC_STATS: u32 = 0x07ff;

/// timestamp structure for statx
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StatxTimestamp {
    /// seconds
    pub tv_sec: i64,
    /// nanoseconds
    pub tv_nsec: u32,
    /// reserved
    pub __reserved: i32,
}

/// statx structure
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct Statx {
    /// mask
    pub stx_mask: u32,
    /// block size
    pub stx_blksize: u32,
    /// attributes
    pub stx_attributes: u64,
    /// hard links
    pub stx_nlink: u32,
    /// owner uid
    pub stx_uid: u32,
    /// owner gid
    pub stx_gid: u32,
    /// mode
    pub stx_mode: u16,
    /// spare
    pub __spare0: [u16; 1],
    /// inode number
    pub stx_ino: u64,
    /// file size
    pub stx_size: u64,
    /// number of blocks
    pub stx_blocks: u64,
    /// attributes mask
    pub stx_attributes_mask: u64,
    /// access time
    pub stx_atime: StatxTimestamp,
    /// birth time
    pub stx_btime: StatxTimestamp,
    /// change time
    pub stx_ctime: StatxTimestamp,
    /// modification time
    pub stx_mtime: StatxTimestamp,
    /// rdev major
    pub stx_rdev_major: u32,
    /// rdev minor
    pub stx_rdev_minor: u32,
    /// dev major
    pub stx_dev_major: u32,
    /// dev minor
    pub stx_dev_minor: u32,
    /// spare2
    pub __spare2: [u64; 14],
}

impl From<Metadata> for Statx {
    fn from(info: Metadata) -> Self {
        let dev_major = ((info.dev >> 8) & 0xfff) as u32;
        let dev_minor = (info.dev & 0xff) as u32;
        let rdev_major = ((info.rdev >> 8) & 0xfff) as u32;
        let rdev_minor = (info.rdev & 0xff) as u32;

        Statx {
            stx_mask: STATX_BASIC_STATS,
            stx_blksize: info.blk_size as u32,
            stx_attributes: 0,
            stx_nlink: info.nlinks as u32,
            stx_uid: info.uid as u32,
            stx_gid: info.gid as u32,
            stx_mode: StatMode::from_type_mode(info.type_, info.mode as _).bits() as u16,
            __spare0: [0; 1],
            stx_ino: info.inode as u64,
            stx_size: info.size as u64,
            stx_blocks: info.blocks as u64,
            stx_attributes_mask: 0,
            stx_atime: StatxTimestamp {
                tv_sec: info.atime.sec,
                tv_nsec: info.atime.nsec as u32,
                __reserved: 0,
            },
            stx_btime: StatxTimestamp {
                tv_sec: info.ctime.sec,
                tv_nsec: info.ctime.nsec as u32,
                __reserved: 0,
            },
            stx_ctime: StatxTimestamp {
                tv_sec: info.ctime.sec,
                tv_nsec: info.ctime.nsec as u32,
                __reserved: 0,
            },
            stx_mtime: StatxTimestamp {
                tv_sec: info.mtime.sec,
                tv_nsec: info.mtime.nsec as u32,
                __reserved: 0,
            },
            stx_rdev_major: rdev_major,
            stx_rdev_minor: rdev_minor,
            stx_dev_major: dev_major,
            stx_dev_minor: dev_minor,
            __spare2: [0; 14],
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod abi_tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn stat_layout_is_stable() {
        // Lock layout: userspace musl binaries depend on this size/alignment.
        assert_eq!(size_of::<Stat>(), 120);
        assert_eq!(align_of::<Stat>(), 8);
    }
}

/// What `st_mode` carries out of an inode, and what it must not.
///
/// `StatMode` is a `bitflags` set and the conversion ran `from_bits_truncate`
/// over the filesystem's `mode` -- which drops, in silence, every bit the set
/// does not name. It did not name `S_ISVTX`.
#[cfg(test)]
mod mode_tests {
    use super::*;
    use alloc::vec::Vec;

    /// The one that is visible from a shell. A sticky directory is how every
    /// shared temp directory on the system says "you may only delete your
    /// own", and it was read back as an ordinary world-writable directory.
    #[test]
    fn the_sticky_bit_survives_a_stat() {
        let mode = StatMode::from_type_mode(FileType::Dir, 0o1777);
        assert!(mode.contains(StatMode::STICKY));
        assert_eq!(mode.bits(), 0o41777, "/tmp reads as {:o}", mode.bits());
    }

    /// The test that would have caught it by construction: `st_mode`'s low
    /// twelve bits are all permission bits, so all twelve have to be named or
    /// `from_bits_truncate` eats one.
    #[test]
    fn every_permission_bit_of_0o7777_is_named() {
        let named = StatMode::SET_UID
            | StatMode::SET_GID
            | StatMode::STICKY
            | StatMode::OWNER_MASK
            | StatMode::GROUP_MASK
            | StatMode::OTHER_MASK;
        assert_eq!(named.bits(), StatMode::PERMISSION_MASK);
        // And the round trip that the conversion actually makes.
        for bit in 0..12 {
            let m = 1u16 << bit;
            assert_eq!(
                StatMode::from_type_mode(FileType::File, m).bits() & 0o7777,
                u32::from(m),
                "bit {} of the mode was dropped",
                bit
            );
        }
    }

    #[test]
    fn setuid_and_setgid_survive() {
        let mode = StatMode::from_type_mode(FileType::File, 0o6755);
        assert!(mode.contains(StatMode::SET_UID));
        assert!(mode.contains(StatMode::SET_GID));
        assert_eq!(mode.bits(), 0o106755);
    }

    /// One `S_IFMT` value each, and no two alike: userspace switches on this.
    #[test]
    fn every_file_type_gets_its_own_s_ifmt() {
        let types = [
            (FileType::File, 0o100_000),
            (FileType::Dir, 0o40_000),
            (FileType::SymLink, 0o120_000),
            (FileType::CharDevice, 0o20_000),
            (FileType::BlockDevice, 0o60_000),
            (FileType::Socket, 0o140_000),
            (FileType::NamedPipe, 0o10_000),
        ];
        let mut seen = Vec::new();
        for (type_, ifmt) in types {
            let mode = StatMode::from_type_mode(type_, 0o644);
            assert_eq!(
                (mode & StatMode::TYPE_MASK).bits(),
                ifmt,
                "{:?} reported {:o}",
                type_,
                mode.bits()
            );
            assert_eq!(mode.bits() & 0o7777, 0o644, "{:?} lost its mode", type_);
            assert!(!seen.contains(&ifmt), "{:?} shares its S_IFMT", type_);
            seen.push(ifmt);
        }
    }

    /// `Metadata::mode` is a `u16`, wide enough for the `S_IFMT` bits, and
    /// filesystems that store `st_mode` whole do leave them in it. Without
    /// the mask those are read as a type and ORed in, so one inode comes back
    /// as two kinds of file: `S_IFREG | S_IFDIR` is neither.
    #[test]
    fn the_type_never_comes_from_the_mode() {
        let mode = StatMode::from_type_mode(FileType::File, 0o40_755);
        assert_eq!((mode & StatMode::TYPE_MASK).bits(), 0o100_000);
        assert_eq!(mode.bits(), 0o100_755);
        // And the mode a filesystem stores whole still yields its own type.
        let dir = StatMode::from_type_mode(FileType::Dir, 0o40_755 & 0o7777);
        assert_eq!(dir.bits(), 0o40_755);
    }
}

/// The two ways an inode reaches userspace -- `struct stat` and
/// `struct statx` -- are two `From<Metadata>` impls, written apart. These pin
/// what each carries and, where both carry it, that they agree.
#[cfg(test)]
mod stat_conversion_tests {
    use super::*;
    use linux_object::fs::vfs::Timespec;

    const DEV: usize = (0x5 << 8) | 0x2b;
    const RDEV: usize = (0x1 << 8) | 0x3; // /dev/null

    fn meta() -> Metadata {
        Metadata {
            dev: DEV,
            inode: 0x2bad,
            size: 4097,
            blk_size: 512,
            blocks: 9,
            atime: Timespec { sec: 111, nsec: 1 },
            mtime: Timespec { sec: 222, nsec: 2 },
            ctime: Timespec { sec: 333, nsec: 3 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 3,
            uid: 1000,
            gid: 1001,
            rdev: RDEV,
        }
    }

    #[test]
    fn stat_carries_every_field_of_the_metadata() {
        let m = meta();
        let s: Stat = m.clone().into();
        assert_eq!(s.dev, DEV as _);
        assert_eq!(s.ino, 0x2bad);
        assert_eq!(s.nlink, 3);
        assert_eq!(s.mode.bits(), 0o100_644);
        assert_eq!(s.uid, 1000);
        assert_eq!(s.gid, 1001);
        assert_eq!(s.rdev, RDEV as _);
        assert_eq!(s.size, 4097);
        assert_eq!(s.blksize, 512);
        assert_eq!(s.blocks, 9);
        assert_eq!(s.atime.sec, 111);
        assert_eq!(s.mtime.sec, 222);
        assert_eq!(s.ctime.sec, 333);
        assert_eq!(s.atime.nsec, 1);
        assert_eq!(s.mtime.nsec, 2);
        assert_eq!(s.ctime.nsec, 3);
    }

    #[test]
    fn stat_and_statx_agree_on_every_field_they_share() {
        let m = meta();
        let s: Stat = m.clone().into();
        let x: Statx = m.into();
        assert_eq!(u64::from(s.mode.bits()), u64::from(x.stx_mode));
        assert_eq!(s.nlink as u64, u64::from(x.stx_nlink));
        assert_eq!(u64::from(s.uid), u64::from(x.stx_uid));
        assert_eq!(u64::from(s.gid), u64::from(x.stx_gid));
        assert_eq!(s.ino as u64, x.stx_ino);
        assert_eq!(s.size as u64, x.stx_size);
        assert_eq!(s.blocks as u64, x.stx_blocks);
        assert_eq!(s.blksize as u64, u64::from(x.stx_blksize));
        assert_eq!(s.atime.sec, x.stx_atime.tv_sec);
        assert_eq!(s.mtime.sec, x.stx_mtime.tv_sec);
        assert_eq!(s.ctime.sec, x.stx_ctime.tv_sec);
        assert_eq!(s.atime.nsec as u32, x.stx_atime.tv_nsec);
    }

    /// `Metadata` packs a device as `(major << 8) | minor` and `statx` is the
    /// one caller that takes it apart again, so the two have to mean the same
    /// thing by it.
    #[test]
    fn statx_splits_dev_and_rdev_the_way_the_metadata_packs_them() {
        let x: Statx = meta().into();
        assert_eq!(x.stx_dev_major, 0x5);
        assert_eq!(x.stx_dev_minor, 0x2b);
        assert_eq!(x.stx_rdev_major, 0x1);
        assert_eq!(x.stx_rdev_minor, 0x3);
        assert_eq!(
            (u64::from(x.stx_dev_major) << 8) | u64::from(x.stx_dev_minor),
            DEV as u64
        );
    }

    /// `stx_mask` is the kernel saying which fields it filled. `stx_btime`
    /// holds a copy of `ctime` because nothing here records a birth time, so
    /// the mask must not claim it -- a caller that trusts a made-up birth
    /// time is worse off than one that knows there is none.
    #[test]
    fn statx_reports_the_basic_stats_and_no_birth_time() {
        const STATX_BTIME: u32 = 0x800;
        let x: Statx = meta().into();
        assert_eq!(x.stx_mask, STATX_BASIC_STATS);
        assert_eq!(x.stx_mask & STATX_BTIME, 0);
        assert_eq!(x.stx_attributes, 0);
        assert_eq!(x.stx_attributes_mask, 0);
    }

    /// `stat`'s `tv_sec` used to be a `usize`, so this came back as
    /// 18446744073709550616 and `ls -l` printed a year in the hundreds of
    /// billions. `statx` in the same file always answered -1000.
    #[test]
    fn a_timestamp_before_the_epoch_stays_negative_in_both_buffers() {
        let m = Metadata {
            atime: Timespec {
                sec: -1000,
                nsec: 7,
            },
            ..meta()
        };
        let s: Stat = m.clone().into();
        let x: Statx = m.into();
        assert_eq!(s.atime.sec, -1000);
        assert_eq!(s.atime.nsec, 7);
        assert_eq!(x.stx_atime.tv_sec, -1000);
        assert_eq!(s.atime.sec, x.stx_atime.tv_sec);
    }

    #[test]
    fn the_sticky_bit_reaches_both_buffers() {
        let m = Metadata {
            type_: FileType::Dir,
            mode: 0o1777,
            ..meta()
        };
        let s: Stat = m.clone().into();
        let x: Statx = m.into();
        assert_eq!(s.mode.bits(), 0o41777);
        assert_eq!(x.stx_mode, 0o41777);
    }
}
