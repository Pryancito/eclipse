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

/// INode for `MountFS`
pub struct MNode {
    /// The inner INode
    inode: Arc<dyn INode>,
    /// Associated `MountFS`
    vfs: Arc<MountFS>,
    /// Weak reference to self
    self_ref: Weak<MNode>,
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
    pub fn is_mountpoint(&self) -> bool {
        let inode_id = self.inode.metadata().map(|m| m.inode).unwrap_or(0);
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

    fn overlaid_inode(&self) -> Arc<MNode> {
        let inode_id = self.metadata().unwrap().inode;
        if let Some(sub_vfs) = self.vfs.mountpoints.read().get(&inode_id) {
            sub_vfs.mountpoint_root_inode()
        } else {
            self.self_ref.upgrade().unwrap()
        }
    }

    fn is_mountpoint_root(&self) -> bool {
        self.inode.fs().root_inode().metadata().unwrap().inode
            == self.inode.metadata().unwrap().inode
    }

    /// Look up a direct child on the backing inode (no mount-overlay walk).
    pub fn backing_find(&self, name: &str) -> Result<Arc<Self>> {
        Ok(MNode::from_backing(self.vfs.clone(), self.inode.find(name)?))
    }

    /// Wrap a backing-store child inode without traversing mount overlays.
    pub fn from_backing(vfs: Arc<MountFS>, inode: Arc<dyn INode>) -> Arc<Self> {
        MNode {
            inode,
            vfs,
            self_ref: Weak::default(),
        }
        .wrap()
    }

    pub fn create(&self, name: &str, type_: FileType, mode: u32) -> Result<Arc<Self>> {
        Ok(MNode {
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
                        inode: self.inode.find(name)?,
                        vfs: self.vfs.clone(),
                        self_ref: Weak::default(),
                    }
                    .wrap())
                }
            }
            _ => {
                let node = MNode {
                    inode: self.inode.find(name)?,
                    vfs: self.vfs.clone(),
                    self_ref: Weak::default(),
                }
                .wrap();
                let inode_id = node.inode.metadata().map(|m| m.inode).unwrap_or(0);
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
        self.inode.read_at(offset, buf)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
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
        Ok(self.find(false, name)?)
    }

    fn get_entry(&self, id: usize) -> Result<String> {
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
