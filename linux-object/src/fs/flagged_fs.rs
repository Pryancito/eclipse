//! Filesystem wrapper that enforces per-mount flags (e.g. read-only).

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::future::Future;
use core::pin::Pin;

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
    fn sync(&self) -> Result<()> {
        self.inner.sync()
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::new(FlaggedINode {
            inner: self.inner.root_inode(),
            state: self.state.clone(),
        })
    }

    fn info(&self) -> FsInfo {
        self.inner.info()
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
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.inner.read_at(offset, buf)
    }

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

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        self.inner.async_poll()
    }

    fn metadata(&self) -> Result<Metadata> {
        self.inner.metadata()
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        self.check_write()?;
        self.inner.set_metadata(metadata)
    }

    fn sync_all(&self) -> Result<()> {
        self.inner.sync_all()
    }

    fn sync_data(&self) -> Result<()> {
        self.inner.sync_data()
    }

    fn resize(&self, len: usize) -> Result<()> {
        self.check_write()?;
        self.inner.resize(len)
    }

    fn create(&self, name: &str, type_: FileType, mode: u32) -> Result<Arc<dyn INode>> {
        self.check_write()?;
        Ok(self.wrap(self.inner.create(name, type_, mode)?))
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

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        Ok(self.wrap(self.inner.find(name)?))
    }

    fn get_entry(&self, id: usize) -> Result<alloc::string::String> {
        self.inner.get_entry(id)
    }

    fn get_entry_with_metadata(&self, id: usize) -> Result<(Metadata, alloc::string::String)> {
        self.inner.get_entry_with_metadata(id)
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        self.inner.io_control(cmd, data)
    }

    fn mmap(&self, area: MMapArea) -> Result<()> {
        self.inner.mmap(area)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.inner.fs()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}
