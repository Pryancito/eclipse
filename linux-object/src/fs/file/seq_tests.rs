//! A file written out when it is read ([`INode::seq_text`]) is read through
//! an open file as `seq_read` reads a `seq_file`, measured on Linux 6.18 with
//! glibc and musl: from one copy until a read starts elsewhere than where the
//! last one ended, 0 past its end, and with no end for `lseek` to start from.
//! Each `read` wrote the whole file out again.

use super::*;
use async_std::task::block_on;
use core::sync::atomic::{AtomicUsize, Ordering};
use rcore_fs::vfs::{FsError, PollStatus};

/// A file whose text says how many times it has been written out.
struct Counted(AtomicUsize);

fn text(n: usize) -> Vec<u8> {
    alloc::format!("written out {} times\n", n).into_bytes()
}

impl INode for Counted {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        let text = self.seq_text().unwrap()?;
        let rest = text.get(offset..).unwrap_or(&[]);
        let len = rest.len().min(buf.len());
        buf[..len].copy_from_slice(&rest[..len]);
        Ok(len)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Ok(PollStatus::default())
    }
    fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
        crate::fs::pseudo::Pseudo::new("", FileType::File).metadata()
    }
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
    fn seq_text(&self) -> Option<rcore_fs::vfs::Result<Vec<u8>>> {
        Some(Ok(text(self.0.fetch_add(1, Ordering::SeqCst) + 1)))
    }
}

fn open() -> (Arc<File>, Arc<Counted>) {
    let inode = Arc::new(Counted(AtomicUsize::new(0)));
    let file = File::new(inode.clone(), OpenFlags::RDONLY, String::from("/proc/x"));
    (file, inode)
}

fn read(file: &File, len: usize) -> Vec<u8> {
    let mut buf = alloc::vec![0u8; len];
    let n = block_on(FileLike::read(file, &mut buf)).unwrap();
    buf.truncate(n);
    buf
}

fn pread(file: &File, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = alloc::vec![0u8; len];
    let n = block_on(FileLike::read_at(file, offset, &mut buf)).unwrap();
    buf.truncate(n);
    buf
}

fn times(inode: &Counted) -> usize {
    inode.0.load(Ordering::SeqCst)
}

#[test]
fn a_read_in_pieces_is_one_copy() {
    let (file, inode) = open();
    let mut got = Vec::new();
    // Bounded: a read that never moved the position would never end.
    for _ in 0..64 {
        let piece = read(&file, 3);
        if piece.is_empty() {
            break;
        }
        got.extend(piece);
    }
    assert_eq!(got, text(1));
    assert_eq!(times(&inode), 1, "written out once for the whole of it");
}

#[test]
fn past_the_end_a_read_is_0_and_from_the_start_it_is_written_out_again() {
    let (file, inode) = open();
    assert_eq!(read(&file, 64), text(1));
    assert!(read(&file, 64).is_empty());
    assert_eq!(times(&inode), 1, "a read at the end of the copy is the end");
    assert_eq!(File::seek(&file, SeekFrom::Start(0)).unwrap(), 0);
    assert_eq!(read(&file, 64), text(2));
}

#[test]
fn a_read_elsewhere_than_where_the_last_ended_is_of_a_new_copy() {
    let (file, inode) = open();
    assert_eq!(read(&file, 5), text(1)[..5]);
    // Where it ended: the same copy, whatever moved the position there.
    File::seek(&file, SeekFrom::Start(5)).unwrap();
    assert_eq!(read(&file, 3), text(1)[5..8]);
    File::seek(&file, SeekFrom::Current(-6)).unwrap();
    assert_eq!(read(&file, 64), text(2)[2..]);
    assert_eq!(times(&inode), 2);
}

/// `pread` goes by the same rule and leaves the position where it was. One
/// that read to the end leaves the copy ending there, so the `read` after it
/// is of a new one, as on Linux.
#[test]
fn pread_where_the_last_read_ended_goes_on_with_its_copy() {
    let (file, inode) = open();
    assert_eq!(read(&file, 5), text(1)[..5]);
    assert_eq!(pread(&file, 5, 64), text(1)[5..]);
    assert_eq!(times(&inode), 1);
    assert_eq!(pread(&file, 0, 64), text(2));
    assert_eq!(File::seek(&file, SeekFrom::Current(0)).unwrap(), 5);
    assert_eq!(read(&file, 64), text(3)[5..]);
}

#[test]
fn lseek_from_its_end_is_einval() {
    let (file, _) = open();
    for at in [0, -1, 1] {
        assert_eq!(
            File::seek(&file, SeekFrom::End(at)).err(),
            Some(LxError::EINVAL)
        );
    }
    assert_eq!(File::seek(&file, SeekFrom::Start(5)).unwrap(), 5);
    assert_eq!(File::seek(&file, SeekFrom::Current(3)).unwrap(), 8);
}

/// A file that is gone when it would be written out says so, from the read
/// that would have.
#[test]
fn a_copy_that_cannot_be_written_out_is_the_reads_error() {
    struct Gone;
    impl INode for Gone {
        fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            Err(FsError::EntryNotFound)
        }
        fn write_at(&self, _offset: usize, _buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
            Err(FsError::NotSupported)
        }
        fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
            Ok(PollStatus::default())
        }
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
        fn seq_text(&self) -> Option<rcore_fs::vfs::Result<Vec<u8>>> {
            Some(Err(FsError::EntryNotFound))
        }
    }
    let file = File::new(Arc::new(Gone), OpenFlags::RDONLY, String::from("/proc/x"));
    let mut buf = [0u8; 8];
    assert_eq!(
        block_on(FileLike::read(&*file, &mut buf)).err(),
        Some(LxError::ENOENT)
    );
    assert_eq!(File::seek(&file, SeekFrom::Current(0)).unwrap(), 0);
}

/// Any other file is asked once whether it is one: the open file remembers
/// the answer, which every other `read` paid for.
#[test]
fn any_other_file_is_asked_once() {
    struct Plain(AtomicUsize);
    impl INode for Plain {
        fn read_at(&self, _offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            buf.fill(b'x');
            Ok(buf.len())
        }
        fn write_at(&self, _offset: usize, _buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
            Err(FsError::NotSupported)
        }
        fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
            Ok(PollStatus::default())
        }
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
        fn seq_text(&self) -> Option<rcore_fs::vfs::Result<Vec<u8>>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            None
        }
    }
    let inode = Arc::new(Plain(AtomicUsize::new(0)));
    let file = File::new(inode.clone(), OpenFlags::RDONLY, String::from("/f"));
    for _ in 0..3 {
        assert_eq!(read(&file, 4), b"xxxx");
    }
    assert_eq!(pread(&file, 0, 4), b"xxxx");
    assert_eq!(File::seek(&file, SeekFrom::Current(0)).unwrap(), 12);
    assert_eq!(inode.0.load(Ordering::SeqCst), 1);
}
