//! Filesystem wrapper that enforces per-mount flags (e.g. read-only).

use alloc::boxed::Box;
use alloc::sync::Arc;
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
    Arc::new(FlaggedFs { inner, state })
}

struct FlaggedFs {
    inner: Arc<dyn FileSystem>,
    state: Arc<MountState>,
}

impl FileSystem for FlaggedFs {
    // Pure pass-throughs to the wrapped filesystem.
    delegate! {
        to self.inner {
            fn sync(&self) -> Result<()>;
            fn info(&self) -> FsInfo;
        }
    }

    // root_inode must re-wrap so the flags propagate to the returned inode.
    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::new(FlaggedINode {
            inner: self.inner.root_inode(),
            state: self.state.clone(),
        })
    }
}

struct FlaggedINode {
    inner: Arc<dyn INode>,
    state: Arc<MountState>,
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
            fn fs(&self) -> Arc<dyn FileSystem>;
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
