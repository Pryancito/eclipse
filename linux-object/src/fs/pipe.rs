//! Implement INode for Pipe
#![deny(missing_docs)]

use crate::{sync::Event, sync::EventBus};
use alloc::{boxed::Box, collections::vec_deque::VecDeque, sync::Arc, vec::Vec};
use core::{any::Any, cmp::min};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use kernel_hal::sync::Mutex;
use rcore_fs::vfs::*;

mod fifo;
pub use fifo::FifoPartner;

#[derive(Clone, PartialEq, Eq)]
#[allow(dead_code)]
/// Pipe end specify
pub enum PipeEnd {
    /// read end
    Read,
    /// write end
    Write,
    /// Both at once: a FIFO opened `O_RDWR`, which reads what it writes.
    Both,
}

/// Default pipe capacity reported by `fcntl(F_GETPIPE_SZ)`: 16 pages, the
/// Linux default since 2.6.11.
pub const PIPE_DEFAULT_CAPACITY: usize = 65536;

/// `PIPE_BUF`: a write of at most this many bytes is atomic, which on a full
/// pipe means it waits (or answers `EAGAIN`) rather than going in by halves.
/// Linux keeps a pipe as page-sized slots, so "a slot is free" is also "a
/// `PIPE_BUF` fits": that is what the write end's readiness means here.
pub const PIPE_BUF: usize = 4096;

/// Pipe inner data
pub struct PipeData {
    /// pipe buffer
    buf: VecDeque<u8>,
    /// event bus for pipe
    eventbus: EventBus,
    /// number of read ends
    read_cnt: i32,
    /// number of write ends
    write_cnt: i32,
    /// Capacity of the byte queue, `F_GETPIPE_SZ`/`F_SETPIPE_SZ`. A write
    /// that does not fit answers `Again` (the caller waits or reports
    /// `EAGAIN`), and the write end reads as writable only with `PIPE_BUF`
    /// of room. The queue used to be unbounded and every write went through:
    /// `yes | sleep 10` grew the kernel heap by however fast `yes` runs.
    capacity: usize,
    /// How many read ends, and write ends, have ever been opened
    /// (`r_counter`, `w_counter`): a FIFO opener waits for the other one to
    /// move, not for its side to be open, so a writer that came and went
    /// lets a waiting reader go all the same. One each for a `pipe2` pipe.
    r_counter: u64,
    w_counter: u64,
    /// FIFO openers parked until the other side is opened.
    partner_waiters: Vec<Waker>,
}

impl PipeData {
    fn new(ends: i32) -> Self {
        let mut data = PipeData {
            buf: VecDeque::new(),
            eventbus: EventBus::default(),
            read_cnt: ends,
            write_cnt: ends,
            capacity: PIPE_DEFAULT_CAPACITY,
            r_counter: ends as u64,
            w_counter: ends as u64,
            partner_waiters: Vec::new(),
        };
        // An empty pipe has room: a writer subscribing for `WRITABLE` before
        // anything happened must not wait for a transition that never comes.
        data.publish_room();
        data
    }

    /// Bytes a write may add right now.
    fn room(&self) -> usize {
        self.capacity.saturating_sub(self.buf.len())
    }

    /// The write end's readiness: a `PIPE_BUF` (or, for a pipe smaller than
    /// that, the whole capacity) fits. Any smaller amount of room would let a
    /// parked writer wake only to be told `Again` again.
    fn has_room(&self) -> bool {
        self.room() >= PIPE_BUF.min(self.capacity)
    }

    fn publish_room(&mut self) {
        if self.has_room() {
            self.eventbus.set(Event::WRITABLE);
        } else {
            self.eventbus.clear(Event::WRITABLE);
        }
    }

    /// Remove `len` bytes (at most what is queued) and publish what that
    /// changes: an empty queue is no longer readable, the freed room may make
    /// the write end ready.
    fn drain_front(&mut self, len: usize) {
        self.buf.drain(..len);
        if self.buf.is_empty() {
            self.eventbus.clear(Event::READABLE);
        }
        self.publish_room();
    }
}

/// pipe struct
pub struct Pipe {
    data: Arc<Mutex<PipeData>>,
    direction: PipeEnd,
    /// What `fstat` says of either end, which pipefs keeps in the pipe's
    /// inode: its number, the same at both ends and the one in its
    /// `/proc/<pid>/fd` name, and who made it.
    ino: usize,
    uid: usize,
    gid: usize,
    /// The FIFO node this end was opened through, which `fstat` describes;
    /// `None` for a `pipe2` pipe.
    node: Option<Arc<dyn INode>>,
    /// The `w_counter` a FIFO reader opened `O_NONBLOCK` with no writer saw
    /// (`f_pipe`): having no writer is not a hangup for it until one has
    /// come. 0, which no `w_counter` of a `pipe2` pipe is, for every other.
    hup_after: u64,
}

impl Clone for Pipe {
    fn clone(&self) -> Self {
        let mut data = self.data.lock();
        if self.reads() {
            data.read_cnt += 1;
        }
        if self.writes() {
            data.write_cnt += 1;
        }
        Pipe {
            data: self.data.clone(),
            direction: self.direction.clone(),
            node: self.node.clone(),
            ..*self
        }
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // pipe end closed
        let mut data = self.data.lock();
        let mut side_gone = false;
        if self.reads() {
            data.read_cnt -= 1;
            side_gone |= data.read_cnt == 0;
        }
        if self.writes() {
            data.write_cnt -= 1;
            side_gone |= data.write_cnt == 0;
        }
        // Latch CLOSED only when this drop takes a whole SIDE away (EOF for
        // readers / EPIPE for writers) — that is when `poll()` starts
        // reporting the condition, so "CLOSED latched ⇒ a poll scan is ready"
        // holds and a parked readiness subscription can never immediate-fire
        // in a loop. The previous unconditional `set` latched CLOSED whenever
        // ANY handle dropped — including a dup'd fd with live siblings
        // (routine in shell redirections), which left the flag permanently on
        // for a perfectly healthy pipe and woke every later event-bus waiter
        // spuriously. And not on every drop while a side is gone: a FIFO's
        // sides come back (an open clears the flag), and a FIFO reader with
        // no writer yet must not be woken by another reader's close.
        if side_gone {
            data.eventbus.set(Event::CLOSED);
        }
    }
}

#[allow(dead_code)]
impl Pipe {
    /// Create a pair of INode: (read, write)
    pub fn create_pair() -> (Pipe, Pipe) {
        Self::create_pair_for(0, 0)
    }

    /// [`create_pair`](Self::create_pair) for a pipe that `uid` and `gid`
    /// own, as pipefs gives its inode the creator's `fsuid` and `fsgid`:
    /// reopening an end through `/proc/<pid>/fd/N` asks the same permission
    /// as opening a file.
    pub fn create_pair_for(uid: u32, gid: u32) -> (Pipe, Pipe) {
        let data = Arc::new(Mutex::new(PipeData::new(1)));
        let ino = zircon_object::object::KObjectBase::new_koid() as usize;
        let (uid, gid) = (uid as usize, gid as usize);
        (
            Pipe {
                data: data.clone(),
                direction: PipeEnd::Read,
                ino,
                uid,
                gid,
                node: None,
                hup_after: 0,
            },
            Pipe {
                data,
                direction: PipeEnd::Write,
                ino,
                uid,
                gid,
                node: None,
                hup_after: 0,
            },
        )
    }

    /// What `/proc/<pid>/fd/N` names either end: `pipe:[ino]`.
    pub fn link_name(&self) -> alloc::string::String {
        alloc::format!("pipe:[{}]", self.ino)
    }
    /// True when this handle is the read end of the pipe.
    pub fn is_read_end(&self) -> bool {
        self.direction == PipeEnd::Read
    }

    /// The FIFO node this end was opened through, `None` for a `pipe2`
    /// pipe: reopening the end through `/proc/<pid>/fd/N` opens the node.
    pub fn fifo_node(&self) -> Option<Arc<dyn INode>> {
        self.node.clone()
    }

    /// Whether this end reads: the read end, or a FIFO opened `O_RDWR`.
    fn reads(&self) -> bool {
        matches!(self.direction, PipeEnd::Read | PipeEnd::Both)
    }

    /// Whether this end writes: the write end, or a FIFO opened `O_RDWR`.
    fn writes(&self) -> bool {
        matches!(self.direction, PipeEnd::Write | PipeEnd::Both)
    }

    /// `pipe_poll`'s hangup for this end: a reader all of whose writers are
    /// gone, once it has seen one (see `hup_after`), and a writer all of
    /// whose readers are. An end that does both is its own other side.
    fn hangup(&self, data: &PipeData) -> bool {
        match self.direction {
            PipeEnd::Read => data.write_cnt == 0 && data.w_counter != self.hup_after,
            PipeEnd::Write => data.read_cnt == 0,
            PipeEnd::Both => false,
        }
    }

    /// Whether `other` shares this pipe's buffer. `tee`/`splice` between
    /// both ends of the same pipe is `EINVAL` (`do_tee` / `do_splice`);
    /// without the check, a peek-and-write loop fills the buffer forever.
    pub fn same_buffer(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }

    /// Park `waker` on this pipe's event bus for the next readiness
    /// transition (see `FileLike::subscribe_readiness`; reached through
    /// `File`'s inode downcast). The bus lives inside `PipeData`, so the
    /// unsubscribe handle captures the shared `Arc` and relocks it on drop.
    pub fn subscribe_readiness(
        &self,
        events: crate::fs::PollEvents,
        waker: &core::task::Waker,
    ) -> crate::sync::ReadinessSub {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        let id = {
            let mut data = self.data.lock();
            crate::sync::subscribe_waker(&mut data.eventbus, mask, waker)
        };
        match id {
            Some(id) => {
                let data = self.data.clone();
                crate::sync::ReadinessSub::try_new(move || {
                    data.lock().eventbus.unsubscribe(id);
                })
                .unwrap_or_else(|| {
                    self.data.lock().eventbus.unsubscribe(id);
                    crate::sync::ReadinessSub::noop()
                })
            }
            None => crate::sync::ReadinessSub::noop(),
        }
    }

    /// The pipe's publication counter for the directions `events` names (see
    /// `FileLike::readiness_seq`). One bus serves both ends, so it also moves
    /// for the other end's events -- but only in the directions asked for.
    pub fn readiness_seq(&self, events: crate::fs::PollEvents) -> u64 {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        self.data.lock().eventbus.seq_for(mask)
    }

    /// Park `waker` for the pipe's next published event in `events` (see
    /// `FileLike::subscribe_edge`).
    pub fn subscribe_edge(
        &self,
        events: crate::fs::PollEvents,
        waker: &core::task::Waker,
        seen: u64,
    ) -> crate::sync::ReadinessSub {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        let id = self.data.lock().eventbus.subscribe_edge(mask, waker, seen);
        match id {
            Some(id) => {
                let data = self.data.clone();
                crate::sync::ReadinessSub::try_new(move || {
                    data.lock().eventbus.unsubscribe(id);
                })
                .unwrap_or_else(|| {
                    self.data.lock().eventbus.unsubscribe(id);
                    crate::sync::ReadinessSub::noop()
                })
            }
            None => crate::sync::ReadinessSub::noop(),
        }
    }

    /// Nominal capacity, as reported by `fcntl(F_GETPIPE_SZ)`. Shared between
    /// both ends, like the kernel's pipe buffer is.
    pub fn capacity(&self) -> usize {
        self.data.lock().capacity
    }

    /// Bytes currently queued (`FIONREAD` / `pipe_ioctl`). Shared between
    /// ends — Linux reports the same occupancy on either fd.
    pub fn buffered_len(&self) -> usize {
        self.data.lock().buf.len()
    }

    /// Set the capacity (`fcntl(F_SETPIPE_SZ)`); the caller has already
    /// rounded and bounds-checked the value. Shrinking below what is queued
    /// is refused with `Busy` (`EBUSY`), as `pipe_set_size` does.
    pub fn set_capacity(&self, cap: usize) -> Result<()> {
        let mut data = self.data.lock();
        if cap < data.buf.len() {
            return Err(FsError::Busy);
        }
        data.capacity = cap;
        data.publish_room();
        Ok(())
    }

    /// Copy up to `len` buffered bytes without consuming them (`tee(2)`), plus
    /// whether any write end is still open — which is what distinguishes
    /// "would block" from end-of-stream when the buffer comes back empty.
    /// `None` when called on the write end.
    pub fn peek_data(&self, len: usize) -> Option<(alloc::vec::Vec<u8>, bool)> {
        if !self.reads() {
            return None;
        }
        let data = self.data.lock();
        let out = data.buf.iter().take(len).copied().collect();
        Some((out, data.write_cnt > 0))
    }

    /// Bytes a write could add right now, `None` once every read end is gone
    /// (a write would be `Broken`). `splice(2)` asks before it takes anything
    /// out of its input, so that nothing is consumed that cannot be delivered.
    pub fn write_room(&self) -> Option<usize> {
        let data = self.data.lock();
        (data.read_cnt > 0).then(|| data.room())
    }

    /// Drop the first `len` buffered bytes (`splice(2)` after it has delivered
    /// what `peek_data` showed it), returning how many went; `None` on the
    /// write end. Readiness moves exactly as after a `read_at` of that size.
    pub fn consume(&self, len: usize) -> Option<usize> {
        if !self.reads() {
            return None;
        }
        let mut data = self.data.lock();
        let len = min(len, data.buf.len());
        data.drain_front(len);
        Some(len)
    }

    /// whether the pipe struct is readable
    fn can_read(&self) -> bool {
        if self.reads() {
            // true
            let data = self.data.lock();
            !data.buf.is_empty() || data.write_cnt == 0 // other end closed
        } else {
            false
        }
    }

    /// whether the pipe struct is writeable
    fn can_write(&self) -> bool {
        if self.writes() {
            let data = self.data.lock();
            data.read_cnt > 0 && data.has_room()
        } else {
            false
        }
    }

    /// [`INode::async_poll`] for the directions `events` asks about. Only
    /// an end that does both needs telling: `async_poll` waits for what a
    /// read waits for, and a blocking write into a full FIFO opened
    /// `O_RDWR` would be told at once that its bytes are still there.
    pub fn async_poll_for(
        &self,
        events: crate::fs::PollEvents,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + '_>> {
        match self.direction {
            PipeEnd::Both => self.ready(events.wants_read(), events.wants_write()),
            _ => self.async_poll(),
        }
    }

    /// Wait until this end can read (`read`) or write (`write`), or has
    /// hung up, and answer what `poll` says then.
    fn ready(
        &self,
        read: bool,
        write: bool,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + '_>> {
        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct PipeFuture<'a> {
            pipe: &'a Pipe,
            read: bool,
            write: bool,
            sub_id: Option<u64>,
        }

        impl Drop for PipeFuture<'_> {
            fn drop(&mut self) {
                if let Some(id) = self.sub_id.take() {
                    self.pipe.data.lock().eventbus.unsubscribe(id);
                }
            }
        }

        impl<'a> Future for PipeFuture<'a> {
            type Output = Result<PollStatus>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                // Readiness and subscription under ONE hold of the pipe lock.
                // The previous shape checked readiness (taking and releasing
                // the lock inside `can_read`/`can_write`), and only then
                // re-took the lock to subscribe — a window in which the peer
                // could push its byte and fire the eventbus at nobody. With the
                // bus latching flags and firing only on transitions, a wake
                // missed there was not late, it was lost for good. The
                // subscribe-time check in `EventBus::subscribe` now also closes
                // this generically; doing it under one lock here means the pipe
                // does not depend on that second line of defense.
                //
                // sub_id + Drop: poll/epoll re-scan drops this future every
                // pass; without unsubscribe, orphaned wakers pile up and a
                // later pipe write wakes a freed task (UAF → delayed PAGE FAULT).
                let this = self.get_mut();
                let mut data = this.pipe.data.lock();
                let hangup = this.pipe.hangup(&data);
                // Readable data, or EOF/hangup when writers are gone. Room to
                // write while readers remain; hangup when they are gone (must
                // wake POLLHUP/POLLERR interest, not spin Pending). A full
                // pipe parks the writer here until a read frees a `PIPE_BUF`.
                let ready =
                    hangup || this.read && !data.buf.is_empty() || this.write && data.has_room();
                if ready {
                    if let Some(id) = this.sub_id.take() {
                        data.eventbus.unsubscribe(id);
                    }
                    drop(data);
                    return Poll::Ready(this.pipe.poll());
                }
                // Not `sub_id.is_none()`: the callback is one-shot, so after
                // the first wake the id names nothing on the bus. Another
                // reader that drains the buffer before we run leaves us
                // Pending forever with no callback. See `EventBusFuture`.
                let live = this
                    .sub_id
                    .map(|id| data.eventbus.is_subscribed(id))
                    .unwrap_or(false);
                if !live {
                    this.sub_id = None;
                    // Only this end's own transitions: the flags are latched
                    // and `subscribe` fires at once on any that is already
                    // set, so a reader parked on an empty pipe must not be
                    // woken by the `WRITABLE` an empty pipe always carries
                    // (nor a full pipe's writer by `READABLE`). `CLOSED` is
                    // set only once a whole side is gone, which is a hangup
                    // for whichever end is left to poll.
                    let mut mask = Event::CLOSED | Event::ERROR;
                    if this.read {
                        mask |= Event::READABLE;
                    }
                    if this.write {
                        mask |= Event::WRITABLE;
                    }
                    this.sub_id =
                        crate::sync::subscribe_waker(&mut data.eventbus, mask, cx.waker());
                }
                Poll::Pending
            }
        }

        Box::pin(PipeFuture {
            pipe: self,
            read,
            write,
            sub_id: None,
        })
    }
}

impl INode for Pipe {
    /// read from pipe
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.reads() {
            let mut data = self.data.lock();
            if data.buf.is_empty() && data.write_cnt > 0 {
                Err(FsError::Again)
            } else {
                let len = min(buf.len(), data.buf.len());
                // Bulk copy from the deque's two contiguous halves instead of a
                // per-byte pop_front loop (a 64 KiB read was 65 536 branchy
                // pops under the pipe mutex).
                let (front, back) = data.buf.as_slices();
                if len <= front.len() {
                    buf[..len].copy_from_slice(&front[..len]);
                } else {
                    buf[..front.len()].copy_from_slice(front);
                    buf[front.len()..len].copy_from_slice(&back[..len - front.len()]);
                }
                data.drain_front(len);
                Ok(len)
            }
        } else {
            Ok(0)
        }
    }

    /// write to pipe
    ///
    /// `pipe_write`: with no reader left the answer is `Broken` (`EPIPE`, and
    /// the syscall adds `SIGPIPE`), whatever the buffer holds. A write of at
    /// most `PIPE_BUF` bytes goes in whole or not at all; a bigger one takes
    /// what fits. `Again` is "no room for this", which a blocking writer
    /// turns into a wait for [`Event::WRITABLE`] and a non-blocking one into
    /// `EAGAIN`. Every write used to succeed in full: a reader that had gone
    /// away was never noticed, so `yes | head -1` ran `yes` for ever, and the
    /// queue grew without bound while it did.
    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        if self.writes() {
            let mut data = self.data.lock();
            if data.read_cnt == 0 {
                return Err(FsError::Broken);
            }
            let room = data.room();
            let take = if buf.len() <= PIPE_BUF {
                if room < buf.len() {
                    return Err(FsError::Again);
                }
                buf.len()
            } else {
                if room == 0 {
                    return Err(FsError::Again);
                }
                min(room, buf.len())
            };
            // Copy-slice specialization (memcpy) instead of a per-byte
            // push_back loop.
            data.buf.extend(&buf[..take]);
            data.eventbus.set(Event::READABLE);
            data.publish_room();
            Ok(take)
        } else {
            Ok(0)
        }
    }

    /// monitoring events and determine whether the pipe is readable or writeable
    /// if the write end is not close and the buffer is empty, the read end will be block
    fn poll(&self) -> Result<PollStatus> {
        let data = self.data.lock();
        let hangup = self.hangup(&data);
        Ok(PollStatus {
            read: self.reads() && (!data.buf.is_empty() || hangup),
            // Writable with a `PIPE_BUF` of room, as `pipe_poll` reports
            // `POLLOUT` for a free slot; a full pipe is not writable, or a
            // writer parked on it spins on `Again`.
            write: self.writes() && data.read_cnt > 0 && data.has_room(),
            // Linux: POLLERR on the write end when no readers remain.
            error: self.writes() && data.read_cnt == 0,
            hangup,
        })
    }

    /// What a blocking read waits for, as every `async_poll` does: an end
    /// that does both waits to read, and [`Pipe::async_poll_for`] is how a
    /// writer waits on it.
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        self.ready(self.reads(), !self.reads())
    }

    /// `fstat` of either end: a FIFO, `0600`, with the pipe's own number and
    /// a page for `st_blksize`, as pipefs answers. There was no answer at
    /// all, so `fstat` of a pipe failed (`ENOSYS`): `S_ISFIFO` could not be
    /// asked of one, and stdio, which sizes its buffer from `st_blksize`,
    /// had to guess.
    ///
    /// An end of a FIFO is its node, with nothing in it: what was written
    /// is in the pipe.
    fn metadata(&self) -> Result<Metadata> {
        if let Some(node) = &self.node {
            let mut meta = node.metadata()?;
            meta.size = 0;
            return Ok(meta);
        }
        let mut meta = crate::fs::anon_metadata(FileType::NamedPipe, self.ino);
        meta.blk_size = PIPE_BUF;
        meta.uid = self.uid;
        meta.gid = self.gid;
        Ok(meta)
    }

    /// return the any ref
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};
    use core::task::{RawWaker, RawWakerVTable, Waker};

    fn flag_waker(flag: &'static AtomicBool) -> Waker {
        fn raw(ptr: *const ()) -> RawWaker {
            unsafe fn clone(ptr: *const ()) -> RawWaker {
                raw(ptr)
            }
            unsafe fn wake(ptr: *const ()) {
                (*(ptr as *const AtomicBool)).store(true, Ordering::SeqCst);
            }
            unsafe fn wake_by_ref(ptr: *const ()) {
                (*(ptr as *const AtomicBool)).store(true, Ordering::SeqCst);
            }
            unsafe fn drop(_: *const ()) {}
            RawWaker::new(ptr, &RawWakerVTable::new(clone, wake, wake_by_ref, drop))
        }
        unsafe { Waker::from_raw(raw(flag as *const AtomicBool as *const ())) }
    }

    /// Both ends of `create_pair` share a buffer; two pairs do not. That is
    /// the predicate `tee`/`splice` use for `EINVAL`.
    #[test]
    fn ends_of_one_pair_share_a_buffer_and_two_pairs_do_not() {
        let (r1, w1) = Pipe::create_pair();
        let (r2, w2) = Pipe::create_pair();
        assert!(r1.same_buffer(&w1));
        assert!(w1.same_buffer(&r1));
        assert!(!r1.same_buffer(&r2));
        assert!(!r1.same_buffer(&w2));
        assert!(!w1.same_buffer(&w2));
    }

    /// poll/epoll drops Pending async_poll futures every re-scan; without
    /// unsubscribe that leaks wakers into UAF when the pipe later gets data.
    #[test]
    fn async_poll_drop_unsubscribes() {
        static WOKE: AtomicBool = AtomicBool::new(false);
        WOKE.store(false, Ordering::SeqCst);

        let (r, _w) = Pipe::create_pair();
        // Empty read end with a live writer → not ready → must subscribe.
        assert!(!r.can_read());

        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);
        let mut fut = r.async_poll();
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        let n = r.data.lock().eventbus.get_callback_len();
        assert!(n >= 1, "Pending poll must park a waker");

        drop(fut);
        assert_eq!(
            r.data.lock().eventbus.get_callback_len(),
            n - 1,
            "Drop must unsubscribe the parked pipe waker"
        );
        assert!(!WOKE.load(Ordering::SeqCst));
    }

    /// Compressed stand-in for a 30–40 min compositor session: tens of thousands
    /// of poll re-scans must leave the pipe EventBus empty.
    #[test]
    fn async_poll_drop_soak_does_not_fill_bus() {
        static WOKE: AtomicBool = AtomicBool::new(false);
        WOKE.store(false, Ordering::SeqCst);

        let (r, _w) = Pipe::create_pair();
        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);

        for _ in 0..20_000 {
            let mut fut = r.async_poll();
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
            drop(fut);
            assert_eq!(
                r.data.lock().eventbus.get_callback_len(),
                0,
                "each Drop must clear the parked pipe waker"
            );
        }
    }

    fn fill(w: &Pipe, n: usize) {
        let bytes = alloc::vec![0xabu8; n];
        assert_eq!(w.write_at(0, &bytes), Ok(n));
    }

    fn drain(r: &Pipe, n: usize) -> usize {
        let mut sink = alloc::vec![0u8; n];
        r.read_at(0, &mut sink).unwrap()
    }

    /// `pipe_write`: no reader left is `EPIPE`, whatever the pipe holds.
    /// Every write used to succeed, so a writer never learnt its reader had
    /// gone and `yes | head -1` never ended.
    #[test]
    fn a_write_with_no_reader_is_a_broken_pipe() {
        let (r, w) = Pipe::create_pair();
        assert_eq!(w.write_at(0, b"still read"), Ok(10));
        drop(r);
        assert_eq!(w.write_at(0, b"x"), Err(FsError::Broken));
        let s = w.poll().unwrap();
        assert!(
            s.hangup && s.error && !s.write,
            "POLLHUP|POLLERR, not POLLOUT"
        );
    }

    /// The byte queue is bounded by the pipe's capacity: what does not fit
    /// answers `Again`, and room comes back as the reader drains. It used to
    /// take everything, for ever.
    #[test]
    fn the_queue_stops_at_its_capacity() {
        let (r, w) = Pipe::create_pair();
        fill(&w, PIPE_DEFAULT_CAPACITY);
        assert_eq!(w.write_at(0, b"x"), Err(FsError::Again));
        assert!(!w.poll().unwrap().write, "a full pipe is not writable");
        assert_eq!(drain(&r, PIPE_BUF), PIPE_BUF);
        assert!(w.poll().unwrap().write);
        assert_eq!(w.write_at(0, b"x"), Ok(1));
    }

    /// `PIPE_BUF` atomicity: a write of at most that many bytes is all or
    /// nothing, a bigger one takes what fits.
    #[test]
    fn a_small_write_is_all_or_nothing_and_a_big_one_takes_what_fits() {
        let (_r, w) = Pipe::create_pair();
        fill(&w, PIPE_DEFAULT_CAPACITY - 100);
        let small = [0u8; 200];
        assert_eq!(w.write_at(0, &small), Err(FsError::Again), "200 into 100");
        let big = [0u8; PIPE_BUF + 1];
        assert_eq!(w.write_at(0, &big), Ok(100), "what fits of a big write");
        assert_eq!(w.write_at(0, &big), Err(FsError::Again), "nothing fits now");
        // At the boundary a PIPE_BUF write still goes in whole.
        assert_eq!(drain(&_r, PIPE_BUF), PIPE_BUF);
        assert_eq!(w.write_at(0, &[0u8; PIPE_BUF]), Ok(PIPE_BUF));
    }

    /// The write end is ready only with a `PIPE_BUF` of room: with less, a
    /// parked writer of an atomic chunk would be woken just to be told
    /// `Again` again.
    #[test]
    fn writable_means_a_pipe_buf_of_room() {
        let (r, w) = Pipe::create_pair();
        fill(&w, PIPE_DEFAULT_CAPACITY - PIPE_BUF + 1);
        assert!(!w.can_write());
        assert!(!w.poll().unwrap().write);
        assert!(!r.data.lock().eventbus.events().contains(Event::WRITABLE));
        assert_eq!(drain(&r, 1), 1);
        assert!(w.can_write());
        assert!(w.poll().unwrap().write);
        assert!(r.data.lock().eventbus.events().contains(Event::WRITABLE));
    }

    /// A fresh pipe advertises room on its bus from the start, so a writer
    /// that subscribes for `WRITABLE` before anything happened is not left
    /// waiting for a transition that never comes.
    #[test]
    fn a_fresh_pipe_advertises_room() {
        let (r, w) = Pipe::create_pair();
        assert!(r.data.lock().eventbus.events().contains(Event::WRITABLE));
        assert!(w.can_write());
        fill(&w, PIPE_DEFAULT_CAPACITY);
        assert!(!w.data.lock().eventbus.events().contains(Event::WRITABLE));
    }

    /// The blocking write path parks on `async_poll` when the pipe is full
    /// and must be woken by the read that frees room.
    #[test]
    fn a_writer_parked_on_a_full_pipe_is_woken_by_a_read() {
        static WOKE: AtomicBool = AtomicBool::new(false);
        WOKE.store(false, Ordering::SeqCst);

        let (r, w) = Pipe::create_pair();
        fill(&w, PIPE_DEFAULT_CAPACITY);
        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);
        let mut fut = w.async_poll();
        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Pending),
            "full: parks"
        );
        assert!(!WOKE.load(Ordering::SeqCst));
        // A read that frees less than a PIPE_BUF is not the wake.
        assert_eq!(drain(&r, 10), 10);
        assert!(!WOKE.load(Ordering::SeqCst), "10 bytes of room is not room");
        assert_eq!(drain(&r, PIPE_BUF), PIPE_BUF);
        assert!(
            WOKE.load(Ordering::SeqCst),
            "a PIPE_BUF of room wakes the writer"
        );
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(s)) => assert!(s.write),
            other => panic!(
                "expected Ready(write), got {:?}",
                other.map(|r| r.map(|s| s.write))
            ),
        }
    }

    /// `splice(2)` looks at the room before it takes anything out of its
    /// input, and a pipe with no reader left has none to offer: the answer
    /// must be `EPIPE`, not a write that fails after the input was consumed.
    #[test]
    fn write_room_is_what_fits_and_nothing_once_the_readers_are_gone() {
        let (r, w) = Pipe::create_pair();
        assert_eq!(w.write_room(), Some(PIPE_DEFAULT_CAPACITY));
        fill(&w, PIPE_DEFAULT_CAPACITY - 100);
        assert_eq!(w.write_room(), Some(100));
        fill(&w, 100);
        assert_eq!(w.write_room(), Some(0), "full: no room, but not broken");
        drop(r);
        assert_eq!(w.write_room(), None, "no reader: a write would be EPIPE");
    }

    /// After `tee`-style peeking, `consume` drops exactly the delivered bytes
    /// and leaves the rest in order; readiness moves as a read of that size
    /// would move it.
    #[test]
    fn consume_drops_only_the_delivered_bytes_and_publishes_readiness() {
        let (r, w) = Pipe::create_pair();
        assert_eq!(w.write_at(0, b"abcdef"), Ok(6));
        assert_eq!(w.consume(3), None, "only the read end consumes");
        assert_eq!(r.consume(2), Some(2));
        let (rest, _) = r.peek_data(10).unwrap();
        assert_eq!(rest, b"cdef", "the rest keeps its order");
        assert!(r.data.lock().eventbus.events().contains(Event::READABLE));
        assert_eq!(r.consume(100), Some(4), "at most what is queued");
        assert!(
            !r.data.lock().eventbus.events().contains(Event::READABLE),
            "an emptied pipe is no longer readable"
        );
    }

    /// Consuming a `PIPE_BUF` out of a full pipe wakes a parked writer, as a
    /// read would: `splice` draining a full pipe into a file must let the
    /// producer go on.
    #[test]
    fn consume_frees_room_for_a_parked_writer() {
        static WOKE: AtomicBool = AtomicBool::new(false);
        WOKE.store(false, Ordering::SeqCst);

        let (r, w) = Pipe::create_pair();
        fill(&w, PIPE_DEFAULT_CAPACITY);
        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);
        let mut fut = w.async_poll();
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(r.consume(PIPE_BUF), Some(PIPE_BUF));
        assert!(WOKE.load(Ordering::SeqCst), "a PIPE_BUF of room wakes it");
        assert!(w.can_write());
        assert_eq!(w.write_at(0, &[0u8; PIPE_BUF]), Ok(PIPE_BUF));
    }

    /// `F_SETPIPE_SZ` below what is queued is `EBUSY` (`pipe_set_size`);
    /// growing it makes room at once.
    #[test]
    fn shrinking_the_capacity_below_what_is_queued_is_busy() {
        let (_r, w) = Pipe::create_pair();
        fill(&w, 10_000);
        assert_eq!(w.set_capacity(PIPE_BUF), Err(FsError::Busy));
        assert_eq!(w.capacity(), PIPE_DEFAULT_CAPACITY);
        assert_eq!(
            w.set_capacity(10_000),
            Ok(()),
            "exactly what is queued is fine"
        );
        assert!(!w.can_write(), "and leaves no room");
        assert!(!w.data.lock().eventbus.events().contains(Event::WRITABLE));
        assert_eq!(w.write_at(0, b"x"), Err(FsError::Again));
        assert_eq!(w.set_capacity(16_384), Ok(()));
        assert!(w.can_write());
        // Growing publishes the room, so a writer parked on the old size wakes.
        assert!(w.data.lock().eventbus.events().contains(Event::WRITABLE));
        assert_eq!(w.write_at(0, b"x"), Ok(1));
    }

    /// `fstat` of either end is one FIFO: the same number at both ends, a
    /// different one for the next pipe, `0600`, a page for `st_blksize`, and
    /// the creator for owner, which a reopen through `/proc` is checked
    /// against. A clone (a `dup`) is the same pipe again.
    #[test]
    fn both_ends_answer_fstat_as_one_fifo() {
        let (r, w) = Pipe::create_pair_for(1000, 100);
        let (other, _) = Pipe::create_pair();
        let (rm, wm) = (r.metadata().unwrap(), w.metadata().unwrap());
        assert_eq!(
            (rm.type_, wm.type_),
            (FileType::NamedPipe, FileType::NamedPipe)
        );
        assert_eq!(rm.inode, wm.inode);
        assert_ne!(rm.inode, other.metadata().unwrap().inode);
        assert_eq!((rm.mode, rm.blk_size, rm.nlinks), (0o600, 4096, 1));
        assert_eq!((wm.uid, wm.gid), (1000, 100));
        let again = w.clone().metadata().unwrap();
        assert_eq!((again.inode, again.uid, again.gid), (wm.inode, 1000, 100));
    }

    /// `/proc/<pid>/fd/N` names a pipe by that same number.
    #[test]
    fn a_pipe_is_named_by_its_inode() {
        let (r, w) = Pipe::create_pair();
        let ino = r.metadata().unwrap().inode;
        assert_eq!(r.link_name(), alloc::format!("pipe:[{}]", ino));
        assert_eq!(w.link_name(), r.link_name());
    }
}
