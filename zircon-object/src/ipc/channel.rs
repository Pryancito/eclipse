use {
    crate::object::*,
    alloc::collections::VecDeque,
    alloc::sync::{Arc, Weak},
    alloc::vec::Vec,
    core::convert::TryInto,
    core::future::Future,
    core::pin::Pin,
    core::sync::atomic::{AtomicU32, Ordering},
    core::task::{Context, Poll, Waker},
    kernel_hal::sync::Mutex,
};

/// Bidirectional interprocess communication
///
/// # SYNOPSIS
///
/// A channel is a bidirectional transport of messages consisting of some
/// amount of byte data and some number of handles.
///
/// # DESCRIPTION
///
/// The process of sending a message via a channel has two steps. The first is to
/// atomically write the data into the channel and move ownership of all handles in
/// the message into this channel. This operation always consumes the handles: at
/// the end of the call, all handles either are all in the channel or are all
/// discarded. The second operation, channel read, is similar: on success
/// all the handles in the next message are atomically moved into the
/// receiving process' handle table. On failure, the channel retains ownership.
pub struct Channel {
    base: KObjectBase,
    _counter: CountHelper,
    peer: Weak<Channel>,
    recv_queue: Mutex<VecDeque<T>>,
    call_reply: Mutex<ReplyTable>,
    next_txid: AtomicU32,
}

/// The `call`s an endpoint has in flight, each waiting for the reply that
/// carries its txid.
///
/// This used to be a `HashMap` of `oneshot` senders: every call allocated
/// the oneshot's shared state and hashed its txid twice, which was most of
/// the cost of a call+reply on the object (see `ipc_baseline`). An endpoint
/// has a handful of calls in flight at most -- usually one -- so a vector
/// searched from the end is faster than hashing, and the entries' storage is
/// reused: a steady stream of calls allocates nothing after the first.
///
/// Every state change happens under the one lock that guards the table, so
/// the window the oneshot had (a writer holding the sender of a call that
/// was being cancelled) does not exist: a reply either reaches a call that
/// is still there, or finds no entry and is queued as an ordinary message.
#[derive(Default)]
struct ReplyTable {
    slots: Vec<ReplySlot>,
}

struct ReplySlot {
    txid: TxID,
    state: SlotState,
}

enum SlotState {
    /// No reply yet; the waker of the last poll, if it was polled.
    Waiting(Option<Waker>),
    /// The reply (or `PEER_CLOSED`), not yet taken by the call.
    Ready(ZxResult<T>),
}

impl ReplyTable {
    fn position(&self, txid: TxID) -> Option<usize> {
        self.slots.iter().rposition(|slot| slot.txid == txid)
    }

    /// Hand `msg` to the call waiting for `txid`, answering the waker to wake
    /// once the lock is released. `Err(msg)` when there is none, or its reply
    /// is already in.
    fn deliver(&mut self, txid: TxID, msg: T) -> Result<Option<Waker>, T> {
        let Some(i) = self.position(txid) else {
            return Err(msg);
        };
        let slot = &mut self.slots[i];
        let SlotState::Waiting(waker) = &mut slot.state else {
            return Err(msg);
        };
        let waker = waker.take();
        slot.state = SlotState::Ready(Ok(msg));
        Ok(waker)
    }

    /// Remove the slot of `txid`, answering the reply it held, if any.
    fn remove(&mut self, txid: TxID) -> Option<ZxResult<T>> {
        let i = self.position(txid)?;
        match self.slots.swap_remove(i).state {
            SlotState::Ready(reply) => Some(reply),
            SlotState::Waiting(_) => None,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

type T = MessagePacket;
type TxID = u32;

/// How many messages one endpoint may hold unread before the writer that
/// put one more there is told about it: Zircon's `kMaxPendingMessageCount`.
///
/// The message past the limit is still queued -- `zx_channel_write` succeeds
/// -- and the writing thread gets a `ZX_EXCP_POLICY_ERROR` with
/// `ZX_EXCP_POLICY_CODE_CHANNEL_FULL_WRITE`, which kills the process unless
/// a handler lets it carry on. That is what keeps a process nobody reads from
/// out of the kernel heap: `ChannelTest.ChannelFullException` writes until
/// it is killed and then waits for the process to be gone. Refusing the
/// write with `SHOULD_WAIT`, which is what this used to do, left that test
/// waiting for ever: the writer stopped and nobody killed it.
pub const MAX_PENDING_MESSAGE_COUNT: usize = 3500;

/// Transaction ids the kernel hands out for `call` live in the upper half;
/// the lower half is for userspace's own ids.
const KERNEL_TXID_BASE: TxID = 0x8000_0000;

impl_kobject!(Channel
    fn peer(&self) -> ZxResult<Arc<dyn KernelObject>> {
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        Ok(peer)
    }
    fn related_koid(&self) -> KoID {
        self.peer.upgrade().map(|p| p.id()).unwrap_or(0)
    }
);
define_count_helper!(Channel);

impl Channel {
    /// Create a channel and return a pair of its endpoints
    #[allow(unsafe_code)]
    pub fn create() -> (Arc<Self>, Arc<Self>) {
        let channel0 = Arc::new(Channel {
            base: KObjectBase::with_signal(Signal::WRITABLE),
            _counter: CountHelper::new(),
            peer: Weak::default(),
            recv_queue: Default::default(),
            call_reply: Default::default(),
            next_txid: AtomicU32::new(0),
        });
        let channel1 = Arc::new(Channel {
            base: KObjectBase::with_signal(Signal::WRITABLE),
            _counter: CountHelper::new(),
            peer: Arc::downgrade(&channel0),
            recv_queue: Default::default(),
            call_reply: Default::default(),
            next_txid: AtomicU32::new(0),
        });
        // no other reference of `channel0`
        unsafe { &mut *(Arc::as_ptr(&channel0) as *mut Channel) }.peer = Arc::downgrade(&channel1);
        (channel0, channel1)
    }

    /// Read a packet from the channel if check is ok, otherwise the msg will keep.
    pub fn check_and_read(&self, checker: impl FnOnce(&T) -> ZxResult) -> ZxResult<T> {
        let mut recv_queue = self.recv_queue.lock();
        if let Some(msg) = recv_queue.front() {
            checker(msg)?;
            let msg = recv_queue.pop_front().unwrap();
            if recv_queue.is_empty() {
                self.base.signal_clear(Signal::READABLE);
            }
            return Ok(msg);
        }
        if self.peer_closed() {
            Err(ZxError::PEER_CLOSED)
        } else {
            Err(ZxError::SHOULD_WAIT)
        }
    }

    /// Read a packet from the channel
    pub fn read(&self) -> ZxResult<T> {
        self.check_and_read(|_| Ok(()))
    }

    /// How many messages the peer holds unread: what a writer that must
    /// not grow the peer's queue without end asks before it writes. None
    /// when the peer is gone, whose queue went with it.
    pub fn peer_pending(&self) -> usize {
        self.peer
            .upgrade()
            .map_or(0, |peer| peer.recv_queue.lock().len())
    }

    /// Write a packet to the channel
    pub fn write(&self, msg: T) -> ZxResult {
        self.write_counted(msg).map(|_| ())
    }

    /// Write a packet to the channel and say how many the peer now holds
    /// unread, so the caller can tell whether this one went past
    /// [`MAX_PENDING_MESSAGE_COUNT`]. A reply that a parked `call` took
    /// counts as nothing pending.
    pub fn write_counted(&self, msg: T) -> ZxResult<usize> {
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        // check first 4 bytes: whether it is a call reply?
        let txid = msg.get_txid();
        // The reply table's lock is released before `push_general` takes the
        // peer's queue: nothing may hold both.
        let msg = if txid != 0 {
            let delivered = peer.call_reply.lock().deliver(txid, msg);
            match delivered {
                Ok(waker) => {
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                    return Ok(0);
                }
                // No call is waiting for it (never was, or it was cancelled):
                // Zircon leaves it on the endpoint as an ordinary message.
                Err(msg) => msg,
            }
        } else {
            msg
        };
        peer.push_general(msg)
    }

    /// Send a message to a channel and await a reply.
    ///
    /// The first four bytes of the written and read back messages are treated as a
    /// transaction ID.  The kernel generates a txid for the
    /// written message, replacing that part of the message as read from userspace.
    ///
    /// `msg.data` must have at lease a length of 4 bytes.
    ///
    /// Dropping the returned future (a deadline that passed, a killed
    /// thread) cancels the call: its reply slot goes away, and a reply that
    /// arrives later is an ordinary message on this endpoint, as in Zircon.
    /// The slot used to outlive the future, so every timed-out call left one
    /// behind for good and swallowed its late reply.
    pub async fn call(self: &Arc<Self>, msg: T) -> ZxResult<T> {
        self.call_counted(msg)?.1.await
    }

    /// `call`, split where the syscall layer needs it: the request is queued
    /// here and now, and what comes back is how many messages the peer holds
    /// unread with it -- the same count [`write_counted`] answers, for the
    /// same policy exception past [`MAX_PENDING_MESSAGE_COUNT`] -- plus the
    /// future that waits for the reply. A `call` is a write as far as the
    /// peer's queue is concerned, and Zircon raises
    /// `ZX_EXCP_POLICY_CODE_CHANNEL_FULL_WRITE` for it too: without that a
    /// caller whose peer never answers, each call timing out in turn, would
    /// grow the peer's queue without end and nothing would ever say so.
    ///
    /// [`write_counted`]: Channel::write_counted
    #[allow(clippy::type_complexity)]
    pub fn call_counted(
        self: &Arc<Self>,
        mut msg: T,
    ) -> ZxResult<(usize, impl Future<Output = ZxResult<T>> + '_)> {
        assert!(msg.data.len() >= 4);
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        let txid = self.new_txid();
        msg.set_txid(txid);
        self.call_reply.lock().slots.push(ReplySlot {
            txid,
            state: SlotState::Waiting(None),
        });
        // From here on the slot belongs to the future: dropping it (an error
        // below, a deadline, a killed thread) removes the slot.
        let reply = ReplyFuture {
            channel: self,
            txid,
            done: false,
        };
        let pending = peer.push_general(msg)?;
        Ok((pending, reply))
    }

    /// Push a message to general queue, called from peer. Answers how many
    /// are queued with it, the limit included: see
    /// [`MAX_PENDING_MESSAGE_COUNT`] for why none is ever refused.
    fn push_general(&self, msg: T) -> ZxResult<usize> {
        let mut send_queue = self.recv_queue.lock();
        send_queue.push_back(msg);
        if send_queue.len() == 1 {
            self.base.signal_set(Signal::READABLE);
        }
        Ok(send_queue.len())
    }

    /// Generate a new transaction ID for `call`.
    ///
    /// The counter wraps inside the kernel's half: a plain `fetch_add`
    /// reached 0 after `2^31` calls, and a reply carrying txid 0 is an
    /// ordinary message, so that call would never have returned.
    fn new_txid(&self) -> TxID {
        KERNEL_TXID_BASE | (self.next_txid.fetch_add(1, Ordering::SeqCst) & !KERNEL_TXID_BASE)
    }

    /// Is peer channel closed?
    fn peer_closed(&self) -> bool {
        self.peer.strong_count() == 0
    }
}

/// The reply of one `call`. Its slot goes when the call ends however it
/// ends, so a cancelled call does not keep it.
struct ReplyFuture<'a> {
    channel: &'a Channel,
    txid: TxID,
    /// The reply has been taken, and the slot with it.
    done: bool,
}

impl Future for ReplyFuture<'_> {
    type Output = ZxResult<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut table = self.channel.call_reply.lock();
        let Some(i) = table.position(self.txid) else {
            // Only `done` futures have no slot, and those are not polled.
            return Poll::Ready(Err(ZxError::INTERNAL));
        };
        match &mut table.slots[i].state {
            SlotState::Waiting(waker) => {
                match waker {
                    Some(w) if w.will_wake(cx.waker()) => {}
                    _ => *waker = Some(cx.waker().clone()),
                }
                Poll::Pending
            }
            SlotState::Ready(_) => {
                let reply = table.remove(self.txid).expect("just found");
                drop(table);
                self.done = true;
                Poll::Ready(reply)
            }
        }
    }
}

impl Drop for ReplyFuture<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let reply = self.channel.call_reply.lock().remove(self.txid);
        // A reply that made it in before the call gave up is put on the
        // endpoint, where Zircon leaves a late reply: dropping it was a lost
        // reply, since the peer's `write` had answered `Ok`. The table's
        // lock is released first; `push_general` takes the queue.
        if let Some(Ok(msg)) = reply {
            // A full queue is the one case with nowhere to put it.
            let _ = self.channel.push_general(msg);
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        if let Some(peer) = self.peer.upgrade() {
            peer.base
                .signal_change(Signal::WRITABLE, Signal::PEER_CLOSED);
            // Only the calls still waiting: one whose reply is already in
            // keeps it. Woken once the table's lock is released.
            let mut wakers = Vec::new();
            for slot in peer.call_reply.lock().slots.iter_mut() {
                if let SlotState::Waiting(waker) = &mut slot.state {
                    wakers.extend(waker.take());
                    slot.state = SlotState::Ready(Err(ZxError::PEER_CLOSED));
                }
            }
            for waker in wakers {
                waker.wake();
            }
        }
    }
}

/// The message transferred in the channel.
/// See [Channel](struct.Channel.html) for details.
#[derive(Default, Debug)]
pub struct MessagePacket {
    /// The data carried by the message packet
    pub data: Vec<u8>,
    /// See [Channel](struct.Channel.html) for details.
    pub handles: Vec<Handle>,
}

impl MessagePacket {
    /// Set txid (the first 4 bytes)
    pub fn set_txid(&mut self, txid: TxID) {
        if self.data.len() >= core::mem::size_of::<TxID>() {
            self.data[..4].copy_from_slice(&txid.to_ne_bytes());
        }
    }

    /// Get txid (the first 4 bytes)
    pub fn get_txid(&self) -> TxID {
        if self.data.len() >= core::mem::size_of::<TxID>() {
            TxID::from_ne_bytes(self.data[..4].try_into().unwrap())
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use core::future::Future;
    use core::pin::Pin;
    use core::sync::atomic::*;
    use core::task::{Context, Poll, Waker};
    use core::time::Duration;

    fn message(data: &[u8]) -> MessagePacket {
        MessagePacket {
            data: data.to_vec(),
            handles: Vec::new(),
        }
    }

    fn reply_to(txid: TxID, data: &[u8]) -> MessagePacket {
        let mut reply = txid.to_ne_bytes().to_vec();
        reply.extend_from_slice(data);
        message(&reply)
    }

    /// A `call` polled once by hand, so it is on the wire and waiting.
    fn call_in_flight<'a>(
        channel: &'a Arc<Channel>,
        data: &[u8],
    ) -> Pin<Box<dyn Future<Output = ZxResult<MessagePacket>> + 'a>> {
        let mut future: Pin<Box<dyn Future<Output = ZxResult<MessagePacket>> + 'a>> =
            Box::pin(channel.call(message(data)));
        assert!(future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        future
    }

    /// Baseline for the microkernel IPC work: cost of the channel object
    /// alone (no trap, no context switch) for a 32-byte message written on
    /// one end and read on the other, and for a full `call` + reply.
    /// Run with `cargo test -p zircon-object --lib --features
    /// libos,aspace-separate,elf --release ipc_baseline -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ipc_baseline() {
        extern crate std;
        use std::time::Instant;
        const N: u32 = 200_000;
        let (end0, end1) = Channel::create();
        let payload = [0u8; 32];

        let start = Instant::now();
        for _ in 0..N {
            end0.write(message(&payload)).unwrap();
            core::hint::black_box(end1.read().unwrap());
        }
        let oneway = start.elapsed().as_nanos() as f64 / N as f64;

        let mut cx = Context::from_waker(Waker::noop());
        let start = Instant::now();
        for _ in 0..N {
            let mut fut = Box::pin(end0.call(message(&payload)));
            assert!(fut.as_mut().poll(&mut cx).is_pending());
            let req = end1.read().unwrap();
            end1.write(reply_to(req.get_txid(), &payload)).unwrap();
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(r) => drop(core::hint::black_box(r.unwrap())),
                Poll::Pending => panic!("reply not delivered"),
            }
        }
        let call = start.elapsed().as_nanos() as f64 / N as f64;
        std::println!("ipc_baseline: write+read {oneway:.1} ns, call+reply {call:.1} ns");
    }

    #[test]
    fn test_basics() {
        let (end0, end1) = Channel::create();
        assert!(Arc::ptr_eq(
            &end0.peer().unwrap().downcast_arc().unwrap(),
            &end1
        ));
        assert_eq!(end0.related_koid(), end1.id());

        drop(end1);
        assert_eq!(end0.peer().unwrap_err(), ZxError::PEER_CLOSED);
        assert_eq!(end0.related_koid(), 0);
    }

    #[test]
    fn read_write() {
        let (channel0, channel1) = Channel::create();
        // write a message to each other
        channel0
            .write(MessagePacket {
                data: Vec::from("hello 1"),
                handles: Vec::new(),
            })
            .unwrap();
        channel1
            .write(MessagePacket {
                data: Vec::from("hello 0"),
                handles: Vec::new(),
            })
            .unwrap();

        // read message should success
        let recv_msg = channel1.read().unwrap();
        assert_eq!(recv_msg.data.as_slice(), b"hello 1");
        assert!(recv_msg.handles.is_empty());

        let recv_msg = channel0.read().unwrap();
        assert_eq!(recv_msg.data.as_slice(), b"hello 0");
        assert!(recv_msg.handles.is_empty());

        // read more message should fail.
        assert_eq!(channel0.read().err(), Some(ZxError::SHOULD_WAIT));
        assert_eq!(channel1.read().err(), Some(ZxError::SHOULD_WAIT));
    }

    #[test]
    fn a_writer_can_ask_how_many_messages_the_peer_has_unread() {
        let (channel0, channel1) = Channel::create();
        assert_eq!(channel0.peer_pending(), 0);
        channel0.write(message(b"one")).unwrap();
        channel0.write(message(b"two")).unwrap();
        // What this end wrote is the other end's to read, not its own.
        assert_eq!(channel0.peer_pending(), 2);
        assert_eq!(channel1.peer_pending(), 0);
        channel1.read().unwrap();
        assert_eq!(channel0.peer_pending(), 1);
        // A peer that is gone has nothing unread.
        drop(channel1);
        assert_eq!(channel0.peer_pending(), 0);
    }

    /// `check_and_read` exists so that a read whose check fails **keeps** the
    /// message: that is the whole difference between it and `read`, and it is
    /// how `zx_channel_read` refuses a buffer that is too small without
    /// destroying what it could not hand over. Every test in this file went
    /// through `read`, which passes a check that never fails, so throwing the
    /// check's error away and eating the message passed green.
    #[test]
    fn a_read_whose_check_fails_leaves_the_message_where_it_was() {
        let (channel0, channel1) = Channel::create();
        channel0.write(message(b"keep me")).unwrap();

        // The check sees the message it is being asked about, and its refusal
        // is what comes back.
        let refused = channel1.check_and_read(|msg| {
            assert_eq!(msg.data.as_slice(), b"keep me");
            Err(ZxError::BUFFER_TOO_SMALL)
        });
        assert_eq!(refused.err(), Some(ZxError::BUFFER_TOO_SMALL));

        // And the message is still there, still readable, still the same one.
        assert_eq!(
            channel1.read().unwrap().data.as_slice(),
            b"keep me",
            "the refused read ate the message anyway"
        );
        assert_eq!(channel1.read().err(), Some(ZxError::SHOULD_WAIT));
    }

    /// A channel is a queue, and nothing here said so: every test wrote one
    /// message per direction, so a queue that handed messages back in the
    /// order it was emptied -- newest first -- passed all of them.
    #[test]
    fn messages_come_back_in_the_order_they_were_written() {
        let (channel0, channel1) = Channel::create();
        for i in 0..4u8 {
            channel0.write(message(&[i])).unwrap();
        }
        for i in 0..4u8 {
            assert_eq!(
                channel1.read().unwrap().data.as_slice(),
                &[i],
                "the {}th message out is not the {}th that went in",
                i,
                i
            );
        }
        assert_eq!(channel1.read().err(), Some(ZxError::SHOULD_WAIT));
    }

    #[test]
    fn peer_closed() {
        let (channel0, channel1) = Channel::create();
        // write a message from peer, then drop it
        channel1.write(MessagePacket::default()).unwrap();
        drop(channel1);
        // read the first message should success.
        channel0.read().unwrap();
        // read more message should fail.
        assert_eq!(channel0.read().err(), Some(ZxError::PEER_CLOSED));
        // write message should fail.
        assert_eq!(
            channel0.write(MessagePacket::default()),
            Err(ZxError::PEER_CLOSED)
        );
    }

    #[test]
    fn signal() {
        let (channel0, channel1) = Channel::create();

        // initial status is writable and not readable.
        let init_signal = channel0.base.signal();
        assert!(!init_signal.contains(Signal::READABLE));
        assert!(init_signal.contains(Signal::WRITABLE));

        // register callback for `Signal::READABLE` & `Signal::PEER_CLOSED`:
        //   set `readable` and `peer_closed`
        let readable = Arc::new(AtomicBool::new(false));
        let peer_closed = Arc::new(AtomicBool::new(false));
        channel0.add_signal_callback(Box::new({
            let readable = readable.clone();
            let peer_closed = peer_closed.clone();
            move |signal| {
                readable.store(signal.contains(Signal::READABLE), Ordering::SeqCst);
                peer_closed.store(signal.contains(Signal::PEER_CLOSED), Ordering::SeqCst);
                false
            }
        }));

        // writing to peer should trigger `Signal::READABLE`.
        channel1.write(MessagePacket::default()).unwrap();
        assert!(readable.load(Ordering::SeqCst));

        // reading all messages should cause `Signal::READABLE` be cleared.
        channel0.read().unwrap();
        assert!(!readable.load(Ordering::SeqCst));

        // peer closed should trigger `Signal::PEER_CLOSED`.
        assert!(!peer_closed.load(Ordering::SeqCst));
        drop(channel1);
        assert!(peer_closed.load(Ordering::SeqCst));
    }

    #[async_std::test]
    async fn call() {
        let (channel0, channel1) = Channel::create();
        async_std::task::spawn({
            let channel1 = channel1.clone();
            async move {
                async_std::task::sleep(Duration::from_millis(10)).await;
                let recv_msg = channel1.read().unwrap();
                let txid = recv_msg.get_txid();
                assert_eq!(txid, 0x8000_0000);
                assert_eq!(txid.to_ne_bytes(), &recv_msg.data[..4]);
                assert_eq!(&recv_msg.data[4..], b"o 0");
                // write an irrelevant message
                channel1
                    .write(MessagePacket {
                        data: Vec::from("hello 1"),
                        handles: Vec::new(),
                    })
                    .unwrap();
                // reply the call
                let mut data: Vec<u8> = vec![];
                data.append(&mut txid.to_ne_bytes().to_vec());
                data.append(&mut Vec::from("hello 2"));
                channel1
                    .write(MessagePacket {
                        data,
                        handles: Vec::new(),
                    })
                    .unwrap();
            }
        });

        let recv_msg = channel0
            .call(MessagePacket {
                data: Vec::from("hello 0"),
                handles: Vec::new(),
            })
            .await
            .unwrap();
        let txid = recv_msg.get_txid();
        assert_eq!(txid, 0x8000_0000);
        assert_eq!(txid.to_ne_bytes(), &recv_msg.data[..4]);
        assert_eq!(&recv_msg.data[4..], b"hello 2");

        // peer dropped when calling
        let (channel0, channel1) = Channel::create();
        async_std::task::spawn({
            async move {
                async_std::task::sleep(Duration::from_millis(10)).await;
                drop(channel1);
            }
        });
        assert_eq!(
            channel0
                .call(MessagePacket {
                    data: Vec::from("hello 0"),
                    handles: Vec::new(),
                })
                .await
                .unwrap_err(),
            ZxError::PEER_CLOSED
        );
    }

    /// A writer whose peer never reads is not refused -- Zircon queues the
    /// message and raises a policy exception in the writer instead, which is
    /// the syscall layer's job -- but every write says how many the peer
    /// holds, so the one past `MAX_PENDING_MESSAGE_COUNT` can be told apart.
    /// The count is of the peer's unread messages: reading one makes room,
    /// and a reply a parked `call` takes is never pending.
    #[test]
    fn every_write_reports_how_many_the_peer_holds_unread() {
        let (channel0, channel1) = Channel::create();
        for n in 1..=MAX_PENDING_MESSAGE_COUNT {
            assert_eq!(channel0.write_counted(message(b"fill")).unwrap(), n);
        }
        assert_eq!(
            channel0.write_counted(message(b"one too many")).unwrap(),
            MAX_PENDING_MESSAGE_COUNT + 1,
            "the message past the limit is queued, not refused"
        );
        assert_eq!(channel1.read().unwrap().data, b"fill");
        assert_eq!(
            channel0.write_counted(message(b"room again")).unwrap(),
            MAX_PENDING_MESSAGE_COUNT + 1
        );

        let (caller, callee) = Channel::create();
        let mut call: Pin<Box<dyn Future<Output = ZxResult<MessagePacket>>>> =
            Box::pin(caller.call(message(b"call")));
        assert!(call
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        let request = callee.read().unwrap();
        assert_eq!(
            callee.write_counted(request).unwrap(),
            0,
            "a reply the parked call takes is not pending on the endpoint"
        );
    }

    /// A `call` is a write to the peer, and the one past the limit says so
    /// the same way: the count comes back before the reply is waited for,
    /// so the syscall layer can raise the policy exception that keeps a
    /// caller whose peer never answers out of the kernel heap.
    #[test]
    fn a_call_reports_how_many_the_peer_holds_unread_before_waiting() {
        let (caller, callee) = Channel::create();
        for _ in 0..MAX_PENDING_MESSAGE_COUNT {
            caller.write(message(b"fill")).unwrap();
        }
        let (pending, reply) = caller.call_counted(message(b"call")).unwrap();
        assert_eq!(
            pending,
            MAX_PENDING_MESSAGE_COUNT + 1,
            "the request past the limit is queued and counted, not refused"
        );
        let mut reply: Pin<Box<dyn Future<Output = ZxResult<MessagePacket>>>> = Box::pin(reply);
        assert!(reply
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        drop(reply);
        assert!(
            caller.call_reply.lock().is_empty(),
            "a call given up frees its reply slot"
        );
        assert_eq!(callee.read().unwrap().data, b"fill");
    }

    /// Every timed-out `zx_channel_call` used to leave its reply slot in the
    /// endpoint for good, and the reply that came late was handed to that
    /// dead slot and lost. A cancelled call frees its slot, and its late
    /// reply is an ordinary message the caller can read.
    #[test]
    fn a_cancelled_call_frees_its_slot_and_its_late_reply_is_an_ordinary_message() {
        let (channel0, channel1) = Channel::create();
        let call = call_in_flight(&channel0, b"txidping");
        assert_eq!(channel0.call_reply.lock().len(), 1);
        drop(call);
        assert!(
            channel0.call_reply.lock().is_empty(),
            "the deadline passed: the slot goes with the call"
        );
        let request = channel1.read().unwrap();
        let txid = request.get_txid();
        assert_eq!(&request.data[4..], b"ping");
        channel1.write(reply_to(txid, b"pong")).unwrap();
        let late = channel0.read().unwrap();
        assert_eq!(late.get_txid(), txid);
        assert_eq!(&late.data[4..], b"pong");
    }

    /// The other order, which is the one that lost the message: the reply
    /// arrives FIRST, so `write` finds the slot and hands it over, and only
    /// then does the call give up (its deadline, or a killed thread).
    ///
    /// The reply was then sitting inside a oneshot nobody would ever read,
    /// and dropping the slot dropped it with the receiver: `write` had
    /// answered `Ok`, the caller had been told nothing, and the endpoint
    /// held nothing. The test above passes either way, because there the
    /// slot is already gone when the reply is written.
    #[test]
    fn a_reply_that_beat_the_cancellation_is_still_left_on_the_endpoint() {
        let (channel0, channel1) = Channel::create();
        let call = call_in_flight(&channel0, b"txidping");
        let request = channel1.read().unwrap();
        let txid = request.get_txid();
        // The reply goes in while the call is still waiting, so it is handed
        // to the slot and not queued.
        channel1.write(reply_to(txid, b"pong")).unwrap();
        assert!(
            channel0.recv_queue.lock().is_empty(),
            "the reply went to the waiting call, not to the queue"
        );
        // And now the call gives up without ever being polled again.
        drop(call);
        assert!(
            channel0.call_reply.lock().is_empty(),
            "the slot goes with the call"
        );
        let late = channel0.read().expect("the reply is on the endpoint");
        assert_eq!(late.get_txid(), txid);
        assert_eq!(&late.data[4..], b"pong");
    }

    /// A reply carrying a txid nobody is waiting for -- a call long gone, or
    /// one that never was -- is an ordinary message, not dropped.
    ///
    /// (The oneshot this table replaced had a window where the writer held
    /// the sender of a call being cancelled; every state change now happens
    /// under the table's lock, so that window does not exist.)
    #[test]
    fn a_reply_nobody_waits_for_is_queued_instead_of_dropped() {
        let (channel0, channel1) = Channel::create();
        channel1.write(reply_to(0x8000_0001, b"pong")).unwrap();
        let late = channel0.read().expect("the reply is on the endpoint");
        assert_eq!(late.get_txid(), 0x8000_0001);
        assert_eq!(&late.data[4..], b"pong");
    }

    /// A second reply for the same txid, after the first was handed to the
    /// call but before the call took it, is not swallowed either.
    #[test]
    fn a_second_reply_for_the_same_call_is_queued() {
        let (channel0, channel1) = Channel::create();
        let mut call = call_in_flight(&channel0, b"txidping");
        let txid = channel1.read().unwrap().get_txid();
        channel1.write(reply_to(txid, b"one")).unwrap();
        channel1.write(reply_to(txid, b"two")).unwrap();
        match call.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(Ok(reply)) => assert_eq!(&reply.data[4..], b"one"),
            _ => panic!("the call did not get the first reply"),
        }
        assert_eq!(&channel0.read().unwrap().data[4..], b"two");
    }

    /// A reply already handed to the call survives the peer closing before
    /// the call takes it.
    #[test]
    fn a_delivered_reply_survives_the_peer_closing() {
        let (channel0, channel1) = Channel::create();
        let mut call = call_in_flight(&channel0, b"txidping");
        let txid = channel1.read().unwrap().get_txid();
        channel1.write(reply_to(txid, b"pong")).unwrap();
        drop(channel1);
        match call.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(Ok(reply)) => assert_eq!(&reply.data[4..], b"pong"),
            _ => panic!("the delivered reply was lost"),
        }
    }

    /// A call whose peer goes away while it waits is woken with
    /// `PEER_CLOSED`, not left waiting.
    #[test]
    fn a_waiting_call_is_woken_when_the_peer_closes() {
        extern crate std;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc as StdArc;
        use std::task::Wake;
        struct Flag(AtomicBool);
        impl Wake for Flag {
            fn wake(self: StdArc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flag = StdArc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(flag.clone());
        let (channel0, channel1) = Channel::create();
        let mut call: Pin<Box<dyn Future<Output = ZxResult<MessagePacket>>>> =
            Box::pin(channel0.call(message(b"txidping")));
        assert!(call
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        drop(channel1);
        assert!(flag.0.load(Ordering::SeqCst), "the waiting call was woken");
        match call.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(Err(ZxError::PEER_CLOSED)) => {}
            _ => panic!("expected PEER_CLOSED"),
        }
    }

    /// The txid counter wraps inside the kernel's half. It used to run off
    /// the end of `u32` into 0, and a reply with txid 0 is an ordinary
    /// message, so the call that drew it would have waited for ever.
    #[test]
    fn txids_wrap_inside_the_kernel_range() {
        let (channel0, channel1) = Channel::create();
        channel0.next_txid.store(u32::MAX, Ordering::SeqCst);
        let last = call_in_flight(&channel0, b"last");
        assert_eq!(channel1.read().unwrap().get_txid(), u32::MAX);
        drop(last);
        let mut wrapped = call_in_flight(&channel0, b"wrapped");
        let request = channel1.read().unwrap();
        assert_eq!(
            request.get_txid(),
            0x8000_0000,
            "back to the first kernel txid, never 0"
        );
        channel1.write(reply_to(request.get_txid(), b"ok")).unwrap();
        match wrapped
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(Ok(reply)) => assert_eq!(&reply.data[4..], b"ok"),
            other => panic!(
                "the wrapped call did not get its reply: {:?}",
                other.map(|r| r.map(|_| ()))
            ),
        }
        assert!(channel0.call_reply.lock().is_empty());
    }
}
