use {
    crate::object::*,
    alloc::collections::VecDeque,
    alloc::sync::{Arc, Weak},
    alloc::vec::Vec,
    core::convert::TryInto,
    core::sync::atomic::{AtomicU32, Ordering},
    futures::channel::oneshot::{self, Sender},
    hashbrown::HashMap,
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
    call_reply: Mutex<HashMap<TxID, Sender<ZxResult<T>>>>,
    next_txid: AtomicU32,
}

type T = MessagePacket;
type TxID = u32;

/// How many messages one endpoint holds unread before a writer is told to
/// wait: Zircon's `kMaxPendingMessageCount`. Without it a process that
/// nobody reads from could put the whole kernel heap into one channel.
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

    /// Write a packet to the channel
    pub fn write(&self, msg: T) -> ZxResult {
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        // check first 4 bytes: whether it is a call reply?
        let txid = msg.get_txid();
        // Out of the map before anything else is locked: `push_general` below
        // takes the peer's queue, and nothing may hold both.
        let waiting = if txid != 0 {
            peer.call_reply.lock().remove(&txid)
        } else {
            None
        };
        if let Some(sender) = waiting {
            // A reply the caller can no longer take is not lost. The send
            // fails when the call was cancelled between the lookup above and
            // here (`ReplySlot::drop` closes the receiving half), and Zircon
            // leaves a late reply on the endpoint as an ordinary message, so
            // that is where it goes. The answer used to be `let _ =`: the
            // message vanished and the endpoint never saw it.
            return match sender.send(Ok(msg)) {
                Ok(()) => Ok(()),
                // The value comes back exactly as it was sent.
                Err(Ok(msg)) => peer.push_general(msg),
                Err(Err(e)) => Err(e),
            };
        }
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
    pub async fn call(self: &Arc<Self>, mut msg: T) -> ZxResult<T> {
        assert!(msg.data.len() >= 4);
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        let txid = self.new_txid();
        msg.set_txid(txid);
        let (sender, receiver) = oneshot::channel();
        self.call_reply.lock().insert(txid, sender);
        let mut slot = ReplySlot {
            channel: self,
            txid,
            receiver: Some(receiver),
        };
        peer.push_general(msg)?;
        drop(peer);
        let reply = {
            let receiver = slot.receiver.as_mut().expect("just set");
            receiver.await.unwrap_or(Err(ZxError::INTERNAL))
        };
        // The reply is in hand, so there is nothing left for `Drop` to put
        // back on the endpoint.
        slot.receiver = None;
        drop(slot);
        reply
    }

    /// Push a message to general queue, called from peer.
    fn push_general(&self, msg: T) -> ZxResult {
        let mut send_queue = self.recv_queue.lock();
        if send_queue.len() >= MAX_PENDING_MESSAGE_COUNT {
            return Err(ZxError::SHOULD_WAIT);
        }
        send_queue.push_back(msg);
        if send_queue.len() == 1 {
            self.base.signal_set(Signal::READABLE);
        }
        Ok(())
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

/// The reply slot of one `call`, removed when the call ends however it
/// ends, so a cancelled call does not keep it.
struct ReplySlot<'a> {
    channel: &'a Channel,
    txid: TxID,
    /// The receiving half, held only so that a cancelled call can put back a
    /// reply that had already been handed over to it. `None` once the call
    /// has taken the reply itself.
    receiver: Option<oneshot::Receiver<ZxResult<T>>>,
}

impl Drop for ReplySlot<'_> {
    fn drop(&mut self) {
        self.channel.call_reply.lock().remove(&self.txid);
        let Some(mut receiver) = self.receiver.take() else {
            return;
        };
        // Closing first shuts the window the other way round: a writer that
        // took the sender out of the map but has not sent yet now fails, and
        // `write` queues its reply as an ordinary message.
        receiver.close();
        // And a reply that made it in before all that is put on the endpoint,
        // where Zircon leaves a late reply. Dropping the receiver with a
        // message inside it was a lost reply: the peer's `write` had answered
        // `Ok`, and the endpoint never held anything.
        if let Ok(Some(Ok(msg))) = receiver.try_recv() {
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
            for (_, sender) in core::mem::take(&mut *peer.call_reply.lock()).into_iter() {
                let _ = sender.send(Err(ZxError::PEER_CLOSED));
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

    /// A writer whose peer never reads used to grow the kernel heap without
    /// limit; the endpoint now holds `MAX_PENDING_MESSAGE_COUNT` unread and
    /// tells the writer to wait, for `write` and `call` alike, and a `call`
    /// that was refused leaves no reply slot behind.
    #[test]
    fn an_endpoint_holds_at_most_the_pending_limit_unread() {
        let (channel0, channel1) = Channel::create();
        for _ in 0..MAX_PENDING_MESSAGE_COUNT {
            channel0.write(message(b"fill")).unwrap();
        }
        assert_eq!(
            channel0.write(message(b"one too many")),
            Err(ZxError::SHOULD_WAIT)
        );
        let mut call: Pin<Box<dyn Future<Output = ZxResult<MessagePacket>>>> =
            Box::pin(channel0.call(message(b"call")));
        assert_eq!(
            call.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .map(|r| r.map(|_| ())),
            Poll::Ready(Err(ZxError::SHOULD_WAIT))
        );
        drop(call);
        assert!(
            channel0.call_reply.lock().is_empty(),
            "a refused call keeps no reply slot"
        );
        assert_eq!(channel1.read().unwrap().data, b"fill");
        channel0.write(message(b"room again")).unwrap();
        assert_eq!(channel1.recv_queue.lock().len(), MAX_PENDING_MESSAGE_COUNT);
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

    /// And the window in between: the writer has taken the sender out of the
    /// map but has not sent yet when the call gives up, so the send fails.
    /// `write` must not swallow that either -- it queues the reply as an
    /// ordinary message.
    ///
    /// The state is built by hand, because reaching it for real needs two
    /// threads: a slot in the map whose receiving half is already gone is
    /// exactly what the writer finds if it loses that race.
    #[test]
    fn a_reply_whose_send_finds_nobody_is_queued_instead_of_dropped() {
        let (channel0, channel1) = Channel::create();
        let txid = 0x8000_0001;
        let (sender, receiver) = oneshot::channel();
        channel0.call_reply.lock().insert(txid, sender);
        drop(receiver);
        // `write` finds the slot, the send fails, and the reply has to land
        // somewhere. It used to land nowhere and the call answered `Ok`.
        channel1.write(reply_to(txid, b"pong")).unwrap();
        let late = channel0.read().expect("the reply is on the endpoint");
        assert_eq!(late.get_txid(), txid);
        assert_eq!(&late.data[4..], b"pong");
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
