//! `sendfile(2)` and `copy_file_range(2)` on a ramfs file and a bounded,
//! socket-like output that is not a `File`. Every test names bytes the
//! old loop reported wrong: a chunk already out when `EAGAIN` came back,
//! which the caller then sent again.

use super::{copy_range, Copied};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use linux_object::error::{LxError, LxResult};
use linux_object::fs::vfs::FileType;
use linux_object::fs::{File, FileLike, OpenFlags, PollEvents, PollStatus, SeekFrom};
use rcore_fs::vfs::FileSystem;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::*;

/// A ramfs file holding `content`, opened read-write at offset 0.
fn file(content: &[u8]) -> Arc<dyn FileLike> {
    let fs = RamFS::new();
    let inode = fs.root_inode().create("f", FileType::File, 0o644).unwrap();
    if !content.is_empty() {
        inode.write_at(0, content).unwrap();
    }
    // The inode only holds a `Weak` to its filesystem, and `File::write`
    // upgrades it; keep the ramfs alive for the test.
    core::mem::forget(fs);
    File::new(inode, OpenFlags::RDWR, String::from("/f"))
}

fn bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// A socket-like output, not a `File`: takes what fits in `room`, then
/// answers `EAGAIN` (or, with `stall`, a write of 0), and gets `refill`
/// bytes of room back each time a writer waits on it.
struct Bounded {
    base: KObjectBase,
    taken: lock::Mutex<Vec<u8>>,
    room: AtomicUsize,
    refill: usize,
    stall: bool,
    waits: AtomicUsize,
    /// What it reports for its access mode. A socket does not police this
    /// itself the way a `File` does, which is what makes the two `EBADF`
    /// checks `copy_range`'s own business.
    flags: OpenFlags,
}

impl_kobject!(Bounded);

impl Bounded {
    fn new(room: usize, refill: usize, stall: bool) -> Arc<Self> {
        Self::with_flags(room, refill, stall, OpenFlags::RDWR)
    }

    fn with_flags(room: usize, refill: usize, stall: bool, flags: OpenFlags) -> Arc<Self> {
        Arc::new(Bounded {
            base: KObjectBase::new(),
            taken: lock::Mutex::new(Vec::new()),
            room: AtomicUsize::new(room),
            refill,
            stall,
            waits: AtomicUsize::new(0),
            flags,
        })
    }
    fn as_file_like(self: &Arc<Self>) -> Arc<dyn FileLike> {
        self.clone()
    }
}

#[async_trait::async_trait]
impl FileLike for Bounded {
    fn flags(&self) -> OpenFlags {
        self.flags
    }
    fn set_flags(&self, _: OpenFlags) -> LxResult {
        Ok(())
    }
    async fn read(&self, _: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }
    fn write(&self, buf: &[u8]) -> LxResult<usize> {
        let room = self.room.load(Ordering::SeqCst);
        if room == 0 {
            return if self.stall {
                Ok(0)
            } else {
                Err(LxError::EAGAIN)
            };
        }
        let take = buf.len().min(room);
        self.taken.lock().extend_from_slice(&buf[..take]);
        self.room.fetch_sub(take, Ordering::SeqCst);
        Ok(take)
    }
    async fn read_at(&self, _: u64, _: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }
    fn poll(&self, _: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus::default())
    }
    async fn async_poll(&self, _: PollEvents) -> LxResult<PollStatus> {
        self.waits.fetch_add(1, Ordering::SeqCst);
        self.room.fetch_add(self.refill, Ordering::SeqCst);
        Ok(PollStatus::default())
    }
}

/// A non-blocking socket that fills after the first chunk: the answer is
/// the bytes that went out, not `EAGAIN`. With the error and no count, a
/// server resent from the same offset and the peer got the head twice.
#[async_std::test]
async fn a_socket_that_fills_after_some_bytes_reports_them_not_eagain() {
    let f = file(&bytes(3000));
    let sock = Bounded::new(1000, 0, false);
    let c = copy_range(&f, Some(0), &sock.as_file_like(), None, 3000, false)
        .await
        .unwrap();
    assert_eq!(
        c,
        Copied {
            moved: 1000,
            in_offset: Some(1000),
            out_offset: None
        }
    );
    assert_eq!(sock.taken.lock().as_slice(), &bytes(3000)[..1000]);
}

/// Nothing went out: then, and only then, the error is the answer.
#[async_std::test]
async fn a_socket_with_no_room_at_all_is_eagain() {
    let f = file(&bytes(100));
    let sock = Bounded::new(0, 0, false);
    assert_eq!(
        copy_range(&f, None, &sock.as_file_like(), None, 100, false)
            .await
            .unwrap_err(),
        LxError::EAGAIN
    );
    assert_eq!(f.seek(SeekFrom::Current(0)).unwrap(), 0);
}

/// A blocking socket waits for room and carries on, as `write(2)` on it
/// does: the whole file goes out, in as many waits as it takes.
#[async_std::test]
async fn a_blocking_socket_waits_for_room_until_everything_is_out() {
    let f = file(&bytes(3000));
    let sock = Bounded::new(1000, 1000, false);
    let c = copy_range(&f, None, &sock.as_file_like(), None, 3000, true)
        .await
        .unwrap();
    assert_eq!(c.moved, 3000);
    assert_eq!(sock.taken.lock().as_slice(), &bytes(3000)[..]);
    assert_eq!(sock.waits.load(Ordering::SeqCst), 2);
    assert_eq!(f.seek(SeekFrom::Current(0)).unwrap(), 3000);
}

/// Without an explicit input offset the fd's own position moves by the
/// bytes delivered, so the next `sendfile` from the same fd resumes at
/// the first byte the socket did not take.
#[async_std::test]
async fn the_input_fd_position_ends_at_the_first_undelivered_byte() {
    let f = file(&bytes(3000));
    f.seek(SeekFrom::Start(500)).unwrap();
    let sock = Bounded::new(1000, 0, false);
    let c = copy_range(&f, None, &sock.as_file_like(), None, 3000, false)
        .await
        .unwrap();
    assert_eq!(c.moved, 1000);
    assert_eq!(f.seek(SeekFrom::Current(0)).unwrap(), 1500);
    assert_eq!(sock.taken.lock().as_slice(), &bytes(3000)[500..1500]);
}

/// A write that takes nothing ends the copy with the count so far. It
/// used to be reported as `EBADF`, with the bytes already out unreported.
#[async_std::test]
async fn a_write_of_nothing_ends_the_copy_with_what_went_out() {
    let f = file(&bytes(3000));
    let sock = Bounded::new(1000, 0, true);
    let c = copy_range(&f, Some(0), &sock.as_file_like(), None, 3000, true)
        .await
        .unwrap();
    assert_eq!(c.moved, 1000);
    assert_eq!(c.in_offset, Some(1000));
}

/// `copy_file_range` between two files with both offsets: the bytes land
/// at `off_out`, each offset advances by the count, and neither fd's own
/// position moves. The output used to be seeked to `off_out` and back.
#[async_std::test]
async fn explicit_offsets_advance_and_the_fd_positions_stay() {
    let src = file(&bytes(100));
    let dst = file(&[0u8; 64]);
    src.seek(SeekFrom::Start(7)).unwrap();
    dst.seek(SeekFrom::Start(5)).unwrap();
    let c = copy_range(&src, Some(10), &dst, Some(8), 20, false)
        .await
        .unwrap();
    assert_eq!(
        c,
        Copied {
            moved: 20,
            in_offset: Some(30),
            out_offset: Some(28)
        }
    );
    assert_eq!(src.seek(SeekFrom::Current(0)).unwrap(), 7);
    assert_eq!(dst.seek(SeekFrom::Current(0)).unwrap(), 5);
    let mut got = [0u8; 28];
    dst.read_at(0, &mut got).await.unwrap();
    assert_eq!(&got[..8], &[0u8; 8]);
    assert_eq!(&got[8..], &bytes(100)[10..30]);
}

/// Reading past the end stops the copy at the file's size, with the
/// count of what there was; a count of zero moves nothing.
#[async_std::test]
async fn the_copy_stops_at_the_end_of_the_input() {
    let src = file(&bytes(50));
    let dst = file(b"");
    let c = copy_range(&src, Some(30), &dst, None, 1000, false)
        .await
        .unwrap();
    assert_eq!(c.moved, 20);
    assert_eq!(c.in_offset, Some(50));
    let c = copy_range(&src, Some(0), &dst, None, 0, false)
        .await
        .unwrap();
    assert_eq!(c.moved, 0);
}

/// The wrong ends are `EBADF` before anything is read or written.
#[async_std::test]
async fn the_wrong_ends_are_ebadf() {
    let ro = File::new(
        RamFS::new()
            .root_inode()
            .create("r", FileType::File, 0o644)
            .unwrap(),
        OpenFlags::RDONLY,
        String::from("/r"),
    );
    let src = file(b"data");
    assert_eq!(
        copy_range(&src, Some(0), &(ro as Arc<dyn FileLike>), None, 4, false)
            .await
            .unwrap_err(),
        LxError::EBADF
    );
    let wo = File::new(
        RamFS::new()
            .root_inode()
            .create("w", FileType::File, 0o644)
            .unwrap(),
        OpenFlags::WRONLY,
        String::from("/w"),
    );
    let dst = file(b"");
    assert_eq!(
        copy_range(&(wo as Arc<dyn FileLike>), Some(0), &dst, None, 4, false)
            .await
            .unwrap_err(),
        LxError::EBADF
    );
}

/// The two `EBADF` checks are `copy_range`'s own, and on two `File`s the
/// mutation is invisible: a `File` polices its own access mode, so the read
/// or the write fails with the same `EBADF` a moment later. A socket does
/// not -- it takes whatever it is handed. So `sendfile` into a socket the
/// caller opened read-only copied the file straight into it.
#[async_std::test]
async fn a_socket_that_does_not_police_its_own_mode_is_still_ebadf() {
    let f = file(&bytes(64));
    let sock = Bounded::with_flags(1000, 0, false, OpenFlags::RDONLY);
    assert_eq!(
        copy_range(&f, Some(0), &sock.as_file_like(), None, 64, false)
            .await
            .unwrap_err(),
        LxError::EBADF
    );
    assert!(
        sock.taken.lock().is_empty(),
        "bytes reached a socket opened read-only"
    );
    // And the input end: a socket the caller opened write-only. Reading it
    // would answer `ENOSYS` here, which is not the answer `sendfile` owes.
    let src = Bounded::with_flags(0, 0, false, OpenFlags::WRONLY);
    let dst = file(b"");
    assert_eq!(
        copy_range(&src.as_file_like(), Some(0), &dst, None, 4, false)
            .await
            .unwrap_err(),
        LxError::EBADF
    );
}

/// An input that hands over at most `per_read` bytes at a time, the way a
/// pipe or a socket does.
///
/// Every test above it reads from a ramfs `File`, which always returns
/// everything it has: the loop goes round **once** and nothing that
/// depends on a second pass can be seen. That is one `File` hiding three
/// separate mutations.
struct Trickle {
    base: KObjectBase,
    content: Vec<u8>,
    per_read: usize,
}

impl_kobject!(Trickle);

impl Trickle {
    fn new(content: Vec<u8>, per_read: usize) -> Arc<dyn FileLike> {
        Arc::new(Trickle {
            base: KObjectBase::new(),
            content,
            per_read,
        })
    }
}

#[async_trait::async_trait]
impl FileLike for Trickle {
    fn flags(&self) -> OpenFlags {
        OpenFlags::RDWR
    }
    fn set_flags(&self, _: OpenFlags) -> LxResult {
        Ok(())
    }
    async fn read(&self, _: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }
    fn write(&self, _: &[u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        let start = offset as usize;
        if start >= self.content.len() {
            return Ok(0);
        }
        let n = self.per_read.min(buf.len()).min(self.content.len() - start);
        buf[..n].copy_from_slice(&self.content[start..start + n]);
        Ok(n)
    }
    fn poll(&self, _: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus::default())
    }
    async fn async_poll(&self, _: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus::default())
    }
}

/// `count` is a ceiling, not a hint. The clamp that enforces it only ever
/// decides on a second pass, so with a `File` on the input it was dead
/// code: dropping it let a `copy_file_range` for 20 bytes move 24.
#[async_std::test]
async fn a_count_is_a_ceiling_even_when_the_input_dribbles() {
    let src = Trickle::new(bytes(40), 8);
    let dst = file(b"");
    let c = copy_range(&src, Some(0), &dst, Some(0), 20, false)
        .await
        .unwrap();
    assert_eq!(c.moved, 20, "the copy ran past its count");
    assert_eq!(c.in_offset, Some(20));
    assert_eq!(c.out_offset, Some(20));
}

/// Each read starts where the last one ended, and each write lands after
/// the bytes already delivered. Get either wrong and the output is the head
/// of the input over and over -- silently, with the right byte count.
#[async_std::test]
async fn the_copy_walks_forward_on_both_ends_across_reads() {
    let src = Trickle::new(bytes(24), 8);
    let dst = file(b"");
    let c = copy_range(&src, Some(0), &dst, Some(0), 24, false)
        .await
        .unwrap();
    assert_eq!(c.moved, 24);
    let mut out = alloc::vec![0u8; 24];
    assert_eq!(dst.read_at(0, &mut out).await.unwrap(), 24);
    assert_eq!(out, bytes(24), "the output is not the input, in order");
}
