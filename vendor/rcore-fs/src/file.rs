use crate::vfs::{INode, Metadata, Result};
use alloc::{string::String, sync::Arc};

pub struct File {
    inode: Arc<dyn INode>,
    offset: usize,
    readable: bool,
    writable: bool,
}

impl File {
    pub fn new(inode: Arc<dyn INode>, readable: bool, writable: bool) -> Self {
        File {
            inode,
            offset: 0,
            readable,
            writable,
        }
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        assert!(self.readable);
        let len = self.inode.read_at(self.offset, buf)?;
        self.offset += len;
        Ok(len)
    }

    pub fn write(&mut self, buf: &[u8]) -> Result<usize> {
        assert!(self.writable);
        let len = self.inode.write_at(self.offset, buf)?;
        self.offset += len;
        Ok(len)
    }

    pub fn info(&self) -> Result<Metadata> {
        self.inode.metadata()
    }

    pub fn get_entry(&self, id: usize) -> Result<String> {
        self.inode.get_entry(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{FileType, FsError, PollStatus, Timespec};
    use alloc::vec::Vec;
    use core::any::Any;
    use spin::Mutex;

    /// An inode that moves at most `chunk` bytes per call, which is what a
    /// short read or a short write looks like to `File`.
    struct Stutter {
        data: Mutex<Vec<u8>>,
        chunk: usize,
    }

    fn stutter(data: &[u8], chunk: usize) -> Arc<Stutter> {
        Arc::new(Stutter {
            data: Mutex::new(data.to_vec()),
            chunk,
        })
    }

    fn meta(size: usize) -> Metadata {
        Metadata {
            dev: 7,
            inode: 9,
            size,
            blk_size: 512,
            blocks: 1,
            atime: Timespec { sec: 1, nsec: 0 },
            mtime: Timespec { sec: 2, nsec: 0 },
            ctime: Timespec { sec: 3, nsec: 0 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        }
    }

    impl INode for Stutter {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
            let data = self.data.lock();
            if offset > data.len() {
                return Err(FsError::InvalidParam);
            }
            let len = buf.len().min(self.chunk).min(data.len() - offset);
            buf[..len].copy_from_slice(&data[offset..offset + len]);
            Ok(len)
        }

        fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
            let mut data = self.data.lock();
            let len = buf.len().min(self.chunk);
            if data.len() < offset + len {
                data.resize(offset + len, 0);
            }
            data[offset..offset + len].copy_from_slice(&buf[..len]);
            Ok(len)
        }

        fn poll(&self) -> Result<PollStatus> {
            Ok(PollStatus::default())
        }

        fn metadata(&self) -> Result<Metadata> {
            Ok(meta(self.data.lock().len()))
        }

        fn get_entry(&self, id: usize) -> Result<String> {
            match id {
                0 => Ok(String::from(".")),
                1 => Ok(String::from("..")),
                _ => Err(FsError::EntryNotFound),
            }
        }

        fn as_any_ref(&self) -> &dyn Any {
            self
        }
    }

    /// Eight bytes three at a time: the offset has to follow the length the
    /// inode returned and not the length of the buffer handed in, or the second
    /// read skips what the first one did not reach.
    #[test]
    fn a_read_moves_the_offset_by_what_was_actually_read() {
        let mut f = File::new(stutter(b"abcdefgh", 3), true, false);
        let mut buf = [0u8; 8];
        assert_eq!(f.read(&mut buf).expect("the first read"), 3);
        assert_eq!(&buf[..3], b"abc");
        assert_eq!(f.read(&mut buf).expect("the second read"), 3);
        assert_eq!(&buf[..3], b"def");
        assert_eq!(f.read(&mut buf).expect("the third read"), 2);
        assert_eq!(&buf[..2], b"gh");
        assert_eq!(
            f.read(&mut buf).expect("the read at the end"),
            0,
            "a read past the end of the file"
        );
    }

    #[test]
    fn a_write_moves_the_offset_by_what_was_actually_written() {
        let inode = stutter(b"", 3);
        let mut f = File::new(inode.clone(), false, true);
        assert_eq!(f.write(b"abcdefgh").expect("the first write"), 3);
        assert_eq!(f.write(b"defgh").expect("the second write"), 3);
        assert_eq!(
            inode.data.lock().as_slice(),
            b"abcdef",
            "the second write did not go where the first one stopped"
        );
    }

    #[test]
    #[should_panic]
    fn reading_what_was_not_opened_for_reading_is_a_bug_and_not_an_error() {
        let mut f = File::new(stutter(b"abc", 3), false, true);
        let _ = f.read(&mut [0u8; 3]);
    }

    #[test]
    #[should_panic]
    fn writing_what_was_not_opened_for_writing_is_a_bug_and_not_an_error() {
        let mut f = File::new(stutter(b"abc", 3), true, false);
        let _ = f.write(b"x");
    }

    #[test]
    fn the_metadata_and_the_directory_entries_are_the_inodes_own() {
        let f = File::new(stutter(b"abcde", 3), true, true);
        assert_eq!(f.info().expect("metadata").size, 5);
        assert_eq!(f.get_entry(1).expect("the second entry"), "..");
        assert_eq!(f.get_entry(9), Err(FsError::EntryNotFound));
    }

    /// Reading does not disturb where a write will land, and the other way
    /// round: one offset serves both, the way `read(2)` and `write(2)` share
    /// one on a file descriptor.
    #[test]
    fn the_read_and_the_write_offset_are_the_same_offset() {
        let inode = stutter(b"abcdefgh", 3);
        let mut f = File::new(inode.clone(), true, true);
        let mut buf = [0u8; 3];
        assert_eq!(f.read(&mut buf).expect("the read"), 3);
        assert_eq!(f.write(b"XYZ").expect("the write"), 3);
        assert_eq!(
            inode.data.lock().as_slice(),
            b"abcXYZgh",
            "the write did not carry on from where the read stopped"
        );
    }
}
