//! The export, asked by path: what each request of the kernel is in NFS
//! calls.
//!
//! The kernel names files by their path under the mount (`""` is the top,
//! `"a/b"` a file in a directory); NFS names them by handle. So this keeps
//! the handle of every path it has looked up, and looks a path up again,
//! once, when the server says its handle went stale (`ESTALE`): a file
//! another client renamed over, or a server that restarted with handles
//! that do not survive it.
//!
//! What NFSv3 cannot say as the kernel asks it:
//!
//! - A symbolic link is made with its target, and the kernel makes the
//!   link first and writes the target into it after. So a new link is
//!   held here until its target is written, and a target written to a
//!   link already on the server makes it again.
//! - Writes are `UNSTABLE`, as Linux's are, and committed when the kernel
//!   asks for a sync and a second after the last write
//!   ([`Export::commit`]).
//! - Every call is made as root (`AUTH_UNIX` uid 0), as the kernel's own
//!   NFS makes them: the kernel checks the permissions it was given, and an
//!   export that squashes root answers with what `nobody` may do.

use std::collections::{HashMap, HashSet};

use fatserve::wire::LIST_END;
use nfs3::client::RpcTransport;
use nfs3::errno::Errno;
use nfs3::mount3::FileHandle;
use nfs3::nfs3::{Fattr3, Ftype3};
use nfs3::nfs3_write::{CreateHow, NodeKind, SetAttr, SetTime, StableHow};
use nfs3::session::NfsSession;
use nfs3::Timespec;

/// A Linux `errno`, as the kernel is answered with one.
pub type Error = i32;

/// What is answered: a value or an `errno`.
pub type Answer<T> = Result<T, Error>;

/// The magic `statfs` answers for an NFS mount (`NFS_SUPER_MAGIC`).
pub const NFS_MAGIC: u64 = 0x6969;

/// A record's fixed part on a POSIX mount: what the kernel's
/// `served::wire::RECORD_HEAD_POSIX` says.
pub const RECORD_HEAD: usize = 64;

/// What `stat` of the top of the mount is called in its record, which
/// needs a name: the kernel takes nothing from it.
const TOP_NAME: &str = "top";

/// The block size `statfs` counts in.
const BLOCK: u64 = 4096;

/// The kind bits of a mode.
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
const S_IFIFO: u32 = 0o010000;
const S_IFSOCK: u32 = 0o140000;
const S_IFCHR: u32 = 0o020000;
const S_IFBLK: u32 = 0o060000;

fn errno(e: Errno) -> Error {
    e.0
}

/// A path's directory and its name in it.
fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// `path` is `dir` or under it.
fn under(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The bytes the record of `name` takes.
pub fn record_len(name: &str) -> usize {
    (RECORD_HEAD + name.len()).next_multiple_of(8)
}

/// The mode of what `attr` describes, kind and permissions.
pub fn mode_of(attr: &Fattr3) -> u32 {
    let kind = match attr.ftype {
        Ftype3::Reg => S_IFREG,
        Ftype3::Dir => S_IFDIR,
        Ftype3::Lnk => S_IFLNK,
        Ftype3::Fifo => S_IFIFO,
        Ftype3::Sock => S_IFSOCK,
        Ftype3::Chr => S_IFCHR,
        Ftype3::Blk => S_IFBLK,
    };
    kind | attr.mode & 0o7777
}

/// Write the record of `attr` called `name` at the start of `out`, which
/// holds it.
pub fn put_record(name: &str, attr: &Fattr3, out: &mut [u8]) {
    let len = record_len(name);
    out[..len].fill(0);
    // Linux's `new_encode_dev`.
    let (major, minor) = (attr.rdev.0 as u64, attr.rdev.1 as u64);
    let rdev = (major << 8) | (minor & 0xff) | ((minor & !0xff) << 12);
    out[0..2].copy_from_slice(&(len as u16).to_ne_bytes());
    out[2..4].copy_from_slice(&(name.len() as u16).to_ne_bytes());
    out[4..8].copy_from_slice(&mode_of(attr).to_ne_bytes());
    out[8..16].copy_from_slice(&attr.size.to_ne_bytes());
    out[16..24].copy_from_slice(&attr.mtime.sec.to_ne_bytes());
    out[24..32].copy_from_slice(&attr.atime.sec.to_ne_bytes());
    out[32..40].copy_from_slice(&attr.ctime.sec.to_ne_bytes());
    out[40..44].copy_from_slice(&attr.uid.to_ne_bytes());
    out[44..48].copy_from_slice(&attr.gid.to_ne_bytes());
    out[48..52].copy_from_slice(&attr.nlink.to_ne_bytes());
    out[56..64].copy_from_slice(&rdev.to_ne_bytes());
    out[RECORD_HEAD..RECORD_HEAD + name.len()].copy_from_slice(name.as_bytes());
}

/// The attributes a link that is not on the server yet is said to have.
fn held_link(target: &[u8], mode: u32) -> Fattr3 {
    let zero = Timespec { sec: 0, nsec: 0 };
    Fattr3 {
        ftype: Ftype3::Lnk,
        mode: mode & 0o7777,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: target.len() as u64,
        used: 0,
        rdev: (0, 0),
        fsid: 0,
        fileid: 0,
        atime: zero,
        mtime: zero,
        ctime: zero,
    }
}

/// A directory's entries, as the kernel is given them a slot at a time.
struct Listed {
    name: String,
    attr: Fattr3,
}

/// One mounted export.
pub struct Export<T: RpcTransport> {
    session: NfsSession<T>,
    /// The handle of each path looked up, and what it is.
    handles: HashMap<String, (FileHandle, Ftype3)>,
    /// The listing being handed out, of the directory it is of.
    listing: Option<(String, Vec<Listed>)>,
    /// Symbolic links made and not on the server yet: their target so far
    /// and their permissions.
    held: HashMap<String, (Vec<u8>, u32)>,
    /// Files written `UNSTABLE` since the last commit.
    written: HashSet<FileHandle>,
    read_only: bool,
}

impl<T: RpcTransport> Export<T> {
    /// The export whose `session` is open, `read_only` or not.
    pub fn new(session: NfsSession<T>, read_only: bool) -> Self {
        let mut handles = HashMap::new();
        handles.insert(String::new(), (session.root().clone(), Ftype3::Dir));
        Self {
            session,
            handles,
            listing: None,
            held: HashMap::new(),
            written: HashSet::new(),
            read_only,
        }
    }

    /// The session, for whoever needs a call this does not make.
    pub fn session(&mut self) -> &mut NfsSession<T> {
        &mut self.session
    }

    /// What the kernel keeps of the paths under `path` may be untrue now.
    fn forget_under(&mut self, path: &str) {
        self.listing = None;
        self.handles.retain(|p, _| p.is_empty() || !under(p, path));
    }

    fn writable(&self) -> Answer<()> {
        if self.read_only {
            Err(libc::EROFS)
        } else {
            Ok(())
        }
    }

    /// The handle of `path`, and what it is.
    fn node(&mut self, path: &str) -> Answer<(FileHandle, Ftype3)> {
        if let Some(found) = self.handles.get(path) {
            return Ok(found.clone());
        }
        let (dir, name) = split(path);
        let (dir_fh, kind) = self.node(dir)?;
        if kind != Ftype3::Dir {
            return Err(libc::ENOTDIR);
        }
        let found = self.session.lookup(&dir_fh, name).map_err(errno)?;
        let kind = match found.attr {
            Some(attr) => attr.ftype,
            None => self.session.getattr(&found.fh).map_err(errno)?.ftype,
        };
        self.handles
            .insert(path.to_string(), (found.fh.clone(), kind));
        Ok((found.fh, kind))
    }

    /// `call` on the handle of `path`, and once more on a fresh one if the
    /// server says the one kept went stale.
    fn on<R>(
        &mut self,
        path: &str,
        mut call: impl FnMut(&mut Self, &FileHandle, Ftype3) -> Answer<R>,
    ) -> Answer<R> {
        let (fh, kind) = self.node(path)?;
        match call(self, &fh, kind) {
            Err(e) if e == libc::ESTALE && !path.is_empty() => {
                self.forget_under(path);
                let (fh, kind) = self.node(path)?;
                call(self, &fh, kind)
            }
            done => done,
        }
    }

    /// The attributes of `path`.
    fn attr(&mut self, path: &str) -> Answer<Fattr3> {
        if let Some((target, mode)) = self.held.get(path) {
            return Ok(held_link(target, *mode));
        }
        self.on(path, |v, fh, _| v.session.getattr(fh).map_err(errno))
    }

    /// The record of `path` itself, called `name`, in `out`: its bytes.
    fn record(&mut self, path: &str, name: &str, out: &mut [u8]) -> Answer<usize> {
        let attr = self.attr(path)?;
        let len = record_len(name);
        if len > out.len() {
            return Err(libc::ENAMETOOLONG);
        }
        put_record(name, &attr, out);
        Ok(len)
    }

    /// `OP_STAT`: the record of `path`.
    pub fn stat(&mut self, path: &str, out: &mut [u8]) -> Answer<usize> {
        let name = match split(path).1 {
            "" => TOP_NAME,
            name => name,
        }
        .to_string();
        self.record(path, &name, out)
    }

    /// `OP_LIST`: the directory `path`'s entries from entry `start` on, as
    /// records in `out`: `(records, bytes, next)`, `next` being `LIST_END`
    /// after the last.
    pub fn list(&mut self, path: &str, start: u64, out: &mut [u8]) -> Answer<(usize, usize, u64)> {
        let fresh = start == 0 || self.listing.as_ref().is_none_or(|(p, _)| p != path);
        if fresh {
            let entries = self.on(path, |v, fh, kind| {
                if kind != Ftype3::Dir {
                    return Err(libc::ENOTDIR);
                }
                v.session.readdir_all(fh, true).map_err(errno)
            })?;
            let mut listed = Vec::with_capacity(entries.len());
            for entry in entries {
                if entry.name == "." || entry.name == ".." {
                    continue;
                }
                let full = if path.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{path}/{}", entry.name)
                };
                let attr = match (entry.attr, entry.fh) {
                    (Some(attr), Some(fh)) => {
                        self.handles.insert(full, (fh, attr.ftype));
                        attr
                    }
                    // A server that did not say: one LOOKUP for it, and an
                    // entry gone meanwhile is left out.
                    _ => match self.attr(&full) {
                        Ok(attr) => attr,
                        Err(libc::ENOENT) => continue,
                        Err(e) => return Err(e),
                    },
                };
                listed.push(Listed {
                    name: entry.name,
                    attr,
                });
            }
            // Links held here are in the directory as far as the kernel
            // knows.
            for (held, (target, mode)) in &self.held {
                let (dir, name) = split(held);
                if dir == path && !listed.iter().any(|l| l.name == name) {
                    listed.push(Listed {
                        name: name.to_string(),
                        attr: held_link(target, *mode),
                    });
                }
            }
            self.listing = Some((path.to_string(), listed));
        }
        let listed = &self.listing.as_ref().expect("just made").1;
        let from = usize::try_from(start)
            .unwrap_or(usize::MAX)
            .min(listed.len());
        let (mut count, mut used) = (0, 0);
        for entry in &listed[from..] {
            let len = record_len(&entry.name);
            if used + len > out.len() {
                return Ok((count, used, (from + count) as u64));
            }
            put_record(&entry.name, &entry.attr, &mut out[used..]);
            used += len;
            count += 1;
        }
        self.listing = None;
        Ok((count, used, LIST_END))
    }

    /// `OP_READ`: up to `out.len()` bytes of `path` from `offset`: the
    /// target of a symbolic link, or a file's bytes.
    pub fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Answer<usize> {
        let target = match self.held.get(path) {
            Some((target, _)) => Some(target.clone()),
            None => self.on(path, |v, fh, kind| match kind {
                Ftype3::Dir => Err(libc::EISDIR),
                Ftype3::Lnk => v
                    .session
                    .readlink(fh)
                    .map(|t| Some(t.into_bytes()))
                    .map_err(errno),
                _ => Ok(None),
            })?,
        };
        match target {
            Some(target) => {
                let from = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(target.len());
                let n = out.len().min(target.len() - from);
                out[..n].copy_from_slice(&target[from..from + n]);
                Ok(n)
            }
            None => self.on(path, |v, fh, _| {
                v.session.read_into(fh, offset, out).map_err(errno)
            }),
        }
    }

    /// `OP_WRITE`: `data` into `path` at `offset`.
    pub fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Answer<usize> {
        self.writable()?;
        let at = usize::try_from(offset).map_err(|_| libc::EFBIG)?;
        let splice = |target: &mut Vec<u8>| {
            if target.len() < at + data.len() {
                target.resize(at + data.len(), 0);
            }
            target[at..at + data.len()].copy_from_slice(data);
        };
        if let Some((mut target, mode)) = self.held.remove(path) {
            splice(&mut target);
            return match self.make_link(path, &target, mode) {
                Ok(()) => Ok(data.len()),
                Err(e) => {
                    self.held.insert(path.to_string(), (target, mode));
                    Err(e)
                }
            };
        }
        let (fh, kind) = self.node(path)?;
        match kind {
            Ftype3::Dir => Err(libc::EISDIR),
            Ftype3::Lnk => {
                // Made again with the target as it is now.
                let mut target = self.session.readlink(&fh).map_err(errno)?.into_bytes();
                let attr = self.session.getattr(&fh).map_err(errno)?;
                splice(&mut target);
                let (dir, name) = split(path);
                let (dir_fh, _) = self.node(dir)?;
                self.session.remove(&dir_fh, name).map_err(errno)?;
                self.handles.remove(path);
                self.make_link(path, &target, attr.mode)?;
                Ok(data.len())
            }
            _ => {
                self.on(path, |v, fh, _| {
                    v.session
                        .write_all(fh, offset, data, StableHow::Unstable)
                        .map_err(errno)
                        .map(|(how, _)| {
                            if how != StableHow::FileSync {
                                v.written.insert(fh.clone());
                            }
                        })
                })?;
                Ok(data.len())
            }
        }
    }

    /// The link `path` to `target` on the server.
    fn make_link(&mut self, path: &str, target: &[u8], mode: u32) -> Answer<()> {
        let (dir, name) = split(path);
        let (dir_fh, _) = self.node(dir)?;
        let attr = SetAttr {
            mode: Some(mode & 0o7777),
            ..SetAttr::nothing()
        };
        let made = self
            .session
            .symlink(&dir_fh, name, target, &attr)
            .map_err(errno)?;
        if let Some(fh) = made.fh {
            self.handles.insert(path.to_string(), (fh, Ftype3::Lnk));
        }
        self.listing = None;
        Ok(())
    }

    /// `OP_CREATE`: make `path` a file of the kind and permissions `mode`
    /// says; its record, called by its name, in `out`.
    pub fn create(&mut self, path: &str, mode: u32, out: &mut [u8]) -> Answer<usize> {
        self.writable()?;
        let (dir, name) = split(path);
        let name = name.to_string();
        let attr = SetAttr {
            mode: Some(mode & 0o7777),
            ..SetAttr::nothing()
        };
        if mode & S_IFMT == S_IFLNK {
            if self.node(path).is_ok() || self.held.contains_key(path) {
                return Err(libc::EEXIST);
            }
            self.held.insert(path.to_string(), (Vec::new(), mode));
            self.listing = None;
            return self.record(path, &name, out);
        }
        let (dir_fh, kind) = self.node(dir)?;
        if kind != Ftype3::Dir {
            return Err(libc::ENOTDIR);
        }
        let s = &mut self.session;
        let made = match mode & S_IFMT {
            S_IFREG => s.create(&dir_fh, &name, CreateHow::Guarded(attr)),
            S_IFDIR => s.mkdir(&dir_fh, &name, &attr),
            S_IFIFO => s.mknod(&dir_fh, &name, NodeKind::Fifo, &attr),
            S_IFSOCK => s.mknod(&dir_fh, &name, NodeKind::Socket, &attr),
            _ => return Err(libc::EOPNOTSUPP),
        }
        .map_err(errno)?;
        self.listing = None;
        match (made.fh, made.attr) {
            (Some(fh), Some(attr)) => {
                self.handles.insert(path.to_string(), (fh, attr.ftype));
                let len = record_len(&name);
                if len > out.len() {
                    return Err(libc::ENAMETOOLONG);
                }
                put_record(&name, &attr, out);
                Ok(len)
            }
            _ => self.record(path, &name, out),
        }
    }

    /// `OP_REMOVE`: remove `path`, a file or a directory with nothing in
    /// it.
    pub fn remove(&mut self, path: &str) -> Answer<()> {
        self.writable()?;
        if self.held.remove(path).is_some() {
            self.listing = None;
            return Ok(());
        }
        let (dir, name) = split(path);
        let (_, kind) = self.node(path)?;
        let (dir_fh, _) = self.node(dir)?;
        let done = if kind == Ftype3::Dir {
            self.session.rmdir(&dir_fh, name)
        } else {
            self.session.remove(&dir_fh, name)
        };
        self.forget_under(path);
        done.map(|_| ()).map_err(errno)
    }

    /// `OP_RENAME`: `from` becomes `to`, in place of what is there; the
    /// record of `to` in `out`.
    pub fn rename(&mut self, from: &str, to: &str, out: &mut [u8]) -> Answer<usize> {
        self.writable()?;
        let ((from_dir, from_name), (to_dir, to_name)) = (split(from), split(to));
        if let Some(held) = self.held.remove(from) {
            self.held.insert(to.to_string(), held);
            self.listing = None;
            return self.record(to, to_name, out);
        }
        let (from_fh, _) = self.node(from_dir)?;
        let (to_fh, _) = self.node(to_dir)?;
        let done = self.session.rename(&from_fh, from_name, &to_fh, to_name);
        self.forget_under(from);
        self.forget_under(to);
        done.map_err(errno)?;
        self.record(to, to_name, out)
    }

    /// `OP_TRUNCATE`: make `path` `len` bytes long.
    pub fn truncate(&mut self, path: &str, len: u64) -> Answer<()> {
        self.set(
            path,
            SetAttr {
                size: Some(len),
                ..SetAttr::nothing()
            },
        )
    }

    /// `OP_SETTIME`: `path` was last written at `secs`.
    pub fn set_time(&mut self, path: &str, secs: i64) -> Answer<()> {
        self.set(
            path,
            SetAttr {
                mtime: Some(SetTime::Client(Timespec { sec: secs, nsec: 0 })),
                ..SetAttr::nothing()
            },
        )
    }

    /// `OP_SETATTR`: `path` belongs to `uid` and `gid`, with the
    /// permissions `perms`.
    pub fn set_attr(&mut self, path: &str, uid: u32, gid: u32, perms: u32) -> Answer<()> {
        if let Some((_, mode)) = self.held.get_mut(path) {
            *mode = perms & 0o7777;
            return Ok(());
        }
        self.set(
            path,
            SetAttr {
                mode: Some(perms & 0o7777),
                uid: Some(uid),
                gid: Some(gid),
                ..SetAttr::nothing()
            },
        )
    }

    fn set(&mut self, path: &str, attr: SetAttr) -> Answer<()> {
        self.writable()?;
        if self.held.contains_key(path) {
            return Ok(());
        }
        self.on(path, |v, fh, _| {
            v.session
                .setattr(fh, &attr, None)
                .map(|_| ())
                .map_err(errno)
        })
    }

    /// `OP_STATFS`: the numbers of `statfs`, as five u64 in `out`: block
    /// size, blocks, free blocks, longest name and the magic.
    pub fn statfs(&mut self, out: &mut [u8]) -> Answer<usize> {
        let root = self.session.root().clone();
        let st = self.session.fsstat(&root).map_err(errno)?;
        let numbers = [BLOCK, st.tbytes / BLOCK, st.fbytes / BLOCK, 255, NFS_MAGIC];
        for (i, n) in numbers.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&n.to_ne_bytes());
        }
        Ok(numbers.len() * 8)
    }

    /// Whether something written may not be on the server's disk yet.
    pub fn is_dirty(&self) -> bool {
        !self.written.is_empty()
    }

    /// `COMMIT` of every file written since the last: what the server holds
    /// in memory goes to its disk. `OP_SYNC`, and a second after a write.
    pub fn commit(&mut self) -> Answer<()> {
        let written: Vec<FileHandle> = self.written.drain().collect();
        let mut failed = Ok(());
        for fh in written {
            match self.session.commit(&fh, 0, 0) {
                Ok(_) => {}
                // Gone meanwhile: nothing of it to keep.
                Err(Errno::ESTALE) => {}
                Err(e) => {
                    self.written.insert(fh);
                    failed = Err(errno(e));
                }
            }
        }
        failed
    }
}

#[cfg(test)]
mod tests;
