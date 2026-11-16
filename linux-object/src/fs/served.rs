//! A filesystem that a process serves: the kernel's side of it.
//!
//! The process (`tools/eclipse-fatfs` is the first) reads and writes the
//! disk and knows its format; the kernel knows neither. What it keeps is
//! what it has been told: the entries of each directory it listed, with
//! their sizes and times, and the pages of each file it read, in the
//! file's page cache (`served_read` in `File`). The kernel is the server's
//! only client, so what it was told stays true, and a name looked up twice,
//! a `stat`, or bytes read twice ask the server nothing.
//!
//! A change goes to the server when it is made -- a `write` returns once
//! the server has the bytes -- and the kernel then changes what it keeps to
//! match: the file's size, its directory's entries, the pages of its cache
//! the write covers. So nothing the kernel holds is ever ahead of the
//! server; what the server holds reaches the device when it is asked to
//! (`fsync`, `sync`, an unmount) or when the server sees fit.
//!
//! A file is its path from the top of the mount, as on the kernel's own
//! FAT, and each request carries the path: the server keeps nothing per
//! open file that the kernel would have to tell it to forget, or to tell
//! the next server after it died. The kernel's own name for a file is a
//! [`Node`], which a rename moves and gives its new path: a file that is
//! open stays the same file.
//!
//! When the server dies the mount stays, and waits for the next one
//! ([`ServedFs`]): the requests carry paths, what was written is in memory
//! the mount keeps for its servers, and a request that was out with the
//! dead server is made again with the next.
//!
//! What this trusts: the server is a process root started for the purpose.
//! It can answer wrongly -- the names, sizes and bytes are whatever it says
//! -- or not at all (a request gives up, as an I/O error). It cannot name
//! anything outside its mount ([`wire::records`]), and it sees no kernel
//! memory: only the window it gave.

mod link;
pub(crate) use link::between_looks;
#[cfg(test)]
mod tests;
pub mod wire;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use kernel_hal::thread::SyncWait;

use lock::Mutex;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};
use zircon_object::ipc::Fifo;
use zircon_object::vm::VmObject;

use self::link::{Answer, Fail, Link};

/// What the kernel's end of a server's FIFO raises when the mount lets go
/// of the server: whoever holds that end to watch the server lets go too.
pub use self::link::LEFT as SERVER_LEFT;
use self::wire::{
    Entry, DATA_AREA, LIST_END, MODE_DIR, OP_CREATE, OP_LIST, OP_READ, OP_REMOVE, OP_RENAME,
    OP_SETTIME, OP_STATFS, OP_SYNC, OP_TRUNCATE, OP_WRITE, STATFS_LEN,
};
use crate::error::{LxError, LxResult};
use crate::process::LinuxProcess;

/// `eclipse_fs_serve` flag: a name matches an entry whatever the case of
/// its ASCII letters, as on FAT.
pub const SERVE_FOLD_CASE: usize = 1;
/// `eclipse_fs_serve` flag: the mount is read-only.
pub const SERVE_READ_ONLY: usize = 2;

/// `eclipse_fs_serve` flag: only take over a mount whose server is gone,
/// and make none.
pub const SERVE_TAKE_OVER: usize = 4;

/// How long a mount whose server died waits for the next one before its
/// requests fail. The same as a served disk's wait for its next driver,
/// for the same reason: longer than the longest pause `eclipse-init` puts
/// between two starts of a service that keeps dying (8 s).
const SERVER_GRACE_MS: u64 = 10_000;

/// The most entries of one directory the kernel takes: a server that never
/// says its listing is over does not fill the heap.
const MAX_ENTRIES: usize = 1 << 20;

/// `st_dev` of each served mount, apart from the other filesystems'.
static DEV_COUNTER: AtomicU64 = AtomicU64::new(0x5E70_0000);

/// The served mounts, by where they are mounted: what an unmount closes,
/// and what the next server of one finds.
static MOUNTS: Mutex<Vec<(String, Arc<ServedFs>)>> = Mutex::new(Vec::new());

/// A path's directory and its name in it.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// The path of `name` in the directory `dir`.
fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        alloc::format!("{dir}/{name}")
    }
}

/// `path` when it is `dir` or under it: what follows `dir` in it.
fn under<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    path.strip_prefix(dir)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn now() -> i64 {
    kernel_hal::timer::wall_clock_now().as_secs() as i64
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

/// A file or a directory, as the kernel knows it: every inode of it is
/// this one node, so what a write made of its size is what the next `stat`
/// says, and its number is its own for as long as it is known.
struct Node {
    ino: u64,
    is_dir: bool,
    /// From the top of the mount. A rename changes it.
    path: Mutex<String>,
    size: AtomicU64,
    mtime: AtomicI64,
    atime: AtomicI64,
    ctime: AtomicI64,
    /// Removed: its path is nobody's, or another file's by now.
    gone: AtomicBool,
    /// The changes to its bytes: how many are with the server now, and how
    /// many there have been. See [`read_token`].
    changes: Mutex<(u32, u64)>,
}

impl Node {
    fn path(&self) -> String {
        self.path.lock().clone()
    }

    fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// The path to ask the server about: none once the node is removed.
    fn live_path(&self) -> Result<String> {
        if self.is_gone() {
            return Err(FsError::EntryNotFound);
        }
        Ok(self.path())
    }
}

/// A change to a file's bytes, from before the server is asked for it
/// until it has answered.
struct Changing<'a>(&'a Node);

impl<'a> Changing<'a> {
    fn new(node: &'a Node) -> Self {
        let mut changes = node.changes.lock();
        changes.0 += 1;
        changes.1 += 1;
        Changing(node)
    }
}

impl Drop for Changing<'_> {
    fn drop(&mut self) {
        let mut changes = self.0.changes.lock();
        changes.0 -= 1;
        changes.1 += 1;
    }
}

/// An entry of a listing: the name the directory has it under, and it.
#[derive(Clone)]
struct Slot {
    name: String,
    node: Arc<Node>,
}

/// A directory's entries, in the order the server listed them. A removed
/// entry leaves its place empty, so that a directory being read while it
/// is emptied (`rm -r`) goes on from where it was and misses nothing:
/// `getdents` counts places. An entry that is made takes the first empty
/// place, so a directory is never longer than the most entries it had.
type Listing = Vec<Option<Slot>>;

/// The directories listed so far, by path, and the changes to them.
struct Dirs {
    listed: BTreeMap<String, Arc<Listing>>,
    /// The changes to directories with the server now, and how many there
    /// have been: a listing read while one was on its way is not kept.
    changing: u32,
    changes: u64,
}

/// Who serves a mount: a link at a time, one per server.
struct Server {
    link: Arc<Link>,
    /// How many servers the mount has had.
    turns: usize,
}

/// A filesystem behind a FIFO.
///
/// When its server dies the mount stays, with what is open on it. A
/// request that was out with the dead server, and any that comes while
/// there is none, waits for the next server and is made again with it;
/// what the kernel keeps of the filesystem (listings, sizes, pages) is
/// still true, because the server keeps what it was told in memory the
/// mount holds for it ([`ServedFs::kept`]) and the next server starts from
/// there. Only when no server comes within [`SERVER_GRACE_MS`] do requests
/// fail -- and a server that comes later still finds the mount. A server
/// that hangs is made one that died: the request that waited on its
/// silence has it killed (`link::Link::hung`).
pub struct ServedFs {
    server: Mutex<Server>,
    /// How long a request waits for the next server, from the death of
    /// the last.
    grace_ms: AtomicU64,
    /// Memory the server gave with the mount: the next server is given it.
    kept: Mutex<Option<Arc<VmObject>>>,
    /// The source and the type of the mount's line in the mount table, and
    /// the flags it was made with: what the next server must say again.
    shown: Mutex<(String, String, usize)>,
    this: Weak<Self>,
    dev_id: u64,
    fold_case: bool,
    read_only: bool,
    root: Arc<Node>,
    next_ino: AtomicU64,
    dirs: Mutex<Dirs>,
    /// The filesystem's numbers, once asked.
    info: Mutex<Option<Numbers>>,
    /// Something was written since: the free space is another number.
    info_stale: AtomicBool,
    /// The mount is made: until then the server is inside the call that
    /// makes it, and asking it anything would wait for the answer in vain.
    mounted: AtomicBool,
}

/// A change to the directories, from before the server is asked for it
/// until what the kernel keeps has been changed to match.
struct ChangingDirs<'a>(&'a ServedFs);

impl Drop for ChangingDirs<'_> {
    fn drop(&mut self) {
        let mut dirs = self.0.dirs.lock();
        dirs.changing -= 1;
        dirs.changes += 1;
        drop(dirs);
        self.0.info_stale.store(true, Ordering::Release);
    }
}

impl ServedFs {
    fn new(link: Arc<Link>, fold_case: bool, read_only: bool) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            server: Mutex::new(Server { link, turns: 1 }),
            grace_ms: AtomicU64::new(SERVER_GRACE_MS),
            kept: Mutex::new(None),
            shown: Mutex::new((String::new(), String::new(), 0)),
            this: this.clone(),
            dev_id: DEV_COUNTER.fetch_add(1, Ordering::Relaxed),
            fold_case,
            read_only,
            root: Arc::new(Node {
                ino: 1,
                is_dir: true,
                path: Mutex::new(String::new()),
                size: AtomicU64::new(0),
                mtime: AtomicI64::new(0),
                atime: AtomicI64::new(0),
                ctime: AtomicI64::new(0),
                gone: AtomicBool::new(false),
                changes: Mutex::new((0, 0)),
            }),
            next_ino: AtomicU64::new(2),
            dirs: Mutex::new(Dirs {
                listed: BTreeMap::new(),
                changing: 0,
                changes: 0,
            }),
            info: Mutex::new(None),
            info_stale: AtomicBool::new(false),
            mounted: AtomicBool::new(false),
        })
    }

    fn arc(&self) -> Arc<Self> {
        self.this.upgrade().expect("ServedFs dropped")
    }

    fn inode(&self, node: Arc<Node>) -> ServedINode {
        ServedINode {
            fs: self.arc(),
            node,
        }
    }

    /// The node of the entry the server described, in the directory `dir`.
    fn node_of(&self, dir: &str, entry: &Entry) -> Arc<Node> {
        Arc::new(Node {
            ino: self.next_ino.fetch_add(1, Ordering::Relaxed),
            is_dir: entry.is_dir,
            path: Mutex::new(join_path(dir, &entry.name)),
            size: AtomicU64::new(if entry.is_dir { 0 } else { entry.size }),
            mtime: AtomicI64::new(entry.mtime),
            atime: AtomicI64::new(entry.atime),
            ctime: AtomicI64::new(entry.ctime),
            gone: AtomicBool::new(false),
            changes: Mutex::new((0, 0)),
        })
    }

    /// The link of the server of the moment.
    fn link(&self) -> Arc<Link> {
        self.server.lock().link.clone()
    }

    /// The next server's link takes over: its turn. `None` while the mount
    /// has a server.
    fn take_over(&self, link: Arc<Link>) -> Option<usize> {
        let mut server = self.server.lock();
        // Its end of the FIFO may have closed a moment ago, with nothing
        // on this side having looked yet.
        if !server.link.looked_dead() {
            return None;
        }
        server.link = link;
        server.turns += 1;
        Some(server.turns)
    }

    /// The server of turn `turn` is gone: its requests fail from now, and
    /// the wait for the next server starts.
    pub fn server_left(&self, turn: usize) {
        let server = self.server.lock();
        if server.turns == turn {
            server.link.close();
        }
    }

    /// How the server of turn `turn` is ended if it hangs: `kill`, which
    /// kills its process.
    pub fn server_killed_with(&self, turn: usize, kill: Box<dyn Fn() + Send + Sync>) {
        let server = self.server.lock();
        if server.turns == turn {
            server.link.killed_with(kill);
        }
    }

    /// Whether the mount has no server and waits for none: the last one
    /// died longer ago than a request waits for the next.
    fn deserted(&self, link: &Link) -> bool {
        link.is_dead() && link::now_ms() > link.died_at() + self.grace_ms.load(Ordering::Relaxed)
    }

    /// Ask the server `op` with `names` in the slot (a path, or two with a
    /// NUL between), `a`, `b` and `data`. When the server dies under the
    /// request it is made again with the next one, and the answer says so:
    /// the dead server may have done what was asked before it died.
    fn ask(&self, op: u32, names: &[u8], a: u64, b: u32, data: &[u8]) -> (Result<Answer>, bool) {
        let mut link = self.link();
        let mut again = false;
        loop {
            match link.call_with(op, names, a, b, data) {
                Ok(answer) => return (Ok(answer), again),
                Err(Fail::Said(e)) => return (Err(e), again),
                Err(Fail::Gone) => {}
            }
            let mut away = SyncWait::new();
            link = loop {
                let now = self.link();
                if !Arc::ptr_eq(&now, &link) {
                    break now;
                }
                if !self.mounted.load(Ordering::Acquire) || self.deserted(&link) {
                    return (Err(FsError::DeviceError), again);
                }
                link::between_looks(&mut away);
            };
            again = true;
        }
    }

    /// [`ask`](Self::ask) about one path, for what may be asked twice.
    fn call(&self, op: u32, path: &str, a: u64, b: u32) -> Result<Answer> {
        self.ask(op, path.as_bytes(), a, b, &[]).0
    }

    fn writable(&self) -> Result<()> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        Ok(())
    }

    fn changing_dirs(&self) -> ChangingDirs<'_> {
        let mut dirs = self.dirs.lock();
        dirs.changing += 1;
        dirs.changes += 1;
        ChangingDirs(self)
    }

    /// The entries of the directory `path`: as listed before, or from the
    /// server. No lock is held while it answers.
    fn listing(&self, path: &str) -> Result<Arc<Listing>> {
        // A listing the server made while a directory was being changed
        // may or may not have the change, so it is asked for again; one
        // that keeps being overtaken is used this once and not kept.
        for last in [false, false, true] {
            let before = {
                let dirs = self.dirs.lock();
                if let Some(listed) = dirs.listed.get(path) {
                    return Ok(listed.clone());
                }
                (dirs.changing == 0).then_some(dirs.changes)
            };
            let entries = self.ask_listing(path)?;
            let listing: Listing = entries
                .iter()
                .map(|entry| {
                    Some(Slot {
                        name: entry.name.clone(),
                        node: self.node_of(path, entry),
                    })
                })
                .collect();
            let listing = Arc::new(listing);
            let mut dirs = self.dirs.lock();
            if let Some(listed) = dirs.listed.get(path) {
                return Ok(listed.clone());
            }
            if dirs.changing == 0 && before == Some(dirs.changes) {
                dirs.listed.insert(path.to_string(), listing.clone());
                return Ok(listing);
            }
            if last {
                return Ok(listing);
            }
        }
        unreachable!("the last round returns")
    }

    fn ask_listing(&self, path: &str) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut data = Vec::new();
        let mut next = 0u64;
        while next != LIST_END {
            let answer = self.call(OP_LIST, path, next, 0)?;
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
        Ok(entries)
    }

    /// The entry called `name` in the directory `dir`, as the server has
    /// it now.
    fn entry_named(&self, dir: &str, name: &str) -> Result<Entry> {
        self.ask_listing(dir)?
            .into_iter()
            .find(|entry| self.same_name(&entry.name, name))
            .ok_or(FsError::EntryNotFound)
    }

    /// The one entry a reply describes: what was made, or renamed.
    fn entry_of(answer: Answer) -> Result<Entry> {
        let mut data = alloc::vec![0u8; answer.len];
        answer.data(0, &mut data)?;
        drop(answer);
        let mut entries = Vec::new();
        wire::records(&data, 1, &mut entries).ok_or(FsError::DeviceError)?;
        entries.pop().ok_or(FsError::DeviceError)
    }

    fn same_name(&self, a: &str, b: &str) -> bool {
        if self.fold_case {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    }

    /// Where in `listing` the entry called `name` is.
    fn position(&self, listing: &Listing, name: &str) -> Option<usize> {
        listing
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|s| self.same_name(&s.name, name)))
    }

    /// The filesystem's numbers: as last asked, or with `fresh` asked
    /// again when something was written since.
    fn numbers(&self, fresh: bool) -> Option<Numbers> {
        let known = *self.info.lock();
        if known.is_some() && !(fresh && self.info_stale.load(Ordering::Acquire)) {
            return known;
        }
        if !self.mounted.load(Ordering::Acquire) {
            return None;
        }
        self.info_stale.store(false, Ordering::Release);
        let Ok(answer) = self.call(OP_STATFS, "", 0, 0) else {
            return known;
        };
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

    /// Drop the removed file's pages: its number is no other file's, but
    /// they are memory nothing will read.
    fn forget_pages(&self, node: &Arc<Node>) {
        if !node.is_dir {
            let inode: Arc<dyn INode> = Arc::new(self.inode(node.clone()));
            super::file::cache_forget(&inode);
        }
    }
}

/// Take the entry at `at` out of `listing`: its place stays, empty, unless
/// it was the last.
fn take_slot(listing: &mut Listing, at: usize) -> Option<Slot> {
    let slot = listing.get_mut(at)?.take();
    while listing.last().is_some_and(|last| last.is_none()) {
        listing.pop();
    }
    slot
}

/// Put an entry in `listing`: in the first empty place, or at the end.
fn put_slot(listing: &mut Listing, slot: Slot) {
    match listing.iter_mut().find(|place| place.is_none()) {
        Some(place) => *place = Some(slot),
        None => listing.push(Some(slot)),
    }
}

impl FileSystem for ServedFs {
    /// What was written is the server's already: this has it reach the
    /// device. A mount with no server, and none on its way, has nothing of
    /// the kernel's to write.
    fn sync(&self) -> Result<()> {
        if self.read_only || !self.mounted.load(Ordering::Acquire) || self.deserted(&self.link()) {
            return Ok(());
        }
        self.call(OP_SYNC, "", 0, 0).map(|_| ())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::new(self.inode(self.root.clone()))
    }

    fn info(&self) -> FsInfo {
        let n = self.numbers(true).unwrap_or(UNASKED);
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

/// A file or a directory of a served filesystem, as the VFS holds it.
struct ServedINode {
    fs: Arc<ServedFs>,
    node: Arc<Node>,
}

impl ServedINode {
    fn is_dir(&self) -> bool {
        self.node.is_dir
    }

    fn dir_path(&self) -> Result<String> {
        if !self.is_dir() {
            return Err(FsError::NotDir);
        }
        self.node.live_path()
    }

    fn parent(&self) -> Result<ServedINode> {
        let path = self.node.path();
        let (dir, name) = split_path(&path);
        // The top is its own parent: the mount above it takes `..` from
        // there.
        if name.is_empty() || dir.is_empty() {
            return Ok(self.fs.inode(self.fs.root.clone()));
        }
        let (above, dir_name) = split_path(dir);
        let node = self
            .fs
            .listing(above)?
            .iter()
            .flatten()
            .find(|s| s.name == dir_name)
            .map(|s| s.node.clone())
            .ok_or(FsError::EntryNotFound)?;
        Ok(self.fs.inode(node))
    }

    fn metadata_of(&self, node: &Node) -> Metadata {
        let size = node.size.load(Ordering::Relaxed) as usize;
        let at = |sec: &AtomicI64| Timespec {
            sec: sec.load(Ordering::Relaxed),
            nsec: 0,
        };
        Metadata {
            dev: self.fs.dev_id as usize,
            inode: node.ino as usize,
            size,
            blk_size: self.fs.numbers(false).unwrap_or(UNASKED).bsize,
            // `st_blocks` counts 512-byte units whatever the block size.
            blocks: size.div_ceil(512),
            atime: at(&node.atime),
            mtime: at(&node.mtime),
            ctime: at(&node.ctime),
            type_: if node.is_dir {
                FileType::Dir
            } else {
                FileType::File
            },
            mode: if node.is_dir { 0o755 } else { 0o644 },
            nlinks: match (node.is_gone(), node.is_dir) {
                (true, _) => 0,
                (false, true) => 2,
                (false, false) => 1,
            },
            uid: 0,
            gid: 0,
            rdev: 0,
        }
    }

    /// A name a directory entry can have, as far as the kernel is
    /// concerned; the server has its own say.
    fn check_name(name: &str) -> Result<()> {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            return Err(FsError::InvalidParam);
        }
        Ok(())
    }
}

impl INode for ServedINode {
    /// The bytes as the server reads them, each call a request: what fills
    /// the page cache. `read(2)` comes through `File`'s `served_read` instead.
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        if self.is_dir() {
            return Err(FsError::IsDir);
        }
        let size = self.node.size.load(Ordering::Relaxed) as usize;
        if offset >= size {
            return Ok(0);
        }
        let path = self.node.live_path()?;
        let want = buf.len().min(size - offset);
        let mut done = 0;
        while done < want {
            let ask = (want - done).min(DATA_AREA);
            let answer = self
                .fs
                .call(OP_READ, &path, (offset + done) as u64, ask as u32)?;
            let n = answer.count.min(answer.len).min(ask);
            if n == 0 {
                break;
            }
            answer.data(0, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(done)
    }

    /// The bytes go to the server, a window of them at a time, and are
    /// written when this returns. Fewer than asked when the volume filled.
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        if self.is_dir() {
            return Err(FsError::IsDir);
        }
        self.fs.writable()?;
        if buf.is_empty() {
            return Ok(0);
        }
        let path = self.node.live_path()?;
        let _changing = Changing::new(&self.node);
        let mut done = 0;
        let mut failed = None;
        while done < buf.len() {
            let piece = &buf[done..(done + DATA_AREA).min(buf.len())];
            // Written twice, when the server died under it, it is the same
            // bytes in the same place.
            let wrote = self
                .fs
                .ask(
                    OP_WRITE,
                    path.as_bytes(),
                    (offset + done) as u64,
                    piece.len() as u32,
                    piece,
                )
                .0
                .map(|answer| answer.count.min(piece.len()));
            match wrote {
                Ok(n) => {
                    done += n;
                    if n < piece.len() {
                        failed = Some(FsError::NoDeviceSpace);
                        break;
                    }
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        if done > 0 {
            self.node
                .size
                .fetch_max((offset + done) as u64, Ordering::Relaxed);
            self.node.mtime.store(now(), Ordering::Relaxed);
            self.fs.info_stale.store(true, Ordering::Release);
        }
        match failed {
            Some(e) if done == 0 => Err(e),
            _ => Ok(done),
        }
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: !self.fs.read_only,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(self.metadata_of(&self.node))
    }

    /// The times are the node's from here on, and a file's time of last
    /// writing goes to the server; the owner and the mode are not the
    /// filesystem's to keep.
    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        self.fs.writable()?;
        let node = &self.node;
        node.atime.store(metadata.atime.sec, Ordering::Relaxed);
        node.ctime.store(metadata.ctime.sec, Ordering::Relaxed);
        let before = node.mtime.swap(metadata.mtime.sec, Ordering::Relaxed);
        if before != metadata.mtime.sec && !node.is_dir && !node.is_gone() {
            let path = node.path();
            self.fs
                .call(OP_SETTIME, &path, metadata.mtime.sec as u64, 0)?;
        }
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<()> {
        if self.is_dir() {
            return Err(FsError::IsDir);
        }
        self.fs.writable()?;
        let path = self.node.live_path()?;
        // The length it has: what `O_TRUNC` asks of a file just made.
        if self.node.size.load(Ordering::Relaxed) == len as u64 {
            return Ok(());
        }
        let _changing = Changing::new(&self.node);
        self.fs.call(OP_TRUNCATE, &path, len as u64, 0)?;
        self.node.size.store(len as u64, Ordering::Relaxed);
        self.node.mtime.store(now(), Ordering::Relaxed);
        self.fs.info_stale.store(true, Ordering::Release);
        Ok(())
    }

    fn create(&self, name: &str, type_: FileType, _mode: u32) -> Result<Arc<dyn INode>> {
        let dir = self.dir_path()?;
        self.fs.writable()?;
        Self::check_name(name)?;
        let mode = match type_ {
            FileType::File => 0,
            FileType::Dir => MODE_DIR,
            _ => return Err(FsError::NotSupported),
        };
        if self.fs.position(&*self.fs.listing(&dir)?, name).is_some() {
            return Err(FsError::EntryExist);
        }
        let _changing = self.fs.changing_dirs();
        let path = join_path(&dir, name);
        let entry = match self.fs.ask(OP_CREATE, path.as_bytes(), mode as u64, 0, &[]) {
            // It was not there before this asked, and the server that was
            // asked first died: that one made it.
            (Err(FsError::EntryExist), true) => self.fs.entry_named(&dir, name)?,
            (answer, _) => ServedFs::entry_of(answer?)?,
        };
        if entry.is_dir != (type_ == FileType::Dir) {
            return Err(FsError::DeviceError);
        }
        let node = self.fs.node_of(&dir, &entry);
        if let Some(listing) = self.fs.dirs.lock().listed.get_mut(&dir) {
            put_slot(
                Arc::make_mut(listing),
                Slot {
                    name: entry.name,
                    node: node.clone(),
                },
            );
        }
        self.node.mtime.store(now(), Ordering::Relaxed);
        Ok(Arc::new(self.fs.inode(node)))
    }

    fn unlink(&self, name: &str) -> Result<()> {
        let dir = self.dir_path()?;
        self.fs.writable()?;
        if name == "." || name == ".." {
            return Err(FsError::DirNotEmpty);
        }
        let listing = self.fs.listing(&dir)?;
        let at = self
            .fs
            .position(&listing, name)
            .ok_or(FsError::EntryNotFound)?;
        let Some(Slot { node, .. }) = listing[at].clone() else {
            return Err(FsError::EntryNotFound);
        };
        drop(listing);
        let path = node.path();
        let _changing = self.fs.changing_dirs();
        match self.fs.ask(OP_REMOVE, path.as_bytes(), 0, 0, &[]) {
            // The server that was asked first removed it, and died.
            (Err(FsError::EntryNotFound), true) => {}
            (answer, _) => drop(answer?),
        }
        node.gone.store(true, Ordering::Release);
        {
            let mut dirs = self.fs.dirs.lock();
            if let Some(listing) = dirs.listed.get_mut(&dir) {
                let listing = Arc::make_mut(listing);
                // By the node, not the place it was found at: that was
                // before the server was asked.
                let at = listing
                    .iter()
                    .position(|s| s.as_ref().is_some_and(|s| Arc::ptr_eq(&s.node, &node)));
                if let Some(at) = at {
                    take_slot(listing, at);
                }
            }
            if node.is_dir {
                dirs.listed.remove(&path);
            }
        }
        self.node.mtime.store(now(), Ordering::Relaxed);
        self.fs.forget_pages(&node);
        Ok(())
    }

    /// `old_name` here becomes `new_name` in `target`, in place of what is
    /// there. The node stays the one it was, under its new path, and so do
    /// the nodes below a directory.
    fn move_(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> Result<()> {
        let from_dir = self.dir_path()?;
        self.fs.writable()?;
        let dest = target
            .as_any_ref()
            .downcast_ref::<ServedINode>()
            .filter(|dest| Arc::ptr_eq(&self.fs, &dest.fs))
            .ok_or(FsError::NotSameFs)?;
        let to_dir = dest.dir_path()?;
        if old_name == "." || old_name == ".." {
            return Err(FsError::InvalidParam);
        }
        Self::check_name(new_name)?;
        let listing = self.fs.listing(&from_dir)?;
        let node = self
            .fs
            .position(&listing, old_name)
            .and_then(|at| listing[at].clone())
            .map(|slot| slot.node)
            .ok_or(FsError::EntryNotFound)?;
        drop(listing);
        // The other directory's entries too, so that what is replaced there
        // is known to be.
        self.fs.listing(&to_dir)?;
        let old_path = node.path();
        let new_path = join_path(&to_dir, new_name);
        if old_path == new_path {
            return Ok(());
        }
        if node.is_dir && under(&to_dir, &old_path).is_some() {
            return Err(FsError::InvalidParam);
        }
        let names = alloc::format!("{old_path}\0{new_path}");
        let _changing = self.fs.changing_dirs();
        let entry = match self.fs.ask(OP_RENAME, names.as_bytes(), 0, 0, &[]) {
            // The server that was asked first renamed it, and died.
            (Err(FsError::EntryNotFound), true) => self.fs.entry_named(&to_dir, new_name)?,
            (answer, _) => ServedFs::entry_of(answer?)?,
        };
        let new_path = join_path(&to_dir, &entry.name);

        let mut replaced = None;
        {
            let mut dirs = self.fs.dirs.lock();
            if let Some(listing) = dirs.listed.get_mut(&from_dir) {
                let listing = Arc::make_mut(listing);
                let at = listing
                    .iter()
                    .position(|s| s.as_ref().is_some_and(|s| Arc::ptr_eq(&s.node, &node)));
                if let Some(at) = at {
                    take_slot(listing, at);
                }
            }
            match dirs.listed.get_mut(&to_dir) {
                Some(listing) => {
                    let listing = Arc::make_mut(listing);
                    if let Some(at) = self.fs.position(listing, &entry.name) {
                        replaced = take_slot(listing, at).map(|slot| slot.node);
                    }
                    put_slot(
                        listing,
                        Slot {
                            name: entry.name,
                            node: node.clone(),
                        },
                    );
                }
                // Nowhere to keep it: the next listing makes a new node of
                // it, and this one must not go on as a second.
                None => node.gone.store(true, Ordering::Release),
            }
            if let Some(replaced) = &replaced {
                replaced.gone.store(true, Ordering::Release);
                if replaced.is_dir {
                    dirs.listed.remove(&new_path);
                }
            }
            *node.path.lock() = new_path.clone();
            if node.is_dir {
                // What was listed below it is below its new path.
                let below: Vec<String> = dirs
                    .listed
                    .keys()
                    .filter(|path| under(path, &old_path).is_some())
                    .cloned()
                    .collect();
                for was in below {
                    let Some(listing) = dirs.listed.remove(&was) else {
                        continue;
                    };
                    let is = alloc::format!("{new_path}{}", &was[old_path.len()..]);
                    for slot in listing.iter().flatten() {
                        *slot.node.path.lock() = join_path(&is, &slot.name);
                    }
                    dirs.listed.insert(is, listing);
                }
            }
        }
        let at = now();
        self.node.mtime.store(at, Ordering::Relaxed);
        dest.node.mtime.store(at, Ordering::Relaxed);
        if let Some(replaced) = replaced {
            self.fs.forget_pages(&replaced);
        }
        Ok(())
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        if !self.is_dir() {
            return Err(FsError::NotDir);
        }
        match name {
            "." => Ok(Arc::new(self.fs.inode(self.node.clone()))),
            ".." => Ok(Arc::new(self.parent()?)),
            name => {
                let listing = self.fs.listing(&self.node.live_path()?)?;
                let at = self
                    .fs
                    .position(&listing, name)
                    .ok_or(FsError::EntryNotFound)?;
                let slot = listing[at].as_ref().ok_or(FsError::EntryNotFound)?;
                Ok(Arc::new(self.fs.inode(slot.node.clone())))
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
                .listing(&self.node.live_path()?)?
                .iter()
                .flatten()
                .nth(i - 2)
                .map(|s| s.name.clone())
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
                let listing = self.fs.listing(&self.node.live_path()?)?;
                let slot = listing
                    .iter()
                    .flatten()
                    .nth(i - 2)
                    .ok_or(FsError::EntryNotFound)?;
                Ok((self.metadata_of(&slot.node), slot.name.clone()))
            }
        }
    }

    fn sync_all(&self) -> Result<()> {
        self.fs.sync()
    }

    fn sync_data(&self) -> Result<()> {
        self.fs.sync()
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
/// itself being a request to a process. Not a file that was removed, which
/// has no page cache any more and nothing to read.
pub(super) fn is_file(inode: &dyn INode) -> bool {
    inode
        .downcast_ref::<ServedINode>()
        .is_some_and(|i| !i.is_dir() && !i.node.is_gone())
}

/// The entry of the served directory `inode` at the place `at` or the
/// first after it, and the place after that one: how an open directory is
/// read, by places that stay where they are while entries are removed
/// ([`Listing`]). `None` for anything but a served directory.
pub(super) fn entry_at(inode: &dyn INode, at: usize) -> Option<Result<(Metadata, String, usize)>> {
    let inode = inode.downcast_ref::<ServedINode>().filter(|i| i.is_dir())?;
    Some(match at {
        0 | 1 => inode
            .get_entry_with_metadata(at)
            .map(|(m, n)| (m, n, at + 1)),
        at => (|| {
            let listing = inode.fs.listing(&inode.node.live_path()?)?;
            let (place, slot) = listing
                .iter()
                .enumerate()
                .skip(at - 2)
                .find_map(|(place, slot)| Some((place, slot.as_ref()?)))
                .ok_or(FsError::EntryNotFound)?;
            Ok((inode.metadata_of(&slot.node), slot.name.clone(), place + 3))
        })(),
    })
}

/// Before the bytes of the served file `inode` are fetched to be kept in
/// its page cache: what [`if_unchanged`] takes afterwards. `None` while a
/// write to the file is with the server, when what is fetched may be from
/// before it or after it.
pub(super) fn read_token(inode: &dyn INode) -> Option<u64> {
    let inode = inode.downcast_ref::<ServedINode>()?;
    let changes = inode.node.changes.lock();
    (changes.0 == 0).then_some(changes.1)
}

/// Run `keep`, which puts fetched bytes in the page cache, unless the file
/// was written to since `token` was taken. No write starts while it runs,
/// so the pages it keeps are the file's, and a write that starts after
/// finds them there and changes them (`cache_overlay_write`).
pub(super) fn if_unchanged(inode: &dyn INode, token: u64, keep: impl FnOnce()) -> bool {
    let Some(inode) = inode.downcast_ref::<ServedINode>() else {
        return false;
    };
    let changes = inode.node.changes.lock();
    let unchanged = *changes == (0, token);
    if unchanged {
        keep();
    }
    unchanged
}

/// What [`serve`] made of a server: the mount it serves and its turn at
/// it, for [`ServedFs::server_left`] when its end of the FIFO closes.
pub struct Served {
    pub fs: Arc<ServedFs>,
    pub turn: usize,
    /// A mount taken over: the memory its first server gave for the
    /// servers after it, when it gave any.
    pub kept: Option<Arc<VmObject>>,
    pub taken_over: bool,
}

/// `eclipse_fs_serve`: mount at `target` the filesystem a process serves on
/// `fifo`, through `window`. `source` and `fstype` are what the mount table
/// shows for it. The caller needs what `mount(2)` needs.
///
/// When `target` already is a served mount with that source and type whose
/// server is gone, the process takes that mount over instead, as it is:
/// the flags must be the ones it was made with. `kept` is memory the mount
/// holds for its servers; the one taking over is given the first one's
/// back. `EBUSY` while the mount has a server; with [`SERVE_TAKE_OVER`],
/// `ENOENT` when there is no such mount to take.
#[allow(clippy::too_many_arguments)]
pub fn serve(
    proc: &LinuxProcess,
    fifo: Arc<Fifo>,
    window: Arc<VmObject>,
    kept: Option<Arc<VmObject>>,
    target: &str,
    source: &str,
    fstype: &str,
    flags: usize,
) -> LxResult<Served> {
    if !proc.capable(crate::process::CAP_SYS_ADMIN) {
        return Err(LxError::EPERM);
    }
    let shown = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic());
    if flags & !(SERVE_FOLD_CASE | SERVE_READ_ONLY | SERVE_TAKE_OVER) != 0
        || !target.starts_with('/')
        || !shown(source)
        || !shown(fstype)
    {
        return Err(LxError::EINVAL);
    }
    let link = Arc::new(Link::new(fifo, window).map_err(|_| LxError::EINVAL)?);
    let made_with = flags & !SERVE_TAKE_OVER;
    let at = super::mount_ops::normalize_target(target);
    let known = MOUNTS
        .lock()
        .iter()
        .find(|(place, fs)| {
            let shown = fs.shown.lock();
            *place == at && shown.0 == source && shown.1 == fstype
        })
        .map(|(_, fs)| fs.clone());
    if let Some(fs) = known {
        if fs.shown.lock().2 != made_with {
            return Err(LxError::EINVAL);
        }
        let turn = fs.take_over(link).ok_or(LxError::EBUSY)?;
        info!("served fs: {at} has a server again");
        let kept = fs.kept.lock().clone();
        return Ok(Served {
            fs,
            turn,
            kept,
            taken_over: true,
        });
    }
    if flags & SERVE_TAKE_OVER != 0 {
        return Err(LxError::ENOENT);
    }
    let read_only = flags & SERVE_READ_ONLY != 0;
    let fs = ServedFs::new(link, flags & SERVE_FOLD_CASE != 0, read_only);
    *fs.kept.lock() = kept;
    *fs.shown.lock() = (String::from(source), String::from(fstype), made_with);
    let target = super::mount_ops::mount_served(target, source, fstype, fs.clone(), read_only)?;
    fs.mounted.store(true, Ordering::Release);
    MOUNTS.lock().push((target, fs.clone()));
    Ok(Served {
        fs,
        turn: 1,
        kept: None,
        taken_over: false,
    })
}

/// `target` was unmounted, with whatever was mounted under it: the servers
/// of the served mounts among them are told, by their FIFO closing.
pub(super) fn unmounted(target: &str) {
    let gone: Vec<Arc<ServedFs>> = {
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
        gone.into_iter().map(|(_, fs)| fs).collect()
    };
    for fs in gone {
        if !fs.read_only && fs.link().looked_dead() && fs.kept.lock().is_some() {
            warn!(
                "served fs: {} is unmounted with no server: what its last one had not \
                 written to the device is lost",
                fs.shown.lock().0
            );
        }
        // Nothing waits for a next server from here on.
        fs.mounted.store(false, Ordering::Release);
        fs.link().close();
        fs.kept.lock().take();
    }
}
