//! What a server of [`SERVE_EXTRAS`] answers past files and directories:
//! `ioctl(2)`, the extended attributes, `fallocate(2)`, `lseek(2)` to data
//! or a hole, and `copy_file_range(2)` as a reflink. The server runs them
//! (`eclipsefs::ioctl`, as the kernel's own EclipseFS mount does); what
//! is the kernel's is here: the argument's copy in and out of user memory,
//! the open files it names (sent by their paths), the page cache, and what
//! the kernel keeps that the change made untrue (a file's size, a
//! directory's listing).
//!
//! `BTRFS_IOC_SEND` and `EFS_IOC_RECEIVE` move a stream through another
//! open file, and the devices of a volume are named by device files: none
//! of it is the server's to reach, and it answers `EOPNOTSUPP`.

use super::*;
use crate::fs::eclipsefs_ioctl::{copy_in, copy_out, FsIoctlContext, XattrReply, XattrRequest};
use crate::fs::file::{cache_flush, cache_reload};
use crate::fs::{path_is_read_only, File, FileLike};
use eclipsefs::ioctl::{
    changes_volume, get16, get32, get64, put64, BTRFS_IOC_DEFRAG, BTRFS_IOC_DEFRAG_RANGE,
    BTRFS_IOC_SEND, BTRFS_IOC_SNAP_CREATE, BTRFS_IOC_SNAP_CREATE_V2, CAP_FOWNER,
    CAP_LINUX_IMMUTABLE, CAP_SYS_ADMIN, EFS_IOC_RECEIVE, FALLOC_ZEROES, FICLONE, FICLONERANGE,
    FIDEDUPERANGE, FIEMAP_EXTENT, FIEMAP_HEADER, FIEMAP_MAX_EXTENTS, FS_IOC_ADD_ENCRYPTION_KEY,
    FS_IOC_FIEMAP, FS_IOC_REMOVE_ENCRYPTION_KEY, FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS,
};
use kernel_hal::user::{UserInPtr, UserOutPtr};
use wire::NAME_AREA;
use wire::{
    CHANGED_ALL_DIRS, CHANGED_BYTES, CHANGED_DIR, CHANGED_NAMED, FACT_ADMIN, FACT_APPEND,
    FACT_FOWNER, FACT_IMMUTABLE, FACT_READABLE, FACT_READ_ONLY, FACT_WRITABLE, IOCTL_HEAD,
    NAMED_APPEND, NAMED_BASE, NAMED_ELSEWHERE, NAMED_READABLE, NAMED_WRITABLE, OP_FALLOCATE,
    OP_IOCTL, OP_SEEK, OP_XATTR, XATTR_GET, XATTR_LIST, XATTR_REMOVE, XATTR_SET,
};

const BLOCK: u64 = 4096;

/// The error a request ended with, as the server said it.
fn lx(fail: Fail) -> LxError {
    match fail {
        Fail::Errno(n) => LxError::from_errno(n),
        other => LxError::from(FsError::from(other)),
    }
}

/// The served file behind `inode`, on a mount whose server answers these.
fn served_of(inode: &Arc<dyn INode>) -> Option<&ServedINode> {
    (**inode)
        .downcast_ref::<ServedINode>()
        .filter(|i| i.fs.extras)
}

/// The `FACT_*` bits of the open file `file` and what `ctx` may do.
fn facts(fs: &ServedFs, file: &File, ctx: Option<&dyn FsIoctlContext>) -> u32 {
    let flags = file.flags();
    let cap = |cap: u32, bit: u32| match ctx {
        Some(ctx) if ctx.capable(cap) => bit,
        _ => 0,
    };
    let on = |yes: bool, bit: u32| if yes { bit } else { 0 };
    cap(CAP_FOWNER, FACT_FOWNER)
        | cap(CAP_LINUX_IMMUTABLE, FACT_IMMUTABLE)
        | cap(CAP_SYS_ADMIN, FACT_ADMIN)
        | on(flags.readable(), FACT_READABLE)
        | on(flags.writable(), FACT_WRITABLE)
        | on(flags.is_append(), FACT_APPEND)
        | on(
            fs.read_only || path_is_read_only(file.path()),
            FACT_READ_ONLY,
        )
}

/// Where in an ioctl's argument its descriptors are, and their bytes.
fn descriptors(cmd: u32, buf: &[u8]) -> Vec<(usize, usize)> {
    match cmd {
        FICLONE => alloc::vec![(0, 4)],
        FICLONERANGE | BTRFS_IOC_SNAP_CREATE | BTRFS_IOC_SNAP_CREATE_V2 => alloc::vec![(0, 8)],
        FIDEDUPERANGE => {
            let count = get16(buf, 16) as usize;
            (0..count)
                .map(|i| (24 + 32 * i, 8))
                .filter(|(at, _)| at + 8 <= buf.len())
                .collect()
        }
        _ => Vec::new(),
    }
}

/// An open file an ioctl named, as it went to the server.
struct Named {
    file: Arc<File>,
    /// Its node, when it is a file of this same mount.
    node: Option<Arc<Node>>,
}

impl ServedINode {
    /// What the server says of this file's size and times now, after a
    /// change it made underneath the kernel.
    fn refresh(&self, node: &Arc<Node>) {
        let Ok(path) = node.live_path() else {
            return;
        };
        if let Ok(entry) = self
            .fs
            .call(OP_STAT, &path, 0, 0)
            .and_then(|a| self.fs.entry_of(a))
        {
            if !node.is_dir {
                node.size.store(entry.size, Ordering::Relaxed);
            }
            node.mtime.store(entry.mtime, Ordering::Relaxed);
            node.ctime.store(entry.ctime, Ordering::Relaxed);
        }
        self.fs.info_stale.store(true, Ordering::Release);
    }

    /// The pages kept of `file` (of node `node`) are loaded again: the
    /// server changed its bytes underneath them.
    fn reload(&self, file: &Arc<dyn INode>, node: &Arc<Node>) {
        self.refresh(node);
        let size = node.size.load(Ordering::Relaxed) as usize;
        cache_reload(file, 0, size);
    }

    /// Ask the server ioctl `cmd` on this file with `names` (the path,
    /// then the named files) and the argument in `buf`, which gets what
    /// goes back: what the ioctl returns, and the `CHANGED_*` bits.
    fn ask_ioctl(
        &self,
        cmd: u32,
        euid: u32,
        facts: u32,
        names: &[u8],
        buf: &mut [u8],
    ) -> LxResult<(usize, u64)> {
        if names.len() > NAME_AREA {
            return Err(LxError::ENAMETOOLONG);
        }
        if IOCTL_HEAD + buf.len() > DATA_AREA {
            return Err(LxError::ENOMEM);
        }
        let mut data = Vec::with_capacity(IOCTL_HEAD + buf.len());
        for word in [cmd, euid, facts, buf.len() as u32] {
            data.extend_from_slice(&word.to_le_bytes());
        }
        data.extend_from_slice(buf);
        let answer = self
            .fs
            .ask_errno(OP_IOCTL, names, 0, data.len() as u32, &data)
            .0
            .map_err(lx)?;
        let back = answer.len.min(buf.len());
        answer.data(0, &mut buf[..back])?;
        // An argument wiped of a raw key: the window is wiped of it too.
        data.fill(0);
        Ok((answer.count, answer.a))
    }

    /// `ioctl(2)` on `file`, this inode, with its argument at `arg`.
    fn user_ioctl(
        &self,
        file: &File,
        cmd: u32,
        arg: usize,
        ctx: &dyn FsIoctlContext,
    ) -> LxResult<usize> {
        if cmd == BTRFS_IOC_SEND || cmd == EFS_IOC_RECEIVE {
            return Err(LxError::EOPNOTSUPP);
        }
        let path = self.node.live_path()?;
        let facts = facts(&self.fs, file, Some(ctx));
        let euid = ctx.euid();
        let inode = file.inode();
        if cmd == FS_IOC_FIEMAP {
            // As many extents as the data area holds; `filefrag` asks again
            // from the last one.
            let head = UserInPtr::<u8>::from(arg).read_array(FIEMAP_HEADER)?;
            let room = (DATA_AREA - IOCTL_HEAD - FIEMAP_HEADER) / FIEMAP_EXTENT;
            let count = (get32(&head, 24) as usize)
                .min(FIEMAP_MAX_EXTENTS)
                .min(room);
            let mut buf = alloc::vec![0u8; FIEMAP_HEADER + FIEMAP_EXTENT * count];
            buf[..FIEMAP_HEADER].copy_from_slice(&head);
            cache_flush(&inode, 0, self.node.size.load(Ordering::Relaxed) as usize);
            let ret = self.ask_ioctl(cmd, euid, facts, path.as_bytes(), &mut buf);
            let mapped = get32(&buf, 20) as usize;
            let out = FIEMAP_HEADER + FIEMAP_EXTENT * mapped.min(count);
            UserOutPtr::<u8>::from(arg).write_array(&buf[..out])?;
            return ret.map(|(n, _)| n);
        }
        let mut buf = copy_in(cmd, arg)?;
        // The descriptors the argument names: by their paths, if they are
        // files of this mount.
        let mut names = path.clone().into_bytes();
        let mut named: Vec<Named> = Vec::new();
        for (at, len) in descriptors(cmd, &buf) {
            let fd = if len == 4 {
                get32(&buf, at) as i32
            } else {
                get64(&buf, at) as i64 as i32
            };
            let index: i64 = match ctx.file(fd) {
                Err(_) => -1,
                Ok(other) => {
                    let other_inode = other.inode();
                    let node = (*other_inode)
                        .downcast_ref::<ServedINode>()
                        .filter(|i| Arc::ptr_eq(&i.fs, &self.fs))
                        .map(|i| i.node.clone())
                        .filter(|n| !n.is_gone());
                    let flags = other.flags();
                    let mut byte = NAMED_BASE;
                    for (yes, bit) in [
                        (flags.readable(), NAMED_READABLE),
                        (flags.writable(), NAMED_WRITABLE),
                        (flags.is_append(), NAMED_APPEND),
                        (node.is_none(), NAMED_ELSEWHERE),
                    ] {
                        if yes {
                            byte |= bit;
                        }
                    }
                    names.push(0);
                    names.push(byte);
                    if let Some(node) = &node {
                        names.extend_from_slice(node.path().as_bytes());
                    }
                    named.push(Named { file: other, node });
                    named.len() as i64 - 1
                }
            };
            if len == 4 {
                buf[at..at + 4].copy_from_slice(&(index as i32).to_le_bytes());
            } else {
                put64(&mut buf, at, index as u64);
            }
        }
        // What the server reads on its own goes back to it first: what a
        // shared mapping changed of the source of a reflink, of the ranges
        // to deduplicate, of a file to defragment.
        match cmd {
            FICLONE | FICLONERANGE => {
                let (off, len) = if cmd == FICLONE {
                    (0, usize::MAX)
                } else {
                    (get64(&buf, 8) as usize, get64(&buf, 16) as usize)
                };
                let len = if len == 0 { usize::MAX } else { len };
                if let Some(src) = named.first() {
                    cache_flush(&src.file.inode(), off, len);
                }
            }
            FIDEDUPERANGE => {
                let len = get64(&buf, 8) as usize;
                cache_flush(&inode, get64(&buf, 0) as usize, len);
                for (i, n) in named.iter().enumerate() {
                    let at = 24 + 32 * i + 8;
                    if at + 8 <= buf.len() {
                        cache_flush(&n.file.inode(), get64(&buf, at) as usize, len);
                    }
                }
            }
            BTRFS_IOC_DEFRAG | BTRFS_IOC_DEFRAG_RANGE => {
                cache_flush(&inode, 0, self.node.size.load(Ordering::Relaxed) as usize);
            }
            _ => {}
        }
        let changes = changes_volume(cmd)
            || matches!(
                cmd,
                FS_IOC_ADD_ENCRYPTION_KEY
                    | FS_IOC_REMOVE_ENCRYPTION_KEY
                    | FS_IOC_REMOVE_ENCRYPTION_KEY_ALL_USERS
            );
        let answered = {
            // Nothing read meanwhile is kept: it may be from before.
            let _bytes = changes.then(|| Changing::new(&self.node));
            let _named: Vec<Changing<'_>> = named
                .iter()
                .filter(|_| changes)
                .filter_map(|n| n.node.as_deref())
                .map(Changing::new)
                .collect();
            let _dirs = changes.then(|| self.fs.changing_dirs());
            let answered = self.ask_ioctl(cmd, euid, facts, &names, &mut buf);
            if let Ok((_, changed)) = answered {
                if changed & CHANGED_DIR != 0 {
                    self.fs.dirs.lock().listed.remove(&path);
                }
                if changed & CHANGED_ALL_DIRS != 0 {
                    self.fs.dirs.lock().listed.clear();
                }
            }
            answered
        };
        let (ret, changed) = match answered {
            Ok(a) => a,
            Err(e) => {
                buf.fill(0);
                return Err(e);
            }
        };
        if changed & CHANGED_BYTES != 0 {
            self.reload(&inode, &self.node);
        }
        if changed & CHANGED_NAMED != 0 {
            for n in &named {
                if let Some(node) = &n.node {
                    self.reload(&n.file.inode(), node);
                }
            }
        }
        copy_out(cmd, arg, &mut buf)?;
        Ok(ret)
    }
}

/// `ioctl(2)` on a file of a served mount that answers them: `None` for
/// any other file.
pub(crate) fn ioctl(
    file: &File,
    cmd: u32,
    arg: usize,
    ctx: &dyn FsIoctlContext,
) -> Option<LxResult<usize>> {
    let inode = file.inode();
    let served = served_of(&inode)?;
    Some(served.user_ioctl(file, cmd, arg, ctx))
}

/// The `*xattr` syscalls on a file of a served mount that keeps them:
/// `None` for any other file. `path` is where the file is, for the
/// read-only test of its mount.
pub(crate) fn xattr(
    inode: &Arc<dyn INode>,
    path: &str,
    req: XattrRequest<'_>,
    euid: u32,
    admin: bool,
) -> Option<LxResult<XattrReply>> {
    let served = served_of(inode)?;
    Some((|| {
        let file = served.node.live_path()?;
        let (kind, name, value, flags) = match req {
            XattrRequest::Get(name) => (XATTR_GET, name, &[][..], 0),
            XattrRequest::Set { name, value, flags } => (XATTR_SET, name, value, flags),
            XattrRequest::List => (XATTR_LIST, "", &[][..], 0),
            XattrRequest::Remove(name) => (XATTR_REMOVE, name, &[][..], 0),
        };
        if value.len() > DATA_AREA {
            return Err(LxError::E2BIG);
        }
        let mut names = file.into_bytes();
        names.push(0);
        names.extend_from_slice(name.as_bytes());
        let read_only = served.fs.read_only || path_is_read_only(path);
        let facts = if read_only { FACT_READ_ONLY } else { 0 } | if admin { FACT_ADMIN } else { 0 };
        let a = kind
            | ((flags & 0xff) as u64) << 8
            | (facts as u64 & 0xffff) << 16
            | (euid as u64) << 32;
        let answer = served
            .fs
            .ask_errno(OP_XATTR, &names, a, value.len() as u32, value)
            .0
            .map_err(lx)?;
        let mut bytes = alloc::vec![0u8; answer.len.min(answer.count)];
        answer.data(0, &mut bytes)?;
        Ok(match req {
            XattrRequest::Get(_) => XattrReply::Value(bytes),
            XattrRequest::List => XattrReply::Names(bytes),
            _ => XattrReply::Done,
        })
    })())
}

/// `fallocate(2)` on a file of a served mount that does it: `None` for
/// any other file.
pub(crate) fn fallocate(file: &File, mode: u32, offset: u64, len: u64) -> Option<LxResult<()>> {
    let inode = file.inode();
    let served = served_of(&inode)?;
    Some((|| {
        if served.fs.read_only || path_is_read_only(file.path()) {
            return Err(LxError::EROFS);
        }
        let path = served.node.live_path()?;
        {
            let _changing = Changing::new(&served.node);
            served
                .fs
                .ask_errno(
                    OP_FALLOCATE,
                    path.as_bytes(),
                    offset,
                    mode,
                    &len.to_le_bytes(),
                )
                .0
                .map_err(lx)?;
        }
        served.refresh(&served.node);
        if mode & FALLOC_ZEROES != 0 {
            cache_reload(&inode, offset as usize, len as usize);
        }
        Ok(())
    })())
}

/// `lseek(SEEK_DATA)` or `lseek(SEEK_HOLE)` on a file of a served mount
/// that knows its extents: `None` for any other file. What a shared
/// mapping changed and was not written back is not asked about: a write
/// is the server's when it returns, and only such a store is not.
pub(crate) fn seek(file: &File, whence: usize, offset: i64) -> Option<LxResult<u64>> {
    let inode = file.inode();
    let served = served_of(&inode)?;
    Some((|| {
        let path = served.node.live_path()?;
        let answer = served
            .fs
            .ask_errno(OP_SEEK, path.as_bytes(), offset as u64, whence as u32, &[])
            .0
            .map_err(lx)?;
        Ok(answer.count as u64)
    })())
}

/// `copy_file_range(2)` as a reflink between two files of the same served
/// mount, when the offsets are block-aligned: a `FICLONERANGE` of whole
/// blocks, or up to the end of `src`. `None` when a reflink does not
/// apply, or the server would not make it, and then the caller copies the
/// bytes.
pub(crate) fn copy_reflink(
    src: &File,
    src_off: u64,
    dst: &File,
    dst_off: u64,
    len: u64,
) -> Option<LxResult<u64>> {
    let dst_inode = dst.inode();
    let d = served_of(&dst_inode)?;
    let src_inode = src.inode();
    let s = (*src_inode)
        .downcast_ref::<ServedINode>()
        .filter(|s| Arc::ptr_eq(&s.fs, &d.fs))?;
    let regular = |n: &Node| n.mode.load(Ordering::Relaxed) & MODE_KIND == MODE_FILE;
    if len == 0
        || dst.flags().is_append()
        || d.fs.read_only
        || path_is_read_only(dst.path())
        || !regular(&s.node)
        || !regular(&d.node)
    {
        return None;
    }
    let src_size = s.node.size.load(Ordering::Relaxed);
    if src_off >= src_size || !src_off.is_multiple_of(BLOCK) || !dst_off.is_multiple_of(BLOCK) {
        return None;
    }
    let mut n = len.min(src_size - src_off);
    if src_off + n != src_size {
        n -= n % BLOCK;
    }
    if n == 0 || (Arc::ptr_eq(&s.node, &d.node) && src_off < dst_off + n && dst_off < src_off + n) {
        return None;
    }
    let (Ok(dst_path), Ok(src_path)) = (d.node.live_path(), s.node.live_path()) else {
        return None;
    };
    cache_flush(&src_inode, src_off as usize, n as usize);
    let mut names = dst_path.into_bytes();
    names.push(0);
    names.push(NAMED_BASE | NAMED_READABLE);
    names.extend_from_slice(src_path.as_bytes());
    let mut buf = [0u8; 32];
    put64(&mut buf, 0, 0);
    put64(&mut buf, 8, src_off);
    put64(&mut buf, 16, n);
    put64(&mut buf, 24, dst_off);
    // The caller's credentials are not asked about: a clone asks for none.
    let facts = facts(&d.fs, dst, None);
    let answered = {
        let _changing = Changing::new(&d.node);
        d.ask_ioctl(FICLONERANGE, 0, facts, &names, &mut buf)
    };
    match answered {
        Ok(_) => {
            d.reload(&dst_inode, &d.node);
            Some(Ok(n))
        }
        Err(_) => None,
    }
}
