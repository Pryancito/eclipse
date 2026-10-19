//! What an EclipseFS mount offers past `read`/`write`: the filesystem ioctls
//! (with the numbers and argument layouts of Linux and btrfs, so `chattr`,
//! `cp --reflink`, `duperemove` and btrfs-progs speak to it unchanged),
//! extended attributes, `fallocate`, reflinked `copy_file_range` and
//! `lseek(SEEK_DATA/SEEK_HOLE)` from the real extents.
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
//! | `BTRFS_IOC_SEND` | send stream of a read-only snapshot into a file |
//! | `EFS_IOC_RECEIVE` | receive such a stream into a directory (EclipseFS's own) |
//! | `EFS_IOC_SET_TIER`, `GET_TIER` | a device's tier: fast (SSD) or slow (HDD) (EclipseFS's own) |
//! | `EFS_IOC_TIER_MIGRATE` | move full data chunks to the slow devices (EclipseFS's own) |
//! | `FS_IOC_SET_ENCRYPTION_POLICY`, `GET_ENCRYPTION_POLICY(_EX)` | fscrypt v2 policy of an empty directory |
//! | `FS_IOC_ADD_ENCRYPTION_KEY`, `REMOVE_ENCRYPTION_KEY(_ALL_USERS)` | master keys (`fscrypt unlock`/`lock`) |
//! | `FS_IOC_GET_ENCRYPTION_KEY_STATUS`, `GET_ENCRYPTION_NONCE` | `fscrypt status` |
//!
//! The send stream is EclipseFS's format, not btrfs's, so it is received
//! with `EFS_IOC_RECEIVE` rather than by `btrfs receive`; and the stream
//! goes to (and comes from) a regular file, never a pipe, because an ioctl
//! here cannot wait for a reader.
//!
//! Every ioctl that changes the filesystem refuses on a read-only mount
//! (`EROFS`) and asks for what Linux asks for: `CAP_SYS_ADMIN` for devices,
//! balance, scrub, quotas, deleting subvolumes and the default subvolume;
//! ownership (or `CAP_FOWNER`) for flags; `CAP_LINUX_IMMUTABLE` to change
//! the immutable or append-only flag.
//!
//! The page cache sits above the filesystem, so whatever changes a file's
//! blocks underneath it (a reflink, a punched hole) reloads the cached pages
//! of that range, and whatever reads a file's blocks on its own (the source
//! of a reflink) first writes back what a shared mapping changed.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryInto;

use eclipsefs::cowfmt::{handle, split, FIRST_INO, FS_TREE};
use eclipsefs::{
    Compression, EclipseFS, EncryptionPolicy, Error as EfsError, FileKind, Profile, Tier,
};
use kernel_hal::user::{UserInPtr, UserOutPtr};
use rcore_fs::vfs::INode;

use super::block_mount::MountBackend;
use super::btrfs_mount::DevAdapter;
use super::eclipsefs_mount::{EclipseMountFs, EclipseMountINode};
use super::file::{cache_flush, cache_reload};
use super::{File, FileLike, SeekFrom};
use crate::error::{LxError, LxResult};

// Linux (`include/uapi/linux/fs.h`).
const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
const FS_IOC32_GETFLAGS: u32 = 0x8004_6601;
const FS_IOC32_SETFLAGS: u32 = 0x4004_6602;
const FICLONE: u32 = 0x4004_9409;
const FICLONERANGE: u32 = 0x4020_940d;
const FIDEDUPERANGE: u32 = 0xc018_9436;
const FS_IOC_FIEMAP: u32 = 0xc020_660b;

// fscrypt (`include/uapi/linux/fscrypt.h`). Only v2 policies (key
// identifiers): v1 policies and keyring keys are deprecated on Linux.
const FS_IOC_SET_ENCRYPTION_POLICY: u32 = 0x800c_6613;
const FS_IOC_GET_ENCRYPTION_POLICY: u32 = 0x400c_6615;
const FS_IOC_GET_ENCRYPTION_POLICY_EX: u32 = 0xc009_6616;
const FS_IOC_ADD_ENCRYPTION_KEY: u32 = 0xc050_6617;
const FS_IOC_REMOVE_ENCRYPTION_KEY: u32 = 0xc040_6618;
const FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS: u32 = 0xc040_6619;
const FS_IOC_GET_ENCRYPTION_KEY_STATUS: u32 = 0xc080_661a;
const FS_IOC_GET_ENCRYPTION_NONCE: u32 = 0x8010_661b;

const FSCRYPT_POLICY_V2: u8 = 2;
/// `struct fscrypt_policy_v2`.
const FSCRYPT_POLICY_V2_SIZE: usize = 24;
const FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER: u32 = 2;
/// `struct fscrypt_key_specifier`.
const FSCRYPT_KEY_SPEC: usize = 40;
/// `struct fscrypt_add_key_arg` before the raw key.
const FSCRYPT_ADD_KEY_ARG: usize = 80;
const FSCRYPT_MAX_KEY_SIZE: usize = 64;
const FSCRYPT_KEY_STATUS_ABSENT: u32 = 1;
const FSCRYPT_KEY_STATUS_PRESENT: u32 = 2;
const FSCRYPT_KEY_STATUS_FLAG_ADDED_BY_SELF: u32 = 1;

// btrfs (`include/uapi/linux/btrfs.h`).
const BTRFS_IOC_SNAP_CREATE: u32 = 0x5000_9401;
const BTRFS_IOC_DEFRAG: u32 = 0x5000_9402;
const BTRFS_IOC_RESIZE: u32 = 0x5000_9403;
const BTRFS_IOC_SYNC: u32 = 0x0000_9408;
const BTRFS_IOC_ADD_DEV: u32 = 0x5000_940a;
const BTRFS_IOC_RM_DEV: u32 = 0x5000_940b;
const BTRFS_IOC_SUBVOL_CREATE: u32 = 0x5000_940e;
const BTRFS_IOC_SNAP_DESTROY: u32 = 0x5000_940f;
const BTRFS_IOC_INO_LOOKUP: u32 = 0xd000_9412;
const BTRFS_IOC_DEFAULT_SUBVOL: u32 = 0x4008_9413;
const BTRFS_IOC_DEFRAG_RANGE: u32 = 0x4030_9410;
const BTRFS_IOC_WAIT_SYNC: u32 = 0x4008_9416;
const BTRFS_IOC_SNAP_CREATE_V2: u32 = 0x5000_9417;
const BTRFS_IOC_START_SYNC: u32 = 0x8008_9418;
const BTRFS_IOC_SUBVOL_CREATE_V2: u32 = 0x5000_9418;
const BTRFS_IOC_SUBVOL_GETFLAGS: u32 = 0x8008_9419;
const BTRFS_IOC_SUBVOL_SETFLAGS: u32 = 0x4008_941a;
const BTRFS_IOC_SCRUB: u32 = 0xc400_941b;
const BTRFS_IOC_SCRUB_CANCEL: u32 = 0x0000_941c;
const BTRFS_IOC_SCRUB_PROGRESS: u32 = 0xc400_941d;
const BTRFS_IOC_DEV_INFO: u32 = 0xd000_941e;
const BTRFS_IOC_FS_INFO: u32 = 0x8400_941f;
const BTRFS_IOC_BALANCE_V2: u32 = 0xc400_9420;
const BTRFS_IOC_BALANCE_CTL: u32 = 0x4004_9421;
const BTRFS_IOC_BALANCE_PROGRESS: u32 = 0x8400_9422;
const BTRFS_IOC_SEND: u32 = 0x4048_9426;
const BTRFS_IOC_QUOTA_CTL: u32 = 0xc010_9428;
const BTRFS_IOC_QGROUP_LIMIT: u32 = 0x8030_942b;
const BTRFS_IOC_QUOTA_RESCAN: u32 = 0x4040_942c;
const BTRFS_IOC_QUOTA_RESCAN_STATUS: u32 = 0x8040_942d;
const BTRFS_IOC_QUOTA_RESCAN_WAIT: u32 = 0x0000_942e;
const BTRFS_IOC_DEV_REPLACE: u32 = 0xca28_9435;
const BTRFS_IOC_GET_FEATURES: u32 = 0x8018_9439;
const BTRFS_IOC_RM_DEV_V2: u32 = 0x5000_943a;
const BTRFS_IOC_GET_SUBVOL_INFO: u32 = 0x81f8_943c;
const BTRFS_IOC_SNAP_DESTROY_V2: u32 = 0x5000_943f;

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
const EFS_TIER_MIGRATE_ALL: u64 = 1 << 0;

const FS_IMMUTABLE_FL: u32 = 0x10;
const FS_APPEND_FL: u32 = 0x20;

const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;
const BTRFS_SUBVOL_QGROUP_INHERIT: u64 = 1 << 2;
const BTRFS_DEVICE_SPEC_BY_ID: u64 = 1 << 3;
const BTRFS_SUBVOL_SPEC_BY_ID: u64 = 1 << 4;

const BTRFS_FS_INFO_FLAG_CSUM_INFO: u64 = 1 << 0;
const BTRFS_FS_INFO_FLAG_GENERATION: u64 = 1 << 1;
const BTRFS_FS_INFO_FLAG_METADATA_UUID: u64 = 1 << 2;

const BTRFS_BALANCE_DATA: u64 = 1 << 0;
const BTRFS_BALANCE_SYSTEM: u64 = 1 << 1;
const BTRFS_BALANCE_METADATA: u64 = 1 << 2;
const BTRFS_BALANCE_RESUME: u64 = 1 << 4;
const BTRFS_BALANCE_ARGS_CONVERT: u64 = 1 << 8;

const BTRFS_BLOCK_GROUP_RAID0: u64 = 1 << 3;
const BTRFS_BLOCK_GROUP_RAID1: u64 = 1 << 4;
const BTRFS_BLOCK_GROUP_DUP: u64 = 1 << 5;
const BTRFS_BLOCK_GROUP_RAID10: u64 = 1 << 6;
const BTRFS_BLOCK_GROUP_RAID5: u64 = 1 << 7;
const BTRFS_BLOCK_GROUP_RAID6: u64 = 1 << 8;
const BTRFS_BLOCK_GROUP_RAID1C3: u64 = 1 << 9;
const BTRFS_BLOCK_GROUP_RAID1C4: u64 = 1 << 10;
const BTRFS_AVAIL_ALLOC_BIT_SINGLE: u64 = 1 << 48;

const BTRFS_DEFRAG_RANGE_COMPRESS: u64 = 1 << 0;
const BTRFS_DEFRAG_RANGE_START_IO: u64 = 1 << 1;

const BTRFS_QGROUP_LIMIT_MAX_RFER: u64 = 1 << 0;
const BTRFS_SEND_FLAG_VERSION: u64 = 1 << 3;

const FILE_DEDUPE_RANGE_SAME: i32 = 0;
const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;

/// `struct fiemap`: the header, then `fm_extent_count` extents.
const FIEMAP_HEADER: usize = 32;
const FIEMAP_EXTENT: usize = 56;
/// The most extents one call fills (Linux bounds it by what the caller's
/// buffer holds; this bounds what the kernel allocates for it).
const FIEMAP_MAX_EXTENTS: usize = 1 << 16;
const FIEMAP_FLAG_SYNC: u32 = 0x1;

const CAP_FOWNER: u32 = 3;
const CAP_LINUX_IMMUTABLE: u32 = 9;
const CAP_SYS_ADMIN: u32 = 21;

const BLOCK: u64 = 4096;

/// What an ioctl needs from the process that calls it.
pub trait FsIoctlContext {
    /// The open file behind descriptor `fd` (`EBADF` if none).
    fn file(&self, fd: i32) -> LxResult<Arc<File>>;
    /// The inode at `path` (a device to add, for instance).
    fn lookup(&self, path: &str) -> LxResult<Arc<dyn INode>>;
    fn capable(&self, cap: u32) -> bool;
    fn euid(&self) -> u32;
}

/// The EclipseFS inode behind `inode`, through the mount wrappers.
fn efs_of(inode: &Arc<dyn INode>) -> Option<(Arc<EclipseMountFs>, u64)> {
    let e = inode.as_any_ref().downcast_ref::<EclipseMountINode>()?;
    Some((e.fs.clone(), e.ino))
}

fn errno(e: EfsError) -> LxError {
    match e {
        EfsError::Io => LxError::EIO,
        EfsError::BadSuperblock => LxError::EINVAL,
        EfsError::Corrupt(msg) => {
            warn!("eclipsefs: corrupt filesystem: {}", msg);
            LxError::EIO
        }
        EfsError::Unsupported(_) => LxError::EOPNOTSUPP,
        EfsError::NotFound => LxError::ENOENT,
        EfsError::Exists => LxError::EEXIST,
        EfsError::NotDir => LxError::ENOTDIR,
        EfsError::IsDir => LxError::EISDIR,
        EfsError::NotEmpty => LxError::ENOTEMPTY,
        EfsError::NoSpace => LxError::ENOSPC,
        EfsError::Invalid => LxError::EINVAL,
        EfsError::NoKey => LxError::ENOKEY,
    }
}

fn get16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn get64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// The NUL-terminated string in `b[at..at + max]` (`ENAMETOOLONG` without a
/// NUL there, as btrfs answers).
fn cstr(b: &[u8], at: usize, max: usize) -> LxResult<&str> {
    let field = &b[at..at + max];
    let end = field
        .iter()
        .position(|&c| c == 0)
        .ok_or(LxError::ENAMETOOLONG)?;
    core::str::from_utf8(&field[..end]).map_err(|_| LxError::EINVAL)
}

/// A name for a new or deleted subvolume: one path component.
fn subvol_name(b: &[u8], at: usize, max: usize) -> LxResult<&str> {
    let name = cstr(b, at, max)?;
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(LxError::EINVAL);
    }
    Ok(name)
}

/// A byte count as the kernel's `memparse` reads it: digits and an optional
/// `k`/`m`/`g`/`t`/`p`/`e` suffix in powers of 1024. `None` for anything
/// else, an overflow or zero.
fn memparse(s: &str) -> Option<u64> {
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

fn put_cstr(b: &mut [u8], at: usize, max: usize, s: &str) -> LxResult<()> {
    if s.len() >= max {
        return Err(LxError::ENAMETOOLONG);
    }
    b[at..at + s.len()].copy_from_slice(s.as_bytes());
    b[at + s.len()] = 0;
    Ok(())
}

/// Whether `cmd` is one of the ioctls this module answers.
pub(crate) fn is_fs_ioctl(cmd: u32) -> bool {
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

/// `ioctl(2)` on a file of an EclipseFS mount: `None` when `file` is on
/// another filesystem or `cmd` is not a filesystem ioctl (the caller then
/// asks the inode as before).
pub fn eclipsefs_ioctl(
    file: &File,
    cmd: u32,
    arg: usize,
    ctx: &dyn FsIoctlContext,
) -> Option<LxResult<usize>> {
    if !is_fs_ioctl(cmd) {
        return None;
    }
    let target = Target::of(file)?;
    Some(target.user_ioctl(cmd, arg, ctx))
}

/// The file an ioctl is called on, and the EclipseFS inode behind it.
pub(crate) struct Target<'a> {
    file: &'a File,
    fs: Arc<EclipseMountFs>,
    ino: u64,
}

impl<'a> Target<'a> {
    pub(crate) fn of(file: &'a File) -> Option<Self> {
        let (fs, ino) = efs_of(&file.inode())?;
        Some(Self { file, fs, ino })
    }

    /// Copy the argument in from user memory (as its size in `cmd` says),
    /// run the ioctl, and copy it back out for the ones that return data.
    fn user_ioctl(&self, cmd: u32, arg: usize, ctx: &dyn FsIoctlContext) -> LxResult<usize> {
        if cmd == FS_IOC_FIEMAP {
            // The header goes back to the caller even when the call fails
            // (the flags it did not understand, for EBADR).
            let head = UserInPtr::<u8>::from(arg).read_array(FIEMAP_HEADER)?;
            let count = (get32(&head, 24) as usize).min(FIEMAP_MAX_EXTENTS);
            let mut buf = vec![0u8; FIEMAP_HEADER + FIEMAP_EXTENT * count];
            buf[..FIEMAP_HEADER].copy_from_slice(&head);
            let ret = self.fiemap(&mut buf);
            let mapped = get32(&buf, 20) as usize;
            let out = FIEMAP_HEADER + FIEMAP_EXTENT * mapped.min(count);
            UserOutPtr::<u8>::from(arg).write_array(&buf[..out])?;
            return ret;
        }
        let mut buf = match cmd {
            // Its argument is not read (often NULL).
            BTRFS_IOC_DEFRAG => Vec::new(),
            // The argument is the source descriptor itself.
            FICLONE => (arg as i32).to_le_bytes().to_vec(),
            // An `int`, whatever `long` in the number says (as Linux reads it).
            FS_IOC_GETFLAGS | FS_IOC_SETFLAGS | FS_IOC32_GETFLAGS | FS_IOC32_SETFLAGS => {
                UserInPtr::<u8>::from(arg).read_array(4)?
            }
            // The version byte says which policy struct follows (the
            // number carries the size of the v1 one).
            FS_IOC_SET_ENCRYPTION_POLICY => {
                let version = UserInPtr::<u8>::from(arg).read_array(1)?[0];
                let size = if version == FSCRYPT_POLICY_V2 {
                    FSCRYPT_POLICY_V2_SIZE
                } else {
                    12
                };
                UserInPtr::<u8>::from(arg).read_array(size)?
            }
            // The size the caller has room for, then that room.
            FS_IOC_GET_ENCRYPTION_POLICY_EX => {
                let head = UserInPtr::<u8>::from(arg).read_array(8)?;
                let room = (get64(&head, 0) as usize).min(FSCRYPT_POLICY_V2_SIZE);
                UserInPtr::<u8>::from(arg).read_array(8 + room)?
            }
            // The raw key follows the struct.
            FS_IOC_ADD_ENCRYPTION_KEY => {
                let head = UserInPtr::<u8>::from(arg).read_array(FSCRYPT_ADD_KEY_ARG)?;
                let raw = get32(&head, 40) as usize;
                if raw > FSCRYPT_MAX_KEY_SIZE {
                    return Err(LxError::EINVAL);
                }
                UserInPtr::<u8>::from(arg).read_array(FSCRYPT_ADD_KEY_ARG + raw)?
            }
            // A header and `dest_count` entries after it.
            FIDEDUPERANGE => {
                let head = UserInPtr::<u8>::from(arg).read_array(24)?;
                let count = get16(&head, 16) as usize;
                UserInPtr::<u8>::from(arg).read_array(24 + 32 * count)?
            }
            _ => {
                let size = ((cmd >> 16) & 0x3fff) as usize;
                UserInPtr::<u8>::from(arg).read_array(size)?
            }
        };
        let ret = self.ioctl(cmd, &mut buf, ctx)?;
        // SET_ENCRYPTION_POLICY is `_IOR` but only reads; the raw key goes
        // back to nobody.
        let returns_data =
            cmd >> 30 & 2 != 0 && cmd != FICLONE && cmd != FS_IOC_SET_ENCRYPTION_POLICY;
        let out = if cmd == FS_IOC_ADD_ENCRYPTION_KEY {
            &buf[..FSCRYPT_ADD_KEY_ARG]
        } else {
            &buf[..]
        };
        if returns_data && !out.is_empty() {
            UserOutPtr::<u8>::from(arg).write_array(out)?;
        }
        if cmd == FS_IOC_ADD_ENCRYPTION_KEY {
            buf.fill(0);
        }
        Ok(ret)
    }

    /// The fscrypt ioctls, with `buf` laid out as `fscrypt.h` lays out
    /// their structs.
    fn fscrypt(&self, cmd: u32, buf: &mut [u8], ctx: &dyn FsIoctlContext) -> LxResult<usize> {
        match cmd {
            FS_IOC_SET_ENCRYPTION_POLICY => {
                if buf.first() != Some(&FSCRYPT_POLICY_V2) || buf.len() < FSCRYPT_POLICY_V2_SIZE {
                    // v1 policies (master key descriptors) are not supported.
                    return Err(LxError::EINVAL);
                }
                if buf[4..8] != [0; 4] {
                    return Err(LxError::EINVAL);
                }
                let policy = EncryptionPolicy {
                    contents_mode: buf[1],
                    filenames_mode: buf[2],
                    flags: buf[3],
                    key_id: buf[8..24].try_into().unwrap(),
                };
                // `inode_owner_or_capable`, else EACCES as fscrypt says.
                self.owner(ctx).map_err(|_| LxError::EACCES)?;
                self.want_write()?;
                self.with(|fs| fs.set_encryption_policy(self.ino, &policy))?;
            }
            FS_IOC_GET_ENCRYPTION_POLICY => {
                // The v1 call cannot return a v2 policy.
                return match self.with(|fs| fs.encryption_policy(self.ino))? {
                    Some(_) => Err(LxError::EINVAL),
                    None => Err(LxError::ENODATA),
                };
            }
            FS_IOC_GET_ENCRYPTION_POLICY_EX => {
                let p = self
                    .with(|fs| fs.encryption_policy(self.ino))?
                    .ok_or(LxError::ENODATA)?;
                if buf.len() < 8 + FSCRYPT_POLICY_V2_SIZE {
                    return Err(LxError::EOVERFLOW);
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
                // A raw key named by its identifier; keyring keys
                // (`key_id`) and v1 descriptors are not supported.
                if get32(buf, 0) != FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER
                    || get32(buf, 4) != 0
                    || get32(buf, 44) != 0
                    || buf[48..FSCRYPT_ADD_KEY_ARG].iter().any(|&b| b != 0)
                {
                    return Err(LxError::EINVAL);
                }
                let raw = get32(buf, 40) as usize;
                let id = self.with(|fs| {
                    fs.add_encryption_key(&buf[FSCRYPT_ADD_KEY_ARG..FSCRYPT_ADD_KEY_ARG + raw])
                })?;
                buf[8..FSCRYPT_KEY_SPEC].fill(0);
                buf[8..24].copy_from_slice(&id);
                self.fs.invalidate_all_dirs();
            }
            FS_IOC_REMOVE_ENCRYPTION_KEY | FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS => {
                if cmd == FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS {
                    Self::admin(ctx).map_err(|_| LxError::EACCES)?;
                }
                let id = Self::key_identifier(buf)?;
                if buf[FSCRYPT_KEY_SPEC..].iter().any(|&b| b != 0) {
                    return Err(LxError::EINVAL);
                }
                match self.with(|fs| fs.remove_encryption_key(&id)) {
                    Err(LxError::ENOENT) => return Err(LxError::ENOKEY),
                    r => r?,
                }
                // `removal_status_flags`: every file let go of the key.
                put32(buf, FSCRYPT_KEY_SPEC, 0);
                self.fs.invalidate_all_dirs();
            }
            FS_IOC_GET_ENCRYPTION_KEY_STATUS => {
                let id = Self::key_identifier(buf)?;
                let present = self.with(|fs| fs.encryption_key_present(&id))?;
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
                let nonce = match self.with(|fs| fs.encryption_nonce(self.ino)) {
                    Err(LxError::ENOENT) => return Err(LxError::ENODATA),
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
    fn key_identifier(buf: &[u8]) -> LxResult<[u8; 16]> {
        if get32(buf, 0) != FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER || get32(buf, 4) != 0 {
            return Err(LxError::EINVAL);
        }
        Ok(buf[8..24].try_into().unwrap())
    }

    /// `FS_IOC_FIEMAP` on `buf` (a `struct fiemap` with room for
    /// `fm_extent_count` extents): fills `fm_mapped_extents` and the
    /// extents, or with no room only counts them.
    pub(crate) fn fiemap(&self, buf: &mut [u8]) -> LxResult<usize> {
        let start = get64(buf, 0);
        let len = get64(buf, 8);
        let flags = get32(buf, 16);
        let room = (buf.len() - FIEMAP_HEADER) / FIEMAP_EXTENT;
        put32(buf, 20, 0);
        if flags & !FIEMAP_FLAG_SYNC != 0 {
            put32(buf, 16, flags & !FIEMAP_FLAG_SYNC);
            return Err(LxError::EBADR);
        }
        if len == 0 {
            return Err(LxError::EINVAL);
        }
        // Data still in the page cache is mapped once written back.
        if let Ok(st) = self.stat(self.ino) {
            cache_flush(&self.file.inode(), 0, st.size as usize);
        }
        if flags & FIEMAP_FLAG_SYNC != 0 {
            self.with(|fs| fs.sync())?;
        }
        let map = match self.with(|fs| fs.fiemap(self.ino, start, len)) {
            // A directory has no data to map (btrfs answers the same).
            Err(LxError::EISDIR) => Vec::new(),
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

    fn with<R>(&self, f: impl FnOnce(&mut EclipseFS) -> eclipsefs::Result<R>) -> LxResult<R> {
        f(&mut self.fs.inner.lock()).map_err(errno)
    }

    /// `mnt_want_write_file`: EROFS on a read-only mount.
    fn want_write(&self) -> LxResult<()> {
        if self.fs.read_only || super::path_is_read_only(self.file.path()) {
            return Err(LxError::EROFS);
        }
        Ok(())
    }

    fn admin(ctx: &dyn FsIoctlContext) -> LxResult<()> {
        if ctx.capable(CAP_SYS_ADMIN) {
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    fn stat(&self, ino: u64) -> LxResult<eclipsefs::InodeStat> {
        self.with(|fs| fs.stat(ino))
    }

    /// `inode_owner_or_capable`.
    fn owner(&self, ctx: &dyn FsIoctlContext) -> LxResult<()> {
        if self.stat(self.ino)?.uid == ctx.euid() || ctx.capable(CAP_FOWNER) {
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    /// Write and search permission on this directory, to add or remove a
    /// name in it.
    fn may_change_dir(&self, ctx: &dyn FsIoctlContext) -> LxResult<()> {
        let st = self.stat(self.ino)?;
        if st.kind != FileKind::Dir {
            return Err(LxError::ENOTDIR);
        }
        let allowed = ctx.capable(CAP_SYS_ADMIN)
            || (st.uid == ctx.euid() && st.mode & 0o300 == 0o300)
            || st.mode & 0o003 == 0o003;
        if allowed {
            Ok(())
        } else {
            Err(LxError::EACCES)
        }
    }

    /// The other file of a reflink or dedupe, which must be on this very
    /// mount.
    fn same_fs(&self, other: &File) -> LxResult<u64> {
        let (fs, ino) = efs_of(&other.inode()).ok_or(LxError::EXDEV)?;
        if !Arc::ptr_eq(&fs, &self.fs) {
            return Err(LxError::EXDEV);
        }
        Ok(ino)
    }

    /// A regular file for a reflink or dedupe (`EISDIR`, else `EINVAL`).
    fn regular(&self, ino: u64) -> LxResult<eclipsefs::InodeStat> {
        let st = self.stat(ino)?;
        match st.kind {
            FileKind::Regular => Ok(st),
            FileKind::Dir => Err(LxError::EISDIR),
            _ => Err(LxError::EINVAL),
        }
    }

    /// The subvolume this inode is in, which must be its root directory.
    fn subvol_root(&self) -> LxResult<u64> {
        let (id, local) = split(self.ino);
        if local != FIRST_INO {
            return Err(LxError::EINVAL);
        }
        Ok(id)
    }

    fn subvol_info(&self, id: u64) -> LxResult<eclipsefs::SubvolInfo> {
        self.with(|fs| fs.list_subvolumes())?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or(LxError::ENOENT)
    }

    fn devices(&self) -> Vec<eclipsefs::DeviceInfo> {
        let fs = self.fs.inner.lock();
        let devs = fs.devices();
        if !devs.is_empty() {
            return devs;
        }
        // A filesystem on one device that never became a volume.
        let st = fs.fsinfo();
        vec![eclipsefs::DeviceInfo {
            devid: 1,
            size_bytes: st.total_bytes,
            allocated_bytes: st.bytes_used,
            present: true,
            tier: Tier::Any,
        }]
    }

    /// Run ioctl `cmd` with its argument already in `buf` (and `buf` holding
    /// what goes back to the caller after).
    pub(crate) fn ioctl(
        &self,
        cmd: u32,
        buf: &mut [u8],
        ctx: &dyn FsIoctlContext,
    ) -> LxResult<usize> {
        match cmd {
            FS_IOC_FIEMAP => return self.fiemap(buf),
            FS_IOC_GETFLAGS | FS_IOC32_GETFLAGS => {
                let flags = self.with(|fs| fs.get_flags(self.ino))?;
                put32(buf, 0, flags);
            }
            FS_IOC_SETFLAGS | FS_IOC32_SETFLAGS => {
                self.owner(ctx)?;
                self.want_write()?;
                let new = get32(buf, 0);
                let old = self.with(|fs| fs.get_flags(self.ino))?;
                if (old ^ new) & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0
                    && !ctx.capable(CAP_LINUX_IMMUTABLE)
                {
                    return Err(LxError::EPERM);
                }
                self.with(|fs| fs.set_flags(self.ino, new))?;
            }
            FICLONE => {
                let src = ctx.file(get32(buf, 0) as i32)?;
                self.clone_from(&src, 0, 0, 0)?;
            }
            FICLONERANGE => {
                let src = ctx.file(get64(buf, 0) as i64 as i32)?;
                self.clone_from(&src, get64(buf, 8), get64(buf, 16), get64(buf, 24))?;
            }
            FIDEDUPERANGE => self.dedupe(buf, ctx)?,
            FS_IOC_SET_ENCRYPTION_POLICY
            | FS_IOC_GET_ENCRYPTION_POLICY
            | FS_IOC_GET_ENCRYPTION_POLICY_EX
            | FS_IOC_ADD_ENCRYPTION_KEY
            | FS_IOC_REMOVE_ENCRYPTION_KEY
            | FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS
            | FS_IOC_GET_ENCRYPTION_KEY_STATUS
            | FS_IOC_GET_ENCRYPTION_NONCE => return self.fscrypt(cmd, buf, ctx),
            BTRFS_IOC_DEFRAG => self.defrag(None, ctx)?,
            BTRFS_IOC_DEFRAG_RANGE => self.defrag(Some(buf), ctx)?,
            BTRFS_IOC_SYNC | BTRFS_IOC_WAIT_SYNC => self.with(|fs| fs.sync())?,
            BTRFS_IOC_START_SYNC => {
                let gen = self.with(|fs| fs.sync().map(|_| fs.generation()))?;
                put64(buf, 0, gen);
            }
            BTRFS_IOC_SUBVOL_CREATE | BTRFS_IOC_SUBVOL_CREATE_V2 => {
                let name = if cmd == BTRFS_IOC_SUBVOL_CREATE {
                    subvol_name(buf, 8, 4088)?
                } else {
                    if get64(buf, 16) & !BTRFS_SUBVOL_QGROUP_INHERIT != 0 {
                        return Err(LxError::EOPNOTSUPP);
                    }
                    subvol_name(buf, 56, 4040)?
                };
                self.want_write()?;
                self.may_change_dir(ctx)?;
                self.with(|fs| fs.create_subvolume(self.ino, name))?;
                self.fs.invalidate_dir(self.ino);
            }
            BTRFS_IOC_SNAP_CREATE | BTRFS_IOC_SNAP_CREATE_V2 => {
                let (name, read_only) = if cmd == BTRFS_IOC_SNAP_CREATE {
                    (subvol_name(buf, 8, 4088)?, false)
                } else {
                    let flags = get64(buf, 16);
                    if flags & !(BTRFS_SUBVOL_RDONLY | BTRFS_SUBVOL_QGROUP_INHERIT) != 0 {
                        return Err(LxError::EOPNOTSUPP);
                    }
                    (
                        subvol_name(buf, 56, 4040)?,
                        flags & BTRFS_SUBVOL_RDONLY != 0,
                    )
                };
                let src = ctx.file(get64(buf, 0) as i64 as i32)?;
                let src = self.same_fs(&src)?;
                self.want_write()?;
                self.may_change_dir(ctx)?;
                self.with(|fs| fs.snapshot(src, self.ino, name, read_only))?;
                self.fs.invalidate_dir(self.ino);
            }
            BTRFS_IOC_SNAP_DESTROY | BTRFS_IOC_SNAP_DESTROY_V2 => {
                let name = if cmd == BTRFS_IOC_SNAP_DESTROY {
                    subvol_name(buf, 8, 4088)?
                } else {
                    if get64(buf, 16) & BTRFS_SUBVOL_SPEC_BY_ID != 0 {
                        return Err(LxError::EOPNOTSUPP);
                    }
                    subvol_name(buf, 56, 4040)?
                };
                Self::admin(ctx)?;
                self.want_write()?;
                self.with(|fs| fs.delete_subvolume(self.ino, name))?;
                self.fs.invalidate_dir(self.ino);
            }
            BTRFS_IOC_SUBVOL_GETFLAGS => {
                let id = self.subvol_root()?;
                let ro = id != FS_TREE && self.subvol_info(id)?.read_only;
                put64(buf, 0, if ro { BTRFS_SUBVOL_RDONLY } else { 0 });
            }
            BTRFS_IOC_SUBVOL_SETFLAGS => {
                let flags = get64(buf, 0);
                if flags & !BTRFS_SUBVOL_RDONLY != 0 {
                    return Err(LxError::EOPNOTSUPP);
                }
                let id = self.subvol_root()?;
                self.owner(ctx)?;
                self.want_write()?;
                self.with(|fs| fs.set_subvolume_read_only(id, flags != 0))?;
            }
            BTRFS_IOC_DEFAULT_SUBVOL => {
                Self::admin(ctx)?;
                self.want_write()?;
                let id = match get64(buf, 0) {
                    0 => FS_TREE,
                    id => id,
                };
                self.with(|fs| fs.set_default_subvolume(id))?;
            }
            BTRFS_IOC_GET_SUBVOL_INFO => self.get_subvol_info(buf)?,
            BTRFS_IOC_INO_LOOKUP => self.ino_lookup(buf, ctx)?,
            BTRFS_IOC_FS_INFO => self.fs_info(buf),
            BTRFS_IOC_DEV_INFO => self.dev_info(buf)?,
            BTRFS_IOC_GET_FEATURES => buf.fill(0),
            BTRFS_IOC_SCRUB => self.scrub(buf, ctx)?,
            BTRFS_IOC_SCRUB_CANCEL | BTRFS_IOC_SCRUB_PROGRESS => return Err(LxError::ENOTCONN),
            BTRFS_IOC_QUOTA_CTL => {
                Self::admin(ctx)?;
                self.want_write()?;
                match get64(buf, 0) {
                    // ENABLE and ENABLE_SIMPLE_QUOTA.
                    1 | 4 => self.with(|fs| fs.quota_enable())?,
                    2 => self.with(|fs| fs.quota_disable())?,
                    _ => return Err(LxError::EINVAL),
                }
                put64(buf, 8, 0);
            }
            BTRFS_IOC_QUOTA_RESCAN => {
                Self::admin(ctx)?;
                self.want_write()?;
                if get64(buf, 0) != 0 {
                    return Err(LxError::EINVAL);
                }
                self.with(|fs| fs.quota_rescan())?;
            }
            // The rescan above runs to the end before it returns.
            BTRFS_IOC_QUOTA_RESCAN_STATUS => {
                Self::admin(ctx)?;
                buf.fill(0);
            }
            BTRFS_IOC_QUOTA_RESCAN_WAIT => {}
            BTRFS_IOC_QGROUP_LIMIT => {
                Self::admin(ctx)?;
                self.want_write()?;
                let id = match get64(buf, 0) {
                    0 => split(self.ino).0,
                    id if id >> 48 != 0 => return Err(LxError::EOPNOTSUPP),
                    id => id,
                };
                let flags = get64(buf, 8);
                if flags & !BTRFS_QGROUP_LIMIT_MAX_RFER != 0 {
                    return Err(LxError::EOPNOTSUPP);
                }
                if flags != 0 {
                    let limit = match get64(buf, 16) {
                        u64::MAX => None,
                        max => Some(max),
                    };
                    self.with(|fs| fs.set_qgroup_limit(id, limit))?;
                }
            }
            BTRFS_IOC_RESIZE => {
                Self::admin(ctx)?;
                self.want_write()?;
                self.resize(cstr(buf, 8, 4088)?)?;
            }
            BTRFS_IOC_ADD_DEV => {
                Self::admin(ctx)?;
                self.want_write()?;
                let dev = self.open_device(cstr(buf, 8, 4088)?, ctx)?;
                if eclipsefs::probe(&*dev).is_ok_and(|p| p.uuid == self.uuid()) {
                    return Err(LxError::EEXIST);
                }
                self.with(|fs| fs.add_device(dev))?;
            }
            BTRFS_IOC_RM_DEV | BTRFS_IOC_RM_DEV_V2 => {
                Self::admin(ctx)?;
                self.want_write()?;
                let devid = if cmd == BTRFS_IOC_RM_DEV_V2
                    && get64(buf, 16) & BTRFS_DEVICE_SPEC_BY_ID != 0
                {
                    get64(buf, 56) as u32
                } else if cmd == BTRFS_IOC_RM_DEV_V2 {
                    self.devid_of(cstr(buf, 56, 4040)?, ctx)?
                } else {
                    self.devid_of(cstr(buf, 8, 4088)?, ctx)?
                };
                self.with(|fs| fs.remove_device(devid))?;
            }
            BTRFS_IOC_DEV_REPLACE => self.dev_replace(buf, ctx)?,
            BTRFS_IOC_BALANCE_V2 => self.balance(buf, ctx)?,
            BTRFS_IOC_BALANCE_CTL | BTRFS_IOC_BALANCE_PROGRESS => return Err(LxError::ENOTCONN),
            BTRFS_IOC_SEND => self.send(buf, ctx)?,
            EFS_IOC_RECEIVE => self.receive(buf, ctx)?,
            EFS_IOC_SET_TIER => {
                Self::admin(ctx)?;
                self.want_write()?;
                let (devid, tier) = (get64(buf, 0), get64(buf, 8));
                if devid > u32::MAX as u64 || tier > u8::MAX as u64 {
                    return Err(LxError::EINVAL);
                }
                let tier = Tier::from_id(tier as u8).map_err(|_| LxError::EINVAL)?;
                let devid = devid as u32;
                self.with(|fs| fs.set_device_tier(devid, tier))?;
            }
            EFS_IOC_GET_TIER => {
                let devid = get64(buf, 0);
                let dev = self
                    .devices()
                    .into_iter()
                    .find(|d| d.devid as u64 == devid)
                    .ok_or(LxError::ENODEV)?;
                put64(buf, 8, dev.tier.id() as u64);
            }
            EFS_IOC_TIER_MIGRATE => {
                Self::admin(ctx)?;
                self.want_write()?;
                let all = get64(buf, 0) & EFS_TIER_MIGRATE_ALL != 0;
                let r = self.with(|fs| fs.tier_migrate(all))?;
                put64(buf, 8, r.relocated);
            }
            _ => return Err(LxError::ENOTTY),
        }
        Ok(0)
    }

    fn uuid(&self) -> [u8; 16] {
        self.fs.inner.lock().uuid()
    }

    /// `FICLONE`/`FICLONERANGE` on this file (the destination) from `src`.
    fn clone_from(&self, src: &File, src_off: u64, len: u64, dst_off: u64) -> LxResult<()> {
        let dst_flags = self.file.flags();
        if !dst_flags.writable() || dst_flags.is_append() || !src.flags().readable() {
            return Err(LxError::EBADF);
        }
        let src_ino = self.same_fs(src)?;
        let src_size = self.regular(src_ino)?.size;
        self.regular(self.ino)?;
        self.want_write()?;
        let len_or_rest = if len == 0 {
            src_size.saturating_sub(src_off)
        } else {
            len
        };
        if src_ino == self.ino && src_off < dst_off + len_or_rest && dst_off < src_off + len_or_rest
        {
            return Err(LxError::EINVAL);
        }
        cache_flush(&src.inode(), src_off as usize, len_or_rest as usize);
        let n = self.with(|fs| fs.clone_range(src_ino, src_off, self.ino, dst_off, len))?;
        cache_reload(&self.file.inode(), dst_off as usize, n as usize);
        Ok(())
    }

    /// `FIDEDUPERANGE`, called on the source.
    /// `BTRFS_IOC_DEFRAG` (the whole file) and `BTRFS_IOC_DEFRAG_RANGE`
    /// (`struct btrfs_ioctl_defrag_range_args`), as `btrfs filesystem
    /// defragment [-c]` calls them. A file must be open for writing, or
    /// its owner's, or the caller `CAP_SYS_ADMIN`; a directory (btrfs
    /// defragments its metadata, which here is rewritten on every change)
    /// needs `CAP_SYS_ADMIN` and has nothing to do.
    fn defrag(&self, range: Option<&[u8]>, ctx: &dyn FsIoctlContext) -> LxResult<()> {
        self.want_write()?;
        let st = self.stat(self.ino)?;
        match st.kind {
            FileKind::Dir => return Self::admin(ctx),
            FileKind::Regular => {}
            _ => return Err(LxError::EINVAL),
        }
        if !self.file.flags().writable() && !ctx.capable(CAP_SYS_ADMIN) {
            self.owner(ctx)?;
        }
        let (mut start, mut len, mut threshold, mut compress, mut sync) =
            (0, u64::MAX, 0, None, false);
        if let Some(b) = range {
            let flags = get64(b, 16);
            if flags & !(BTRFS_DEFRAG_RANGE_COMPRESS | BTRFS_DEFRAG_RANGE_START_IO) != 0 {
                return Err(LxError::EOPNOTSUPP);
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
                    _ => return Err(LxError::EINVAL),
                });
            }
            sync = flags & BTRFS_DEFRAG_RANGE_START_IO != 0;
        }
        // What the page cache holds is defragmented with the rest. The
        // bytes do not change, so nothing cached needs reloading after.
        cache_flush(&self.file.inode(), 0, st.size as usize);
        self.with(|fs| fs.defrag(self.ino, start, len, threshold, compress))?;
        if sync {
            self.with(|fs| fs.sync())?;
        }
        Ok(())
    }

    /// `BTRFS_IOC_RESIZE` with what `btrfs filesystem resize` passes:
    /// `[devid:]size`, the size being `max`, bytes with an optional
    /// `k`/`m`/`g`/`t`/`p`/`e` (powers of 1024), or such an amount after
    /// `+` or `-` to add to or take from the device's current size.
    fn resize(&self, arg: &str) -> LxResult<()> {
        let (devid, spec) = match arg.split_once(':') {
            Some((d, s)) => (d.parse::<u32>().map_err(|_| LxError::EINVAL)?, s),
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
            let n = memparse(amount).ok_or(LxError::EINVAL)?;
            let cur = self
                .devices()
                .iter()
                .find(|d| d.devid == devid)
                .ok_or(LxError::ENODEV)?
                .size_bytes;
            Some(match sign {
                1 => cur.checked_add(n).ok_or(LxError::EINVAL)?,
                -1 => cur.checked_sub(n).ok_or(LxError::EINVAL)?,
                _ => n,
            })
        };
        match self.with(|fs| fs.resize(devid, size)) {
            Err(LxError::ENOENT) => Err(LxError::ENODEV),
            r => r.map(|_| ()),
        }
    }

    fn dedupe(&self, buf: &mut [u8], ctx: &dyn FsIoctlContext) -> LxResult<()> {
        let src_off = get64(buf, 0);
        let len = get64(buf, 8);
        let count = get16(buf, 16) as usize;
        if get16(buf, 18) != 0 || get32(buf, 20) != 0 {
            return Err(LxError::EINVAL);
        }
        if !self.file.flags().readable() {
            return Err(LxError::EBADF);
        }
        self.regular(self.ino)?;
        self.want_write()?;
        cache_flush(&self.file.inode(), src_off as usize, len as usize);
        for i in 0..count {
            let at = 24 + 32 * i;
            if get32(buf, at + 28) != 0 {
                return Err(LxError::EINVAL);
            }
            let dst_off = get64(buf, at + 8);
            let (status, done) =
                match self.dedupe_one(get64(buf, at) as i64 as i32, src_off, dst_off, len, ctx) {
                    Ok(0) if len > 0 => (FILE_DEDUPE_RANGE_DIFFERS, 0),
                    Ok(n) => (FILE_DEDUPE_RANGE_SAME, n),
                    Err(e) => (-(e as i32), 0),
                };
            put64(buf, at + 16, done);
            put32(buf, at + 24, status as u32);
        }
        Ok(())
    }

    fn dedupe_one(
        &self,
        fd: i32,
        src_off: u64,
        dst_off: u64,
        len: u64,
        ctx: &dyn FsIoctlContext,
    ) -> LxResult<u64> {
        let dst = ctx.file(fd)?;
        let dst_ino = self.same_fs(&dst)?;
        let st = self.regular(dst_ino)?;
        // `allow_file_dedupe`: open for writing, or the caller's own file.
        if !dst.flags().writable() && st.uid != ctx.euid() && !ctx.capable(CAP_SYS_ADMIN) {
            return Err(LxError::EPERM);
        }
        cache_flush(&dst.inode(), dst_off as usize, len as usize);
        let n = self.with(|fs| fs.dedupe_range(self.ino, src_off, dst_ino, dst_off, len))?;
        cache_reload(&dst.inode(), dst_off as usize, n as usize);
        Ok(n)
    }

    fn get_subvol_info(&self, buf: &mut [u8]) -> LxResult<()> {
        let id = split(self.ino).0;
        let info = self.subvol_info(id)?;
        buf.fill(0);
        put64(buf, 0, info.id);
        put_cstr(buf, 8, 256, &info.name)?;
        put64(buf, 264, info.parent);
        put64(buf, 272, if info.dir == 0 { 0 } else { split(info.dir).1 });
        put64(buf, 280, self.with(|fs| Ok(fs.generation()))?);
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
    fn ino_lookup(&self, buf: &mut [u8], ctx: &dyn FsIoctlContext) -> LxResult<()> {
        let tree = match get64(buf, 0) {
            0 => split(self.ino).0,
            id => id,
        };
        let objectid = get64(buf, 8);
        if objectid != FIRST_INO && !ctx.capable(CAP_SYS_ADMIN) {
            return Err(LxError::EPERM);
        }
        let mut parts: Vec<String> = Vec::new();
        let mut cur = handle(tree, objectid);
        {
            let mut fs = self.fs.inner.lock();
            while split(cur).1 != FIRST_INO {
                let parent = fs.parent(cur).map_err(errno)?;
                let name = fs
                    .readdir(parent)
                    .map_err(errno)?
                    .into_iter()
                    .find(|e| e.ino == cur)
                    .map(|e| e.name)
                    .ok_or(LxError::ENOENT)?;
                parts.push(name);
                if split(parent).0 != tree {
                    return Err(LxError::ENOENT);
                }
                cur = parent;
            }
        }
        let mut path = String::new();
        for part in parts.iter().rev() {
            path.push_str(part);
            path.push('/');
        }
        put64(buf, 0, tree);
        put_cstr(buf, 16, 4080, &path)
    }

    fn fs_info(&self, buf: &mut [u8]) {
        let want = get64(buf, 48);
        let devs = self.devices();
        let (uuid, gen) = {
            let fs = self.fs.inner.lock();
            (fs.uuid(), fs.generation())
        };
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

    fn dev_info(&self, buf: &mut [u8]) -> LxResult<()> {
        let devid = get64(buf, 0);
        let dev = self
            .devices()
            .into_iter()
            .find(|d| d.devid as u64 == devid && d.present)
            .ok_or(LxError::ENODEV)?;
        let uuid = self.uuid();
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
    fn scrub(&self, buf: &mut [u8], ctx: &dyn FsIoctlContext) -> LxResult<()> {
        Self::admin(ctx)?;
        let devid = get64(buf, 0);
        let devs = self.devices();
        if !devs.iter().any(|d| d.devid as u64 == devid && d.present) {
            return Err(LxError::ENODEV);
        }
        buf[32..152].fill(0);
        let first = devs.iter().filter(|d| d.present).map(|d| d.devid).min();
        if first != Some(devid as u32) {
            return Ok(());
        }
        let r = self.with(|fs| fs.scrub())?;
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

    /// The device at `path` as a block device for the volume.
    fn open_device(
        &self,
        path: &str,
        ctx: &dyn FsIoctlContext,
    ) -> LxResult<Arc<dyn eclipsefs::BlockDevice>> {
        let inode = ctx.lookup(path)?;
        let backend = MountBackend::from_inode(inode).map_err(|_| LxError::ENOTBLK)?;
        if !matches!(backend, MountBackend::Block(_)) {
            return Err(LxError::ENOTBLK);
        }
        let size = super::block_mount::backend_size(&backend)?;
        let device = super::block_mount::device_from_backend(&backend)?;
        Ok(Arc::new(DevAdapter::new(device, size)))
    }

    /// The device `name` names in `btrfs device remove`/`replace`: an id,
    /// `missing`, or the path of a member.
    fn devid_of(&self, name: &str, ctx: &dyn FsIoctlContext) -> LxResult<u32> {
        if let Ok(id) = name.parse::<u32>() {
            return Ok(id);
        }
        if name == "missing" {
            return self
                .devices()
                .into_iter()
                .find(|d| !d.present)
                .map(|d| d.devid)
                .ok_or(LxError::EINVAL);
        }
        let dev = self.open_device(name, ctx)?;
        match eclipsefs::probe(&*dev) {
            Ok(p) if p.uuid == self.uuid() => Ok(p.devid),
            _ => Err(LxError::EINVAL),
        }
    }

    fn dev_replace(&self, buf: &mut [u8], ctx: &dyn FsIoctlContext) -> LxResult<()> {
        Self::admin(ctx)?;
        match get64(buf, 0) {
            // START: runs to the end before it returns.
            0 => {
                self.want_write()?;
                let src = match get64(buf, 16) {
                    0 => self.devid_of(cstr(buf, 32, 1025)?, ctx)?,
                    id => id as u32,
                };
                let new = self.open_device(cstr(buf, 1057, 1025)?, ctx)?;
                self.with(|fs| fs.replace_device(src, new))?;
                put64(buf, 8, 0);
            }
            // STATUS: none running (BTRFS_IOCTL_DEV_REPLACE_STATE_NEVER_STARTED).
            1 => {
                buf[16..64].fill(0);
                put64(buf, 8, 0);
            }
            // CANCEL: BTRFS_IOCTL_DEV_REPLACE_RESULT_NOT_STARTED.
            2 => put64(buf, 8, 1),
            _ => return Err(LxError::EINVAL),
        }
        Ok(())
    }

    fn balance(&self, buf: &mut [u8], ctx: &dyn FsIoctlContext) -> LxResult<()> {
        Self::admin(ctx)?;
        self.want_write()?;
        let flags = get64(buf, 0);
        if flags & BTRFS_BALANCE_RESUME != 0 {
            return Err(LxError::ENOTCONN);
        }
        let all = flags & (BTRFS_BALANCE_DATA | BTRFS_BALANCE_METADATA | BTRFS_BALANCE_SYSTEM) == 0;
        let convert = |kind: u64, at: usize| -> LxResult<Option<Profile>> {
            if (all || flags & kind != 0) && get64(buf, at + 64) & BTRFS_BALANCE_ARGS_CONVERT != 0 {
                profile(get64(buf, at + 56)).map(Some)
            } else {
                Ok(None)
            }
        };
        let data = convert(BTRFS_BALANCE_DATA, 16)?;
        let meta = convert(BTRFS_BALANCE_METADATA, 152)?;
        let report = self.with(|fs| fs.balance(data, meta))?;
        let done = report.relocated + report.freed;
        put64(buf, 8, 0);
        put64(buf, 424, done);
        put64(buf, 432, done);
        put64(buf, 440, done);
        Ok(())
    }

    /// `BTRFS_IOC_SEND` of this read-only snapshot into a regular file.
    fn send(&self, buf: &mut [u8], ctx: &dyn FsIoctlContext) -> LxResult<()> {
        Self::admin(ctx)?;
        if get64(buf, 32) & !BTRFS_SEND_FLAG_VERSION != 0 {
            return Err(LxError::EOPNOTSUPP);
        }
        let out = ctx.file(get64(buf, 0) as i64 as i32)?;
        if !out.flags().writable() {
            return Err(LxError::EBADF);
        }
        if out.metadata()?.type_ != rcore_fs::vfs::FileType::File {
            return Err(LxError::EINVAL);
        }
        let parent = match get64(buf, 24) {
            0 => None,
            id => Some(handle(id, FIRST_INO)),
        };
        let write_all = |data: &[u8]| -> LxResult<()> {
            let mut done = 0;
            while done < data.len() {
                match out.write(&data[done..])? {
                    0 => return Err(LxError::EIO),
                    n => done += n,
                }
            }
            Ok(())
        };
        if efs_of(&out.inode()).is_some_and(|(fs, _)| Arc::ptr_eq(&fs, &self.fs)) {
            // Into a file of this same filesystem, which is locked while
            // the stream is made: gather it first.
            let mut stream = Vec::new();
            self.with(|fs| {
                fs.send(self.ino, parent, &mut |d| {
                    stream.extend_from_slice(d);
                    Ok(())
                })
            })?;
            write_all(&stream)
        } else {
            let mut failed = None;
            self.with(|fs| {
                fs.send(self.ino, parent, &mut |d| {
                    write_all(d).map_err(|e| {
                        failed = Some(e);
                        EfsError::Io
                    })
                })
            })
            .map_err(|e| failed.unwrap_or(e))?;
            Ok(())
        }
    }

    /// `EFS_IOC_RECEIVE`: the stream in the file `fd`, from its position,
    /// becomes a read-only subvolume in this directory.
    fn receive(&self, buf: &mut [u8], ctx: &dyn FsIoctlContext) -> LxResult<()> {
        Self::admin(ctx)?;
        if buf[8..32].iter().any(|&b| b != 0) {
            return Err(LxError::EINVAL);
        }
        self.want_write()?;
        self.may_change_dir(ctx)?;
        let input = ctx.file(get64(buf, 0) as i64 as i32)?;
        if !input.flags().readable() {
            return Err(LxError::EBADF);
        }
        if input.metadata()?.type_ != rcore_fs::vfs::FileType::File {
            return Err(LxError::EINVAL);
        }
        let inode = input.inode();
        let start = input.seek(SeekFrom::Current(0))?;
        let mut pos = start;
        let read_exact = |pos: &mut u64, dst: &mut [u8]| -> eclipsefs::Result<()> {
            let mut done = 0;
            while done < dst.len() {
                match inode.read_at(*pos as usize, &mut dst[done..]) {
                    Ok(0) | Err(_) => return Err(EfsError::Corrupt("send stream cut short")),
                    Ok(n) => {
                        done += n;
                        *pos += n as u64;
                    }
                }
            }
            Ok(())
        };
        if efs_of(&inode).is_some_and(|(fs, _)| Arc::ptr_eq(&fs, &self.fs)) {
            // From a file of this same filesystem: read it all first.
            let size = inode.metadata()?.size as u64;
            let mut stream = vec![0u8; size.saturating_sub(start) as usize];
            read_exact(&mut pos, &mut stream).map_err(errno)?;
            let mut at = 0;
            self.with(|fs| {
                fs.receive(self.ino, &mut |dst| {
                    let src = stream
                        .get(at..at + dst.len())
                        .ok_or(EfsError::Corrupt("send stream cut short"))?;
                    dst.copy_from_slice(src);
                    at += dst.len();
                    Ok(())
                })
            })?;
            pos = start + at as u64;
        } else {
            self.with(|fs| fs.receive(self.ino, &mut |dst| read_exact(&mut pos, dst)))?;
        }
        input.seek(SeekFrom::Start(pos))?;
        self.fs.invalidate_dir(self.ino);
        Ok(())
    }
}

/// A balance `convert=` target (`BTRFS_BLOCK_GROUP_*` bits) as a profile.
fn profile(bits: u64) -> LxResult<Profile> {
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
        _ => return Err(LxError::EINVAL),
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
const COMPRESSION_XATTR: &str = "btrfs.compression";

/// The `*xattr` syscalls on a file of an EclipseFS mount (`None` for any
/// other filesystem). `path` is where the file is, for the read-only test
/// of its mount; `euid` and `admin` stand for the caller's credentials.
pub fn eclipsefs_xattr(
    inode: &Arc<dyn INode>,
    path: &str,
    req: XattrRequest<'_>,
    euid: u32,
    admin: bool,
) -> Option<LxResult<XattrReply>> {
    let (fs, ino) = efs_of(inode)?;
    Some(xattr(&fs, ino, path, req, euid, admin))
}

fn xattr(
    fs: &Arc<EclipseMountFs>,
    ino: u64,
    path: &str,
    req: XattrRequest<'_>,
    euid: u32,
    admin: bool,
) -> LxResult<XattrReply> {
    let mut efs = fs.inner.lock();
    let name = match req {
        XattrRequest::Get(n) | XattrRequest::Set { name: n, .. } | XattrRequest::Remove(n) => n,
        XattrRequest::List => "",
    };
    if name.starts_with("trusted.") && !admin {
        return Err(LxError::EPERM);
    }
    if !matches!(req, XattrRequest::Get(_) | XattrRequest::List) {
        if fs.read_only || super::path_is_read_only(path) {
            return Err(LxError::EROFS);
        }
        let st = efs.stat(ino).map_err(errno)?;
        if st.uid != euid && !admin {
            return Err(LxError::EPERM);
        }
        if name.starts_with("user.") && !matches!(st.kind, FileKind::Regular | FileKind::Dir) {
            return Err(LxError::EPERM);
        }
    }
    // A missing attribute is ENODATA, not ENOENT.
    let absent = |e: EfsError| match e {
        EfsError::NotFound => LxError::ENODATA,
        e => errno(e),
    };
    Ok(match req {
        XattrRequest::Get(COMPRESSION_XATTR) => match efs.compression(ino).map_err(errno)? {
            Some(algo) => XattrReply::Value(algo.name().as_bytes().to_vec()),
            None => return Err(LxError::ENODATA),
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
                        .ok_or(LxError::EINVAL)?,
                ),
            };
            efs.set_compression(ino, algo).map_err(errno)?;
            XattrReply::Done
        }
        XattrRequest::Remove(COMPRESSION_XATTR) => {
            efs.set_compression(ino, None).map_err(errno)?;
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
                Err(EfsError::Unsupported(_)) => Vec::new(),
                Err(e) => return Err(errno(e)),
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

/// `fallocate(2)` on a file of an EclipseFS mount, with the mode already
/// judged by the syscall layer: real preallocation, a punched hole that
/// gives its blocks back, a zeroed range (`None` for other filesystems).
pub fn eclipsefs_fallocate(file: &File, mode: u32, offset: u64, len: u64) -> Option<LxResult<()>> {
    let target = Target::of(file)?;
    Some((|| {
        target.want_write()?;
        target.with(|fs| fs.fallocate(target.ino, mode, offset, len))?;
        // PUNCH_HOLE and ZERO_RANGE: the range now reads as zeros.
        if mode & (0x02 | 0x10) != 0 {
            cache_reload(&file.inode(), offset as usize, len as usize);
        }
        Ok(())
    })())
}

/// `copy_file_range(2)` as a reflink, when both files are on the same
/// EclipseFS mount and the offsets are block-aligned (as btrfs does it).
/// `lseek(SEEK_DATA)` (3) or `lseek(SEEK_HOLE)` (4) on a file of an
/// EclipseFS mount: the new position, from the file's real extents.
/// `None` when `file` is on another filesystem.
pub fn eclipsefs_seek(file: &File, whence: usize, offset: i64) -> Option<LxResult<u64>> {
    const SEEK_DATA: usize = 3;
    let target = Target::of(file)?;
    Some((|| {
        let size = target.regular(target.ino)?.size;
        // Like `generic_file_llseek_size`: an offset at or past the end
        // (or negative, read as unsigned) is ENXIO.
        if offset < 0 || offset as u64 >= size {
            return Err(LxError::ENXIO);
        }
        cache_flush(&file.inode(), 0, size as usize);
        let off = offset as u64;
        let found = if whence == SEEK_DATA {
            target.with(|fs| fs.seek_data(target.ino, off))?
        } else {
            target.with(|fs| fs.seek_hole(target.ino, off))?
        };
        found.ok_or(LxError::ENXIO)
    })())
}

/// Copies at most `len` bytes and returns how many: whole blocks, or up to
/// the end of `src`. `None` when a reflink does not apply, and then the
/// caller copies the bytes.
pub fn eclipsefs_copy_reflink(
    src: &File,
    src_off: u64,
    dst: &File,
    dst_off: u64,
    len: u64,
) -> Option<LxResult<u64>> {
    let target = Target::of(dst)?;
    let src_ino = target.same_fs(src).ok()?;
    if len == 0 || dst.flags().is_append() || target.want_write().is_err() {
        return None;
    }
    let src_size = target.regular(src_ino).ok()?.size;
    target.regular(target.ino).ok()?;
    if src_off >= src_size || !src_off.is_multiple_of(BLOCK) || !dst_off.is_multiple_of(BLOCK) {
        return None;
    }
    let mut n = len.min(src_size - src_off);
    if src_off + n != src_size {
        n -= n % BLOCK;
    }
    if n == 0 || (src_ino == target.ino && src_off < dst_off + n && dst_off < src_off + n) {
        return None;
    }
    cache_flush(&src.inode(), src_off as usize, n as usize);
    let done = target
        .with(|fs| fs.clone_range(src_ino, src_off, target.ino, dst_off, n))
        .ok()?;
    cache_reload(&dst.inode(), dst_off as usize, done as usize);
    Some(Ok(done))
}

#[cfg(test)]
mod tests {
    //! The ioctls, xattrs, fallocate and reflink on RAM disks, through the
    //! same `Target` the syscall reaches, with the argument structs laid out
    //! byte for byte as btrfs-progs and coreutils lay them out.

    use super::super::eclipsefs_mount::testdisk::*;
    use super::super::eclipsefs_mount::{forget_block_device, register_block_device, EfsOptions};
    use super::super::OpenFlags;
    use super::*;
    use alloc::format;
    use alloc::string::ToString;
    use lock::Mutex;
    use rcore_fs::vfs::{FileSystem, FileType, FsError};
    use zcore_drivers::scheme::BlockScheme;

    /// The process side: its descriptors (by index), credentials and the
    /// device nodes it can name.
    struct Ctx {
        files: Mutex<Vec<Arc<File>>>,
        euid: u32,
        devices: Vec<(String, Arc<dyn INode>)>,
    }

    impl Ctx {
        fn root() -> Self {
            Self::as_user(0)
        }

        fn as_user(euid: u32) -> Self {
            Self {
                files: Mutex::new(Vec::new()),
                euid,
                devices: Vec::new(),
            }
        }

        /// Open `inode` and return its descriptor.
        fn open(&self, inode: Arc<dyn INode>, flags: OpenFlags) -> i32 {
            let mut files = self.files.lock();
            files.push(File::new(inode, flags, String::from("/efs-test/f")));
            files.len() as i32 - 1
        }

        fn fd(&self, fd: i32) -> Arc<File> {
            self.files.lock()[fd as usize].clone()
        }

        /// Run `cmd` on descriptor `fd` with `buf` as its argument.
        fn ioctl(&self, fd: i32, cmd: u32, buf: &mut [u8]) -> LxResult<usize> {
            let file = self.fd(fd);
            Target::of(&file)
                .expect("an EclipseFS file")
                .ioctl(cmd, buf, self)
        }
    }

    impl FsIoctlContext for Ctx {
        fn file(&self, fd: i32) -> LxResult<Arc<File>> {
            self.files
                .lock()
                .get(fd as usize)
                .cloned()
                .ok_or(LxError::EBADF)
        }
        fn lookup(&self, path: &str) -> LxResult<Arc<dyn INode>> {
            self.devices
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, i)| i.clone())
                .ok_or(LxError::ENOENT)
        }
        fn capable(&self, _cap: u32) -> bool {
            self.euid == 0
        }
        fn euid(&self) -> u32 {
            self.euid
        }
    }

    fn mount(disk: &Arc<RamDisk>) -> Arc<EclipseMountFs> {
        mount_with(disk, false, "")
    }

    fn mount_with(disk: &Arc<RamDisk>, read_only: bool, data: &str) -> Arc<EclipseMountFs> {
        let opts = EfsOptions::parse(data).unwrap();
        EclipseMountFs::open_with(&MountBackend::Block(disk.clone()), read_only, &opts).unwrap()
    }

    fn file_with(dir: &Arc<dyn INode>, name: &str, data: &[u8]) -> Arc<dyn INode> {
        let f = dir.create(name, FileType::File, 0o644).unwrap();
        if !data.is_empty() {
            assert_eq!(f.write_at(0, data).unwrap(), data.len());
        }
        f
    }

    fn read_all(f: &Arc<dyn INode>) -> Vec<u8> {
        let size = f.metadata().unwrap().size;
        let mut buf = vec![0u8; size];
        let mut done = 0;
        while done < size {
            done += f.read_at(done, &mut buf[done..]).unwrap();
        }
        buf
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// `struct btrfs_ioctl_vol_args`: an fd and a name.
    fn vol_args(fd: i64, name: &str) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[0..8].copy_from_slice(&fd.to_le_bytes());
        b[8..8 + name.len()].copy_from_slice(name.as_bytes());
        b
    }

    /// `struct btrfs_ioctl_vol_args_v2`.
    fn vol_args_v2(fd: i64, flags: u64, name: &str) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[0..8].copy_from_slice(&fd.to_le_bytes());
        b[16..24].copy_from_slice(&flags.to_le_bytes());
        b[56..56 + name.len()].copy_from_slice(name.as_bytes());
        b
    }

    fn used(fs: &Arc<EclipseMountFs>) -> u64 {
        fs.inner.lock().fsinfo().bytes_used
    }

    fn names(dir: &Arc<dyn INode>) -> Vec<String> {
        (2..)
            .map_while(|i| dir.get_entry(i).ok())
            .collect::<Vec<_>>()
    }

    #[test]
    fn the_ioctl_numbers_are_the_ones_linux_and_btrfs_progs_use() {
        const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
            dir << 30 | size << 16 | ty << 8 | nr
        }
        const W: u32 = 1;
        const R: u32 = 2;
        assert_eq!(FS_IOC_GETFLAGS, ioc(R, b'f' as u32, 1, 8));
        assert_eq!(FS_IOC_SETFLAGS, ioc(W, b'f' as u32, 2, 8));
        assert_eq!(FICLONE, ioc(W, 0x94, 9, 4));
        assert_eq!(FICLONERANGE, ioc(W, 0x94, 13, 32));
        assert_eq!(FIDEDUPERANGE, ioc(R | W, 0x94, 54, 24));
        assert_eq!(FS_IOC_FIEMAP, ioc(R | W, b'f' as u32, 11, 32));
        assert_eq!(BTRFS_IOC_DEFRAG, ioc(W, 0x94, 2, 4096));
        assert_eq!(BTRFS_IOC_RESIZE, ioc(W, 0x94, 3, 4096));
        assert_eq!(BTRFS_IOC_DEFRAG_RANGE, ioc(W, 0x94, 16, 48));
        assert_eq!(BTRFS_IOC_SNAP_CREATE_V2, ioc(W, 0x94, 23, 4096));
        assert_eq!(BTRFS_IOC_SUBVOL_CREATE_V2, ioc(W, 0x94, 24, 4096));
        assert_eq!(BTRFS_IOC_START_SYNC, ioc(R, 0x94, 24, 8));
        assert_eq!(BTRFS_IOC_SCRUB, ioc(R | W, 0x94, 27, 1024));
        assert_eq!(BTRFS_IOC_DEV_INFO, ioc(R | W, 0x94, 30, 4096));
        assert_eq!(BTRFS_IOC_FS_INFO, ioc(R, 0x94, 31, 1024));
        assert_eq!(BTRFS_IOC_BALANCE_V2, ioc(R | W, 0x94, 32, 1024));
        assert_eq!(BTRFS_IOC_SEND, ioc(W, 0x94, 38, 72));
        assert_eq!(BTRFS_IOC_QGROUP_LIMIT, ioc(R, 0x94, 43, 48));
        assert_eq!(BTRFS_IOC_DEV_REPLACE, ioc(R | W, 0x94, 53, 2600));
        assert_eq!(BTRFS_IOC_GET_SUBVOL_INFO, ioc(R, 0x94, 60, 504));
        assert_eq!(BTRFS_IOC_SNAP_DESTROY_V2, ioc(W, 0x94, 63, 4096));
        assert_eq!(EFS_IOC_RECEIVE, ioc(W, 0x94, 0xf0, 32));
        assert_eq!(EFS_IOC_SET_TIER, ioc(W, 0x94, 0xf1, 32));
        assert_eq!(EFS_IOC_GET_TIER, ioc(R | W, 0x94, 0xf2, 32));
        assert_eq!(EFS_IOC_TIER_MIGRATE, ioc(R | W, 0x94, 0xf3, 32));
        assert!(is_fs_ioctl(FICLONE));
        assert!(!is_fs_ioctl(0x5401), "TCGETS is not ours");
    }

    #[test]
    fn an_ioctl_this_module_does_not_know_is_enotty() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let ctx = Ctx::root();
        let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        assert_eq!(ctx.ioctl(fd, 0x5401, &mut [0u8; 8]), Err(LxError::ENOTTY));
    }

    #[test]
    fn chattr_sets_and_lsattr_reads_the_flags() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let f = file_with(&root, "f", b"");
        let ctx = Ctx::root();
        let fd = ctx.open(f.clone(), OpenFlags::RDONLY);
        let mut flags = [0u8; 4];
        ctx.ioctl(fd, FS_IOC_GETFLAGS, &mut flags).unwrap();
        assert_eq!(u32::from_le_bytes(flags), 0);
        // chattr +C (no-COW), through the 32-bit number too.
        let nocow = 0x0080_0000u32;
        ctx.ioctl(fd, FS_IOC32_SETFLAGS, &mut nocow.to_le_bytes())
            .unwrap();
        ctx.ioctl(fd, FS_IOC_GETFLAGS, &mut flags).unwrap();
        assert_eq!(u32::from_le_bytes(flags), nocow);
        // chattr +i: the file can no longer be written.
        ctx.ioctl(
            fd,
            FS_IOC_SETFLAGS,
            &mut (nocow | FS_IMMUTABLE_FL).to_le_bytes(),
        )
        .unwrap();
        assert!(f.write_at(0, b"x").is_err());
    }

    #[test]
    fn immutable_and_append_only_need_cap_linux_immutable_even_for_the_owner() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let f = file_with(&fs.root_inode(), "f", b"");
        let mut md = f.metadata().unwrap();
        md.uid = 1000;
        f.set_metadata(&md).unwrap();
        let user = Ctx::as_user(1000);
        let fd = user.open(f.clone(), OpenFlags::RDONLY);
        for flag in [FS_IMMUTABLE_FL, FS_APPEND_FL] {
            assert_eq!(
                user.ioctl(fd, FS_IOC_SETFLAGS, &mut flag.to_le_bytes()),
                Err(LxError::EPERM)
            );
        }
        // Other flags are the owner's to change; a stranger's are not.
        user.ioctl(fd, FS_IOC_SETFLAGS, &mut 0x0080_0000u32.to_le_bytes())
            .unwrap();
        let stranger = Ctx::as_user(1001);
        let fd = stranger.open(f, OpenFlags::RDONLY);
        assert_eq!(
            stranger.ioctl(fd, FS_IOC_SETFLAGS, &mut 0u32.to_le_bytes()),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn ficlone_shares_the_blocks_instead_of_copying_them() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let data = pattern(1 << 20, 7);
        let src = file_with(&root, "src", &data);
        let dst = file_with(&root, "dst", b"");
        fs.sync().unwrap();
        let before = used(&fs);
        let ctx = Ctx::root();
        let s = ctx.open(src, OpenFlags::RDONLY);
        let d = ctx.open(dst.clone(), OpenFlags::RDWR);
        ctx.ioctl(d, FICLONE, &mut s.to_le_bytes()).unwrap();
        assert_eq!(read_all(&dst), data);
        fs.sync().unwrap();
        assert!(
            used(&fs) < before + (64 << 10),
            "1 MiB cloned, {} bytes more in use",
            used(&fs) - before
        );
    }

    #[test]
    fn ficlone_wants_a_writable_destination_and_a_readable_source() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let src = file_with(&root, "src", b"abc");
        let dst = file_with(&root, "dst", b"");
        let ctx = Ctx::root();
        let s = ctx.open(src.clone(), OpenFlags::RDONLY);
        let d_ro = ctx.open(dst.clone(), OpenFlags::RDONLY);
        let d_append = ctx.open(dst.clone(), OpenFlags::RDWR | OpenFlags::APPEND);
        let s_wo = ctx.open(src, OpenFlags::WRONLY);
        let d = ctx.open(dst, OpenFlags::WRONLY);
        assert_eq!(
            ctx.ioctl(d_ro, FICLONE, &mut s.to_le_bytes()),
            Err(LxError::EBADF)
        );
        assert_eq!(
            ctx.ioctl(d_append, FICLONE, &mut s.to_le_bytes()),
            Err(LxError::EBADF)
        );
        assert_eq!(
            ctx.ioctl(d, FICLONE, &mut s_wo.to_le_bytes()),
            Err(LxError::EBADF)
        );
        assert_eq!(
            ctx.ioctl(d, FICLONE, &mut 99i32.to_le_bytes()),
            Err(LxError::EBADF)
        );
        // A directory cannot be cloned.
        let dir = ctx.open(root, OpenFlags::RDONLY);
        assert_eq!(
            ctx.ioctl(d, FICLONE, &mut dir.to_le_bytes()),
            Err(LxError::EISDIR)
        );
        // Nor a file onto itself.
        let both = ctx.open(src_named(&fs, "src"), OpenFlags::RDWR);
        assert_eq!(
            ctx.ioctl(both, FICLONE, &mut both.to_le_bytes()),
            Err(LxError::EINVAL)
        );
    }

    fn src_named(fs: &Arc<EclipseMountFs>, name: &str) -> Arc<dyn INode> {
        fs.root_inode().find(name).unwrap()
    }

    #[test]
    fn ficlone_across_two_filesystems_is_exdev() {
        let a = mount(&formatted(16 << 20));
        let b = mount(&formatted(16 << 20));
        let ctx = Ctx::root();
        let s = ctx.open(file_with(&a.root_inode(), "s", b"x"), OpenFlags::RDONLY);
        let d = ctx.open(file_with(&b.root_inode(), "d", b""), OpenFlags::RDWR);
        assert_eq!(
            ctx.ioctl(d, FICLONE, &mut s.to_le_bytes()),
            Err(LxError::EXDEV)
        );
    }

    #[test]
    fn ficlonerange_clones_an_aligned_range_into_the_middle() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let src_data = pattern(64 << 10, 1);
        let dst_data = pattern(64 << 10, 2);
        let ctx = Ctx::root();
        let s = ctx.open(file_with(&root, "s", &src_data), OpenFlags::RDONLY);
        let dst = file_with(&root, "d", &dst_data);
        let d = ctx.open(dst.clone(), OpenFlags::RDWR);
        // struct file_clone_range: src_fd, src_offset, src_length, dest_offset.
        let mut arg = [0u8; 32];
        arg[0..8].copy_from_slice(&(s as i64).to_le_bytes());
        arg[8..16].copy_from_slice(&8192u64.to_le_bytes());
        arg[16..24].copy_from_slice(&16384u64.to_le_bytes());
        arg[24..32].copy_from_slice(&4096u64.to_le_bytes());
        ctx.ioctl(d, FICLONERANGE, &mut arg).unwrap();
        let mut want = dst_data.clone();
        want[4096..20480].copy_from_slice(&src_data[8192..24576]);
        assert_eq!(read_all(&dst), want);
        // An offset off a block boundary is refused.
        arg[8..16].copy_from_slice(&100u64.to_le_bytes());
        assert_eq!(ctx.ioctl(d, FICLONERANGE, &mut arg), Err(LxError::EINVAL));
    }

    /// `struct file_dedupe_range` with one `file_dedupe_range_info` per fd.
    fn dedupe_arg(src_off: u64, len: u64, dests: &[(i32, u64)]) -> Vec<u8> {
        let mut b = vec![0u8; 24 + 32 * dests.len()];
        b[0..8].copy_from_slice(&src_off.to_le_bytes());
        b[8..16].copy_from_slice(&len.to_le_bytes());
        b[16..18].copy_from_slice(&(dests.len() as u16).to_le_bytes());
        for (i, (fd, off)) in dests.iter().enumerate() {
            let at = 24 + 32 * i;
            b[at..at + 8].copy_from_slice(&(*fd as i64).to_le_bytes());
            b[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
        }
        b
    }

    fn dedupe_result(b: &[u8], i: usize) -> (u64, i32) {
        let at = 24 + 32 * i;
        (get64(b, at + 16), get32(b, at + 24) as i32)
    }

    #[test]
    fn fideduperange_reports_same_differs_and_a_bad_fd_per_destination() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let data = pattern(256 << 10, 3);
        let ctx = Ctx::root();
        let s = ctx.open(file_with(&root, "s", &data), OpenFlags::RDONLY);
        let same = ctx.open(file_with(&root, "same", &data), OpenFlags::RDWR);
        let other = ctx.open(
            file_with(&root, "other", &pattern(256 << 10, 4)),
            OpenFlags::RDWR,
        );
        fs.sync().unwrap();
        let before = used(&fs);
        let mut arg = dedupe_arg(0, 256 << 10, &[(same, 0), (other, 0), (77, 0)]);
        ctx.ioctl(s, FIDEDUPERANGE, &mut arg).unwrap();
        assert_eq!(dedupe_result(&arg, 0), (256 << 10, FILE_DEDUPE_RANGE_SAME));
        assert_eq!(dedupe_result(&arg, 1), (0, FILE_DEDUPE_RANGE_DIFFERS));
        assert_eq!(dedupe_result(&arg, 2), (0, -(LxError::EBADF as i32)));
        fs.sync().unwrap();
        assert!(used(&fs) < before, "the duplicate's blocks were given back");
        assert_eq!(read_all(&src_named(&fs, "same")), data);
        // The reserved fields must be zero.
        let mut bad = dedupe_arg(0, 4096, &[(same, 0)]);
        bad[18] = 1;
        assert_eq!(ctx.ioctl(s, FIDEDUPERANGE, &mut bad), Err(LxError::EINVAL));
    }

    #[test]
    fn subvolumes_and_snapshots_are_made_listed_and_deleted_by_ioctl() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "home"))
            .unwrap();
        let home = root.find("home").unwrap();
        file_with(&home, "notes", b"before");
        let h = ctx.open(home.clone(), OpenFlags::RDONLY);
        ctx.ioctl(
            top,
            BTRFS_IOC_SNAP_CREATE_V2,
            &mut vol_args_v2(h as i64, BTRFS_SUBVOL_RDONLY, "home-snap"),
        )
        .unwrap();
        // The directory listing sees both without a remount.
        let mut listed = names(&root);
        listed.sort();
        assert_eq!(listed, ["home", "home-snap"]);
        home.find("notes").unwrap().write_at(0, b"after!").unwrap();
        let snap = root.find("home-snap").unwrap();
        assert_eq!(read_all(&snap.find("notes").unwrap()), b"before");
        let sn = ctx.open(snap.clone(), OpenFlags::RDONLY);
        let mut flags = [0u8; 8];
        ctx.ioctl(sn, BTRFS_IOC_SUBVOL_GETFLAGS, &mut flags)
            .unwrap();
        assert_eq!(u64::from_le_bytes(flags), BTRFS_SUBVOL_RDONLY);
        assert!(snap.create("new", FileType::File, 0o644).is_err());
        ctx.ioctl(sn, BTRFS_IOC_SUBVOL_SETFLAGS, &mut 0u64.to_le_bytes())
            .unwrap();
        snap.create("new", FileType::File, 0o644).unwrap();
        ctx.ioctl(
            top,
            BTRFS_IOC_SNAP_DESTROY_V2,
            &mut vol_args_v2(0, 0, "home-snap"),
        )
        .unwrap();
        assert_eq!(names(&root), ["home"]);
        assert!(root.find("home-snap").is_err());
    }

    #[test]
    fn subvolume_names_and_permissions_are_checked() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        for bad in ["", ".", "..", "a/b"] {
            assert_eq!(
                ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, bad)),
                Err(LxError::EINVAL),
                "{:?}",
                bad
            );
        }
        let mut unterminated = vec![b'x'; 4096];
        unterminated[0..8].fill(0);
        assert_eq!(
            ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut unterminated),
            Err(LxError::ENAMETOOLONG)
        );
        // Creating needs write access to the directory; deleting, CAP_SYS_ADMIN.
        let user = Ctx::as_user(1000);
        let utop = user.open(root.clone(), OpenFlags::RDONLY);
        assert_eq!(
            user.ioctl(utop, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "mine")),
            Err(LxError::EACCES)
        );
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "sv"))
            .unwrap();
        assert_eq!(
            user.ioctl(utop, BTRFS_IOC_SNAP_DESTROY, &mut vol_args(0, "sv")),
            Err(LxError::EPERM)
        );
        // A file is not a directory to make a subvolume in.
        let f = ctx.open(file_with(&root, "f", b""), OpenFlags::RDONLY);
        assert_eq!(
            ctx.ioctl(f, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "x")),
            Err(LxError::ENOTDIR)
        );
        // Only a subvolume's root directory has subvolume flags.
        let d = root.create("plain", FileType::Dir, 0o755).unwrap();
        let d = ctx.open(d, OpenFlags::RDONLY);
        assert_eq!(
            ctx.ioctl(d, BTRFS_IOC_SUBVOL_GETFLAGS, &mut [0u8; 8]),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn get_subvol_info_and_ino_lookup_describe_where_a_file_is() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        ctx.ioctl(
            top,
            BTRFS_IOC_SUBVOL_CREATE_V2,
            &mut vol_args_v2(0, 0, "sv"),
        )
        .unwrap();
        let sv = root.find("sv").unwrap();
        let deep = sv
            .create("a", FileType::Dir, 0o755)
            .unwrap()
            .create("b", FileType::Dir, 0o755)
            .unwrap();
        let fd = ctx.open(sv.clone(), OpenFlags::RDONLY);
        let mut info = vec![0u8; 504];
        ctx.ioctl(fd, BTRFS_IOC_GET_SUBVOL_INFO, &mut info).unwrap();
        let id = get64(&info, 0);
        assert!(id > FS_TREE);
        assert_eq!(cstr(&info, 8, 256).unwrap(), "sv");
        assert_eq!(get64(&info, 264), FS_TREE, "its parent is the top");
        assert_eq!(get64(&info, 272), FIRST_INO, "named in the top's root dir");
        assert_eq!(get64(&info, 288), 0, "writable");
        // INO_LOOKUP: the path of `a/b` inside the subvolume.
        let mut arg = vec![0u8; 4096];
        arg[8..16].copy_from_slice(&split(deep.metadata().unwrap().inode as u64).1.to_le_bytes());
        ctx.ioctl(fd, BTRFS_IOC_INO_LOOKUP, &mut arg).unwrap();
        assert_eq!(get64(&arg, 0), id);
        assert_eq!(cstr(&arg, 16, 4080).unwrap(), "a/b/");
        // Its own root, which any user may ask about.
        let user = Ctx::as_user(1000);
        let ufd = user.open(sv, OpenFlags::RDONLY);
        let mut arg = vec![0u8; 4096];
        arg[8..16].copy_from_slice(&FIRST_INO.to_le_bytes());
        user.ioctl(ufd, BTRFS_IOC_INO_LOOKUP, &mut arg).unwrap();
        assert_eq!((get64(&arg, 0), cstr(&arg, 16, 4080).unwrap()), (id, ""));
    }

    #[test]
    fn the_default_subvolume_is_what_the_next_mount_shows() {
        let disk = formatted(16 << 20);
        {
            let fs = mount(&disk);
            let root = fs.root_inode();
            let ctx = Ctx::root();
            let top = ctx.open(root.clone(), OpenFlags::RDONLY);
            ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "rootfs"))
                .unwrap();
            let sv = root.find("rootfs").unwrap();
            file_with(&sv, "etc-hostname", b"eclipse");
            let fd = ctx.open(sv, OpenFlags::RDONLY);
            let mut info = vec![0u8; 504];
            ctx.ioctl(fd, BTRFS_IOC_GET_SUBVOL_INFO, &mut info).unwrap();
            let user = Ctx::as_user(5);
            let utop = user.open(root.clone(), OpenFlags::RDONLY);
            assert_eq!(
                user.ioctl(
                    utop,
                    BTRFS_IOC_DEFAULT_SUBVOL,
                    &mut get64(&info, 0).to_le_bytes()
                ),
                Err(LxError::EPERM)
            );
            ctx.ioctl(
                top,
                BTRFS_IOC_DEFAULT_SUBVOL,
                &mut get64(&info, 0).to_le_bytes(),
            )
            .unwrap();
            ctx.ioctl(top, BTRFS_IOC_SYNC, &mut []).unwrap();
        }
        let fs = mount(&disk);
        assert_eq!(
            read_all(&fs.root_inode().find("etc-hostname").unwrap()),
            b"eclipse"
        );
        // subvolid=5 still reaches the top.
        let top = mount_with(&disk, false, "subvolid=5");
        assert!(top.root_inode().find("rootfs").is_ok());
    }

    #[test]
    fn fs_info_and_dev_info_describe_a_single_device() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let ctx = Ctx::root();
        let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        let mut info = vec![0u8; 1024];
        put64(
            &mut info,
            48,
            BTRFS_FS_INFO_FLAG_GENERATION | BTRFS_FS_INFO_FLAG_CSUM_INFO,
        );
        ctx.ioctl(fd, BTRFS_IOC_FS_INFO, &mut info).unwrap();
        assert_eq!((get64(&info, 0), get64(&info, 8)), (1, 1));
        assert_eq!(&info[16..32], &[9u8; 16]);
        assert_eq!(get32(&info, 32), 4096);
        assert_eq!(get16(&info, 46), 4, "CRC32C");
        assert_eq!(
            get64(&info, 48),
            BTRFS_FS_INFO_FLAG_GENERATION | BTRFS_FS_INFO_FLAG_CSUM_INFO
        );
        assert_eq!(get64(&info, 56), fs.inner.lock().generation());
        let mut dev = vec![0u8; 4096];
        put64(&mut dev, 0, 1);
        ctx.ioctl(fd, BTRFS_IOC_DEV_INFO, &mut dev).unwrap();
        assert_eq!(get64(&dev, 32), 16 << 20);
        assert_eq!(&dev[40..56], &[9u8; 16]);
        put64(&mut dev, 0, 2);
        assert_eq!(
            ctx.ioctl(fd, BTRFS_IOC_DEV_INFO, &mut dev),
            Err(LxError::ENODEV)
        );
        // START_SYNC hands back the generation it committed.
        file_with(&fs.root_inode(), "x", b"x");
        let mut gen = [0u8; 8];
        ctx.ioctl(fd, BTRFS_IOC_START_SYNC, &mut gen).unwrap();
        assert_eq!(u64::from_le_bytes(gen), fs.inner.lock().generation());
    }

    #[test]
    fn scrub_reads_everything_on_the_first_device_and_needs_cap_sys_admin() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        file_with(&fs.root_inode(), "data", &pattern(200 << 10, 9));
        let ctx = Ctx::root();
        let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        let mut arg = vec![0u8; 1024];
        put64(&mut arg, 0, 1);
        ctx.ioctl(fd, BTRFS_IOC_SCRUB, &mut arg).unwrap();
        assert!(get64(&arg, 32) >= 50, "data blocks: {}", get64(&arg, 32));
        assert!(get64(&arg, 40) > 0, "tree nodes");
        assert_eq!(get64(&arg, 72) + get64(&arg, 80) + get64(&arg, 120), 0);
        put64(&mut arg, 0, 3);
        assert_eq!(
            ctx.ioctl(fd, BTRFS_IOC_SCRUB, &mut arg),
            Err(LxError::ENODEV)
        );
        let user = Ctx::as_user(1000);
        let ufd = user.open(fs.root_inode(), OpenFlags::RDONLY);
        put64(&mut arg, 0, 1);
        assert_eq!(
            user.ioctl(ufd, BTRFS_IOC_SCRUB, &mut arg),
            Err(LxError::EPERM)
        );
        assert_eq!(
            ctx.ioctl(fd, BTRFS_IOC_SCRUB_PROGRESS, &mut arg),
            Err(LxError::ENOTCONN)
        );
    }

    #[test]
    fn a_qgroup_limit_set_by_ioctl_stops_writes_past_it() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "q"))
            .unwrap();
        let q = root.find("q").unwrap();
        let qfd = ctx.open(q.clone(), OpenFlags::RDONLY);
        let mut ctl = [0u8; 16];
        put64(&mut ctl, 0, 1);
        ctx.ioctl(top, BTRFS_IOC_QUOTA_CTL, &mut ctl).unwrap();
        // qgroupid 0: the subvolume the fd is in.
        let mut lim = [0u8; 48];
        put64(&mut lim, 8, BTRFS_QGROUP_LIMIT_MAX_RFER);
        put64(&mut lim, 16, 256 << 10);
        ctx.ioctl(qfd, BTRFS_IOC_QGROUP_LIMIT, &mut lim).unwrap();
        let f = file_with(&q, "big", b"");
        let chunk = pattern(64 << 10, 5);
        let mut written = 0;
        let refused = loop {
            match f.write_at(written, &chunk) {
                Ok(n) if n > 0 && written < (1 << 20) => written += n,
                Ok(_) => break None,
                Err(e) => break Some(e),
            }
        };
        assert!(
            matches!(refused, Some(FsError::NoDeviceSpace)),
            "{:?}",
            refused
        );
        assert!(written <= 256 << 10);
        // `limit none`.
        put64(&mut lim, 16, u64::MAX);
        ctx.ioctl(qfd, BTRFS_IOC_QGROUP_LIMIT, &mut lim).unwrap();
        f.write_at(written, &chunk).unwrap();
        // Higher-level qgroups and exclusive limits are not kept.
        put64(&mut lim, 0, 1 << 48 | 5);
        assert_eq!(
            ctx.ioctl(qfd, BTRFS_IOC_QGROUP_LIMIT, &mut lim),
            Err(LxError::EOPNOTSUPP)
        );
        put64(&mut ctl, 0, 9);
        assert_eq!(
            ctx.ioctl(top, BTRFS_IOC_QUOTA_CTL, &mut ctl),
            Err(LxError::EINVAL)
        );
        put64(&mut ctl, 0, 2);
        ctx.ioctl(top, BTRFS_IOC_QUOTA_CTL, &mut ctl).unwrap();
        assert!(!fs.inner.lock().quota_enabled());
    }

    #[test]
    fn a_read_only_mount_refuses_every_change_but_answers_questions() {
        let disk = formatted(16 << 20);
        {
            let fs = mount(&disk);
            file_with(&fs.root_inode(), "f", b"x");
            fs.sync().unwrap();
        }
        let fs = mount_with(&disk, true, "");
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        let f = ctx.open(root.find("f").unwrap(), OpenFlags::RDONLY);
        let mut flags = [0u8; 4];
        ctx.ioctl(f, FS_IOC_GETFLAGS, &mut flags).unwrap();
        assert_eq!(
            ctx.ioctl(f, FS_IOC_SETFLAGS, &mut 0u32.to_le_bytes()),
            Err(LxError::EROFS)
        );
        assert_eq!(
            ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "x")),
            Err(LxError::EROFS)
        );
        let mut ctl = [0u8; 16];
        put64(&mut ctl, 0, 1);
        assert_eq!(
            ctx.ioctl(top, BTRFS_IOC_QUOTA_CTL, &mut ctl),
            Err(LxError::EROFS)
        );
        let inode = root.find("f").unwrap();
        assert_eq!(
            eclipsefs_xattr(
                &inode,
                "/efs-test/f",
                XattrRequest::Set {
                    name: "user.a",
                    value: b"1",
                    flags: 0
                },
                0,
                true
            ),
            Some(Err(LxError::EROFS))
        );
    }

    fn xattr_as(inode: &Arc<dyn INode>, req: XattrRequest<'_>, euid: u32) -> LxResult<XattrReply> {
        eclipsefs_xattr(inode, "/efs-test/f", req, euid, euid == 0).unwrap()
    }

    #[test]
    fn extended_attributes_round_trip_through_the_syscall_layer() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let f = file_with(&fs.root_inode(), "f", b"");
        let set = |name, value: &'static [u8], flags| XattrRequest::Set { name, value, flags };
        assert_eq!(
            xattr_as(&f, set("user.mime", b"text/plain", 0), 0),
            Ok(XattrReply::Done)
        );
        assert_eq!(
            xattr_as(&f, XattrRequest::Get("user.mime"), 0),
            Ok(XattrReply::Value(b"text/plain".to_vec()))
        );
        assert_eq!(
            xattr_as(&f, set("trusted.t", b"1", 0), 0),
            Ok(XattrReply::Done)
        );
        assert_eq!(
            xattr_as(&f, XattrRequest::List, 0),
            Ok(XattrReply::Names(b"trusted.t\0user.mime\0".to_vec()))
        );
        // A user sees no trusted.* name and may not read one.
        assert_eq!(
            xattr_as(&f, XattrRequest::List, 1000),
            Ok(XattrReply::Names(b"user.mime\0".to_vec()))
        );
        assert_eq!(
            xattr_as(&f, XattrRequest::Get("trusted.t"), 1000),
            Err(LxError::EPERM)
        );
        // Nor change another's attributes.
        assert_eq!(
            xattr_as(&f, set("user.x", b"1", 0), 1000),
            Err(LxError::EPERM)
        );
        // XATTR_CREATE on an existing one, XATTR_REPLACE on a missing one.
        assert_eq!(
            xattr_as(&f, set("user.mime", b"x", 1), 0),
            Err(LxError::EEXIST)
        );
        assert_eq!(
            xattr_as(&f, set("user.none", b"x", 2), 0),
            Err(LxError::ENODATA)
        );
        assert_eq!(
            xattr_as(&f, XattrRequest::Get("user.none"), 0),
            Err(LxError::ENODATA)
        );
        assert_eq!(
            xattr_as(&f, XattrRequest::Remove("user.mime"), 0),
            Ok(XattrReply::Done)
        );
        assert_eq!(
            xattr_as(&f, XattrRequest::Remove("user.mime"), 0),
            Err(LxError::ENODATA)
        );
        // user.* only on files and directories.
        let link = fs
            .root_inode()
            .create("l", FileType::SymLink, 0o777)
            .unwrap();
        link.write_at(0, b"f").unwrap();
        assert_eq!(
            xattr_as(&link, set("user.x", b"1", 0), 0),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn the_btrfs_compression_property_is_the_files_compression() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let f = file_with(&fs.root_inode(), "f", b"");
        assert_eq!(
            xattr_as(&f, XattrRequest::Get(COMPRESSION_XATTR), 0),
            Err(LxError::ENODATA)
        );
        let set = |value: &'static [u8]| XattrRequest::Set {
            name: COMPRESSION_XATTR,
            value,
            flags: 0,
        };
        xattr_as(&f, set(b"zstd"), 0).unwrap();
        assert_eq!(
            xattr_as(&f, XattrRequest::Get(COMPRESSION_XATTR), 0),
            Ok(XattrReply::Value(b"zstd".to_vec()))
        );
        assert_eq!(xattr_as(&f, set(b"brotli"), 0), Err(LxError::EINVAL));
        xattr_as(&f, set(b""), 0).unwrap();
        assert_eq!(
            xattr_as(&f, XattrRequest::Get(COMPRESSION_XATTR), 0),
            Err(LxError::ENODATA)
        );
    }

    #[test]
    fn a_punched_hole_reads_as_zeros_and_gives_its_blocks_back() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let data = pattern(1 << 20, 11);
        let inode = file_with(&fs.root_inode(), "f", &data);
        fs.sync().unwrap();
        let before = used(&fs);
        let file = File::new(inode.clone(), OpenFlags::RDWR, String::from("/efs-test/f"));
        // FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE.
        eclipsefs_fallocate(&file, 0x3, 4096, 512 << 10)
            .unwrap()
            .unwrap();
        let mut want = data;
        want[4096..4096 + (512 << 10)].fill(0);
        assert_eq!(read_all(&inode), want);
        fs.sync().unwrap();
        assert!(used(&fs) + (400 << 10) < before);
        // Plain preallocation grows the file; KEEP_SIZE does not.
        eclipsefs_fallocate(&file, 0, 1 << 20, 1 << 20)
            .unwrap()
            .unwrap();
        assert_eq!(inode.metadata().unwrap().size, 2 << 20);
        eclipsefs_fallocate(&file, 0x1, 2 << 20, 1 << 20)
            .unwrap()
            .unwrap();
        assert_eq!(inode.metadata().unwrap().size, 2 << 20);
    }

    /// `struct fiemap` asking for `count` extents of `start..start+len`.
    fn fiemap_args(start: u64, len: u64, flags: u32, count: u32) -> Vec<u8> {
        let mut b = vec![0u8; FIEMAP_HEADER + FIEMAP_EXTENT * count as usize];
        put64(&mut b, 0, start);
        put64(&mut b, 8, len);
        put32(&mut b, 16, flags);
        put32(&mut b, 24, count);
        b
    }

    /// The extents a FIEMAP answer holds, as (logical, length, flags).
    fn fiemap_extents(b: &[u8]) -> Vec<(u64, u64, u32)> {
        (0..get32(b, 20) as usize)
            .map(|i| {
                let e = &b[FIEMAP_HEADER + i * FIEMAP_EXTENT..];
                (get64(e, 0), get64(e, 16), get32(e, 40))
            })
            .collect()
    }

    /// data [0,256K) | hole | data [512K,768K), size 1 MiB.
    fn sparse_file(fs: &Arc<EclipseMountFs>) -> Arc<dyn INode> {
        let f = file_with(&fs.root_inode(), "sparse", &pattern(256 << 10, 1));
        assert_eq!(
            f.write_at(512 << 10, &pattern(256 << 10, 2)).unwrap(),
            256 << 10
        );
        f.resize(1 << 20).unwrap();
        fs.sync().unwrap();
        f
    }

    /// Two files written a block at a time, in turns and in separate
    /// transactions, so each one's blocks alternate with the other's.
    fn fragmented_pair(fs: &Arc<EclipseMountFs>, blocks: usize) -> [Arc<dyn INode>; 2] {
        let root = fs.root_inode();
        let a = file_with(&root, "a", b"");
        let b = file_with(&root, "b", b"");
        for i in 0..blocks {
            let at = i * BLOCK as usize;
            a.write_at(at, &pattern(BLOCK as usize, i as u8)).unwrap();
            b.write_at(at, &pattern(BLOCK as usize, i as u8 ^ 0x80))
                .unwrap();
            fs.sync().unwrap();
        }
        [a, b]
    }

    fn defrag_range_args(start: u64, len: u64, flags: u64, thresh: u32, algo: u32) -> Vec<u8> {
        let mut b = vec![0u8; 48];
        put64(&mut b, 0, start);
        put64(&mut b, 8, len);
        put64(&mut b, 16, flags);
        put32(&mut b, 24, thresh);
        put32(&mut b, 28, algo);
        b
    }

    fn extent_count(ctx: &Ctx, fd: i32) -> usize {
        let mut b = fiemap_args(0, u64::MAX, 0, 64);
        ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b).unwrap();
        fiemap_extents(&b).len()
    }

    #[test]
    fn defrag_makes_a_fragmented_file_one_extent() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let [a, _] = fragmented_pair(&fs, 16);
        let data = read_all(&a);
        let ctx = Ctx::root();
        let fd = ctx.open(a.clone(), OpenFlags::RDWR);
        assert_eq!(extent_count(&ctx, fd), 16);
        ctx.ioctl(fd, BTRFS_IOC_DEFRAG, &mut []).unwrap();
        assert_eq!(extent_count(&ctx, fd), 1);
        assert_eq!(read_all(&a), data);
        fs.inner.lock().check().unwrap();
    }

    #[test]
    fn defrag_range_compresses_and_honours_the_range() {
        const ENCODED: u32 = 0x8;
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let [a, b] = fragmented_pair(&fs, 16);
        let ctx = Ctx::root();
        let fa = ctx.open(a.clone(), OpenFlags::RDWR);
        // Blocks 4..8 only: three runs remain around the new one.
        let mut r = defrag_range_args(4 * BLOCK, 4 * BLOCK, 0, 0, 0);
        ctx.ioctl(fa, BTRFS_IOC_DEFRAG_RANGE, &mut r).unwrap();
        assert_eq!(extent_count(&ctx, fa), 4 + 1 + 8);
        // `-c zstd` over the whole file (len 0 runs to the end).
        let fb = ctx.open(b.clone(), OpenFlags::RDWR);
        let data = read_all(&b);
        let mut r = defrag_range_args(
            0,
            0,
            BTRFS_DEFRAG_RANGE_COMPRESS | BTRFS_DEFRAG_RANGE_START_IO,
            0,
            3,
        );
        ctx.ioctl(fb, BTRFS_IOC_DEFRAG_RANGE, &mut r).unwrap();
        let mut m = fiemap_args(0, u64::MAX, 0, 64);
        ctx.ioctl(fb, FS_IOC_FIEMAP, &mut m).unwrap();
        assert!(fiemap_extents(&m).iter().all(|e| e.2 & ENCODED != 0));
        assert_eq!(read_all(&b), data);
        fs.inner.lock().check().unwrap();
    }

    #[test]
    fn defrag_refuses_bad_arguments_and_strangers() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let [a, _] = fragmented_pair(&fs, 2);
        let ctx = Ctx::root();
        let fd = ctx.open(a.clone(), OpenFlags::RDWR);
        let mut r = defrag_range_args(0, 0, 1 << 5, 0, 0);
        assert_eq!(
            ctx.ioctl(fd, BTRFS_IOC_DEFRAG_RANGE, &mut r),
            Err(LxError::EOPNOTSUPP)
        );
        let mut r = defrag_range_args(0, 0, BTRFS_DEFRAG_RANGE_COMPRESS, 0, 9);
        assert_eq!(
            ctx.ioctl(fd, BTRFS_IOC_DEFRAG_RANGE, &mut r),
            Err(LxError::EINVAL)
        );
        // Someone else, with the file open only for reading.
        let user = Ctx::as_user(1000);
        let ro = user.open(a.clone(), OpenFlags::RDONLY);
        assert_eq!(
            user.ioctl(ro, BTRFS_IOC_DEFRAG, &mut []),
            Err(LxError::EPERM)
        );
        // A directory: CAP_SYS_ADMIN, and nothing to do.
        let dfd = user.open(fs.root_inode(), OpenFlags::RDONLY);
        assert_eq!(
            user.ioctl(dfd, BTRFS_IOC_DEFRAG, &mut []),
            Err(LxError::EPERM)
        );
        let dfd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        ctx.ioctl(dfd, BTRFS_IOC_DEFRAG, &mut []).unwrap();
    }

    #[test]
    fn defrag_writes_back_the_page_cache_first() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let [a, _] = fragmented_pair(&fs, 8);
        let ctx = Ctx::root();
        let fd = ctx.open(a.clone(), OpenFlags::RDWR);
        // A write through the descriptor that may still sit in the cache.
        let file = ctx.fd(fd);
        file.write_at(BLOCK, &[0xee; 100]).unwrap();
        let want = {
            let mut v = read_all(&a);
            v[BLOCK as usize..BLOCK as usize + 100].fill(0xee);
            v
        };
        ctx.ioctl(fd, BTRFS_IOC_DEFRAG, &mut []).unwrap();
        assert_eq!(read_all(&a), want);
        assert_eq!(extent_count(&ctx, fd), 1);
    }

    #[test]
    fn memparse_reads_sizes_as_btrfs_progs_writes_them() {
        assert_eq!(memparse("4096"), Some(4096));
        assert_eq!(memparse("8m"), Some(8 << 20));
        assert_eq!(memparse("2G"), Some(2 << 30));
        assert_eq!(memparse("1e"), Some(1 << 60));
        assert_eq!(memparse("0"), None);
        assert_eq!(memparse(""), None);
        assert_eq!(memparse("12x"), None);
        assert_eq!(memparse("m"), None);
        assert_eq!(memparse("99999999999e"), None);
    }

    #[test]
    fn resize_shrinks_grows_and_reads_relative_sizes() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let data = pattern(3 << 20, 4);
        let f = file_with(&fs.root_inode(), "f", &data);
        let ctx = Ctx::root();
        let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        let size = || fs.inner.lock().fsinfo().total_bytes;
        ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "-8m"))
            .unwrap();
        assert_eq!(size(), 24 << 20);
        ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "1:+4m"))
            .unwrap();
        assert_eq!(size(), 28 << 20);
        ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "16m"))
            .unwrap();
        assert_eq!(size(), 16 << 20);
        ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "max"))
            .unwrap();
        assert_eq!(size(), 32 << 20);
        assert_eq!(read_all(&f), data);
        fs.inner.lock().check().unwrap();
    }

    #[test]
    fn resize_refuses_bad_sizes_devices_and_users() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let ctx = Ctx::root();
        let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        for bad in ["", "big", "1:", "x:8m", "64m", "-64m"] {
            assert_eq!(
                ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, bad)),
                Err(LxError::EINVAL),
                "{bad}"
            );
        }
        assert_eq!(
            ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "7:8m")),
            Err(LxError::ENODEV)
        );
        assert_eq!(
            ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "7:max")),
            Err(LxError::ENODEV)
        );
        let user = Ctx::as_user(1000);
        let ufd = user.open(fs.root_inode(), OpenFlags::RDONLY);
        assert_eq!(
            user.ioctl(ufd, BTRFS_IOC_RESIZE, &mut vol_args(0, "8m")),
            Err(LxError::EPERM)
        );
        // Too full to shrink that far.
        file_with(&fs.root_inode(), "big", &pattern(12 << 20, 1));
        fs.sync().unwrap();
        assert_eq!(
            ctx.ioctl(fd, BTRFS_IOC_RESIZE, &mut vol_args(0, "8m")),
            Err(LxError::ENOSPC)
        );
        assert_eq!(fs.inner.lock().fsinfo().total_bytes, 16 << 20);
    }

    #[test]
    fn fiemap_maps_the_data_and_counts_with_no_room() {
        const LAST: u32 = 0x1;
        const SHARED: u32 = 0x2000;
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let ctx = Ctx::root();
        let f = sparse_file(&fs);
        let fd = ctx.open(f.clone(), OpenFlags::RDONLY);
        let mut b = fiemap_args(0, u64::MAX, 0, 16);
        ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b).unwrap();
        let ext = fiemap_extents(&b);
        let spans: Vec<_> = ext
            .iter()
            .map(|&(l, n, _)| (l >> 10, (l + n) >> 10))
            .collect();
        // Writes may land in more than one extent; together they cover the
        // two data ranges exactly.
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (s, t) in spans {
            match merged.last_mut() {
                Some(m) if m.1 == s => m.1 = t,
                _ => merged.push((s, t)),
            }
        }
        assert_eq!(merged, [(0, 256), (512, 768)]);
        assert_eq!(ext.last().unwrap().2 & LAST, LAST);
        assert!(ext.iter().all(|e| e.2 & SHARED == 0));
        // No room: only the count comes back.
        let mut c = fiemap_args(0, u64::MAX, 0, 0);
        ctx.ioctl(fd, FS_IOC_FIEMAP, &mut c).unwrap();
        assert_eq!(get32(&c, 20) as usize, ext.len());
        // Room for one: the first extent, without LAST unless it is.
        let mut one = fiemap_args(0, u64::MAX, 0, 1);
        ctx.ioctl(fd, FS_IOC_FIEMAP, &mut one).unwrap();
        assert_eq!(fiemap_extents(&one), ext[..1]);
        // A reflinked copy shares its blocks.
        let g = file_with(&fs.root_inode(), "copy", b"");
        let gfd = ctx.open(g, OpenFlags::RDWR);
        let mut arg = (fd as i32).to_le_bytes().to_vec();
        ctx.ioctl(gfd, FICLONE, &mut arg).unwrap();
        let mut b = fiemap_args(0, u64::MAX, 1, 16);
        ctx.ioctl(gfd, FS_IOC_FIEMAP, &mut b).unwrap();
        assert!(fiemap_extents(&b).iter().all(|e| e.2 & SHARED != 0));
    }

    #[test]
    fn fiemap_refuses_unknown_flags_and_an_empty_range() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let ctx = Ctx::root();
        let fd = ctx.open(file_with(&fs.root_inode(), "f", b"x"), OpenFlags::RDONLY);
        // FIEMAP_FLAG_XATTR (2) is not supported: EBADR, and fm_flags says
        // which flags were not understood.
        let mut b = fiemap_args(0, u64::MAX, 0x3, 4);
        assert_eq!(ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b), Err(LxError::EBADR));
        assert_eq!(get32(&b, 16), 0x2);
        let mut b = fiemap_args(0, 0, 0, 4);
        assert_eq!(ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b), Err(LxError::EINVAL));
        // Inline data: one piece, flagged as such.
        let mut b = fiemap_args(0, u64::MAX, 0, 4);
        ctx.ioctl(fd, FS_IOC_FIEMAP, &mut b).unwrap();
        assert_eq!(fiemap_extents(&b), [(0, 1, 0x1 | 0x100 | 0x200)]);
        // A directory maps nothing.
        let dfd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        let mut b = fiemap_args(0, u64::MAX, 0, 4);
        ctx.ioctl(dfd, FS_IOC_FIEMAP, &mut b).unwrap();
        assert_eq!(get32(&b, 20), 0);
    }

    #[test]
    fn seek_data_and_seek_hole_follow_the_extents() {
        const DATA: usize = 3;
        const HOLE: usize = 4;
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let f = sparse_file(&fs);
        let file = File::new(f.clone(), OpenFlags::RDONLY, String::from("/efs-test/f"));
        let seek = |w, off: i64| eclipsefs_seek(&file, w, off).unwrap();
        assert_eq!(seek(DATA, 0), Ok(0));
        assert_eq!(seek(HOLE, 0), Ok(256 << 10));
        assert_eq!(seek(DATA, 300 << 10), Ok(512 << 10));
        assert_eq!(seek(HOLE, 600 << 10), Ok(768 << 10));
        // Only a hole after the last data, and nothing at or past the end.
        assert_eq!(seek(DATA, 800 << 10), Err(LxError::ENXIO));
        assert_eq!(seek(HOLE, 800 << 10), Ok(800 << 10));
        assert_eq!(seek(DATA, 1 << 20), Err(LxError::ENXIO));
        assert_eq!(seek(HOLE, -1), Err(LxError::ENXIO));
        // Data written through the page cache counts before it is synced.
        assert_eq!(f.write_at(300 << 10, b"late").unwrap(), 4);
        assert_eq!(seek(DATA, 260 << 10), Ok(300 << 10 & !4095));
    }

    #[test]
    fn copy_file_range_reflinks_aligned_ranges_and_leaves_the_rest_to_a_copy() {
        let disk = formatted(32 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let data = pattern((1 << 20) + 123, 13);
        let src = File::new(
            file_with(&root, "s", &data),
            OpenFlags::RDONLY,
            String::from("/efs-test/s"),
        );
        let dst_inode = file_with(&root, "d", b"");
        let dst = File::new(
            dst_inode.clone(),
            OpenFlags::RDWR,
            String::from("/efs-test/d"),
        );
        // Unaligned: not ours to do.
        assert!(eclipsefs_copy_reflink(&src, 1, &dst, 0, 4096).is_none());
        // Aligned and up to the end of the source, odd tail included.
        assert_eq!(
            eclipsefs_copy_reflink(&src, 0, &dst, 0, u64::MAX),
            Some(Ok(data.len() as u64))
        );
        assert_eq!(read_all(&dst_inode), data);
        // A length that is not whole blocks short of the end is cut to blocks.
        let dst2 = File::new(
            file_with(&root, "d2", b""),
            OpenFlags::RDWR,
            String::from("/efs-test/d2"),
        );
        assert_eq!(
            eclipsefs_copy_reflink(&src, 0, &dst2, 0, 10_000),
            Some(Ok(8192))
        );
        assert!(eclipsefs_copy_reflink(&src, 0, &dst2, 0, 100).is_none());
        // Different filesystems copy.
        let other = mount(&formatted(16 << 20));
        let far = File::new(
            file_with(&other.root_inode(), "x", b""),
            OpenFlags::RDWR,
            String::from("/efs-test/x"),
        );
        assert!(eclipsefs_copy_reflink(&src, 0, &far, 0, 4096).is_none());
    }

    #[test]
    fn a_snapshot_sent_into_a_file_is_received_back_into_a_directory() {
        let disk = formatted(48 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "src"))
            .unwrap();
        let src = root.find("src").unwrap();
        let body = pattern(300 << 10, 17);
        file_with(&src, "file", &body);
        src.create("dir", FileType::Dir, 0o755).unwrap();
        let s = ctx.open(src, OpenFlags::RDONLY);
        ctx.ioctl(
            top,
            BTRFS_IOC_SNAP_CREATE_V2,
            &mut vol_args_v2(s as i64, BTRFS_SUBVOL_RDONLY, "snap"),
        )
        .unwrap();
        let snap = ctx.open(root.find("snap").unwrap(), OpenFlags::RDONLY);
        let stream = ctx.open(file_with(&root, "stream", b""), OpenFlags::RDWR);
        // struct btrfs_ioctl_send_args: send_fd, clone count/sources, parent, flags.
        let mut send = [0u8; 72];
        send[0..8].copy_from_slice(&(stream as i64).to_le_bytes());
        ctx.ioctl(snap, BTRFS_IOC_SEND, &mut send).unwrap();
        let len = ctx.fd(stream).metadata().unwrap().size;
        assert!(
            len > body.len(),
            "the stream carries the data: {} bytes",
            len
        );
        // Receive it from the start of the stream file.
        let back = root.create("back", FileType::Dir, 0o755).unwrap();
        let b = ctx.open(back.clone(), OpenFlags::RDONLY);
        let input = ctx.open(src_named(&fs, "stream"), OpenFlags::RDONLY);
        let mut recv = [0u8; 32];
        recv[0..8].copy_from_slice(&(input as i64).to_le_bytes());
        ctx.ioctl(b, EFS_IOC_RECEIVE, &mut recv).unwrap();
        assert_eq!(
            ctx.fd(input).seek(SeekFrom::Current(0)).unwrap(),
            len as u64,
            "the stream was read to its end"
        );
        let got = names(&back);
        assert_eq!(got.len(), 1, "{:?}", got);
        let copy = back.find(&got[0]).unwrap();
        assert_eq!(read_all(&copy.find("file").unwrap()), body);
        assert!(copy.find("dir").is_ok());
        // Sending needs CAP_SYS_ADMIN, and a file, not a directory, to go to.
        let user = Ctx::as_user(1000);
        let usnap = user.open(root.find("snap").unwrap(), OpenFlags::RDONLY);
        assert_eq!(
            user.ioctl(usnap, BTRFS_IOC_SEND, &mut send),
            Err(LxError::EPERM)
        );
        let mut to_dir = [0u8; 72];
        to_dir[0..8].copy_from_slice(&(top as i64).to_le_bytes());
        assert!(ctx.ioctl(snap, BTRFS_IOC_SEND, &mut to_dir).is_err());
    }

    #[test]
    fn mount_options_are_read_and_bad_values_refused() {
        let o =
            EfsOptions::parse("rw,noatime,compress=zstd:3,degraded,device=/dev/sdb2,subvol=/@home")
                .unwrap();
        assert_eq!(o.compress, Some(Some(Compression::Zstd)));
        assert!(o.degraded);
        assert_eq!(o.devices, ["sdb2"]);
        assert_eq!(o.subvol.as_deref(), Some("/@home"));
        assert_eq!(
            EfsOptions::parse("compress=no").unwrap().compress,
            Some(None)
        );
        assert_eq!(
            EfsOptions::parse("compress").unwrap().compress,
            Some(Some(Compression::Zlib))
        );
        assert_eq!(
            EfsOptions::parse("subvolid=257").unwrap().subvolid,
            Some(257)
        );
        for bad in ["compress=brotli", "subvolid=x", "subvol", "device="] {
            assert!(EfsOptions::parse(bad).is_err(), "{}", bad);
        }
        assert_eq!(EfsOptions::parse("").unwrap(), EfsOptions::default());
    }

    #[test]
    fn subvol_mounts_a_subvolume_as_the_root() {
        let disk = formatted(16 << 20);
        {
            let fs = mount(&disk);
            let root = fs.root_inode();
            let ctx = Ctx::root();
            let top = ctx.open(root.clone(), OpenFlags::RDONLY);
            ctx.ioctl(top, BTRFS_IOC_SUBVOL_CREATE, &mut vol_args(0, "@home"))
                .unwrap();
            file_with(&root.find("@home").unwrap(), "inside", b"yes");
            fs.sync().unwrap();
        }
        let fs = mount_with(&disk, false, "subvol=/@home");
        assert_eq!(read_all(&fs.root_inode().find("inside").unwrap()), b"yes");
        let opts = EfsOptions::parse("subvol=/missing").unwrap();
        assert!(EclipseMountFs::open_with(&MountBackend::Block(disk), false, &opts).is_err());
    }

    #[test]
    fn mount_compress_compresses_new_data() {
        let disk = formatted(32 << 20);
        let fs = mount_with(&disk, false, "compress=zstd");
        let f = file_with(&fs.root_inode(), "z", &vec![b'a'; 1 << 20]);
        fs.sync().unwrap();
        let ino = f
            .as_any_ref()
            .downcast_ref::<EclipseMountINode>()
            .unwrap()
            .ino;
        let size = fs.inner.lock().compsize(ino).unwrap();
        assert!(size.disk_bytes < size.uncompressed_bytes / 4, "{:?}", size);
    }

    /// `n` RAM disks formatted as one volume with filesystem uuid `[tag; 16]`.
    fn volume(n: usize, tag: u8, profile: Profile) -> Vec<Arc<RamDisk>> {
        let disks: Vec<Arc<RamDisk>> = (0..n).map(|_| blank(48 << 20)).collect();
        let devs: Vec<Arc<dyn eclipsefs::BlockDevice>> = disks
            .iter()
            .map(|d| Arc::new(Formatter(d.clone())) as Arc<dyn eclipsefs::BlockDevice>)
            .collect();
        let opts = eclipsefs::mkfs::MkfsOptions {
            label: "VOL".into(),
            fsid: [tag; 16],
            chunk_uuid: [0; 16],
            dev_uuid: [0; 16],
            subvol_uuid: [0; 16],
            now: (1_700_000_000, 0),
        };
        eclipsefs::mkfs::format_devices(&devs, &opts, profile, profile).unwrap();
        disks
    }

    #[test]
    fn a_volume_is_put_together_from_the_registered_devices() {
        let disks = volume(2, 0xa1, Profile::Raid1);
        for (i, d) in disks.iter().enumerate() {
            let block: Arc<dyn BlockScheme> = d.clone();
            assert!(probe_member(&block), "device {} passes the probe", i);
            register_block_device(&format!("efs-vol-a1-{}", i), block);
        }
        let data = pattern(500 << 10, 21);
        {
            // Mount through the SECOND device: the first is found by scan.
            let fs = mount(&disks[1]);
            assert_eq!(fs.inner.lock().devices().len(), 2);
            file_with(&fs.root_inode(), "mirrored", &data);
            fs.sync().unwrap();
            let ctx = Ctx::root();
            let fd = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
            let mut info = vec![0u8; 1024];
            ctx.ioctl(fd, BTRFS_IOC_FS_INFO, &mut info).unwrap();
            assert_eq!((get64(&info, 0), get64(&info, 8)), (2, 2));
            let mut dev = vec![0u8; 4096];
            put64(&mut dev, 0, 2);
            ctx.ioctl(fd, BTRFS_IOC_DEV_INFO, &mut dev).unwrap();
            assert_eq!(get64(&dev, 32), 48 << 20);
        }
        let fs = mount(&disks[0]);
        assert_eq!(read_all(&fs.root_inode().find("mirrored").unwrap()), data);
    }

    fn probe_member(block: &Arc<dyn BlockScheme>) -> bool {
        super::super::eclipsefs_mount::probe_eclipsefs_superblock(block)
    }

    #[test]
    fn a_volume_with_a_device_missing_mounts_only_degraded() {
        let disks = volume(2, 0xa2, Profile::Raid1);
        for (i, d) in disks.iter().enumerate() {
            let block: Arc<dyn BlockScheme> = d.clone();
            register_block_device(&format!("efs-vol-a2-{}", i), block);
        }
        {
            let fs = mount(&disks[0]);
            file_with(&fs.root_inode(), "kept", b"both copies");
            fs.sync().unwrap();
        }
        // The second device goes away.
        forget_block_device("efs-vol-a2-1");
        let opts = EfsOptions::default();
        assert_eq!(
            EclipseMountFs::open_with(&MountBackend::Block(disks[0].clone()), false, &opts).err(),
            Some(FsError::InvalidParam)
        );
        let fs = mount_with(&disks[0], true, "degraded");
        assert_eq!(
            read_all(&fs.root_inode().find("kept").unwrap()),
            b"both copies"
        );
    }

    #[test]
    fn parity_and_tiers_by_ioctl() {
        let disks = volume(3, 0xa5, Profile::Raid5);
        for (i, d) in disks.iter().enumerate() {
            let block: Arc<dyn BlockScheme> = d.clone();
            register_block_device(&format!("efs-vol-a5-{}", i), block);
        }
        let data = pattern(2 << 20, 25);
        {
            let fs = mount(&disks[0]);
            assert_eq!(fs.inner.lock().profiles(), (Profile::Raid5, Profile::Raid5));
            file_with(&fs.root_inode(), "striped", &data);
            fs.sync().unwrap();
            let ctx = Ctx::root();
            let top = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
            // btrfs balance start -dconvert=raid6 (three devices are enough)
            let mut bal = vec![0u8; 1024];
            put64(&mut bal, 0, BTRFS_BALANCE_DATA);
            put64(&mut bal, 16 + 56, BTRFS_BLOCK_GROUP_RAID6);
            put64(&mut bal, 16 + 64, BTRFS_BALANCE_ARGS_CONVERT);
            ctx.ioctl(top, BTRFS_IOC_BALANCE_V2, &mut bal).unwrap();
            assert_eq!(fs.inner.lock().profiles(), (Profile::Raid6, Profile::Raid5));
            // Device 1 fast, the others slow.
            for (devid, tier) in [(1u64, 1u64), (2, 2), (3, 2)] {
                let mut t = vec![0u8; 32];
                put64(&mut t, 0, devid);
                put64(&mut t, 8, tier);
                ctx.ioctl(top, EFS_IOC_SET_TIER, &mut t).unwrap();
            }
            let mut t = vec![0u8; 32];
            put64(&mut t, 0, 1);
            ctx.ioctl(top, EFS_IOC_GET_TIER, &mut t).unwrap();
            assert_eq!(get64(&t, 8), 1);
            put64(&mut t, 0, 9);
            assert_eq!(
                ctx.ioctl(top, EFS_IOC_GET_TIER, &mut t),
                Err(LxError::ENODEV)
            );
            let mut bad = vec![0u8; 32];
            put64(&mut bad, 0, 1);
            put64(&mut bad, 8, 7);
            assert_eq!(
                ctx.ioctl(top, EFS_IOC_SET_TIER, &mut bad),
                Err(LxError::EINVAL)
            );
            // Only two slow devices: RAID6 data cannot live on them alone,
            // so it stays put; nothing breaks.
            let mut mig = vec![0u8; 32];
            put64(&mut mig, 0, EFS_TIER_MIGRATE_ALL);
            ctx.ioctl(top, EFS_IOC_TIER_MIGRATE, &mut mig).unwrap();
            assert_eq!(read_all(&fs.root_inode().find("striped").unwrap()), data);
            fs.sync().unwrap();
        }
        // Lose a device: parity has the rest.
        forget_block_device("efs-vol-a5-2");
        let fs = mount_with(&disks[0], true, "degraded");
        assert_eq!(read_all(&fs.root_inode().find("striped").unwrap()), data);
        assert_eq!(fs.inner.lock().device_tier(1), Tier::Fast);
        forget_block_device("efs-vol-a5-0");
        forget_block_device("efs-vol-a5-1");
    }

    #[test]
    fn devices_are_added_balanced_and_removed_by_ioctl() {
        let disk = formatted(48 << 20);
        let fs = mount(&disk);
        let data = pattern(1 << 20, 23);
        file_with(&fs.root_inode(), "payload", &data);
        // Bigger than the first: a mirror of all of it has to fit.
        let extra = blank(64 << 20);
        let extra_block: Arc<dyn BlockScheme> = extra.clone();
        let node: Arc<dyn INode> = Arc::new(super::super::devfs::BlockDev::new(
            200,
            extra_block,
            "efs-extra".to_string(),
        ));
        let mut ctx = Ctx::root();
        ctx.devices.push((String::from("/dev/efs-extra"), node));
        let top = ctx.open(fs.root_inode(), OpenFlags::RDONLY);
        ctx.ioctl(top, BTRFS_IOC_ADD_DEV, &mut vol_args(0, "/dev/efs-extra"))
            .unwrap();
        assert_eq!(fs.inner.lock().devices().len(), 2);
        // Adding it twice is refused.
        assert_eq!(
            ctx.ioctl(top, BTRFS_IOC_ADD_DEV, &mut vol_args(0, "/dev/efs-extra")),
            Err(LxError::EEXIST)
        );
        // btrfs balance start -dconvert=raid1 -mconvert=raid1
        let mut bal = vec![0u8; 1024];
        put64(&mut bal, 0, BTRFS_BALANCE_DATA | BTRFS_BALANCE_METADATA);
        for at in [16, 152] {
            put64(&mut bal, at + 56, BTRFS_BLOCK_GROUP_RAID1);
            put64(&mut bal, at + 64, BTRFS_BALANCE_ARGS_CONVERT);
        }
        ctx.ioctl(top, BTRFS_IOC_BALANCE_V2, &mut bal).unwrap();
        assert!(get64(&bal, 440) > 0, "chunks were moved");
        assert_eq!(fs.inner.lock().profiles(), (Profile::Raid1, Profile::Raid1));
        // RAID1 cannot drop to one device.
        let mut rm = vol_args_v2(0, BTRFS_DEVICE_SPEC_BY_ID, "");
        put64(&mut rm, 56, 2);
        assert!(ctx.ioctl(top, BTRFS_IOC_RM_DEV_V2, &mut rm).is_err());
        for at in [16, 152] {
            put64(&mut bal, at + 56, BTRFS_AVAIL_ALLOC_BIT_SINGLE);
        }
        ctx.ioctl(top, BTRFS_IOC_BALANCE_V2, &mut bal).unwrap();
        // Removed by path this time.
        ctx.ioctl(top, BTRFS_IOC_RM_DEV, &mut vol_args(0, "/dev/efs-extra"))
            .unwrap();
        assert_eq!(fs.inner.lock().devices().len(), 1);
        assert_eq!(read_all(&src_named(&fs, "payload")), data);
        // A combination of profiles is not one.
        put64(
            &mut bal,
            16 + 56,
            BTRFS_BLOCK_GROUP_RAID5 | BTRFS_BLOCK_GROUP_RAID1,
        );
        assert_eq!(
            ctx.ioctl(top, BTRFS_IOC_BALANCE_V2, &mut bal),
            Err(LxError::EINVAL)
        );
    }

    /// `struct fscrypt_key_specifier` naming key `id`.
    fn key_spec(id: &[u8; 16], size: usize) -> Vec<u8> {
        let mut b = vec![0u8; size];
        put32(&mut b, 0, FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER);
        b[8..24].copy_from_slice(id);
        b
    }

    /// `FS_IOC_ADD_ENCRYPTION_KEY` with raw key `key`; the identifier.
    fn add_key(ctx: &Ctx, fd: i32, key: &[u8]) -> LxResult<[u8; 16]> {
        let mut b = vec![0u8; FSCRYPT_ADD_KEY_ARG + key.len()];
        put32(&mut b, 0, FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER);
        put32(&mut b, 40, key.len() as u32);
        b[FSCRYPT_ADD_KEY_ARG..].copy_from_slice(key);
        ctx.ioctl(fd, FS_IOC_ADD_ENCRYPTION_KEY, &mut b)?;
        Ok(b[8..24].try_into().unwrap())
    }

    /// `struct fscrypt_policy_v2` (AES-256-XTS, AES-256-CTS, 32-byte
    /// name padding).
    fn policy_v2(id: &[u8; 16]) -> Vec<u8> {
        let mut b = vec![0u8; FSCRYPT_POLICY_V2_SIZE];
        b[..4].copy_from_slice(&[FSCRYPT_POLICY_V2, 1, 4, 3]);
        b[8..24].copy_from_slice(id);
        b
    }

    fn key_status(ctx: &Ctx, fd: i32, id: &[u8; 16]) -> u32 {
        let mut b = key_spec(id, 128);
        ctx.ioctl(fd, FS_IOC_GET_ENCRYPTION_KEY_STATUS, &mut b)
            .unwrap();
        get32(&b, 64)
    }

    #[test]
    fn the_fscrypt_numbers_are_the_ones_linux_uses() {
        const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
            dir << 30 | size << 16 | (b'f' as u32) << 8 | nr
        }
        const W: u32 = 1;
        const R: u32 = 2;
        assert_eq!(FS_IOC_SET_ENCRYPTION_POLICY, ioc(R, 19, 12));
        assert_eq!(FS_IOC_GET_ENCRYPTION_POLICY, ioc(W, 21, 12));
        assert_eq!(FS_IOC_GET_ENCRYPTION_POLICY_EX, ioc(R | W, 22, 9));
        assert_eq!(FS_IOC_ADD_ENCRYPTION_KEY, ioc(R | W, 23, 80));
        assert_eq!(FS_IOC_REMOVE_ENCRYPTION_KEY, ioc(R | W, 24, 64));
        assert_eq!(FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS, ioc(R | W, 25, 64));
        assert_eq!(FS_IOC_GET_ENCRYPTION_KEY_STATUS, ioc(R | W, 26, 128));
        assert_eq!(FS_IOC_GET_ENCRYPTION_NONCE, ioc(R, 27, 16));
        assert_eq!(LxError::ENOKEY as isize, 126);
    }

    #[test]
    fn an_encrypted_directory_locks_and_unlocks_like_fscrypt() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        let key = [0x33u8; 64];
        let id = add_key(&ctx, top, &key).unwrap();
        assert_eq!(key_status(&ctx, top, &id), FSCRYPT_KEY_STATUS_PRESENT);

        let vault = root.create("vault", FileType::Dir, 0o700).unwrap();
        let vfd = ctx.open(vault.clone(), OpenFlags::RDONLY);
        ctx.ioctl(vfd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id))
            .unwrap();
        let mut ex = vec![0u8; 8 + FSCRYPT_POLICY_V2_SIZE];
        put64(&mut ex, 0, FSCRYPT_POLICY_V2_SIZE as u64);
        ctx.ioctl(vfd, FS_IOC_GET_ENCRYPTION_POLICY_EX, &mut ex)
            .unwrap();
        assert_eq!(get64(&ex, 0), FSCRYPT_POLICY_V2_SIZE as u64);
        assert_eq!(ex[8..], policy_v2(&id)[..]);
        // The old call cannot describe a v2 policy.
        assert_eq!(
            ctx.ioctl(vfd, FS_IOC_GET_ENCRYPTION_POLICY, &mut [0u8; 12]),
            Err(LxError::EINVAL)
        );
        let mut flags = [0u8; 4];
        ctx.ioctl(vfd, FS_IOC_GETFLAGS, &mut flags).unwrap();
        assert_ne!(u32::from_le_bytes(flags) & 0x800, 0);

        let data = pattern(20_000, 7);
        let f = file_with(&vault, "secret.txt", &data);
        let ffd = ctx.open(f.clone(), OpenFlags::RDONLY);
        let mut nonce = [0u8; 16];
        ctx.ioctl(ffd, FS_IOC_GET_ENCRYPTION_NONCE, &mut nonce)
            .unwrap();
        let mut dir_nonce = [0u8; 16];
        ctx.ioctl(vfd, FS_IOC_GET_ENCRYPTION_NONCE, &mut dir_nonce)
            .unwrap();
        assert_ne!(nonce, dir_nonce);
        assert_eq!(names(&vault), ["secret.txt"]);

        // Lock: names become no-key names, the data and new names need the
        // key.
        let mut rm = key_spec(&id, 64);
        ctx.ioctl(top, FS_IOC_REMOVE_ENCRYPTION_KEY, &mut rm)
            .unwrap();
        assert_eq!(get32(&rm, 40), 0);
        assert_eq!(key_status(&ctx, top, &id), FSCRYPT_KEY_STATUS_ABSENT);
        let hidden = names(&vault);
        assert_eq!(hidden.len(), 1);
        assert_ne!(hidden[0], "secret.txt");
        assert!(vault.find(&hidden[0]).is_ok());
        let mut buf = [0u8; 16];
        assert_eq!(f.read_at(0, &mut buf), Err(FsError::NoKey));
        assert_eq!(
            vault.create("new", FileType::File, 0o644).err(),
            Some(FsError::NoKey)
        );
        assert_eq!(
            LxError::from(FsError::NoKey),
            LxError::ENOKEY,
            "userspace sees ENOKEY"
        );
        assert_eq!(
            ctx.ioctl(top, FS_IOC_REMOVE_ENCRYPTION_KEY, &mut key_spec(&id, 64)),
            Err(LxError::ENOKEY)
        );

        // Unlock: everything is back.
        assert_eq!(add_key(&ctx, top, &key).unwrap(), id);
        assert_eq!(names(&vault), ["secret.txt"]);
        assert_eq!(read_all(&vault.find("secret.txt").unwrap()), data);
        fs.sync().unwrap();
        drop(fs);
        // A new mount starts locked.
        let fs = mount(&disk);
        let vault = fs.root_inode().find("vault").unwrap();
        assert!(vault.find("secret.txt").is_err());
    }

    #[test]
    fn fscrypt_ioctls_refuse_what_linux_refuses() {
        let disk = formatted(16 << 20);
        let fs = mount(&disk);
        let root = fs.root_inode();
        let ctx = Ctx::root();
        let top = ctx.open(root.clone(), OpenFlags::RDONLY);
        let id = add_key(&ctx, top, &[0x44u8; 32]).unwrap();
        // Keys too short, from a keyring, or named by a v1 descriptor.
        assert_eq!(add_key(&ctx, top, &[1u8; 8]), Err(LxError::EINVAL));
        let mut b = vec![0u8; FSCRYPT_ADD_KEY_ARG + 32];
        put32(&mut b, 0, FSCRYPT_KEY_SPEC_TYPE_IDENTIFIER);
        put32(&mut b, 40, 32);
        put32(&mut b, 44, 1234);
        assert_eq!(
            ctx.ioctl(top, FS_IOC_ADD_ENCRYPTION_KEY, &mut b),
            Err(LxError::EINVAL)
        );
        let mut v1 = key_spec(&id, 64);
        put32(&mut v1, 0, 1);
        assert_eq!(
            ctx.ioctl(top, FS_IOC_REMOVE_ENCRYPTION_KEY, &mut v1),
            Err(LxError::EINVAL)
        );

        // Plain inodes have no policy and no nonce.
        let mut ex = vec![0u8; 8 + FSCRYPT_POLICY_V2_SIZE];
        put64(&mut ex, 0, FSCRYPT_POLICY_V2_SIZE as u64);
        assert_eq!(
            ctx.ioctl(top, FS_IOC_GET_ENCRYPTION_POLICY_EX, &mut ex),
            Err(LxError::ENODATA)
        );
        assert_eq!(
            ctx.ioctl(top, FS_IOC_GET_ENCRYPTION_NONCE, &mut [0u8; 16]),
            Err(LxError::ENODATA)
        );

        // A v1 policy, a full directory, a file, a missing key, a stranger.
        let full = root.create("full", FileType::Dir, 0o755).unwrap();
        file_with(&full, "x", b"");
        let full = ctx.open(full, OpenFlags::RDONLY);
        let mut v1 = vec![0u8; 12];
        v1[1] = 1;
        v1[2] = 4;
        assert_eq!(
            ctx.ioctl(full, FS_IOC_SET_ENCRYPTION_POLICY, &mut v1),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            ctx.ioctl(full, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id)),
            Err(LxError::ENOTEMPTY)
        );
        let f = ctx.open(file_with(&root, "f", b""), OpenFlags::RDONLY);
        assert_eq!(
            ctx.ioctl(f, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id)),
            Err(LxError::ENOTDIR)
        );
        let empty = root.create("empty", FileType::Dir, 0o777).unwrap();
        let efd = ctx.open(empty.clone(), OpenFlags::RDONLY);
        assert_eq!(
            ctx.ioctl(efd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&[9; 16])),
            Err(LxError::ENOKEY)
        );
        let stranger = Ctx::as_user(1000);
        let sfd = stranger.open(empty.clone(), OpenFlags::RDONLY);
        assert_eq!(
            stranger.ioctl(sfd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id)),
            Err(LxError::EACCES)
        );
        assert_eq!(
            stranger.ioctl(
                sfd,
                FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS,
                &mut key_spec(&id, 64)
            ),
            Err(LxError::EACCES)
        );
        // Once set, the same policy is fine and another one is EEXIST.
        ctx.ioctl(efd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id))
            .unwrap();
        ctx.ioctl(efd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&id))
            .unwrap();
        let other = add_key(&ctx, top, &[0x55u8; 32]).unwrap();
        assert_eq!(
            ctx.ioctl(efd, FS_IOC_SET_ENCRYPTION_POLICY, &mut policy_v2(&other)),
            Err(LxError::EEXIST)
        );
        ctx.ioctl(
            top,
            FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS,
            &mut key_spec(&id, 64),
        )
        .unwrap();
    }
}
