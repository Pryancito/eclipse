//! The kernel transport: [`RpcTransport`] over one of the kernel's own TCP
//! sockets, driven **synchronously**.
//!
//! Everything below this file is a pure function from bytes to bytes. This is
//! where the bytes start moving, and the problem it solves is not the protocol
//! but a mismatch in the kernel:
//!
//! - the VFS methods a filesystem must implement (`INode::read_at`, `find`)
//!   are **synchronous**;
//! - the kernel's `Socket` trait is **`async`**;
//! - and on bare metal there is no `block_on` to bridge them. The tree's
//!   `async_std::task::block_on` calls are all tests or libos.
//!
//! What makes it work anyway is that `TcpSocketState::read`
//! (`linux-object/src/net/tcp.rs`) is already *almost* synchronous: its loop
//! drives the NIC by hand — [`drain_deferred_jobs`](kernel_hal::deferred_job::drain_deferred_jobs)
//! then [`drain_net_urgent`](crate::net::drain_net_urgent) — and uses its
//! `.await` only to park between attempts. Replace that one park with
//! [`kernel_hal::thread::yield_now_sync`] and the same loop runs from a
//! synchronous caller, with no change to the executor and no second code path
//! through the stack.
//!
//! # Why this talks to smoltcp and not to `Socket`
//!
//! Because the `Socket` methods are the `async` ones. Going through them would
//! need exactly the `block_on` that does not exist here. The socket set is the
//! same one every other socket in the kernel lives in — this is not a private
//! network stack — so a handle registered here is visible to `netstat`, counts
//! against the socket limit, and is polled by the same interface.
//!
//! # The three things a transport must not get wrong
//!
//! **A partial write is not a short write.** [`RpcTransport::send`] promises
//! *all* of the bytes, because a record header that went out without its
//! payload leaves the server waiting for bytes the client believes it sent.
//! That is a hang, not an error, and it is invisible from the client's side —
//! so the loop here is the thing that makes the rest of the client's timeout
//! logic meaningful.
//!
//! **Nothing-yet is not end-of-stream.** `recv` returns `Ok(0)` when no bytes
//! have arrived, which is the common case, and
//! [`TransportError::Closed`](super::client::TransportError::Closed) only when
//! the peer really closed. Collapsing the two makes every momentarily-empty
//! socket look like a server that hung up, which the client would answer by
//! tearing down a connection that was fine.
//!
//! **A park that does not park burns a core.** The client polls, so `poll` is
//! called between every read attempt. It drives the NIC and then yields, and
//! the yield is allowed to do nothing (see
//! [`yield_now_sync`](kernel_hal::thread::yield_now_sync)) — which is why the
//! caller is a loop with a deadline rather than code that assumes time passed.

use alloc::vec::Vec;

use smoltcp::socket::{SocketHandle, TcpSocket};

use zcore_drivers::net::get_sockets;

use crate::net::GlobalSocketHandle;

use super::client::{RpcTransport, TransportError};

/// How long [`KernelTransport::send`] will keep trying to hand bytes to a
/// socket whose send buffer stays full.
///
/// A full send buffer means the peer is not reading, or the window is shut.
/// Waiting forever would hold the thread that drives the network stack, so
/// there is a cap; it is generous because exceeding it tears the connection
/// down, and a slow server is not a broken one.
pub const SEND_TIMEOUT_MS: u64 = 60_000;

/// The most consecutive passes [`KernelTransport::send`] will make **without
/// the clock advancing at all**.
///
/// Only passes over a frozen clock count, and the counter is reset the moment
/// time moves. That distinction is the whole point of the bound and the first
/// version of it got it wrong: counting every stalled pass made this a second,
/// shorter deadline, because at the 1 ms tight park ten thousand parks are ten
/// seconds while the real deadline is sixty — so a merely slow peer was torn
/// down six times sooner than the timeout said, and the timeout was the
/// documented limit. Counting only frozen passes leaves the deadline as the
/// one limit on patience and leaves this as what it was for: an exit for a
/// clock that has stopped, since a loop whose only exit is the clock does not
/// terminate without one, and this loop runs on the thread that drives the
/// network stack.
pub const MAX_SEND_STALLS: u32 = 10_000;

/// The most passes [`KernelTransport::poll`] will make round its park loop,
/// however little the clock has moved.
///
/// Generous enough that it never ends a park early on a working clock — a
/// yield plus two drains is nowhere near a microsecond, so five thousand of
/// them is well past the 5 ms longest park — and finite so that a clock which
/// stops cannot wedge the thread that drives the network stack.
pub const MAX_PARK_PASSES: u32 = 5_000;

/// The most consecutive turns [`recv_at_least`] will take **without the clock
/// advancing at all**.
///
/// The same exit, one level up. Each turn is a read plus a park, so on a
/// working clock the deadline ends the wait long before this does, and the
/// counter resets whenever time moves. Without it a bounded park just returns
/// into an unbounded loop, which is not a bound.
pub const MAX_RECV_PASSES: u32 = 5_000;

/// Consecutive passes of a loop that the clock did not move through.
///
/// Every bounded loop in this file has the same two exits: a deadline, which
/// is the real limit on patience, and this, which is the exit for a clock
/// that has stopped — because a loop whose only exit is the clock does not
/// terminate without one, and these loops run on the thread that drives the
/// network stack.
///
/// # Why the clock reading outlives the pass
///
/// The first version sampled the clock twice inside one pass and compared
/// those two readings. That only sees time that passes *between the samples*,
/// so time passing anywhere else in the loop — in the parts of a pass that
/// are not being timed, or at a granularity coarser than one pass — looked
/// like a frozen clock. A server dribbling eight bytes a read into a 64 KiB
/// reply makes thousands of fast passes, each too short to move a
/// millisecond clock, and the bound would fire on a connection whose deadline
/// was nowhere near: a working clock must not be able to trigger the
/// stopped-clock exit. So the newest reading is kept *across* passes as a
/// high-water mark, and any advance against it — wherever it happened —
/// resets the count.
struct StallCount {
    /// The newest clock reading seen so far, from any pass.
    seen: u64,
    /// Passes since that reading changed.
    passes: u32,
}

impl StallCount {
    fn new(now: u64) -> Self {
        Self {
            seen: now,
            passes: 0,
        }
    }

    /// Note real progress — bytes moved — which is not a stall whatever the
    /// clock says.
    fn progress(&mut self, now: u64) {
        if now > self.seen {
            self.seen = now;
        }
        self.passes = 0;
    }

    /// One pass, with the clock as it read before and after the work the pass
    /// did. True when the loop has run out of passes and must give up.
    fn turn(&mut self, before: u64, after: u64, limit: u32) -> bool {
        let newest = if before > after { before } else { after };
        if newest > self.seen {
            self.seen = newest;
            self.passes = 0;
            return false;
        }
        self.passes += 1;
        self.passes >= limit
    }
}

/// How long to park between read attempts, by how many came back empty.
///
/// Lifted from `tcp.rs`'s own policy, and for its reason: parking loosely
/// while a transfer is active stretches the ACK clock, which caps throughput
/// at roughly one window per park and stops the peer's congestion window from
/// recovering. So park tight while something is arriving — during a real
/// transfer the next attempt usually has data, so the count stays low — and
/// relax only once the socket has genuinely gone quiet.
#[inline]
pub fn park_ms(consecutive_empty: u32) -> u64 {
    match consecutive_empty {
        0..=7 => 1,
        8..=31 => 2,
        _ => 5,
    }
}

/// What a smoltcp error means for a *read*, given whether the receive half is
/// still open.
///
/// `Exhausted` is the one that must not become an error: it means the receive
/// buffer is empty right now. Whether that is "nothing yet" or "end of
/// stream" is not in the error — it is in `may_recv()`, which stays true while
/// the peer could still send. Deciding end-of-stream from the error alone is
/// how a client truncates a transfer the instant a read lands between two
/// segments.
#[inline]
pub fn classify_read(err: smoltcp::Error, may_recv: bool) -> Result<usize, TransportError> {
    match err {
        smoltcp::Error::Exhausted if may_recv => Ok(0),
        // The buffer is empty and the peer cannot send again: a real EOF.
        smoltcp::Error::Exhausted => Err(TransportError::Closed),
        smoltcp::Error::Finished => Err(TransportError::Closed),
        // `Illegal` on a read is a socket that is not in a state to receive —
        // torn down by a reset, or never connected.
        smoltcp::Error::Illegal => Err(TransportError::Reset),
        _ => Err(TransportError::Other),
    }
}

/// What a smoltcp error means for a *write*.
///
/// There is no "nothing yet" on this side: a send that could not take bytes
/// has a full buffer, which the caller handles by waiting, and that case never
/// reaches here because it comes back as a short count rather than an error.
#[inline]
pub fn classify_write(err: smoltcp::Error) -> TransportError {
    match err {
        smoltcp::Error::Illegal | smoltcp::Error::Finished => TransportError::Closed,
        _ => TransportError::Other,
    }
}

/// What drives the network stack so queued bytes go out and arrived bytes
/// land in a socket.
///
/// A seam with one purpose: on the machine this is the NIC, and the host has
/// no NIC. [`KernelTransport::send`]'s loop over partial writes only runs when
/// the send buffer fills, which needs a payload bigger than 512 KiB *and*
/// something draining the far end while the loop turns — and on the host the
/// only thing that can move bytes is the test's own loopback interface.
/// Without this, the most important loop in the file is unreachable from a
/// test, which is exactly where a bug would sit unseen.
pub trait StackDriver {
    /// Push what is queued and pull what has arrived.
    fn drive(&mut self);
}

/// The real one: the kernel's two drains.
///
/// The *unthrottled* drain, for the reason `tcp.rs` gives for using it in a
/// blocking read: the throttled tick runs every few milliseconds, which is
/// long enough for a fast link to overflow the NIC's receive ring before
/// anything empties it. A call actively waiting for a reply must pull as fast
/// as the data arrives.
#[derive(Default)]
pub struct KernelDriver;

impl StackDriver for KernelDriver {
    fn drive(&mut self) {
        kernel_hal::deferred_job::drain_deferred_jobs();
        crate::net::drain_net_urgent();
    }
}

/// An [`RpcTransport`] over one kernel TCP socket.
///
/// The handle is held raw, as `tcp.rs` holds it, rather than as a
/// `TcpSocketState`: the state object's read and write are the `async` ones
/// this file exists to avoid.
pub struct KernelTransport<D: StackDriver = KernelDriver> {
    /// The socket, owned: this wrapper is reference counted and its `Drop`
    /// releases the set's slot, so holding it here is what keeps the
    /// connection alive for as long as the mount uses it -- and what frees it
    /// when the mount goes away.
    handle: GlobalSocketHandle,
    /// Consecutive read attempts that found nothing, which sets the park
    /// length. Reset by any byte, so a busy connection never relaxes.
    empty_polls: u32,
    /// What moves the bytes. [`KernelDriver`] everywhere but in the tests.
    driver: D,
    /// How long [`Self::send`] keeps trying before giving the connection up.
    ///
    /// A field rather than a constant so a test can ask for the giving-up
    /// behaviour without waiting a minute for it. The default is
    /// [`SEND_TIMEOUT_MS`].
    send_timeout_ms: u64,
    /// The stall bound in force. See [`MAX_SEND_STALLS`].
    ///
    /// A field for the same reason `send_timeout_ms` is one: the bound is
    /// deliberately wide enough that a test cannot reach it in a suite, and a
    /// bound nothing can reach is a bound nothing measures.
    max_send_stalls: u32,
    /// Whether this connection has been given up on, and why it had to be.
    ///
    /// A `send` that fails having already handed *part* of a record to the
    /// socket leaves the server waiting for the rest of a message. The client
    /// above only sees an error and keeps the transport, so its next call
    /// writes a record header where the server expects the previous payload:
    /// not a lost call but a stream out of step, surfacing in some later call
    /// that had nothing to do with it.
    ///
    /// Half a record cannot be withdrawn from the server's buffer, so the
    /// only honest repair is to stop using the connection. Once this is set
    /// the socket has been aborted and every operation answers
    /// [`TransportError::NotConnected`].
    poisoned: bool,
}

// Nothing in the kernel builds a transport yet: the mount path that will is
// the next batch, and until then the only callers are the tests. The `allow`
// is on the constructors rather than on the module, so anything else here
// that falls out of use is still reported.
#[allow(dead_code)]
impl KernelTransport<KernelDriver> {
    /// Wrap an already-connected socket, driving the real stack.
    pub(crate) fn new(handle: GlobalSocketHandle) -> Self {
        Self::with_driver(handle, KernelDriver)
    }
}

impl<D: StackDriver> KernelTransport<D> {
    /// Wrap an already-connected socket handle.
    ///
    /// Connecting is not done here. A connect is a wait of its own with its
    /// own timeout and its own error mapping, and the socket this is handed
    /// may be a reconnection of an existing mount rather than a fresh one.
    #[allow(dead_code)]
    pub(crate) fn with_driver(handle: GlobalSocketHandle, driver: D) -> Self {
        Self {
            handle,
            empty_polls: 0,
            driver,
            send_timeout_ms: SEND_TIMEOUT_MS,
            max_send_stalls: MAX_SEND_STALLS,
            poisoned: false,
        }
    }

    /// Shorten the send timeout. For the test that asks what happens when a
    /// peer never reads; a minute is the right answer in the kernel and the
    /// wrong one in a suite.
    #[cfg(test)]
    pub(crate) fn with_send_timeout(mut self, ms: u64) -> Self {
        self.send_timeout_ms = ms;
        self
    }

    /// Lower the stall bound, so a test can show what it does and does not
    /// count. See [`MAX_SEND_STALLS`].
    #[cfg(test)]
    pub(crate) fn with_max_send_stalls(mut self, stalls: u32) -> Self {
        self.max_send_stalls = stalls;
        self
    }

    /// The smoltcp handle, for a caller that has to close or re-connect it.
    fn sock(&self) -> SocketHandle {
        self.handle.inner()
    }

    /// The handle, for a test that has to look at the socket the transport is
    /// holding. The transport owns it, so a test cannot keep its own copy.
    #[cfg(test)]
    pub(crate) fn sock_for_test(&self) -> SocketHandle {
        self.sock()
    }

    /// The driver, for a test that has to see what it drained or how often it
    /// was asked to run. The transport owns it, for the same reason.
    #[cfg(test)]
    pub(crate) fn driver_for_test(&self) -> &D {
        &self.driver
    }

    /// Whether this connection has been given up on. See
    /// [`Self::poisoned`].
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Give the connection up: abort the socket and refuse every later
    /// operation.
    ///
    /// `abort` and not `close`: a close is a FIN, which tells the server
    /// "that is all I have to say" about a message this client left half
    /// sent, and the server would wait out its own timeout on the rest. A
    /// reset says the connection is gone, which is true.
    fn poison(&mut self) {
        if self.poisoned {
            return;
        }
        self.poisoned = true;
        let sets = get_sockets();
        let mut sets = sets.lock();
        sets.get::<TcpSocket>(self.sock()).abort();
        drop(sets);
        // Push the reset out rather than leaving it queued behind the bytes
        // that caused this.
        self.drive();
    }

    /// The error to report for a failed `send`, giving the connection up
    /// first if part of the message had already gone out.
    ///
    /// `sent == 0` is an ordinary failure: nothing reached the server, so the
    /// connection is still in step and the caller may try again on it. Any
    /// other value left a partial record in the server's buffer, and from
    /// there the connection can only mislead -- see [`Self::poisoned`].
    fn give_up_if_partial(&mut self, sent: usize, err: TransportError) -> TransportError {
        if sent == 0 {
            return err;
        }
        self.poison();
        err
    }

    /// Drive the network stack so anything that has arrived is in the socket
    /// before it is read, and anything queued goes out.
    fn drive(&mut self) {
        self.driver.drive();
    }
}

impl<D: StackDriver> RpcTransport for KernelTransport<D> {
    fn send(&mut self, data: &[u8]) -> Result<(), TransportError> {
        if self.poisoned {
            return Err(TransportError::NotConnected);
        }
        let deadline = self.now_ms() + self.send_timeout_ms;
        let mut sent = 0;
        // Passes over a send buffer that took nothing **and a clock that did
        // not move**. The deadline is the real limit on patience; this is the
        // exit for a clock that has stopped, because a loop whose only exit
        // is the clock does not terminate without one -- and this one runs on
        // the thread that drives the network stack, so a stalled timer would
        // wedge the machine's networking and not just this mount.
        //
        // Counting *every* stalled pass, as the first version did, made this
        // a second and much shorter deadline: ten thousand 1 ms parks is ten
        // seconds against a documented sixty. So time advancing resets it,
        // and a merely slow peer is bounded by the deadline alone. See
        // [`StallCount`] for why the reading outlives the pass.
        let mut stalls = StallCount::new(self.now_ms());

        while sent < data.len() {
            self.drive();

            let taken = {
                let sets = get_sockets();
                let mut sets = sets.lock();
                let mut socket = sets.get::<TcpSocket>(self.sock());
                if !socket.may_send() {
                    // The send half is gone. Whatever is already queued will
                    // not be delivered, so reporting a partial success would
                    // be a lie the caller cannot detect. Reported outside the
                    // block, because giving the connection up takes the set.
                    None
                } else {
                    Some(socket.send_slice(&data[sent..]))
                }
            };
            let Some(taken) = taken else {
                return Err(self.give_up_if_partial(sent, TransportError::Closed));
            };

            match taken {
                Ok(0) => {
                    // The buffer is full: the peer is not reading, or its
                    // window is shut. Wait, having first pushed what is
                    // already queued out onto the wire.
                    let before = self.now_ms();
                    if before >= deadline {
                        return Err(self.give_up_if_partial(sent, TransportError::Other));
                    }
                    self.poll();
                    let after = self.now_ms();
                    if stalls.turn(before, after, self.max_send_stalls) {
                        return Err(self.give_up_if_partial(sent, TransportError::Other));
                    }
                }
                Ok(n) => {
                    sent += n;
                    stalls.progress(self.now_ms());
                    // Push it out now rather than waiting for the next tick:
                    // the client is about to start waiting for the reply to
                    // bytes still sitting in a buffer.
                    self.drive();
                }
                Err(e) => return Err(self.give_up_if_partial(sent, classify_write(e))),
            }
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.poisoned {
            return Err(TransportError::NotConnected);
        }
        self.drive();

        let (taken, may_recv) = {
            let sets = get_sockets();
            let mut sets = sets.lock();
            let mut socket = sets.get::<TcpSocket>(self.sock());
            let may_recv = socket.may_recv();
            let taken = socket.recv_slice(buf);
            (taken, may_recv)
        };

        match taken {
            // A zero-length read of a non-empty buffer is "nothing yet" and
            // smoltcp reports it as a count, not as `Exhausted` — so it is
            // turned into the same `Exhausted` the classifier understands,
            // rather than being mistaken for the end of the stream.
            Ok(0) if !buf.is_empty() => {
                let out = classify_read(smoltcp::Error::Exhausted, may_recv);
                if matches!(out, Ok(0)) {
                    self.empty_polls = self.empty_polls.saturating_add(1);
                }
                out
            }
            Ok(n) => {
                self.empty_polls = 0;
                Ok(n)
            }
            Err(e) => classify_read(e, may_recv),
        }
    }

    fn now_ms(&self) -> u64 {
        kernel_hal::timer::timer_now().as_millis() as u64
    }

    fn give_up(&mut self) {
        self.poison();
    }

    fn poll(&mut self) {
        self.drive();
        // Hand the CPU over for roughly the park this many empty reads asks
        // for. The yield is allowed to return at once without parking, so
        // this is a bound on patience and not a sleep: the caller's loop has
        // the deadline that actually ends the wait.
        //
        // The pass count is a second bound, and not a belt-and-braces one. A
        // loop whose only exit is the clock does not terminate if the clock
        // stops, and this loop runs on the thread that drives the network
        // stack -- so a stalled timer would not merely hang this mount, it
        // would take the machine's networking with it. Whatever the clock
        // says, this returns, and the caller is a loop that can come back.
        let until = self.now_ms() + park_ms(self.empty_polls);
        let mut stalls = StallCount::new(self.now_ms());
        while self.now_ms() < until {
            let before = self.now_ms();
            kernel_hal::thread::yield_now_sync();
            self.drive();
            let after = self.now_ms();
            if stalls.turn(before, after, MAX_PARK_PASSES) {
                return;
            }
        }
    }
}

/// Read a whole RPC record's worth of bytes, for a caller that wants the
/// transport's loop without the client's state machine.
///
/// Exists for the tests and for `MOUNT`, which is one call-and-reply before
/// there is a mount to hang a client off.
///
/// # The deadline is checked every turn, not only on an empty read
///
/// Checking it only when `recv` returned nothing is a deadline a dribbling
/// server walks straight past: a byte at a time keeps taking the other arm,
/// and if the message eventually completes this returns **success** long
/// after the caller's deadline expired. That is worse than a missed timeout,
/// because a caller that set a deadline is told it was honoured.
/// [`RpcClient::wait_for_reply`](super::client::RpcClient) checks it every
/// turn, and so does this.
///
/// # The deadline bounds the waiting, not when the bytes may have arrived
///
/// It is checked at the top of a turn and not again after the read that
/// completes the record, so a record whose last chunk lands late is still
/// returned. That is deliberate. The deadline exists to stop this waiting,
/// and a *complete* reply that arrived late is not something waiting longer
/// would improve: throwing it away costs a retransmit of a call the server
/// has already executed, and NFSv3 has procedures that are not idempotent
/// (`CREATE` with `GUARDED`, `REMOVE`, `RENAME`). Linux's RPC does the same —
/// the timer requeues the task for retransmission, but a reply that arrives
/// is matched by its xid and processed. So the deadline is honoured to within
/// one read, and the dribbling server that never completes a record still
/// dies at the top of the turn that starts past it.
///
/// # And a bound for the clock that stops
///
/// The park inside [`KernelTransport::poll`] is bounded, but a bounded park
/// that returns into an unbounded loop is not a bound at all: with a frozen
/// clock, `now_ms() >= deadline_ms` is false forever and this spins. So the
/// turns that the clock does not move are counted here too, and running out
/// of them is a terminal transport error rather than another turn.
pub fn recv_at_least<T: RpcTransport>(
    transport: &mut T,
    want: usize,
    deadline_ms: u64,
) -> Result<Vec<u8>, TransportError> {
    let mut out = Vec::with_capacity(want);
    let mut chunk = [0u8; 512];
    let mut stalls = StallCount::new(transport.now_ms());
    while out.len() < want {
        let before = transport.now_ms();
        if before >= deadline_ms {
            return Err(abandon(transport, out.len(), TransportError::Other));
        }
        let room = core::cmp::min(chunk.len(), want - out.len());
        let taken = match transport.recv(&mut chunk[..room]) {
            Ok(n) => n,
            Err(e) => return Err(abandon(transport, out.len(), e)),
        };
        match taken {
            0 => transport.poll(),
            n => out.extend_from_slice(&chunk[..n]),
        }
        let after = transport.now_ms();
        if taken != 0 {
            stalls.progress(after);
        } else if stalls.turn(before, after, MAX_RECV_PASSES) {
            return Err(abandon(transport, out.len(), TransportError::Other));
        }
    }
    Ok(out)
}

/// The error to report when [`recv_at_least`] walks away from a record it
/// had only part of.
///
/// Giving up partway through a record is the receive side of the partial
/// `send`: the bytes already taken are gone from the socket, so the next read
/// on this connection starts in the middle of a message and reads a length
/// field out of somebody's payload. The stream cannot be put back, so the
/// connection is given up on — which [`KernelTransport`] does by aborting the
/// socket, and the fakes do not need to do at all.
///
/// Nothing read means nothing lost: the connection is still in step and the
/// caller may try again on it.
fn abandon<T: RpcTransport>(transport: &mut T, read: usize, err: TransportError) -> TransportError {
    if read != 0 {
        transport.give_up();
    }
    err
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod transport_tests;
