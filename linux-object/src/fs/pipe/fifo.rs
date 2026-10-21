//! Named pipes, `fifo(7)`: a FIFO node stands for one pipe, every open of
//! the node is an end of it, and it lives while any end is open and goes,
//! with whatever it still held, with the last one (`fifo_open`).
//!
//! Opening a FIFO used to open its node as a file: what one process wrote
//! went into the node and the next reader read it from the start, a reader
//! saw end-of-file with its writer still writing, an `open` never waited for
//! the other end, and a writer never learnt that its reader had gone.

use super::*;
use crate::error::{LxError, LxResult};
use crate::fs::record_lock::FileKey;
use crate::fs::OpenFlags;
use alloc::{collections::BTreeMap, sync::Weak};
use lazy_static::lazy_static;

lazy_static! {
    /// The pipe of every FIFO with an end open, by the `(dev, inode)` of
    /// its node, the identity the record locks key a file by.
    static ref FIFOS: Mutex<BTreeMap<FileKey, Weak<Mutex<PipeData>>>> =
        Mutex::new(BTreeMap::new());
}

/// The other side a FIFO opener has to wait for, because nobody had it
/// open: how many times it had been opened then (see [`Pipe::partner`]).
pub struct FifoPartner {
    seen: u64,
}

impl Pipe {
    /// `fifo_open`: an end of the pipe `node` stands for, for reading,
    /// writing or both as `flags` say, made if no end of it is open; and,
    /// for a blocking open with nobody at the other end, that other end,
    /// which the open must wait for ([`Pipe::partner`]).
    ///
    /// `ENXIO` for a write-only `O_NONBLOCK` open with no reader. A reader
    /// opened `O_NONBLOCK` with no writer reads end-of-file, but has not
    /// hung up until a writer has come and gone.
    pub fn open_fifo(
        node: Arc<dyn INode>,
        flags: OpenFlags,
    ) -> LxResult<(Pipe, Option<FifoPartner>)> {
        let meta = node.metadata()?;
        let direction = match (flags.readable(), flags.writable()) {
            (true, true) => PipeEnd::Both,
            (false, true) => PipeEnd::Write,
            _ => PipeEnd::Read,
        };
        let data = {
            let mut fifos = FIFOS.lock();
            // A pipe goes with its last end, and its entry with the next
            // open of any FIFO.
            fifos.retain(|_, pipe| pipe.strong_count() > 0);
            let key = (meta.dev, meta.inode);
            match fifos.get(&key).and_then(Weak::upgrade) {
                Some(data) => data,
                None => {
                    let data = Arc::new(Mutex::new(PipeData::new(0)));
                    fifos.insert(key, Arc::downgrade(&data));
                    data
                }
            }
        };
        let mut d = data.lock();
        if direction == PipeEnd::Write && flags.non_block() && d.read_cnt == 0 {
            return Err(LxError::ENXIO);
        }
        let mut pipe = Pipe {
            data: data.clone(),
            direction,
            ino: meta.inode,
            uid: meta.uid,
            gid: meta.gid,
            node: Some(node),
            hup_after: 0,
        };
        if pipe.reads() {
            d.read_cnt += 1;
            d.r_counter += 1;
        }
        if pipe.writes() {
            d.write_cnt += 1;
            d.w_counter += 1;
        }
        // A side that was gone is back, or the end that waits for it has not
        // seen it go. And `CLOSED` is latched: a subscription fires at once
        // on it, so a reader parked until a writer comes would be woken by
        // a writer that went before it opened, over and over.
        d.eventbus.clear(Event::CLOSED);
        for waker in d.partner_waiters.drain(..) {
            waker.wake();
        }
        let partner = match pipe.direction {
            PipeEnd::Read if d.write_cnt == 0 => Some(d.w_counter),
            PipeEnd::Write if d.read_cnt == 0 => Some(d.r_counter),
            _ => None,
        };
        let partner = match partner {
            Some(seen) if !flags.non_block() => Some(FifoPartner { seen }),
            Some(seen) => {
                pipe.hup_after = seen;
                None
            }
            None => None,
        };
        drop(d);
        Ok((pipe, partner))
    }

    /// `wait_for_partner`: done once the other side has been opened since
    /// `partner` was taken, even if it has been closed again.
    ///
    /// Not interruptible by itself: the open that waits wraps it in
    /// `process::interruptible`, and dropping it leaves its waker behind
    /// for the next open of the FIFO to wake and drop.
    pub fn partner(&self, partner: FifoPartner) -> impl Future<Output = ()> + '_ {
        core::future::poll_fn(move |cx| {
            let mut data = self.data.lock();
            let opened = if self.reads() {
                data.w_counter
            } else {
                data.r_counter
            };
            if opened != partner.seen {
                return Poll::Ready(());
            }
            if !data.partner_waiters.iter().any(|w| w.will_wake(cx.waker())) {
                data.partner_waiters.push(cx.waker().clone());
            }
            Poll::Pending
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::task::Wake;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use rcore_fs::vfs::FileSystem;
    use rcore_fs_ramfs::RamFS;

    const RD: OpenFlags = OpenFlags::RDONLY;
    const WR: OpenFlags = OpenFlags::WRONLY;
    const RW: OpenFlags = OpenFlags::RDWR;

    fn a_fifo() -> Arc<dyn INode> {
        let fs = RamFS::new();
        let node = fs
            .root_inode()
            .create("fifo", FileType::NamedPipe, 0o640)
            .unwrap();
        // The node holds its file system by a `Weak`.
        core::mem::forget(fs);
        node
    }

    fn open(node: &Arc<dyn INode>, flags: OpenFlags) -> LxResult<(Pipe, Option<FifoPartner>)> {
        Pipe::open_fifo(node.clone(), flags)
    }

    /// An end opened `O_NONBLOCK`, which never waits.
    fn now(node: &Arc<dyn INode>, flags: OpenFlags) -> Pipe {
        let (end, partner) = open(node, flags | OpenFlags::NON_BLOCK).unwrap();
        assert!(partner.is_none());
        end
    }

    fn read(end: &Pipe, len: usize) -> Result<alloc::vec::Vec<u8>> {
        let mut buf = alloc::vec![0u8; len];
        let n = end.read_at(0, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting() -> (Arc<Count>, Waker) {
        let count = Arc::new(Count(AtomicUsize::new(0)));
        (count.clone(), Waker::from(count))
    }

    fn woken(count: &Count) -> usize {
        count.0.load(Ordering::SeqCst)
    }

    /// Every open is an end of one pipe: two writers' bytes come out of one
    /// reader in order, the reader waits while a writer is left, and reads
    /// end-of-file once none is.
    #[test]
    fn every_open_of_a_fifo_is_the_same_pipe() {
        let node = a_fifo();
        let r = now(&node, RD);
        let (w1, w2) = (now(&node, WR), now(&node, WR));
        assert_eq!(w1.write_at(0, b"1"), Ok(1));
        assert_eq!(w2.write_at(0, b"2"), Ok(1));
        drop(w1);
        assert_eq!(read(&r, 4).unwrap(), b"12");
        assert_eq!(read(&r, 1), Err(FsError::Again), "a writer is left");
        drop(w2);
        assert_eq!(read(&r, 1).unwrap(), b"", "every writer gone");
    }

    /// What was written is in the pipe and goes with its last end: the node
    /// holds nothing, and the next reader finds nothing.
    #[test]
    fn what_was_written_goes_with_the_last_end() {
        let node = a_fifo();
        let (r, w) = (now(&node, RD), now(&node, WR));
        assert_eq!(w.write_at(0, b"lost"), Ok(4));
        assert_eq!(node.metadata().unwrap().size, 0);
        drop((w, r));
        let r = now(&node, RD);
        assert_eq!(read(&r, 8).unwrap(), b"");
    }

    /// `ENXIO` for a writer that will not wait and has no reader, and it
    /// leaves no writer behind: the reader that comes next has not hung up.
    #[test]
    fn a_write_only_open_with_no_reader_is_enxio() {
        let node = a_fifo();
        let got = open(&node, WR | OpenFlags::NON_BLOCK).map(|_| ());
        assert_eq!(got, Err(LxError::ENXIO));
        let r = now(&node, RD);
        assert!(!r.poll().unwrap().hangup);
        let w = now(&node, WR);
        assert_eq!(w.write_at(0, b"z"), Ok(1));
        assert_eq!(read(&r, 1).unwrap(), b"z");
    }

    /// `f_pipe`: a reader opened with no writer reads end-of-file, but has
    /// not hung up -- a `poll` for a writer must not return at once -- until
    /// one has come and gone.
    #[test]
    fn a_reader_with_no_writer_yet_has_not_hung_up() {
        let node = a_fifo();
        let r = now(&node, RD);
        assert_eq!(read(&r, 1).unwrap(), b"");
        let s = r.poll().unwrap();
        assert!(!s.read && !s.hangup);
        let w = now(&node, WR);
        assert_eq!(read(&r, 1), Err(FsError::Again));
        drop(w);
        let s = r.poll().unwrap();
        assert!(s.read && s.hangup);
    }

    /// A reader parked for a writer is not woken by one that went before it
    /// opened, nor by another reader's close: `CLOSED` is latched, and a
    /// latched flag wakes every subscription at once. The writer that comes
    /// and goes after it does wake it. The first reader keeps the pipe, and
    /// the `CLOSED` its writer left, alive throughout.
    #[test]
    fn a_reader_parked_for_a_writer_wakes_for_its_writer_only() {
        let node = a_fifo();
        let _first = now(&node, RD);
        drop(now(&node, WR));
        let r = now(&node, RD);
        let (count, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        let mut wait = r.async_poll();
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        drop(now(&node, RD));
        assert_eq!(woken(&count), 0);
        let w = now(&node, WR);
        assert_eq!(woken(&count), 0, "a writer that is there is no event");
        drop(w);
        assert!(woken(&count) > 0, "its going is");
        match wait.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(s)) => assert!(s.hangup),
            other => panic!("still waiting: {}", other.is_pending()),
        }
    }

    /// `wait_for_partner`: a blocking open waits for the other side to be
    /// opened, and a writer that came and went lets it go all the same --
    /// to read end-of-file and a hangup.
    #[test]
    fn a_blocking_open_waits_for_the_other_side_even_if_it_has_gone() {
        let node = a_fifo();
        let (r, partner) = open(&node, RD).unwrap();
        let partner = partner.expect("nobody at the other end");
        let (count, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        let mut wait = Box::pin(r.partner(partner));
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        drop(now(&node, WR));
        assert_eq!(woken(&count), 1);
        assert!(wait.as_mut().poll(&mut cx).is_ready());
        drop(wait);
        assert_eq!(read(&r, 1).unwrap(), b"");
        assert!(r.poll().unwrap().hangup);
    }

    /// The other way round: a writer waits for a reader. And an open with
    /// someone already at the other end does not wait.
    #[test]
    fn a_blocking_writer_waits_for_a_reader() {
        let node = a_fifo();
        let (w, partner) = open(&node, WR).unwrap();
        let mut wait = Box::pin(w.partner(partner.expect("no reader")));
        let (count, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        let (r, partner) = open(&node, RD).unwrap();
        assert!(partner.is_none(), "a writer is there");
        assert_eq!(woken(&count), 1);
        assert!(wait.as_mut().poll(&mut cx).is_ready());
        assert_eq!(w.write_at(0, b"w"), Ok(1));
        assert_eq!(read(&r, 1).unwrap(), b"w");
    }

    /// `O_RDWR` opens at once, reads what it wrote, and is its own writer
    /// and reader, so it neither reads end-of-file nor hangs up.
    #[test]
    fn an_end_for_both_reads_what_it_wrote_and_never_hangs_up() {
        let node = a_fifo();
        let (f, partner) = open(&node, RW).unwrap();
        assert!(partner.is_none());
        assert_eq!(f.write_at(0, b"ab"), Ok(2));
        assert_eq!(read(&f, 2).unwrap(), b"ab");
        assert_eq!(read(&f, 1), Err(FsError::Again));
        let s = f.poll().unwrap();
        assert!(s.write && !s.read && !s.hangup && !s.error);
        // A copy is both too: with the first gone it is still its own writer.
        let copy = f.clone();
        drop(f);
        assert_eq!(read(&copy, 1), Err(FsError::Again));
    }

    /// An end for both waits for what it is asked about: a blocking read of
    /// an empty one is not told about room, nor a writer on a full one about
    /// the bytes it is holding.
    #[test]
    fn an_end_for_both_waits_for_the_direction_asked_about() {
        let node = a_fifo();
        let f = now(&node, RW);
        let (_count, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert!(f.async_poll().as_mut().poll(&mut cx).is_pending());
        let mut fill = alloc::vec![0u8; PIPE_DEFAULT_CAPACITY];
        assert_eq!(f.write_at(0, &fill), Ok(PIPE_DEFAULT_CAPACITY));
        let mut room = f.async_poll_for(crate::fs::PollEvents::OUT);
        assert!(room.as_mut().poll(&mut cx).is_pending());
        assert!(f
            .async_poll_for(crate::fs::PollEvents::IN)
            .as_mut()
            .poll(&mut cx)
            .is_ready());
        assert_eq!(f.read_at(0, &mut fill[..PIPE_BUF]), Ok(PIPE_BUF));
        match room.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(s)) => assert!(s.write),
            other => panic!("still waiting: {}", other.is_pending()),
        }
    }

    /// The same through the open file, which is how `write` waits for room:
    /// `File::async_poll` must ask the pipe about writing, not about what an
    /// `async_poll` waits for.
    #[test]
    fn a_writer_on_a_full_fifo_for_both_waits_through_the_file() {
        use crate::fs::{File, FileLike, PollEvents};
        let node = a_fifo();
        let end = now(&node, RW);
        let fill = alloc::vec![0u8; PIPE_DEFAULT_CAPACITY];
        assert_eq!(end.write_at(0, &fill), Ok(PIPE_DEFAULT_CAPACITY));
        let file: Arc<dyn FileLike> =
            File::new(Arc::new(end), RW, alloc::string::String::from("/fifo"));
        let (_count, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        let mut room = Box::pin(file.async_poll(PollEvents::OUT));
        assert!(room.as_mut().poll(&mut cx).is_pending());
    }

    /// `fstat` of an end is the node: its number, type and mode, and a size
    /// of 0 whatever the pipe holds -- or the node, into which FIFOs were
    /// written before they were pipes.
    #[test]
    fn an_end_is_its_node_with_nothing_in_it() {
        let node = a_fifo();
        assert_eq!(node.write_at(0, b"old"), Ok(3));
        let (r, w) = (now(&node, RD), now(&node, WR));
        assert_eq!(w.write_at(0, b"abc"), Ok(3));
        let (meta, own) = (r.metadata().unwrap(), node.metadata().unwrap());
        assert_eq!(
            (meta.type_, meta.inode, meta.mode, meta.size),
            (FileType::NamedPipe, own.inode, 0o640, 0)
        );
        assert_eq!(w.metadata().unwrap().inode, own.inode);
    }

    /// The table keeps a FIFO for as long as an end of it is open, and lets
    /// it go at the next open of any FIFO once none is.
    #[test]
    fn the_table_lets_a_fifo_go_with_its_last_end() {
        let node = a_fifo();
        let meta = node.metadata().unwrap();
        let key = (meta.dev, meta.inode);
        let r = now(&node, RD);
        assert!(FIFOS.lock().contains_key(&key));
        drop(r);
        drop(now(&a_fifo(), RD));
        assert!(!FIFOS.lock().contains_key(&key));
    }
}
