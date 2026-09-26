#![cfg_attr(not(any(test, feature = "std")), no_std)]

extern crate alloc;
#[macro_use]
extern crate log;

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
};
use core::{any::Any, future::Future, pin::Pin};
use rcore_fs::vfs::*;
use spin::RwLock;

/// The filesystem on which all the other filesystems are mounted
pub struct MountFS {
    /// The inner file system
    inner: Arc<dyn FileSystem>,
    /// All mounted children file systems
    mountpoints: RwLock<BTreeMap<INodeId, Arc<MountFS>>>,
    /// The mount point of this file system
    self_mountpoint: Option<Arc<MNode>>,
    /// Weak reference to self
    self_ref: Weak<MountFS>,
}

type INodeId = usize;

/// [diag] Magic stamped into every live `MNode` at construction and verified on
/// entry to hot `INode` methods (see `check_poison`). The "intermittent" kernel
/// corruption (`timeout -s TERM 1 sleep 5`) surfaces as a `#PF` in
/// `<MNode as INode>::metadata` dereferencing a garbage inner-inode vtable
/// (`0x87`). Checking this canary (plus the inode fat pointer) before the deref
/// lets the method return `EIO` instead of faulting, and logs whether the slot
/// was reused (UAF — `poison` clobbered to another allocation's data) or the
/// inode pointer alone was overwritten (targeted wild write, `poison` intact).
const MNODE_POISON: u64 = 0x4d4e_4f44_455f_4f4b; // "MNODE_OK"

/// INode for `MountFS`
///
/// [diag] `repr(C)` pins the field order so `check_poison` can inspect the raw
/// words at known offsets without dereferencing: word0 = `poison`, word1/2 =
/// the `inode` fat pointer (data ptr, vtable ptr). The recorded corruption
/// leaves `poison` intact but clobbers the inode's vtable word to a tiny value
/// (`0x87`), so validating that word — not just `poison` — is what catches it.
#[repr(C)]
pub struct MNode {
    /// [diag] Corruption canary — first field so a UAF/realloc or a wild write
    /// over the head of the struct clobbers it before the pointers.
    poison: u64,
    /// The inner INode
    inode: Arc<dyn INode>,
    /// Associated `MountFS`
    vfs: Arc<MountFS>,
    /// Weak reference to self
    self_ref: Weak<MNode>,
}

impl MNode {
    /// [defense-in-depth] Verify the corruption canary before dereferencing the
    /// inner inode. Called on entry to the hottest `INode` methods.
    ///
    /// The "intermittent" kernel corruption (`timeout -s TERM 1 sleep 5`) can
    /// leave an `MNode`'s inner `Arc<dyn INode>` fat pointer clobbered with a
    /// tiny value (`0x87`); the original code then called straight through that
    /// garbage vtable → `#PF` at `0x97` → (with the corruption already on the
    /// stack) a silent triple fault. This guard turns that machine-killing fault
    /// into a *recoverable* `EIO` for the one syscall that touched the bad node:
    /// the process gets an error, the kernel stays up. It also logs the exact
    /// clobber pattern (poison state + both halves of the inode fat pointer) so
    /// the corruption remains diagnosable.
    ///
    /// This does NOT fix the underlying wild write (whose primary victim is the
    /// executor stack, not these nodes — see `docs/README-crash-repro.md`); it
    /// is pure hardening of a known secondary crash site, in the spirit of the
    /// dedicated `#GP` IST stack.
    #[inline]
    fn check_poison(&self, who: &str) -> Result<()> {
        let raw = self as *const Self as *const u64;
        // SAFETY: self is a live &self reference; reading the first four words of
        // its own (repr(C)) storage is in-bounds even when the contents are
        // garbage. w0=poison, w1=inode.data, w2=inode.vtable, w3=vfs.
        let (w0, w1, w2, w3) = unsafe {
            (
                core::ptr::read_volatile(raw),
                core::ptr::read_volatile(raw.add(1)),
                core::ptr::read_volatile(raw.add(2)),
                core::ptr::read_volatile(raw.add(3)),
            )
        };
        // A live kernel pointer sits at 0xffff_ff00_0000_0000+ ; the corruption
        // sprays tiny values (0x01/0x87/…). Flag the inode fat pointer (data +
        // vtable) if either half is not a plausible kernel pointer — that is the
        // exact word (`0x87`) the recorded #PF dereferenced.
        //
        // "Plausible" is measured against `self`, not against a hardcoded
        // higher-half base: we are inside `self`'s own method, so its address is
        // by construction a live node in whatever address space this kernel runs
        // in. On bare metal that is the higher half and the test below is the
        // original one. Under libos the kernel is an ordinary Linux process and
        // every pointer is a low userspace address (`0x55f2…` in the CI log), so
        // the fixed floor called all of them garbage: `check_poison` guards
        // `read_at`, `write_at`, `metadata`, `find` and `get_entry`, so EVERY
        // MountFS operation returned `DeviceError`. The dynamic loader could not
        // resolve `PT_INTERP` and all 302 cases of the `Linux Libc Test Libos`
        // job failed at spawn, each logging "(poison ok) … *INODE-PTR-GARBAGE*"
        // — the canary intact, which is exactly what a false positive looks like.
        const HIGHER_HALF: u64 = 0xffff_8000_0000_0000;
        let self_addr = self as *const Self as u64;
        let bad_kptr = |w: u64| {
            if self_addr >= HIGHER_HALF {
                w < HIGHER_HALF
            } else {
                // libos: no higher half to anchor on. The wild write this guard
                // exists for sprays tiny values, and those stay caught.
                w < 0x1000
            }
        };
        let poison_bad = w0 != MNODE_POISON;
        let inode_bad = bad_kptr(w1) || bad_kptr(w2);
        if poison_bad || inode_bad {
            let poison_state = if poison_bad {
                "*POISON-CLOBBERED*"
            } else {
                "(poison ok)"
            };
            let inode_state = if inode_bad {
                "*INODE-PTR-GARBAGE*"
            } else {
                "(inode ok)"
            };
            error!(
                "[MNODE-CORRUPT] in {}: self={:p} poison={:#x} (want {:#x}) {} \
                 inode.data={:#x} inode.vtable={:#x} vfs={:#x} {} -> returning EIO \
                 instead of dereferencing (poison-intact + inode-bad == the wild \
                 write hit the inode fat pointer specifically, not a whole-slot UAF)",
                who, self, w0, MNODE_POISON, poison_state, w1, w2, w3, inode_state,
            );
            return Err(FsError::DeviceError);
        }
        Ok(())
    }
}

impl MountFS {
    /// The filesystem mounted at this mount point (not nested children).
    pub fn inner_fs(&self) -> Arc<dyn FileSystem> {
        self.inner.clone()
    }

    /// Create a `MountFS` wrapper for file system `fs`
    pub fn new(fs: Arc<dyn FileSystem>) -> Arc<Self> {
        MountFS {
            inner: fs,
            mountpoints: RwLock::new(BTreeMap::new()),
            self_mountpoint: None,
            self_ref: Weak::default(),
        }
        .wrap()
    }

    /// Wrap pure `MountFS` with `Arc<..>`.
    fn wrap(self) -> Arc<Self> {
        let fs = Arc::new(self);
        let weak = Arc::downgrade(&fs);
        let ptr = Arc::into_raw(fs) as *mut Self;
        unsafe {
            (*ptr).self_ref = weak;
            Arc::from_raw(ptr)
        }
    }

    /// Strong type version of `root_inode`
    pub fn mountpoint_root_inode(&self) -> Arc<MNode> {
        MNode {
            poison: MNODE_POISON,
            inode: self.inner.root_inode(),
            vfs: self.self_ref.upgrade().unwrap(),
            self_ref: Weak::default(),
        }
        .wrap()
    }
}

impl MNode {
    fn wrap(self) -> Arc<Self> {
        let inode = Arc::new(self);
        let weak = Arc::downgrade(&inode);
        let ptr = Arc::into_raw(inode) as *mut Self;
        unsafe {
            (*ptr).self_ref = weak;
            Arc::from_raw(ptr)
        }
    }

    /// Mount file system `fs` at this INode
    pub fn mount(&self, fs: Arc<dyn FileSystem>) -> Result<Arc<MountFS>> {
        let metadata = self.inode.metadata()?;
        if metadata.type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }
        if self.vfs.mountpoints.read().contains_key(&metadata.inode) {
            return Err(FsError::Busy);
        }
        let new_fs = MountFS {
            inner: fs,
            mountpoints: RwLock::new(BTreeMap::new()),
            self_mountpoint: Some(self.self_ref.upgrade().unwrap()),
            self_ref: Weak::default(),
        }
        .wrap();
        self.vfs
            .mountpoints
            .write()
            .insert(metadata.inode, new_fs.clone());
        Ok(new_fs)
    }

    /// Returns whether a child filesystem is mounted at this directory.
    ///
    /// Same rule as [`Self::overlaid_inode`], and for the same reason: a node
    /// whose id cannot be read is a node no mount can be keyed to. It used to
    /// fall back to id `0` and ask the mount table about THAT, which is a
    /// different node's answer -- 0 is an id a filesystem in this tree really
    /// hands out (every value file under `/sys` reports it), so the fallback is
    /// not a harmless placeholder.
    pub fn is_mountpoint(&self) -> bool {
        let inode_id = match self.inode.metadata() {
            Ok(metadata) => metadata.inode,
            Err(_) => return false,
        };
        self.vfs.mountpoints.read().contains_key(&inode_id)
    }

    /// Returns the mounted child filesystem, if any.
    pub fn mounted_inner_fs(&self) -> Option<Arc<dyn FileSystem>> {
        let inode_id = self.inode.metadata().ok()?.inode;
        self.vfs
            .mountpoints
            .read()
            .get(&inode_id)
            .map(|mfs| mfs.inner_fs())
    }

    /// Unmount a filesystem previously mounted at this directory.
    pub fn umount(&self) -> Result<()> {
        let inode_id = self.inode.metadata()?.inode;
        if self.vfs.mountpoints.write().remove(&inode_id).is_none() {
            return Err(FsError::InvalidParam);
        }
        Ok(())
    }

    /// The filesystem mounted over this node, or the node itself.
    ///
    /// The `metadata()` here is the one `check_poison` was put in front of, so
    /// it is fallible on purpose -- and on a real disk it is fallible anyway,
    /// because a bad sector answers `DeviceError`. It used to be `unwrap`ed,
    /// which turned a read error into a kernel PANIC in the one file whose
    /// stated job is to return `EIO` instead of dying. `find_name_by_child`
    /// calls this once per directory entry, so `getcwd` on a failing disk was
    /// enough.
    ///
    /// A node whose id cannot be read is a node no mount can be keyed to, so
    /// the answer is the same as for a node with nothing mounted over it.
    fn overlaid_inode(&self) -> Arc<MNode> {
        let inode_id = match self.metadata() {
            Ok(metadata) => metadata.inode,
            Err(_) => return self.self_ref.upgrade().unwrap(),
        };
        if let Some(sub_vfs) = self.vfs.mountpoints.read().get(&inode_id) {
            sub_vfs.mountpoint_root_inode()
        } else {
            self.self_ref.upgrade().unwrap()
        }
    }

    /// Whether this node is the root of the filesystem it belongs to.
    ///
    /// Two more of the same `unwrap`s, and this one is on the `..` path:
    /// `find(root, "..")` asks it before deciding whether to climb out to the
    /// mount point, so `cd ..` in a mounted filesystem whose disk had just
    /// failed panicked the kernel. A node that cannot say which inode it is
    /// cannot be shown to be the root, and answering `false` sends the caller
    /// down `self.inode.find("..")`, which reports the read error properly.
    fn is_mountpoint_root(&self) -> bool {
        match (
            self.inode.fs().root_inode().metadata(),
            self.inode.metadata(),
        ) {
            (Ok(root), Ok(here)) => root.inode == here.inode,
            _ => false,
        }
    }

    /// Look up a direct child on the backing inode (no mount-overlay walk).
    pub fn backing_find(&self, name: &str) -> Result<Arc<Self>> {
        Ok(MNode::from_backing(
            self.vfs.clone(),
            self.inode.find(name)?,
        ))
    }

    /// Wrap a backing-store child inode without traversing mount overlays.
    pub fn from_backing(vfs: Arc<MountFS>, inode: Arc<dyn INode>) -> Arc<Self> {
        MNode {
            poison: MNODE_POISON,
            inode,
            vfs,
            self_ref: Weak::default(),
        }
        .wrap()
    }

    pub fn create(&self, name: &str, type_: FileType, mode: u32) -> Result<Arc<Self>> {
        Ok(MNode {
            poison: MNODE_POISON,
            inode: self.inode.create(name, type_, mode)?,
            vfs: self.vfs.clone(),
            self_ref: Weak::default(),
        }
        .wrap())
    }

    pub fn find(&self, root: bool, name: &str) -> Result<Arc<Self>> {
        match name {
            "" | "." => Ok(self.self_ref.upgrade().unwrap()),
            ".." => {
                if root {
                    Ok(self.self_ref.upgrade().unwrap())
                } else if self.is_mountpoint_root() {
                    match &self.vfs.self_mountpoint {
                        Some(inode) => inode.find(root, ".."),
                        None => Ok(self.self_ref.upgrade().unwrap()),
                    }
                } else {
                    Ok(MNode {
                        poison: MNODE_POISON,
                        inode: self.inode.find(name)?,
                        vfs: self.vfs.clone(),
                        self_ref: Weak::default(),
                    }
                    .wrap())
                }
            }
            _ => {
                let node = MNode {
                    poison: MNODE_POISON,
                    inode: self.inode.find(name)?,
                    vfs: self.vfs.clone(),
                    self_ref: Weak::default(),
                }
                .wrap();
                // The third copy of the same rule (see `overlaid_inode` and
                // `is_mountpoint`): only a node that can say which inode it is
                // can be shown to have something mounted on it. Reading `0`
                // when the answer is an error asks the mount table about a
                // node that is not this one.
                let inode_id = match node.inode.metadata() {
                    Ok(metadata) => metadata.inode,
                    Err(_) => return Ok(node),
                };
                if let Some(sub_vfs) = self.vfs.mountpoints.read().get(&inode_id) {
                    Ok(sub_vfs.mountpoint_root_inode())
                } else {
                    Ok(node)
                }
            }
        }
    }

    pub fn find_name_by_child(&self, child: &Arc<MNode>) -> Result<String> {
        for index in 0.. {
            let name = self.inode.get_entry(index)?;
            match name.as_ref() {
                "." | ".." => {}
                _ => {
                    // `find` already resolves a mount point to the root of what
                    // is mounted on it, so `overlaid_inode` answers the same
                    // node for every entry reachable this way; it is here for
                    // the contract, not for an input that needs it. Its own
                    // tests are what cover it.
                    let queryback = self.find(false, &name)?.overlaid_inode();
                    if Arc::ptr_eq(&queryback.vfs, &child.vfs)
                        && queryback.inode.metadata()?.inode == child.inode.metadata()?.inode
                    {
                        return Ok(name);
                    }
                }
            }
        }
        Err(FsError::EntryNotFound)
    }
}

impl FileSystem for MountFS {
    fn sync(&self) -> Result<()> {
        self.inner.sync()?;
        for mount_fs in self.mountpoints.read().values() {
            mount_fs.sync()?;
        }
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        match &self.self_mountpoint {
            Some(inode) => inode.vfs.root_inode(),
            None => self.mountpoint_root_inode(),
        }
    }

    fn info(&self) -> FsInfo {
        self.inner.info()
    }
}

impl INode for MNode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.check_poison("read_at")?;
        self.inode.read_at(offset, buf)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        self.check_poison("write_at")?;
        self.inode.write_at(offset, buf)
    }

    fn poll(&self) -> Result<PollStatus> {
        self.inode.poll()
    }

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        self.inode.async_poll()
    }

    fn metadata(&self) -> Result<Metadata> {
        self.check_poison("metadata")?;
        self.inode.metadata()
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        self.inode.set_metadata(metadata)
    }

    fn sync_all(&self) -> Result<()> {
        self.inode.sync_all()
    }

    fn sync_data(&self) -> Result<()> {
        self.inode.sync_data()
    }

    fn resize(&self, len: usize) -> Result<()> {
        self.inode.resize(len)
    }

    fn create(&self, name: &str, type_: FileType, mode: u32) -> Result<Arc<dyn INode>> {
        Ok(self.create(name, type_, mode)?)
    }

    fn create2(
        &self,
        name: &str,
        type_: FileType,
        mode: u32,
        data: usize,
    ) -> Result<Arc<dyn INode>> {
        Ok(MNode {
            poison: MNODE_POISON,
            inode: self.inode.create2(name, type_, mode, data)?,
            vfs: self.vfs.clone(),
            self_ref: Weak::default(),
        }
        .wrap())
    }

    fn link(&self, name: &str, other: &Arc<dyn INode>) -> Result<()> {
        self.inode.link(name, other)
    }

    fn unlink(&self, name: &str) -> Result<()> {
        let inode_id = self.inode.find(name)?.metadata()?.inode;
        if self.vfs.mountpoints.read().contains_key(&inode_id) {
            return Err(FsError::Busy);
        }
        self.inode.unlink(name)
    }

    fn move_(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> Result<()> {
        self.inode.move_(old_name, target, new_name)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        self.check_poison("find")?;
        Ok(self.find(false, name)?)
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        self.check_poison("get_entry")?;
        self.inode.get_entry(id)
    }

    fn get_entry_with_metadata(&self, id: usize) -> Result<(Metadata, String)> {
        self.inode.get_entry_with_metadata(id)
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        self.inode.io_control(cmd, data)
    }

    fn mmap(&self, area: MMapArea) -> Result<()> {
        self.inode.mmap(area)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.vfs.clone()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self.inode.as_any_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use rcore_fs_ramfs::RamFS;

    // ---------------------------------------------------------------- helpers

    /// A `MountFS` over a fresh ramfs, and its root node.
    fn tree() -> (Arc<MountFS>, Arc<MNode>) {
        let fs = MountFS::new(RamFS::new());
        let root = fs.mountpoint_root_inode();
        (fs, root)
    }

    fn dir_at(at: &Arc<MNode>, name: &str) -> Arc<MNode> {
        at.create(name, FileType::Dir, 0o755).unwrap()
    }

    fn file_at(at: &Arc<MNode>, name: &str) -> Arc<MNode> {
        at.create(name, FileType::File, 0o644).unwrap()
    }

    fn ino(n: &Arc<MNode>) -> usize {
        n.metadata().unwrap().inode
    }

    fn as_dyn(n: &Arc<MNode>) -> Arc<dyn INode> {
        n.clone() as Arc<dyn INode>
    }

    /// The names a directory lists, without `.` and `..`.
    fn entries(d: &Arc<MNode>) -> Vec<String> {
        let mut v = Vec::new();
        for i in 2.. {
            match d.get_entry(i) {
                Ok(name) => v.push(name),
                Err(_) => break,
            }
        }
        v
    }

    /// Read one word of a live `MNode`, overwrite it, run `f`, and put it back.
    ///
    /// The words are the ones [`MNode::check_poison`] reads: 0 is the canary, 1
    /// and 2 are the halves of the inner inode's fat pointer, 3 is the vfs. The
    /// recorded corruption clobbers word 2 with a tiny value; this is the only
    /// way to reach the guard that exists for it. Restored before the node is
    /// dropped, because dropping an `Arc` through a clobbered vtable is the very
    /// fault being guarded against.
    fn with_word_clobbered<T, F>(node: &Arc<MNode>, word: usize, value: u64, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let raw = Arc::as_ptr(node) as *mut u64;
        unsafe {
            let saved = core::ptr::read_volatile(raw.add(word));
            core::ptr::write_volatile(raw.add(word), value);
            let got = f();
            core::ptr::write_volatile(raw.add(word), saved);
            got
        }
    }

    /// A filesystem that wraps another one and can be told to fail `metadata()`,
    /// which is what a disk with a bad sector does (`DeviceError`).
    ///
    /// It is the only way to reach the three `unwrap`s this batch removed:
    /// nothing a ramfs does can make `metadata()` fail, so `overlaid_inode` and
    /// `is_mountpoint_root` had no reachable failure to be tested against and
    /// panicked the kernel when a real disk produced one.
    struct FlakyFS {
        inner: Arc<dyn FileSystem>,
        fail: AtomicBool,
        /// `(real id, id to report)`. A filesystem numbers its inodes however it
        /// likes, so two of them collide freely -- a devfs and a ramfs both
        /// start at 1, and every value file under `/sys` reports 0. Without this
        /// knob every id in a test comes from one global counter and no test can
        /// tell an id apart from the node it belongs to.
        renumber: RwLock<Option<(usize, usize)>>,
        syncs: AtomicUsize,
        self_ref: RwLock<Weak<FlakyFS>>,
    }

    impl FlakyFS {
        fn new(inner: Arc<dyn FileSystem>) -> Arc<Self> {
            let fs = Arc::new(FlakyFS {
                inner,
                fail: AtomicBool::new(false),
                renumber: RwLock::new(None),
                syncs: AtomicUsize::new(0),
                self_ref: RwLock::new(Weak::default()),
            });
            *fs.self_ref.write() = Arc::downgrade(&fs);
            fs
        }

        fn break_the_disk(&self) {
            self.fail.store(true, Ordering::SeqCst);
        }

        fn report_as(&self, real: usize, reported: usize) {
            *self.renumber.write() = Some((real, reported));
        }
    }

    impl FileSystem for FlakyFS {
        fn sync(&self) -> Result<()> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            self.inner.sync()
        }

        fn root_inode(&self) -> Arc<dyn INode> {
            Flaky::wrap(self.self_ref.read().clone(), self.inner.root_inode())
        }

        fn info(&self) -> FsInfo {
            self.inner.info()
        }
    }

    struct Flaky {
        fs: Weak<FlakyFS>,
        inner: Arc<dyn INode>,
    }

    impl Flaky {
        fn wrap(fs: Weak<FlakyFS>, inner: Arc<dyn INode>) -> Arc<dyn INode> {
            Arc::new(Flaky { fs, inner })
        }

        fn broken(&self) -> bool {
            self.fs
                .upgrade()
                .map(|f| f.fail.load(Ordering::SeqCst))
                .unwrap_or(false)
        }

        fn renumber(&self) -> Option<(usize, usize)> {
            self.fs.upgrade().and_then(|f| *f.renumber.read())
        }
    }

    impl INode for Flaky {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
            self.inner.read_at(offset, buf)
        }
        fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
            self.inner.write_at(offset, buf)
        }
        fn poll(&self) -> Result<PollStatus> {
            self.inner.poll()
        }
        fn metadata(&self) -> Result<Metadata> {
            if self.broken() {
                return Err(FsError::DeviceError);
            }
            let mut metadata = self.inner.metadata()?;
            if let Some((real, reported)) = self.renumber() {
                if metadata.inode == real {
                    metadata.inode = reported;
                }
            }
            Ok(metadata)
        }
        fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
            self.inner.set_metadata(metadata)
        }
        fn sync_all(&self) -> Result<()> {
            self.inner.sync_all()
        }
        fn sync_data(&self) -> Result<()> {
            self.inner.sync_data()
        }
        fn resize(&self, len: usize) -> Result<()> {
            self.inner.resize(len)
        }
        fn create(&self, name: &str, type_: FileType, mode: u32) -> Result<Arc<dyn INode>> {
            Ok(Self::wrap(
                self.fs.clone(),
                self.inner.create(name, type_, mode)?,
            ))
        }
        fn link(&self, name: &str, other: &Arc<dyn INode>) -> Result<()> {
            self.inner.link(name, other)
        }
        fn unlink(&self, name: &str) -> Result<()> {
            self.inner.unlink(name)
        }
        fn move_(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> Result<()> {
            self.inner.move_(old_name, target, new_name)
        }
        fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
            Ok(Self::wrap(self.fs.clone(), self.inner.find(name)?))
        }
        fn get_entry(&self, id: usize) -> Result<String> {
            self.inner.get_entry(id)
        }
        fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
            self.inner.io_control(cmd, data)
        }
        fn mmap(&self, area: MMapArea) -> Result<()> {
            self.inner.mmap(area)
        }
        fn fs(&self) -> Arc<dyn FileSystem> {
            self.fs.upgrade().unwrap()
        }
        fn as_any_ref(&self) -> &dyn Any {
            self
        }
    }

    // ------------------------------------------------------------- mounting

    #[test]
    fn a_mounted_directory_resolves_to_the_root_of_what_is_mounted_there() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        let guest = RamFS::new();
        let guest_root_ino = guest.root_inode().metadata().unwrap().inode;
        mnt.mount(guest).unwrap();
        let over = root.find(false, "mnt").unwrap();
        assert_eq!(
            ino(&over),
            guest_root_ino,
            "walking to a mount point must arrive in the guest, not on the \
             directory it covers"
        );
        assert_ne!(ino(&over), ino(&mnt));
    }

    #[test]
    fn mounting_over_a_file_is_refused() {
        let (_top, root) = tree();
        let f = file_at(&root, "f");
        assert_eq!(f.mount(RamFS::new()).err(), Some(FsError::NotDir));
    }

    #[test]
    fn mounting_twice_over_one_directory_is_refused() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        mnt.mount(RamFS::new()).unwrap();
        assert_eq!(mnt.mount(RamFS::new()).err(), Some(FsError::Busy));
    }

    #[test]
    fn what_a_mountpoint_covers_is_hidden_and_comes_back_when_it_is_unmounted() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        file_at(&mnt, "underneath");
        mnt.mount(RamFS::new()).unwrap();
        let over = root.find(false, "mnt").unwrap();
        assert_eq!(
            over.find(false, "underneath").err(),
            Some(FsError::EntryNotFound)
        );
        mnt.umount().unwrap();
        let back = root.find(false, "mnt").unwrap();
        assert!(back.find(false, "underneath").is_ok());
    }

    #[test]
    fn is_mountpoint_is_true_only_where_something_is_mounted() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        let plain = dir_at(&root, "plain");
        assert!(!mnt.is_mountpoint());
        mnt.mount(RamFS::new()).unwrap();
        assert!(mnt.is_mountpoint());
        assert!(!plain.is_mountpoint());
        assert!(!root.is_mountpoint());
        mnt.umount().unwrap();
        assert!(!mnt.is_mountpoint());
    }

    #[test]
    fn mounted_inner_fs_names_the_filesystem_that_was_mounted_and_nothing_else() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        assert!(mnt.mounted_inner_fs().is_none());
        let guest: Arc<dyn FileSystem> = RamFS::new();
        mnt.mount(guest.clone()).unwrap();
        let named = mnt.mounted_inner_fs().unwrap();
        assert!(Arc::ptr_eq(&named, &guest));
        assert!(dir_at(&root, "plain").mounted_inner_fs().is_none());
    }

    #[test]
    fn umounting_a_directory_with_nothing_mounted_is_refused() {
        let (_top, root) = tree();
        let plain = dir_at(&root, "plain");
        assert_eq!(plain.umount().err(), Some(FsError::InvalidParam));
        assert_eq!(root.umount().err(), Some(FsError::InvalidParam));
    }

    #[test]
    fn unlinking_a_directory_something_is_mounted_on_is_refused() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        mnt.mount(RamFS::new()).unwrap();
        assert_eq!(root.unlink("mnt").err(), Some(FsError::Busy));
        mnt.umount().unwrap();
        root.unlink("mnt").unwrap();
    }

    #[test]
    fn a_mount_inside_a_mount_is_reached_through_both() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        mnt.mount(RamFS::new()).unwrap();
        let guest_root = root.find(false, "mnt").unwrap();
        let inner_mnt = dir_at(&guest_root, "deep");
        let innermost = RamFS::new();
        let innermost_ino = innermost.root_inode().metadata().unwrap().inode;
        inner_mnt.mount(innermost).unwrap();
        let reached = root
            .find(false, "mnt")
            .unwrap()
            .find(false, "deep")
            .unwrap();
        assert_eq!(ino(&reached), innermost_ino);
        // A file made in the innermost filesystem is visible along the whole
        // path and nowhere else.
        file_at(&reached, "leaf");
        assert!(root
            .find(false, "mnt")
            .unwrap()
            .find(false, "deep")
            .unwrap()
            .find(false, "leaf")
            .is_ok());
        assert_eq!(entries(&inner_mnt), Vec::<String>::new());
    }

    #[test]
    fn sync_reaches_the_filesystems_mounted_inside() {
        let (top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        let guest = FlakyFS::new(RamFS::new());
        mnt.mount(guest.clone()).unwrap();
        top.sync().unwrap();
        assert_eq!(
            guest.syncs.load(Ordering::SeqCst),
            1,
            "a `sync(2)` that stops at the top of the tree leaves every mounted \
             filesystem unflushed"
        );
    }

    // -------------------------------------------------------------- the dots

    #[test]
    fn the_empty_name_and_dot_are_the_node_itself() {
        let (_top, root) = tree();
        let d = dir_at(&root, "d");
        assert!(Arc::ptr_eq(&d.find(false, "").unwrap(), &d));
        assert!(Arc::ptr_eq(&d.find(false, ".").unwrap(), &d));
    }

    #[test]
    fn dotdot_at_the_top_of_the_tree_is_the_top_of_the_tree() {
        let (_top, root) = tree();
        // The `root` flag is how the process's own root is pinned: `..` from it
        // must not escape, whatever the filesystem underneath says.
        let d = dir_at(&root, "d");
        assert!(Arc::ptr_eq(&d.find(true, "..").unwrap(), &d));
        assert!(Arc::ptr_eq(&root.find(true, "..").unwrap(), &root));
    }

    #[test]
    fn dotdot_inside_one_filesystem_stays_inside_it() {
        let (_top, root) = tree();
        let a = dir_at(&root, "a");
        let b = dir_at(&a, "b");
        assert_eq!(ino(&b.find(false, "..").unwrap()), ino(&a));
        assert_eq!(ino(&a.find(false, "..").unwrap()), ino(&root));
    }

    #[test]
    fn dotdot_at_a_mount_root_climbs_out_to_the_directory_it_was_mounted_on() {
        let (_top, root) = tree();
        let outer = dir_at(&root, "outer");
        let mnt = dir_at(&outer, "mnt");
        mnt.mount(RamFS::new()).unwrap();
        let guest_root = root
            .find(false, "outer")
            .unwrap()
            .find(false, "mnt")
            .unwrap();
        // `cd /outer/mnt; cd ..` has to land in /outer, which is in the OTHER
        // filesystem: the guest's own `..` would answer its own root.
        let up = guest_root.find(false, "..").unwrap();
        assert_eq!(ino(&up), ino(&outer));
    }

    #[test]
    fn dotdot_at_the_root_of_a_mountfs_that_is_mounted_nowhere_is_itself() {
        let (_top, root) = tree();
        let up = root.find(false, "..").unwrap();
        assert!(
            Arc::ptr_eq(&up, &root),
            "the top of the tree is mounted on nothing, so there is nowhere to \
             climb to"
        );
    }

    // ------------------------------------------------------ naming a child

    #[test]
    fn find_name_by_child_names_the_child_the_way_its_parent_does() {
        let (_top, root) = tree();
        let d = dir_at(&root, "some-name");
        file_at(&root, "another");
        assert_eq!(root.find_name_by_child(&d).unwrap(), "some-name");
    }

    #[test]
    fn find_name_by_child_names_a_mounted_directory_by_the_name_it_covers() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        mnt.mount(RamFS::new()).unwrap();
        let guest_root = root.find(false, "mnt").unwrap();
        // This is what `overlaid_inode` is for: walking the parent's entries
        // gives the DIRECTORY, and the child being named is the mounted root.
        // `getcwd` inside /mnt is this call.
        assert_eq!(root.find_name_by_child(&guest_root).unwrap(), "mnt");
    }

    #[test]
    fn find_name_by_child_of_something_that_is_not_a_child_is_not_found() {
        let (_top, root) = tree();
        let a = dir_at(&root, "a");
        let stranger = dir_at(&a, "stranger");
        assert_eq!(
            root.find_name_by_child(&stranger).err(),
            Some(FsError::EntryNotFound)
        );
    }

    // ------------------------------------------------- through to the inside

    #[test]
    fn backing_find_goes_under_the_mount_and_find_goes_over_it() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        let hidden = file_at(&mnt, "underneath");
        mnt.mount(RamFS::new()).unwrap();
        let under = root.backing_find("mnt").unwrap();
        assert_eq!(ino(&under), ino(&mnt));
        assert_eq!(
            ino(&under.backing_find("underneath").unwrap()),
            ino(&hidden)
        );
        assert_ne!(ino(&root.find(false, "mnt").unwrap()), ino(&mnt));
    }

    #[test]
    fn the_filesystem_of_a_node_is_the_mount_wrapper_and_not_what_is_under_it() {
        let (top, root) = tree();
        let d = dir_at(&root, "d");
        let as_inode = as_dyn(&d);
        let named = as_inode.fs();
        assert!(Arc::ptr_eq(&named, &(top.clone() as Arc<dyn FileSystem>)));
    }

    #[test]
    fn a_node_created_through_the_wrapper_is_wrapped_too() {
        let (top, root) = tree();
        let made = as_dyn(&root).create("made", FileType::File, 0o644).unwrap();
        assert!(
            made.downcast_ref::<MNode>().is_none(),
            "downcast reaches the inner inode"
        );
        assert!(Arc::ptr_eq(&made.fs(), &(top as Arc<dyn FileSystem>)));
    }

    #[test]
    fn create2_through_the_wrapper_keeps_the_device_number() {
        let (top, root) = tree();
        let node = as_dyn(&root)
            .create2("tty", FileType::CharDevice, 0o666, make_rdev(5, 0))
            .unwrap();
        assert_eq!(node.metadata().unwrap().rdev, make_rdev(5, 0));
        assert!(Arc::ptr_eq(&node.fs(), &(top as Arc<dyn FileSystem>)));
    }

    #[test]
    fn reads_writes_and_truncates_go_straight_through() {
        let (_top, root) = tree();
        let f = file_at(&root, "f");
        assert_eq!(f.write_at(0, b"contents").unwrap(), 8);
        let mut buf = [0u8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(&buf, b"contents");
        f.resize(3).unwrap();
        assert_eq!(f.metadata().unwrap().size, 3);
        assert!(f.poll().unwrap().read);
        assert!(f.sync_all().is_ok() && f.sync_data().is_ok());
    }

    #[test]
    fn a_chmod_through_the_wrapper_reaches_the_inode() {
        let (_top, root) = tree();
        let f = file_at(&root, "f");
        let mut want = f.metadata().unwrap();
        want.mode = 0o600;
        f.set_metadata(&want).unwrap();
        assert_eq!(f.metadata().unwrap().mode, 0o600);
    }

    #[test]
    fn a_directory_lists_what_the_filesystem_under_it_lists() {
        let (_top, root) = tree();
        file_at(&root, "beta");
        dir_at(&root, "alpha");
        assert_eq!(root.get_entry(0).unwrap(), ".");
        assert_eq!(root.get_entry(1).unwrap(), "..");
        assert_eq!(
            entries(&root),
            alloc::vec![String::from("alpha"), String::from("beta")]
        );
        let (md, name) = root.get_entry_with_metadata(2).unwrap();
        assert_eq!(name, "alpha");
        assert_eq!(md.type_, FileType::Dir);
    }

    #[test]
    fn a_link_and_a_rename_go_through_to_the_filesystem_underneath() {
        let (_top, root) = tree();
        let f = file_at(&root, "f");
        as_dyn(&root).link("g", &as_dyn(&f)).unwrap();
        assert_eq!(ino(&root.find(false, "g").unwrap()), ino(&f));
        as_dyn(&root).move_("g", &as_dyn(&root), "h").unwrap();
        assert_eq!(root.find(false, "g").err(), Some(FsError::EntryNotFound));
        assert_eq!(ino(&root.find(false, "h").unwrap()), ino(&f));
    }

    #[test]
    fn a_mountfs_reports_the_quota_of_what_is_mounted_in_it_and_names_it() {
        let inner: Arc<dyn FileSystem> = RamFS::new();
        let want = inner.info().bsize;
        let top = MountFS::new(inner.clone());
        assert_eq!(top.info().bsize, want);
        // `inner_fs` is what is mounted HERE, not what is mounted inside it:
        // `statfs` on a path has to name the filesystem the path is on.
        assert!(Arc::ptr_eq(&top.inner_fs(), &inner));
        let root = top.mountpoint_root_inode();
        let mnt = dir_at(&root, "mnt");
        let guest: Arc<dyn FileSystem> = RamFS::new();
        let guest_mountfs = mnt.mount(guest.clone()).unwrap();
        assert!(Arc::ptr_eq(&top.inner_fs(), &inner));
        assert!(Arc::ptr_eq(&guest_mountfs.inner_fs(), &guest));
    }

    #[test]
    fn the_root_inode_of_a_mounted_filesystem_is_the_top_of_the_whole_tree() {
        let (top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        let guest_mountfs = mnt.mount(RamFS::new()).unwrap();
        // Not the guest's own root: a `MountFS` that was mounted somewhere
        // answers `root_inode` by climbing to the top, which is what makes an
        // absolute path resolve the same from anywhere in the tree.
        let from_guest = guest_mountfs.root_inode();
        let from_top = top.root_inode();
        assert_eq!(
            from_guest.metadata().unwrap().inode,
            from_top.metadata().unwrap().inode
        );
        assert_eq!(from_guest.metadata().unwrap().inode, ino(&root));
    }

    // ------------------------------------- a disk that cannot read its inodes

    #[test]
    fn a_node_whose_id_cannot_be_read_is_its_own_overlay() {
        let guest = FlakyFS::new(RamFS::new());
        let top = MountFS::new(guest.clone());
        let root = top.mountpoint_root_inode();
        let mnt = dir_at(&root, "mnt");
        mnt.mount(RamFS::new()).unwrap();
        assert!(
            !Arc::ptr_eq(&mnt.overlaid_inode(), &mnt),
            "while the disk answers, the overlay is the mounted root"
        );
        guest.break_the_disk();
        assert!(
            Arc::ptr_eq(&mnt.overlaid_inode(), &mnt),
            "a node whose id cannot be read is a node no mount can be keyed to, \
             so the answer is the node itself -- it used to be an `unwrap` and a \
             kernel panic"
        );
    }

    #[test]
    fn getcwd_over_a_disk_that_cannot_read_its_inodes_answers_an_error() {
        let guest = FlakyFS::new(RamFS::new());
        let top = MountFS::new(guest.clone());
        let root = top.mountpoint_root_inode();
        let d = dir_at(&root, "d");
        assert_eq!(root.find_name_by_child(&d).unwrap(), "d");
        guest.break_the_disk();
        // `find_name_by_child` calls `overlaid_inode` once per directory entry,
        // so `getcwd` on a failing disk was enough to panic the kernel.
        assert_eq!(
            root.find_name_by_child(&d).err(),
            Some(FsError::DeviceError),
            "the one file whose stated job is to answer EIO instead of dying"
        );
    }

    #[test]
    fn climbing_out_of_a_mounted_filesystem_that_cannot_read_its_inodes_answers() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        let guest = FlakyFS::new(RamFS::new());
        mnt.mount(guest.clone()).unwrap();
        let guest_root = root.find(false, "mnt").unwrap();
        // The parent of the MOUNT POINT, not the mount point: `/mnt` and the
        // guest's root are one path component, so `/mnt/..` is `/`.
        assert_eq!(ino(&guest_root.find(false, "..").unwrap()), ino(&root));
        guest.break_the_disk();
        // `find(.., "..")` asks `is_mountpoint_root` before deciding whether to
        // climb out, and that asked the disk twice through an `unwrap`: `cd ..`
        // in a mounted filesystem whose disk had just failed took the machine
        // down. A node that cannot say which inode it is cannot be shown to be
        // the root, so the walk goes to the guest's own `..` instead, which
        // reports whatever the disk says.
        assert!(guest_root.find(false, "..").is_ok());
        assert!(!guest_root.is_mountpoint());
    }

    #[test]
    fn dotdot_in_a_subdirectory_of_a_filesystem_whose_disk_failed_stays_inside_it() {
        let (_top, root) = tree();
        let mnt = dir_at(&root, "mnt");
        let guest = FlakyFS::new(RamFS::new());
        let guest_mountfs = mnt.mount(guest.clone()).unwrap();
        let guest_root = root.find(false, "mnt").unwrap();
        let sub = dir_at(&guest_root, "sub");
        assert_eq!(ino(&sub.find(false, "..").unwrap()), ino(&guest_root));
        guest.break_the_disk();
        // `sub` is not the root of anything, and a node that cannot say which
        // inode it is must not be TAKEN for one either: calling it a mount root
        // sends `..` out of the filesystem, so `cd /mnt/sub; cd ..` would land
        // in `/` instead of `/mnt`.
        //
        // Which filesystem the answer is in is the thing to look at, not its
        // inode number: the number is exactly what this disk can no longer say.
        let up = sub.find(false, "..").unwrap();
        assert!(
            Arc::ptr_eq(&up.vfs, &guest_mountfs),
            "`..` climbed out of the mounted filesystem"
        );
    }

    #[test]
    fn a_mount_hung_on_inode_zero_does_not_swallow_the_nodes_that_cannot_be_read() {
        let host = FlakyFS::new(RamFS::new());
        let top = MountFS::new(host.clone());
        let root = top.mountpoint_root_inode();
        let mnt = dir_at(&root, "mnt");
        let elsewhere = dir_at(&root, "elsewhere");
        // 0 is an id a filesystem in this tree really hands out, so it is a key
        // a mount really can be hung on.
        host.report_as(ino(&mnt), 0);
        assert_eq!(ino(&mnt), 0);
        mnt.mount(RamFS::new()).unwrap();
        host.break_the_disk();
        assert!(
            Arc::ptr_eq(&elsewhere.overlaid_inode(), &elsewhere),
            "a node whose id cannot be read was overlaid with whatever is \
             mounted at 0, which is a different node's answer"
        );
        assert!(!elsewhere.is_mountpoint());
        assert!(elsewhere.mounted_inner_fs().is_none());
        // And the same rule on the path every lookup takes.
        let found = root.find(false, "elsewhere").unwrap();
        assert!(Arc::ptr_eq(&found.vfs, &top));
    }

    #[test]
    fn find_name_by_child_will_not_name_a_node_of_another_filesystem() {
        let (_a, root_a) = tree();
        let x = dir_at(&root_a, "x");
        // A node of a different tree that reports the same inode number. Two
        // filesystems number independently, so an id alone does not identify a
        // node -- only an id together with the filesystem it came from.
        let other = FlakyFS::new(RamFS::new());
        let top_b = MountFS::new(other.clone());
        let root_b = top_b.mountpoint_root_inode();
        let impostor = dir_at(&root_b, "impostor");
        other.report_as(ino(&impostor), ino(&x));
        assert_eq!(ino(&impostor), ino(&x));
        assert_eq!(root_a.find_name_by_child(&x).unwrap(), "x");
        assert_eq!(
            root_a.find_name_by_child(&impostor).err(),
            Some(FsError::EntryNotFound),
            "`getcwd` would have answered a name from a filesystem this node is \
             not in"
        );
    }

    #[test]
    fn a_mountpoint_whose_id_cannot_be_read_is_not_reported_as_one() {
        let guest = FlakyFS::new(RamFS::new());
        let top = MountFS::new(guest.clone());
        let root = top.mountpoint_root_inode();
        let mnt = dir_at(&root, "mnt");
        mnt.mount(RamFS::new()).unwrap();
        assert!(mnt.is_mountpoint());
        assert!(mnt.mounted_inner_fs().is_some());
        guest.break_the_disk();
        assert!(!mnt.is_mountpoint());
        assert!(mnt.mounted_inner_fs().is_none());
    }

    // ----------------------------------------------------- the canary itself

    #[test]
    fn a_live_node_at_a_low_address_is_not_mistaken_for_garbage() {
        // Under libos the kernel is an ordinary Linux process and every pointer
        // is a low userspace address. A guard that measured them against a fixed
        // higher-half floor called all of them garbage, so EVERY MountFS
        // operation returned `DeviceError`: the dynamic loader could not resolve
        // `PT_INTERP` and all 302 cases of `Linux Libc Test Libos` failed at
        // spawn, each logging the canary intact -- which is what a false
        // positive looks like.
        let (_top, root) = tree();
        dir_at(&root, "d");
        assert!(
            (Arc::as_ptr(&root) as u64) < 0xffff_8000_0000_0000,
            "this test only means anything below the higher half"
        );
        assert!(root.metadata().is_ok());
        assert!(as_dyn(&root).find("d").is_ok());
        assert!(root.get_entry(0).is_ok());
        assert!(
            root.read_at(0, &mut [0u8; 1]).is_err(),
            "a directory, but not EIO"
        );
        assert_eq!(root.read_at(0, &mut [0u8; 1]).err(), Some(FsError::IsDir));
    }

    #[test]
    fn a_node_whose_canary_was_clobbered_answers_an_io_error() {
        let (_top, root) = tree();
        dir_at(&root, "d");
        let got = with_word_clobbered(&root, 0, 0, || {
            (
                root.metadata().err(),
                as_dyn(&root).find("d").err(),
                root.get_entry(0).err(),
            )
        });
        assert_eq!(got.0, Some(FsError::DeviceError));
        assert_eq!(got.1, Some(FsError::DeviceError));
        assert_eq!(got.2, Some(FsError::DeviceError));
    }

    #[test]
    fn a_node_whose_inner_pointer_was_clobbered_answers_an_io_error() {
        let (_top, root) = tree();
        let f = file_at(&root, "f");
        f.write_at(0, b"body").unwrap();
        // Word 2 is the inner inode's vtable half, and `0x87` is the value the
        // recorded corruption left there: calling through it faulted at `0x97`
        // and, with the corruption already on the stack, triple-faulted.
        let got = with_word_clobbered(&f, 2, 0x87, || {
            (
                f.metadata().err(),
                f.read_at(0, &mut [0u8; 4]).err(),
                f.write_at(0, b"x").err(),
            )
        });
        assert_eq!(got.0, Some(FsError::DeviceError));
        assert_eq!(got.1, Some(FsError::DeviceError));
        assert_eq!(got.2, Some(FsError::DeviceError));
        // Put back, the node works again: the guard refuses the operation, it
        // does not wreck the node.
        assert_eq!(f.read_at(0, &mut [0u8; 4]).unwrap(), 4);
    }

    #[test]
    fn a_node_whose_inner_data_pointer_was_clobbered_answers_an_io_error() {
        let (_top, root) = tree();
        let f = file_at(&root, "f");
        let got = with_word_clobbered(&f, 1, 0x1, || f.metadata().err());
        assert_eq!(got, Some(FsError::DeviceError));
    }

    #[test]
    fn the_unguarded_methods_are_the_ones_that_touch_no_vtable_of_their_own() {
        // `poll`, `set_metadata`, `resize` and the rest carry no canary check on
        // purpose -- the guard is on the five hottest paths, which is where the
        // recorded fault landed. Pinned so that removing a check from one of the
        // five is a test failure and not a silent regression.
        let (_top, root) = tree();
        let f = file_at(&root, "f");
        for word in [0usize, 2].iter() {
            let value = if *word == 0 { 0 } else { 0x87 };
            let got = with_word_clobbered(&f, *word, value, || {
                (
                    f.metadata().err(),
                    f.read_at(0, &mut [0u8; 1]).err(),
                    f.write_at(0, b"x").err(),
                )
            });
            assert_eq!(got.0, Some(FsError::DeviceError));
            assert_eq!(got.1, Some(FsError::DeviceError));
            assert_eq!(got.2, Some(FsError::DeviceError));
        }
        let got = with_word_clobbered(&root, 0, 0, || {
            (as_dyn(&root).find("f").err(), root.get_entry(0).err())
        });
        assert_eq!(got.0, Some(FsError::DeviceError));
        assert_eq!(got.1, Some(FsError::DeviceError));
    }
}
