use super::*;

/// ECLIPSE: `/dev/full` (char 1:7). Reads give zeros like `/dev/zero`;
/// every write fails with `ENOSPC` (`write_full`), which is how programs
/// test what they do when a disk fills up.
pub struct FullINode {
    inode_id: usize,
}

impl FullINode {
    pub fn new() -> Self {
        Self {
            inode_id: DevFS::new_inode_id(),
        }
    }
}

impl Default for FullINode {
    fn default() -> Self {
        Self::new()
    }
}

impl INode for FullINode {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NoDeviceSpace)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: DEVFS_DEV,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o666,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(1, 7),
        })
    }

    impl_inode!();
}
