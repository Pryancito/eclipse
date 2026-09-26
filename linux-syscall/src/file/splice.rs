//! `splice(2)` / `tee(2)` / `vmsplice(2)` — pipe-centric data movement.
//!
//! Linux implements these zero-copy by handing pipe-buffer page references
//! around. Here the pipe buffer is a plain byte queue, so the calls move the
//! bytes through a bounded kernel buffer instead: the observable semantics
//! (what lands where, offset handling, the error protocol from the man pages)
//! are preserved, the zero-copy part is not. `coreutils`, `pv`, systemd's
//! journal and busybox all fall back gracefully on kernels without splice, but
//! only after a probing call — answering it for real is one less EINVAL path.

use super::*;
use linux_object::error::LxResult;

const SPLICE_F_MOVE: usize = 1;
const SPLICE_F_NONBLOCK: usize = 2;
const SPLICE_F_MORE: usize = 4;
const SPLICE_F_GIFT: usize = 8;
const SPLICE_FLAGS_ALL: usize = SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT;

/// The inode behind a file-like when it is a pipe. Pipes here are always a
/// `File` wrapping a `Pipe` inode (see `sys_pipe2`); `dyn INode` only offers
/// `downcast_ref`, so the caller keeps the returned `Arc` alive and borrows
/// the `Pipe` out of it at the use site.
pub(super) fn pipe_inode(f: &Arc<dyn FileLike>) -> Option<Arc<dyn rcore_fs::vfs::INode>> {
    let file = f.downcast_ref::<File>()?;
    let inode = file.inode();
    inode.downcast_ref::<Pipe>().is_some().then_some(inode)
}

/// What `splice_bytes` moved and where the two sides now stand: the explicit
/// offsets advanced by the bytes that actually reached the output, `None`
/// where the caller gave none (that side used and moved its own position).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Spliced {
    pub moved: usize,
    pub off_in: Option<i64>,
    pub off_out: Option<i64>,
}

/// Deliver `data` to the output, at `off_out` when one is given (the fd's own
/// position does not move) or at the fd's position otherwise.
fn deliver(
    file_out: &Arc<dyn FileLike>,
    off_out: Option<i64>,
    data: &[u8],
) -> LxResult<(usize, Option<i64>)> {
    match off_out {
        Some(off) => {
            let written = file_out.write_at(off as u64, data)?;
            Ok((written, Some(off + written as i64)))
        }
        None => Ok((file_out.write(data)?, None)),
    }
}

/// Move up to `len` bytes from `file_in` to `file_out`, at least one of them
/// a pipe, the way `do_splice` does: **nothing leaves the input that does not
/// reach the output.**
///
/// The old shape read `len` bytes out of the input first and wrote them
/// afterwards, so whatever the output pipe could not take -- all of it when
/// full, the tail when short of room -- was gone: consumed from the input
/// pipe, or skipped in the input file because `*off_in` had already advanced
/// by the bytes read rather than the bytes delivered. And a full output pipe
/// answered `EAGAIN` to a blocking caller, where `write(2)` on the same fd
/// waits.
///
/// Now an output pipe is asked for its room first (waiting for some, or
/// `EAGAIN` under `nonblock`, `EPIPE` with no reader left), the read is capped
/// to it, an input pipe is only peeked and then `consume`d by the delivered
/// count, and an input file's position -- `*off_in` or the fd's own -- ends at
/// the byte after the last one delivered.
pub(super) async fn splice_bytes(
    file_in: &Arc<dyn FileLike>,
    off_in: Option<i64>,
    file_out: &Arc<dyn FileLike>,
    off_out: Option<i64>,
    len: usize,
    nonblock: bool,
) -> LxResult<Spliced> {
    let done = |moved, off_in, off_out| {
        Ok(Spliced {
            moved,
            off_in,
            off_out,
        })
    };
    if !file_in.flags().readable() || !file_out.flags().writable() {
        return Err(LxError::EBADF);
    }
    if len == 0 {
        return done(0, off_in, off_out);
    }
    let mut len = len.min(super::SYSCALL_IO_MAX);

    // The output pipe's room bounds what may leave the input.
    let inode_out = pipe_inode(file_out);
    if let Some(inode_out) = &inode_out {
        let pipe_out = inode_out.downcast_ref::<Pipe>().ok_or(LxError::EINVAL)?;
        loop {
            match pipe_out.write_room() {
                None => return Err(LxError::EPIPE),
                Some(0) => {
                    if nonblock || file_out.flags().non_block() {
                        return Err(LxError::EAGAIN);
                    }
                    file_out.async_poll(PollEvents::OUT).await?;
                }
                Some(room) => {
                    len = len.min(room);
                    break;
                }
            }
        }
    }

    let inode_in = pipe_inode(file_in);
    if let Some(inode_in) = &inode_in {
        let pipe_in = inode_in.downcast_ref::<Pipe>().ok_or(LxError::EINVAL)?;
        let data = loop {
            let (data, writers_alive) = pipe_in.peek_data(len).ok_or(LxError::EBADF)?;
            if !data.is_empty() {
                break data;
            }
            if !writers_alive {
                return done(0, off_in, off_out);
            }
            if nonblock || file_in.flags().non_block() {
                return Err(LxError::EAGAIN);
            }
            file_in.async_poll(PollEvents::IN).await?;
        };
        let (written, off_out) = deliver(file_out, off_out, &data)?;
        pipe_in.consume(written);
        return done(written, off_in, off_out);
    }

    let mut buf = crate::try_zeroed_buf(len)?;
    let n = match off_in {
        Some(off) => file_in.read_at(off as u64, &mut buf).await?,
        None => file_in.read(&mut buf).await?,
    };
    if n == 0 {
        return done(0, off_in, off_out);
    }
    let delivered = deliver(file_out, off_out, &buf[..n]);
    let written = delivered.as_ref().map(|(w, _)| *w).unwrap_or(0);
    // The fd's own position moved by the bytes read; put it back on the byte
    // after the last one delivered.
    if off_in.is_none() && written < n {
        file_in.seek(SeekFrom::Current(-((n - written) as i64)))?;
    }
    let (written, off_out) = delivered?;
    done(written, off_in.map(|off| off + written as i64), off_out)
}

impl Syscall<'_> {
    /// splice data to/from a pipe
    /// (see [linux man splice(2)](https://www.man7.org/linux/man-pages/man2/splice.2.html)).
    ///
    /// Moves up to `len` bytes from `fd_in` to `fd_out`, where at least one of
    /// the two must be a pipe (`EINVAL` otherwise) and an offset may only be
    /// given for the non-pipe side (`ESPIPE` otherwise), exactly the man-page
    /// contract. `SPLICE_F_NONBLOCK` makes the pipe side answer `EAGAIN`
    /// instead of waiting; MOVE/MORE/GIFT are hints and accepted as such.
    pub async fn sys_splice(
        &self,
        fd_in: FileDesc,
        off_in: UserInOutPtr<i64>,
        fd_out: FileDesc,
        off_out: UserInOutPtr<i64>,
        len: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "splice: fd_in={:?}, off_in={:?}, fd_out={:?}, off_out={:?}, len={}, flags={:#x}",
            fd_in, off_in, fd_out, off_out, len, flags
        );
        if flags & !SPLICE_FLAGS_ALL != 0 {
            return Err(LxError::EINVAL);
        }
        let proc = self.linux_process();
        let file_in = proc.get_file_like(fd_in)?;
        let file_out = proc.get_file_like(fd_out)?;
        let in_is_pipe = pipe_inode(&file_in).is_some();
        let out_is_pipe = pipe_inode(&file_out).is_some();
        if !in_is_pipe && !out_is_pipe {
            return Err(LxError::EINVAL);
        }
        if (in_is_pipe && !off_in.is_null()) || (out_is_pipe && !off_out.is_null()) {
            return Err(LxError::ESPIPE);
        }
        let read_offset = |p: &UserInOutPtr<i64>| -> LxResult<Option<i64>> {
            if p.is_null() {
                return Ok(None);
            }
            let off = p.read()?;
            if off < 0 {
                return Err(LxError::EINVAL);
            }
            Ok(Some(off))
        };
        let in_off = read_offset(&off_in)?;
        let out_off = read_offset(&off_out)?;

        let spliced = splice_bytes(
            &file_in,
            in_off,
            &file_out,
            out_off,
            len,
            flags & SPLICE_F_NONBLOCK != 0,
        )
        .await
        .inspect_err(|&e| self.raise_sigpipe_if_due(e, out_is_pipe))?;
        if let Some(off) = spliced.off_in {
            let mut off_in = off_in;
            off_in.write(off)?;
        }
        if let Some(off) = spliced.off_out {
            let mut off_out = off_out;
            off_out.write(off)?;
        }
        Ok(spliced.moved)
    }

    /// duplicate pipe content
    /// (see [linux man tee(2)](https://www.man7.org/linux/man-pages/man2/tee.2.html)).
    ///
    /// Copies up to `len` bytes from the read end `fd_in` to the write end
    /// `fd_out` **without consuming** the input — the defining property of
    /// `tee`, served by `Pipe::peek_data`. Both fds must be pipes (`EINVAL`).
    /// An empty input pipe blocks until data or writer hang-up unless
    /// `SPLICE_F_NONBLOCK` asks for `EAGAIN`.
    pub async fn sys_tee(
        &self,
        fd_in: FileDesc,
        fd_out: FileDesc,
        len: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "tee: fd_in={:?}, fd_out={:?}, len={}, flags={:#x}",
            fd_in, fd_out, len, flags
        );
        if flags & !SPLICE_FLAGS_ALL != 0 {
            return Err(LxError::EINVAL);
        }
        let proc = self.linux_process();
        let file_in = proc.get_file_like(fd_in)?;
        let file_out = proc.get_file_like(fd_out)?;
        let inode_in = pipe_inode(&file_in).ok_or(LxError::EINVAL)?;
        let inode_out = pipe_inode(&file_out).ok_or(LxError::EINVAL)?;
        let pipe_in = inode_in.downcast_ref::<Pipe>().ok_or(LxError::EINVAL)?;
        let pipe_out = inode_out.downcast_ref::<Pipe>().ok_or(LxError::EINVAL)?;
        if !pipe_in.is_read_end() || pipe_out.is_read_end() {
            return Err(LxError::EBADF);
        }
        if len == 0 {
            return Ok(0);
        }
        let len = len.min(super::SYSCALL_IO_MAX);
        loop {
            let (data, writers_alive) = pipe_in.peek_data(len).ok_or(LxError::EBADF)?;
            if !data.is_empty() {
                return file_out.write(&data);
            }
            if !writers_alive {
                return Ok(0);
            }
            if flags & SPLICE_F_NONBLOCK != 0 {
                return Err(LxError::EAGAIN);
            }
            // Wait for data or writer hang-up, then re-check.
            file_in.async_poll(PollEvents::IN).await?;
        }
    }

    /// splice user pages to/from a pipe
    /// (see [linux man vmsplice(2)](https://www.man7.org/linux/man-pages/man2/vmsplice.2.html)).
    ///
    /// On the write end the iovecs are gathered into the pipe; on the read end
    /// the pipe is scattered into the iovecs. Without page-reference pipe
    /// buffers this is exactly `writev`/`readv` on the pipe fd — including for
    /// `SPLICE_F_GIFT`, which is only ever an optimisation hint.
    pub async fn sys_vmsplice(
        &self,
        fd: FileDesc,
        iov_ptr: usize,
        iov_count: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "vmsplice: fd={:?}, iov={:#x}, count={}, flags={:#x}",
            fd, iov_ptr, iov_count, flags
        );
        if flags & !SPLICE_FLAGS_ALL != 0 {
            return Err(LxError::EINVAL);
        }
        let proc = self.linux_process();
        let file = proc.get_file_like(fd)?;
        let inode = pipe_inode(&file).ok_or(LxError::EBADF)?;
        let is_read_end = inode
            .downcast_ref::<Pipe>()
            .map(Pipe::is_read_end)
            .unwrap_or(false);
        if is_read_end {
            self.sys_readv(fd, iov_ptr.into(), iov_count).await
        } else {
            self.sys_writev(fd, iov_ptr.into(), iov_count).await
        }
    }
}

#[cfg(test)]
mod tests {
    //! `splice(2)` on real pipes and a ramfs file. Every test here names a
    //! byte that the old shape lost: it read the input first and found out
    //! afterwards how much of it the output could take.

    use super::{splice_bytes, Spliced};
    use alloc::string::String;
    use alloc::sync::Arc;
    use linux_object::error::{LxError, LxResult};
    use linux_object::fs::vfs::{FileType, FsError, INode, PollStatus};
    use linux_object::fs::{
        File, FileLike, OpenFlags, Pipe, SeekFrom, PIPE_BUF, PIPE_DEFAULT_CAPACITY,
    };
    use rcore_fs::vfs::FileSystem;
    use rcore_fs_ramfs::RamFS;

    fn pipe(flags: OpenFlags) -> (Arc<dyn FileLike>, Arc<dyn FileLike>, Arc<Pipe>) {
        let (r, w) = Pipe::create_pair();
        let (r, w) = (Arc::new(r), Arc::new(w));
        let rf = File::new(
            r.clone(),
            flags | OpenFlags::RDONLY,
            String::from("pipe_r:[]"),
        );
        let wf = File::new(w, flags | OpenFlags::WRONLY, String::from("pipe_w:[]"));
        (rf, wf, r)
    }

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

    fn bytes(n: usize) -> alloc::vec::Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    fn fill(w: &Arc<dyn FileLike>, n: usize) {
        assert_eq!(w.write(&alloc::vec![0xeeu8; n]).unwrap(), n);
    }

    fn queued(r: &Arc<Pipe>) -> alloc::vec::Vec<u8> {
        r.peek_data(usize::MAX).unwrap().0
    }

    async fn splice(
        file_in: &Arc<dyn FileLike>,
        off_in: Option<i64>,
        file_out: &Arc<dyn FileLike>,
        off_out: Option<i64>,
        len: usize,
        nonblock: bool,
    ) -> LxResult<Spliced> {
        splice_bytes(file_in, off_in, file_out, off_out, len, nonblock).await
    }

    /// Pipe to full pipe, non-blocking: `EAGAIN`, and the input still holds
    /// every byte. It used to hold none: the read had already consumed them
    /// when the write came back `EAGAIN`.
    #[async_std::test]
    async fn a_full_output_pipe_takes_nothing_out_of_the_input_pipe() {
        let (a_r, a_w, a) = pipe(OpenFlags::empty());
        let (_b_r, b_w, _) = pipe(OpenFlags::empty());
        a_w.write(b"ten bytes!").unwrap();
        fill(&b_w, PIPE_DEFAULT_CAPACITY);
        assert_eq!(
            splice(&a_r, None, &b_w, None, 10, true).await.unwrap_err(),
            LxError::EAGAIN
        );
        assert_eq!(queued(&a), b"ten bytes!", "nothing was consumed");
    }

    /// Pipe to a pipe with less room than asked: only what fits moves, and
    /// the input keeps the rest, in order. The old shape consumed all `len`
    /// and delivered the head. The request is under `PIPE_BUF`, where a
    /// `write(2)` would be all-or-nothing: splice moves pipe buffers, not
    /// atomic records, so it takes the room there is.
    #[async_std::test]
    async fn only_what_the_output_pipe_can_take_leaves_the_input_pipe() {
        let (a_r, a_w, a) = pipe(OpenFlags::empty());
        let (b_r, b_w, b) = pipe(OpenFlags::empty());
        let payload = bytes(3000);
        assert!(payload.len() <= PIPE_BUF);
        a_w.write(&payload).unwrap();
        fill(&b_w, PIPE_DEFAULT_CAPACITY - 100);
        let s = splice(&a_r, None, &b_w, None, 3000, false).await.unwrap();
        assert_eq!(
            s,
            Spliced {
                moved: 100,
                off_in: None,
                off_out: None
            }
        );
        assert_eq!(queued(&a), &payload[100..], "the tail is still there");
        let mut sink = alloc::vec![0u8; PIPE_DEFAULT_CAPACITY];
        b_r.read(&mut sink).await.unwrap();
        assert_eq!(&sink[PIPE_DEFAULT_CAPACITY - 100..], &payload[..100]);
        drop(b);
    }

    /// File to a pipe short of room, with an explicit offset: `*off_in`
    /// advances by the bytes delivered, not by the bytes read. It used to
    /// jump by the read, so the next splice skipped the undelivered tail.
    #[async_std::test]
    async fn off_in_advances_by_what_was_delivered_not_by_what_was_read() {
        let f = file(&bytes(5000));
        let (_b_r, b_w, b) = pipe(OpenFlags::empty());
        fill(&b_w, PIPE_DEFAULT_CAPACITY - 100);
        let s = splice(&f, Some(0), &b_w, None, 5000, false).await.unwrap();
        assert_eq!(s.moved, 100);
        assert_eq!(s.off_in, Some(100), "the offset ends after byte 99");
        assert_eq!(
            f.seek(SeekFrom::Current(0)).unwrap(),
            0,
            "the fd's own position is not touched with an explicit offset"
        );
        let q = queued(&b);
        assert_eq!(&q[q.len() - 100..], &bytes(5000)[..100]);
    }

    /// The same without an offset: the fd's own position, which the read
    /// moved by `n`, is put back on the first undelivered byte, so `cat`
    /// resuming with `read(2)` sees the tail exactly once.
    #[async_std::test]
    async fn the_fd_position_of_an_input_file_ends_at_the_first_undelivered_byte() {
        let f = file(&bytes(5000));
        let (_b_r, b_w, _) = pipe(OpenFlags::empty());
        fill(&b_w, PIPE_DEFAULT_CAPACITY - 100);
        let s = splice(&f, None, &b_w, None, 5000, false).await.unwrap();
        assert_eq!(s.moved, 100);
        assert_eq!(f.seek(SeekFrom::Current(0)).unwrap(), 100);
    }

    /// An output pipe with no reader is `EPIPE` before anything is read, as
    /// `pipe_write` answers; the input file's position does not move. The
    /// pipe is also full: a blocking caller must get the error, not a wait
    /// for room that no reader will ever make.
    #[async_std::test]
    async fn an_output_pipe_with_no_reader_is_epipe_and_consumes_nothing() {
        let f = file(&bytes(100));
        let (b_r, b_w, _) = pipe(OpenFlags::empty());
        fill(&b_w, PIPE_DEFAULT_CAPACITY);
        drop(b_r);
        assert_eq!(
            splice(&f, None, &b_w, None, 100, false).await.unwrap_err(),
            LxError::EPIPE
        );
        assert_eq!(f.seek(SeekFrom::Current(0)).unwrap(), 0);
    }

    /// A blocking splice into a full pipe waits for room, as `write(2)` on
    /// that fd does, and then moves what fits. It used to answer `EAGAIN`,
    /// which a blocking caller does not handle, having lost the input.
    #[async_std::test]
    async fn a_blocking_splice_waits_for_room_in_a_full_output_pipe() {
        let (a_r, a_w, a) = pipe(OpenFlags::empty());
        let (b_r, b_w, _) = pipe(OpenFlags::empty());
        a_w.write(&bytes(PIPE_BUF)).unwrap();
        fill(&b_w, PIPE_DEFAULT_CAPACITY);
        // The reader stays open past its read: dropping it would turn the
        // room it made into "no reader left".
        let drain = async_std::task::spawn(async move {
            async_std::task::sleep(core::time::Duration::from_millis(100)).await;
            let mut sink = alloc::vec![0u8; PIPE_BUF];
            (b_r.read(&mut sink).await.unwrap(), b_r)
        });
        let s = splice(&a_r, None, &b_w, None, PIPE_BUF, false)
            .await
            .unwrap();
        assert_eq!(drain.await.0, PIPE_BUF);
        assert_eq!(s.moved, PIPE_BUF);
        assert!(
            queued(&a).is_empty(),
            "delivered in full, so consumed in full"
        );
    }

    /// `O_NONBLOCK` on the output fd counts like `SPLICE_F_NONBLOCK`.
    #[async_std::test]
    async fn a_non_blocking_output_fd_answers_eagain_when_full() {
        let (a_r, a_w, _) = pipe(OpenFlags::empty());
        let (_b_r, b_w, _) = pipe(OpenFlags::NON_BLOCK);
        a_w.write(b"x").unwrap();
        fill(&b_w, PIPE_DEFAULT_CAPACITY);
        assert_eq!(
            splice(&a_r, None, &b_w, None, 1, false).await.unwrap_err(),
            LxError::EAGAIN
        );
    }

    /// An empty input pipe: `EAGAIN` while a writer lives and the call must
    /// not wait, end of stream (0) once the writers are gone.
    #[async_std::test]
    async fn an_empty_input_pipe_is_eagain_with_a_writer_and_eof_without() {
        let (a_r, a_w, _) = pipe(OpenFlags::empty());
        let f = file(b"");
        assert_eq!(
            splice(&a_r, None, &f, Some(0), 10, true).await.unwrap_err(),
            LxError::EAGAIN
        );
        drop(a_w);
        let s = splice(&a_r, None, &f, Some(0), 10, false).await.unwrap();
        assert_eq!(s.moved, 0);
        assert_eq!(s.off_out, Some(0), "an offset that moved nothing stays");
    }

    /// Pipe to file with `off_out`: the bytes land at the offset, `*off_out`
    /// advances by them, the fd's own position stays, and the pipe is drained
    /// by exactly that many.
    #[async_std::test]
    async fn pipe_to_file_writes_at_off_out_and_drains_the_pipe_by_what_landed() {
        let (a_r, a_w, a) = pipe(OpenFlags::empty());
        a_w.write(b"hello world").unwrap();
        let f = file(&[0u8; 32]);
        let s = splice(&a_r, None, &f, Some(8), 5, false).await.unwrap();
        assert_eq!(
            s,
            Spliced {
                moved: 5,
                off_in: None,
                off_out: Some(13)
            }
        );
        assert_eq!(queued(&a), b" world");
        assert_eq!(f.seek(SeekFrom::Current(0)).unwrap(), 0);
        let mut got = [0u8; 13];
        f.read_at(0, &mut got).await.unwrap();
        assert_eq!(&got, b"\0\0\0\0\0\0\0\0hello");
    }

    /// A device that takes at most three bytes per write, like a tty or a
    /// socket whose buffer is nearly full.
    struct ShortSink(lock::Mutex<alloc::vec::Vec<u8>>);

    impl INode for ShortSink {
        fn read_at(&self, _: usize, _: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            Ok(0)
        }
        fn write_at(&self, _: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
            let take = buf.len().min(3);
            self.0.lock().extend_from_slice(&buf[..take]);
            Ok(take)
        }
        fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
            Ok(PollStatus {
                read: false,
                write: true,
                error: false,
                hangup: false,
            })
        }
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
    }

    /// Pipe to a device that takes a short write: the pipe is drained by
    /// the three bytes that landed, not by the twelve that were peeked.
    #[async_std::test]
    async fn a_short_write_on_the_output_drains_the_pipe_by_what_landed() {
        let (a_r, a_w, a) = pipe(OpenFlags::empty());
        a_w.write(b"twelve bytes").unwrap();
        let sink = Arc::new(ShortSink(lock::Mutex::new(alloc::vec::Vec::new())));
        let out: Arc<dyn FileLike> =
            File::new(sink.clone(), OpenFlags::WRONLY, String::from("/dev/short"));
        let s = splice(&a_r, None, &out, None, 12, false).await.unwrap();
        assert_eq!(s.moved, 3);
        assert_eq!(sink.0.lock().as_slice(), b"twe");
        assert_eq!(
            queued(&a),
            b"lve bytes",
            "nine bytes wait for the next call"
        );
    }

    /// A file whose `read_at` fills the output pipe first: the writer that
    /// races `splice` between its room check and its write, made
    /// deterministic. The room it saw is gone when it writes.
    struct RacingSource {
        content: alloc::vec::Vec<u8>,
        rival: Arc<dyn FileLike>,
        steal: usize,
    }

    impl INode for RacingSource {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            self.rival
                .write(&alloc::vec![0xffu8; self.steal])
                .expect("the rival's write fits");
            let rest = &self.content[offset.min(self.content.len())..];
            let n = rest.len().min(buf.len());
            buf[..n].copy_from_slice(&rest[..n]);
            Ok(n)
        }
        fn write_at(&self, _: usize, _: &[u8]) -> rcore_fs::vfs::Result<usize> {
            Err(FsError::NotSupported)
        }
        fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
            Ok(PollStatus {
                read: true,
                write: false,
                error: false,
                hangup: false,
            })
        }
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
    }

    fn racing_file(content: &[u8], rival: Arc<dyn FileLike>, steal: usize) -> Arc<dyn FileLike> {
        let inode = Arc::new(RacingSource {
            content: content.to_vec(),
            rival,
            steal,
        });
        File::new(inode, OpenFlags::RDONLY, String::from("/racing"))
    }

    /// File to pipe, with another writer taking most of the room between the
    /// room check and the write: `*off_in` still advances by the bytes that
    /// landed, which is fewer than were read.
    #[async_std::test]
    async fn off_in_follows_the_delivered_bytes_when_a_rival_takes_the_room() {
        let (_b_r, b_w, b) = pipe(OpenFlags::empty());
        fill(&b_w, PIPE_DEFAULT_CAPACITY - PIPE_BUF - 100);
        // Room seen: PIPE_BUF + 100. The rival takes PIPE_BUF, leaving 100
        // for the PIPE_BUF + 100 bytes read, more than a PIPE_BUF so the
        // write takes what fits.
        let f = racing_file(&bytes(PIPE_BUF + 100), b_w.clone(), PIPE_BUF);
        let s = splice(&f, Some(0), &b_w, None, PIPE_BUF + 100, false)
            .await
            .unwrap();
        assert_eq!(s.moved, 100);
        assert_eq!(s.off_in, Some(100));
        let q = queued(&b);
        assert_eq!(&q[q.len() - 100..], &bytes(100)[..]);
    }

    /// The same race without an offset: the fd's position, which the read
    /// carried past every byte read, ends on the first byte not delivered.
    #[async_std::test]
    async fn the_fd_position_follows_the_delivered_bytes_when_a_rival_takes_the_room() {
        let (_b_r, b_w, _) = pipe(OpenFlags::empty());
        fill(&b_w, PIPE_DEFAULT_CAPACITY - PIPE_BUF - 100);
        let f = racing_file(&bytes(PIPE_BUF + 100), b_w.clone(), PIPE_BUF);
        let s = splice(&f, None, &b_w, None, PIPE_BUF + 100, false)
            .await
            .unwrap();
        assert_eq!(s.moved, 100);
        assert_eq!(f.seek(SeekFrom::Current(0)).unwrap(), 100);
    }

    /// `fd_in` must be open for reading and `fd_out` for writing (`EBADF`),
    /// checked before any waiting: a full pipe's read end given as the
    /// output would otherwise be waited on for room that never comes.
    #[async_std::test]
    async fn the_wrong_ends_are_ebadf_before_anything_waits() {
        let (a_r, a_w, _) = pipe(OpenFlags::empty());
        fill(&a_w, PIPE_DEFAULT_CAPACITY);
        let f = file(b"data");
        assert_eq!(
            splice(&a_w, None, &f, Some(0), 4, false).await.unwrap_err(),
            LxError::EBADF
        );
        assert_eq!(
            splice(&f, Some(0), &a_r, None, 4, false).await.unwrap_err(),
            LxError::EBADF
        );
    }
}
