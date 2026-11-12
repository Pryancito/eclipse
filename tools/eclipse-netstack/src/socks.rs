//! The sockets of the stack: what the kernel asks for a program, and what
//! arrives for one.
//!
//! A program's socket is the kernel's object: the descriptor, its flags,
//! the queue `recvfrom` and `poll` read. What the kernel cannot do is put a
//! datagram on the card or take one off it, and that is what it asks of
//! this process, over a channel, in messages that go one way:
//!
//! ```text
//! all:    0  u32 zero   4  u32 op   8  u64 socket
//! BIND    (kernel to stack)  16  u16 port
//! SEND    (kernel to stack)  16  [u8; 4] to    20  u16 port  22  u16 zero  24  the datagram
//! CLOSE   (kernel to stack)
//! DATA    (stack to kernel)  16  one record after another, to the end:
//!             0  u64 socket  8  [u8; 4] from  12  u16 port  14  u16 length  16  the datagram
//! ```
//!
//! little-endian, the addresses as they are written
//! (`linux-object/src/net/user_stack.rs` is the kernel's side, and says
//! the same). The kernel numbers the sockets and gives out the ports, so a
//! `BIND` is told, not asked: the stack opens a smoltcp socket on that
//! port and from then on every datagram that arrives for it goes to the
//! kernel, at once, and every `SEND` goes to the card. A `DATA` carries
//! all the datagrams of a turn of the stack, for whichever sockets (its
//! header's socket is zero): the kernel is woken once for them all.
//!
//! A [`Stack`] is the interface and those sockets. It makes no system
//! call: whoever has it carries the messages.

use std::collections::BTreeMap;

use smoltcp::iface::Interface;
use smoltcp::phy::Device;
use smoltcp::socket::{SocketHandle, SocketSet, UdpPacketMetadata, UdpSocket, UdpSocketBuffer};
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, IpCidr, IpEndpoint, Ipv4Address};

/// The operations of a message.
pub const OP_BIND: u32 = 1;
pub const OP_SEND: u32 = 2;
pub const OP_CLOSE: u32 = 3;
pub const OP_DATA: u32 = 4;

/// What every message starts with.
pub const HEADER_LEN: usize = 16;
/// Where the datagram starts in `SEND`.
pub const DATAGRAM_AT: usize = 24;
/// What each record of a `DATA` starts with.
pub const RECORD_LEN: usize = 16;
/// The longest message: a channel's.
pub const MESSAGE_MAX: usize = 65536;

/// The IPv4 and UDP headers a datagram travels behind.
const HEADERS: usize = 20 + 8;
/// The datagrams a socket holds between the card and the kernel: as many
/// as a card delivers in one turn, since each turn empties it.
const RX_DATAGRAMS: usize = 64;
/// The datagrams a socket holds on their way to the card, while the card
/// has no room or the neighbour has not said where it is.
const TX_DATAGRAMS: usize = 32;

/// Why a message from the kernel was not done.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Too short, or an operation that is not one.
    Malformed,
    /// A `BIND` for a socket that has a port, or to port 0.
    Bind,
    /// A `SEND` or `CLOSE` for a socket never bound.
    NoSocket,
    /// A datagram longer than one frame carries.
    TooLong,
    /// A destination that is not on the card's network.
    Unreachable,
    /// The socket has [`TX_DATAGRAMS`] waiting already.
    Full,
}

/// A message's operation and socket, when it has both.
pub fn parse_header(m: &[u8]) -> Option<(u32, u64)> {
    if m.len() < HEADER_LEN {
        return None;
    }
    let op = u32::from_le_bytes(m[4..8].try_into().ok()?);
    let sock = u64::from_le_bytes(m[8..16].try_into().ok()?);
    Some((op, sock))
}

/// Make `m` a `DATA` with no records yet.
pub fn start_data(m: &mut Vec<u8>) {
    m.clear();
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&OP_DATA.to_le_bytes());
    m.extend_from_slice(&0u64.to_le_bytes());
}

/// Add a record to the `DATA` message `m`: `data`, from `addr:port`, for
/// socket `sock`.
pub fn push_record(m: &mut Vec<u8>, sock: u64, addr: [u8; 4], port: u16, data: &[u8]) {
    m.extend_from_slice(&sock.to_le_bytes());
    m.extend_from_slice(&addr);
    m.extend_from_slice(&port.to_le_bytes());
    m.extend_from_slice(&(data.len() as u16).to_le_bytes());
    m.extend_from_slice(data);
}

/// What [`Stack::arrived`] put in its message.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Arrived {
    /// No datagram had arrived: there is no message to send.
    Nothing,
    /// Every datagram that had arrived.
    All,
    /// As many as one message holds: there are more to ask for.
    More,
}

/// An interface and the sockets the kernel has on it.
pub struct Stack<D: for<'d> Device<'d>> {
    iface: Interface<'static, D>,
    set: SocketSet<'static>,
    socks: BTreeMap<u64, SocketHandle>,
}

impl<D: for<'d> Device<'d>> Stack<D> {
    /// The stack of `iface`, with no sockets.
    pub fn new(iface: Interface<'static, D>) -> Stack<D> {
        Stack {
            iface,
            set: SocketSet::new(vec![]),
            socks: BTreeMap::new(),
        }
    }

    /// The card.
    pub fn device(&self) -> &D {
        self.iface.device()
    }

    /// The card, to put another in its place.
    pub fn device_mut(&mut self) -> &mut D {
        self.iface.device_mut()
    }

    /// The longest datagram this stack carries: what fits one frame, since
    /// it does not fragment.
    pub fn max_datagram(&self) -> usize {
        self.iface.device().capabilities().ip_mtu() - HEADERS
    }

    /// How many sockets the kernel has here.
    pub fn sockets(&self) -> usize {
        self.socks.len()
    }

    /// Whether `addr` can be sent to: it is on the card's network, or
    /// everybody. There are no routes yet, and a datagram smoltcp cannot
    /// place stays at the head of its socket's queue with every later one
    /// behind it.
    fn reaches(&self, addr: Ipv4Address) -> bool {
        addr.is_broadcast()
            || self.iface.ip_addrs().iter().any(|cidr| match cidr {
                IpCidr::Ipv4(cidr) => cidr.contains_addr(&addr),
                #[allow(unreachable_patterns)]
                _ => false,
            })
    }

    /// Do what a message from the kernel says. `Ok(true)` when there is a
    /// datagram to send, which the next [`poll`](Self::poll) does.
    pub fn request(&mut self, m: &[u8]) -> Result<bool, Refused> {
        let (op, sock) = parse_header(m).ok_or(Refused::Malformed)?;
        match op {
            OP_BIND => {
                let port: [u8; 2] = m[HEADER_LEN..].try_into().map_err(|_| Refused::Malformed)?;
                let port = u16::from_le_bytes(port);
                if port == 0 || self.socks.contains_key(&sock) {
                    return Err(Refused::Bind);
                }
                let room = self.max_datagram();
                let buffer = |datagrams: usize| {
                    UdpSocketBuffer::new(
                        vec![UdpPacketMetadata::EMPTY; datagrams],
                        vec![0u8; datagrams * room],
                    )
                };
                let mut socket = UdpSocket::new(buffer(RX_DATAGRAMS), buffer(TX_DATAGRAMS));
                socket.bind(port).map_err(|_| Refused::Bind)?;
                self.socks.insert(sock, self.set.add(socket));
                Ok(false)
            }
            OP_SEND => {
                if m.len() < DATAGRAM_AT {
                    return Err(Refused::Malformed);
                }
                let handle = *self.socks.get(&sock).ok_or(Refused::NoSocket)?;
                let addr = Ipv4Address::from_bytes(&m[16..20]);
                let port = u16::from_le_bytes([m[20], m[21]]);
                let data = &m[DATAGRAM_AT..];
                if data.len() > self.max_datagram() {
                    return Err(Refused::TooLong);
                }
                // Port 0 is nobody's, and smoltcp says so with a panic in
                // a debug build.
                if port == 0 || !self.reaches(addr) {
                    return Err(Refused::Unreachable);
                }
                let to = IpEndpoint::new(IpAddress::Ipv4(addr), port);
                self.set
                    .get::<UdpSocket>(handle)
                    .send_slice(data, to)
                    .map_err(|_| Refused::Full)?;
                Ok(true)
            }
            OP_CLOSE if m.len() == HEADER_LEN => {
                // A socket closed before it had a port was never told here.
                if let Some(handle) = self.socks.remove(&sock) {
                    self.set.remove(handle);
                }
                Ok(false)
            }
            _ => Err(Refused::Malformed),
        }
    }

    /// Carry the card's frames through the stack, both ways. What arrived
    /// for a socket waits for [`arrived`](Self::arrived), which whoever
    /// polls calls next.
    pub fn poll(&mut self, now: Instant) {
        // What poll has to say about a frame it did not understand is not
        // news.
        let _ = self.iface.poll(&mut self.set, now);
    }

    /// Make `m` the `DATA` of the datagrams that arrived for the sockets,
    /// taking them off the sockets: all of them, or as many as fit one
    /// message.
    pub fn arrived(&mut self, m: &mut Vec<u8>) -> Arrived {
        start_data(m);
        for (&sock, &handle) in &self.socks {
            let mut socket = self.set.get::<UdpSocket>(handle);
            while let Ok((data, _)) = socket.peek() {
                if m.len() + RECORD_LEN + data.len() > MESSAGE_MAX {
                    return Arrived::More;
                }
                let Ok((data, from)) = socket.recv() else {
                    break;
                };
                if let IpAddress::Ipv4(addr) = from.addr {
                    push_record(m, sock, addr.0, from.port, data);
                }
            }
        }
        if m.len() > HEADER_LEN {
            Arrived::All
        } else {
            Arrived::Nothing
        }
    }

    /// Whether the stack has something to do that no frame will remind it
    /// of: a datagram that waits for a neighbour to be asked again.
    pub fn restless(&self, now: Instant) -> bool {
        self.iface.poll_delay(&self.set, now).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::*;
    use smoltcp::wire::{IpProtocol, Ipv4Packet, UdpPacket};

    fn message(op: u32, sock: u64, rest: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&0u32.to_le_bytes());
        m.extend_from_slice(&op.to_le_bytes());
        m.extend_from_slice(&sock.to_le_bytes());
        m.extend_from_slice(rest);
        m
    }

    fn bind(sock: u64, port: u16) -> Vec<u8> {
        message(OP_BIND, sock, &port.to_le_bytes())
    }

    fn send(sock: u64, to: Ipv4Address, port: u16, data: &[u8]) -> Vec<u8> {
        let mut rest = to.0.to_vec();
        rest.extend_from_slice(&port.to_le_bytes());
        rest.extend_from_slice(&[0, 0]);
        rest.extend_from_slice(data);
        message(OP_SEND, sock, &rest)
    }

    /// A stack over a bench's card, which THEIRS has met.
    fn stack() -> (Stack<crate::port::Port>, Window) {
        let mut bench = Bench::new();
        bench.meet();
        let Bench { window, iface, .. } = bench;
        (Stack::new(iface), window)
    }

    /// A datagram as the kernel is told of it: its socket, who sent it,
    /// and what it carried.
    type Told = (u64, [u8; 4], u16, Vec<u8>);

    /// The records of a `DATA` message.
    fn records(m: &[u8]) -> Vec<Told> {
        assert_eq!(parse_header(m), Some((OP_DATA, 0)));
        let (mut rest, mut out) = (&m[HEADER_LEN..], Vec::new());
        while !rest.is_empty() {
            let sock = u64::from_le_bytes(rest[..8].try_into().unwrap());
            let addr: [u8; 4] = rest[8..12].try_into().unwrap();
            let port = u16::from_le_bytes([rest[12], rest[13]]);
            let len = u16::from_le_bytes([rest[14], rest[15]]) as usize;
            out.push((
                sock,
                addr,
                port,
                rest[RECORD_LEN..RECORD_LEN + len].to_vec(),
            ));
            rest = &rest[RECORD_LEN + len..];
        }
        out
    }

    /// Poll, and what the kernel is told arrived.
    fn poll(stack: &mut Stack<crate::port::Port>) -> Vec<Told> {
        stack.poll(Instant::from_millis(1));
        let mut m = Vec::new();
        match stack.arrived(&mut m) {
            Arrived::Nothing => {
                assert_eq!(m.len(), HEADER_LEN);
                Vec::new()
            }
            Arrived::All => records(&m),
            Arrived::More => panic!("more than one message"),
        }
    }

    fn told(sock: u64, port: u16, data: &[u8]) -> Told {
        (sock, THEIR_IP.0, port, data.to_vec())
    }

    /// The UDP datagram in `frame`: its ports and what it carries, when it
    /// is one from us to THEIRS whose checksums hold.
    fn udp_of(frame: &[u8]) -> (u16, u16, Vec<u8>) {
        let ip = ipv4_of(frame);
        let ip = Ipv4Packet::new_checked(&ip[..]).unwrap();
        assert!(ip.verify_checksum());
        assert_eq!(ip.protocol(), IpProtocol::Udp);
        assert_eq!((ip.src_addr(), ip.dst_addr()), (OUR_IP, THEIR_IP));
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert!(udp.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into()));
        (udp.src_port(), udp.dst_port(), udp.payload().to_vec())
    }

    #[test]
    fn the_messages_are_laid_out_as_the_kernel_lays_them_out() {
        // The same bytes `linux-object/src/net/user_stack.rs` builds and
        // reads, written out: neither side can move a field alone.
        let mut data = Vec::new();
        start_data(&mut data);
        push_record(
            &mut data,
            0x0807_0605_0403_0201,
            [10, 0, 9, 1],
            0x1234,
            b"hi",
        );
        assert_eq!(
            data,
            [
                0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 0, 9,
                1, 0x34, 0x12, 2, 0, b'h', b'i'
            ]
        );
        assert_eq!(
            send(7, Ipv4Address([10, 0, 9, 1]), 0x1234, b"hi"),
            [
                0, 0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 10, 0, 9, 1, 0x34, 0x12, 0, 0,
                b'h', b'i'
            ]
        );
        assert_eq!(parse_header(&data), Some((OP_DATA, 0)));
        assert_eq!(parse_header(&data[..HEADER_LEN - 1]), None);
        assert_eq!(
            bind(7, 5000),
            [0, 0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x13]
        );
        assert_eq!((OP_SEND, OP_CLOSE), (2, 3));
        assert_eq!((HEADER_LEN, DATAGRAM_AT, RECORD_LEN), (16, 24, 16));
    }

    #[test]
    fn a_datagram_for_a_bound_port_goes_to_the_kernel_with_who_sent_it() {
        let (mut stack, window) = stack();
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        assert_eq!(stack.request(&bind(6, 7001)), Ok(false));
        assert_eq!(stack.sockets(), 2);
        window.deliver(stack.device(), 0, &udp_frame(4000, 7000, b"hello"));
        window.deliver(stack.device(), 1, &udp_frame(4001, 7001, b""));
        window.deliver(stack.device(), 2, &udp_frame(4000, 7000, b"again"));
        // All three in one message, socket by socket.
        assert_eq!(
            poll(&mut stack),
            [
                told(5, 4000, b"hello"),
                told(5, 4000, b"again"),
                told(6, 4001, b"")
            ]
        );
        // Each is told once.
        assert!(poll(&mut stack).is_empty());
    }

    #[test]
    fn a_send_leaves_by_the_card_from_the_sockets_port() {
        let (mut stack, window) = stack();
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        assert_eq!(stack.request(&send(5, THEIR_IP, 4000, b"reply")), Ok(true));
        assert!(poll(&mut stack).is_empty());
        let turn = turn(stack.device());
        assert_eq!(turn.len(), 1);
        assert_eq!(
            udp_of(&window.sent(&turn[0])),
            (7000, 4000, b"reply".to_vec())
        );
    }

    #[test]
    fn the_longest_datagram_is_what_one_frame_carries() {
        let (mut stack, window) = stack();
        assert_eq!(stack.max_datagram(), 1472);
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        let long = vec![0x5au8; 1473];
        assert_eq!(
            stack.request(&send(5, THEIR_IP, 4000, &long)),
            Err(Refused::TooLong)
        );
        assert_eq!(
            stack.request(&send(5, THEIR_IP, 4000, &long[..1472])),
            Ok(true)
        );
        poll(&mut stack);
        let turn = turn(stack.device());
        assert_eq!(turn.len(), 1);
        assert_eq!(turn[0].len as usize, 1514);
        assert_eq!(udp_of(&window.sent(&turn[0])).2, &long[..1472]);
        // And one that long arrives whole.
        window.deliver(stack.device(), 0, &udp_frame(4000, 7000, &long[..1472]));
        let got = poll(&mut stack);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].3, &long[..1472]);
    }

    #[test]
    fn what_cannot_be_done_is_refused_and_changes_nothing() {
        let (mut stack, _window) = stack();
        assert_eq!(stack.request(&[0; 15]), Err(Refused::Malformed));
        assert_eq!(stack.request(&message(9, 5, &[])), Err(Refused::Malformed));
        assert_eq!(
            stack.request(&message(OP_DATA, 5, &[0; 8])),
            Err(Refused::Malformed)
        );
        assert_eq!(
            stack.request(&message(OP_BIND, 5, &[1])),
            Err(Refused::Malformed)
        );
        assert_eq!(
            stack.request(&message(OP_BIND, 5, &[1, 2, 3])),
            Err(Refused::Malformed)
        );
        assert_eq!(
            stack.request(&message(OP_CLOSE, 5, &[0])),
            Err(Refused::Malformed)
        );
        assert_eq!(
            stack.request(&message(OP_SEND, 5, &[0; 7])),
            Err(Refused::Malformed)
        );
        assert_eq!(stack.request(&bind(5, 0)), Err(Refused::Bind));
        assert_eq!(
            stack.request(&send(5, THEIR_IP, 4000, b"x")),
            Err(Refused::NoSocket)
        );
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        assert_eq!(stack.request(&bind(5, 7001)), Err(Refused::Bind));
        // Port 0, and a network this card is not on.
        assert_eq!(
            stack.request(&send(5, THEIR_IP, 0, b"x")),
            Err(Refused::Unreachable)
        );
        assert_eq!(
            stack.request(&send(5, Ipv4Address([192, 168, 1, 1]), 53, b"x")),
            Err(Refused::Unreachable)
        );
        assert_eq!(stack.sockets(), 1);
        assert!(!stack.restless(Instant::from_millis(1)));
        assert!(turn(stack.device()).is_empty());
    }

    #[test]
    fn a_closed_socket_hears_nothing_and_its_port_can_be_bound_again() {
        let (mut stack, window) = stack();
        // A close for a socket never bound is not an error: the kernel
        // tells every close.
        assert_eq!(stack.request(&message(OP_CLOSE, 5, &[])), Ok(false));
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        assert_eq!(stack.request(&message(OP_CLOSE, 5, &[])), Ok(false));
        assert_eq!(stack.sockets(), 0);
        window.deliver(stack.device(), 0, &udp_frame(4000, 7000, b"late"));
        assert!(poll(&mut stack).is_empty());
        // What smoltcp answers to a datagram for no socket (an ICMP error)
        // is the card's to finish with.
        for m in turn(stack.device()) {
            window.done(stack.device(), &m);
        }
        assert_eq!(stack.request(&bind(6, 7000)), Ok(false));
        window.deliver(stack.device(), 1, &udp_frame(4000, 7000, b"now"));
        let got = poll(&mut stack);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 6);
    }

    #[test]
    fn datagrams_wait_for_the_card_and_past_so_many_are_refused() {
        let (mut stack, window) = stack();
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        // The card has TX slots for the first ones; the rest wait in the
        // socket, and one more than it holds is refused.
        for i in 0..TX + TX_DATAGRAMS {
            assert_eq!(
                stack.request(&send(5, THEIR_IP, 4000, &[i as u8])),
                Ok(true)
            );
            poll(&mut stack);
        }
        assert_eq!(
            stack.request(&send(5, THEIR_IP, 4000, b"x")),
            Err(Refused::Full)
        );
        // As the card finishes with a slot, the next goes, in order.
        let mut seen = Vec::new();
        while seen.len() < TX + TX_DATAGRAMS {
            let turn = turn(stack.device());
            assert!(!turn.is_empty(), "the stack stopped sending");
            for m in &turn {
                seen.push(udp_of(&window.sent(m)).2[0]);
                window.done(stack.device(), m);
            }
            poll(&mut stack);
        }
        assert_eq!(seen, (0..(TX + TX_DATAGRAMS) as u8).collect::<Vec<_>>());
    }

    #[test]
    fn more_datagrams_than_a_message_holds_take_another() {
        let (mut stack, window) = stack();
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        // 48 of the longest: 44 fit one message.
        let long = vec![0x77u8; 1472];
        for i in 0..48u16 {
            let slot = (i % RX as u16) as u32;
            window.deliver(stack.device(), slot, &udp_frame(4000 + i, 7000, &long));
            stack.poll(Instant::from_millis(1));
            turn(stack.device());
        }
        let mut m = Vec::new();
        assert_eq!(stack.arrived(&mut m), Arrived::More);
        assert!(m.len() <= MESSAGE_MAX);
        let first = records(&m);
        assert_eq!(first.len(), 44);
        assert_eq!(stack.arrived(&mut m), Arrived::All);
        let second = records(&m);
        assert_eq!(second.len(), 4);
        // In the order they arrived, none twice.
        let ports: Vec<u16> = first.iter().chain(&second).map(|r| r.2).collect();
        assert_eq!(ports, (4000..4048).collect::<Vec<_>>());
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);
    }

    #[test]
    fn a_datagram_for_a_neighbour_not_met_asks_for_it_and_goes_when_it_answers() {
        let mut bench = Bench::new();
        let Bench { window, iface, .. } = {
            // Nobody met: no `meet()`.
            bench.poll();
            bench
        };
        let mut stack = Stack::new(iface);
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        assert_eq!(stack.request(&send(5, THEIR_IP, 4000, b"wait")), Ok(true));
        poll(&mut stack);
        let asked = turn(stack.device());
        assert_eq!(asked.len(), 1);
        assert!(is_arp_request_for(&window.sent(&asked[0]), THEIR_IP));
        window.done(stack.device(), &asked[0]);
        // The datagram is still to go: the stack must be polled again
        // without a frame to make it.
        assert!(stack.restless(Instant::from_millis(1)));
        // THEIRS answers, and the datagram follows.
        window.deliver(stack.device(), 0, &arp_reply());
        poll(&mut stack);
        let sent: Vec<_> = turn(stack.device())
            .into_iter()
            .filter(|m| m.op == udrivers::net::OP_TX)
            .collect();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            udp_of(&window.sent(&sent[0])),
            (7000, 4000, b"wait".to_vec())
        );
        assert!(!stack.restless(Instant::from_millis(2)));
    }
}
