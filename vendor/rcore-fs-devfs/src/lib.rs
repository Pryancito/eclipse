#![cfg_attr(not(any(test, feature = "std")), no_std)]

extern crate alloc;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
};
use core::any::Any;
use rcore_fs::vfs::*;
use spin::RwLock;

pub mod special;

/// Device file system
///
/// The filesystem for all device files.
/// It should be mounted at /dev.
///
/// The file system is readonly from the root INode.
/// You can add or remove devices through `add()` and `remove()`.
pub struct DevFS {
    root: Arc<DevINode>,
}

impl FileSystem for DevFS {
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        self.root.clone()
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

impl DevFS {
    pub fn new() -> Arc<Self> {
        let fs = Arc::new(Self {
            root: DevINode::new(),
        });
        *fs.root.fs.write() = Arc::downgrade(&fs);
        fs
    }

    pub fn root(&self) -> Arc<DevINode> {
        self.root.clone()
    }

    /// Generate a new inode id
    pub fn new_inode_id() -> usize {
        use core::sync::atomic::*;
        static ID: AtomicUsize = AtomicUsize::new(1);
        ID.fetch_add(1, Ordering::SeqCst)
    }
}

pub struct DevINode {
    this: Weak<DevINode>,
    parent: Weak<DevINode>,
    fs: RwLock<Weak<DevFS>>,
    children: RwLock<BTreeMap<String, Arc<dyn INode>>>,
    inode_id: usize,
}

impl DevINode {
    fn new_with_parent(parent: Weak<DevINode>) -> Arc<Self> {
        Self {
            this: Weak::default(),
            parent,
            fs: RwLock::new(Weak::default()),
            children: RwLock::new(BTreeMap::new()),
            inode_id: DevFS::new_inode_id(),
        }
        .wrap()
    }

    fn new() -> Arc<Self> {
        Self::new_with_parent(Weak::default())
    }

    /// Wrap pure DevFS with Arc
    /// Used in constructors
    fn wrap(self) -> Arc<Self> {
        // Create an Arc, make a Weak from it, then put it into the struct.
        // It's a little tricky.
        let this = Arc::new(self);
        let weak = Arc::downgrade(&this);
        let ptr = Arc::into_raw(this) as *mut Self;
        unsafe {
            (*ptr).this = weak;
        }
        unsafe { Arc::from_raw(ptr) }
    }

    pub fn add_dir(&self, name: &str) -> Result<Arc<DevINode>> {
        let mut children = self.children.write();
        if children.contains_key(name) {
            return Err(FsError::EntryExist);
        }
        let dir = Self::new_with_parent(self.this.clone());
        *dir.fs.write() = self.fs.read().clone();
        children.insert(String::from(name), dir.clone());
        Ok(dir)
    }

    pub fn add(&self, name: &str, dev: Arc<dyn INode>) -> Result<()> {
        let mut children = self.children.write();
        if children.contains_key(name) {
            return Err(FsError::EntryExist);
        }
        children.insert(String::from(name), dev);
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let mut children = self.children.write();
        children.remove(name).ok_or(FsError::EntryNotFound)?;
        Ok(())
    }
}

impl INode for DevINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::IsDir)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::IsDir)
    }

    fn poll(&self) -> Result<PollStatus> {
        Err(FsError::IsDir)
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: self.inode_id,
            size: self.children.read().len(),
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::Dir,
            mode: 0o755,
            nlinks: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn set_metadata(&self, _metadata: &Metadata) -> Result<()> {
        Err(FsError::NotSupported)
    }

    fn sync_all(&self) -> Result<()> {
        Ok(())
    }

    fn sync_data(&self) -> Result<()> {
        Ok(())
    }

    fn resize(&self, _len: usize) -> Result<()> {
        Err(FsError::IsDir)
    }

    fn create(&self, _name: &str, _type_: FileType, _mode: u32) -> Result<Arc<dyn INode>> {
        Err(FsError::NotSupported)
    }

    fn link(&self, _name: &str, _other: &Arc<dyn INode>) -> Result<()> {
        Err(FsError::NotSupported)
    }

    fn unlink(&self, _name: &str) -> Result<()> {
        Err(FsError::NotSupported)
    }

    fn move_(&self, _old_name: &str, _target: &Arc<dyn INode>, _new_name: &str) -> Result<()> {
        Err(FsError::NotSupported)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(self.this.upgrade().ok_or(FsError::EntryNotFound)?),
            ".." => Ok(self.parent.upgrade().ok_or(FsError::EntryNotFound)?),
            name => self
                .children
                .read()
                .get(name)
                .cloned()
                .ok_or(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            i => {
                if let Some(s) = self.children.read().keys().nth(i - 2) {
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
        self.fs.read().upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::special::{NullINode, ZeroINode};
    use super::*;

    fn null() -> Arc<dyn INode> {
        Arc::new(NullINode::new()) as Arc<dyn INode>
    }

    fn zero() -> Arc<dyn INode> {
        Arc::new(ZeroINode::new()) as Arc<dyn INode>
    }

    fn ino(n: &Arc<dyn INode>) -> usize {
        n.metadata().unwrap().inode
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

    /// A `/dev` with the two nodes every system has.
    fn dev() -> Arc<DevFS> {
        let fs = DevFS::new();
        fs.root().add("null", null()).unwrap();
        fs.root().add("zero", zero()).unwrap();
        fs
    }

    // ----------------------------------------------------------- the registry

    #[test]
    fn a_device_is_handed_back_exactly_as_it_was_added() {
        let fs = DevFS::new();
        let n = null();
        fs.root().add("null", Arc::clone(&n)).unwrap();
        let found = fs.root_inode().find("null").unwrap();
        assert!(
            Arc::ptr_eq(&found, &n),
            "a device node is registered, not copied: the driver on the other \
             side of it is the same object"
        );
    }

    #[test]
    fn adding_a_name_that_is_taken_is_refused_and_keeps_the_first_one() {
        let fs = DevFS::new();
        let first = null();
        fs.root().add("null", Arc::clone(&first)).unwrap();
        let second = null();
        assert_eq!(
            fs.root().add("null", Arc::clone(&second)).err(),
            Some(FsError::EntryExist)
        );
        assert!(Arc::ptr_eq(&fs.root_inode().find("null").unwrap(), &first));
    }

    #[test]
    fn adding_a_directory_whose_name_is_taken_is_refused() {
        let fs = DevFS::new();
        fs.root().add("input", null()).unwrap();
        assert_eq!(fs.root().add_dir("input").err(), Some(FsError::EntryExist));
    }

    #[test]
    fn removing_a_device_takes_its_name_away() {
        let fs = dev();
        fs.root().remove("null").unwrap();
        assert_eq!(
            fs.root_inode().find("null").err(),
            Some(FsError::EntryNotFound)
        );
        assert_eq!(entries(&fs.root_inode()), alloc::vec![String::from("zero")]);
    }

    #[test]
    fn removing_a_name_that_is_not_there_is_refused() {
        let fs = DevFS::new();
        assert_eq!(fs.root().remove("null").err(), Some(FsError::EntryNotFound));
    }

    #[test]
    fn the_root_the_filesystem_hands_out_is_the_one_devices_are_added_to() {
        let fs = DevFS::new();
        fs.root().add("null", null()).unwrap();
        let by_trait = fs.root_inode();
        let by_hand = fs.root() as Arc<dyn INode>;
        assert!(Arc::ptr_eq(&by_trait, &by_hand));
        assert!(by_trait.find("null").is_ok());
    }

    // -------------------------------------------------- read only from above

    #[test]
    fn userspace_cannot_add_or_remove_anything_in_a_devfs() {
        let fs = dev();
        let root = fs.root_inode();
        // `mknod`, `ln`, `rm` and `mv` in /dev all end here. Devices arrive
        // through `add`, from the driver that owns them, and nowhere else.
        assert_eq!(
            root.create("mine", FileType::CharDevice, 0o666).err(),
            Some(FsError::NotSupported)
        );
        assert_eq!(
            root.link("also_null", &root.find("null").unwrap()).err(),
            Some(FsError::NotSupported)
        );
        assert_eq!(root.unlink("null").err(), Some(FsError::NotSupported));
        assert_eq!(
            root.move_("null", &root, "nil").err(),
            Some(FsError::NotSupported)
        );
        assert!(root.find("null").is_ok());
    }

    #[test]
    fn a_devfs_directory_refuses_a_chmod() {
        let fs = DevFS::new();
        let root = fs.root_inode();
        let want = root.metadata().unwrap();
        assert_eq!(root.set_metadata(&want).err(), Some(FsError::NotSupported));
    }

    // ------------------------------------------------------- the directories

    #[test]
    fn a_devfs_directory_is_a_directory_with_the_mode_a_dev_has() {
        let fs = dev();
        let md = fs.root_inode().metadata().unwrap();
        assert_eq!(md.type_, FileType::Dir);
        assert_eq!(md.mode, 0o755);
        assert_eq!(md.nlinks, 2);
        assert_eq!(
            md.size, 2,
            "the size of a /dev is how many entries it holds"
        );
    }

    #[test]
    fn a_devfs_subdirectory_climbs_back_to_the_root_above_it() {
        let fs = DevFS::new();
        let input = fs.root().add_dir("input").unwrap() as Arc<dyn INode>;
        let root = fs.root_inode();
        assert_eq!(ino(&input.find("..").unwrap()), ino(&root));
        assert_eq!(ino(&input.find(".").unwrap()), ino(&input));
        // /dev/input/by-path, two levels down.
        let deeper = input
            .downcast_ref::<DevINode>()
            .unwrap()
            .add_dir("by-path")
            .unwrap() as Arc<dyn INode>;
        assert_eq!(ino(&deeper.find("..").unwrap()), ino(&input));
    }

    #[test]
    fn a_devfs_subdirectory_belongs_to_the_devfs_that_made_it() {
        let fs = DevFS::new();
        let input = fs.root().add_dir("input").unwrap() as Arc<dyn INode>;
        assert!(Arc::ptr_eq(&input.fs().root_inode(), &fs.root_inode()));
    }

    #[test]
    fn the_root_of_a_devfs_has_no_parent_of_its_own() {
        // A root that is not its own parent, unlike every other filesystem
        // here. It does not show: /dev is reached through a mount, and
        // `MountFS` answers `..` at a mount root by climbing to the directory
        // the filesystem was mounted on without ever asking the inode. This
        // pins the behaviour so that a future caller reaching past the mount
        // finds it written down rather than by surprise.
        let fs = DevFS::new();
        assert_eq!(
            fs.root_inode().find("..").err(),
            Some(FsError::EntryNotFound)
        );
        assert!(fs.root_inode().find(".").is_ok());
    }

    #[test]
    fn a_devfs_directory_lists_dot_dotdot_and_then_its_children_in_order() {
        let fs = dev();
        let root = fs.root_inode();
        assert_eq!(root.get_entry(0).unwrap(), ".");
        assert_eq!(root.get_entry(1).unwrap(), "..");
        assert_eq!(root.get_entry(2).unwrap(), "null");
        assert_eq!(root.get_entry(3).unwrap(), "zero");
        assert_eq!(root.get_entry(4).err(), Some(FsError::EntryNotFound));
    }

    #[test]
    fn a_devfs_directory_is_not_a_file() {
        let fs = dev();
        let root = fs.root_inode();
        assert_eq!(root.read_at(0, &mut [0u8; 4]).err(), Some(FsError::IsDir));
        assert_eq!(root.write_at(0, b"x").err(), Some(FsError::IsDir));
        assert_eq!(root.poll().err(), Some(FsError::IsDir));
        assert_eq!(root.resize(0).err(), Some(FsError::IsDir));
    }

    #[test]
    fn a_devfs_directory_supports_neither_ioctl_nor_mmap() {
        let fs = DevFS::new();
        let root = fs.root_inode();
        assert_eq!(root.io_control(0, 0).err(), Some(FsError::NotSupported));
        assert_eq!(
            root.mmap(MMapArea {
                start_vaddr: 0,
                end_vaddr: 0x1000,
                prot: 0,
                flags: 0,
                offset: 0,
            })
            .err(),
            Some(FsError::NotSupported)
        );
    }

    #[test]
    fn a_devfs_syncs_and_reports_no_quota() {
        let fs = dev();
        assert!(fs.sync().is_ok());
        assert!(fs.root_inode().sync_all().is_ok());
        assert!(fs.root_inode().sync_data().is_ok());
        let info = fs.info();
        assert_eq!((info.blocks, info.bfree, info.namemax), (0, 0, 0));
    }

    #[test]
    fn every_devfs_inode_has_an_id_of_its_own() {
        let fs = dev();
        let root = fs.root_inode();
        let input = fs.root().add_dir("input").unwrap() as Arc<dyn INode>;
        let mut ids = alloc::vec![
            ino(&root),
            ino(&input),
            ino(&root.find("null").unwrap()),
            ino(&root.find("zero").unwrap()),
        ];
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            before,
            "`MountFS` keys its mount points by this number, so a repeat would \
             put a mounted filesystem over the wrong directory"
        );
    }

    // -------------------------------------------------- /dev/null, /dev/zero

    #[test]
    fn null_reads_nothing_and_leaves_the_buffer_as_it_found_it() {
        let n = null();
        let mut buf = [0xabu8; 16];
        assert_eq!(n.read_at(0, &mut buf).unwrap(), 0);
        assert_eq!(&buf, &[0xabu8; 16], "a read of nothing writes nothing");
    }

    #[test]
    fn null_swallows_every_write_whole() {
        let n = null();
        assert_eq!(n.write_at(0, b"anything").unwrap(), 8);
        assert_eq!(n.write_at(1 << 40, &[0u8; 4096]).unwrap(), 4096);
        assert_eq!(n.write_at(0, b"").unwrap(), 0);
        assert_eq!(n.metadata().unwrap().size, 0, "and grows not at all");
    }

    #[test]
    fn zero_fills_whatever_it_is_given_at_any_offset() {
        let z = zero();
        let mut buf = [0xabu8; 64];
        assert_eq!(z.read_at(0, &mut buf).unwrap(), 64);
        assert!(buf.iter().all(|&b| b == 0));
        let mut buf = [0xabu8; 3];
        assert_eq!(z.read_at(1 << 40, &mut buf).unwrap(), 3);
        assert!(buf.iter().all(|&b| b == 0));
        assert_eq!(z.read_at(0, &mut []).unwrap(), 0);
    }

    #[test]
    fn zero_swallows_every_write_whole() {
        let z = zero();
        assert_eq!(z.write_at(99, b"discarded").unwrap(), 9);
        assert_eq!(z.metadata().unwrap().size, 0);
    }

    #[test]
    fn the_special_devices_carry_the_numbers_linux_gives_them() {
        // /dev/null is 1:3 and /dev/zero is 1:5 on every Linux ever shipped,
        // and a libc that stats them checks.
        assert_eq!(null().metadata().unwrap().rdev, make_rdev(1, 3));
        assert_eq!(zero().metadata().unwrap().rdev, make_rdev(1, 5));
    }

    #[test]
    fn a_special_device_is_a_character_device_anyone_may_use() {
        for node in [null(), zero()].iter() {
            let md = node.metadata().unwrap();
            assert_eq!(md.type_, FileType::CharDevice);
            assert_eq!(md.mode, 0o666);
            assert_eq!(md.nlinks, 1);
            assert_eq!(md.dev, 1);
            assert_eq!(md.blocks, 0);
        }
    }

    #[test]
    fn a_special_device_is_always_ready_and_never_hangs_up() {
        for node in [null(), zero()].iter() {
            let p = node.poll().unwrap();
            assert!(p.read && p.write);
            assert!(
                !p.error && !p.hangup,
                "a POLLHUP on /dev/null would end every poll loop waiting on it"
            );
        }
    }

    #[test]
    fn a_special_device_is_not_a_directory() {
        for node in [null(), zero()].iter() {
            assert_eq!(node.find("x").err(), Some(FsError::NotDir));
            assert_eq!(node.get_entry(0).err(), Some(FsError::NotDir));
            assert_eq!(
                node.create("x", FileType::File, 0o644).err(),
                Some(FsError::NotDir)
            );
            assert_eq!(node.unlink("x").err(), Some(FsError::NotDir));
            assert_eq!(node.link("x", &null()).err(), Some(FsError::NotDir));
            assert_eq!(node.move_("x", &null(), "y").err(), Some(FsError::NotDir));
        }
    }

    #[test]
    fn a_special_device_cannot_be_truncated_or_mapped() {
        for node in [null(), zero()].iter() {
            assert_eq!(node.resize(0).err(), Some(FsError::NotSupported));
            assert_eq!(node.io_control(0, 0).err(), Some(FsError::NotSupported));
            assert_eq!(
                node.mmap(MMapArea {
                    start_vaddr: 0,
                    end_vaddr: 0x1000,
                    prot: 0,
                    flags: 0,
                    offset: 0,
                })
                .err(),
                Some(FsError::NotSupported)
            );
        }
    }

    #[test]
    fn a_special_device_accepts_a_chmod_and_a_sync_without_complaint() {
        let n = null();
        let md = n.metadata().unwrap();
        // `touch /dev/null` and `fsync` on it are ordinary things to do, and a
        // pseudo-device has nothing to store or flush.
        assert!(n.set_metadata(&md).is_ok());
        assert!(n.sync_all().is_ok());
        assert!(n.sync_data().is_ok());
    }

    #[test]
    fn a_special_device_answers_which_filesystem_it_is_on_instead_of_panicking() {
        // This used to be `unimplemented!()`, which is a kernel panic. A
        // pseudo-device belongs to no filesystem, and `no_fs()` says so.
        for node in [null(), zero()].iter() {
            let fs = node.fs();
            assert!(
                rcore_fs::vfs::is_no_fs(&fs),
                "a device with no filesystem must answer the placeholder, not \
                 take the machine down"
            );
            assert!(fs.sync().is_ok());
        }
    }

    #[test]
    fn two_special_devices_are_two_inodes() {
        assert_ne!(ino(&null()), ino(&null()));
        assert_ne!(ino(&null()), ino(&zero()));
    }

    #[test]
    fn a_special_device_downcasts_to_what_it_is() {
        let n = null();
        assert!(n.downcast_ref::<NullINode>().is_some());
        assert!(n.downcast_ref::<ZeroINode>().is_none());
        assert!(zero().downcast_ref::<ZeroINode>().is_some());
    }
}
