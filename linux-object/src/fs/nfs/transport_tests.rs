//! What the transports share, measured without one: the park, and
//! [`recv_at_least`] against a transport that is nothing but a clock and a
//! trickle.

use super::*;

/// The bound has to be wide enough never to end a park early on a working
/// clock.
const _: () = assert!(MAX_PARK_PASSES >= 1_000);

#[test]
fn the_park_stays_tight_while_data_is_arriving_and_relaxes_only_when_idle() {
    // TCP is self-clocked: the peer sends the next window only after it sees
    // an ACK, and the ACK comes out of this loop. A loose park while a
    // transfer is active caps throughput at about one window per park.
    assert_eq!(park_ms(0), 1);
    assert_eq!(park_ms(7), 1);
    assert_eq!(park_ms(8), 2);
    assert_eq!(park_ms(31), 2);
    assert_eq!(park_ms(32), 5);
    assert_eq!(park_ms(u32::MAX), 5);
    // Monotonic: a longer silence never parks for less.
    for n in 0..64u32 {
        assert!(park_ms(n) <= park_ms(n + 1), "n={}", n);
    }
}

/// A transport that is nothing but a clock and a trickle, for the two things
/// [`recv_at_least`] has to get right that a real socket cannot be made to do
/// on demand: a clock that has stopped, and a server that dribbles.
///
/// `recv_at_least` is generic over the trait, and these two behaviours are
/// a property of *its* loop.
struct Trickle {
    /// Milliseconds, moved by `advance_per_call` on every operation.
    now: u64,
    /// How much the clock moves per call. Zero is the stopped clock.
    advance_per_call: u64,
    /// How many bytes each `recv` hands over. Zero never delivers anything.
    bytes_per_recv: usize,
    /// How many times `recv` was called.
    recvs: u32,
    /// Whether the caller gave the connection up on this transport.
    gave_up: bool,
}

impl Trickle {
    fn new(advance_per_call: u64, bytes_per_recv: usize) -> Self {
        Self {
            now: 1_000,
            advance_per_call,
            bytes_per_recv,
            recvs: 0,
            gave_up: false,
        }
    }
}

impl RpcTransport for Trickle {
    fn send(&mut self, _data: &[u8]) -> Result<(), TransportError> {
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.recvs = self.recvs.saturating_add(1);
        self.now = self.now.saturating_add(self.advance_per_call);
        let n = self.bytes_per_recv.min(buf.len());
        for b in buf[..n].iter_mut() {
            *b = 0xA5;
        }
        Ok(n)
    }

    fn now_ms(&self) -> u64 {
        self.now
    }

    fn poll(&mut self) {
        self.now = self.now.saturating_add(self.advance_per_call);
    }

    fn give_up(&mut self) {
        self.gave_up = true;
    }
}

#[test]
fn a_dribbling_server_cannot_walk_past_the_deadline() {
    // The deadline used to be checked only when `recv` returned nothing, so a
    // server handing over one byte at a time always took the other arm. If
    // the message eventually completed, `recv_at_least` returned SUCCESS long
    // after the deadline had expired -- worse than a missed timeout, because
    // a caller that set a deadline is told it was honoured.
    let mut t = Trickle::new(10, 1);
    let deadline = t.now_ms() + 100;
    let outcome = recv_at_least(&mut t, 10_000, deadline);
    assert_eq!(
        outcome,
        Err(TransportError::Other),
        "a trickle completed the read past its own deadline"
    );
    assert!(
        t.now_ms() <= deadline + 50,
        "it ran {} ms past the deadline before noticing",
        t.now_ms() - deadline
    );
}

#[test]
fn a_dribbling_server_that_finishes_inside_the_deadline_still_succeeds() {
    // The other half of the previous test: checking the deadline every turn
    // must not fail a read that is simply arriving in small pieces.
    // Far more turns than the pass bound allows, which is the point: the
    // bound counts only the turns the clock did not move, and a read arriving
    // in small pieces with time passing must reset it on every turn. Without
    // that reset this read dies of the frozen-clock bound while the clock is
    // working perfectly.
    let mut t = Trickle::new(1, 8);
    let deadline = t.now_ms() + 20_000;
    let want = (MAX_RECV_PASSES as usize + 2_000) * 8;
    let got = recv_at_least(&mut t, want, deadline).expect("a trickle inside the deadline");
    assert_eq!(got.len(), want);
    assert!(
        t.recvs > MAX_RECV_PASSES,
        "the read finished in {} turns, which is too few to prove the reset",
        t.recvs
    );
    assert!(got.iter().all(|&b| b == 0xA5));
}

#[test]
fn a_receive_comes_back_even_if_the_clock_does_not_move() {
    // A bounded park that returns into an unbounded loop is not a bound. With
    // a stopped clock, `now_ms() >= deadline_ms` is false for ever, so the
    // turns the clock does not move are counted here too.
    let mut t = Trickle::new(0, 0);
    let deadline = t.now_ms() + 1_000;
    assert_eq!(
        recv_at_least(&mut t, 16, deadline),
        Err(TransportError::Other),
        "a stopped clock left the receive loop spinning"
    );
    assert!(
        t.recvs <= MAX_RECV_PASSES + 1,
        "it turned {} times, so the pass bound is not holding",
        t.recvs
    );
}

#[test]
fn a_read_that_delivers_bytes_is_progress_whatever_the_clock_says() {
    // The second review round's finding, and the sharper half of it. The
    // stopped-clock bound is for a loop making no progress; a read that hands
    // over bytes IS progress, and a server that dribbles eight bytes a read
    // into a 64 KiB reply makes far more passes than the bound allows. The
    // first version compared two clock readings taken inside one pass, so
    // every one of those fast passes looked frozen and the bound fired on a
    // connection whose deadline was nowhere near: a working clock -- or in
    // this test no clock at all -- must not be able to trigger the
    // stopped-clock exit.
    let want = 8 * (MAX_RECV_PASSES as usize + 10);
    let mut t = Trickle::new(0, 8);
    let got = recv_at_least(&mut t, want, 1_000_000).expect("a dribbling read is not a stall");
    assert_eq!(got.len(), want);
    assert!(
        t.recvs > MAX_RECV_PASSES,
        "it took {} reads, which is not past the bound and so measures nothing",
        t.recvs
    );
    assert_eq!(
        t.now_ms(),
        1_000,
        "the clock was supposed to be the stopped one"
    );
}

#[test]
fn a_record_whose_last_chunk_lands_late_is_still_returned() {
    // Deliberate, and pinned here so it cannot drift: the deadline bounds
    // how long this waits, not when the bytes are allowed to arrive. A
    // *complete* reply that landed late is not improved by waiting longer,
    // and discarding it costs a retransmit of a call the server has already
    // run -- which for `REMOVE` or `RENAME` is not a free thing to do twice.
    let mut t = Trickle::new(200, 1);
    let got = recv_at_least(&mut t, 1, 1_100).expect("a complete record is not a timeout");
    assert_eq!(got.len(), 1);
    assert!(
        t.now_ms() > 1_100,
        "the last chunk was supposed to land after the deadline"
    );
    assert!(
        !t.gave_up,
        "a record that completed left the stream in step, so there is nothing to give up"
    );
}

#[test]
fn walking_away_from_half_a_record_gives_the_connection_up() {
    // The receive side of the partial send. The bytes already taken are gone
    // from the socket, so the next read on this connection would start in the
    // middle of a message and take a length field out of somebody's payload.
    // The stream cannot be put back, so the connection must not be reused.
    let mut t = Trickle::new(200, 1);
    assert!(
        recv_at_least(&mut t, 10, 1_100).is_err(),
        "the deadline was supposed to end this read"
    );
    assert!(
        t.gave_up,
        "part of a record was read and the connection was kept anyway"
    );
}

#[test]
fn a_read_that_took_nothing_leaves_the_connection_usable() {
    // The other half of the same rule, and the reason it is not simply "give
    // up on any failed read": nothing was taken from the socket, so the
    // stream is still in step and the caller may retry on this connection.
    // Giving up here would turn every ordinary timeout into a dead mount.
    let mut t = Trickle::new(200, 0);
    assert!(recv_at_least(&mut t, 10, 1_100).is_err());
    assert!(
        !t.gave_up,
        "a read that got no bytes cannot have left a record half read"
    );
}

/// The receive bound has to be wide enough never to cut short a read that is
/// merely slow, for the same reason the other two do.
const _: () = assert!(MAX_RECV_PASSES >= 1_000);
