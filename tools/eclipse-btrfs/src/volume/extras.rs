//! What a volume answers past files and directories, when its filesystem
//! does ([`Tree::EXTRAS`]): ioctls, extended attributes, `fallocate` and
//! `lseek(SEEK_DATA/SEEK_HOLE)`, as the kernel asks for them
//! ([`fatserve::wire::OP_IOCTL`] and after). What changes the volume is
//! written down like any other change, and made again after a takeover.

use super::*;
use crate::tree::Ask;
use eclipsefs::ioctl::{
    arg_size, changes_volume, XattrReply, XattrRequest, BTRFS_IOC_SYNC, BTRFS_IOC_WAIT_SYNC,
};
use fatserve::wire::{
    CHANGED_ALL_DIRS, CHANGED_DIR, FACT_ADMIN, FACT_READ_ONLY, IOCTL_HEAD, NAMED_BASE,
    NAMED_ELSEWHERE, XATTR_GET, XATTR_LIST, XATTR_REMOVE, XATTR_SET,
};

impl<F: Tree, B: Blocks + Send + 'static> Volume<F, B> {
    /// What the kernel keeps of the paths under `path` may be untrue now.
    fn forget_under(&mut self, path: &str) {
        self.listing = None;
        self.paths.retain(|p, _| !under(p, path));
    }

    /// Ioctl on the file the first of `names` is, with the open files the
    /// rest are; `data` its [`IOCTL_HEAD`] and argument. The argument as
    /// it goes back goes in `out`: what the ioctl returns, the `CHANGED_*`
    /// bits, and the bytes of `out`.
    pub fn ioctl(
        &mut self,
        names: &str,
        data: &[u8],
        out: &mut [u8],
    ) -> Result<(usize, u64, usize), Errno> {
        if !F::EXTRAS {
            return Err(libc::ENOTTY);
        }
        let word = |i: usize| {
            data.get(i * 4..i * 4 + 4)
                .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                .ok_or(libc::EINVAL)
        };
        let (cmd, euid, facts, len) = (word(0)?, word(1)?, word(2)?, word(3)? as usize);
        let arg = data
            .get(IOCTL_HEAD..IOCTL_HEAD.saturating_add(len))
            .ok_or(libc::EINVAL)?;
        let mut parts = names.split('\0');
        let path = parts.next().unwrap_or("");
        let named: Vec<(u8, &str)> = parts
            .map(|p| match p.as_bytes().first() {
                Some(&bits) if bits & 0xf0 == NAMED_BASE => (bits, &p[1..]),
                _ => (NAMED_BASE | NAMED_ELSEWHERE, ""),
            })
            .collect();
        // The kernel copied what the number says, or what the header of
        // the argument says it holds; the code reads no further than the
        // number in any case.
        let mut buf = arg.to_vec();
        if buf.len() < arg_size(cmd) {
            buf.resize(arg_size(cmd), 0);
        }
        if matches!(cmd, BTRFS_IOC_SYNC | BTRFS_IOC_WAIT_SYNC) {
            // A commit, as `sync` is one.
            self.sync()?;
            return Ok((0, 0, 0));
        }
        let run = |v: &mut Self, buf: &mut [u8]| -> Result<(usize, u64), Errno> {
            let (ino, _) = v.node(path)?;
            let named = named
                .iter()
                .map(|&(bits, p)| {
                    if bits & NAMED_ELSEWHERE != 0 {
                        Ok((None, bits))
                    } else {
                        v.node(p).map(|(ino, _)| (Some(ino), bits))
                    }
                })
                .collect();
            let ask = Ask { euid, facts, named };
            v.fs.ioctl(ino, cmd, buf, &ask)
        };
        let done = if changes_volume(cmd) {
            self.change(
                (OP_IOCTL, names, 0, data.len() as u32, data),
                &[path],
                |v| run(v, &mut buf),
            )
        } else {
            // What the code reads is what was written: what is kept of a
            // file being written goes to it first.
            self.write_tail()?;
            run(self, &mut buf)
        };
        let (ret, changed) = done?;
        if changed & CHANGED_DIR != 0 {
            self.forget_under(path);
        }
        if changed & CHANGED_ALL_DIRS != 0 {
            self.forget_under("");
        }
        let n = len.min(out.len()).min(buf.len());
        out[..n].copy_from_slice(&buf[..n]);
        buf.fill(0);
        Ok((ret, changed, n))
    }

    /// An extended attribute: `names` is the file's path, a NUL and the
    /// attribute's name; `a` the request, its flags and `FACT_*` bits as
    /// [`fatserve::wire::OP_XATTR`] says, with the caller's `euid`;
    /// `value` what to set. The value or the names go in `out`, and their
    /// bytes are the answer.
    pub fn xattr(
        &mut self,
        names: &str,
        a: u64,
        value: &[u8],
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        if !F::EXTRAS {
            return Err(libc::EOPNOTSUPP);
        }
        let (path, name) = names.split_once('\0').ok_or(libc::EINVAL)?;
        let flags = (a >> 8) as u32 & 0xff;
        let facts = (a >> 16) as u32 & 0xffff;
        let euid = (a >> 32) as u32;
        let req = match a & 0xff {
            XATTR_GET => XattrRequest::Get(name),
            XATTR_SET => XattrRequest::Set { name, value, flags },
            XATTR_LIST => XattrRequest::List,
            XATTR_REMOVE => XattrRequest::Remove(name),
            _ => return Err(libc::EINVAL),
        };
        let read_only = self.read_only || facts & FACT_READ_ONLY != 0;
        let admin = facts & FACT_ADMIN != 0;
        let run = |v: &mut Self| {
            let (ino, _) = v.node(path)?;
            v.fs.xattr(ino, req, read_only, euid, admin)
        };
        let reply = if req.changes() && !read_only {
            self.change((OP_XATTR, names, a, value.len() as u32, value), &[], run)
        } else {
            self.write_tail()?;
            run(self)
        }?;
        match reply {
            XattrReply::Value(bytes) | XattrReply::Names(bytes) => {
                if bytes.len() > out.len() {
                    return Err(libc::E2BIG);
                }
                out[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            XattrReply::Done => Ok(0),
        }
    }

    /// `fallocate(2)` of `path` from `offset` with `mode`, the length a
    /// u64 in `data`.
    pub fn fallocate(
        &mut self,
        path: &str,
        offset: u64,
        mode: u32,
        data: &[u8],
    ) -> Result<(), Errno> {
        if !F::EXTRAS {
            return Err(libc::EOPNOTSUPP);
        }
        let len = data
            .get(..8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
            .ok_or(libc::EINVAL)?;
        self.change((OP_FALLOCATE, path, offset, mode, &data[..8]), &[], |v| {
            let (ino, _) = v.node(path)?;
            v.fs.fallocate(ino, mode, offset, len)
        })
    }

    /// `lseek(2)` of `path` to `SEEK_DATA` or `SEEK_HOLE` (`whence`) from
    /// `offset`: the new position.
    pub fn seek(&mut self, path: &str, offset: i64, whence: u32) -> Result<u64, Errno> {
        if !F::EXTRAS {
            return Err(libc::EOPNOTSUPP);
        }
        if self.tail.is_of(path) {
            self.write_tail()?;
        }
        let (ino, _) = self.node(path)?;
        self.fs.seek(ino, whence as usize, offset)
    }
}
