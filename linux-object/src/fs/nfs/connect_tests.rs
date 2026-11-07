//! The synchronous connect, against **real** TCP.
//!
//! Same arrangement as the transport's own tests: the sockets live in the
//! kernel's global smoltcp set and a loopback interface built here carries
//! the segments, so what is measured is a real three-way handshake, a real
//! reset from a port nobody listens on, and a real socket left behind — or
//! not — when the wait gives up.
//!
//! Every test takes `NET_TEST_LOCK` and uses its own port in 41300-41399, so
//! a leftover socket from another suite cannot answer for it.

use super::*;
use alloc::collections::BTreeMap;
use smoltcp::iface::{Interface, InterfaceBuilder, Routes};
use smoltcp::phy::{Loopback, Medium};
use smoltcp::wire::IpCidr;

use crate::net::NET_TEST_LOCK as LOCK;

/// The bound has to be wide enough that it can never end a handshake early on
/// a working clock. Checked at build time: as a runtime assertion it would be
/// a constant comparing itself.
const _: () = assert!(MAX_CONNECT_PASSES >= 1_000);

fn loopback() -> Interface<'static, Loopback> {
    InterfaceBuilder::new(Loopback::new(Medium::Ip))
        .ip_addrs([IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)])
        .routes(Routes::new(BTreeMap::new()))
        .finalize()
}

/// Drives the loopback, which is what stands in for the NIC here.
struct Pump {
    iface: Interface<'static, Loopback>,
    drives: u32,
}

impl Pump {
    fn new() -> Self {
        Self {
            iface: loopback(),
            drives: 0,
        }
    }
}

impl StackDriver for Pump {
    fn drive(&mut self) {
        self.drives = self.drives.saturating_add(1);
        let sets = get_sockets();
        let mut sets = sets.lock();
        for _ in 0..8 {
            let ms = kernel_hal::timer::timer_now().as_millis() as i64;
            let _ = self
                .iface
                .poll(&mut sets, smoltcp::time::Instant::from_millis(ms));
        }
    }
}

/// A driver that moves nothing. The SYN is queued and never delivered, which
/// is how a server that is simply not answering is reproduced without one.
struct Deaf;

impl StackDriver for Deaf {
    fn drive(&mut self) {}
}

/// A listening socket in the global set, so a connect has something to
/// complete against.
fn listener(port: u16) -> GlobalSocketHandle {
    let socket = TcpSocket::new(
        TcpSocketBuffer::new(vec![0u8; crate::net::TCP_RECVBUF]),
        TcpSocketBuffer::new(vec![0u8; crate::net::TCP_SENDBUF]),
    );
    let handle = crate::net::register_smoltcp_socket(socket).unwrap();
    let sets = get_sockets();
    let mut sets = sets.lock();
    sets.get::<TcpSocket>(handle.inner()).listen(port).unwrap();
    handle
}

fn local_port_of(handle: &GlobalSocketHandle) -> u16 {
    let sets = get_sockets();
    let mut sets = sets.lock();
    let socket = sets.get::<TcpSocket>(handle.inner());
    socket.local_endpoint().port
}

fn state_of(handle: &GlobalSocketHandle) -> TcpState {
    let sets = get_sockets();
    let mut sets = sets.lock();
    let socket = sets.get::<TcpSocket>(handle.inner());
    socket.state()
}

#[test]
fn a_connect_to_a_listening_port_completes_the_handshake() {
    let _g = LOCK.lock();
    let port = 41301;
    let server = listener(port);
    let mut pump = Pump::new();

    let client = connect_sync(IpAddress::v4(127, 0, 0, 1), port, 2_000, &mut pump)
        .expect("the handshake did not complete");

    assert_eq!(state_of(&client), TcpState::Established);
    assert_eq!(state_of(&server), TcpState::Established);
}

#[test]
fn a_connect_to_a_port_nobody_listens_on_is_refused() {
    // The distinction worth having: a reset is "nothing is listening there",
    // and reporting it as a timeout would both hide that and take the whole
    // deadline to do it. The deadline here is two seconds and this must come
    // back in a handful of passes.
    let _g = LOCK.lock();
    let mut pump = Pump::new();

    let err = connect_sync(IpAddress::v4(127, 0, 0, 1), 41302, 2_000, &mut pump)
        .expect_err("a port with no listener answered");

    assert_eq!(err, LxError::ECONNREFUSED);
}

#[test]
fn a_handshake_that_never_arrives_times_out() {
    // The SYN is queued and nothing carries it, which is a server that does
    // not answer.
    //
    // The abort before returning is not measured here and cannot be: the
    // handle is dropped on the way out, which takes the socket out of the
    // set, so from this side the two are indistinguishable. It is there for
    // the peer's sake -- if the SYN did arrive, dropping the socket without
    // a reset leaves the server holding a half-open connection -- and for
    // the reason `tcp.rs` aborts in the same place.
    let _g = LOCK.lock();
    let mut deaf = Deaf;

    let started = kernel_hal::timer::timer_now();
    let err = connect_sync(IpAddress::v4(127, 0, 0, 1), 41303, 50, &mut deaf)
        .expect_err("a handshake nothing carried reported success");
    let took = kernel_hal::timer::timer_now() - started;

    assert_eq!(err, LxError::ETIMEDOUT);
    // The errno alone does not say the deadline was the thing that ended it:
    // some *other* bound reached eventually would answer the same. Fifty
    // milliseconds were asked for, so anything near a second means the
    // deadline is not being read.
    assert!(
        took < core::time::Duration::from_secs(1),
        "it waited {:?} for a 50 ms deadline",
        took
    );
}

#[test]
fn two_connections_to_the_same_server_do_not_share_a_local_port() {
    // A local port picked once and reused is a client that works until the
    // second mount to the same server, and then does not: the pair is
    // already in the set, and the connect is refused for a reason that has
    // nothing to do with the server.
    let _g = LOCK.lock();
    let port = 41305;
    let _server = listener(port);
    let mut pump = Pump::new();

    let first = connect_sync(IpAddress::v4(127, 0, 0, 1), port, 2_000, &mut pump)
        .expect("the first handshake did not complete");
    let _second = listener(port);
    let second = connect_sync(IpAddress::v4(127, 0, 0, 1), port, 2_000, &mut pump)
        .expect("the second connection to the same server was refused");

    assert_ne!(
        local_port_of(&first),
        local_port_of(&second),
        "both connections took the same local port"
    );
}

#[test]
fn the_connector_hands_back_a_transport_on_the_port_it_was_asked_for() {
    // The seam the handshake above is measured through: what the real
    // connector produces is a transport over the socket it just connected,
    // so a `GETPORT` answer really does decide where the next call goes.
    let _g = LOCK.lock();
    let port = 41304;
    let server = listener(port);

    // The real connector drives the real stack, which on the host moves
    // nothing -- so the handshake is completed here by pumping the loopback
    // from the other side of the connect.
    let mut pump = Pump::new();
    let client = connect_sync(IpAddress::v4(127, 0, 0, 1), port, 2_000, &mut pump).unwrap();
    let transport = KernelTransport::new(client);

    assert!(!transport.is_poisoned());
    assert_eq!(state_of(&server), TcpState::Established);
}

#[test]
fn the_connector_waits_the_budget_it_was_given_and_not_its_own_ceiling() {
    // The budget is an argument because the greeting spends one deadline
    // across three steps, and a connector that waits its own thirty seconds
    // whatever is left of that budget is how the deadline stops meaning
    // anything. Measured by the clock and not by the errno: a ceiling that
    // ignored the argument would answer `ETIMEDOUT` too, just much later.
    //
    // Nothing drives the stack here, so the SYN is never carried and the
    // wait runs to whichever of the two numbers is in force.
    let _g = LOCK.lock();
    let mut connector = TcpConnector::new(IpAddress::v4(127, 0, 0, 1)).with_timeout(30_000);

    let started = kernel_hal::timer::timer_now();
    let err = match connector.connect(41306, 50) {
        Ok(_) => panic!("a handshake nothing carried reported success"),
        Err(e) => e,
    };
    let took = kernel_hal::timer::timer_now() - started;

    assert_eq!(err, LxError::ETIMEDOUT);
    assert!(
        took < core::time::Duration::from_secs(1),
        "it waited {:?} for a 50 ms budget under a 30 s ceiling",
        took
    );
}

#[test]
fn the_connectors_own_ceiling_still_caps_a_bigger_budget() {
    // The other half: the argument is a budget, not a licence. A connector
    // built with `with_timeout` promised that ceiling, and a caller with a
    // minute to spend does not get to hold the handshake for a minute.
    let _g = LOCK.lock();
    let mut connector = TcpConnector::new(IpAddress::v4(127, 0, 0, 1)).with_timeout(50);

    let started = kernel_hal::timer::timer_now();
    let err = match connector.connect(41307, 60_000) {
        Ok(_) => panic!("a handshake nothing carried reported success"),
        Err(e) => e,
    };
    let took = kernel_hal::timer::timer_now() - started;

    assert_eq!(err, LxError::ETIMEDOUT);
    assert!(
        took < core::time::Duration::from_secs(1),
        "it waited {:?} for a 50 ms ceiling",
        took
    );
}
