//! The kernel transport, over a **real** TCP connection.
//!
//! These are not fake-transport tests. The sockets live in the kernel's own
//! global smoltcp set — the same one every other socket in the system is in —
//! and a loopback interface built here carries the segments between two of
//! them. So what is measured is the transport against real TCP: a real
//! handshake, real send and receive buffers, real window and real framing.
//!
//! The one thing the host cannot do is drive the NIC, because there is no NIC:
//! `drain_net_urgent()` walks the registered net devices and there are none,
//! so a test moves the loopback itself with [`pump`]. In the kernel that drain
//! is what [`KernelTransport::drive`] relies on, and here it is a no-op, which
//! is why these tests interleave `pump` by hand rather than parking.
//!
//! Every test takes `NET_TEST_LOCK` — shared with the TCP and UDP suites,
//! because the socket set is global — and uses its own port in 41200-41299 so
//! a leftover socket from another test cannot answer.

use super::*;
use alloc::collections::BTreeMap;
use alloc::vec;
use smoltcp::iface::{Interface, InterfaceBuilder, Routes};
use smoltcp::phy::{Loopback, Medium};
use smoltcp::socket::TcpSocketBuffer;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, IpCidr};

use crate::net::NET_TEST_LOCK as LOCK;

/// Both bounds have to be wide enough never to cut short a park or a send that
/// is merely slow. Checked at compile time: as a runtime assertion it would be
/// a constant, which says nothing a build cannot say sooner.
const _: () = assert!(MAX_PARK_PASSES >= 1_000);
const _: () = assert!(MAX_SEND_STALLS >= 1_000);

/// A socket with the kernel's real buffer sizes, registered in the kernel's
/// real socket set.
fn register() -> GlobalSocketHandle {
    let socket = TcpSocket::new(
        TcpSocketBuffer::new(vec![0u8; crate::net::TCP_RECVBUF]),
        TcpSocketBuffer::new(vec![0u8; crate::net::TCP_SENDBUF]),
    );
    crate::net::register_smoltcp_socket(socket).unwrap()
}

fn loopback() -> Interface<'static, Loopback> {
    InterfaceBuilder::new(Loopback::new(Medium::Ip))
        .ip_addrs([IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)])
        .routes(Routes::new(BTreeMap::new()))
        .finalize()
}

/// Move whatever is queued through the loopback, advancing smoltcp's clock so
/// its timers run.
fn pump(iface: &mut Interface<'static, Loopback>) {
    let sets = get_sockets();
    let mut sets = sets.lock();
    for _ in 0..16 {
        let ms = kernel_hal::timer::timer_now().as_millis() as i64;
        let _ = iface.poll(&mut sets, Instant::from_millis(ms));
    }
}

/// Drives the loopback from inside the transport's own loops, and drains the
/// far end as it goes.
///
/// This is what makes [`KernelTransport::send`]'s partial-write loop reachable
/// from a test. The loop only turns when the send buffer is full, and the
/// buffer only empties if something on the other side reads -- which, during a
/// blocking `send`, can only be the driver.
struct TestDriver {
    iface: Interface<'static, Loopback>,
    far: SocketHandle,
    /// Everything the far end has read, in order.
    drained: Vec<u8>,
    /// How many times the transport asked for the stack to be driven.
    drives: u32,
}

impl TestDriver {
    fn new(far: SocketHandle) -> Self {
        Self {
            iface: loopback(),
            far,
            drained: Vec::new(),
            drives: 0,
        }
    }
}

impl StackDriver for TestDriver {
    fn drive(&mut self) {
        self.drives = self.drives.saturating_add(1);
        let sets = get_sockets();
        let mut sets = sets.lock();
        for _ in 0..8 {
            let ms = kernel_hal::timer::timer_now().as_millis() as i64;
            let _ = self.iface.poll(&mut sets, Instant::from_millis(ms));
            let mut s = sets.get::<TcpSocket>(self.far);
            let mut buf = [0u8; 16 * 1024];
            while let Ok(n) = s.recv_slice(&mut buf) {
                if n == 0 {
                    break;
                }
                self.drained.extend_from_slice(&buf[..n]);
            }
        }
    }
}

/// A connected pair in the global socket set: `(server, client)`.
fn connected_pair(
    iface: &mut Interface<'static, Loopback>,
    port: u16,
) -> (GlobalSocketHandle, GlobalSocketHandle) {
    let server = register();
    let client = register();
    {
        let sets = get_sockets();
        let mut sets = sets.lock();
        let mut s = sets.get::<TcpSocket>(server.inner());
        s.listen(port).unwrap();
    }
    {
        let sets = get_sockets();
        let mut sets = sets.lock();
        let mut c = sets.get::<TcpSocket>(client.inner());
        c.connect((IpAddress::v4(127, 0, 0, 1), port), 49100 + port % 400)
            .unwrap();
    }
    pump(iface);
    {
        let sets = get_sockets();
        let mut sets = sets.lock();
        assert!(
            sets.get::<TcpSocket>(client.inner()).may_send(),
            "the handshake did not complete"
        );
    }
    (server, client)
}

/// What the far end has received so far.
fn server_takes(server: &GlobalSocketHandle, want: usize) -> Vec<u8> {
    let sets = get_sockets();
    let mut sets = sets.lock();
    let mut s = sets.get::<TcpSocket>(server.inner());
    let mut buf = vec![0u8; want];
    match s.recv_slice(&mut buf) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(_) => Vec::new(),
    }
}

/// Put bytes on the far end, as a server answering.
fn server_says(server: &GlobalSocketHandle, data: &[u8]) {
    let sets = get_sockets();
    let mut sets = sets.lock();
    let mut s = sets.get::<TcpSocket>(server.inner());
    assert_eq!(s.send_slice(data).unwrap(), data.len());
}

/// Close the far end's send half, as a server hanging up.
fn server_closes(server: &GlobalSocketHandle) {
    let sets = get_sockets();
    let mut sets = sets.lock();
    sets.get::<TcpSocket>(server.inner()).close();
}

/// Abort the far end, which puts a RST on the wire.
fn server_aborts(server: &GlobalSocketHandle) {
    let sets = get_sockets();
    let mut sets = sets.lock();
    sets.get::<TcpSocket>(server.inner()).abort();
}

/// Read until `want` bytes are in hand, pumping the loopback between tries.
///
/// Stands in for the park the kernel would do: the host has no NIC to drive,
/// so progress comes from the pump rather than from the transport's own
/// `poll`.
fn read_pumping(
    t: &mut KernelTransport,
    iface: &mut Interface<'static, Loopback>,
    want: usize,
) -> Result<Vec<u8>, TransportError> {
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    for _ in 0..512 {
        if out.len() >= want {
            return Ok(out);
        }
        pump(iface);
        let room = core::cmp::min(buf.len(), want - out.len());
        match t.recv(&mut buf[..room])? {
            0 => {}
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    Ok(out)
}

// ======== the pure decisions, which no connection is needed to check ========

#[test]
fn an_empty_buffer_on_an_open_socket_is_nothing_yet_and_not_the_end() {
    // The distinction the whole transport rests on. `Exhausted` means the
    // receive buffer is empty *now*; whether that is the end of the stream is
    // in `may_recv`, not in the error. Reading it from the error alone
    // truncates a transfer the instant a read lands between two segments.
    assert_eq!(classify_read(smoltcp::Error::Exhausted, true), Ok(0));
    assert_eq!(
        classify_read(smoltcp::Error::Exhausted, false),
        Err(TransportError::Closed)
    );
}

#[test]
fn a_finished_stream_is_closed_and_an_illegal_one_is_a_reset() {
    // Two different things for the caller: a server that hung up cleanly can
    // be reconnected to, one that reset may be refusing the mount.
    assert_eq!(
        classify_read(smoltcp::Error::Finished, true),
        Err(TransportError::Closed)
    );
    assert_eq!(
        classify_read(smoltcp::Error::Illegal, true),
        Err(TransportError::Reset)
    );
    assert_eq!(
        classify_read(smoltcp::Error::Unaddressable, true),
        Err(TransportError::Other)
    );
}

#[test]
fn a_write_to_a_torn_down_socket_is_closed_and_not_merely_other() {
    // The caller retries on `Other` and gives up the connection on `Closed`,
    // so calling a hung-up peer "other" makes the client retransmit into a
    // socket that will never carry it.
    assert_eq!(
        classify_write(smoltcp::Error::Illegal),
        TransportError::Closed
    );
    assert_eq!(
        classify_write(smoltcp::Error::Finished),
        TransportError::Closed
    );
    assert_eq!(
        classify_write(smoltcp::Error::Unaddressable),
        TransportError::Other
    );
}

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

// ======== and the same transport against real TCP ========

#[test]
fn a_record_sent_through_the_transport_arrives_whole_at_the_far_end() {
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41201);
    let mut t = KernelTransport::new(client);

    // An RPC record as `rpc.rs` frames it: a 4-byte marker with the
    // last-fragment bit, then the payload.
    let mut record = Vec::new();
    record.extend_from_slice(&(0x8000_0000u32 | 12).to_be_bytes());
    record.extend_from_slice(&[0xab; 12]);

    assert_eq!(t.send(&record), Ok(()));
    pump(&mut iface);

    assert_eq!(server_takes(&server, 64), record, "every byte, in order");
}

#[test]
fn a_payload_larger_than_one_segment_still_arrives_whole() {
    // The promise `send` makes is *all* of the bytes. A record whose header
    // went out without its payload leaves the server waiting for bytes the
    // client believes it sent, which is a hang and not an error -- so this is
    // the test that makes the client's timeouts mean anything.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41202);
    let mut t = KernelTransport::new(client);

    // Each byte derived from its index, so a lost or reordered chunk shows up
    // as a mismatch rather than as more of the same byte.
    let big: Vec<u8> = (0..40_000usize).map(|i| (i % 251) as u8).collect();
    assert_eq!(t.send(&big), Ok(()));

    let mut got = Vec::new();
    for _ in 0..512 {
        pump(&mut iface);
        let chunk = server_takes(&server, 8192);
        if chunk.is_empty() && got.len() >= big.len() {
            break;
        }
        got.extend_from_slice(&chunk);
    }
    assert_eq!(got.len(), big.len(), "all of it");
    assert_eq!(got, big, "and in order");
}

#[test]
fn a_reply_comes_back_through_the_transport_byte_for_byte() {
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41203);
    let mut t = KernelTransport::new(client);

    let reply: Vec<u8> = (0..300usize).map(|i| (i % 253) as u8).collect();
    server_says(&server, &reply);

    let got = read_pumping(&mut t, &mut iface, reply.len()).unwrap();
    assert_eq!(got, reply);
}

#[test]
fn a_read_with_nothing_there_yet_is_zero_and_not_an_error() {
    // The common case while waiting for a reply: the connection is fine and
    // nothing has arrived. An error here would make the client tear down a
    // healthy connection on its first poll.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (_server, client) = connected_pair(&mut iface, 41204);
    let mut t = KernelTransport::new(client);

    let mut buf = [0u8; 64];
    pump(&mut iface);
    assert_eq!(t.recv(&mut buf), Ok(0));
    assert_eq!(t.recv(&mut buf), Ok(0), "and again, still not an error");
}

#[test]
fn a_server_that_hangs_up_after_answering_gives_the_data_then_the_close() {
    // Order matters: what is already queued must be handed over before the
    // close is reported, or a reply that did arrive is lost because the FIN
    // overtook it in the client's logic.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41205);
    let mut t = KernelTransport::new(client);

    server_says(&server, b"la respuesta entera");
    server_closes(&server);
    pump(&mut iface);

    let got = read_pumping(&mut t, &mut iface, 19).unwrap();
    assert_eq!(&got, b"la respuesta entera", "the data first");

    // And only once it is drained does the close show.
    let mut buf = [0u8; 16];
    let mut outcome = Ok(0);
    for _ in 0..64 {
        pump(&mut iface);
        outcome = t.recv(&mut buf);
        if outcome != Ok(0) {
            break;
        }
    }
    assert_eq!(outcome, Err(TransportError::Closed), "then the close");
}

#[test]
fn a_reset_connection_is_reported_and_not_read_as_an_empty_one() {
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41206);
    let mut t = KernelTransport::new(client);

    server_aborts(&server);
    pump(&mut iface);

    let mut buf = [0u8; 64];
    let mut outcome = Ok(0);
    for _ in 0..64 {
        pump(&mut iface);
        outcome = t.recv(&mut buf);
        if outcome != Ok(0) {
            break;
        }
    }
    assert!(
        matches!(
            outcome,
            Err(TransportError::Reset) | Err(TransportError::Closed)
        ),
        "a reset must not look like an idle socket: {:?}",
        outcome
    );
}

#[test]
fn a_send_on_a_connection_the_peer_reset_does_not_report_success() {
    // Reporting success would make the client wait out its whole timeout for
    // a reply to a call that was never carried.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41207);
    let mut t = KernelTransport::new(client);

    server_aborts(&server);
    for _ in 0..8 {
        pump(&mut iface);
    }

    // Either the send is refused outright, or it is accepted into a buffer
    // that then cannot drain -- which the deadline catches. What must not
    // happen is a plain `Ok(())` on a live connection that is gone, so the
    // socket's own state is what the assertion rests on.
    let outcome = t.send(b"una llamada que no llega");
    if outcome.is_ok() {
        let sets = get_sockets();
        let mut sets = sets.lock();
        let socket = sets.get::<TcpSocket>(t.sock_for_test());
        assert!(
            !socket.may_send() || !socket.may_recv(),
            "the send said it worked on a connection that is still up"
        );
    }
}

#[test]
fn a_full_round_trip_runs_the_whole_stack_over_real_tcp() {
    // Call out, reply back, through the framing in `rpc.rs`. This is the
    // first point in the batches where an encoder, the record marking and a
    // real TCP connection are all in the same test.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41208);
    let mut t = KernelTransport::new(client);

    let body = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
    let mut call = Vec::new();
    call.extend_from_slice(&(0x8000_0000u32 | body.len() as u32).to_be_bytes());
    call.extend_from_slice(&body);

    assert_eq!(t.send(&call), Ok(()));
    pump(&mut iface);
    assert_eq!(server_takes(&server, 64), call, "the call as framed");

    // The server answers with a record of its own.
    let answer = [0x99u8, 0xaa, 0xbb, 0xcc];
    let mut framed = Vec::new();
    framed.extend_from_slice(&(0x8000_0000u32 | answer.len() as u32).to_be_bytes());
    framed.extend_from_slice(&answer);
    server_says(&server, &framed);

    let got = read_pumping(&mut t, &mut iface, framed.len()).unwrap();
    assert_eq!(got, framed);

    // And the marker the client would parse off it says last-fragment, 4.
    let marker = u32::from_be_bytes([got[0], got[1], got[2], got[3]]);
    assert_eq!(marker & 0x8000_0000, 0x8000_0000, "last fragment");
    assert_eq!(marker & 0x7fff_ffff, 4, "of four bytes");
}

#[test]
fn the_clock_the_transport_reports_moves_forward() {
    // The client measures its timeouts with this and nothing else, so a clock
    // that stood still would turn every timeout into a hang.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (_server, client) = connected_pair(&mut iface, 41209);
    let t = KernelTransport::new(client);

    let first = t.now_ms();
    let started = kernel_hal::timer::timer_now();
    while kernel_hal::timer::timer_now() - started < core::time::Duration::from_millis(5) {
        core::hint::spin_loop();
    }
    assert!(t.now_ms() >= first, "and never backwards");
    assert!(
        t.now_ms() > first,
        "5 ms of spinning has to show on a millisecond clock"
    );
}

#[test]
fn reading_nothing_relaxes_the_park_and_a_byte_tightens_it_again() {
    // The counter behind `park_ms` is state on the transport, so this is the
    // half the pure test cannot see: that empty reads raise it and data
    // resets it. A counter that never reset would leave a busy mount parking
    // as if it were idle.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41210);
    let mut t = KernelTransport::new(client);

    let mut buf = [0u8; 32];
    for _ in 0..12 {
        pump(&mut iface);
        assert_eq!(t.recv(&mut buf), Ok(0));
    }
    assert!(
        park_ms(t.empty_polls) > park_ms(0),
        "twelve empty reads should have relaxed the park"
    );

    server_says(&server, b"algo");
    for _ in 0..64 {
        pump(&mut iface);
        if let Ok(n) = t.recv(&mut buf) {
            if n > 0 {
                break;
            }
        }
    }
    assert_eq!(
        park_ms(t.empty_polls),
        park_ms(0),
        "a byte puts it back to the tight park"
    );
}

// ======== the partial-write loop, which needs more than the send buffer ========

/// A payload past `TCP_SENDBUF`, so `send_slice` *must* come back short and
/// the loop in `send` has to turn more than once.
///
/// Below this size the whole thing goes into the buffer in one call and the
/// loop is decoration -- which is how a batch of mutations to it survived a
/// suite that only ever sent 40 KiB.
fn bigger_than_the_send_buffer() -> Vec<u8> {
    let len = crate::net::TCP_SENDBUF + 64 * 1024;
    // Each byte derived from its index, so a chunk sent twice, skipped, or
    // sent from the wrong offset shows up as a mismatch at a known place
    // rather than as more of the same byte.
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn a_payload_past_the_send_buffer_still_arrives_whole_and_in_order() {
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41211);
    let far = server.inner();
    let mut t = KernelTransport::with_driver(client, TestDriver::new(far));

    let big = bigger_than_the_send_buffer();
    assert_eq!(
        t.send(&big),
        Ok(()),
        "every byte, however many loops it took"
    );

    // Whatever is still in flight, drive until it lands.
    for _ in 0..64 {
        t.poll();
    }
    let got = &t.driver_for_test().drained;
    assert_eq!(got.len(), big.len(), "all of it arrived");
    assert_eq!(got, &big, "and in the order it was sent");
}

#[test]
fn the_send_loop_really_turns_more_than_once_for_such_a_payload() {
    // The guard on the test above: if one `send_slice` took the whole
    // payload, that test would pass with the loop removed. This asserts the
    // conditions the loop exists for were actually met.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41212);
    let far = server.inner();
    let mut t = KernelTransport::with_driver(client, TestDriver::new(far));

    let big = bigger_than_the_send_buffer();
    assert!(
        big.len() > crate::net::TCP_SENDBUF,
        "the payload has to exceed the buffer or there is nothing to loop over"
    );
    assert_eq!(t.send(&big), Ok(()));
    assert!(
        t.driver_for_test().drives > 1,
        "one pass through the loop means the buffer never filled: {} drives",
        t.driver_for_test().drives
    );
}

#[test]
fn a_send_that_cannot_make_progress_gives_up_instead_of_waiting_for_ever() {
    // A far end that never reads: the buffer fills and stays full. The send
    // has to end on its deadline rather than hold the thread -- which, in the
    // kernel, is the thread that drives the network stack, so waiting for
    // ever would take the machine's networking down with it.
    //
    // The real driver, deliberately: on the host it moves nothing, which is
    // exactly the peer-never-reads case. The timeout is shortened because a
    // minute is the right answer in the kernel and the wrong one in a suite.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (_server, client) = connected_pair(&mut iface, 41213);
    let mut t = KernelTransport::new(client).with_send_timeout(200);

    let big = bigger_than_the_send_buffer();
    let started = kernel_hal::timer::timer_now();
    let outcome = t.send(&big);
    let took = kernel_hal::timer::timer_now() - started;

    assert!(
        outcome.is_err(),
        "a send that never got the bytes out must not report success"
    );
    assert!(
        took < core::time::Duration::from_secs(10),
        "it gave up after {:?}, which is not a deadline",
        took
    );
}

#[test]
fn the_clock_is_in_milliseconds_and_not_some_other_unit() {
    // `now_ms` is the only clock the client measures its timeouts with, so
    // the unit is load-bearing: nanoseconds would make every timeout fire a
    // million times too early, seconds a thousand times too late. Checking
    // that it merely increases does not catch either.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (_server, client) = connected_pair(&mut iface, 41214);
    let t = KernelTransport::new(client);

    let before = t.now_ms();
    let started = kernel_hal::timer::timer_now();
    let waited = core::time::Duration::from_millis(50);
    while kernel_hal::timer::timer_now() - started < waited {
        core::hint::spin_loop();
    }
    let elapsed = t.now_ms() - before;
    assert!(
        (40..=400).contains(&elapsed),
        "50 ms of waiting read as {} of whatever unit this is",
        elapsed
    );
}

#[test]
fn a_send_checks_the_socket_can_still_send_before_handing_it_bytes() {
    // The far end is gone. Accepting the bytes and reporting success would
    // make the client wait out its whole timeout for a reply to a call that
    // was never carried.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41215);
    let far = server.inner();
    {
        let sets = get_sockets();
        let mut sets = sets.lock();
        sets.get::<TcpSocket>(far).abort();
    }
    let mut t = KernelTransport::with_driver(client, TestDriver::new(far));
    // Let the reset land.
    for _ in 0..8 {
        t.poll();
    }

    assert_eq!(
        t.send(b"una llamada que no llega"),
        Err(TransportError::Closed),
        "a send into a dead connection must not report success"
    );
}

#[test]
fn a_read_drives_the_stack_itself_rather_than_hoping_someone_else_did() {
    // Every other read test pumps the loopback from the test body, which hides
    // whether `recv` drives anything: the data is already in the socket by the
    // time it looks. Here the transport's own driver is the *only* thing that
    // can move a byte -- nothing in the test body pumps -- so a `recv` that
    // did not drive would wait for data that never comes.
    //
    // In the kernel that is the difference between a mount that works and one
    // that hangs on the first call, because there the NIC only hands packets
    // over when something drains it.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41216);
    let far = server.inner();

    // Queue the answer on the far end, then drop the test's own interface so
    // nothing but the transport can carry it.
    {
        let sets = get_sockets();
        let mut sets = sets.lock();
        assert_eq!(
            sets.get::<TcpSocket>(far)
                .send_slice(b"la respuesta")
                .unwrap(),
            12
        );
    }
    drop(iface);

    let mut t = KernelTransport::with_driver(client, TestDriver::new(far));
    let mut got = Vec::new();
    let mut buf = [0u8; 32];
    for _ in 0..64 {
        match t.recv(&mut buf) {
            Ok(0) => {}
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => panic!("{:?}", e),
        }
        if got.len() >= 12 {
            break;
        }
    }
    assert_eq!(
        &got, b"la respuesta",
        "recv had to drive the stack to get this"
    );
}

#[test]
fn a_park_comes_back_even_if_the_clock_does_not_move() {
    // The park loop's first exit is the clock, and a loop whose only exit is
    // the clock does not terminate if the clock stops. This one has a pass
    // count as well, because it runs on the thread that drives the network
    // stack: a stalled timer there would not merely hang the mount, it would
    // take the machine's networking with it.
    //
    // The clock here is real and does move, so what this pins is the bound
    // itself: wide enough never to cut a working park short, finite so that a
    // stopped clock cannot wedge the thread.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (_server, client) = connected_pair(&mut iface, 41217);
    let mut t = KernelTransport::new(client);

    let started = kernel_hal::timer::timer_now();
    for _ in 0..4 {
        t.poll();
    }
    assert!(
        kernel_hal::timer::timer_now() - started < core::time::Duration::from_secs(5),
        "four parks must not take seconds"
    );
}

// ------------------------------------------------- the Copilot round of #2498

/// A transport that is nothing but a clock and a trickle, for the two things
/// [`recv_at_least`] has to get right that a real socket cannot be made to do
/// on demand: a clock that has stopped, and a server that dribbles.
///
/// It is not a stand-in for [`KernelTransport`] — everything else in this file
/// measures the real one over real TCP. `recv_at_least` is generic over the
/// trait, and these two behaviours are a property of *its* loop.
struct Trickle {
    /// Milliseconds, moved by `advance_per_call` on every operation.
    now: u64,
    /// How much the clock moves per call. Zero is the stopped clock.
    advance_per_call: u64,
    /// How many bytes each `recv` hands over. Zero never delivers anything.
    bytes_per_recv: usize,
    /// How many times `recv` was called.
    recvs: u32,
}

impl Trickle {
    fn new(advance_per_call: u64, bytes_per_recv: usize) -> Self {
        Self {
            now: 1_000,
            advance_per_call,
            bytes_per_recv,
            recvs: 0,
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
fn a_send_that_got_part_of_a_record_out_gives_the_connection_up() {
    // The worst of the round. A send that fails having already handed PART of
    // a record to the socket leaves the server waiting for the rest of a
    // message. The client above only sees an error and keeps the transport,
    // so its next call writes a record header where the server expects the
    // previous payload: not a lost call but a stream out of step, surfacing
    // in some later call that had nothing to do with it.
    //
    // Half a record cannot be withdrawn from the server's buffer, so the only
    // honest repair is to stop using the connection.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (_server, client) = connected_pair(&mut iface, 41218);
    let mut t = KernelTransport::new(client).with_send_timeout(200);

    // Larger than the send buffer and a far end that never reads: the first
    // chunk goes into the buffer, the rest cannot, and the deadline ends it.
    let big = bigger_than_the_send_buffer();
    assert!(
        t.send(&big).is_err(),
        "a send that never got the bytes out must not report success"
    );
    assert!(
        t.is_poisoned(),
        "the connection carried half a record and was kept anyway"
    );
    // And the socket is really gone, not merely flagged: a flag that left the
    // connection open would keep the server waiting out its own timeout on
    // the rest of a message that is never coming.
    assert_eq!(
        socket_state(t.sock_for_test()),
        smoltcp::socket::TcpState::Closed,
        "giving the connection up left the socket alive"
    );
}

/// The smoltcp state of a socket in the kernel's set.
fn socket_state(handle: SocketHandle) -> smoltcp::socket::TcpState {
    let sets = get_sockets();
    let mut sets = sets.lock();
    let state = sets.get::<TcpSocket>(handle).state();
    state
}

#[test]
fn nothing_reaches_the_wire_on_a_connection_that_was_given_up_on() {
    // What giving it up is for: every later operation has to refuse, because
    // the stream is out of step and the next record would be framed against
    // the tail of the last one.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41219);
    let mut t = KernelTransport::new(client).with_send_timeout(200);

    let big = bigger_than_the_send_buffer();
    let _ = t.send(&big);
    assert!(t.is_poisoned());

    assert_eq!(
        t.send(b"la llamada siguiente"),
        Err(TransportError::NotConnected),
        "a later call was framed onto a stream that is out of step"
    );
    let mut buf = [0u8; 16];
    assert_eq!(
        t.recv(&mut buf),
        Err(TransportError::NotConnected),
        "a read on a connection given up on must refuse too"
    );
    pump(&mut iface);
    let _ = server;
}

#[test]
fn a_send_that_got_nothing_out_leaves_the_connection_usable() {
    // The other side of it, and the reason this is not just "give up on any
    // error". With `sent == 0` nothing reached the server, so the stream is
    // still in step and the caller may try the same connection again. Giving
    // it up there would turn every refused write into a dead mount.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41220);
    let mut t = KernelTransport::new(client);

    // The peer resets, so the very first `send_slice` fails with nothing
    // sent.
    server_aborts(&server);
    pump(&mut iface);
    let outcome = t.send(b"una peticion");
    assert!(outcome.is_err(), "a send to a reset peer must not succeed");
    assert!(
        !t.is_poisoned(),
        "a send that handed over no bytes gave the connection up anyway"
    );
}

/// A driver that resets the far end part-way through the send loop.
///
/// The only way to reach the `may_send` arm of that loop *with bytes already
/// gone*: the peer has to go away between one turn and the next, and on the
/// host nothing else moves while the loop is running. That is what the
/// `StackDriver` seam is for.
struct AbortAfter {
    iface: Interface<'static, Loopback>,
    far: SocketHandle,
    drives: u32,
    /// The drive on which the far end resets.
    at: u32,
}

impl StackDriver for AbortAfter {
    fn drive(&mut self) {
        self.drives = self.drives.saturating_add(1);
        if self.drives == self.at {
            let sets = get_sockets();
            let mut sets = sets.lock();
            sets.get::<TcpSocket>(self.far).abort();
        }
        let sets = get_sockets();
        let mut sets = sets.lock();
        for _ in 0..16 {
            let ms = kernel_hal::timer::timer_now().as_millis() as i64;
            let _ = self.iface.poll(&mut sets, Instant::from_millis(ms));
        }
    }
}

#[test]
fn a_send_half_done_when_the_peer_goes_away_gives_the_connection_up_too() {
    // The other way out of the send loop with bytes already gone: the first
    // chunk lands, then the peer resets, so the next turn finds `may_send`
    // false. The record is half sent either way, so this path has to give the
    // connection up for the same reason the deadline path does -- and it did
    // not, which a mutation of the deadline path alone would never have
    // shown.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (server, client) = connected_pair(&mut iface, 41223);
    let far = server.inner();
    let mut t = KernelTransport::with_driver(
        client,
        AbortAfter {
            iface: loopback(),
            far,
            drives: 0,
            at: 2,
        },
    )
    .with_send_timeout(2_000);

    // Bigger than the send buffer, so the loop has to turn again after the
    // first chunk -- and by then the driver has reset the far end.
    let big = bigger_than_the_send_buffer();
    assert!(
        t.send(&big).is_err(),
        "a send whose peer went away must not report success"
    );
    assert!(
        t.is_poisoned(),
        "the send half went away with part of a record out, and the connection was kept"
    );
    let _ = server;
}

#[test]
fn the_stall_bound_counts_only_the_passes_the_clock_did_not_move() {
    // With the bound lowered to five, a send against a far end that never
    // reads still has to run to its deadline: the clock is working, so every
    // park advances it and the counter never gets past one. Counting every
    // stalled pass -- which is what the first version did -- would end this
    // send after five parks, in a handful of milliseconds.
    let _g = LOCK.lock();
    let mut iface = loopback();
    let (_server, client) = connected_pair(&mut iface, 41222);
    let mut t = KernelTransport::new(client)
        .with_send_timeout(200)
        .with_max_send_stalls(5);

    let big = bigger_than_the_send_buffer();
    let started = kernel_hal::timer::timer_now();
    assert!(t.send(&big).is_err());
    let took = kernel_hal::timer::timer_now() - started;

    assert!(
        took >= core::time::Duration::from_millis(150),
        "it gave up after {:?}, so the stall count ended it and not the 200 ms deadline",
        took
    );
}

/// The receive bound has to be wide enough never to cut short a read that is
/// merely slow, for the same reason the other two do.
const _: () = assert!(MAX_RECV_PASSES >= 1_000);
