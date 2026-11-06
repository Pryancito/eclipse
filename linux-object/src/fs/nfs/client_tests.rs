//! The RPC client, against a fake transport that can be made to misbehave in
//! the exact ways a real one does but cannot be asked to.
//!
//! The fake is the point of these tests. A server that answers the call
//! before last, one that goes silent for exactly one timeout, one that closes
//! half way through a record, a clock that steps backwards — each is a few
//! lines here and each is untestable against a real socket in CI.

use super::*;
use crate::fs::nfs::rpc::{
    frame_message, AuthUnix, CallTarget, Credential, RpcError, RpcRejection, MSG_TYPE_REPLY,
};
use crate::fs::nfs::xdr::{XdrDecoder, XdrEncoder};
use alloc::collections::VecDeque;
use alloc::vec;

/// A transport the test drives: it records what was sent, hands back what the
/// test queued, and has a clock the test moves.
#[derive(Debug, Default)]
struct FakeTransport {
    /// Everything the client has written, in order.
    sent: Vec<Vec<u8>>,
    /// Bytes still to be handed to the client, in the chunks `recv` will
    /// return them in. An empty chunk means "nothing this time", which is how
    /// a poll that found no data is modelled.
    to_deliver: VecDeque<Vec<u8>>,
    /// The clock, in milliseconds.
    now: u64,
    /// How much the clock advances on each `poll`.
    ///
    /// This is what makes a timeout reachable in a test without sleeping: the
    /// client's only way to pass time is to poll, so a fake that steps the
    /// clock there can make a timeout happen in however many polls the test
    /// wants.
    ms_per_poll: u64,
    /// An error for `recv` to return once `to_deliver` is empty.
    on_exhausted: Option<TransportError>,
    /// An error for the next `send` to fail with.
    send_error: Option<TransportError>,
    /// How many times `poll` has been called.
    polls: u32,
}

/// The most sends this fake will record before refusing.
///
/// A bound, not a behaviour: a change that makes the retry loop retransmit
/// without ever stopping would otherwise grow [`FakeTransport::sent`] until
/// the test process is killed for its memory, and a killed process names no
/// test — the same failure mode as a hung one. Refusing past the cap turns
/// that into an ordinary error and a named failure. No test here needs more
/// than a few hundred.
const MAX_FAKE_SENDS: usize = 10_000;

impl FakeTransport {
    fn new() -> Self {
        Self {
            ms_per_poll: 1,
            ..Default::default()
        }
    }

    /// Queue `data` to be delivered as one `recv`.
    fn deliver(&mut self, data: &[u8]) -> &mut Self {
        self.to_deliver.push_back(data.to_vec());
        self
    }

    /// Queue `data` to be delivered one byte per `recv`.
    fn deliver_byte_by_byte(&mut self, data: &[u8]) -> &mut Self {
        for b in data {
            self.to_deliver.push_back(vec![*b]);
        }
        self
    }

    /// Queue `count` reads that find nothing.
    fn deliver_nothing(&mut self, count: usize) -> &mut Self {
        for _ in 0..count {
            self.to_deliver.push_back(Vec::new());
        }
        self
    }

    /// The payload of the n-th thing sent, with its record header stripped.
    fn sent_message(&self, n: usize) -> &[u8] {
        &self.sent[n][4..]
    }
}

impl RpcTransport for FakeTransport {
    fn send(&mut self, data: &[u8]) -> Result<(), TransportError> {
        if let Some(e) = self.send_error.take() {
            return Err(e);
        }
        if self.sent.len() >= MAX_FAKE_SENDS {
            return Err(TransportError::Other);
        }
        self.sent.push(data.to_vec());
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        // Time passes on a read attempt too, not only in `poll`. With the
        // clock advancing only in `poll`, a change that stopped the client
        // polling left it spinning with a frozen clock that could never reach
        // a timeout, so the test hung instead of failing — and a hung test
        // takes the CI job down without naming itself.
        self.now = self.now.wrapping_add(self.ms_per_poll);
        match self.to_deliver.pop_front() {
            Some(chunk) => {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                // What did not fit goes back for the next read, which is what
                // a real transport does.
                if n < chunk.len() {
                    self.to_deliver.push_front(chunk[n..].to_vec());
                }
                Ok(n)
            }
            None => match self.on_exhausted {
                Some(e) => Err(e),
                None => Ok(0),
            },
        }
    }

    fn now_ms(&self) -> u64 {
        self.now
    }

    fn poll(&mut self) {
        self.polls = self.polls.saturating_add(1);
        self.now = self.now.wrapping_add(self.ms_per_poll);
    }
}

/// The bytes of an accepted, successful reply to `xid`.
fn reply(xid: u32, results: &[u8]) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(xid);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(0); // MSG_ACCEPTED
    enc.u32(0); // verifier flavour AUTH_NULL
    enc.opaque(&[]);
    enc.u32(0); // SUCCESS
    let mut out = enc.into_bytes();
    out.extend_from_slice(results);
    frame_message(&out)
}

fn a_target() -> CallTarget {
    CallTarget::new(100_003, 3, 1)
}

/// A policy that times out after ten polls of the fake clock, for the tests
/// that are about timing out.
fn quick_policy(retrans: u32, soft: bool) -> RetryPolicy {
    RetryPolicy {
        timeout_ms: 10,
        retrans,
        soft,
        deadline_ms: 0,
    }
}

/// A policy that waits far longer than any test here needs, for the tests
/// that are about something else.
///
/// Long, but **not** infinite, and the fake's clock always advances on a
/// poll. Earlier versions of these tests stopped the clock
/// (`ms_per_poll = 0`) so a slow delivery could not expire, and the cost was
/// that any change which stopped a reply from being recognised made the test
/// **hang** rather than fail — a hung test takes the CI job down with a
/// timeout instead of naming itself. A mutation run found three of them that
/// way. With the clock running and a generous timeout, the same change fails
/// with `TimedOut` and says which test it was.
fn patient_policy() -> RetryPolicy {
    RetryPolicy {
        timeout_ms: 1_000_000,
        retrans: 0,
        soft: true,
        deadline_ms: 0,
    }
}

// ------------------------------------------------------------- the easy path

#[test]
fn a_call_sends_a_framed_message_and_returns_the_results() {
    let mut t = FakeTransport::new();
    t.deliver(&reply(100, &[7, 7, 7, 7]));
    let mut c = RpcClient::new(t, 100, RetryPolicy::default());

    let got = c
        .call(a_target(), &Credential::Null, &[1, 2, 3, 4])
        .unwrap();
    assert_eq!(got, vec![7, 7, 7, 7]);
    assert_eq!(c.transport().sent.len(), 1, "one call, one send");
    assert_eq!(
        &c.transport().sent[0][..4],
        &[0x80, 0, 0, c.transport().sent[0].len() as u8 - 4],
        "the message goes out framed as one final record"
    );
}

#[test]
fn the_arguments_follow_the_envelope_in_the_message_that_is_sent() {
    let mut t = FakeTransport::new();
    t.deliver(&reply(5, &[]));
    let mut c = RpcClient::new(t, 5, RetryPolicy::default());
    c.call(a_target(), &Credential::Null, &[0xaa, 0xbb, 0xcc, 0xdd])
        .unwrap();

    let msg = c.transport().sent_message(0);
    assert_eq!(
        &msg[msg.len() - 4..],
        &[0xaa, 0xbb, 0xcc, 0xdd],
        "the arguments are the tail of the message"
    );
}

#[test]
fn transaction_ids_increase_by_one_per_call() {
    let mut t = FakeTransport::new();
    t.deliver(&reply(70, &[1])).deliver(&reply(71, &[2]));
    let mut c = RpcClient::new(t, 70, RetryPolicy::default());

    assert_eq!(c.call(a_target(), &Credential::Null, &[]).unwrap(), vec![1]);
    assert_eq!(c.call(a_target(), &Credential::Null, &[]).unwrap(), vec![2]);
}

/// A reply that arrives in pieces — which is what TCP does — must still be
/// one reply. The client's only way to notice more bytes is to poll, so this
/// also exercises the wait loop.
#[test]
fn a_reply_that_arrives_one_byte_at_a_time_is_still_one_reply() {
    let mut t = FakeTransport::new();
    t.deliver_byte_by_byte(&reply(1, &[4, 4, 4, 4]));
    let mut c = RpcClient::new(t, 1, patient_policy());

    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]).unwrap(),
        vec![4, 4, 4, 4]
    );
}

#[test]
fn reads_that_find_nothing_are_not_an_error() {
    let mut t = FakeTransport::new();
    t.deliver_nothing(50).deliver(&reply(1, &[9]));
    let mut c = RpcClient::new(t, 1, patient_policy());

    assert_eq!(c.call(a_target(), &Credential::Null, &[]).unwrap(), vec![9]);
    assert!(
        c.transport().polls >= 50,
        "the client polled while waiting, {} times",
        c.transport().polls
    );
}

#[test]
fn an_auth_unix_credential_reaches_the_wire() {
    let mut t = FakeTransport::new();
    t.deliver(&reply(1, &[]));
    let cred = Credential::Unix(AuthUnix::new(b"eclipse", 1000, 100));
    let mut c = RpcClient::new(t, 1, RetryPolicy::default());
    c.call(a_target(), &cred, &[]).unwrap();

    let msg = c.transport().sent_message(0);
    let mut dec = XdrDecoder::new(msg);
    for _ in 0..6 {
        dec.u32().unwrap();
    }
    assert_eq!(dec.u32(), Ok(1), "the credential flavour is AUTH_UNIX");
}

// ------------------------------------------------------- timeouts and retries

/// The property the server's duplicate-reply cache depends on: a retransmit
/// is the SAME bytes, transaction id included. A fresh id per attempt turns
/// every timed-out `MKDIR` into two executions, the second of which fails
/// with `EEXIST` after the first had succeeded.
#[test]
fn a_retransmit_sends_the_same_bytes_and_the_same_xid() {
    let mut t = FakeTransport::new();
    // Nothing until the timeout has fired once, then the reply. Eight empty
    // reads is one timeout's worth and a little over, so the second attempt
    // is the one that gets an answer.
    t.deliver_nothing(8).deliver(&reply(42, &[1]));
    let mut c = RpcClient::new(t, 42, quick_policy(3, true));

    assert_eq!(c.call(a_target(), &Credential::Null, &[]).unwrap(), vec![1]);
    let sent = &c.transport().sent;
    assert!(sent.len() >= 2, "the call was retransmitted");
    assert_eq!(
        sent[0], sent[1],
        "a retransmit must be byte-identical, so the server's reply cache matches it"
    );
}

#[test]
fn a_soft_call_gives_up_after_the_allowed_retries() {
    let t = FakeTransport::new();
    let mut c = RpcClient::new(t, 1, quick_policy(2, true));
    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]),
        Err(CallError::TimedOut)
    );
    assert_eq!(
        c.transport().sent.len(),
        3,
        "the first attempt plus two retransmits"
    );
}

#[test]
fn retrans_zero_sends_once_and_gives_up() {
    let t = FakeTransport::new();
    let mut c = RpcClient::new(t, 1, quick_policy(0, true));
    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]),
        Err(CallError::TimedOut)
    );
    assert_eq!(c.transport().sent.len(), 1);
}

/// A hard call does not give up for running out of retries — but it does stop
/// at the deadline, because in this kernel a VFS call that never returns
/// holds a thread that is driving the network stack.
#[test]
fn a_hard_call_ignores_retrans_but_stops_at_the_deadline() {
    let t = FakeTransport::new();
    let policy = RetryPolicy {
        timeout_ms: 10,
        retrans: 0,
        soft: false,
        deadline_ms: 100,
    };
    let mut c = RpcClient::new(t, 1, policy);
    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]),
        Err(CallError::TimedOut)
    );
    assert!(
        c.transport().sent.len() > 1,
        "a hard call retransmits past retrans=0, sent {}",
        c.transport().sent.len()
    );
}

/// `deadline_ms: 0` is "no bound", which is what a caller asking for Linux's
/// exact behaviour wants. Checked by showing a hard call with no deadline
/// keeps going far past where a bounded one stopped.
#[test]
fn a_hard_call_with_no_deadline_keeps_retransmitting() {
    let mut t = FakeTransport::new();
    // Answer eventually, so the test terminates: the point is that the client
    // was still trying long after any retrans limit would have stopped it.
    t.deliver_nothing(500).deliver(&reply(1, &[3]));
    let policy = RetryPolicy {
        timeout_ms: 10,
        retrans: 0,
        soft: false,
        deadline_ms: 0,
    };
    let mut c = RpcClient::new(t, 1, policy);
    assert_eq!(c.call(a_target(), &Credential::Null, &[]).unwrap(), vec![3]);
    assert!(
        c.transport().sent.len() > 10,
        "sent {} times",
        c.transport().sent.len()
    );
}

// -------------------------------------------------------------- stale replies

/// The case that makes a naive client tear down a healthy connection: a call
/// times out, the client moves on, and *then* the slow server answers the old
/// call. That answer arrives in the middle of the next call's wait.
#[test]
fn an_answer_to_a_call_already_given_up_on_is_discarded() {
    let mut t = FakeTransport::new();
    // The answer to xid 10 (given up on) arrives first, then the one to 11.
    t.deliver(&reply(10, &[0xaa])).deliver(&reply(11, &[0xbb]));
    let mut c = RpcClient::new(t, 11, patient_policy());

    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]).unwrap(),
        vec![0xbb],
        "the stale reply must not be returned as this call's answer"
    );
    assert_eq!(c.stale_replies(), 1, "and it must be counted");
}

#[test]
fn several_stale_replies_are_all_discarded_and_counted() {
    let mut t = FakeTransport::new();
    for xid in 1..=5u32 {
        t.deliver(&reply(xid, &[xid as u8]));
    }
    t.deliver(&reply(6, &[0xff]));
    let mut c = RpcClient::new(t, 6, patient_policy());

    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]).unwrap(),
        vec![0xff]
    );
    assert_eq!(c.stale_replies(), 5);
}

/// Why the wait loop drains the assembler with a `while` and not an `if`: a
/// `feed` can complete several messages, and each stale one discarded must
/// not cost a poll.
///
/// It is not just an idle poll. The fake's clock advances on every poll, and
/// a real transport's park takes real time, so a burst of stale replies —
/// exactly what a server that has fallen behind sends — would spend the
/// timeout on messages that were already in hand, and the call would expire
/// with its own answer sitting in the buffer. A mutation run caught this:
/// turning the `while` into an `if` changed no assertion in the suite.
#[test]
fn discarding_stale_replies_costs_no_polls() {
    let mut t = FakeTransport::new();
    // Twenty answers to abandoned calls and then ours, all in ONE read.
    let mut burst = Vec::new();
    for xid in 1..=20u32 {
        burst.extend_from_slice(&reply(xid, &[xid as u8]));
    }
    burst.extend_from_slice(&reply(21, &[0xff]));
    t.deliver(&burst);
    let mut c = RpcClient::new(t, 21, patient_policy());

    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]).unwrap(),
        vec![0xff]
    );
    assert_eq!(c.stale_replies(), 20);
    assert_eq!(
        c.transport().polls,
        1,
        "one poll fetched the whole burst; discarding the 20 stale replies must cost none"
    );
}

/// An id that was never sent is a different matter: the stream is out of step
/// and the connection cannot be used for anything else.
///
/// The clock is left running here on purpose, where most of these tests stop
/// it. A test whose clock does not move cannot time out, so an
/// implementation that *discarded* this reply instead of refusing it would
/// make this test hang rather than fail — and a hung test takes the whole CI
/// job down with a timeout instead of naming itself. With the clock running,
/// that implementation fails with `TimedOut` and says which test it was. (A
/// mutation run found this: the discarding variant hung the suite.)
#[test]
fn a_reply_for_an_id_never_sent_is_a_mismatch() {
    let mut t = FakeTransport::new();
    t.deliver(&reply(500, &[1]));
    let mut c = RpcClient::new(t, 10, quick_policy(0, true));

    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]),
        Err(CallError::Rpc(RpcError::XidMismatch {
            expected: 10,
            got: 500
        }))
    );
}

/// The "older" comparison is in the circle, not on the line. A plain `<`
/// would call every id later than the current one for the whole second half
/// of the counter's life.
#[test]
fn older_is_compared_in_the_circle_so_it_survives_the_wrap() {
    assert!(xid_is_older(9, 10));
    assert!(!xid_is_older(11, 10));
    assert!(!xid_is_older(10, 10), "an id is not older than itself");
    // Across the wrap: 0xffff_ffff comes just before 0 and 1.
    assert!(xid_is_older(0xffff_ffff, 0));
    assert!(xid_is_older(0xffff_fffe, 1));
    assert!(!xid_is_older(0, 0xffff_ffff));
    // Half the space away is the furthest "older" reaches.
    assert!(xid_is_older(1, 0x8000_0000));
    assert!(!xid_is_older(0x8000_0001, 1));
}

/// And the same across the wrap, through the client.
#[test]
fn a_stale_reply_is_discarded_across_the_counter_wrap() {
    let mut t = FakeTransport::new();
    t.deliver(&reply(0xffff_ffff, &[0xaa]))
        .deliver(&reply(0, &[0xbb]));
    let mut c = RpcClient::new(t, 0xffff_ffff, patient_policy());

    // First call takes the 0xffff_ffff reply, so the counter wraps to 0.
    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]).unwrap(),
        vec![0xaa]
    );
    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]).unwrap(),
        vec![0xbb],
        "the call after the wrap works"
    );
}

// -------------------------------------------------------- transport failures

#[test]
fn a_send_failure_is_reported_as_a_transport_error() {
    let mut t = FakeTransport::new();
    t.send_error = Some(TransportError::NotConnected);
    let mut c = RpcClient::new(t, 1, RetryPolicy::default());
    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]),
        Err(CallError::Transport(TransportError::NotConnected))
    );
}

#[test]
fn a_reset_while_waiting_is_reported() {
    let mut t = FakeTransport::new();
    t.on_exhausted = Some(TransportError::Reset);
    let mut c = RpcClient::new(t, 1, quick_policy(0, true));
    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]),
        Err(CallError::Transport(TransportError::Reset))
    );
}

/// A clean close between messages is an ordinary end of connection.
#[test]
fn a_close_between_messages_is_a_plain_close() {
    let mut t = FakeTransport::new();
    t.on_exhausted = Some(TransportError::Closed);
    let mut c = RpcClient::new(t, 1, quick_policy(0, true));
    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]),
        Err(CallError::Transport(TransportError::Closed))
    );
}

/// A close in the middle of a record truncated a reply, and reporting that as
/// a plain close is how a caller comes to treat a truncated answer as a
/// complete one.
#[test]
fn a_close_part_way_through_a_record_is_a_truncated_message() {
    let mut t = FakeTransport::new();
    let full = reply(1, &[1, 2, 3, 4]);
    t.deliver(&full[..6]); // the header and two bytes
    t.on_exhausted = Some(TransportError::Closed);
    let mut c = RpcClient::new(t, 1, quick_policy(0, true));

    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]),
        Err(CallError::TruncatedMessage)
    );
}

#[test]
fn a_refused_envelope_is_reported_as_an_rpc_error_not_a_timeout() {
    let mut enc = XdrEncoder::new();
    enc.u32(1).u32(MSG_TYPE_REPLY).u32(1).u32(1).u32(5);
    let denied = frame_message(&enc.into_bytes());

    let mut t = FakeTransport::new();
    t.deliver(&denied);
    let mut c = RpcClient::new(t, 1, quick_policy(0, true));

    assert_eq!(
        c.call(a_target(), &Credential::Null, &[]),
        Err(CallError::Rpc(RpcError::Rejected(RpcRejection::AuthError(
            5
        )))),
        "a refused credential must not look like a slow server"
    );
}

// ------------------------------------------------------------------ the clock

/// A clock that steps backwards must not make every call time out instantly.
/// `timer_now` is read on several CPUs here, so this is not hypothetical, and
/// an underflowing `now - then` produces a huge elapsed time.
#[test]
fn a_clock_that_steps_backwards_does_not_time_out_every_call() {
    assert_eq!(elapsed(100, 50), 0, "backwards is zero elapsed, not huge");
    assert_eq!(elapsed(50, 100), 50);
    assert_eq!(elapsed(0, 0), 0);
}

#[test]
fn a_call_whose_reply_is_already_buffered_needs_no_poll() {
    let mut t = FakeTransport::new();
    // Two replies in one read: the second call must find its answer in the
    // assembler without the transport being polled for more.
    let mut both = reply(1, &[1]);
    both.extend_from_slice(&reply(2, &[2]));
    t.deliver(&both);
    let mut c = RpcClient::new(t, 1, patient_policy());

    assert_eq!(c.call(a_target(), &Credential::Null, &[]).unwrap(), vec![1]);
    let polls_after_first = c.transport().polls;
    assert_eq!(c.call(a_target(), &Credential::Null, &[]).unwrap(), vec![2]);
    assert_eq!(
        c.transport().polls,
        polls_after_first,
        "the buffered reply was taken without another poll"
    );
}
