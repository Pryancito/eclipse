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

/// Paged byte buffer: `len` logical bytes stored across `BLOCK`-sized blocks.
#[derive(Default)]
struct PagedBytes {
    blocks: Vec<alloc::boxed::Box<[u8]>>,
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

    /// Resize to `new_len`, zero-filling any newly exposed bytes.
    fn resize(&mut self, new_len: usize) {
        let nblocks = (new_len + BLOCK - 1) / BLOCK;
        if nblocks < self.blocks.len() {
            self.blocks.truncate(nblocks);
        } else {
            self.blocks.reserve(nblocks - self.blocks.len());
            while self.blocks.len() < nblocks {
                self.blocks.push(Self::alloc_zeroed_block());
            }
        }
        if new_len < self.len {
            // Shrinking: clear the stale tail of the last (now partial) block
            // so a later grow does not resurrect old bytes.
            let off = new_len % BLOCK;
            if off != 0 {
                if let Some(b) = self.blocks.get_mut(new_len / BLOCK) {
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
            buf[done..done + chunk].copy_from_slice(&self.blocks[bi][bo..bo + chunk]);
            done += chunk;
        }
        n
    }

    fn write_at(&mut self, offset: usize, buf: &[u8]) {
        // A hole (offset past current end) must read back as zeros; materialize
        // it before writing the data so intermediate blocks exist and are zeroed.
        if offset > self.len {
            self.resize(offset);
        }
        let end = offset + buf.len();
        let nblocks = (end + BLOCK - 1) / BLOCK;
        self.blocks.reserve(nblocks.saturating_sub(self.blocks.len()));
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done;
            let bi = pos / BLOCK;
            let bo = pos % BLOCK;
            let chunk = (BLOCK - bo).min(buf.len() - done);
            if bi < self.blocks.len() {
                self.blocks[bi][bo..bo + chunk].copy_from_slice(&buf[done..done + chunk]);
            } else if bo == 0 && chunk == BLOCK {
                // New block fully covered by this write: allocate and copy in a
                // single pass, skipping the zero-fill entirely.
                let mut v = Vec::with_capacity(BLOCK);
                v.extend_from_slice(&buf[done..done + chunk]);
                self.blocks.push(v.into_boxed_slice());
            } else {
                // New trailing block only partially written: zero then fill.
                let mut b = Self::alloc_zeroed_block();
                b[bo..bo + chunk].copy_from_slice(&buf[done..done + chunk]);
                self.blocks.push(b);
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
        let fs = Arc::new(RamFS { root });
        let mut root = fs.root.0.write();
        root.parent = Arc::downgrade(&fs.root);
        root.this = Arc::downgrade(&fs.root);
        root.fs = Arc::downgrade(&fs);
        root.extra.inode =
            Arc::into_raw(root.this.upgrade().unwrap()) as *const RamFSINode as usize;
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
        file.content.write_at(offset, buf);
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

/// Lock INodes order by their inode id
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
