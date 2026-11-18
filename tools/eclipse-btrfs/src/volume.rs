//! A btrfs volume as the kernel asks for it: by path, with what each
//! request needs answered and nothing kept per open file.
//!
//! The btrfs code keeps the tree blocks it changed in memory, and here
//! writes them, with the superblock, every [`CHECKPOINT_EVERY`] changes
//! and when asked to sync ([`Volume::checkpoint`]): not to the device, but
//! into the disk's kept memory ([`fatserve::disk`]), as one group, so that
//! the memory with the device under it is always a whole volume. The
//! changes since are written down in the kept memory too ([`crate::log`]),
//! as the kernel asked for them. A server that takes the mount over mounts
//! the volume as the trees were last written and makes those changes
//! again: the volume is then what the kernel was told. A server that dies
//! inside a writing of the trees leaves the group, which the next one's
//! disk takes back, with the number of the writing: the changes written
//! down are then still the ones to make again.
//!
//! What this keeps besides only saves work: which inode each path is, and
//! the listing being sent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use btrfs::{Btrfs, FileKind, InodeStat};
use fatserve::disk::{Blocks, Disk, Kept};

use crate::dev::{lock, Dev, DiskRef, Shared};
use crate::log::{self, Log};
use crate::tail::{self, Tail};
use fatserve::wire::{
    OP_CREATE, OP_REMOVE, OP_RENAME, OP_SETATTR, OP_SETTIME, OP_TRUNCATE, OP_WRITE,
};

/// The changes made between two writings of the trees: the number the
/// kernel's btrfs waits for too.
pub const CHECKPOINT_EVERY: usize = 32;

/// The most bytes of what was kept of a file that are written down when
/// btrfs is given them: more is a writing of the trees instead.
const WRITE_NOTED_MOST: usize = 64 * 1024;

/// A Linux `errno`.
pub type Errno = i32;

/// The `statfs` magic of btrfs.
pub const BTRFS_SUPER_MAGIC: u64 = 0x9123_683e;

/// A record's fixed part on a POSIX mount: what the kernel's
/// `served::wire::RECORD_HEAD_POSIX` says.
pub const RECORD_HEAD: usize = 64;

/// What `stat` of the top of the mount is called in its record, which
/// needs a name: the kernel takes nothing from it.
const TOP_NAME: &str = "top";

fn errno_of(e: btrfs::Error) -> Errno {
    use btrfs::Error as E;
    match e {
        E::Io | E::Corrupt(_) => libc::EIO,
        E::BadSuperblock | E::Invalid => libc::EINVAL,
        E::Unsupported(_) => libc::EOPNOTSUPP,
        E::NotFound => libc::ENOENT,
        E::Exists => libc::EEXIST,
        E::NotDir => libc::ENOTDIR,
        E::IsDir => libc::EISDIR,
        E::NotEmpty => libc::ENOTEMPTY,
        E::NoSpace => libc::ENOSPC,
    }
}

fn wall_clock() -> (u64, u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs(), now.subsec_nanos())
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

/// Write the record of the file `st` called `name` at the start of `out`,
/// which holds it.
pub fn put_record(name: &str, st: &InodeStat, out: &mut [u8]) {
    let len = record_len(name);
    out[..len].fill(0);
    out[0..2].copy_from_slice(&(len as u16).to_ne_bytes());
    out[2..4].copy_from_slice(&(name.len() as u16).to_ne_bytes());
    out[4..8].copy_from_slice(&st.mode.to_ne_bytes());
    out[8..16].copy_from_slice(&st.size.to_ne_bytes());
    out[16..24].copy_from_slice(&(st.mtime.0 as i64).to_ne_bytes());
    out[24..32].copy_from_slice(&(st.atime.0 as i64).to_ne_bytes());
    out[32..40].copy_from_slice(&(st.ctime.0 as i64).to_ne_bytes());
    out[40..44].copy_from_slice(&st.uid.to_ne_bytes());
    out[44..48].copy_from_slice(&st.gid.to_ne_bytes());
    out[48..52].copy_from_slice(&st.nlink.to_ne_bytes());
    out[56..64].copy_from_slice(&st.rdev.to_ne_bytes());
    out[RECORD_HEAD..RECORD_HEAD + name.len()].copy_from_slice(name.as_bytes());
}

/// The kind of file a mode asks for, as btrfs says it.
fn kind_of(mode: u32) -> Option<FileKind> {
    Some(match mode & 0o170000 {
        0o100000 => FileKind::Regular,
        0o040000 => FileKind::Dir,
        0o120000 => FileKind::Symlink,
        0o010000 => FileKind::Fifo,
        0o140000 => FileKind::Socket,
        _ => return None,
    })
}

/// The memory a mount holds for its servers: the end of the file being
/// written ([`tail`]), the changes since the trees were written
/// ([`log`]), then the disk's chunks.
pub const KEPT_LEN: usize = tail::LEN + log::LEN + Kept::LEN;

/// [`KEPT_LEN`] bytes of memory, cut in its three parts.
pub struct Memory {
    pub tail: Tail,
    pub log: Log,
    pub disk: Kept,
}

impl Memory {
    /// Memory of this process's own, for a read-only mount.
    pub fn private() -> Self {
        Memory {
            tail: Tail::private(),
            log: Log::private(),
            disk: Kept::private(),
        }
    }

    /// The memory at `base`.
    ///
    /// # Safety
    /// `base` is the start of [`KEPT_LEN`] bytes, at a page's start, that
    /// are this process's to read and write for as long as this is used.
    pub unsafe fn at(base: *mut u8) -> Self {
        Memory {
            tail: Tail::at(base),
            log: Log::at(base.add(tail::LEN)),
            disk: Kept::at(base.add(tail::LEN + log::LEN)),
        }
    }
}

/// One file of a listing being sent.
struct Listed {
    name: String,
    st: InodeStat,
}

pub struct Volume<B: Blocks + Send + 'static> {
    fs: Btrfs,
    disk: DiskRef<B>,
    tail: Tail,
    log: Log,
    /// The changes a dead server wrote down are being made again: nothing
    /// is written down, and what it kept of a file waits for the last.
    replaying: bool,
    read_only: bool,
    /// The inode of each path asked about, and its kind.
    paths: HashMap<String, (u64, FileKind)>,
    /// The directory whose listing is being sent, a reply at a time.
    listing: Option<(String, Vec<Listed>)>,
    /// How many tree changes were made into the kept memory.
    pub checkpoints: u64,
    /// The bytes btrfs has written into it.
    written: Arc<AtomicU64>,
    /// The most bytes one checkpoint wrote.
    pub largest_checkpoint: u64,
    #[cfg(test)]
    before_write: crate::dev::Hook,
}

impl<B: Blocks + Send + 'static> Volume<B> {
    /// The volume on `blocks`, `len` bytes, with what the servers before
    /// left in `memory`. Read-only, nothing is written whatever is asked.
    pub fn open(blocks: B, len: u64, memory: Memory, read_only: bool) -> Result<Self, String> {
        let disk = Disk::over(blocks, len, memory.disk)
            .ok_or("the memory the mount kept is another device's")?;
        let disk: DiskRef<B> = Arc::new(Mutex::new(Shared(disk)));
        let written = Arc::new(AtomicU64::new(0));
        let dev = Dev::new(disk.clone(), len, written.clone());
        #[cfg(test)]
        let before_write = dev.before_write.clone();
        let dev: Arc<dyn btrfs::BlockDevice> = Arc::new(dev);
        let mut fs = Btrfs::mount(dev, read_only).map_err(|e| format!("{e:?}"))?;
        fs.set_clock(wall_clock);
        // The trees are written when this says, and only then.
        fs.set_commit_interval(u32::MAX);
        let mut volume = Volume {
            fs,
            disk,
            tail: memory.tail,
            log: memory.log,
            replaying: false,
            read_only,
            paths: HashMap::new(),
            listing: None,
            checkpoints: 0,
            written,
            largest_checkpoint: 0,
            #[cfg(test)]
            before_write,
        };
        if read_only {
            return Ok(volume);
        }
        let failed = |e: Errno| format!("errno {e}");
        // The changes written down follow the last writing of the trees,
        // or one before it: then the trees have them.
        let written_at = lock(&volume.disk).0.word();
        if volume.log.base() != written_at {
            volume.log.restart(written_at);
        }
        // What a first change does before any (a free space tree the code
        // does not keep is marked stale), and, as the kernel's btrfs, an
        // image written onto a larger partition takes the whole of it:
        // neither changes what the volume holds, and both are written now,
        // as one group.
        lock(&volume.disk).0.begin();
        let prepared = volume.fs.prepare_writes();
        let grew = prepared.and_then(|()| volume.fs.grow_to_device());
        let synced = volume.fs.sync();
        lock(&volume.disk).0.commit();
        prepared.map_err(|e| format!("{e:?}"))?;
        synced.map_err(|e| format!("{e:?}"))?;
        if grew.unwrap_or(false) {
            eprintln!("eclipse-btrfs: the volume grew to its device");
        }
        // What the server before had done since, then what it kept of a
        // file being written, which was the last thing it was told.
        let changes = volume.log.changes();
        volume.replaying = true;
        for change in &changes {
            volume.make_again(change);
        }
        volume.replaying = false;
        volume.write_tail().map_err(failed)?;
        volume.checkpoint().map_err(failed)?;
        Ok(volume)
    }

    /// A change a server before this one was asked for, made again as it
    /// was asked for. What it answered is not this one's to know: the
    /// kernel was told, and the same change on the same volume answers the
    /// same.
    fn make_again(&mut self, change: &log::Change) {
        let mut out = vec![0u8; 4096];
        let path = change.names.as_str();
        let _ = match change.op {
            OP_WRITE => self.write(path, change.a, &change.data).map(|_| ()),
            OP_CREATE => self.create(path, change.a as u32, &mut out).map(|_| ()),
            OP_REMOVE => self.remove(path),
            OP_RENAME => match path.split_once('\0') {
                Some((from, to)) => self.rename(from, to, &mut out).map(|_| ()),
                None => Ok(()),
            },
            OP_TRUNCATE => self.truncate(path, change.a),
            OP_SETTIME => self.set_time(path, change.a as i64),
            OP_SETATTR => self.set_attr(path, (change.a >> 32) as u32, change.a as u32, change.b),
            _ => Ok(()),
        };
    }

    /// Whether the memory the mount kept was a server's before.
    pub fn adopted(&self) -> bool {
        lock(&self.disk).0.adopted()
    }

    /// The trees btrfs changed go into the kept memory, as one group with
    /// the number of this writing of them; the changes written down are
    /// then the trees', and nothing is written down after it.
    fn checkpoint(&mut self) -> Result<(), Errno> {
        if self.read_only || self.replaying {
            return Ok(());
        }
        let before = self.written.load(Ordering::Relaxed);
        let next = self.log.base() + 1;
        lock(&self.disk).0.begin();
        let done = self.fs.sync().map_err(errno_of);
        {
            let mut disk = lock(&self.disk);
            disk.0.set_word_in_group(next);
            disk.0.commit();
        }
        self.log.restart(next);
        self.checkpoints += 1;
        let wrote = self.written.load(Ordering::Relaxed) - before;
        self.largest_checkpoint = self.largest_checkpoint.max(wrote);
        done
    }

    /// Write down a change that was made: `op` with `names`, `a`, `b` and
    /// `data`, as the kernel asks for it. The trees are written when there
    /// are enough of them.
    fn note(&mut self, op: u32, names: &str, a: u64, b: u32, data: &[u8]) -> Result<(), Errno> {
        if self.replaying {
            return Ok(());
        }
        if !self.log.push(op, names, a, b, data) {
            // No room: not written down, so the trees must have it now.
            return self.checkpoint();
        }
        if self.log.records() >= CHECKPOINT_EVERY {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// A change: made, then written down as `op`, `names`, `a`, `b` and
    /// `data`. What is kept of the file being written goes to btrfs first,
    /// and what is known of paths that the change may have changed is
    /// forgotten. Written down whatever it answered: made again on the
    /// same volume it answers the same.
    #[allow(clippy::too_many_arguments)]
    fn change<T>(
        &mut self,
        (op, names, a, b, data): (u32, &str, u64, u32, &[u8]),
        forget: &[&str],
        f: impl FnOnce(&mut Self) -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        if self.read_only {
            return Err(libc::EROFS);
        }
        self.write_tail()?;
        self.listing = None;
        for gone in forget {
            self.paths.retain(|path, _| !under(path, gone));
        }
        if !self.log.has_room(Log::record_len(names, data)) {
            self.checkpoint()?;
        }
        let done = f(self);
        self.note(op, names, a, b, data)?;
        done
    }

    /// What is kept of the file being written goes to btrfs.
    fn write_tail(&mut self) -> Result<(), Errno> {
        if self.replaying {
            return Ok(());
        }
        let Some((path, offset, data)) = self.tail.kept() else {
            return Ok(());
        };
        let (path, data) = (path.to_string(), data.to_vec());
        let (ino, _) = self.node(&path)?;
        let mut done = 0;
        while done < data.len() {
            let n = self
                .fs
                .write(ino, offset + done as u64, &data[done..])
                .map_err(errno_of)?;
            if n == 0 {
                return Err(libc::ENOSPC);
            }
            done += n;
        }
        // Written down, or in the trees, before it goes from the tail: a
        // server that dies in between leaves it in both, and writing it
        // twice is the same bytes in the same place. A short one is written
        // down; a long one costs less as a writing of the trees.
        if data.len() <= WRITE_NOTED_MOST && self.log.has_room(Log::record_len(&path, &data)) {
            self.note(OP_WRITE, &path, offset, data.len() as u32, &data)?;
        } else {
            self.checkpoint()?;
        }
        self.tail.clear();
        Ok(())
    }

    /// The inode at `path`, and its kind.
    fn node(&mut self, path: &str) -> Result<(u64, FileKind), Errno> {
        if path.is_empty() {
            return Ok((self.fs.root_ino(), FileKind::Dir));
        }
        if let Some(known) = self.paths.get(path) {
            return Ok(*known);
        }
        let (dir, name) = split(path);
        let (dir_ino, kind) = self.node(dir)?;
        if kind != FileKind::Dir {
            return Err(libc::ENOTDIR);
        }
        let ino = self.fs.lookup(dir_ino, name).map_err(errno_of)?;
        let kind = self.fs.stat(ino).map_err(errno_of)?.kind;
        self.paths.insert(path.to_string(), (ino, kind));
        Ok((ino, kind))
    }

    /// The directory `path`'s entries from entry `start` on, as records in
    /// `out`: `(records, bytes, next)`, `next` being `LIST_END` after the
    /// last.
    pub fn list(
        &mut self,
        path: &str,
        start: u64,
        out: &mut [u8],
    ) -> Result<(usize, usize, u64), Errno> {
        // The size of the file being written is the one btrfs says.
        if !self.read_only {
            self.write_tail()?;
        }
        let fresh = start == 0 || self.listing.as_ref().is_none_or(|(p, _)| p != path);
        if fresh {
            let (ino, kind) = self.node(path)?;
            if kind != FileKind::Dir {
                return Err(libc::ENOTDIR);
            }
            let entries = self.fs.readdir(ino).map_err(errno_of)?;
            let mut listed = Vec::with_capacity(entries.len());
            for entry in entries {
                let st = self.fs.stat(entry.ino).map_err(errno_of)?;
                let full = if path.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{path}/{}", entry.name)
                };
                self.paths.insert(full, (entry.ino, st.kind));
                listed.push(Listed {
                    name: entry.name,
                    st,
                });
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
            put_record(&entry.name, &entry.st, &mut out[used..]);
            used += len;
            count += 1;
        }
        self.listing = None;
        Ok((count, used, fatserve::wire::LIST_END))
    }

    /// The record of `path` itself, called `name`, in `out`: its bytes.
    fn record(&mut self, path: &str, name: &str, out: &mut [u8]) -> Result<usize, Errno> {
        let (ino, _) = self.node(path)?;
        let st = self.fs.stat(ino).map_err(errno_of)?;
        let len = record_len(name);
        if len > out.len() {
            return Err(libc::ENAMETOOLONG);
        }
        put_record(name, &st, out);
        Ok(len)
    }

    /// The record of the top of the volume, in `out`.
    pub fn stat(&mut self, path: &str, out: &mut [u8]) -> Result<usize, Errno> {
        let name = match split(path).1 {
            "" => TOP_NAME,
            name => name,
        };
        let name = name.to_string();
        self.record(path, &name, out)
    }

    /// Up to `out.len()` bytes of `path` from `offset`: the target of a
    /// symbolic link, or a file's bytes.
    pub fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, Errno> {
        if self.tail.is_of(path) {
            self.write_tail()?;
        }
        let (ino, kind) = self.node(path)?;
        match kind {
            FileKind::Dir => Err(libc::EISDIR),
            FileKind::Symlink => {
                let target = self.fs.read_link(ino).map_err(errno_of)?;
                let from = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(target.len());
                let n = out.len().min(target.len() - from);
                out[..n].copy_from_slice(&target[from..from + n]);
                Ok(n)
            }
            _ => self.fs.read(ino, offset, out).map_err(errno_of),
        }
    }

    /// `data` into `path` at `offset`: all of it, or fewer bytes when the
    /// volume filled up.
    pub fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<usize, Errno> {
        if self.read_only {
            return Err(libc::EROFS);
        }
        let noted = (OP_WRITE, path, offset, data.len() as u32, data);
        if !self.replaying && self.tail.continues(path, offset, data.len()) {
            self.tail.append(data);
            return Ok(data.len());
        }
        let (ino, kind) = self.node(path)?;
        match kind {
            FileKind::Dir => return Err(libc::EISDIR),
            FileKind::Symlink => {
                return self.change(noted, &[], |v| {
                    v.fs.write_symlink(ino, offset, data).map_err(errno_of)
                });
            }
            _ => {}
        }
        self.write_tail()?;
        if !self.replaying && data.len() < tail::TAIL_MAX && self.tail.start(path, offset, data) {
            return Ok(data.len());
        }
        self.change(noted, &[], |v| {
            v.fs.write(ino, offset, data).map_err(errno_of)
        })
    }

    /// Make `path` a file of the kind and permissions `mode` says; its
    /// record, called by its name, in `out`.
    pub fn create(&mut self, path: &str, mode: u32, out: &mut [u8]) -> Result<usize, Errno> {
        let kind = kind_of(mode).ok_or(libc::EOPNOTSUPP)?;
        let (dir, name) = split(path);
        let name = name.to_string();
        self.change((OP_CREATE, path, mode as u64, 0, &[]), &[], |v| {
            let (dir_ino, dir_kind) = v.node(dir)?;
            if dir_kind != FileKind::Dir {
                return Err(libc::ENOTDIR);
            }
            let ino =
                v.fs.create(dir_ino, &name, kind, mode & 0o7777, 0)
                    .map_err(errno_of)?;
            v.paths.insert(path.to_string(), (ino, kind));
            Ok(())
        })?;
        self.record(path, &name, out)
    }

    /// Remove `path`: a file, or a directory with nothing in it.
    pub fn remove(&mut self, path: &str) -> Result<(), Errno> {
        let (dir, name) = split(path);
        let name = name.to_string();
        self.change((OP_REMOVE, path, 0, 0, &[]), &[path], |v| {
            let (dir_ino, _) = v.node(dir)?;
            v.fs.unlink(dir_ino, &name).map_err(errno_of)
        })
    }

    /// `from` becomes `to`, in place of what is there; the record of `to`
    /// in `out`.
    pub fn rename(&mut self, from: &str, to: &str, out: &mut [u8]) -> Result<usize, Errno> {
        let ((from_dir, from_name), (to_dir, to_name)) = (split(from), split(to));
        let (from_name, to_name) = (from_name.to_string(), to_name.to_string());
        let names = format!("{from}\0{to}");
        self.change((OP_RENAME, &names, 0, 0, &[]), &[from, to], |v| {
            let (from_ino, _) = v.node(from_dir)?;
            let (to_ino, _) = v.node(to_dir)?;
            v.fs.rename(from_ino, &from_name, to_ino, &to_name)
                .map_err(errno_of)
        })?;
        self.record(to, &to_name, out)
    }

    /// Make `path` `len` bytes long.
    pub fn truncate(&mut self, path: &str, len: u64) -> Result<(), Errno> {
        self.change((OP_TRUNCATE, path, len, 0, &[]), &[], |v| {
            let (ino, _) = v.node(path)?;
            v.fs.truncate(ino, len).map_err(errno_of)
        })
    }

    /// `path` was last written at `secs`.
    pub fn set_time(&mut self, path: &str, secs: i64) -> Result<(), Errno> {
        self.change((OP_SETTIME, path, secs as u64, 0, &[]), &[], |v| {
            let (ino, _) = v.node(path)?;
            v.fs.set_attr(ino, None, None, None, None, Some((secs.max(0) as u64, 0)))
                .map_err(errno_of)
        })
    }

    /// `path` belongs to `uid` and `gid`, with the permissions `perms`.
    pub fn set_attr(&mut self, path: &str, uid: u32, gid: u32, perms: u32) -> Result<(), Errno> {
        let owner = (uid as u64) << 32 | gid as u64;
        self.change((OP_SETATTR, path, owner, perms, &[]), &[], |v| {
            let (ino, _) = v.node(path)?;
            v.fs.set_attr(ino, Some(perms & 0o7777), Some(uid), Some(gid), None, None)
                .map_err(errno_of)
        })
    }

    /// The numbers of `statfs`, as five u64 in `out`: block size, blocks,
    /// free blocks, longest name and the magic.
    pub fn statfs(&mut self, out: &mut [u8]) -> Result<usize, Errno> {
        let st = self.fs.fsinfo();
        let bsize = st.block_size.max(512);
        let free = st.total_bytes.saturating_sub(st.bytes_used);
        let numbers = [
            bsize,
            st.total_bytes / bsize,
            free / bsize,
            255,
            BTRFS_SUPER_MAGIC,
        ];
        for (i, n) in numbers.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&n.to_ne_bytes());
        }
        Ok(numbers.len() * 8)
    }

    /// Whether something is not on the device yet.
    pub fn is_dirty(&self) -> bool {
        self.tail.kept().is_some() || self.log.records() > 0 || lock(&self.disk).0.is_dirty()
    }

    /// Everything reaches the device: what the timer does, a second after
    /// a change.
    pub fn flush(&mut self) -> Result<(), Errno> {
        if self.read_only {
            return Ok(());
        }
        self.write_tail()?;
        self.checkpoint()?;
        lock(&self.disk)
            .0
            .flush()
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
    }

    /// `sync`: the same, asked for.
    pub fn sync(&mut self) -> Result<(), Errno> {
        self.flush()
    }

    /// How many times the device was read, for whoever measures.
    pub fn device_reads(&self) -> u64 {
        lock(&self.disk).0.device_reads
    }
}

#[cfg(test)]
mod tests;
