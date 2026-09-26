//! Filesystem wrapper that enforces per-mount flags (e.g. read-only).
//!
//! A wrapper in the inode chain has two obligations beyond the flag it is
//! there for. It must be **transparent to a downcast**, which is what
//! `as_any_ref` below is about. And it must **stay in the chain**: an inode's
//! `fs()` is a way back to the root of its filesystem, so handing back the
//! wrapped filesystem let anyone walk out of a read-only mount and write
//! through the front door.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::sync::Weak;
use core::any::Any;
use core::future::Future;
use core::pin::Pin;

use delegate::delegate;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, MMapArea, Metadata, PollStatus, Result,
};

use super::mount_state::MountState;

fn ro_err() -> FsError {
    FsError::ReadOnly
}

pub fn wrap_fs(inner: Arc<dyn FileSystem>, state: Arc<MountState>) -> Arc<dyn FileSystem> {
    // `new_cyclic` so every inode can hold a way back to this wrapper, which is
    // what `INode::fs` owes its caller. The back-reference is weak and the root
    // inode is built once here: the strong direction is filesystem -> root, so
    // there is no cycle to leak, and `root_inode` can hand back the same `Arc`
    // every time.
    Arc::new_cyclic(|me: &Weak<FlaggedFs>| {
        let root = Arc::new(FlaggedINode {
            inner: inner.root_inode(),
            state: state.clone(),
            fs: me.clone(),
        });
        FlaggedFs { inner, root }
    })
}

struct FlaggedFs {
    inner: Arc<dyn FileSystem>,
    // No `state` here: `FsInfo` has no room for a read-only bit, so there is
    // nothing at this level to gate, and the flags live where they are read --
    // on the inodes.
    /// The wrapped root, built once. `root_inode` used to allocate a fresh
    /// wrapper per call, so two calls answered with two different `Arc`s and
    /// every `Arc::ptr_eq` against the root -- which is how a mount root is
    /// recognised -- was false.
    root: Arc<FlaggedINode>,
}

impl FileSystem for FlaggedFs {
    // Pure pass-throughs to the wrapped filesystem.
    delegate! {
        to self.inner {
            fn sync(&self) -> Result<()>;
            fn info(&self) -> FsInfo;
        }
    }

    // root_inode must answer with the wrapped root, so the flags propagate --
    // and with the SAME one each time.
    fn root_inode(&self) -> Arc<dyn INode> {
        self.root.clone()
    }
}

struct FlaggedINode {
    inner: Arc<dyn INode>,
    state: Arc<MountState>,
    /// The wrapper this inode belongs to, for [`INode::fs`]. Weak because the
    /// filesystem owns the root inode: a strong link both ways would leak the
    /// mount.
    fs: Weak<FlaggedFs>,
}

impl FlaggedINode {
    fn check_write(&self) -> Result<()> {
        if self.state.is_read_only() {
            return Err(ro_err());
        }
        Ok(())
    }

    fn wrap(&self, inode: Arc<dyn INode>) -> Arc<dyn INode> {
        Arc::new(FlaggedINode {
            inner: inode,
            state: self.state.clone(),
            fs: self.fs.clone(),
        })
    }
}

impl INode for FlaggedINode {
    // Read-only / metadata operations forward straight to the inner inode.
    delegate! {
        to self.inner {
            fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize>;
            fn metadata(&self) -> Result<Metadata>;
            fn sync_all(&self) -> Result<()>;
            fn sync_data(&self) -> Result<()>;
            fn get_entry(&self, id: usize) -> Result<alloc::string::String>;
            fn get_entry_with_metadata(
                &self,
                id: usize,
            ) -> Result<(Metadata, alloc::string::String)>;
            fn io_control(&self, cmd: u32, data: usize) -> Result<usize>;
            fn mmap(&self, area: MMapArea) -> Result<()>;
        }
    }

    /// The filesystem this inode belongs to -- **the wrapper**, not what it
    /// wraps.
    ///
    /// This was delegated straight to the inner inode, so `inode.fs()` handed
    /// back the unwrapped filesystem and `inode.fs().root_inode()` was a way
    /// out of a read-only mount: the root it returned carried no flags, and
    /// every write from there down went through. `rcore-fs-mountfs` answers
    /// with its own `vfs` for the same reason.
    fn fs(&self) -> Arc<dyn FileSystem> {
        match self.fs.upgrade() {
            Some(fs) => fs,
            // The mount is gone, so there is no flag left to enforce and
            // nothing this can widen: an inode that outlived its mount is
            // already detached from the tree.
            None => self.inner.fs(),
        }
    }

    // `async_poll` ties the returned future's lifetime to `&self`; kept explicit
    // rather than delegated so the borrow plumbing stays obvious.
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        self.inner.async_poll()
    }

    // Mutating operations are gated on the read-only flag.
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        self.check_write()?;
        self.inner.write_at(offset, buf)
    }

    fn poll(&self) -> Result<PollStatus> {
        let mut status = self.inner.poll()?;
        if self.state.is_read_only() {
            status.write = false;
        }
        Ok(status)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        self.check_write()?;
        self.inner.set_metadata(metadata)
    }

    fn resize(&self, len: usize) -> Result<()> {
        self.check_write()?;
        self.inner.resize(len)
    }

    fn create(&self, name: &str, type_: FileType, mode: u32) -> Result<Arc<dyn INode>> {
        self.check_write()?;
        Ok(self.wrap(self.inner.create(name, type_, mode)?))
    }

    fn create2(
        &self,
        name: &str,
        type_: FileType,
        mode: u32,
        data: usize,
    ) -> Result<Arc<dyn INode>> {
        self.check_write()?;
        Ok(self.wrap(self.inner.create2(name, type_, mode, data)?))
    }

    fn link(&self, name: &str, other: &Arc<dyn INode>) -> Result<()> {
        self.check_write()?;
        self.inner.link(name, other)
    }

    fn unlink(&self, name: &str) -> Result<()> {
        self.check_write()?;
        self.inner.unlink(name)
    }

    fn move_(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> Result<()> {
        self.check_write()?;
        self.inner.move_(old_name, target, new_name)
    }

    // find re-wraps so the returned child also carries the mount flags.
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        Ok(self.wrap(self.inner.find(name)?))
    }

    /// The wrapped inode's, as `MNode` does: `link` and `move_` take a
    /// second inode, and the filesystem underneath (`btrfs_mount`,
    /// `fat_mount`, ramfs) downcasts it to its own inode type. Answering
    /// with the wrapper made that downcast fail, `NotSameFs` (EXDEV) or
    /// `NotSupported` (ENOSYS), so on a mounted disk `ln` failed and `mv`
    /// fell back to a copy or failed outright. Nothing downcasts to
    /// `FlaggedINode` itself.
    fn as_any_ref(&self) -> &dyn Any {
        self.inner.as_any_ref()
    }
}

#[cfg(test)]
mod flagged_fs_tests {
    //! The wrapper handed the wrapped filesystem its own wrappers as the
    //! second inode of `link` and `move_`, and every filesystem underneath
    //! downcasts that inode to its own type: on a mounted disk `ln` was
    //! EXDEV and `mv` ENOSYS or a copy.

    use super::*;
    use crate::fs::mount_state::{MountState, MS_RDONLY};
    use alloc::vec::Vec;
    use lock::Mutex;
    use rcore_fs_ramfs::RamFS;

    fn state(flags: usize) -> Arc<MountState> {
        Arc::new(MountState::from_options(flags, ""))
    }

    fn ramfs(flags: usize) -> Arc<dyn INode> {
        wrap_fs(RamFS::new(), state(flags)).root_inode()
    }

    #[test]
    fn a_link_hands_the_filesystem_its_own_inode_not_the_wrapper() {
        let root = ramfs(0);
        let a = root.create("a", FileType::File, 0o644).unwrap();
        // `a` is one of ours; ramfs must still recognise it as one of its.
        root.link("b", &a).unwrap();
        let b = root.find("b").unwrap();
        assert_eq!(
            b.metadata().unwrap().inode,
            a.metadata().unwrap().inode,
            "b is a is the same inode"
        );
        assert_eq!(b.metadata().unwrap().nlinks, 2);
    }

    /// A directory that, like `btrfs_mount` and `fat_mount`, only accepts a
    /// `move_` target of its own type, and records which one it was given.
    struct StrictDir {
        moved_into: Mutex<Vec<usize>>,
    }

    impl INode for StrictDir {
        fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
            Err(FsError::IsDir)
        }
        fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
            Err(FsError::IsDir)
        }
        fn poll(&self) -> Result<PollStatus> {
            Err(FsError::IsDir)
        }
        fn move_(&self, _old: &str, target: &Arc<dyn INode>, _new: &str) -> Result<()> {
            let target = target
                .downcast_ref::<StrictDir>()
                .ok_or(FsError::NotSupported)?;
            self.moved_into
                .lock()
                .push(target as *const StrictDir as usize);
            Ok(())
        }
        fn as_any_ref(&self) -> &dyn Any {
            self
        }
    }

    fn strict(state: &Arc<MountState>) -> (Arc<StrictDir>, Arc<dyn INode>) {
        let dir = Arc::new(StrictDir {
            moved_into: Mutex::new(Vec::new()),
        });
        let wrapped: Arc<dyn INode> = Arc::new(FlaggedINode {
            inner: dir.clone(),
            state: state.clone(),
            fs: Weak::new(),
        });
        (dir, wrapped)
    }

    #[test]
    fn a_move_hands_the_filesystem_its_own_target_directory() {
        let rw = state(0);
        let (src, src_wrapped) = strict(&rw);
        let (dst, dst_wrapped) = strict(&rw);
        src_wrapped.move_("a", &dst_wrapped, "b").unwrap();
        assert_eq!(
            *src.moved_into.lock(),
            [Arc::as_ptr(&dst) as usize],
            "the filesystem saw its own directory"
        );
        // An inode from elsewhere is passed through for the filesystem's own
        // cross-device check, never mistaken for one of ours.
        let foreign: Arc<dyn INode> = RamFS::new().root_inode();
        assert_eq!(
            src_wrapped.move_("a", &foreign, "b"),
            Err(FsError::NotSupported)
        );
    }

    #[test]
    fn a_read_only_mount_refuses_every_write_until_remounted() {
        let st = state(MS_RDONLY);
        let root = wrap_fs(RamFS::new(), st.clone()).root_inode();
        assert_eq!(
            root.create("a", FileType::File, 0o644).err(),
            Some(FsError::ReadOnly)
        );
        assert_eq!(root.unlink("a").unwrap_err(), FsError::ReadOnly);
        assert_eq!(root.link("b", &root).unwrap_err(), FsError::ReadOnly);
        assert_eq!(root.move_("a", &root, "b").unwrap_err(), FsError::ReadOnly);
        assert_eq!(root.resize(0).unwrap_err(), FsError::ReadOnly);
        // `mount -o remount,rw` flips the shared state, and the inodes
        // already handed out follow it.
        st.set_read_only(false);
        let a = root.create("a", FileType::File, 0o644).unwrap();
        assert_eq!(a.write_at(0, b"x").unwrap(), 1);
    }

    #[test]
    fn through_the_mount_table_ln_and_mv_reach_the_filesystem() {
        // What a path lookup hands the syscalls: an `MNode` over our
        // wrapper over the filesystem, for a disk mounted with flags.
        let vfs = rcore_fs_mountfs::MountFS::new(wrap_fs(RamFS::new(), state(0)));
        let root = vfs.root_inode();
        let a = root.create("a", FileType::File, 0o644).unwrap();
        root.link("b", &a).unwrap();
        let d = root.create("d", FileType::Dir, 0o755).unwrap();
        root.move_("a", &d, "c").unwrap();
        assert!(root.find("a").is_err());
        assert_eq!(
            d.find("c").unwrap().metadata().unwrap().inode,
            root.find("b").unwrap().metadata().unwrap().inode
        );
    }
}

#[cfg(test)]
mod chain_tests {
    //! The other half of what a wrapper owes: staying in the chain. An
    //! inode's `fs()` is a way back to the root of its filesystem, so
    //! answering with the filesystem underneath handed out an unwrapped root
    //! and a read-only mount could be written through it. The mount root also
    //! has to be one inode, not a fresh allocation per call, because that is
    //! what `Arc::ptr_eq` against it is asking.

    use super::*;
    use crate::fs::mount_state::{MountState, MS_RDONLY};
    use rcore_fs_ramfs::RamFS;

    fn mount(flags: usize) -> Arc<dyn FileSystem> {
        wrap_fs(RamFS::new(), Arc::new(MountState::from_options(flags, "")))
    }

    fn mount_with(state: Arc<MountState>) -> Arc<dyn FileSystem> {
        wrap_fs(RamFS::new(), state)
    }

    #[test]
    fn the_filesystem_an_inode_names_is_the_mount_and_not_what_it_wraps() {
        let fs = mount(0);
        let root = fs.root_inode();
        let child = root.create("d", FileType::Dir, 0o755).unwrap();
        assert!(Arc::ptr_eq(&root.fs(), &fs));
        assert!(Arc::ptr_eq(&child.fs(), &fs));
        assert!(Arc::ptr_eq(&root.find("d").unwrap().fs(), &fs));
    }

    #[test]
    fn a_read_only_mount_cannot_be_written_through_the_filesystem_it_names() {
        let fs = mount(MS_RDONLY);
        let root = fs.root_inode();
        assert_eq!(
            root.create("x", FileType::File, 0o644).err(),
            Some(FsError::ReadOnly)
        );
        // The way back out: an inode names its filesystem, and its root used
        // to come back unwrapped.
        let back = root.fs().root_inode();
        assert_eq!(
            back.create("x", FileType::File, 0o644).err(),
            Some(FsError::ReadOnly)
        );
    }

    #[test]
    fn the_root_of_a_mount_is_the_same_inode_every_time() {
        let fs = mount(0);
        assert!(Arc::ptr_eq(&fs.root_inode(), &fs.root_inode()));
    }

    #[test]
    fn an_inode_whose_mount_is_gone_still_names_a_filesystem() {
        let inner = RamFS::new();
        let root = {
            let fs = wrap_fs(inner.clone(), Arc::new(MountState::from_options(0, "")));
            fs.root_inode()
        };
        // The back-reference to the wrapper is weak, so this is the one case
        // where the answer is the filesystem underneath -- and by then there
        // is no mount left and no flag left to enforce.
        assert!(Arc::ptr_eq(&root.fs(), &(inner as Arc<dyn FileSystem>)));
    }

    // ---- the flag it is there for ----

    #[test]
    fn a_read_only_mount_refuses_every_way_of_changing_a_file() {
        let rw = mount(0);
        let root = rw.root_inode();
        let d = root.create("d", FileType::Dir, 0o755).unwrap();
        let f = d.create("f", FileType::File, 0o644).unwrap();
        drop((root, d, f));

        let fs = mount(MS_RDONLY);
        let root = fs.root_inode();
        let f = root.create("f", FileType::File, 0o644);
        assert_eq!(f.err(), Some(FsError::ReadOnly));
        assert_eq!(
            root.create2("f", FileType::File, 0o644, 0).err(),
            Some(FsError::ReadOnly)
        );
        assert_eq!(root.unlink("anything").err(), Some(FsError::ReadOnly));
        assert_eq!(root.resize(0).err(), Some(FsError::ReadOnly));
        assert_eq!(root.write_at(0, b"x").err(), Some(FsError::ReadOnly));
        assert_eq!(root.link("l", &root).err(), Some(FsError::ReadOnly));
        assert_eq!(root.move_("a", &root, "b").err(), Some(FsError::ReadOnly));
        let meta = root.metadata().unwrap();
        assert_eq!(root.set_metadata(&meta).err(), Some(FsError::ReadOnly));
    }

    #[test]
    fn reading_a_read_only_mount_goes_through() {
        let fs = mount(MS_RDONLY);
        let root = fs.root_inode();
        assert!(root.metadata().is_ok());
        assert!(root.sync_all().is_ok());
        assert!(root.sync_data().is_ok());
        let mut buf = [0u8; 4];
        // A directory is not readable as bytes, but the call must reach the
        // filesystem to say so rather than being refused as a write.
        assert_ne!(root.read_at(0, &mut buf).err(), Some(FsError::ReadOnly));
    }

    #[test]
    fn poll_says_a_file_on_a_read_only_mount_cannot_be_written() {
        let state = Arc::new(MountState::from_options(0, ""));
        let fs = mount_with(state.clone());
        let f = fs.root_inode().create("f", FileType::File, 0o644).unwrap();
        assert!(f.poll().unwrap().write);
        // What `poll` answers is what `select`/`epoll` tell a program about
        // whether writing is worth trying.
        state.set_read_only(true);
        assert!(!f.poll().unwrap().write);
        assert!(f.poll().unwrap().read);
    }

    #[test]
    fn a_mount_made_read_only_after_the_fact_starts_refusing() {
        let state = Arc::new(MountState::from_options(0, ""));
        let fs = mount_with(state.clone());
        let root = fs.root_inode();
        let d = root.create("d", FileType::Dir, 0o755).unwrap();
        // `mount -o remount,ro` changes the state the mount already has, and
        // every inode already handed out has to start refusing -- which is why
        // they share one `Arc<MountState>` rather than a copy of the flag.
        state.set_read_only(true);
        assert_eq!(
            root.create("x", FileType::File, 0o644).err(),
            Some(FsError::ReadOnly)
        );
        assert_eq!(
            d.create("x", FileType::File, 0o644).err(),
            Some(FsError::ReadOnly)
        );
        state.set_read_only(false);
        assert!(d.create("x", FileType::File, 0o644).is_ok());
    }

    #[test]
    fn a_writable_mount_changes_nothing_about_the_filesystem_under_it() {
        let fs = mount(0);
        let root = fs.root_inode();
        let f = root.create("f", FileType::File, 0o644).unwrap();
        assert_eq!(f.write_at(0, b"hola").unwrap(), 4);
        let mut buf = [0u8; 4];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"hola");
        f.resize(2).unwrap();
        assert_eq!(f.metadata().unwrap().size, 2);
        root.unlink("f").unwrap();
        assert_eq!(root.find("f").err(), Some(FsError::EntryNotFound));
    }

    #[test]
    fn a_directory_walked_through_the_mount_keeps_its_flags_all_the_way_down() {
        let rw = mount(0);
        let root = rw.root_inode();
        let a = root.create("a", FileType::Dir, 0o755).unwrap();
        let b = a.create("b", FileType::Dir, 0o755).unwrap();
        b.create("c", FileType::Dir, 0o755).unwrap();
        drop((root, a, b));

        let state = Arc::new(MountState::from_options(0, ""));
        let fs = mount_with(state.clone());
        let root = fs.root_inode();
        let a = root.create("a", FileType::Dir, 0o755).unwrap();
        let b = a.create("b", FileType::Dir, 0o755).unwrap();
        state.set_read_only(true);
        // Three levels down, reached by `create` and by `find`, and both have
        // to answer for the mount.
        assert_eq!(
            b.create("c", FileType::Dir, 0o755).err(),
            Some(FsError::ReadOnly)
        );
        let found = root.find("a").unwrap().find("b").unwrap();
        assert_eq!(
            found.create("c", FileType::Dir, 0o755).err(),
            Some(FsError::ReadOnly)
        );
        assert!(Arc::ptr_eq(&found.fs(), &fs));
    }

    #[test]
    fn what_a_read_only_mount_does_not_refuse_is_written_down() {
        // `io_control` reaches the filesystem whatever the mount says: most
        // ioctls read, Linux gates the mutating ones one by one inside the
        // filesystem, and there is nothing here that can tell them apart. The
        // test is here so the day someone gives this wrapper a list, they find
        // the line that says the list is the only way.
        let fs = mount(MS_RDONLY);
        assert_ne!(
            fs.root_inode().io_control(0, 0).err(),
            Some(FsError::ReadOnly)
        );
    }
}
