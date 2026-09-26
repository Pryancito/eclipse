#![cfg_attr(not(any(test, feature = "std")), no_std)]

extern crate alloc;
extern crate log;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;
use rcore_fs::vfs::*;
use spin::{RwLock, RwLockWriteGuard};

/// Size of a storage block. File content is kept as a list of independently
/// allocated blocks instead of one contiguous `Vec<u8>`, so that growing a
/// file (the common sequential-write case) only appends a block rather than
/// reallocating and copying the entire file each time it outgrows its
/// capacity. This mirrors how a real kernel keeps file data in page-sized
/// units and removes the O(n) realloc-copy that dominated large writes.
const BLOCK: usize = 4096;

/// Paged byte buffer: `len` logical bytes stored SPARSELY across `BLOCK`-sized
/// blocks — `None` slots are holes that read back as zeros and cost no memory.
///
/// Sparseness is load-bearing, matching Linux tmpfs/memfd semantics: clients
/// size shm files far beyond what they touch. foot ftruncates its
/// wl_shm buffer pool memfd to 512 MiB and then writes only the few MiB it
/// actually renders; the previous eager zero-filled `resize` allocated all
/// 131k blocks from the kernel heap in that one call and OOM-panicked the
/// whole desktop. Blocks now materialize only when first written.
#[derive(Default)]
struct PagedBytes {
    blocks: Vec<Option<alloc::boxed::Box<[u8]>>>,
    len: usize,
}

impl PagedBytes {
    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    fn alloc_zeroed_block() -> alloc::boxed::Box<[u8]> {
        alloc::vec![0u8; BLOCK].into_boxed_slice()
    }

    /// Resize to `new_len`. Growing exposes a hole (zeros, no allocation);
    /// shrinking frees now-unreachable blocks and zeroes the stale tail of the
    /// last partial block so a later grow does not resurrect old bytes.
    fn resize(&mut self, new_len: usize) {
        let nblocks = new_len.div_ceil(BLOCK);
        if nblocks < self.blocks.len() {
            self.blocks.truncate(nblocks);
        } else {
            // Hole: slots exist but hold no storage until first written.
            self.blocks.resize_with(nblocks, || None);
        }
        if new_len < self.len {
            let off = new_len % BLOCK;
            if off != 0 {
                if let Some(Some(b)) = self.blocks.get_mut(new_len / BLOCK) {
                    for x in &mut b[off..] {
                        *x = 0;
                    }
                }
            }
        }
        self.len = new_len;
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if offset >= self.len {
            return 0;
        }
        let n = buf.len().min(self.len - offset);
        let mut done = 0;
        while done < n {
            let pos = offset + done;
            let bi = pos / BLOCK;
            let bo = pos % BLOCK;
            let chunk = (BLOCK - bo).min(n - done);
            match self.blocks.get(bi).and_then(|b| b.as_ref()) {
                Some(b) => buf[done..done + chunk].copy_from_slice(&b[bo..bo + chunk]),
                // Hole: reads as zeros.
                None => buf[done..done + chunk].fill(0),
            }
            done += chunk;
        }
        n
    }

    fn write_at(&mut self, offset: usize, buf: &[u8]) {
        let end = offset + buf.len();
        let nblocks = end.div_ceil(BLOCK);
        if nblocks > self.blocks.len() {
            // Any implicit gap between old EOF and `offset` stays a hole.
            self.blocks.resize_with(nblocks, || None);
        }
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done;
            let bi = pos / BLOCK;
            let bo = pos % BLOCK;
            let chunk = (BLOCK - bo).min(buf.len() - done);
            match &mut self.blocks[bi] {
                Some(b) => b[bo..bo + chunk].copy_from_slice(&buf[done..done + chunk]),
                slot @ None => {
                    // First write materializes the block. A fully covered block
                    // skips the zero-fill. An all-zero write into a hole stays a
                    // hole (no allocation) — matches sparse file semantics and
                    // avoids heap densification from zero writeback.
                    if buf[done..done + chunk].iter().all(|&b| b == 0) {
                        done += chunk;
                        continue;
                    }
                    let b = if bo == 0 && chunk == BLOCK {
                        let mut v = alloc::vec::Vec::with_capacity(BLOCK);
                        v.extend_from_slice(&buf[done..done + chunk]);
                        v.into_boxed_slice()
                    } else {
                        let mut b = Self::alloc_zeroed_block();
                        b[bo..bo + chunk].copy_from_slice(&buf[done..done + chunk]);
                        b
                    };
                    *slot = Some(b);
                }
            }
            done += chunk;
        }
        if end > self.len {
            self.len = end;
        }
    }
}

pub struct RamFS {
    root: Arc<LockedINode>,
    /// Held for the whole of a directory rename.
    ///
    /// Moving a directory has to check that the destination is not inside the
    /// directory being moved, and that check is worth nothing if another
    /// rename can run between it and the move: two threads swapping
    /// `a/b` and `b/a` would each see a legal destination and leave a cycle
    /// with no name, unreachable and unfreeable. Linux serialises
    /// cross-directory renames on `s_vfs_rename_mutex` for exactly this, and
    /// so does this. Taken before any inode lock, and by nothing else.
    rename: spin::Mutex<()>,
}

impl FileSystem for RamFS {
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::clone(&self.root) as _
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            bsize: 0,
            frsize: 0,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            namemax: 0,
        }
    }
}

impl RamFS {
    pub fn new() -> Arc<Self> {
        let root = Arc::new(LockedINode(RwLock::new(RamFSINode {
            this: Weak::default(),
            parent: Weak::default(),
            children: BTreeMap::new(),
            content: PagedBytes::default(),
            extra: Metadata {
                dev: 0,
                inode: new_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: Timespec { sec: 0, nsec: 0 },
                mtime: Timespec { sec: 0, nsec: 0 },
                ctime: Timespec { sec: 0, nsec: 0 },
                type_: FileType::Dir,
                mode: 0o777,
                nlinks: 1,
                uid: 0,
                gid: 0,
                rdev: 0,
            },
            fs: Weak::default(),
        })));
        let fs = Arc::new(RamFS {
            root,
            rename: spin::Mutex::new(()),
        });
        let mut root = fs.root.0.write();
        root.parent = Arc::downgrade(&fs.root);
        root.this = Arc::downgrade(&fs.root);
        root.fs = Arc::downgrade(&fs);
        // The root's `extra.inode` is the id `new_inode_id()` gave it above.
        // It used to be overwritten here with
        // `Arc::into_raw(root.this.upgrade().unwrap()) as usize`, which is two
        // bugs in one line. The number is what `stat(2)` answers as `st_ino`,
        // so `stat /tmp` handed any unprivileged process a live kernel heap
        // ADDRESS -- on bare metal a higher-half one, which is as much of the
        // kernel's layout as KASLR exists to hide. And `Arc::into_raw` takes
        // the reference without ever giving it back: the strong count stayed
        // one too high forever, so the root node -- and through its `children`
        // map the whole filesystem -- could never be freed.
        //
        // Nothing wanted the pointer. What the id has to be is unique, which
        // the counter already guarantees, and [`lock_multiple`] now relies on
        // exactly that.
        drop(root);
        fs
    }
}

struct RamFSINode {
    /// Reference to parent INode
    parent: Weak<LockedINode>,
    /// Reference to myself
    this: Weak<LockedINode>,
    /// Reference to children INodes
    children: BTreeMap<String, Arc<LockedINode>>,
    /// Content of the file (paged storage; see [`PagedBytes`])
    content: PagedBytes,
    /// INode metadata
    extra: Metadata,
    /// Reference to FS
    fs: Weak<RamFS>,
}

struct LockedINode(RwLock<RamFSINode>);

impl LockedINode {
    /// Move the directory `old_name` out of `self` and into `target` under
    /// `new_name`.
    ///
    /// The entry changes hands and the directory's `..` follows it, so the two
    /// `children` maps and the moved node's `parent` all have to change
    /// together. Three inodes at most, and `self` and `target` are the same one
    /// for a rename inside one directory, so the locks are taken through
    /// [`lock_multiple`] over the DISTINCT set -- see what that function needs
    /// from its callers.
    fn move_dir(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> Result<()> {
        let target = target
            .downcast_ref::<LockedINode>()
            .ok_or(FsError::NotSameFs)?;
        // Serialise against every other directory rename in this filesystem
        // before looking at anything, so the loop check below still holds when
        // the move happens. See `RamFS::rename`.
        let fs = self.0.read().fs.upgrade().ok_or(FsError::EntryNotFound)?;
        let _rename = fs.rename.lock();

        let elem = match self.0.read().children.get(old_name) {
            Some(elem) => Arc::clone(elem),
            None => return Err(FsError::EntryNotFound),
        };
        if target.0.read().extra.type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }
        // A directory cannot be moved into itself or into anything below it:
        // the subtree would have no name left, so nothing could reach it and
        // nothing could free it. Linux answers `EINVAL`; walk up from the
        // destination and refuse if the directory being moved is on the way.
        //
        // `ptr_eq` and not the inode id, because the id is only unique within
        // one filesystem and this walk cannot leave one anyway.
        let mut at = target.0.read().this.upgrade();
        while let Some(node) = at {
            if Arc::ptr_eq(&node, &elem) {
                return Err(FsError::InvalidParam);
            }
            let parent = node.0.read().parent.upgrade();
            at = match parent {
                // The root is its own parent, so stop there rather than
                // walking in place forever.
                Some(p) if !Arc::ptr_eq(&p, &node) => Some(p),
                _ => None,
            };
        }

        let same_dir = core::ptr::eq(self, target);
        let locks: &[&RwLock<RamFSINode>] = if same_dir {
            &[&self.0, &elem.0]
        } else {
            &[&self.0, &target.0, &elem.0]
        };
        let mut guards = lock_multiple(locks).into_iter();
        let mut from = guards.next().unwrap();
        if same_dir {
            let mut moved = guards.next().unwrap();
            // The caller cleared the destination before this lock was taken, so
            // say again under it that the name is free.
            if from.children.contains_key(new_name) {
                return Err(FsError::EntryExist);
            }
            // Whether the removal comes before or after the insert makes no
            // difference: `move_` returns early when the two names are the same
            // inode, so `old_name != new_name` holds here. Written down because
            // swapping the two lines is a mutation no test can catch, and the
            // reason is the guard above this function rather than anything in
            // it.
            from.children.remove(old_name);
            let this = moved.this.upgrade().ok_or(FsError::EntryNotFound)?;
            from.children.insert(String::from(new_name), this);
            // `..` did not change, but say so where a reader looks for it.
            moved.parent = Weak::clone(&from.this);
        } else {
            let mut to = guards.next().unwrap();
            let mut moved = guards.next().unwrap();
            if to.children.contains_key(new_name) {
                return Err(FsError::EntryExist);
            }
            from.children.remove(old_name);
            let this = moved.this.upgrade().ok_or(FsError::EntryNotFound)?;
            to.children.insert(String::from(new_name), this);
            moved.parent = Weak::clone(&to.this);
        }
        Ok(())
    }
}

impl INode for LockedINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let file = self.0.read();
        if file.extra.type_ == FileType::Dir {
            return Err(FsError::IsDir);
        }
        let n = file.content.read_at(offset, buf);
        Ok(n)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let mut file = self.0.write();
        if file.extra.type_ == FileType::Dir {
            return Err(FsError::IsDir);
        }
        let before = file.content.len();
        file.content.write_at(offset, buf);
        // Growth telemetry: a ramfs file silently growing without bound is a
        // kernel-heap leak (each 4 KiB block is a heap allocation) — the
        // desktop OOM was ~456 MiB of such blocks with no obvious owner. Warn
        // each time a file crosses another 32 MiB boundary, with its inode id
        // so the writer can be identified.
        let after = file.content.len();
        const STEP: usize = 32 * 1024 * 1024;
        if after / STEP > before / STEP {
            log::warn!(
                "[ramfs] inode={} grew to {} MiB (write_at offset={})",
                file.extra.inode,
                after >> 20,
                offset
            );
        }
        Ok(buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        let file = self.0.read();
        if file.extra.type_ == FileType::Dir {
            return Err(FsError::IsDir);
        }
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let file = self.0.read();
        let mut metadata = file.extra.clone();
        metadata.size = file.content.len();
        Ok(metadata)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        let mut file = self.0.write();
        file.extra.atime = metadata.atime;
        file.extra.mtime = metadata.mtime;
        file.extra.ctime = metadata.ctime;
        file.extra.mode = metadata.mode;
        file.extra.uid = metadata.uid;
        file.extra.gid = metadata.gid;
        Ok(())
    }

    fn sync_all(&self) -> Result<()> {
        Ok(())
    }

    fn sync_data(&self) -> Result<()> {
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<()> {
        let mut file = self.0.write();
        if file.extra.type_ == FileType::File {
            // See write_at: name any file resized past each 32 MiB boundary.
            const STEP: usize = 32 * 1024 * 1024;
            if len / STEP > file.content.len() / STEP {
                log::warn!(
                    "[ramfs] inode={} resized {} -> {} MiB",
                    file.extra.inode,
                    file.content.len() >> 20,
                    len >> 20
                );
            }
            file.content.resize(len);
            Ok(())
        } else {
            Err(FsError::NotFile)
        }
    }

    fn create2(
        &self,
        name: &str,
        type_: FileType,
        mode: u32,
        data: usize,
    ) -> Result<Arc<dyn INode>> {
        let mut file = self.0.write();
        if file.extra.type_ == FileType::Dir {
            if name == "." || name == ".." {
                return Err(FsError::EntryExist);
            }
            if file.children.contains_key(name) {
                return Err(FsError::EntryExist);
            }
            let temp_file = Arc::new(LockedINode(RwLock::new(RamFSINode {
                parent: Weak::clone(&file.this),
                this: Weak::default(),
                children: BTreeMap::new(),
                content: PagedBytes::default(),
                extra: Metadata {
                    dev: 0,
                    inode: new_inode_id(),
                    size: 0,
                    blk_size: 0,
                    blocks: 0,
                    atime: Timespec { sec: 0, nsec: 0 },
                    mtime: Timespec { sec: 0, nsec: 0 },
                    ctime: Timespec { sec: 0, nsec: 0 },
                    type_,
                    mode: mode as u16,
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    rdev: data,
                },
                fs: Weak::clone(&file.fs),
            })));
            temp_file.0.write().this = Arc::downgrade(&temp_file);
            file.children
                .insert(String::from(name), Arc::clone(&temp_file));
            Ok(temp_file)
        } else {
            Err(FsError::NotDir)
        }
    }

    fn link(&self, name: &str, other: &Arc<dyn INode>) -> Result<()> {
        let other = other
            .downcast_ref::<LockedINode>()
            .ok_or(FsError::NotSameFs)?;
        // `lock_multiple` write-locks every entry it is given, so naming one
        // inode twice is not an error it can report -- it waits for a lock the
        // same thread already holds, forever. `ln <dir> <dir>/name` does name
        // it twice, and `sys_linkat` hands the pair straight here, so that one
        // command spun a CPU inside the syscall with this inode's write lock
        // held, which made every later access to that directory spin too.
        //
        // The check that refuses it is the `IsDir` three lines below: the only
        // way `other` can BE the directory it is being linked into is that it
        // is a directory, and a hard link to one is not a thing. It just sat
        // one line the wrong side of the lock.
        if core::ptr::eq(self, other) {
            return Err(FsError::IsDir);
        }
        // EXDEV, which `downcast_ref` cannot give: every inode of every ramfs
        // is a `LockedINode`, so the cast succeeds between two DIFFERENT ramfs
        // and `NotSameFs` never fired. There are several mounted at once --
        // `/tmp`, `/run`, `/run/media/apk-cache` -- and `sys_linkat` hands any
        // pair of them straight here, so `ln /tmp/a /run/b` linked one
        // filesystem's inode into another's directory. Its `fs` back-pointer
        // still named the filesystem it was born in, which is a `Weak`: once
        // that ramfs was unmounted and dropped, `fs()` on the surviving name
        // unwrapped a dead `Weak` and took the kernel down. Nor does `dev`
        // distinguish them -- every ramfs inode reports `dev: 0`.
        //
        // What does distinguish them is exactly that back-pointer.
        check_same_fs(self, other)?;
        // to make sure locking order.
        let mut locks = lock_multiple(&[&self.0, &other.0]).into_iter();

        let mut file = locks.next().unwrap();
        let mut other_l = locks.next().unwrap();

        if file.extra.type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }
        if other_l.extra.type_ == FileType::Dir {
            return Err(FsError::IsDir);
        }
        if file.children.contains_key(name) {
            return Err(FsError::EntryExist);
        }

        file.children
            .insert(String::from(name), other_l.this.upgrade().unwrap());
        other_l.extra.nlinks += 1;
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<()> {
        let mut file = self.0.write();
        if file.extra.type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }
        if name == "." || name == ".." {
            return Err(FsError::DirNotEmpty);
        }
        let other = file.children.get(name).ok_or(FsError::EntryNotFound)?;
        if !other.0.read().children.is_empty() {
            return Err(FsError::DirNotEmpty);
        }
        other.0.write().extra.nlinks -= 1;
        file.children.remove(name);
        Ok(())
    }

    fn move_(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> Result<()> {
        let elem = self.find(old_name)?;
        // EXDEV, for the same reason as in [`INode::link`] -- and here it is
        // worse, because the source name is removed once the destination is
        // linked, so `mv /tmp/x /run/x` did not copy the file across, it handed
        // `/run` an inode that `/tmp` still owns. Checked after the source is
        // looked up, so a missing source is still `ENOENT`, as `renameat(2)`
        // promises.
        check_same_fs(
            self,
            target
                .downcast_ref::<LockedINode>()
                .ok_or(FsError::NotSameFs)?,
        )?;
        // POSIX rename(2) atomically REPLACES an existing destination. Our
        // `link` below refuses an existing name with `EntryExist`, so a rename
        // over an existing file would fail with EEXIST — which breaks any tool
        // that writes-to-temp-then-renames (apk: "updating <repo>: File exists"
        // on every index refresh after the first, leaving the cache un-updated).
        // Remove the destination first, unless it is the very same inode as the
        // source (rename to itself is a no-op) or a non-empty directory
        // (ENOTEMPTY — never clobber a populated dir).
        if let Ok(existing) = target.find(new_name) {
            let same = matches!(
                (existing.metadata(), elem.metadata()),
                (Ok(a), Ok(b)) if a.dev == b.dev && a.inode == b.inode
            );
            if same {
                return Ok(());
            }
            // `unlink` below refuses a populated directory with the same
            // error, so this guard changes no answer a caller can see -- it
            // says the rule where a reader looks for it instead of leaving it
            // to a side effect two calls away. A mutation that breaks it
            // therefore survives on purpose.
            if existing.metadata()?.type_ == FileType::Dir && existing.get_entry(2).is_ok() {
                return Err(FsError::DirNotEmpty);
            }
            target.unlink(new_name)?;
        }
        // A directory cannot go through `link`, which refuses one outright --
        // a hard link to a directory is not a thing -- so this whole path
        // answered `EISDIR` for every `mv` of a directory in a ramfs, which is
        // `/tmp`, `/run` and `/dev/shm`. Linux does not route rename through
        // `->link` either: `simple_rename` moves the entry and fixes the
        // directory's `..` itself, which is what this does.
        if elem.metadata()?.type_ == FileType::Dir {
            return self.move_dir(old_name, target, new_name);
        }
        target.link(new_name, &elem)?;
        if let Err(err) = self.unlink(old_name) {
            // recover
            target.unlink(new_name)?;
            return Err(err);
        }
        Ok(())
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        let file = self.0.read();
        if file.extra.type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }
        //info!("find it: {} {}", name, file.parent.is_none());
        match name {
            "." => Ok(file.this.upgrade().ok_or(FsError::EntryNotFound)?),
            ".." => Ok(file.parent.upgrade().ok_or(FsError::EntryNotFound)?),
            name => {
                let s = file.children.get(name).ok_or(FsError::EntryNotFound)?;
                Ok(Arc::clone(s) as Arc<dyn INode>)
            }
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        let file = self.0.read();
        if file.extra.type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }

        match id {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            i => {
                if let Some(s) = file.children.keys().nth(i - 2) {
                    Ok(s.to_string())
                } else {
                    Err(FsError::EntryNotFound)
                }
            }
        }
    }

    fn io_control(&self, _cmd: u32, _data: usize) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn mmap(&self, _area: MMapArea) -> Result<()> {
        Err(FsError::NotSupported)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Weak::upgrade(&self.0.read().fs).unwrap()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// Refuse an operation that spans two filesystems, the way `rename(2)` and
/// `link(2)` promise to (`EXDEV`).
///
/// The filesystem an inode belongs to is the one thing that tells two ramfs
/// apart: the inode type does not (they share one), and neither does `dev`
/// (every ramfs inode reports 0). Both `fs` fields are `Weak`, and comparing
/// them as pointers is right even when one is dangling, which is the case this
/// exists to keep out of the kernel.
fn check_same_fs(a: &LockedINode, b: &LockedINode) -> Result<()> {
    let (a, b) = (a.0.read().fs.clone(), b.0.read().fs.clone());
    if Weak::ptr_eq(&a, &b) {
        Ok(())
    } else {
        Err(FsError::NotSameFs)
    }
}

/// Lock INodes order by their inode id
/// Write-lock every inode in `locks` and hand the guards back in the caller's
/// order, taking them in a canonical order so two threads locking the same two
/// inodes cannot each wait for the other's.
///
/// Two things it needs from its callers, both of which used to be unwritten.
/// The entries have to be DISTINCT: it takes one write lock per entry, so the
/// same inode twice is a thread waiting for itself. And `extra.inode` has to
/// be unique per inode, because that is the key the canonical order is built
/// from -- `sort_by_key` leaves entries with equal keys in the order they came
/// in, so two inodes sharing an id would be locked in one order by
/// `link(a, b)` and the other by `link(b, a)`. The id comes from
/// [`new_inode_id`] for every inode, root included, which is what makes that
/// true.
fn lock_multiple<'a>(locks: &[&'a RwLock<RamFSINode>]) -> Vec<RwLockWriteGuard<'a, RamFSINode>> {
    let mut order: Vec<usize> = (0..locks.len()).collect();
    let mut guards = BTreeMap::new();
    order.sort_by_key(|&i| locks[i].read().extra.inode);
    for i in order {
        guards.insert(i, locks[i].write());
    }
    let mut ret = Vec::new();
    for i in 0..locks.len() {
        ret.push(guards.remove(&i).unwrap());
    }
    ret
}

/// Generate a new inode id
fn new_inode_id() -> usize {
    use core::sync::atomic::*;
    static ID: AtomicUsize = AtomicUsize::new(1);
    ID.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    // ---------------------------------------------------------------- helpers

    /// How many of a file's blocks actually hold storage, as opposed to the
    /// holes that read back as zeros and cost nothing. Sparseness is the whole
    /// point of [`PagedBytes`], and it is invisible through the `INode` API.
    fn allocated(p: &PagedBytes) -> usize {
        p.blocks.iter().filter(|b| b.is_some()).count()
    }

    fn dir_at(at: &Arc<dyn INode>, name: &str) -> Arc<dyn INode> {
        at.create(name, FileType::Dir, 0o755).unwrap()
    }

    fn file_at(at: &Arc<dyn INode>, name: &str) -> Arc<dyn INode> {
        at.create(name, FileType::File, 0o644).unwrap()
    }

    fn ino(n: &Arc<dyn INode>) -> usize {
        n.metadata().unwrap().inode
    }

    fn kind(n: &Arc<dyn INode>) -> FileType {
        n.metadata().unwrap().type_
    }

    /// The names a directory lists, without `.` and `..`.
    fn entries(d: &Arc<dyn INode>) -> Vec<String> {
        let mut v = Vec::new();
        for i in 2.. {
            match d.get_entry(i) {
                Ok(name) => v.push(name),
                Err(_) => break,
            }
        }
        v
    }

    /// Run `f` on its own thread and fail if it does not finish.
    ///
    /// Every path in this module that takes more than one inode lock can, if the
    /// rule that keeps the set distinct is wrong, wait for a lock the same
    /// thread already holds -- forever, on a spinlock, with a directory's write
    /// lock held. A test that reaches such a path runs here instead, so the
    /// failure is a sentence rather than a hung CI job with no clue in it.
    ///
    /// A panic inside `f` is re-raised: its message is printed by the thread
    /// that raised it, and the `join` below fails this test.
    fn within<F>(what: &str, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        match rx.recv_timeout(Duration::from_secs(20)) {
            // Disconnected means `f` panicked and dropped the sender; `join`
            // turns that into this test's failure.
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                handle.join().expect("the body of the test failed");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("{} never returned: it is spinning on a lock", what)
            }
        }
    }

    // ------------------------------------------------- PagedBytes: the holes

    #[test]
    fn growing_a_file_exposes_a_hole_and_allocates_nothing() {
        let mut p = PagedBytes::default();
        p.resize(100 * BLOCK);
        assert_eq!(p.len(), 100 * BLOCK);
        assert_eq!(
            allocated(&p),
            0,
            "an ftruncate is a promise about length, not about memory: foot sizes \
             its wl_shm pool to 512 MiB and writes a few of them"
        );
        assert_eq!(p.blocks.len(), 100, "the slots exist, the storage does not");
    }

    #[test]
    fn a_hole_reads_back_as_zeros() {
        let mut p = PagedBytes::default();
        p.resize(4 * BLOCK);
        let mut buf = [0xffu8; 64];
        assert_eq!(p.read_at(2 * BLOCK + 7, &mut buf), 64);
        assert!(buf.iter().all(|&b| b == 0));
        assert_eq!(allocated(&p), 0, "reading a hole must not materialize it");
    }

    #[test]
    fn a_write_of_nothing_but_zeros_into_a_hole_leaves_it_a_hole() {
        let mut p = PagedBytes::default();
        p.resize(2 * BLOCK);
        p.write_at(0, &[0u8; BLOCK]);
        assert_eq!(
            allocated(&p),
            0,
            "zero writeback is what densified the heap; a zero into a hole is \
             already there"
        );
        let mut buf = [0xffu8; 8];
        p.read_at(0, &mut buf);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn a_write_of_one_nonzero_byte_materializes_only_its_own_block() {
        let mut p = PagedBytes::default();
        p.resize(10 * BLOCK);
        p.write_at(5 * BLOCK + 3, b"x");
        assert_eq!(allocated(&p), 1);
        let mut buf = [0u8; 1];
        p.read_at(5 * BLOCK + 3, &mut buf);
        assert_eq!(&buf, b"x");
    }

    #[test]
    fn a_write_past_the_end_leaves_the_gap_between_a_hole() {
        let mut p = PagedBytes::default();
        p.write_at(0, b"a");
        p.write_at(3 * BLOCK, b"b");
        assert_eq!(p.len(), 3 * BLOCK + 1);
        assert_eq!(allocated(&p), 2, "blocks 1 and 2 were never written");
        let mut buf = [0xffu8; 16];
        p.read_at(BLOCK, &mut buf);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn a_write_that_spans_three_blocks_reads_back_whole() {
        let mut p = PagedBytes::default();
        let data: Vec<u8> = (0..2 * BLOCK + 4).map(|i| (i % 251) as u8).collect();
        p.write_at(BLOCK - 2, &data);
        assert_eq!(p.len(), 3 * BLOCK + 2);
        let mut back = alloc::vec![0u8; data.len()];
        assert_eq!(p.read_at(BLOCK - 2, &mut back), data.len());
        assert_eq!(back, data);
        let mut before = [0xffu8; 1];
        p.read_at(BLOCK - 3, &mut before);
        assert_eq!(before[0], 0, "the byte before the write was never written");
    }

    #[test]
    fn a_block_written_end_to_end_reads_back_end_to_end() {
        let mut p = PagedBytes::default();
        p.write_at(0, &[7u8; BLOCK]);
        assert_eq!(p.len(), BLOCK);
        let mut back = [0u8; BLOCK];
        p.read_at(0, &mut back);
        assert!(back.iter().all(|&b| b == 7));
    }

    #[test]
    fn a_read_is_clipped_to_the_length_and_not_to_the_blocks() {
        let mut p = PagedBytes::default();
        p.write_at(0, b"hello");
        let mut buf = [0xffu8; 10];
        assert_eq!(p.read_at(0, &mut buf), 5);
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(&buf[5..], &[0xffu8; 5], "nothing past the end is touched");
    }

    #[test]
    fn a_read_starting_past_the_end_reads_nothing() {
        let mut p = PagedBytes::default();
        p.write_at(0, b"hello");
        let mut buf = [0xffu8; 4];
        assert_eq!(p.read_at(5, &mut buf), 0);
        assert_eq!(p.read_at(9999, &mut buf), 0);
        assert_eq!(&buf, &[0xffu8; 4]);
    }

    #[test]
    fn shrinking_then_growing_does_not_resurrect_the_old_tail() {
        let mut p = PagedBytes::default();
        p.write_at(0, &[0xaau8; 10]);
        p.resize(4);
        p.resize(10);
        let mut back = [0xffu8; 10];
        p.read_at(0, &mut back);
        assert_eq!(&back[..4], &[0xaau8; 4]);
        assert_eq!(
            &back[4..],
            &[0u8; 6],
            "a truncate followed by a grow must expose zeros, not the bytes the \
             file used to hold"
        );
    }

    #[test]
    fn shrinking_frees_the_blocks_past_the_new_end() {
        let mut p = PagedBytes::default();
        p.write_at(0, b"first");
        p.write_at(3 * BLOCK, b"last");
        assert_eq!(allocated(&p), 2);
        p.resize(BLOCK);
        assert_eq!(p.blocks.len(), 1);
        assert_eq!(allocated(&p), 1, "the block at 3*BLOCK is unreachable now");
    }

    #[test]
    fn shrinking_to_nothing_frees_everything() {
        let mut p = PagedBytes::default();
        p.write_at(0, &[1u8; 3 * BLOCK]);
        assert_eq!(allocated(&p), 3);
        p.resize(0);
        assert_eq!(p.len(), 0);
        assert_eq!(p.blocks.len(), 0);
        assert_eq!(allocated(&p), 0);
    }

    // ------------------------------------------- the root's own inode number

    #[test]
    fn the_root_inode_number_is_a_counter_value_and_not_its_own_address() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let child = file_at(&root, "f");
        // The counter only goes up and the root drew from it first, so this
        // ordering holds for any RamFS in the process. It fails the moment the
        // id is an address instead.
        assert!(
            ino(&root) < ino(&child),
            "root ino {} should be below its child's {}: the root is older",
            ino(&root),
            ino(&child)
        );
        assert_ne!(
            ino(&root),
            Arc::as_ptr(&fs.root) as usize,
            "`st_ino` of /tmp used to BE the root node's kernel heap address, \
             which any unprivileged process could read with stat(2)"
        );
    }

    #[test]
    fn the_root_and_its_children_are_freed_with_the_filesystem() {
        let fs = RamFS::new();
        let root_weak = Arc::downgrade(&fs.root);
        let child = file_at(&fs.root_inode(), "f");
        let child_weak = Arc::downgrade(&child);
        drop(child);
        drop(fs);
        assert!(
            root_weak.upgrade().is_none(),
            "the root leaked: `Arc::into_raw` took a reference and never gave it \
             back, so every ramfs ever mounted stayed in the kernel heap"
        );
        assert!(
            child_weak.upgrade().is_none(),
            "the root's `children` map holds the whole tree, so leaking the root \
             leaks the filesystem"
        );
    }

    #[test]
    fn every_inode_of_a_filesystem_has_an_id_of_its_own() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let a = dir_at(&root, "a");
        let b = dir_at(&root, "b");
        let c = file_at(&a, "c");
        let mut ids = alloc::vec![ino(&root), ino(&a), ino(&b), ino(&c)];
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            before,
            "`lock_multiple` orders locks by this id, so two inodes sharing one \
             would be locked in opposite orders by `link(a, b)` and `link(b, a)`"
        );
    }

    // ---------------------------------------------------------------- create

    #[test]
    fn a_created_file_is_found_under_its_name() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let f = file_at(&root, "f");
        assert_eq!(ino(&root.find("f").unwrap()), ino(&f));
        assert_eq!(kind(&f), FileType::File);
        assert_eq!(f.metadata().unwrap().mode, 0o644);
        assert_eq!(f.metadata().unwrap().nlinks, 1);
    }

    #[test]
    fn a_created_directory_is_a_directory_and_starts_empty() {
        let fs = RamFS::new();
        let d = dir_at(&fs.root_inode(), "d");
        assert_eq!(kind(&d), FileType::Dir);
        assert_eq!(entries(&d), Vec::<String>::new());
    }

    #[test]
    fn creating_a_name_that_is_taken_is_refused() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        file_at(&root, "f");
        assert_eq!(
            root.create("f", FileType::File, 0o644).err(),
            Some(FsError::EntryExist)
        );
    }

    #[test]
    fn dot_and_dotdot_can_never_be_created() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        assert_eq!(
            root.create(".", FileType::Dir, 0o755).err(),
            Some(FsError::EntryExist)
        );
        assert_eq!(
            root.create("..", FileType::Dir, 0o755).err(),
            Some(FsError::EntryExist)
        );
    }

    #[test]
    fn creating_inside_a_file_is_refused() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "f");
        assert_eq!(
            f.create("x", FileType::File, 0o644).err(),
            Some(FsError::NotDir)
        );
    }

    #[test]
    fn create2_puts_its_data_in_the_raw_device_id() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let node = root
            .create2("tty", FileType::CharDevice, 0o666, make_rdev(5, 0))
            .unwrap();
        assert_eq!(kind(&node), FileType::CharDevice);
        assert_eq!(node.metadata().unwrap().rdev, make_rdev(5, 0));
    }

    #[test]
    fn a_new_child_belongs_to_the_filesystem_that_created_it() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let deep = file_at(&dir_at(&root, "a"), "b");
        assert!(Arc::ptr_eq(
            &(deep.fs().root_inode()),
            &(fs.root_inode() as Arc<dyn INode>)
        ));
    }

    // ------------------------------------------------------------ find, dots

    #[test]
    fn dot_is_the_directory_itself_and_dotdot_is_its_parent() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let d = dir_at(&root, "d");
        assert_eq!(ino(&d.find(".").unwrap()), ino(&d));
        assert_eq!(ino(&d.find("..").unwrap()), ino(&root));
    }

    #[test]
    fn the_root_is_its_own_parent() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        assert_eq!(ino(&root.find("..").unwrap()), ino(&root));
        assert_eq!(ino(&root.find(".").unwrap()), ino(&root));
    }

    #[test]
    fn a_name_that_is_not_there_is_not_found() {
        let fs = RamFS::new();
        assert_eq!(
            fs.root_inode().find("nope").err(),
            Some(FsError::EntryNotFound)
        );
    }

    #[test]
    fn find_inside_a_file_is_refused() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "f");
        assert_eq!(f.find("x").err(), Some(FsError::NotDir));
        assert_eq!(f.find(".").err(), Some(FsError::NotDir));
    }

    // --------------------------------------------------------- get_entry

    #[test]
    fn a_directory_lists_dot_dotdot_and_then_its_children_in_order() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        file_at(&root, "beta");
        file_at(&root, "alpha");
        assert_eq!(root.get_entry(0).unwrap(), ".");
        assert_eq!(root.get_entry(1).unwrap(), "..");
        assert_eq!(root.get_entry(2).unwrap(), "alpha");
        assert_eq!(root.get_entry(3).unwrap(), "beta");
        assert_eq!(root.get_entry(4).err(), Some(FsError::EntryNotFound));
    }

    #[test]
    fn listing_a_file_is_refused() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "f");
        assert_eq!(f.get_entry(0).err(), Some(FsError::NotDir));
    }

    // -------------------------------------------------------- read and write

    #[test]
    fn what_was_written_is_what_is_read_back() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "f");
        assert_eq!(f.write_at(0, b"hola").unwrap(), 4);
        let mut buf = [0u8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"hola");
        assert_eq!(f.metadata().unwrap().size, 4);
    }

    #[test]
    fn reading_or_writing_a_directory_is_refused() {
        let fs = RamFS::new();
        let d = dir_at(&fs.root_inode(), "d");
        assert_eq!(d.read_at(0, &mut [0u8; 4]).err(), Some(FsError::IsDir));
        assert_eq!(d.write_at(0, b"x").err(), Some(FsError::IsDir));
        assert_eq!(d.poll().err(), Some(FsError::IsDir));
    }

    #[test]
    fn a_file_is_always_ready_and_never_hung_up() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "f");
        let p = f.poll().unwrap();
        assert!(p.read && p.write);
        assert!(!p.error && !p.hangup);
    }

    #[test]
    fn resizing_a_directory_is_refused() {
        let fs = RamFS::new();
        let d = dir_at(&fs.root_inode(), "d");
        assert_eq!(d.resize(0).err(), Some(FsError::NotFile));
    }

    #[test]
    fn resize_moves_the_size_the_metadata_reports() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "f");
        f.resize(5000).unwrap();
        assert_eq!(f.metadata().unwrap().size, 5000);
        f.resize(3).unwrap();
        assert_eq!(f.metadata().unwrap().size, 3);
    }

    #[test]
    fn set_metadata_changes_the_owner_and_the_mode_and_nothing_else() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "f");
        let mut want = f.metadata().unwrap();
        want.mode = 0o600;
        want.uid = 1000;
        want.gid = 1000;
        want.mtime = Timespec { sec: 42, nsec: 7 };
        want.type_ = FileType::Dir;
        want.inode = 0xdead;
        f.set_metadata(&want).unwrap();
        let now = f.metadata().unwrap();
        assert_eq!((now.mode, now.uid, now.gid), (0o600, 1000, 1000));
        assert_eq!(now.mtime, Timespec { sec: 42, nsec: 7 });
        assert_eq!(now.type_, FileType::File, "a chmod cannot change the type");
        assert_ne!(now.inode, 0xdead, "nor the inode number");
    }

    // ------------------------------------------------------------------ link

    #[test]
    fn linking_a_directory_into_itself_is_refused_instead_of_spinning_forever() {
        within("`ln <dir> <dir>/self`", || {
            let fs = RamFS::new();
            let d = dir_at(&fs.root_inode(), "d");
            assert_eq!(
                d.link("self", &d),
                Err(FsError::IsDir),
                "`lock_multiple` write-locks each entry it is given, so naming \
                 one inode twice is a thread waiting for itself"
            );
        });
    }

    #[test]
    fn linking_the_root_into_itself_is_refused_instead_of_spinning_forever() {
        within("`ln / //self`", || {
            let fs = RamFS::new();
            let root = fs.root_inode();
            assert_eq!(root.link("self", &root), Err(FsError::IsDir));
        });
    }

    #[test]
    fn a_hard_link_to_a_directory_is_refused() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let d = dir_at(&root, "d");
        assert_eq!(root.link("also_d", &d).err(), Some(FsError::IsDir));
    }

    #[test]
    fn a_hard_link_gives_one_inode_two_names_and_two_links() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let f = file_at(&root, "f");
        f.write_at(0, b"shared").unwrap();
        root.link("g", &f).unwrap();
        assert_eq!(f.metadata().unwrap().nlinks, 2);
        let g = root.find("g").unwrap();
        assert_eq!(ino(&g), ino(&f));
        let mut buf = [0u8; 6];
        g.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"shared");
    }

    #[test]
    fn linking_into_a_file_is_refused() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let a = file_at(&root, "a");
        let b = file_at(&root, "b");
        assert_eq!(a.link("x", &b).err(), Some(FsError::NotDir));
    }

    #[test]
    fn linking_over_a_name_that_is_taken_is_refused() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let f = file_at(&root, "f");
        file_at(&root, "g");
        assert_eq!(root.link("g", &f).err(), Some(FsError::EntryExist));
        assert_eq!(
            f.metadata().unwrap().nlinks,
            1,
            "a refused link counts for nothing"
        );
    }

    #[test]
    fn linking_across_filesystems_is_refused() {
        let one = RamFS::new();
        let two = RamFS::new();
        let f = file_at(&two.root_inode(), "f");
        let root = one.root_inode();
        // `downcast_ref` cannot refuse this: both sides are `LockedINode`. Two
        // ramfs are mounted at once in any running system, and `ln /tmp/a
        // /run/b` used to land an inode of one in the other's directory.
        assert_eq!(root.link("f", &f).err(), Some(FsError::NotSameFs));
        assert_eq!(root.find("f").err(), Some(FsError::EntryNotFound));
        assert_eq!(f.metadata().unwrap().nlinks, 1);
    }

    #[test]
    fn an_inode_linked_out_of_a_dropped_filesystem_would_have_no_filesystem() {
        // Why the refusal above matters: `fs` is a `Weak`, and `fs()` unwraps
        // it. The link kept the inode alive; it did not keep the ramfs it named
        // alive, so the surviving name pointed at a filesystem that was gone.
        let f = {
            let two = RamFS::new();
            let f = file_at(&two.root_inode(), "f");
            drop(two);
            f
        };
        let locked = f.downcast_ref::<LockedINode>().unwrap();
        assert!(
            locked.0.read().fs.upgrade().is_none(),
            "this is the dead `Weak` that `fs()` used to unwrap"
        );
    }

    // ---------------------------------------------------------------- unlink

    #[test]
    fn unlinking_drops_the_name_and_the_link_count() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let f = file_at(&root, "f");
        root.link("g", &f).unwrap();
        root.unlink("f").unwrap();
        assert_eq!(root.find("f").err(), Some(FsError::EntryNotFound));
        assert_eq!(f.metadata().unwrap().nlinks, 1);
        assert_eq!(ino(&root.find("g").unwrap()), ino(&f));
    }

    #[test]
    fn unlinking_a_name_that_is_not_there_is_refused() {
        let fs = RamFS::new();
        assert_eq!(
            fs.root_inode().unlink("nope").err(),
            Some(FsError::EntryNotFound)
        );
    }

    #[test]
    fn unlinking_a_directory_that_still_holds_something_is_refused() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let d = dir_at(&root, "d");
        file_at(&d, "inside");
        assert_eq!(root.unlink("d").err(), Some(FsError::DirNotEmpty));
        d.unlink("inside").unwrap();
        root.unlink("d").unwrap();
    }

    #[test]
    fn unlinking_inside_a_file_is_refused() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "f");
        assert_eq!(f.unlink("x").err(), Some(FsError::NotDir));
    }

    #[test]
    fn dot_and_dotdot_cannot_be_unlinked() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        assert_eq!(root.unlink(".").err(), Some(FsError::DirNotEmpty));
        assert_eq!(root.unlink("..").err(), Some(FsError::DirNotEmpty));
    }

    // ---------------------------------------------------- lock_multiple

    #[test]
    fn lock_multiple_hands_its_guards_back_in_the_order_it_was_asked() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let a = dir_at(&root, "a");
        let b = dir_at(&root, "b");
        let (la, lb) = (
            a.downcast_ref::<LockedINode>().unwrap(),
            b.downcast_ref::<LockedINode>().unwrap(),
        );
        let (ia, ib) = (ino(&a), ino(&b));
        // `b` was made second, so it sorts after `a` and the canonical order is
        // not the caller's here.
        let guards = lock_multiple(&[&lb.0, &la.0]);
        assert_eq!(guards[0].extra.inode, ib);
        assert_eq!(guards[1].extra.inode, ia);
    }

    // ------------------------------------------------------- rename: a file

    #[test]
    fn renaming_a_file_within_one_directory_moves_the_name() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let f = file_at(&root, "old");
        f.write_at(0, b"body").unwrap();
        root.move_("old", &root, "new").unwrap();
        assert_eq!(root.find("old").err(), Some(FsError::EntryNotFound));
        assert_eq!(ino(&root.find("new").unwrap()), ino(&f));
    }

    #[test]
    fn renaming_a_file_into_another_directory_moves_it() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let to = dir_at(&root, "to");
        let f = file_at(&root, "f");
        root.move_("f", &to, "f").unwrap();
        assert_eq!(root.find("f").err(), Some(FsError::EntryNotFound));
        assert_eq!(ino(&to.find("f").unwrap()), ino(&f));
    }

    #[test]
    fn renaming_over_an_existing_file_replaces_it() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let src = file_at(&root, "src");
        let dst = file_at(&root, "dst");
        let (i_src, i_dst) = (ino(&src), ino(&dst));
        // apk writes its index to a temp name and renames it over the old one on
        // every refresh; EEXIST here left the cache un-updated forever.
        root.move_("src", &root, "dst").unwrap();
        assert_eq!(ino(&root.find("dst").unwrap()), i_src);
        assert_ne!(i_src, i_dst);
    }

    #[test]
    fn renaming_a_name_onto_itself_is_a_no_op() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let f = file_at(&root, "f");
        root.move_("f", &root, "f").unwrap();
        assert_eq!(ino(&root.find("f").unwrap()), ino(&f));
    }

    #[test]
    fn renaming_something_that_is_not_there_is_refused() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        assert_eq!(
            root.move_("nope", &root, "x").err(),
            Some(FsError::EntryNotFound)
        );
    }

    // -------------------------------------------------- rename: a directory

    #[test]
    fn renaming_a_directory_moves_it_instead_of_answering_that_it_is_a_directory() {
        within("`mv /old /new` on a directory", || {
            let fs = RamFS::new();
            let root = fs.root_inode();
            let d = dir_at(&root, "old");
            let inner = file_at(&d, "inside");
            // The old path went through `link`, which refuses a directory
            // outright, so every `mv` of a directory under /tmp, /run or
            // /dev/shm gave EISDIR.
            root.move_("old", &root, "new").unwrap();
            assert_eq!(root.find("old").err(), Some(FsError::EntryNotFound));
            let moved = root.find("new").unwrap();
            assert_eq!(ino(&moved), ino(&d));
            assert_eq!(ino(&moved.find("inside").unwrap()), ino(&inner));
        });
    }

    #[test]
    fn a_directory_moved_to_another_parent_takes_its_subtree_with_it() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let from = dir_at(&root, "from");
        let to = dir_at(&root, "to");
        let d = dir_at(&from, "d");
        let deep = file_at(&dir_at(&d, "sub"), "leaf");
        from.move_("d", &to, "d2").unwrap();
        assert_eq!(from.find("d").err(), Some(FsError::EntryNotFound));
        let moved = to.find("d2").unwrap();
        assert_eq!(ino(&moved), ino(&d));
        assert_eq!(
            ino(&moved.find("sub").unwrap().find("leaf").unwrap()),
            ino(&deep)
        );
    }

    #[test]
    fn a_moved_directory_points_its_dotdot_at_its_new_parent() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let from = dir_at(&root, "from");
        let to = dir_at(&root, "to");
        let d = dir_at(&from, "d");
        from.move_("d", &to, "d").unwrap();
        assert_eq!(
            ino(&d.find("..").unwrap()),
            ino(&to),
            "`cd` into a moved directory and `cd ..` must arrive where the \
             directory now lives"
        );
    }

    #[test]
    fn a_directory_renamed_in_place_keeps_its_parent() {
        within("`mv /p/old /p/new` on a directory", || {
            let fs = RamFS::new();
            let root = fs.root_inode();
            let parent = dir_at(&root, "p");
            let d = dir_at(&parent, "old");
            parent.move_("old", &parent, "new").unwrap();
            assert_eq!(entries(&parent), alloc::vec![String::from("new")]);
            assert_eq!(ino(&d.find("..").unwrap()), ino(&parent));
        });
    }

    #[test]
    fn a_directory_cannot_be_moved_into_itself() {
        within("`mv /d /d/inside`", || {
            let fs = RamFS::new();
            let root = fs.root_inode();
            let d = dir_at(&root, "d");
            // Refused before the locks are taken, which matters twice over: the
            // destination and the directory being moved are the SAME inode here,
            // so `lock_multiple` would wait for a lock this thread already
            // holds.
            assert_eq!(
                root.move_("d", &d, "inside").err(),
                Some(FsError::InvalidParam),
                "the subtree would have no name left: nothing could reach it \
                 and nothing could free it"
            );
            assert_eq!(ino(&root.find("d").unwrap()), ino(&d));
        });
    }

    #[test]
    fn a_directory_cannot_be_moved_into_its_own_child() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let d = dir_at(&root, "d");
        let child = dir_at(&d, "child");
        assert_eq!(
            root.move_("d", &child, "loop").err(),
            Some(FsError::InvalidParam)
        );
        assert_eq!(ino(&root.find("d").unwrap()), ino(&d));
        assert_eq!(entries(&child), Vec::<String>::new());
    }

    #[test]
    fn a_directory_cannot_be_moved_into_its_own_grandchild() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let d = dir_at(&root, "d");
        let deep = dir_at(&dir_at(&d, "a"), "b");
        assert_eq!(
            root.move_("d", &deep, "loop").err(),
            Some(FsError::InvalidParam)
        );
    }

    #[test]
    fn moving_a_directory_up_to_the_root_terminates() {
        // The walk that refuses a cycle climbs `parent` from the destination,
        // and the root is its own parent: without a stop there it walks in
        // place forever with the rename lock held.
        within("`mv /a/b /`", || {
            let fs = RamFS::new();
            let root = fs.root_inode();
            let a = dir_at(&root, "a");
            let b = dir_at(&a, "b");
            a.move_("b", &root, "b").unwrap();
            assert_eq!(ino(&root.find("b").unwrap()), ino(&b));
            assert_eq!(a.find("b").err(), Some(FsError::EntryNotFound));
        });
    }

    #[test]
    fn moving_a_directory_onto_a_name_held_by_a_full_directory_is_refused() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let src = dir_at(&root, "src");
        let dst = dir_at(&root, "dst");
        file_at(&dst, "occupant");
        assert_eq!(
            root.move_("src", &root, "dst").err(),
            Some(FsError::DirNotEmpty)
        );
        assert_eq!(ino(&root.find("src").unwrap()), ino(&src));
        assert_eq!(ino(&root.find("dst").unwrap()), ino(&dst));
    }

    #[test]
    fn moving_a_directory_over_an_empty_directory_replaces_it() {
        within("`mv /src /dst` over an empty directory", || {
            let fs = RamFS::new();
            let root = fs.root_inode();
            let src = dir_at(&root, "src");
            file_at(&src, "keep");
            dir_at(&root, "dst");
            root.move_("src", &root, "dst").unwrap();
            let now = root.find("dst").unwrap();
            assert_eq!(ino(&now), ino(&src));
            assert!(now.find("keep").is_ok());
            assert_eq!(entries(&root), alloc::vec![String::from("dst")]);
        });
    }

    #[test]
    fn moving_a_directory_into_something_that_is_not_a_directory_is_refused() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let d = dir_at(&root, "d");
        let f = file_at(&root, "f");
        assert_eq!(root.move_("d", &f, "x").err(), Some(FsError::NotDir));
        assert_eq!(ino(&root.find("d").unwrap()), ino(&d));
    }

    #[test]
    fn moving_a_directory_out_of_a_tree_leaves_its_siblings_alone() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let from = dir_at(&root, "from");
        let d = dir_at(&from, "d");
        file_at(&from, "sibling");
        dir_at(&from, "zsibling");
        let to = dir_at(&root, "to");
        from.move_("d", &to, "d").unwrap();
        assert_eq!(
            entries(&from),
            alloc::vec![String::from("sibling"), String::from("zsibling")]
        );
        assert_eq!(entries(&to), alloc::vec![String::from("d")]);
        assert_eq!(ino(&to.find("d").unwrap()), ino(&d));
    }

    #[test]
    fn renaming_a_directory_to_its_own_name_is_a_no_op() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        let d = dir_at(&root, "d");
        file_at(&d, "inside");
        root.move_("d", &root, "d").unwrap();
        assert_eq!(ino(&root.find("d").unwrap()), ino(&d));
        assert!(d.find("inside").is_ok());
    }

    #[test]
    fn renaming_into_another_filesystem_is_refused_and_leaves_the_source_alone() {
        let one = RamFS::new();
        let two = RamFS::new();
        let root = one.root_inode();
        let d = dir_at(&root, "d");
        let f = file_at(&root, "f");
        let other_root = two.root_inode();
        assert_eq!(
            root.move_("d", &other_root, "d").err(),
            Some(FsError::NotSameFs)
        );
        assert_eq!(
            root.move_("f", &other_root, "f").err(),
            Some(FsError::NotSameFs)
        );
        // A rename unlinks the source once the destination is linked, so a
        // refusal that came too late would have moved the inode out of this
        // filesystem for good.
        assert_eq!(ino(&root.find("d").unwrap()), ino(&d));
        assert_eq!(ino(&root.find("f").unwrap()), ino(&f));
        assert_eq!(entries(&other_root), Vec::<String>::new());
    }

    #[test]
    fn a_missing_source_is_not_found_even_across_filesystems() {
        let one = RamFS::new();
        let two = RamFS::new();
        // `renameat(2)` reports ENOENT for a source that is not there, whatever
        // else is wrong with the call, so the EXDEV check goes after the lookup.
        assert_eq!(
            one.root_inode().move_("nope", &two.root_inode(), "x").err(),
            Some(FsError::EntryNotFound)
        );
    }

    // ---------------------------------------------------------- the whole fs

    #[test]
    fn a_fresh_filesystem_is_an_empty_writable_root() {
        let fs = RamFS::new();
        let root = fs.root_inode();
        assert_eq!(kind(&root), FileType::Dir);
        assert_eq!(root.metadata().unwrap().mode, 0o777);
        assert_eq!(entries(&root), Vec::<String>::new());
        assert!(fs.sync().is_ok());
        assert!(root.sync_all().is_ok());
        assert!(root.sync_data().is_ok());
    }

    #[test]
    fn a_ramfs_reports_no_quota_at_all() {
        let fs = RamFS::new();
        let info = fs.info();
        assert_eq!((info.blocks, info.bfree, info.bavail), (0, 0, 0));
    }

    #[test]
    fn a_ramfs_inode_supports_neither_ioctl_nor_mmap() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "f");
        assert_eq!(f.io_control(0, 0).err(), Some(FsError::NotSupported));
        let area = MMapArea {
            start_vaddr: 0,
            end_vaddr: 0x1000,
            prot: 0,
            flags: 0,
            offset: 0,
        };
        assert_eq!(f.mmap(area).err(), Some(FsError::NotSupported));
    }

    #[test]
    fn a_big_sparse_file_costs_nothing_until_it_is_written() {
        let fs = RamFS::new();
        let f = file_at(&fs.root_inode(), "pool");
        // What foot does to its wl_shm pool. The eager version allocated 131072
        // blocks from the kernel heap inside this one call.
        f.resize(512 * 1024 * 1024).unwrap();
        assert_eq!(f.metadata().unwrap().size, 512 * 1024 * 1024);
        let locked = f.downcast_ref::<LockedINode>().unwrap();
        assert_eq!(allocated(&locked.0.read().content), 0);
        f.write_at(256 * 1024 * 1024, b"rendered").unwrap();
        assert_eq!(allocated(&locked.0.read().content), 1);
        let mut buf = [0u8; 8];
        f.read_at(256 * 1024 * 1024, &mut buf).unwrap();
        assert_eq!(&buf, b"rendered");
    }
}
