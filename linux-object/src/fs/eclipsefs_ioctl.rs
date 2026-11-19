//! What an EclipseFS mount of the kernel offers past `read`/`write`: the
//! filesystem ioctls, extended attributes, `fallocate`, reflinked
//! `copy_file_range` and `lseek(SEEK_DATA/SEEK_HOLE)`.
//!
//! What they do, and the bytes they take, is `eclipsefs::ioctl`'s, which a
//! process serving an EclipseFS volume runs too (`served`). What is the
//! kernel's is here: the copy of an ioctl's argument in from user memory
//! and back out, the open files it names, the page cache above the
//! filesystem, the devices a path names, and `BTRFS_IOC_SEND` and
//! `EFS_IOC_RECEIVE`, whose stream goes through another open file.
//!
//! The send stream is EclipseFS's format, not btrfs's, so it is received
//! with `EFS_IOC_RECEIVE` rather than by `btrfs receive`; and the stream
//! goes to (and comes from) a regular file, never a pipe, because an ioctl
//! here cannot wait for a reader.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use eclipsefs::cowfmt::{handle, FIRST_INO};
#[allow(unused_imports)]
pub(crate) use eclipsefs::ioctl::*;
use eclipsefs::ioctl::{self as shared, Caller, Open};
pub use eclipsefs::ioctl::{
    is_vfs_clone_ioctl, XattrReply, XattrRequest, EFS_IOC_RECEIVE, FICLONE, FICLONERANGE,
    FIDEDUPERANGE, FS_IOC_FIEMAP,
};
use eclipsefs::{EclipseFS, Error as EfsError};
use kernel_hal::user::{UserInPtr, UserOutPtr};
use rcore_fs::vfs::INode;

use super::block_mount::MountBackend;
use super::btrfs_mount::DevAdapter;
use super::eclipsefs_mount::{EclipseMountFs, EclipseMountINode};
use super::file::{cache_flush, cache_reload};
use super::{File, FileLike, SeekFrom};
use crate::error::{LxError, LxResult};
#[cfg(test)]
use alloc::string::String;
#[cfg(test)]
use core::convert::TryInto;
#[cfg(test)]
use eclipsefs::cowfmt::{split, FS_TREE};
#[cfg(test)]
use eclipsefs::{Compression, Profile, Tier};

#[cfg(test)]
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

fn lx(e: shared::Errno) -> LxError {
    LxError::from_errno(e)
}

fn errno(e: EfsError) -> LxError {
    lx(shared::errno_of(e))
}

/// `ioctl(2)` on a file of an EclipseFS mount: `None` when `file` is on
/// another filesystem or `cmd` is not a filesystem ioctl (the caller then
/// asks the inode as before). A served EclipseFS is asked through its
/// server (`served::ioctl`).
pub fn eclipsefs_ioctl(
    file: &File,
    cmd: u32,
    arg: usize,
    ctx: &dyn FsIoctlContext,
) -> Option<LxResult<usize>> {
    if !is_fs_ioctl(cmd) {
        return None;
    }
    if let Some(target) = Target::of(file) {
        return Some(target.user_ioctl(cmd, arg, ctx));
    }
    super::served::ioctl(file, cmd, arg, ctx)
}

/// Copy the argument of ioctl `cmd` in from user memory at `arg`: as many
/// bytes as its number says, or as its header says for the ones that
/// carry more. `FS_IOC_FIEMAP` is copied by its caller.
pub(crate) fn copy_in(cmd: u32, arg: usize) -> LxResult<Vec<u8>> {
    Ok(match cmd {
        // Its argument is not read (often NULL).
        BTRFS_IOC_DEFRAG => Vec::new(),
        // The argument is the source descriptor itself.
        FICLONE => (arg as i32).to_le_bytes().to_vec(),
        // An `int`, whatever `long` in the number says (as Linux reads it).
        FS_IOC_GETFLAGS | FS_IOC_SETFLAGS | FS_IOC32_GETFLAGS | FS_IOC32_SETFLAGS => {
            UserInPtr::<u8>::from(arg).read_array(4)?
        }
        // The version byte says which policy struct follows (the number
        // carries the size of the v1 one).
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
        _ => UserInPtr::<u8>::from(arg).read_array(arg_size(cmd))?,
    })
}

/// Copy back out to `arg` what ioctl `cmd` returns of `buf`, and wipe the
/// raw key an `FS_IOC_ADD_ENCRYPTION_KEY` brought in.
pub(crate) fn copy_out(cmd: u32, arg: usize, buf: &mut [u8]) -> LxResult<()> {
    let out = if cmd == FS_IOC_ADD_ENCRYPTION_KEY {
        &buf[..FSCRYPT_ADD_KEY_ARG.min(buf.len())]
    } else {
        &buf[..]
    };
    let done = if returns_data(cmd) && !out.is_empty() {
        UserOutPtr::<u8>::from(arg)
            .write_array(out)
            .map_err(LxError::from)
    } else {
        Ok(())
    };
    if cmd == FS_IOC_ADD_ENCRYPTION_KEY {
        buf.fill(0);
    }
    done
}

/// The file an ioctl is called on, and the EclipseFS inode behind it.
pub(crate) struct Target<'a> {
    file: &'a File,
    fs: Arc<EclipseMountFs>,
    ino: u64,
}

/// The kernel as an ioctl's [`Caller`]: the process `ctx` (none for what
/// asks nothing of it) and the open files the ioctl named.
struct Kernel<'a> {
    target: &'a Target<'a>,
    ctx: Option<&'a dyn FsIoctlContext>,
    files: RefCell<Vec<Arc<dyn INode>>>,
}

impl Kernel<'_> {
    /// What the filesystem is told of `file`, which is kept to flush and
    /// reload its pages by.
    fn open_of(&self, file: &File) -> Open {
        let flags = file.flags();
        let inode = file.inode();
        let ino = efs_of(&inode)
            .filter(|(fs, _)| Arc::ptr_eq(fs, &self.target.fs))
            .map(|(_, ino)| ino);
        let open = Open {
            ino,
            readable: flags.readable(),
            writable: flags.writable(),
            append: flags.is_append(),
        };
        self.files.borrow_mut().push(inode);
        open
    }

    /// The inode of the open file of inode number `ino`, for its pages.
    fn inode_of(&self, ino: u64) -> Option<Arc<dyn INode>> {
        if ino == self.target.ino {
            return Some(self.target.file.inode());
        }
        self.files
            .borrow()
            .iter()
            .find(|i| efs_of(i).is_some_and(|(_, n)| n == ino))
            .cloned()
    }
}

impl Caller for Kernel<'_> {
    fn with<R>(&self, f: impl FnOnce(&mut EclipseFS) -> R) -> R {
        f(&mut self.target.fs.inner.lock())
    }

    fn capable(&self, cap: u32) -> bool {
        self.ctx.is_some_and(|ctx| ctx.capable(cap))
    }

    fn euid(&self) -> u32 {
        self.ctx.map_or(u32::MAX, |ctx| ctx.euid())
    }

    fn read_only(&self) -> bool {
        self.target.fs.read_only || super::path_is_read_only(self.target.file.path())
    }

    fn this(&self) -> Open {
        let flags = self.target.file.flags();
        Open {
            ino: Some(self.target.ino),
            readable: flags.readable(),
            writable: flags.writable(),
            append: flags.is_append(),
        }
    }

    fn file(&self, fd: i32) -> Result<Open, shared::Errno> {
        let ctx = self.ctx.ok_or(shared::errno::EBADF)?;
        let file = ctx.file(fd).map_err(|e| e as isize as i32)?;
        Ok(self.open_of(&file))
    }

    fn flush(&self, ino: u64, off: u64, len: u64) {
        if let Some(inode) = self.inode_of(ino) {
            cache_flush(&inode, off as usize, len as usize);
        }
    }

    fn reload(&self, ino: u64, off: u64, len: u64) {
        if let Some(inode) = self.inode_of(ino) {
            cache_reload(&inode, off as usize, len as usize);
        }
    }

    fn dir_changed(&self, ino: u64) {
        self.target.fs.invalidate_dir(ino);
    }

    fn all_dirs_changed(&self) {
        self.target.fs.invalidate_all_dirs();
    }

    /// The device at `path` as a block device for the volume.
    fn device(&self, path: &str) -> Result<Arc<dyn eclipsefs::BlockDevice>, shared::Errno> {
        let open = || -> LxResult<Arc<dyn eclipsefs::BlockDevice>> {
            let ctx = self.ctx.ok_or(LxError::EPERM)?;
            let inode = ctx.lookup(path)?;
            let backend = MountBackend::from_inode(inode).map_err(|_| LxError::ENOTBLK)?;
            if !matches!(backend, MountBackend::Block(_)) {
                return Err(LxError::ENOTBLK);
            }
            let size = super::block_mount::backend_size(&backend)?;
            let device = super::block_mount::device_from_backend(&backend)?;
            Ok(Arc::new(DevAdapter::new(device, size)))
        };
        open().map_err(|e| e as isize as i32)
    }
}

impl<'a> Target<'a> {
    pub(crate) fn of(file: &'a File) -> Option<Self> {
        let (fs, ino) = efs_of(&file.inode())?;
        Some(Self { file, fs, ino })
    }

    fn caller(&'a self, ctx: Option<&'a dyn FsIoctlContext>) -> Kernel<'a> {
        Kernel {
            target: self,
            ctx,
            files: RefCell::new(Vec::new()),
        }
    }

    /// Copy the argument in from user memory (as its size in `cmd` says),
    /// run the ioctl, and copy it back out for the ones that return data.
    fn user_ioctl(&self, cmd: u32, arg: usize, ctx: &dyn FsIoctlContext) -> LxResult<usize> {
        if cmd == FS_IOC_FIEMAP {
            // The header goes back to the caller even when the call fails
            // (the flags it did not understand, for EBADR).
            let head = UserInPtr::<u8>::from(arg).read_array(FIEMAP_HEADER)?;
            let count = (get32(&head, 24) as usize).min(FIEMAP_MAX_EXTENTS);
            // Fallible: count from userspace can ask for ~3.5 MiB
            // (`FIEMAP_MAX_EXTENTS` × 56). `vec![0; n]` panicked the machine.
            let need = FIEMAP_HEADER + FIEMAP_EXTENT * count;
            let mut buf = alloc::vec::Vec::new();
            buf.try_reserve_exact(need).map_err(|_| LxError::ENOMEM)?;
            buf.resize(need, 0);
            buf[..FIEMAP_HEADER].copy_from_slice(&head);
            let ret = self.fiemap(&mut buf);
            let mapped = get32(&buf, 20) as usize;
            let out = FIEMAP_HEADER + FIEMAP_EXTENT * mapped.min(count);
            UserOutPtr::<u8>::from(arg).write_array(&buf[..out])?;
            return ret;
        }
        let mut buf = copy_in(cmd, arg)?;
        let ret = self.ioctl(cmd, &mut buf, ctx);
        match ret {
            Ok(n) => copy_out(cmd, arg, &mut buf).map(|_| n),
            Err(e) => {
                buf.fill(0);
                Err(e)
            }
        }
    }

    /// `FS_IOC_FIEMAP` on `buf` (a `struct fiemap` with room for
    /// `fm_extent_count` extents): fills `fm_mapped_extents` and the
    /// extents, or with no room only counts them.
    pub(crate) fn fiemap(&self, buf: &mut [u8]) -> LxResult<usize> {
        shared::fiemap(&self.caller(None), self.ino, buf).map_err(lx)
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

    fn admin(&self, ctx: &dyn FsIoctlContext) -> LxResult<()> {
        if ctx.capable(CAP_SYS_ADMIN) {
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
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
            BTRFS_IOC_SEND => self.send(buf, ctx).map(|_| 0),
            EFS_IOC_RECEIVE => self.receive(buf, ctx).map(|_| 0),
            _ => shared::ioctl(&self.caller(Some(ctx)), self.ino, cmd, buf).map_err(lx),
        }
    }

    /// `BTRFS_IOC_SEND` of this read-only snapshot into a regular file.
    fn send(&self, buf: &mut [u8], ctx: &dyn FsIoctlContext) -> LxResult<()> {
        self.admin(ctx)?;
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
        self.admin(ctx)?;
        if buf[8..32].iter().any(|&b| b != 0) {
            return Err(LxError::EINVAL);
        }
        self.want_write()?;
        shared::may_change_dir(&self.caller(Some(ctx)), self.ino).map_err(lx)?;
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

/// The `*xattr` syscalls on a file of an EclipseFS mount, the kernel's or
/// a served one (`None` for any other filesystem). `path` is where the
/// file is, for the read-only test of its mount; `euid` and `admin` stand
/// for the caller's credentials.
pub fn eclipsefs_xattr(
    inode: &Arc<dyn INode>,
    path: &str,
    req: XattrRequest<'_>,
    euid: u32,
    admin: bool,
) -> Option<LxResult<XattrReply>> {
    let Some((fs, ino)) = efs_of(inode) else {
        return super::served::xattr(inode, path, req, euid, admin);
    };
    let read_only = fs.read_only || super::path_is_read_only(path);
    let mut efs = fs.inner.lock();
    Some(shared::xattr(&mut efs, ino, req, read_only, euid, admin).map_err(lx))
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
    let Some(target) = Target::of(file) else {
        return super::served::fallocate(file, mode, offset, len);
    };
    Some((|| {
        target.want_write()?;
        target.with(|fs| fs.fallocate(target.ino, mode, offset, len))?;
        if mode & FALLOC_ZEROES != 0 {
            cache_reload(&file.inode(), offset as usize, len as usize);
        }
        Ok(())
    })())
}

/// `lseek(SEEK_DATA)` (3) or `lseek(SEEK_HOLE)` (4) on a file of an
/// EclipseFS mount: the new position, from the file's real extents.
/// `None` when `file` is on another filesystem.
pub fn eclipsefs_seek(file: &File, whence: usize, offset: i64) -> Option<LxResult<u64>> {
    let Some(target) = Target::of(file) else {
        return super::served::seek(file, whence, offset);
    };
    if let Ok(st) = target.with(|fs| fs.stat(target.ino)) {
        cache_flush(&file.inode(), 0, st.size as usize);
    }
    let found = shared::seek(&mut target.fs.inner.lock(), target.ino, whence, offset);
    Some(found.map_err(lx))
}

/// `copy_file_range(2)` as a reflink, when both files are on the same
/// EclipseFS mount and the offsets are block-aligned (as btrfs does it).
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
    let Some(target) = Target::of(dst) else {
        return super::served::copy_reflink(src, src_off, dst, dst_off, len);
    };
    let caller = target.caller(None);
    // The source as an open file the caller names, kept to flush it by.
    let src = caller.open_of(src);
    shared::copy_reflink(&caller, &src, src_off, target.ino, dst_off, len).map(Ok)
}

#[cfg(test)]
mod tests;
