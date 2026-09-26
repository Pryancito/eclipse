//! Built-in special device files

use super::*;

macro_rules! impl_inode {
    () => {
        fn set_metadata(&self, _metadata: &Metadata) -> Result<()> {
            Ok(())
        }
        fn sync_all(&self) -> Result<()> {
            Ok(())
        }
        fn sync_data(&self) -> Result<()> {
            Ok(())
        }
        fn resize(&self, _len: usize) -> Result<()> {
            Err(FsError::NotSupported)
        }
        fn create(&self, _name: &str, _type_: FileType, _mode: u32) -> Result<Arc<dyn INode>> {
            Err(FsError::NotDir)
        }
        fn unlink(&self, _name: &str) -> Result<()> {
            Err(FsError::NotDir)
        }
        fn link(&self, _name: &str, _other: &Arc<dyn INode>) -> Result<()> {
            Err(FsError::NotDir)
        }
        fn move_(&self, _old_name: &str, _target: &Arc<dyn INode>, _new_name: &str) -> Result<()> {
            Err(FsError::NotDir)
        }
        fn find(&self, _name: &str) -> Result<Arc<dyn INode>> {
            Err(FsError::NotDir)
        }
        fn get_entry(&self, _id: usize) -> Result<String> {
            Err(FsError::NotDir)
        }
        fn io_control(&self, _cmd: u32, _data: usize) -> Result<usize> {
            Err(FsError::NotSupported)
        }
        fn mmap(&self, _area: MMapArea) -> Result<()> {
            Err(FsError::NotSupported)
        }
        // No `fs()`. It used to be `unimplemented!()` here, which is a panic in
        // a kernel: a pseudo-device belongs to no file system, and the caller
        // asking which one is not doing anything wrong. `INode::fs` already
        // answers that with `vfs::no_fs()`, a file system that owns nothing --
        // this macro was overriding a working default with a landmine, and the
        // one caller that had met it (`linux-object`'s page-cache key, which
        // `write(2)` to /dev/null goes through) had to grow a special case to
        // avoid asking. Leaving the method out is the fix.
        fn as_any_ref(&self) -> &dyn Any {
            self
        }
    };
}

mod null;
mod zero;

pub use self::null::*;
pub use self::zero::*;
