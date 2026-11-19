//! What every transport of this client shares: how long a send waits, how a
//! wait parks, the exit for a clock that has stopped, and a read of a whole
//! record's worth of bytes.
//!
//! The transport itself is [`ServedTransport`](super::served::ServedTransport):
//! a stream socket of the network stack that runs as a process, driven
//! **synchronously**, because the VFS methods a filesystem implements
//! (`INode::read_at`, `find`) are synchronous and on bare metal there is no
//! `block_on`. The kernel used to have a stack of its own and a transport
//! over it here; the rules below were written for that one and hold for
//! any.
//!
//! # The three things a transport must not get wrong
//!
//! **A partial write is not a short write.** [`RpcTransport::send`] promises
//! *all* of the bytes, because a record header that went out without its
//! payload leaves the server waiting for bytes the client believes it sent.
//!
//! **Nothing-yet is not end-of-stream.** `recv` returns `Ok(0)` when no bytes
//! have arrived, which is the common case, and
//! [`TransportError::Closed`](super::client::TransportError::Closed) only when
//! the peer really closed.
//!
//! **A park that does not park burns a core.** The client polls, so `poll` is
//! called between every read attempt, and the caller is a loop with a
//! deadline rather than code that assumes time passed.

use alloc::vec::Vec;

use super::client::{RpcTransport, TransportError};

/// How long a transport's `send` will keep trying to hand bytes to a
/// socket whose send buffer stays full.
///
/// A full send buffer means the peer is not reading, or the window is shut.
/// Waiting forever would hold the thread that drives the network stack, so
/// there is a cap; it is generous because exceeding it tears the connection
/// down, and a slow server is not a broken one.
pub const SEND_TIMEOUT_MS: u64 = 60_000;

/// The most passes a transport's `poll` will make round its park loop,
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
/// The park inside a transport's `poll` is bounded, but a bounded park
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
/// connection is given up on — which a transport does by aborting its
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
