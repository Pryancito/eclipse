//! The RPC client: one call out, one reply back, with timeouts and retries.
//!
//! This is the layer between [`super::rpc`]'s pure codec and whatever moves
//! the bytes. It owns the three things a correct ONC RPC client over TCP has
//! to own, and each of them is a decision that is easy to get wrong in a way
//! that only shows up against a real server:
//!
//! - **The transaction ids.** One per call, and — importantly — *the same one
//!   across retransmits of that call*. See [`RpcClient::call`].
//! - **The waiting.** A reply is read out of a byte stream through
//!   [`RecordAssembler`](super::rpc::RecordAssembler), which means "wait for a
//!   reply" is a loop over partial reads and not a single `recv`.
//! - **The stale replies.** A call that timed out may still be answered, and
//!   that answer arrives in the middle of the *next* call's wait. Treating it
//!   as a protocol violation tears down a connection that is perfectly fine;
//!   see [`xid_is_older`].
//!
//! # Why a trait and not the kernel's socket
//!
//! [`RpcTransport`] is four methods wide so that everything above it is
//! testable on the host. That is not a gratuitous abstraction: the behaviours
//! worth testing here are *timing* and *framing* behaviours — a reply that
//! arrives in two pieces, a server that answers the call before last, a
//! server that goes silent — and none of them can be provoked reliably
//! against a real socket, in a test, in CI. Against a fake transport they are
//! each three lines.
//!
//! The kernel transport that drives the smoltcp socket is the batch after
//! this one. Nothing here knows about sockets.

use alloc::vec::Vec;

use super::rpc::{
    decode_reply, frame_message, peek_xid, CallTarget, Credential, RecordAssembler, RpcError,
};

/// What went wrong moving bytes, as opposed to what went wrong in the
/// protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    /// The connection is not up, or went down.
    NotConnected,
    /// The peer closed the connection.
    Closed,
    /// The peer reset the connection.
    Reset,
    /// Anything else the transport reports.
    Other,
}

/// Moves bytes for [`RpcClient`]. One implementation drives the kernel's TCP
/// socket; the other is the fake the tests are written against.
pub trait RpcTransport {
    /// Write all of `data` to the connection.
    ///
    /// All of it: a partial write is the transport's problem to loop over, not
    /// the client's. A record whose header went out without its payload leaves
    /// the server waiting for bytes that the client thinks it sent, which is
    /// a hang and not an error.
    fn send(&mut self, data: &[u8]) -> Result<(), TransportError>;

    /// Read whatever has arrived into `buf`, without waiting.
    ///
    /// `Ok(0)` means "nothing right now", NOT end of stream — the client
    /// polls, so "nothing yet" is the common case and must not be an error.
    /// A closed peer is [`TransportError::Closed`], which is a different
    /// thing and is why the two are not both zero.
    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;

    /// Milliseconds from some fixed point. Only differences are used, so the
    /// origin does not matter.
    fn now_ms(&self) -> u64;

    /// Let the stack make progress and, if it can, wait briefly for input.
    ///
    /// Called between read attempts. The kernel transport drives the NIC and
    /// parks here; the fake does nothing. A transport whose `poll` neither
    /// waits nor yields turns the client's wait into a busy loop, which is
    /// correct but burns a core — so the kernel one must park.
    fn poll(&mut self);
}

/// Whether `candidate` is an earlier transaction id than `current`.
///
/// Transaction ids increase by one per call and wrap, so "earlier" is a
/// comparison in the circle rather than on the line: the difference is
/// treated as earlier when it is less than half the space, which is the same
/// rule TCP uses for sequence numbers. A plain `<` would call every id
/// "later" than `current` for the whole second half of the counter's life,
/// and the symptom would be a client that tears down its connection once
/// after about two billion calls.
///
/// This is what lets a late reply be *discarded* rather than treated as a
/// protocol violation. A call that timed out may still be answered — the
/// server was slow, not broken — and that answer arrives in the middle of the
/// next call's wait. An id older than the one being waited for is such a
/// reply. An id *newer* is one that was never sent, which means the stream is
/// genuinely out of step.
pub fn xid_is_older(candidate: u32, current: u32) -> bool {
    candidate != current && current.wrapping_sub(candidate) < 0x8000_0000
}

/// How a call should behave when the server does not answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// How long to wait for a reply before retransmitting, in milliseconds.
    pub timeout_ms: u64,
    /// How many times to retransmit before giving up.
    ///
    /// Only consulted when [`Self::soft`] is set: a hard mount retries until
    /// the server answers.
    pub retrans: u32,
    /// Whether to give up at all. See
    /// [`NfsOptions::soft`](super::source::NfsOptions::soft) for why this
    /// kernel defaults to `true` where Linux defaults to `false`.
    pub soft: bool,
    /// An upper bound on how long one `call` may take, whatever `soft` and
    /// `retrans` say, in milliseconds.
    ///
    /// A hard mount has no retry limit, and in this kernel a VFS call holds a
    /// kernel thread that is driving the network stack itself, so "retry
    /// forever" would be a dead machine rather than a stuck process. This is
    /// the backstop that makes a hard mount merely very patient. Zero means
    /// no bound, which is what a caller that really wants Linux's behaviour
    /// asks for.
    pub deadline_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            timeout_ms: 60_000,
            retrans: 3,
            soft: true,
            deadline_ms: 10 * 60_000,
        }
    }
}

/// What went wrong in a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    /// The transport failed.
    Transport(TransportError),
    /// The reply did not decode, or the server refused the envelope.
    Rpc(RpcError),
    /// No reply after every retry the policy allowed.
    TimedOut,
    /// The peer closed the connection part-way through a message.
    ///
    /// Distinct from [`TransportError::Closed`] on purpose: a clean close
    /// between messages is an ordinary end of a connection, while one in the
    /// middle of a record means a reply was truncated. Reporting the second
    /// as the first is how a caller comes to treat a truncated answer as a
    /// complete one.
    TruncatedMessage,
}

impl From<TransportError> for CallError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}

impl From<RpcError> for CallError {
    fn from(e: RpcError) -> Self {
        Self::Rpc(e)
    }
}

/// How many bytes are read from the transport at a time.
///
/// Only a staging buffer: what it does not hold stays in the transport and
/// comes on the next read, so this bounds a stack-or-heap buffer and not a
/// message.
const READ_CHUNK: usize = 8 * 1024;

/// One RPC conversation: a transport, the transaction-id counter, and the
/// record assembler that turns the byte stream back into messages.
#[derive(Debug)]
pub struct RpcClient<T: RpcTransport> {
    transport: T,
    next_xid: u32,
    assembler: RecordAssembler,
    policy: RetryPolicy,
    /// Replies discarded for being answers to calls already given up on.
    ///
    /// Counted rather than logged per occurrence: against a slow server this
    /// happens in bursts, and a log line per stale reply is how a slow mount
    /// becomes a full disk. A non-zero count here means timeouts are being
    /// hit and the timeout is too short, which is worth knowing and is not
    /// worth a line each.
    stale_replies: u64,
}

impl<T: RpcTransport> RpcClient<T> {
    /// A client over `transport`, with transaction ids starting at `first_xid`.
    ///
    /// The starting id is the caller's to choose, and should differ between
    /// connections. It is not a security property — nothing here depends on an
    /// id being unguessable, and over TCP a reply has to arrive on the
    /// connection that sent the call — but starting every connection at the
    /// same number means a reply from a previous connection to the same
    /// server, arriving on a reused port, can carry an id this client is
    /// waiting for. Linux seeds it from the clock for the same reason.
    pub fn new(transport: T, first_xid: u32, policy: RetryPolicy) -> Self {
        Self {
            transport,
            next_xid: first_xid,
            assembler: RecordAssembler::new(),
            policy,
            stale_replies: 0,
        }
    }

    /// The transport, for a caller that has to ask it something this client
    /// does not model (whether it is still connected, say).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// The transport, mutably.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// How many replies have been discarded as answers to abandoned calls.
    ///
    /// Non-zero means the timeout is being hit. See the field's docs.
    pub fn stale_replies(&self) -> u64 {
        self.stale_replies
    }

    /// The retry policy in force.
    pub fn policy(&self) -> RetryPolicy {
        self.policy
    }

    /// Make one call and return the procedure's encoded results.
    ///
    /// # The transaction id is reused across retransmits
    ///
    /// A retransmit sends *the same bytes*, id included. That is not an
    /// optimisation, it is what makes a retry safe: a server keeps a cache of
    /// recently-answered transaction ids (Linux's `nfsd` reply cache), and a
    /// retransmit that matches one gets the cached answer back instead of
    /// being executed again. A client that allocated a fresh id per attempt
    /// would turn every timed-out non-idempotent call into two executions —
    /// two `MKDIR`s where the first succeeded and the reply was lost, so the
    /// second fails with `EEXIST` and the caller sees a mkdir that failed
    /// after it had worked. The same applies to `REMOVE` and to a `WRITE`
    /// whose offset was allocated by the server.
    ///
    /// So the id is allocated once, here, and every attempt in the retry loop
    /// below sends it again.
    pub fn call(
        &mut self,
        target: CallTarget,
        cred: &Credential,
        args: &[u8],
    ) -> Result<Vec<u8>, CallError> {
        let xid = self.next_xid;
        self.next_xid = self.next_xid.wrapping_add(1);

        let message = super::rpc::encode_call(xid, target, cred, args);
        let framed = frame_message(&message);

        let started = self.transport.now_ms();
        let mut attempt = 0u32;
        loop {
            self.transport.send(&framed)?;
            match self.wait_for_reply(xid)? {
                Some(results) => return Ok(results),
                None => {
                    // No reply inside the per-attempt timeout.
                    if self.policy.soft && attempt >= self.policy.retrans {
                        return Err(CallError::TimedOut);
                    }
                    if self.policy.deadline_ms != 0
                        && elapsed(started, self.transport.now_ms()) >= self.policy.deadline_ms
                    {
                        return Err(CallError::TimedOut);
                    }
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }

    /// Wait up to one timeout for the reply to `xid`.
    ///
    /// `Ok(None)` means the timeout ran out, which is the caller's signal to
    /// retransmit. Replies to calls already given up on are discarded here
    /// (see [`xid_is_older`]) rather than ending the wait.
    fn wait_for_reply(&mut self, xid: u32) -> Result<Option<Vec<u8>>, CallError> {
        let deadline_at = self.transport.now_ms();
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // Messages the assembler already holds come first. A `feed` can
            // complete more than one, and a message left buffered while the
            // client waits for bytes that already arrived is a deadlock
            // against a server that has answered.
            while let Some(message) = self.assembler.next_message()? {
                match peek_xid(&message) {
                    // The reply this call is waiting for.
                    Ok(got) if got == xid => {
                        return decode_reply(&message, xid)
                            .map(|body| Some(body.results.to_vec()))
                            .map_err(CallError::Rpc);
                    }
                    // An answer to a call already given up on: the server was
                    // slow, not broken. Discard it and keep waiting.
                    Ok(got) if xid_is_older(got, xid) => {
                        self.stale_replies = self.stale_replies.saturating_add(1);
                        continue;
                    }
                    // An id that was never sent: the stream is out of step and
                    // the connection cannot be used for anything else.
                    Ok(got) => {
                        return Err(CallError::Rpc(RpcError::XidMismatch { expected: xid, got }))
                    }
                    Err(e) => return Err(CallError::Rpc(e)),
                }
            }

            if elapsed(deadline_at, self.transport.now_ms()) >= self.policy.timeout_ms {
                return Ok(None);
            }

            self.transport.poll();
            match self.transport.recv(&mut buf) {
                Ok(0) => {}
                Ok(n) => self.assembler.feed(&buf[..n]),
                // A close between messages ends the connection; one in the
                // middle of a record truncated a reply, and the two must not
                // be reported as the same thing.
                Err(TransportError::Closed) if self.assembler.in_progress() => {
                    return Err(CallError::TruncatedMessage)
                }
                Err(e) => return Err(CallError::Transport(e)),
            }
        }
    }
}

/// Milliseconds from `then` to `now`, as a clock that wraps or that briefly
/// goes backwards would report it.
///
/// Zero rather than an enormous number when `now` is before `then`. A clock
/// that steps backwards is not hypothetical here — `timer_now` is read on
/// several CPUs — and a `now - then` that underflows produces a huge elapsed
/// time, which makes every call time out instantly: a mount that works and
/// then, once, fails everything.
fn elapsed(then: u64, now: u64) -> u64 {
    now.saturating_sub(then)
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
