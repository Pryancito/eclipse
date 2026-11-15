//! A filesystem that a process serves: the kernel's side of it.
//!
//! The process (`tools/eclipse-fatfs` is the first) reads the disk and knows
//! its format; the kernel knows neither. What it keeps is what it has been
//! told: the entries of each directory it listed, with their sizes and
//! times, and the pages of each file it read, in the file's page cache
//! (`served_read` in `File`). The kernel is the server's only client, so
//! what it was told stays true, and a name looked up twice, a `stat`, or
//! bytes read twice ask the server nothing.
//!
//! A file is its path from the top of the mount, as on the kernel's own
//! FAT, and each request carries the path: the server keeps nothing per
//! open file that the kernel would have to tell it to forget, or to tell
//! the next server after it died.
//!
//! What this trusts: the server is a process root started for the purpose.
//! It can answer wrongly -- the names, sizes and bytes are whatever it says
//! -- or not at all (a request gives up, as an I/O error). It cannot name
//! anything outside its mount ([`wire::records`]), and it sees no kernel
//! memory: only the window it gave.
//!
//! This first step mounts read-only.

mod link;
#[cfg(test)]
mod tests;
pub mod wire;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lock::Mutex;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};
use zircon_object::ipc::Fifo;
use zircon_object::vm::VmObject;

use self::link::Link;
use self::wire::{Entry, DATA_AREA, LIST_END, OP_LIST, OP_READ, OP_STATFS, STATFS_LEN};
use crate::error::{LxError, LxResult};
use crate::process::LinuxProcess;

/// `eclipse_fs_serve` flag: a name matches an entry whatever the case of
/// its ASCII letters, as on FAT.
pub const SERVE_FOLD_CASE: usize = 1;

/// The most entries of one directory the kernel takes: a server that never
/// says its listing is over does not fill the heap.
const MAX_ENTRIES: usize = 1 << 20;

/// `st_dev` of each served mount, apart from the other filesystems'.
static DEV_COUNTER: AtomicU64 = AtomicU64::new(0x5E70_0000);

/// The served mounts, by where they are mounted: what an unmount closes.
static MOUNTS: Mutex<Vec<(String, Arc<Link>)>> = Mutex::new(Vec::new());

/// The inode number of a path: FNV-1a, as the kernel's FAT numbers its own.
fn ino_of(path: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A path's directory and its name in it.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// What the server says of the filesystem as a whole.
#[derive(Clone, Copy)]
struct Numbers {
    bsize: usize,
    blocks: usize,
    bfree: usize,
    namemax: usize,
    ftype: u64,
}

/// What is said of one that has not been asked.
const UNASKED: Numbers = Numbers {
    bsize: 4096,
    blocks: 0,
    bfree: 0,
    namemax: 255,
    ftype: 0,
};

pub struct ServedFs {
    link: Arc<Link>,
    this: Weak<Self>,
    dev_id: u64,
    fold_case: bool,
    /// The directories listed so far, by path.
    dirs: Mutex<BTreeMap<String, Arc<Vec<Entry>>>>,
    /// The filesystem's numbers, once asked.
    info: Mutex<Option<Numbers>>,
    /// The mount is made: until then the server is inside the call that
    /// makes it, and asking it anything would wait for the answer in vain.
    mounted: AtomicBool,
}

impl ServedFs {
    fn new(link: Arc<Link>, fold_case: bool) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            link,
            this: this.clone(),
            dev_id: DEV_COUNTER.fetch_add(1, Ordering::Relaxed),
            fold_case,
            dirs: Mutex::new(BTreeMap::new()),
            info: Mutex::new(None),
            mounted: AtomicBool::new(false),
        })
    }

    fn arc(&self) -> Arc<Self> {
        self.this.upgrade().expect("ServedFs dropped")
    }

    /// The entries of the directory `path`: as listed before, or from the
    /// server. No lock is held while it answers.
    fn listing(&self, path: &str) -> Result<Arc<Vec<Entry>>> {
        if let Some(listed) = self.dirs.lock().get(path) {
            return Ok(listed.clone());
        }
        let mut entries = Vec::new();
        let mut data = Vec::new();
        let mut next = 0u64;
        while next != LIST_END {
            let answer = self.link.call(OP_LIST, path, next, 0)?;
            data.resize(answer.len, 0);
            answer.data(0, &mut data)?;
            // An answer that goes nowhere would be asked for again for ever.
            let goes_on = answer.a == LIST_END || (answer.a > next && answer.count > 0);
            if !goes_on || entries.len() + answer.count > MAX_ENTRIES {
                return Err(FsError::DeviceError);
            }
            wire::records(&data, answer.count, &mut entries).ok_or(FsError::DeviceError)?;
            next = answer.a;
        }
        let entries = Arc::new(entries);
        self.dirs.lock().insert(path.to_string(), entries.clone());
        Ok(entries)
    }

    fn same_name(&self, a: &str, b: &str) -> bool {
        if self.fold_case {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    }

    fn numbers(&self) -> Option<Numbers> {
        if let Some(numbers) = *self.info.lock() {
            return Some(numbers);
        }
        if !self.mounted.load(Ordering::Acquire) {
            return None;
        }
        let answer = self.link.call(OP_STATFS, "", 0, 0).ok()?;
        let mut raw = [0u8; STATFS_LEN];
        answer.data(0, &mut raw).ok()?;
        drop(answer);
        let field = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&raw[i * 8..i * 8 + 8]);
            u64::from_ne_bytes(b)
        };
        let numbers = Numbers {
            // A block size `stat` and its readers can live with, whatever
            // the server says.
            bsize: (field(0) as usize).clamp(512, 1 << 20),
            blocks: field(1) as usize,
            bfree: field(2).min(field(1)) as usize,
            namemax: (field(3) as usize).clamp(1, 255),
            ftype: field(4),
        };
        *self.info.lock() = Some(numbers);
        Some(numbers)
    }
}

impl FileSystem for ServedFs {
    /// Nothing to write: the mount is read-only.
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::new(ServedINode {
            fs: self.arc(),
            path: String::new(),
            entry: None,
        })
    }

    fn info(&self) -> FsInfo {
        let n = self.numbers().unwrap_or(UNASKED);
        FsInfo {
            bsize: n.bsize,
            frsize: n.bsize,
            blocks: n.blocks,
            bfree: n.bfree,
            bavail: n.bfree,
            files: 0,
            ffree: 0,
            namemax: n.namemax,
            ftype: n.ftype,
        }
    }
}

/// A file or a directory of a served filesystem: its path from the top of
/// the mount, and what its directory's listing said of it (`None` for the
/// top, which is in no listing).
struct ServedINode {
    fs: Arc<ServedFs>,
    path: String,
    entry: Option<Entry>,
}

impl ServedINode {
    fn is_dir(&self) -> bool {
        self.entry.as_ref().is_none_or(|e| e.is_dir)
    }

    fn child(&self, entry: &Entry) -> ServedINode {
        ServedINode {
            fs: self.fs.clone(),
            // The name as the directory has it, which is the one the
            // server finds.
            path: if self.path.is_empty() {
                entry.name.clone()
            } else {
                alloc::format!("{}/{}", self.path, entry.name)
            },
            entry: Some(entry.clone()),
        }
    }

    fn parent(&self) -> Result<ServedINode> {
        let (dir, name) = split_path(&self.path);
        if name.is_empty() {
            // The top is its own parent: the mount above it takes `..` from
            // there.
            return Ok(ServedINode {
                fs: self.fs.clone(),
                path: String::new(),
                entry: None,
            });
        }
        let (above, dir_name) = split_path(dir);
        let entry = match dir_name {
            "" => None,
            _ => Some(
                self.fs
                    .listing(above)?
                    .iter()
                    .find(|e| e.name == dir_name)
                    .cloned()
                    .ok_or(FsError::EntryNotFound)?,
            ),
        };
        Ok(ServedINode {
            fs: self.fs.clone(),
            path: dir.to_string(),
            entry,
        })
    }
}

impl INode for ServedINode {
    /// The bytes as the server reads them, each call a request: what fills
    /// the page cache. `read(2)` comes through `File`'s `served_read` instead.
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let entry = match &self.entry {
            Some(e) if !e.is_dir => e,
            _ => return Err(FsError::IsDir),
        };
        let size = entry.size as usize;
        if offset >= size {
            return Ok(0);
        }
        let want = buf.len().min(size - offset);
        let mut done = 0;
        while done < want {
            let ask = (want - done).min(DATA_AREA);
            let answer =
                self.fs
                    .link
                    .call(OP_READ, &self.path, (offset + done) as u64, ask as u32)?;
            let n = answer.count.min(answer.len).min(ask);
            if n == 0 {
                break;
            }
            answer.data(0, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(done)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::ReadOnly)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let (size, atime, mtime, ctime) = match &self.entry {
            Some(e) if !e.is_dir => (e.size as usize, e.atime, e.mtime, e.ctime),
            Some(e) => (0, e.atime, e.mtime, e.ctime),
            None => (0, 0, 0, 0),
        };
        let at = |sec: i64| Timespec { sec, nsec: 0 };
        let is_dir = self.is_dir();
        Ok(Metadata {
            dev: self.fs.dev_id as usize,
            inode: ino_of(&self.path) as usize,
            size,
            blk_size: self.fs.numbers().unwrap_or(UNASKED).bsize,
            // `st_blocks` counts 512-byte units whatever the block size.
            blocks: size.div_ceil(512),
            atime: at(atime),
            mtime: at(mtime),
            ctime: at(ctime),
            type_: if is_dir {
                FileType::Dir
            } else {
                FileType::File
            },
            mode: if is_dir { 0o755 } else { 0o644 },
            nlinks: if is_dir { 2 } else { 1 },
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        if !self.is_dir() {
            return Err(FsError::NotDir);
        }
        match name {
            "." => Ok(Arc::new(ServedINode {
                fs: self.fs.clone(),
                path: self.path.clone(),
                entry: self.entry.clone(),
            })),
            ".." => Ok(Arc::new(self.parent()?)),
            name => {
                let entries = self.fs.listing(&self.path)?;
                let entry = entries
                    .iter()
                    .find(|e| self.fs.same_name(&e.name, name))
                    .ok_or(FsError::EntryNotFound)?;
                Ok(Arc::new(self.child(entry)))
            }
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        if !self.is_dir() {
            return Err(FsError::NotDir);
        }
        match id {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            i => self
                .fs
                .listing(&self.path)?
                .get(i - 2)
                .map(|e| e.name.clone())
                .ok_or(FsError::EntryNotFound),
        }
    }

    fn get_entry_with_metadata(&self, id: usize) -> Result<(Metadata, String)> {
        if !self.is_dir() {
            return Err(FsError::NotDir);
        }
        match id {
            0 => Ok((self.metadata()?, String::from("."))),
            1 => Ok((self.parent()?.metadata()?, String::from(".."))),
            i => {
                let entries = self.fs.listing(&self.path)?;
                let entry = entries.get(i - 2).ok_or(FsError::EntryNotFound)?;
                Ok((self.child(entry).metadata()?, entry.name.clone()))
            }
        }
    }

    fn sync_all(&self) -> Result<()> {
        Ok(())
    }

    fn sync_data(&self) -> Result<()> {
        Ok(())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.clone()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// Whether `inode` is a file of a served filesystem: one whose `read(2)`
/// goes through its page cache (`File::read`), each `read_at` of the inode
/// itself being a request to a process.
pub(super) fn is_file(inode: &dyn INode) -> bool {
    inode
        .downcast_ref::<ServedINode>()
        .is_some_and(|i| !i.is_dir())
}

/// `eclipse_fs_serve`: mount at `target` the filesystem a process serves on
/// `fifo`, through `window`. `source` and `fstype` are what the mount table
/// shows for it. The caller needs what `mount(2)` needs.
pub fn serve(
    proc: &LinuxProcess,
    fifo: Arc<Fifo>,
    window: Arc<VmObject>,
    target: &str,
    source: &str,
    fstype: &str,
    flags: usize,
) -> LxResult<()> {
    if !proc.capable(crate::process::CAP_SYS_ADMIN) {
        return Err(LxError::EPERM);
    }
    let shown = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic());
    if flags & !SERVE_FOLD_CASE != 0 || !target.starts_with('/') || !shown(source) || !shown(fstype)
    {
        return Err(LxError::EINVAL);
    }
    let link = Arc::new(Link::new(fifo, window).map_err(|_| LxError::EINVAL)?);
    let fs = ServedFs::new(link.clone(), flags & SERVE_FOLD_CASE != 0);
    let target = super::mount_ops::mount_served(target, source, fstype, fs.clone())?;
    fs.mounted.store(true, Ordering::Release);
    MOUNTS.lock().push((target, link));
    Ok(())
}

/// `target` was unmounted, with whatever was mounted under it: the servers
/// of the served mounts among them are told, by their FIFO closing.
pub(super) fn unmounted(target: &str) {
    let gone: Vec<Arc<Link>> = {
        let mut mounts = MOUNTS.lock();
        let under = |at: &str| {
            at == target
                || target == "/"
                || at
                    .strip_prefix(target)
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        let (gone, kept): (Vec<_>, Vec<_>) = core::mem::take(&mut *mounts)
            .into_iter()
            .partition(|(at, _)| under(at));
        *mounts = kept;
        gone.into_iter().map(|(_, link)| link).collect()
    };
    for link in gone {
        link.close();
    }
}
