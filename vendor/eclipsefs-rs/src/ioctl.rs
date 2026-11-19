//! What a mount offers past `read`/`write`, in the bytes Linux passes:
//! the filesystem ioctls (with the numbers and argument layouts of Linux
//! and btrfs, so `chattr`, `cp --reflink`, `duperemove` and btrfs-progs
//! speak to it unchanged), extended attributes, `fallocate` and
//! `lseek(SEEK_DATA/SEEK_HOLE)`.
//!
//! Both mounts of EclipseFS run this: the kernel's own, and the process
//! that serves a volume (`tools/eclipse-btrfs`'s `eclipse-eclipsefs`). Each
//! says through a [`Caller`] what only it knows: who is asking, the other
//! open files an ioctl names, the page cache above the filesystem, the
//! devices a path names. Answers are Linux `errno` numbers.
//!
//! | ioctl | does |
//! |---|---|
//! | `FS_IOC_GETFLAGS`/`SETFLAGS` | `lsattr`/`chattr` (immutable, append, no-COW, ...) |
//! | `FICLONE`/`FICLONERANGE` | reflink: the destination shares the source's blocks |
//! | `FIDEDUPERANGE` | share the blocks of ranges that hold the same bytes |
//! | `FS_IOC_FIEMAP` | `filefrag`: the file's extents (unwritten, compressed, shared, inline) |
//! | `BTRFS_IOC_DEFRAG`, `DEFRAG_RANGE` | `btrfs filesystem defragment [-c]`: contiguous (or recompressed) data |
//! | `BTRFS_IOC_SUBVOL_CREATE(_V2)` | new subvolume |
//! | `BTRFS_IOC_SNAP_CREATE(_V2)` | snapshot (`BTRFS_SUBVOL_RDONLY` for a read-only one) |
//! | `BTRFS_IOC_SNAP_DESTROY(_V2)` | delete a subvolume by name |
//! | `BTRFS_IOC_SUBVOL_GETFLAGS`/`SETFLAGS` | a subvolume's read-only flag |
//! | `BTRFS_IOC_DEFAULT_SUBVOL` | the subvolume mounted as `/` next time |
//! | `BTRFS_IOC_GET_SUBVOL_INFO`, `BTRFS_IOC_INO_LOOKUP` | `btrfs subvolume show` |
//! | `BTRFS_IOC_SYNC`, `START_SYNC`, `WAIT_SYNC` | commit |
//! | `BTRFS_IOC_FS_INFO`, `BTRFS_IOC_DEV_INFO`, `GET_FEATURES` | `btrfs filesystem show` |
//! | `BTRFS_IOC_SCRUB` | verify (and repair from another copy) every block |
//! | `BTRFS_IOC_QUOTA_CTL`, `QUOTA_RESCAN(_STATUS/_WAIT)`, `QGROUP_LIMIT` | quotas |
//! | `BTRFS_IOC_ADD_DEV`, `RM_DEV(_V2)`, `DEV_REPLACE` | devices of a volume |
//! | `BTRFS_IOC_RESIZE` | `btrfs filesystem resize`: grow, or shrink moving what is past the end |
//! | `BTRFS_IOC_BALANCE_V2` | balance, with `convert` to another profile |
//! | `BTRFS_IOC_SEND` | send stream of a read-only snapshot into a file (the kernel's) |
//! | `EFS_IOC_RECEIVE` | receive such a stream into a directory (the kernel's; EclipseFS's own) |
//! | `EFS_IOC_SET_TIER`, `GET_TIER` | a device's tier: fast (SSD) or slow (HDD) (EclipseFS's own) |
//! | `EFS_IOC_TIER_MIGRATE` | move full data chunks to the slow devices (EclipseFS's own) |
//! | `FS_IOC_SET_ENCRYPTION_POLICY`, `GET_ENCRYPTION_POLICY(_EX)` | fscrypt v2 policy of an empty directory |
//! | `FS_IOC_ADD_ENCRYPTION_KEY`, `REMOVE_ENCRYPTION_KEY(_ALL_USERS)` | master keys (`fscrypt unlock`/`lock`) |
//! | `FS_IOC_GET_ENCRYPTION_KEY_STATUS`, `GET_ENCRYPTION_NONCE` | `fscrypt status` |
//!
//! Every ioctl that changes the filesystem refuses on a read-only mount
//! (`EROFS`) and asks for what Linux asks for: `CAP_SYS_ADMIN` for devices,
//! balance, scrub, quotas, deleting subvolumes and the default subvolume;
//! ownership (or `CAP_FOWNER`) for flags; `CAP_LINUX_IMMUTABLE` to change
//! the immutable or append-only flag.
//!
//! The page cache sits above the filesystem, so whatever changes a file's
//! blocks underneath it (a reflink, a punched hole) has the caller reload
//! the cached pages of that range ([`Caller::reload`]), and whatever reads
//! a file's blocks on its own (the source of a reflink) first has it write
//! back what a shared mapping changed ([`Caller::flush`]).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryInto;

use crate::cowfmt::{handle, split, FIRST_INO, FS_TREE};
use crate::{
    BlockDevice, Compression, EclipseFS, EncryptionPolicy, Error, FileKind, InodeStat, Profile,
    Tier,
};

/// A Linux `errno`, positive.
pub type Errno = i32;

/// The `errno` numbers answered here (`include/uapi/asm-generic/errno*.h`).
pub mod errno {
    use super::Errno;
    pub const EPERM: Errno = 1;
    pub const ENOENT: Errno = 2;
    pub const EIO: Errno = 5;
    pub const ENXIO: Errno = 6;
    pub const E2BIG: Errno = 7;
    pub const EBADF: Errno = 9;
    pub const ENOMEM: Errno = 12;
    pub const EACCES: Errno = 13;
    pub const ENOTBLK: Errno = 15;
    pub const EEXIST: Errno = 17;
    pub const EXDEV: Errno = 18;
    pub const ENODEV: Errno = 19;
    pub const ENOTDIR: Errno = 20;
    pub const EISDIR: Errno = 21;
    pub const EINVAL: Errno = 22;
    pub const ENOTTY: Errno = 25;
    pub const ENOSPC: Errno = 28;
    pub const EROFS: Errno = 30;
    pub const ERANGE: Errno = 34;
    pub const ENAMETOOLONG: Errno = 36;
    pub const ENOTEMPTY: Errno = 39;
    pub const EBADR: Errno = 53;
    pub const ENODATA: Errno = 61;
    pub const EOVERFLOW: Errno = 75;
    pub const EOPNOTSUPP: Errno = 95;
    pub const ENOTCONN: Errno = 107;
    pub const ENOKEY: Errno = 126;
}
use errno::*;

// Linux (`include/uapi/linux/fs.h`).
pub const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
pub const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
pub const FS_IOC32_GETFLAGS: u32 = 0x8004_6601;
pub const FS_IOC32_SETFLAGS: u32 = 0x4004_6602;
pub const FICLONE: u32 = 0x4004_9409;
pub const FICLONERANGE: u32 = 0x4020_940d;
pub const FIDEDUPERANGE: u32 = 0xc018_9436;
pub const FS_IOC_FIEMAP: u32 = 0xc020_660b;

/// `FICLONE` / `FICLONERANGE` / `FIDEDUPERANGE`: VFS ioctls. On a filesystem
/// without clone support Linux answers `EOPNOTSUPP` so `cp --reflink=auto`
/// and friends fall back to a byte copy — not `EINVAL`.
pub fn is_vfs_clone_ioctl(cmd: u32) -> bool {
    matches!(cmd, FICLONE | FICLONERANGE | FIDEDUPERANGE)
}

// fscrypt (`include/uapi/linux/fscrypt.h`). Only v2 policies (key
// identifiers): v1 policies and keyring keys are deprecated on Linux.
pub const FS_IOC_SET_ENCRYPTION_POLICY: u32 = 0x800c_6613;
pub const FS_IOC_GET_ENCRYPTION_POLICY: u32 = 0x400c_6615;
pub const FS_IOC_GET_ENCRYPTION_POLICY_EX: u32 = 0xc009_6616;
pub const FS_IOC_ADD_ENCRYPTION_KEY: u32 = 0xc050_6617;
pub const FS_IOC_REMOVE_ENCRYPTION_KEY: u32 = 0xc040_6618;
pub const FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS: u32 = 0xc040_6619;
pub const FS_IOC_GET_ENCRYPTION_KEY_STATUS: u32 = 0xc080_661a;
pub const FS_IOC_GET_ENCRYPTION_NONCE: u32 = 0x8010_661b;

pub const FSCRYPT_POLICY_V2: u8 = 2;
/// `struct fscrypt_policy_v2`.
pub const FSCRYPT_POLICY_V2_SIZE: usize = 24;
pub const FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER: u32 = 2;
/// `struct fscrypt_key_specifier`.
pub const FSCRYPT_KEY_SPEC: usize = 40;
/// `struct fscrypt_add_key_arg` before the raw key.
pub const FSCRYPT_ADD_KEY_ARG: usize = 80;
pub const FSCRYPT_MAX_KEY_SIZE: usize = 64;
pub const FSCRYPT_KEY_STATUS_ABSENT: u32 = 1;
pub const FSCRYPT_KEY_STATUS_PRESENT: u32 = 2;
pub const FSCRYPT_KEY_STATUS_FLAG_ADDED_BY_SELF: u32 = 1;

// btrfs (`include/uapi/linux/btrfs.h`).
pub const BTRFS_IOC_SNAP_CREATE: u32 = 0x5000_9401;
pub const BTRFS_IOC_DEFRAG: u32 = 0x5000_9402;
pub const BTRFS_IOC_RESIZE: u32 = 0x5000_9403;
pub const BTRFS_IOC_SYNC: u32 = 0x0000_9408;
pub const BTRFS_IOC_ADD_DEV: u32 = 0x5000_940a;
pub const BTRFS_IOC_RM_DEV: u32 = 0x5000_940b;
pub const BTRFS_IOC_SUBVOL_CREATE: u32 = 0x5000_940e;
pub const BTRFS_IOC_SNAP_DESTROY: u32 = 0x5000_940f;
pub const BTRFS_IOC_INO_LOOKUP: u32 = 0xd000_9412;
pub const BTRFS_IOC_DEFAULT_SUBVOL: u32 = 0x4008_9413;
pub const BTRFS_IOC_DEFRAG_RANGE: u32 = 0x4030_9410;
pub const BTRFS_IOC_WAIT_SYNC: u32 = 0x4008_9416;
pub const BTRFS_IOC_SNAP_CREATE_V2: u32 = 0x5000_9417;
pub const BTRFS_IOC_START_SYNC: u32 = 0x8008_9418;
pub const BTRFS_IOC_SUBVOL_CREATE_V2: u32 = 0x5000_9418;
pub const BTRFS_IOC_SUBVOL_GETFLAGS: u32 = 0x8008_9419;
pub const BTRFS_IOC_SUBVOL_SETFLAGS: u32 = 0x4008_941a;
pub const BTRFS_IOC_SCRUB: u32 = 0xc400_941b;
pub const BTRFS_IOC_SCRUB_CANCEL: u32 = 0x0000_941c;
pub const BTRFS_IOC_SCRUB_PROGRESS: u32 = 0xc400_941d;
pub const BTRFS_IOC_DEV_INFO: u32 = 0xd000_941e;
pub const BTRFS_IOC_FS_INFO: u32 = 0x8400_941f;
pub const BTRFS_IOC_BALANCE_V2: u32 = 0xc400_9420;
pub const BTRFS_IOC_BALANCE_CTL: u32 = 0x4004_9421;
pub const BTRFS_IOC_BALANCE_PROGRESS: u32 = 0x8400_9422;
pub const BTRFS_IOC_SEND: u32 = 0x4048_9426;
pub const BTRFS_IOC_QUOTA_CTL: u32 = 0xc010_9428;
pub const BTRFS_IOC_QGROUP_LIMIT: u32 = 0x8030_942b;
pub const BTRFS_IOC_QUOTA_RESCAN: u32 = 0x4040_942c;
pub const BTRFS_IOC_QUOTA_RESCAN_STATUS: u32 = 0x8040_942d;
pub const BTRFS_IOC_QUOTA_RESCAN_WAIT: u32 = 0x0000_942e;
pub const BTRFS_IOC_DEV_REPLACE: u32 = 0xca28_9435;
pub const BTRFS_IOC_GET_FEATURES: u32 = 0x8018_9439;
pub const BTRFS_IOC_RM_DEV_V2: u32 = 0x5000_943a;
pub const BTRFS_IOC_GET_SUBVOL_INFO: u32 = 0x81f8_943c;
pub const BTRFS_IOC_SNAP_DESTROY_V2: u32 = 0x5000_943f;

/// `_IOW(0x94, 0xf0, struct { __s64 fd; __u64 reserved[3]; })`: receive the
/// stream in file `fd` (from its position) into the directory the ioctl is
/// called on. EclipseFS's own; btrfs receives in userspace.
pub const EFS_IOC_RECEIVE: u32 = 0x4020_94f0;
/// `_IOW(0x94, 0xf1, struct { __u64 devid; __u64 tier; __u64 reserved[2]; })`:
/// tier 0 none, 1 fast, 2 slow. With fast and slow devices both there,
/// metadata and new data go to the fast ones first.
pub const EFS_IOC_SET_TIER: u32 = 0x4020_94f1;
/// `_IOWR(0x94, 0xf2, struct { __u64 devid; __u64 tier; __u64 reserved[2]; })`:
/// the tier of device `devid`.
pub const EFS_IOC_GET_TIER: u32 = 0xc020_94f2;
/// `_IOWR(0x94, 0xf3, struct { __u64 flags; __u64 relocated; __u64 reserved[2]; })`:
/// move full data chunks off the fast devices (every one with flag 1) and
/// metadata onto them; `relocated` gets how many chunks moved.
pub const EFS_IOC_TIER_MIGRATE: u32 = 0xc020_94f3;
pub const EFS_TIER_MIGRATE_ALL: u64 = 1 << 0;

pub const FS_IMMUTABLE_FL: u32 = 0x10;
pub const FS_APPEND_FL: u32 = 0x20;

pub const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;
pub const BTRFS_SUBVOL_QGROUP_INHERIT: u64 = 1 << 2;
pub const BTRFS_DEVICE_SPEC_BY_ID: u64 = 1 << 3;
pub const BTRFS_SUBVOL_SPEC_BY_ID: u64 = 1 << 4;

pub const BTRFS_FS_INFO_FLAG_CSUM_INFO: u64 = 1 << 0;
pub const BTRFS_FS_INFO_FLAG_GENERATION: u64 = 1 << 1;
pub const BTRFS_FS_INFO_FLAG_METADATA_UUID: u64 = 1 << 2;

pub const BTRFS_BALANCE_DATA: u64 = 1 << 0;
pub const BTRFS_BALANCE_SYSTEM: u64 = 1 << 1;
pub const BTRFS_BALANCE_METADATA: u64 = 1 << 2;
pub const BTRFS_BALANCE_RESUME: u64 = 1 << 4;
pub const BTRFS_BALANCE_ARGS_CONVERT: u64 = 1 << 8;

pub const BTRFS_BLOCK_GROUP_RAID0: u64 = 1 << 3;
pub const BTRFS_BLOCK_GROUP_RAID1: u64 = 1 << 4;
pub const BTRFS_BLOCK_GROUP_DUP: u64 = 1 << 5;
pub const BTRFS_BLOCK_GROUP_RAID10: u64 = 1 << 6;
pub const BTRFS_BLOCK_GROUP_RAID5: u64 = 1 << 7;
pub const BTRFS_BLOCK_GROUP_RAID6: u64 = 1 << 8;
pub const BTRFS_BLOCK_GROUP_RAID1C3: u64 = 1 << 9;
pub const BTRFS_BLOCK_GROUP_RAID1C4: u64 = 1 << 10;
pub const BTRFS_AVAIL_ALLOC_BIT_SINGLE: u64 = 1 << 48;

pub const BTRFS_DEFRAG_RANGE_COMPRESS: u64 = 1 << 0;
pub const BTRFS_DEFRAG_RANGE_START_IO: u64 = 1 << 1;

pub const BTRFS_QGROUP_LIMIT_MAX_RFER: u64 = 1 << 0;
pub const BTRFS_SEND_FLAG_VERSION: u64 = 1 << 3;

pub const FILE_DEDUPE_RANGE_SAME: i32 = 0;
pub const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;

/// `struct fiemap`: the header, then `fm_extent_count` extents.
pub const FIEMAP_HEADER: usize = 32;
pub const FIEMAP_EXTENT: usize = 56;
/// The most extents one call fills (Linux bounds it by what the caller's
/// buffer holds; this bounds what is allocated for it).
pub const FIEMAP_MAX_EXTENTS: usize = 1 << 16;
pub const FIEMAP_FLAG_SYNC: u32 = 0x1;

pub const CAP_FOWNER: u32 = 3;
pub const CAP_LINUX_IMMUTABLE: u32 = 9;
pub const CAP_SYS_ADMIN: u32 = 21;

const BLOCK: u64 = 4096;

/// An open file an ioctl is called on or names, as the caller knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Open {
    /// Its inode, when it is a file of this very filesystem (`None`:
    /// another's, which a reflink or snapshot refuses with `EXDEV`).
    pub ino: Option<u64>,
    pub readable: bool,
    pub writable: bool,
    pub append: bool,
}

/// What an ioctl needs from the mount and the process that calls it.
pub trait Caller {
    /// Run `f` on the filesystem, which is locked while it runs.
    fn with<R>(&self, f: impl FnOnce(&mut EclipseFS) -> R) -> R;
    fn capable(&self, cap: u32) -> bool;
    fn euid(&self) -> u32;
    /// The file is on a read-only mount.
    fn read_only(&self) -> bool;
    /// How the file the ioctl is called on was opened.
    fn this(&self) -> Open;
    /// The open file behind descriptor `fd` (`EBADF` if none).
    fn file(&self, fd: i32) -> Result<Open, Errno>;
    /// Before the filesystem reads the bytes of `ino` in `off..off + len`
    /// on its own: what the page cache changed there is written back.
    fn flush(&self, _ino: u64, _off: u64, _len: u64) {}
    /// The bytes of `ino` in `off..off + len` changed underneath what is
    /// cached of them.
    fn reload(&self, _ino: u64, _off: u64, _len: u64) {}
    /// The entries of directory `ino` changed.
    fn dir_changed(&self, _ino: u64) {}
    /// What every directory's entries look like may have changed (a key
    /// came or went, and encrypted names with it).
    fn all_dirs_changed(&self) {}
    /// The block device at `path`, to add to the volume.
    fn device(&self, _path: &str) -> Result<Arc<dyn BlockDevice>, Errno> {
        Err(EOPNOTSUPP)
    }
}

/// The `errno` for an error of the filesystem.
pub fn errno_of(e: Error) -> Errno {
    match e {
        Error::Io => EIO,
        Error::BadSuperblock => EINVAL,
        Error::Corrupt(msg) => {
            warn!("eclipsefs: corrupt filesystem: {}", msg);
            EIO
        }
        Error::Unsupported(_) => EOPNOTSUPP,
        Error::NotFound => ENOENT,
        Error::Exists => EEXIST,
        Error::NotDir => ENOTDIR,
        Error::IsDir => EISDIR,
        Error::NotEmpty => ENOTEMPTY,
        Error::NoSpace => ENOSPC,
        Error::Invalid => EINVAL,
        Error::NoKey => ENOKEY,
    }
}

pub fn get16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

pub fn get64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

pub fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// The NUL-terminated string in `b[at..at + max]` (`ENAMETOOLONG` without a
/// NUL there, as btrfs answers).
pub fn cstr(b: &[u8], at: usize, max: usize) -> Result<&str, Errno> {
    let field = &b[at..at + max];
    let end = field.iter().position(|&c| c == 0).ok_or(ENAMETOOLONG)?;
    core::str::from_utf8(&field[..end]).map_err(|_| EINVAL)
}

/// A name for a new or deleted subvolume: one path component.
fn subvol_name(b: &[u8], at: usize, max: usize) -> Result<&str, Errno> {
    let name = cstr(b, at, max)?;
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(EINVAL);
    }
    Ok(name)
}

/// A byte count as Linux's `memparse` reads it: digits and an optional
/// `k`/`m`/`g`/`t`/`p`/`e` suffix in powers of 1024. `None` for anything
/// else, an overflow or zero.
pub fn memparse(s: &str) -> Option<u64> {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let n: u64 = s[..digits].parse().ok()?;
    let shift = match s[digits..].to_ascii_lowercase().as_str() {
        "" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        "t" => 40,
        "p" => 50,
        "e" => 60,
        _ => return None,
    };
    let v = n.checked_mul(1 << shift)?;
    (v > 0).then_some(v)
}

pub fn put_cstr(b: &mut [u8], at: usize, max: usize, s: &str) -> Result<(), Errno> {
    if s.len() >= max {
        return Err(ENAMETOOLONG);
    }
    b[at..at + s.len()].copy_from_slice(s.as_bytes());
    b[at + s.len()] = 0;
    Ok(())
}

/// Whether `cmd` is one of the ioctls this module answers.
pub fn is_fs_ioctl(cmd: u32) -> bool {
    matches!(
        cmd,
        FS_IOC_GETFLAGS
            | FS_IOC_SETFLAGS
            | FS_IOC32_GETFLAGS
            | FS_IOC32_SETFLAGS
            | FICLONE
            | FICLONERANGE
            | FIDEDUPERANGE
            | FS_IOC_FIEMAP
            | BTRFS_IOC_SNAP_CREATE
            | BTRFS_IOC_DEFRAG
            | BTRFS_IOC_RESIZE
            | BTRFS_IOC_DEFRAG_RANGE
            | BTRFS_IOC_SYNC
            | BTRFS_IOC_ADD_DEV
            | BTRFS_IOC_RM_DEV
            | BTRFS_IOC_SUBVOL_CREATE
            | BTRFS_IOC_SNAP_DESTROY
            | BTRFS_IOC_INO_LOOKUP
            | BTRFS_IOC_DEFAULT_SUBVOL
            | BTRFS_IOC_WAIT_SYNC
            | BTRFS_IOC_SNAP_CREATE_V2
            | BTRFS_IOC_START_SYNC
            | BTRFS_IOC_SUBVOL_CREATE_V2
            | BTRFS_IOC_SUBVOL_GETFLAGS
            | BTRFS_IOC_SUBVOL_SETFLAGS
            | BTRFS_IOC_SCRUB
            | BTRFS_IOC_SCRUB_CANCEL
            | BTRFS_IOC_SCRUB_PROGRESS
            | BTRFS_IOC_DEV_INFO
            | BTRFS_IOC_FS_INFO
            | BTRFS_IOC_BALANCE_V2
            | BTRFS_IOC_BALANCE_CTL
            | BTRFS_IOC_BALANCE_PROGRESS
            | BTRFS_IOC_SEND
            | BTRFS_IOC_QUOTA_CTL
            | BTRFS_IOC_QGROUP_LIMIT
            | BTRFS_IOC_QUOTA_RESCAN
            | BTRFS_IOC_QUOTA_RESCAN_STATUS
            | BTRFS_IOC_QUOTA_RESCAN_WAIT
            | BTRFS_IOC_DEV_REPLACE
            | BTRFS_IOC_GET_FEATURES
            | BTRFS_IOC_RM_DEV_V2
            | BTRFS_IOC_GET_SUBVOL_INFO
            | BTRFS_IOC_SNAP_DESTROY_V2
            | EFS_IOC_RECEIVE
            | EFS_IOC_SET_TIER
            | EFS_IOC_GET_TIER
            | EFS_IOC_TIER_MIGRATE
            | FS_IOC_SET_ENCRYPTION_POLICY
            | FS_IOC_GET_ENCRYPTION_POLICY
            | FS_IOC_GET_ENCRYPTION_POLICY_EX
            | FS_IOC_ADD_ENCRYPTION_KEY
            | FS_IOC_REMOVE_ENCRYPTION_KEY
            | FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS
            | FS_IOC_GET_ENCRYPTION_KEY_STATUS
            | FS_IOC_GET_ENCRYPTION_NONCE
    )
}

/// Whether ioctl `cmd` may change what the volume holds, when it
/// succeeds: what a server writes down to make again after a takeover.
/// Not the keys of `fscrypt`, which live in memory only (and are given
/// again after a reboot, or a new server).
pub fn changes_volume(cmd: u32) -> bool {
    matches!(
        cmd,
        FS_IOC_SETFLAGS
            | FS_IOC32_SETFLAGS
            | FICLONE
            | FICLONERANGE
            | FIDEDUPERANGE
            | BTRFS_IOC_SNAP_CREATE
            | BTRFS_IOC_SNAP_CREATE_V2
            | BTRFS_IOC_DEFRAG
            | BTRFS_IOC_DEFRAG_RANGE
            | BTRFS_IOC_RESIZE
            | BTRFS_IOC_SUBVOL_CREATE
            | BTRFS_IOC_SUBVOL_CREATE_V2
            | BTRFS_IOC_SNAP_DESTROY
            | BTRFS_IOC_SNAP_DESTROY_V2
            | BTRFS_IOC_SUBVOL_SETFLAGS
            | BTRFS_IOC_DEFAULT_SUBVOL
            | BTRFS_IOC_QUOTA_CTL
            | BTRFS_IOC_QUOTA_RESCAN
            | BTRFS_IOC_QGROUP_LIMIT
            | BTRFS_IOC_BALANCE_V2
            | BTRFS_IOC_ADD_DEV
            | BTRFS_IOC_RM_DEV
            | BTRFS_IOC_RM_DEV_V2
            | BTRFS_IOC_DEV_REPLACE
            | EFS_IOC_RECEIVE
            | EFS_IOC_SET_TIER
            | EFS_IOC_TIER_MIGRATE
            | FS_IOC_SET_ENCRYPTION_POLICY
    )
}

/// How many bytes of the caller's argument `cmd` reads and writes back,
/// as the number says: `_IOC_SIZE`. Some read more (see the kernel's copy).
pub fn arg_size(cmd: u32) -> usize {
    ((cmd >> 16) & 0x3fff) as usize
}

/// Whether `cmd` writes its argument back (`_IOC_READ`), save the two
/// that are numbered so and only read.
pub fn returns_data(cmd: u32) -> bool {
    cmd >> 30 & 2 != 0 && cmd != FICLONE && cmd != FS_IOC_SET_ENCRYPTION_POLICY
}

fn run<C: Caller, R>(
    c: &C,
    f: impl FnOnce(&mut EclipseFS) -> crate::Result<R>,
) -> Result<R, Errno> {
    c.with(f).map_err(errno_of)
}

/// `mnt_want_write_file`: `EROFS` on a read-only mount.
pub fn want_write<C: Caller>(c: &C) -> Result<(), Errno> {
    if c.read_only() {
        return Err(EROFS);
    }
    Ok(())
}

pub fn admin<C: Caller>(c: &C) -> Result<(), Errno> {
    if c.capable(CAP_SYS_ADMIN) {
        Ok(())
    } else {
        Err(EPERM)
    }
}

fn stat<C: Caller>(c: &C, ino: u64) -> Result<InodeStat, Errno> {
    run(c, |fs| fs.stat(ino))
}

/// `inode_owner_or_capable`.
fn owner<C: Caller>(c: &C, ino: u64) -> Result<(), Errno> {
    if stat(c, ino)?.uid == c.euid() || c.capable(CAP_FOWNER) {
        Ok(())
    } else {
        Err(EPERM)
    }
}

/// Write and search permission on directory `ino`, to add or remove a
/// name in it.
pub fn may_change_dir<C: Caller>(c: &C, ino: u64) -> Result<(), Errno> {
    let st = stat(c, ino)?;
    if st.kind != FileKind::Dir {
        return Err(ENOTDIR);
    }
    let allowed = c.capable(CAP_SYS_ADMIN)
        || (st.uid == c.euid() && st.mode & 0o300 == 0o300)
        || st.mode & 0o003 == 0o003;
    if allowed {
        Ok(())
    } else {
        Err(EACCES)
    }
}

/// The inode of the other file of a reflink, dedupe or snapshot, which
/// must be on this very filesystem.
fn same_fs(other: &Open) -> Result<u64, Errno> {
    other.ino.ok_or(EXDEV)
}

/// A regular file for a reflink or dedupe (`EISDIR`, else `EINVAL`).
fn regular<C: Caller>(c: &C, ino: u64) -> Result<InodeStat, Errno> {
    let st = stat(c, ino)?;
    match st.kind {
        FileKind::Regular => Ok(st),
        FileKind::Dir => Err(EISDIR),
        _ => Err(EINVAL),
    }
}

/// The subvolume `ino` is in, which must be its root directory.
fn subvol_root(ino: u64) -> Result<u64, Errno> {
    let (id, local) = split(ino);
    if local != FIRST_INO {
        return Err(EINVAL);
    }
    Ok(id)
}

fn subvol_info<C: Caller>(c: &C, id: u64) -> Result<crate::SubvolInfo, Errno> {
    run(c, |fs| fs.list_subvolumes())?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or(ENOENT)
}

fn devices<C: Caller>(c: &C) -> Vec<crate::DeviceInfo> {
    c.with(|fs| {
        let devs = fs.devices();
        if !devs.is_empty() {
            return devs;
        }
        // A filesystem on one device that never became a volume.
        let st = fs.fsinfo();
        vec![crate::DeviceInfo {
            devid: 1,
            size_bytes: st.total_bytes,
            allocated_bytes: st.bytes_used,
            present: true,
            tier: Tier::Any,
        }]
    })
}

fn uuid<C: Caller>(c: &C) -> [u8; 16] {
    c.with(|fs| fs.uuid())
}

/// Run ioctl `cmd` on file `ino` with its argument already in `buf` (and
/// `buf` holding what goes back to the caller after). `BTRFS_IOC_SEND`
/// and `EFS_IOC_RECEIVE` move a stream through another open file, which is
/// the kernel's to do: here they are `EOPNOTSUPP`.
pub fn ioctl<C: Caller>(c: &C, ino: u64, cmd: u32, buf: &mut [u8]) -> Result<usize, Errno> {
    match cmd {
        FS_IOC_FIEMAP => return fiemap(c, ino, buf),
        FS_IOC_GETFLAGS | FS_IOC32_GETFLAGS => {
            let flags = run(c, |fs| fs.get_flags(ino))?;
            put32(buf, 0, flags);
        }
        FS_IOC_SETFLAGS | FS_IOC32_SETFLAGS => {
            owner(c, ino)?;
            want_write(c)?;
            let new = get32(buf, 0);
            let old = run(c, |fs| fs.get_flags(ino))?;
            if (old ^ new) & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0
                && !c.capable(CAP_LINUX_IMMUTABLE)
            {
                return Err(EPERM);
            }
            run(c, |fs| fs.set_flags(ino, new))?;
        }
        FICLONE => {
            let src = c.file(get32(buf, 0) as i32)?;
            clone_from(c, ino, &src, 0, 0, 0)?;
        }
        FICLONERANGE => {
            let src = c.file(get64(buf, 0) as i64 as i32)?;
            clone_from(c, ino, &src, get64(buf, 8), get64(buf, 16), get64(buf, 24))?;
        }
        FIDEDUPERANGE => dedupe(c, ino, buf)?,
        FS_IOC_SET_ENCRYPTION_POLICY
        | FS_IOC_GET_ENCRYPTION_POLICY
        | FS_IOC_GET_ENCRYPTION_POLICY_EX
        | FS_IOC_ADD_ENCRYPTION_KEY
        | FS_IOC_REMOVE_ENCRYPTION_KEY
        | FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS
        | FS_IOC_GET_ENCRYPTION_KEY_STATUS
        | FS_IOC_GET_ENCRYPTION_NONCE => return fscrypt(c, ino, cmd, buf),
        BTRFS_IOC_DEFRAG => defrag(c, ino, None)?,
        BTRFS_IOC_DEFRAG_RANGE => defrag(c, ino, Some(buf))?,
        BTRFS_IOC_SYNC | BTRFS_IOC_WAIT_SYNC => run(c, |fs| fs.sync())?,
        BTRFS_IOC_START_SYNC => {
            let gen = run(c, |fs| fs.sync().map(|_| fs.generation()))?;
            put64(buf, 0, gen);
        }
        BTRFS_IOC_SUBVOL_CREATE | BTRFS_IOC_SUBVOL_CREATE_V2 => {
            let name = if cmd == BTRFS_IOC_SUBVOL_CREATE {
                subvol_name(buf, 8, 4088)?
            } else {
                if get64(buf, 16) & !BTRFS_SUBVOL_QGROUP_INHERIT != 0 {
                    return Err(EOPNOTSUPP);
                }
                subvol_name(buf, 56, 4040)?
            };
            want_write(c)?;
            may_change_dir(c, ino)?;
            run(c, |fs| fs.create_subvolume(ino, name))?;
            c.dir_changed(ino);
        }
        BTRFS_IOC_SNAP_CREATE | BTRFS_IOC_SNAP_CREATE_V2 => {
            let (name, read_only) = if cmd == BTRFS_IOC_SNAP_CREATE {
                (subvol_name(buf, 8, 4088)?, false)
            } else {
                let flags = get64(buf, 16);
                if flags & !(BTRFS_SUBVOL_RDONLY | BTRFS_SUBVOL_QGROUP_INHERIT) != 0 {
                    return Err(EOPNOTSUPP);
                }
                (
                    subvol_name(buf, 56, 4040)?,
                    flags & BTRFS_SUBVOL_RDONLY != 0,
                )
            };
            let src = c.file(get64(buf, 0) as i64 as i32)?;
            let src = same_fs(&src)?;
            want_write(c)?;
            may_change_dir(c, ino)?;
            run(c, |fs| fs.snapshot(src, ino, name, read_only))?;
            c.dir_changed(ino);
        }
        BTRFS_IOC_SNAP_DESTROY | BTRFS_IOC_SNAP_DESTROY_V2 => {
            let name = if cmd == BTRFS_IOC_SNAP_DESTROY {
                subvol_name(buf, 8, 4088)?
            } else {
                if get64(buf, 16) & BTRFS_SUBVOL_SPEC_BY_ID != 0 {
                    return Err(EOPNOTSUPP);
                }
                subvol_name(buf, 56, 4040)?
            };
            admin(c)?;
            want_write(c)?;
            run(c, |fs| fs.delete_subvolume(ino, name))?;
            c.dir_changed(ino);
        }
        BTRFS_IOC_SUBVOL_GETFLAGS => {
            let id = subvol_root(ino)?;
            let ro = id != FS_TREE && subvol_info(c, id)?.read_only;
            put64(buf, 0, if ro { BTRFS_SUBVOL_RDONLY } else { 0 });
        }
        BTRFS_IOC_SUBVOL_SETFLAGS => {
            let flags = get64(buf, 0);
            if flags & !BTRFS_SUBVOL_RDONLY != 0 {
                return Err(EOPNOTSUPP);
            }
            let id = subvol_root(ino)?;
            owner(c, ino)?;
            want_write(c)?;
            run(c, |fs| fs.set_subvolume_read_only(id, flags != 0))?;
        }
        BTRFS_IOC_DEFAULT_SUBVOL => {
            admin(c)?;
            want_write(c)?;
            let id = match get64(buf, 0) {
                0 => FS_TREE,
                id => id,
            };
            run(c, |fs| fs.set_default_subvolume(id))?;
        }
        BTRFS_IOC_GET_SUBVOL_INFO => get_subvol_info(c, ino, buf)?,
        BTRFS_IOC_INO_LOOKUP => ino_lookup(c, ino, buf)?,
        BTRFS_IOC_FS_INFO => fs_info(c, buf),
        BTRFS_IOC_DEV_INFO => dev_info(c, buf)?,
        BTRFS_IOC_GET_FEATURES => buf.fill(0),
        BTRFS_IOC_SCRUB => scrub(c, buf)?,
        BTRFS_IOC_SCRUB_CANCEL | BTRFS_IOC_SCRUB_PROGRESS => return Err(ENOTCONN),
        BTRFS_IOC_QUOTA_CTL => {
            admin(c)?;
            want_write(c)?;
            match get64(buf, 0) {
                // ENABLE and ENABLE_SIMPLE_QUOTA.
                1 | 4 => run(c, |fs| fs.quota_enable())?,
                2 => run(c, |fs| fs.quota_disable())?,
                _ => return Err(EINVAL),
            }
            put64(buf, 8, 0);
        }
        BTRFS_IOC_QUOTA_RESCAN => {
            admin(c)?;
            want_write(c)?;
            if get64(buf, 0) != 0 {
                return Err(EINVAL);
            }
            run(c, |fs| fs.quota_rescan())?;
        }
        // The rescan above runs to the end before it returns.
        BTRFS_IOC_QUOTA_RESCAN_STATUS => {
            admin(c)?;
            buf.fill(0);
        }
        BTRFS_IOC_QUOTA_RESCAN_WAIT => {}
        BTRFS_IOC_QGROUP_LIMIT => {
            admin(c)?;
            want_write(c)?;
            let id = match get64(buf, 0) {
                0 => split(ino).0,
                id if id >> 48 != 0 => return Err(EOPNOTSUPP),
                id => id,
            };
            let flags = get64(buf, 8);
            if flags & !BTRFS_QGROUP_LIMIT_MAX_RFER != 0 {
                return Err(EOPNOTSUPP);
            }
            if flags != 0 {
                let limit = match get64(buf, 16) {
                    u64::MAX => None,
                    max => Some(max),
                };
                run(c, |fs| fs.set_qgroup_limit(id, limit))?;
            }
        }
        BTRFS_IOC_RESIZE => {
            admin(c)?;
            want_write(c)?;
            resize(c, cstr(buf, 8, 4088)?)?;
        }
        BTRFS_IOC_ADD_DEV => {
            admin(c)?;
            want_write(c)?;
            let dev = c.device(cstr(buf, 8, 4088)?)?;
            if crate::probe(&*dev).is_ok_and(|p| p.uuid == uuid(c)) {
                return Err(EEXIST);
            }
            run(c, |fs| fs.add_device(dev))?;
        }
        BTRFS_IOC_RM_DEV | BTRFS_IOC_RM_DEV_V2 => {
            admin(c)?;
            want_write(c)?;
            let devid =
                if cmd == BTRFS_IOC_RM_DEV_V2 && get64(buf, 16) & BTRFS_DEVICE_SPEC_BY_ID != 0 {
                    get64(buf, 56) as u32
                } else if cmd == BTRFS_IOC_RM_DEV_V2 {
                    devid_of(c, cstr(buf, 56, 4040)?)?
                } else {
                    devid_of(c, cstr(buf, 8, 4088)?)?
                };
            run(c, |fs| fs.remove_device(devid))?;
        }
        BTRFS_IOC_DEV_REPLACE => dev_replace(c, buf)?,
        BTRFS_IOC_BALANCE_V2 => balance(c, buf)?,
        BTRFS_IOC_BALANCE_CTL | BTRFS_IOC_BALANCE_PROGRESS => return Err(ENOTCONN),
        BTRFS_IOC_SEND | EFS_IOC_RECEIVE => return Err(EOPNOTSUPP),
        EFS_IOC_SET_TIER => {
            admin(c)?;
            want_write(c)?;
            let (devid, tier) = (get64(buf, 0), get64(buf, 8));
            if devid > u32::MAX as u64 || tier > u8::MAX as u64 {
                return Err(EINVAL);
            }
            let tier = Tier::from_id(tier as u8).map_err(|_| EINVAL)?;
            let devid = devid as u32;
            run(c, |fs| fs.set_device_tier(devid, tier))?;
        }
        EFS_IOC_GET_TIER => {
            let devid = get64(buf, 0);
            let dev = devices(c)
                .into_iter()
                .find(|d| d.devid as u64 == devid)
                .ok_or(ENODEV)?;
            put64(buf, 8, dev.tier.id() as u64);
        }
        EFS_IOC_TIER_MIGRATE => {
            admin(c)?;
            want_write(c)?;
            let all = get64(buf, 0) & EFS_TIER_MIGRATE_ALL != 0;
            let r = run(c, |fs| fs.tier_migrate(all))?;
            put64(buf, 8, r.relocated);
        }
        _ => return Err(ENOTTY),
    }
    Ok(0)
}

/// The fscrypt ioctls, with `buf` laid out as `fscrypt.h` lays out their
/// structs.
fn fscrypt<C: Caller>(c: &C, ino: u64, cmd: u32, buf: &mut [u8]) -> Result<usize, Errno> {
    match cmd {
        FS_IOC_SET_ENCRYPTION_POLICY => {
            if buf.first() != Some(&FSCRYPT_POLICY_V2) || buf.len() < FSCRYPT_POLICY_V2_SIZE {
                // v1 policies (master key descriptors) are not supported.
                return Err(EINVAL);
            }
            if buf[4..8] != [0; 4] {
                return Err(EINVAL);
            }
            let policy = EncryptionPolicy {
                contents_mode: buf[1],
                filenames_mode: buf[2],
                flags: buf[3],
                key_id: buf[8..24].try_into().unwrap(),
            };
            // `inode_owner_or_capable`, else EACCES as fscrypt says.
            owner(c, ino).map_err(|_| EACCES)?;
            want_write(c)?;
            run(c, |fs| fs.set_encryption_policy(ino, &policy))?;
        }
        FS_IOC_GET_ENCRYPTION_POLICY => {
            // The v1 call cannot return a v2 policy.
            return match run(c, |fs| fs.encryption_policy(ino))? {
                Some(_) => Err(EINVAL),
                None => Err(ENODATA),
            };
        }
        FS_IOC_GET_ENCRYPTION_POLICY_EX => {
            let p = run(c, |fs| fs.encryption_policy(ino))?.ok_or(ENODATA)?;
            if buf.len() < 8 + FSCRYPT_POLICY_V2_SIZE {
                return Err(EOVERFLOW);
            }
            put64(buf, 0, FSCRYPT_POLICY_V2_SIZE as u64);
            let out = &mut buf[8..8 + FSCRYPT_POLICY_V2_SIZE];
            out.fill(0);
            out[0] = FSCRYPT_POLICY_V2;
            out[1] = p.contents_mode;
            out[2] = p.filenames_mode;
            out[3] = p.flags;
            out[8..24].copy_from_slice(&p.key_id);
        }
        FS_IOC_ADD_ENCRYPTION_KEY => {
            // A raw key named by its identifier; keyring keys (`key_id`)
            // and v1 descriptors are not supported.
            if buf.len() < FSCRYPT_ADD_KEY_ARG
                || get32(buf, 0) != FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER
                || get32(buf, 4) != 0
                || get32(buf, 44) != 0
                || buf[48..FSCRYPT_ADD_KEY_ARG].iter().any(|&b| b != 0)
            {
                return Err(EINVAL);
            }
            let raw = get32(buf, 40) as usize;
            if raw > FSCRYPT_MAX_KEY_SIZE || buf.len() < FSCRYPT_ADD_KEY_ARG + raw {
                return Err(EINVAL);
            }
            let id = run(c, |fs| {
                fs.add_encryption_key(&buf[FSCRYPT_ADD_KEY_ARG..FSCRYPT_ADD_KEY_ARG + raw])
            })?;
            buf[8..FSCRYPT_KEY_SPEC].fill(0);
            buf[8..24].copy_from_slice(&id);
            c.all_dirs_changed();
        }
        FS_IOC_REMOVE_ENCRYPTION_KEY | FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS => {
            if cmd == FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS {
                admin(c).map_err(|_| EACCES)?;
            }
            let id = key_identifier(buf)?;
            if buf[FSCRYPT_KEY_SPEC..].iter().any(|&b| b != 0) {
                return Err(EINVAL);
            }
            match run(c, |fs| fs.remove_encryption_key(&id)) {
                Err(ENOENT) => return Err(ENOKEY),
                r => r?,
            }
            // `removal_status_flags`: every file let go of the key.
            put32(buf, FSCRYPT_KEY_SPEC, 0);
            c.all_dirs_changed();
        }
        FS_IOC_GET_ENCRYPTION_KEY_STATUS => {
            let id = key_identifier(buf)?;
            let present = run(c, |fs| fs.encryption_key_present(&id))?;
            buf[64..].fill(0);
            if present {
                put32(buf, 64, FSCRYPT_KEY_STATUS_PRESENT);
                put32(buf, 68, FSCRYPT_KEY_STATUS_FLAG_ADDED_BY_SELF);
                put32(buf, 72, 1);
            } else {
                put32(buf, 64, FSCRYPT_KEY_STATUS_ABSENT);
            }
        }
        FS_IOC_GET_ENCRYPTION_NONCE => {
            let nonce = match run(c, |fs| fs.encryption_nonce(ino)) {
                Err(ENOENT) => return Err(ENODATA),
                r => r?,
            };
            buf[..16].copy_from_slice(&nonce);
        }
        _ => unreachable!(),
    }
    Ok(0)
}

/// The identifier in the `struct fscrypt_key_specifier` at the start of
/// `buf` (a v1 descriptor is `EINVAL`).
fn key_identifier(buf: &[u8]) -> Result<[u8; 16], Errno> {
    if get32(buf, 0) != FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER || get32(buf, 4) != 0 {
        return Err(EINVAL);
    }
    Ok(buf[8..24].try_into().unwrap())
}

/// `FS_IOC_FIEMAP` on `buf` (a `struct fiemap` with room for
/// `fm_extent_count` extents): fills `fm_mapped_extents` and the extents,
/// or with no room only counts them.
pub fn fiemap<C: Caller>(c: &C, ino: u64, buf: &mut [u8]) -> Result<usize, Errno> {
    let start = get64(buf, 0);
    let len = get64(buf, 8);
    let flags = get32(buf, 16);
    let room = (buf.len() - FIEMAP_HEADER) / FIEMAP_EXTENT;
    put32(buf, 20, 0);
    if flags & !FIEMAP_FLAG_SYNC != 0 {
        put32(buf, 16, flags & !FIEMAP_FLAG_SYNC);
        return Err(EBADR);
    }
    if len == 0 {
        return Err(EINVAL);
    }
    // Data still in the page cache is mapped once written back.
    if let Ok(st) = stat(c, ino) {
        c.flush(ino, 0, st.size);
    }
    if flags & FIEMAP_FLAG_SYNC != 0 {
        run(c, |fs| fs.sync())?;
    }
    let map = match run(c, |fs| fs.fiemap(ino, start, len)) {
        // A directory has no data to map (btrfs answers the same).
        Err(EISDIR) => Vec::new(),
        r => r?,
    };
    if room == 0 {
        put32(buf, 20, map.len() as u32);
        return Ok(0);
    }
    for (i, m) in map.iter().take(room).enumerate() {
        let at = FIEMAP_HEADER + i * FIEMAP_EXTENT;
        let e = &mut buf[at..at + FIEMAP_EXTENT];
        e.fill(0);
        put64(e, 0, m.logical);
        put64(e, 8, m.physical);
        put64(e, 16, m.length);
        put32(e, 40, m.flags);
    }
    put32(buf, 20, map.len().min(room) as u32);
    Ok(0)
}

/// `FICLONE`/`FICLONERANGE` on file `ino` (the destination) from `src`.
fn clone_from<C: Caller>(
    c: &C,
    ino: u64,
    src: &Open,
    src_off: u64,
    len: u64,
    dst_off: u64,
) -> Result<(), Errno> {
    let dst = c.this();
    if !dst.writable || dst.append || !src.readable {
        return Err(EBADF);
    }
    let src_ino = same_fs(src)?;
    let src_size = regular(c, src_ino)?.size;
    regular(c, ino)?;
    want_write(c)?;
    let len_or_rest = if len == 0 {
        src_size.saturating_sub(src_off)
    } else {
        len
    };
    if src_ino == ino && src_off < dst_off + len_or_rest && dst_off < src_off + len_or_rest {
        return Err(EINVAL);
    }
    c.flush(src_ino, src_off, len_or_rest);
    let n = run(c, |fs| fs.clone_range(src_ino, src_off, ino, dst_off, len))?;
    c.reload(ino, dst_off, n);
    Ok(())
}

/// `copy_file_range(2)` as a reflink, from `src` into file `ino` (opened
/// as [`Caller::this`] says), when both are on this filesystem and the
/// offsets are block-aligned (as btrfs does it). Copies at most `len`
/// bytes and returns how many: whole blocks, or up to the end of `src`.
/// `None` when a reflink does not apply, and then the caller copies the
/// bytes.
pub fn copy_reflink<C: Caller>(
    c: &C,
    src: &Open,
    src_off: u64,
    ino: u64,
    dst_off: u64,
    len: u64,
) -> Option<u64> {
    let src_ino = same_fs(src).ok()?;
    if len == 0 || c.this().append || want_write(c).is_err() {
        return None;
    }
    let src_size = regular(c, src_ino).ok()?.size;
    regular(c, ino).ok()?;
    if src_off >= src_size || !src_off.is_multiple_of(BLOCK) || !dst_off.is_multiple_of(BLOCK) {
        return None;
    }
    let mut n = len.min(src_size - src_off);
    if src_off + n != src_size {
        n -= n % BLOCK;
    }
    if n == 0 || (src_ino == ino && src_off < dst_off + n && dst_off < src_off + n) {
        return None;
    }
    c.flush(src_ino, src_off, n);
    let done = run(c, |fs| fs.clone_range(src_ino, src_off, ino, dst_off, n)).ok()?;
    c.reload(ino, dst_off, done);
    Some(done)
}

/// `BTRFS_IOC_DEFRAG` (the whole file) and `BTRFS_IOC_DEFRAG_RANGE`
/// (`struct btrfs_ioctl_defrag_range_args`), as `btrfs filesystem
/// defragment [-c]` calls them. A file must be open for writing, or its
/// owner's, or the caller `CAP_SYS_ADMIN`; a directory (btrfs defragments
/// its metadata, which here is rewritten on every change) needs
/// `CAP_SYS_ADMIN` and has nothing to do.
fn defrag<C: Caller>(c: &C, ino: u64, range: Option<&[u8]>) -> Result<(), Errno> {
    want_write(c)?;
    let st = stat(c, ino)?;
    match st.kind {
        FileKind::Dir => return admin(c),
        FileKind::Regular => {}
        _ => return Err(EINVAL),
    }
    if !c.this().writable && !c.capable(CAP_SYS_ADMIN) {
        owner(c, ino)?;
    }
    let (mut start, mut len, mut threshold, mut compress, mut sync) = (0, u64::MAX, 0, None, false);
    if let Some(b) = range {
        let flags = get64(b, 16);
        if flags & !(BTRFS_DEFRAG_RANGE_COMPRESS | BTRFS_DEFRAG_RANGE_START_IO) != 0 {
            return Err(EOPNOTSUPP);
        }
        start = get64(b, 0);
        // A length of 0 (or one past the largest offset) runs to the end.
        len = match get64(b, 8) {
            0 => u64::MAX,
            n => n,
        };
        threshold = get32(b, 24) as u64;
        if flags & BTRFS_DEFRAG_RANGE_COMPRESS != 0 {
            // btrfs's numbers; zlib by default, and LZ4 stands in for LZO.
            compress = Some(match get32(b, 28) {
                0 | 1 => Compression::Zlib,
                2 => Compression::Lz4,
                3 => Compression::Zstd,
                _ => return Err(EINVAL),
            });
        }
        sync = flags & BTRFS_DEFRAG_RANGE_START_IO != 0;
    }
    // What the page cache holds is defragmented with the rest. The bytes do
    // not change, so nothing cached needs reloading after.
    c.flush(ino, 0, st.size);
    run(c, |fs| fs.defrag(ino, start, len, threshold, compress))?;
    if sync {
        run(c, |fs| fs.sync())?;
    }
    Ok(())
}

/// `BTRFS_IOC_RESIZE` with what `btrfs filesystem resize` passes:
/// `[devid:]size`, the size being `max`, bytes with an optional
/// `k`/`m`/`g`/`t`/`p`/`e` (powers of 1024), or such an amount after `+`
/// or `-` to add to or take from the device's current size.
fn resize<C: Caller>(c: &C, arg: &str) -> Result<(), Errno> {
    let (devid, spec) = match arg.split_once(':') {
        Some((d, s)) => (d.parse::<u32>().map_err(|_| EINVAL)?, s),
        None => (1, arg),
    };
    let size = if spec == "max" {
        None
    } else {
        let (sign, amount) = match spec.as_bytes().first() {
            Some(b'+') => (1, &spec[1..]),
            Some(b'-') => (-1, &spec[1..]),
            _ => (0, spec),
        };
        let n = memparse(amount).ok_or(EINVAL)?;
        let cur = devices(c)
            .iter()
            .find(|d| d.devid == devid)
            .ok_or(ENODEV)?
            .size_bytes;
        Some(match sign {
            1 => cur.checked_add(n).ok_or(EINVAL)?,
            -1 => cur.checked_sub(n).ok_or(EINVAL)?,
            _ => n,
        })
    };
    match run(c, |fs| fs.resize(devid, size)) {
        Err(ENOENT) => Err(ENODEV),
        r => r.map(|_| ()),
    }
}

/// `FIDEDUPERANGE`, called on the source `ino`.
fn dedupe<C: Caller>(c: &C, ino: u64, buf: &mut [u8]) -> Result<(), Errno> {
    let src_off = get64(buf, 0);
    let len = get64(buf, 8);
    let count = get16(buf, 16) as usize;
    if get16(buf, 18) != 0 || get32(buf, 20) != 0 || buf.len() < 24 + 32 * count {
        return Err(EINVAL);
    }
    if !c.this().readable {
        return Err(EBADF);
    }
    regular(c, ino)?;
    want_write(c)?;
    c.flush(ino, src_off, len);
    for i in 0..count {
        let at = 24 + 32 * i;
        if get32(buf, at + 28) != 0 {
            return Err(EINVAL);
        }
        let dst_off = get64(buf, at + 8);
        let (status, done) =
            match dedupe_one(c, ino, get64(buf, at) as i64 as i32, src_off, dst_off, len) {
                Ok(0) if len > 0 => (FILE_DEDUPE_RANGE_DIFFERS, 0),
                Ok(n) => (FILE_DEDUPE_RANGE_SAME, n),
                Err(e) => (-e, 0),
            };
        put64(buf, at + 16, done);
        put32(buf, at + 24, status as u32);
    }
    Ok(())
}

fn dedupe_one<C: Caller>(
    c: &C,
    ino: u64,
    fd: i32,
    src_off: u64,
    dst_off: u64,
    len: u64,
) -> Result<u64, Errno> {
    let dst = c.file(fd)?;
    let dst_ino = same_fs(&dst)?;
    let st = regular(c, dst_ino)?;
    // `allow_file_dedupe`: open for writing, or the caller's own file.
    if !dst.writable && st.uid != c.euid() && !c.capable(CAP_SYS_ADMIN) {
        return Err(EPERM);
    }
    c.flush(dst_ino, dst_off, len);
    let n = run(c, |fs| fs.dedupe_range(ino, src_off, dst_ino, dst_off, len))?;
    c.reload(dst_ino, dst_off, n);
    Ok(n)
}

fn get_subvol_info<C: Caller>(c: &C, ino: u64, buf: &mut [u8]) -> Result<(), Errno> {
    let id = split(ino).0;
    let info = subvol_info(c, id)?;
    buf.fill(0);
    put64(buf, 0, info.id);
    put_cstr(buf, 8, 256, &info.name)?;
    put64(buf, 264, info.parent);
    put64(buf, 272, if info.dir == 0 { 0 } else { split(info.dir).1 });
    put64(buf, 280, c.with(|fs| fs.generation()));
    put64(
        buf,
        288,
        if info.read_only {
            BTRFS_SUBVOL_RDONLY
        } else {
            0
        },
    );
    buf[296..312].copy_from_slice(&info.uuid);
    buf[312..328].copy_from_slice(&info.parent_uuid);
    buf[328..344].copy_from_slice(&info.received_uuid);
    put64(buf, 352, info.snapshot_gen);
    // otime: when it was made.
    put64(buf, 392, info.created.0);
    put32(buf, 400, info.created.1);
    Ok(())
}

/// `BTRFS_IOC_INO_LOOKUP`: the path of directory `objectid` inside
/// subvolume `treeid` (0: this file's), with a trailing `/`.
fn ino_lookup<C: Caller>(c: &C, ino: u64, buf: &mut [u8]) -> Result<(), Errno> {
    let tree = match get64(buf, 0) {
        0 => split(ino).0,
        id => id,
    };
    let objectid = get64(buf, 8);
    if objectid != FIRST_INO && !c.capable(CAP_SYS_ADMIN) {
        return Err(EPERM);
    }
    let parts = c.with(|fs| -> Result<Vec<String>, Errno> {
        let mut parts = Vec::new();
        let mut cur = handle(tree, objectid);
        while split(cur).1 != FIRST_INO {
            let parent = fs.parent(cur).map_err(errno_of)?;
            let name = fs
                .readdir(parent)
                .map_err(errno_of)?
                .into_iter()
                .find(|e| e.ino == cur)
                .map(|e| e.name)
                .ok_or(ENOENT)?;
            parts.push(name);
            if split(parent).0 != tree {
                return Err(ENOENT);
            }
            cur = parent;
        }
        Ok(parts)
    })?;
    let mut path = String::new();
    for part in parts.iter().rev() {
        path.push_str(part);
        path.push('/');
    }
    put64(buf, 0, tree);
    put_cstr(buf, 16, 4080, &path)
}

fn fs_info<C: Caller>(c: &C, buf: &mut [u8]) {
    let want = get64(buf, 48);
    let devs = devices(c);
    let (uuid, gen) = c.with(|fs| (fs.uuid(), fs.generation()));
    buf.fill(0);
    put64(
        buf,
        0,
        devs.iter().map(|d| d.devid as u64).max().unwrap_or(1),
    );
    put64(buf, 8, devs.len() as u64);
    buf[16..32].copy_from_slice(&uuid);
    put32(buf, 32, BLOCK as u32);
    put32(buf, 36, BLOCK as u32);
    put32(buf, 40, BLOCK as u32);
    let got = want
        & (BTRFS_FS_INFO_FLAG_CSUM_INFO
            | BTRFS_FS_INFO_FLAG_GENERATION
            | BTRFS_FS_INFO_FLAG_METADATA_UUID);
    if got & BTRFS_FS_INFO_FLAG_CSUM_INFO != 0 {
        // BTRFS_CSUM_TYPE_CRC32 (CRC32C), four bytes.
        buf[44..46].copy_from_slice(&0u16.to_le_bytes());
        buf[46..48].copy_from_slice(&4u16.to_le_bytes());
    }
    put64(buf, 48, got);
    if got & BTRFS_FS_INFO_FLAG_GENERATION != 0 {
        put64(buf, 56, gen);
    }
    if got & BTRFS_FS_INFO_FLAG_METADATA_UUID != 0 {
        buf[64..80].copy_from_slice(&uuid);
    }
}

fn dev_info<C: Caller>(c: &C, buf: &mut [u8]) -> Result<(), Errno> {
    let devid = get64(buf, 0);
    let dev = devices(c)
        .into_iter()
        .find(|d| d.devid as u64 == devid && d.present)
        .ok_or(ENODEV)?;
    let uuid = uuid(c);
    buf.fill(0);
    put64(buf, 0, devid);
    put64(buf, 24, dev.allocated_bytes);
    put64(buf, 32, dev.size_bytes);
    buf[40..56].copy_from_slice(&uuid);
    Ok(())
}

/// `BTRFS_IOC_SCRUB`: btrfs-progs asks once per device; the whole
/// filesystem is scrubbed on the first device's call and the others
/// report nothing more.
fn scrub<C: Caller>(c: &C, buf: &mut [u8]) -> Result<(), Errno> {
    admin(c)?;
    let devid = get64(buf, 0);
    let devs = devices(c);
    if !devs.iter().any(|d| d.devid as u64 == devid && d.present) {
        return Err(ENODEV);
    }
    buf[32..152].fill(0);
    let first = devs.iter().filter(|d| d.present).map(|d| d.devid).min();
    if first != Some(devid as u32) {
        return Ok(());
    }
    let r = run(c, |fs| fs.scrub())?;
    let bad = (r.bad_data_blocks.len() + r.bad_tree_nodes.len()) as u64;
    put64(buf, 32, r.data_blocks);
    put64(buf, 40, r.tree_nodes);
    put64(buf, 48, r.data_blocks * BLOCK);
    put64(buf, 56, r.tree_nodes * BLOCK);
    put64(buf, 72, r.bad_data_blocks.len() as u64);
    put64(buf, 80, r.bad_tree_nodes.len() as u64);
    put64(buf, 120, bad);
    put64(buf, 128, r.repaired);
    Ok(())
}

/// The device `name` names in `btrfs device remove`/`replace`: an id,
/// `missing`, or the path of a member.
fn devid_of<C: Caller>(c: &C, name: &str) -> Result<u32, Errno> {
    if let Ok(id) = name.parse::<u32>() {
        return Ok(id);
    }
    if name == "missing" {
        return devices(c)
            .into_iter()
            .find(|d| !d.present)
            .map(|d| d.devid)
            .ok_or(EINVAL);
    }
    let dev = c.device(name)?;
    match crate::probe(&*dev) {
        Ok(p) if p.uuid == uuid(c) => Ok(p.devid),
        _ => Err(EINVAL),
    }
}

fn dev_replace<C: Caller>(c: &C, buf: &mut [u8]) -> Result<(), Errno> {
    admin(c)?;
    match get64(buf, 0) {
        // START: runs to the end before it returns.
        0 => {
            want_write(c)?;
            let src = match get64(buf, 16) {
                0 => devid_of(c, cstr(buf, 32, 1025)?)?,
                id => id as u32,
            };
            let new = c.device(cstr(buf, 1057, 1025)?)?;
            run(c, |fs| fs.replace_device(src, new))?;
            put64(buf, 8, 0);
        }
        // STATUS: none running (BTRFS_IOCTL_DEV_REPLACE_STATE_NEVER_STARTED).
        1 => {
            buf[16..64].fill(0);
            put64(buf, 8, 0);
        }
        // CANCEL: BTRFS_IOCTL_DEV_REPLACE_RESULT_NOT_STARTED.
        2 => put64(buf, 8, 1),
        _ => return Err(EINVAL),
    }
    Ok(())
}

fn balance<C: Caller>(c: &C, buf: &mut [u8]) -> Result<(), Errno> {
    admin(c)?;
    want_write(c)?;
    let flags = get64(buf, 0);
    if flags & BTRFS_BALANCE_RESUME != 0 {
        return Err(ENOTCONN);
    }
    let all = flags & (BTRFS_BALANCE_DATA | BTRFS_BALANCE_METADATA | BTRFS_BALANCE_SYSTEM) == 0;
    let convert = |kind: u64, at: usize| -> Result<Option<Profile>, Errno> {
        if (all || flags & kind != 0) && get64(buf, at + 64) & BTRFS_BALANCE_ARGS_CONVERT != 0 {
            profile(get64(buf, at + 56)).map(Some)
        } else {
            Ok(None)
        }
    };
    let data = convert(BTRFS_BALANCE_DATA, 16)?;
    let meta = convert(BTRFS_BALANCE_METADATA, 152)?;
    let report = run(c, |fs| fs.balance(data, meta))?;
    let done = report.relocated + report.freed;
    put64(buf, 8, 0);
    put64(buf, 424, done);
    put64(buf, 432, done);
    put64(buf, 440, done);
    Ok(())
}

/// A balance `convert=` target (`BTRFS_BLOCK_GROUP_*` bits) as a profile.
pub fn profile(bits: u64) -> Result<Profile, Errno> {
    Ok(match bits & !BTRFS_AVAIL_ALLOC_BIT_SINGLE {
        0 => Profile::Single,
        BTRFS_BLOCK_GROUP_DUP => Profile::Dup,
        BTRFS_BLOCK_GROUP_RAID0 => Profile::Raid0,
        BTRFS_BLOCK_GROUP_RAID1 => Profile::Raid1,
        BTRFS_BLOCK_GROUP_RAID10 => Profile::Raid10,
        BTRFS_BLOCK_GROUP_RAID1C3 => Profile::Raid1c3,
        BTRFS_BLOCK_GROUP_RAID1C4 => Profile::Raid1c4,
        BTRFS_BLOCK_GROUP_RAID5 => Profile::Raid5,
        BTRFS_BLOCK_GROUP_RAID6 => Profile::Raid6,
        // Combinations.
        _ => return Err(EINVAL),
    })
}

/// One extended-attribute request, after the syscall layer has judged the
/// name and flags (see `linux-syscall`'s `xattr.rs`).
#[derive(Debug, Clone, Copy)]
pub enum XattrRequest<'a> {
    Get(&'a str),
    Set {
        name: &'a str,
        value: &'a [u8],
        flags: u32,
    },
    List,
    Remove(&'a str),
}

impl XattrRequest<'_> {
    /// Whether it changes the file.
    pub fn changes(&self) -> bool {
        matches!(self, XattrRequest::Set { .. } | XattrRequest::Remove(_))
    }
}

/// The answer to an [`XattrRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XattrReply {
    Value(Vec<u8>),
    /// Every name, each followed by a NUL, as `listxattr(2)` returns them.
    Names(Vec<u8>),
    Done,
}

/// btrfs keeps a file's compression property behind this name
/// (`btrfs property set <file> compression zstd`).
pub const COMPRESSION_XATTR: &str = "btrfs.compression";

/// The `*xattr` syscalls on file `ino`: `read_only` when its mount is,
/// `euid` and `admin` (`CAP_SYS_ADMIN`) the caller's credentials.
pub fn xattr(
    efs: &mut EclipseFS,
    ino: u64,
    req: XattrRequest<'_>,
    read_only: bool,
    euid: u32,
    admin: bool,
) -> Result<XattrReply, Errno> {
    let name = match req {
        XattrRequest::Get(n) | XattrRequest::Set { name: n, .. } | XattrRequest::Remove(n) => n,
        XattrRequest::List => "",
    };
    if name.starts_with("trusted.") && !admin {
        return Err(EPERM);
    }
    if req.changes() {
        if read_only {
            return Err(EROFS);
        }
        let st = efs.stat(ino).map_err(errno_of)?;
        if st.uid != euid && !admin {
            return Err(EPERM);
        }
        if name.starts_with("user.") && !matches!(st.kind, FileKind::Regular | FileKind::Dir) {
            return Err(EPERM);
        }
    }
    // A missing attribute is ENODATA, not ENOENT.
    let absent = |e: Error| match e {
        Error::NotFound => ENODATA,
        e => errno_of(e),
    };
    Ok(match req {
        XattrRequest::Get(COMPRESSION_XATTR) => match efs.compression(ino).map_err(errno_of)? {
            Some(algo) => XattrReply::Value(algo.name().as_bytes().to_vec()),
            None => return Err(ENODATA),
        },
        XattrRequest::Set {
            name: COMPRESSION_XATTR,
            value,
            ..
        } => {
            let algo = match value {
                b"" | b"no" | b"none" => None,
                v => Some(
                    core::str::from_utf8(v)
                        .ok()
                        .and_then(Compression::from_name)
                        .ok_or(EINVAL)?,
                ),
            };
            efs.set_compression(ino, algo).map_err(errno_of)?;
            XattrReply::Done
        }
        XattrRequest::Remove(COMPRESSION_XATTR) => {
            efs.set_compression(ino, None).map_err(errno_of)?;
            XattrReply::Done
        }
        XattrRequest::Get(name) => XattrReply::Value(efs.get_xattr(ino, name).map_err(absent)?),
        XattrRequest::Set { name, value, flags } => {
            efs.set_xattr(ino, name, value, flags).map_err(absent)?;
            XattrReply::Done
        }
        XattrRequest::Remove(name) => {
            efs.remove_xattr(ino, name).map_err(absent)?;
            XattrReply::Done
        }
        XattrRequest::List => {
            let mut out = Vec::new();
            let names = match efs.list_xattrs(ino) {
                Ok(names) => names,
                // A v0.3 image keeps none.
                Err(Error::Unsupported(_)) => Vec::new(),
                Err(e) => return Err(errno_of(e)),
            };
            for n in names {
                if n.starts_with("trusted.") && !admin {
                    continue;
                }
                out.extend_from_slice(n.as_bytes());
                out.push(0);
            }
            XattrReply::Names(out)
        }
    })
}

/// `fallocate(2)`'s `FALLOC_FL_PUNCH_HOLE` and `FALLOC_FL_ZERO_RANGE`:
/// after either, the range reads as zeros.
pub const FALLOC_ZEROES: u32 = 0x02 | 0x10;

/// `lseek(2)`'s `SEEK_DATA`; `SEEK_HOLE` is 4.
pub const SEEK_DATA: usize = 3;
pub const SEEK_HOLE: usize = 4;

/// `lseek(SEEK_DATA)` or `lseek(SEEK_HOLE)` on file `ino`: the new
/// position, from the file's real extents. The caller has written back
/// what the page cache changed.
pub fn seek(efs: &mut EclipseFS, ino: u64, whence: usize, offset: i64) -> Result<u64, Errno> {
    let st = efs.stat(ino).map_err(errno_of)?;
    match st.kind {
        FileKind::Regular => {}
        FileKind::Dir => return Err(EISDIR),
        _ => return Err(EINVAL),
    }
    // Like `generic_file_llseek_size`: an offset at or past the end (or
    // negative, read as unsigned) is ENXIO.
    if offset < 0 || offset as u64 >= st.size {
        return Err(ENXIO);
    }
    let off = offset as u64;
    let found = if whence == SEEK_DATA {
        efs.seek_data(ino, off)
    } else {
        efs.seek_hole(ino, off)
    };
    found.map_err(errno_of)?.ok_or(ENXIO)
}
