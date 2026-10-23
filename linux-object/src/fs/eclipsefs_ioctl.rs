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
pub const FICLONE: u32 = 0x4004_9409;
pub const FICLONERANGE: u32 = 0x4020_940d;
pub const FIDEDUPERANGE: u32 = 0xc018_9436;

/// `FICLONE` / `FICLONERANGE` / `FIDEDUPERANGE`: VFS ioctls. On a filesystem
/// without clone support Linux answers `EOPNOTSUPP` so `cp --reflink=auto`
/// and friends fall back to a byte copy — not `EINVAL`.
pub fn is_vfs_clone_ioctl(cmd: u32) -> bool {
    matches!(cmd, FICLONE | FICLONERANGE | FIDEDUPERANGE)
}
pub const FS_IOC_FIEMAP: u32 = 0xc020_660b;

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

/// `pwritev2(RWF_ATOMIC)` into `inode` (opened as `path`) on an EclipseFS
/// mount: all of `data` or nothing, untorn by a crash. `None` when the
/// inode is on another filesystem.
pub(crate) fn eclipsefs_write_atomic(
    inode: &Arc<dyn INode>,
    path: &str,
    offset: u64,
    data: &[u8],
) -> Option<LxResult<usize>> {
    let (fs, ino) = efs_of(inode)?;
    Some((|| {
        if fs.read_only || super::path_is_read_only(path) {
            return Err(LxError::EROFS);
        }
        fs.inner
            .lock()
            .write_atomic(ino, offset, data)
            .map_err(|e| match e {
                // RWF_ATOMIC on a file that cannot do it.
                EfsError::Unsupported(_) => LxError::EINVAL,
                e => errno(e),
            })
    })())
}

/// `statx`'s atomic write unit (smallest, largest) for `inode`: a regular
/// file of an EclipseFS mount that is not no-COW. `None` otherwise.
pub fn eclipsefs_atomic_write_unit(inode: &Arc<dyn INode>) -> Option<(u32, u32)> {
    let (fs, ino) = efs_of(inode)?;
    let ok = fs.inner.lock().atomic_write_supported(ino).ok()?;
    ok.then_some((
        eclipsefs::format::BLOCK_SIZE as u32,
        eclipsefs::cow::ATOMIC_WRITE_MAX as u32,
    ))
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
mod tests;
