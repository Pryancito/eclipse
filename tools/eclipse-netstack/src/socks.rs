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
//! SEND    (kernel to stack)  16  [u8; 4] to    20  u16 port  22  u8 hop limit  23  u8 zero  24  the datagram
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
//! kernel, at once, and every `SEND` goes to the card, to travel as many
//! hops as it says (0: as many as smoltcp gives a datagram by itself,
//! which is what a kernel from before the byte had a meaning sends: the
//! socket's options are the kernel's to keep). A `DATA` carries
//! all the datagrams of a turn of the stack, for whichever sockets (its
//! header's socket is zero): the kernel is woken once for them all.
//!
//! A socket that speaks ICMP (what `ping` opens) has no port to open:
//!
//! ```text
//! HEAR    (kernel to stack)
//! ICMP    (kernel to stack)  16  [u8; 4] to  20  u8 hop limit  21  three zero  24  the ICMP message
//! ```
//!
//! While any socket hears, from its `HEAR` to its `CLOSE`, every IPv4
//! packet of ICMP the card brings goes to the kernel whole, as a record of
//! a `DATA` for socket 0 and port 0: which socket it is for is the
//! kernel's to say. An `ICMP` is a message to send for a socket that
//! hears, behind an IPv4 header written here. The stack answers the
//! echoes that arrive, heard or not.
//!
//! A stream socket is the same bargain with more to say, and [`tcp`] has
//! it: the connection is a smoltcp socket here, and an `EVENTS` carries
//! what a turn made of every connection.
//!
//! A [`Stack`] is the interface and those sockets. It makes no system
//! call: whoever has it carries the messages.

use std::collections::{BTreeMap, BTreeSet};

use smoltcp::iface::Interface;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::phy::Device;
use smoltcp::socket::{RawPacketMetadata, RawSocket, RawSocketBuffer};
use smoltcp::socket::{SocketHandle, SocketSet, UdpPacketMetadata, UdpSocket, UdpSocketBuffer};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{IpAddress, IpCidr, IpEndpoint, IpProtocol, IpVersion};
use smoltcp::wire::{Ipv4Address, Ipv4Packet, Ipv4Repr};

pub mod lease;
pub mod tcp;
use tcp::{OP_CONNECT, OP_LISTEN, OP_SHUTDOWN, OP_WINDOW, OP_WRITE};

/// The operations of a message.
pub const OP_BIND: u32 = 1;
pub const OP_SEND: u32 = 2;
pub const OP_CLOSE: u32 = 3;
pub const OP_DATA: u32 = 4;
pub const OP_HEAR: u32 = 11;
pub const OP_ICMP: u32 = 12;

/// The socket of a `DATA` record that is an IPv4 packet of ICMP, for
/// whichever sockets hear.
pub const HEARD: u64 = 0;

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
/// The IPv4 header an ICMP message travels behind.
const IP_HEADER: usize = 20;
/// The hops smoltcp gives a datagram whose socket was told none.
const HOPS: u8 = 64;
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
    /// A `BIND` for a socket that has a port, or to port 0; a `CONNECT`,
    /// a `LISTEN` or a `HEAR` for a socket that is one already.
    Bind,
    /// A message for a socket there is none of: never bound, or a
    /// connection that is over.
    NoSocket,
    /// A datagram longer than one frame carries.
    TooLong,
    /// A destination that is not on the card's network, with no gateway
    /// to send it through.
    Unreachable,
    /// The socket has [`TX_DATAGRAMS`] waiting already, or a `WRITE`
    /// carries more than the connection was said to take.
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
    /// Nothing had arrived: there is no message to send.
    Nothing,
    /// Everything that had arrived.
    All,
    /// As much as one message holds, or all there was of one kind: there
    /// is more to ask for.
    More,
}

/// An interface and the sockets the kernel has on it.
pub struct Stack<D: for<'d> Device<'d>> {
    iface: Interface<'static, D>,
    set: SocketSet<'static>,
    /// The datagram sockets.
    socks: BTreeMap<u64, SocketHandle>,
    /// The sockets that hear ICMP.
    hearing: BTreeSet<u64>,
    /// What hears for them, while there is one: a socket that every
    /// IPv4 packet of ICMP reaches, and that sends theirs.
    ear: Option<SocketHandle>,
    streams: tcp::Streams,
    /// When the last [`poll`](Self::poll) was.
    now: Instant,
    /// Whether [`arrived`](Self::arrived) has given the kernel bytes for a
    /// program since [`brought_bytes`](Self::brought_bytes) last asked.
    brought: bool,
    /// Where what is not for the card's network goes, when anywhere.
    gateway: Option<Ipv4Address>,
    lease: lease::Leasing,
}

impl<D: for<'d> Device<'d>> Stack<D> {
    /// The stack of `iface`, with no sockets.
    pub fn new(iface: Interface<'static, D>) -> Stack<D> {
        Stack {
            iface,
            set: SocketSet::new(vec![]),
            socks: BTreeMap::new(),
            hearing: BTreeSet::new(),
            ear: None,
            streams: tcp::Streams::default(),
            now: Instant::from_millis(0),
            brought: false,
            gateway: None,
            lease: lease::Leasing::default(),
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

    /// How many datagram sockets the kernel has here.
    pub fn sockets(&self) -> usize {
        self.socks.len()
    }

    /// How many sockets hear ICMP.
    pub fn hearing(&self) -> usize {
        self.hearing.len()
    }

    /// Whether `sock` is a socket here, of any kind.
    fn knows(&self, sock: u64) -> bool {
        self.socks.contains_key(&sock) || self.hearing.contains(&sock) || self.streams.knows(sock)
    }

    /// The longest ICMP message this stack carries: what fits one frame.
    fn max_icmp(&self) -> usize {
        self.iface.device().capabilities().ip_mtu() - IP_HEADER
    }

    /// `HEAR`: `sock` hears ICMP from now on.
    fn hear(&mut self, sock: u64) -> Result<bool, Refused> {
        if sock == HEARD || self.knows(sock) {
            return Err(Refused::Bind);
        }
        if self.ear.is_none() {
            let room = self.iface.device().capabilities().ip_mtu();
            let buffer = |packets: usize| {
                RawSocketBuffer::new(
                    vec![RawPacketMetadata::EMPTY; packets],
                    vec![0u8; packets * room],
                )
            };
            let ear = RawSocket::new(
                IpVersion::Ipv4,
                IpProtocol::Icmp,
                buffer(RX_DATAGRAMS),
                buffer(TX_DATAGRAMS),
            );
            self.ear = Some(self.set.add(ear));
        }
        self.hearing.insert(sock);
        Ok(false)
    }

    /// `ICMP`: a message of `sock`'s to send.
    fn icmp(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        if m.len() < DATAGRAM_AT {
            return Err(Refused::Malformed);
        }
        if !self.hearing.contains(&sock) {
            return Err(Refused::NoSocket);
        }
        let ear = self.ear.ok_or(Refused::NoSocket)?;
        let to = Ipv4Address::from_bytes(&m[16..20]);
        let hop_limit = m[20];
        let message = &m[DATAGRAM_AT..];
        if hop_limit == 0 {
            return Err(Refused::Malformed);
        }
        if message.len() > self.max_icmp() {
            return Err(Refused::TooLong);
        }
        // What is sent before the card has an address has nobody to come
        // back to.
        let from = self.address().ok_or(Refused::Unreachable)?.address();
        if !self.reaches(to) {
            return Err(Refused::Unreachable);
        }
        let header = Ipv4Repr {
            src_addr: from,
            dst_addr: to,
            protocol: IpProtocol::Icmp,
            payload_len: message.len(),
            hop_limit,
        };
        let mut socket = self.set.get::<RawSocket>(ear);
        let packet = socket
            .send(IP_HEADER + message.len())
            .map_err(|_| Refused::Full)?;
        header.emit(
            &mut Ipv4Packet::new_unchecked(&mut packet[..IP_HEADER]),
            &ChecksumCapabilities::default(),
        );
        packet[IP_HEADER..].copy_from_slice(message);
        Ok(true)
    }

    /// `CLOSE` of a socket that hears, if `sock` is one. With the last
    /// of them goes the ear.
    fn deaf(&mut self, sock: u64) -> bool {
        if !self.hearing.remove(&sock) {
            return false;
        }
        if self.hearing.is_empty() {
            if let Some(ear) = self.ear.take() {
                self.set.remove(ear);
            }
        }
        true
    }

    /// Whether `addr` can be sent to: it is on the card's network, it is
    /// everybody, or there is a gateway to send it through. A datagram
    /// smoltcp cannot place stays at the head of its socket's queue with
    /// every later one behind it, so one that cannot go is not queued.
    fn reaches(&self, addr: Ipv4Address) -> bool {
        addr.is_broadcast()
            || self.iface.ip_addrs().iter().any(|cidr| match cidr {
                // No address yet is not the network of everything.
                IpCidr::Ipv4(cidr) => !cidr.address().is_unspecified() && cidr.contains_addr(&addr),
                #[allow(unreachable_patterns)]
                _ => false,
            })
            || (self.gateway.is_some() && addr.is_unicast() && !addr.is_loopback())
    }

    /// Do what a message from the kernel says. `Ok(true)` when that left
    /// something to send or to tell the kernel, which the next
    /// [`poll`](Self::poll) and [`arrived`](Self::arrived) do.
    pub fn request(&mut self, m: &[u8]) -> Result<bool, Refused> {
        let (op, sock) = parse_header(m).ok_or(Refused::Malformed)?;
        match op {
            OP_CONNECT => self.connect(sock, m),
            OP_LISTEN => self.listen(sock, m),
            OP_WRITE => self.write(sock, m),
            OP_SHUTDOWN if m.len() == HEADER_LEN => self.shutdown(sock),
            OP_WINDOW => self.window(sock, m),
            OP_HEAR if m.len() == HEADER_LEN => self.hear(sock),
            OP_ICMP => self.icmp(sock, m),
            OP_BIND => {
                let port: [u8; 2] = m[HEADER_LEN..].try_into().map_err(|_| Refused::Malformed)?;
                let port = u16::from_le_bytes(port);
                if port == 0 || self.knows(sock) {
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
                self.hops(handle, m[22]);
                self.set
                    .get::<UdpSocket>(handle)
                    .send_slice(data, to)
                    .map_err(|_| Refused::Full)?;
                Ok(true)
            }
            OP_CLOSE if m.len() == HEADER_LEN => {
                // A socket closed before it had a port was never told here.
                if let Some(handle) = self.socks.remove(&sock) {
                    // What it sent and has not left goes first: a program
                    // that sends and ends has sent, and its `CLOSE` can
                    // be here before the stack had a turn. The `SEND`
                    // said there would be frames; this adds none.
                    let _ = self.iface.poll(&mut self.set, self.now);
                    self.set.remove(handle);
                    return Ok(false);
                }
                if self.deaf(sock) {
                    return Ok(false);
                }
                Ok(self.close_stream(sock).unwrap_or(false))
            }
            _ => Err(Refused::Malformed),
        }
    }

    /// Have the next datagram of the socket `handle` travel `hop_limit`
    /// hops at most, or as many as smoltcp gives one when that is 0.
    ///
    /// smoltcp keeps one hop limit for a socket and writes it when a
    /// datagram leaves, not when it is queued. So when the limit is to be
    /// another, the datagrams that wait leave first, with theirs: a
    /// `traceroute` sends one hop further each time from one socket, and
    /// several of its messages can be here before a turn of the stack.
    /// One that still cannot leave then (its neighbour has not answered,
    /// or the card has no room for a frame) goes with the new limit.
    fn hops(&mut self, handle: SocketHandle, hop_limit: u8) {
        let wanted = (hop_limit != 0).then_some(hop_limit);
        let has = self.set.get::<UdpSocket>(handle).hop_limit();
        // Said or not, smoltcp's own is this.
        if wanted.unwrap_or(HOPS) == has.unwrap_or(HOPS) {
            return;
        }
        let _ = self.iface.poll(&mut self.set, self.now);
        self.set.get::<UdpSocket>(handle).set_hop_limit(wanted);
    }

    /// Carry the card's frames through the stack, both ways. What arrived
    /// for a socket waits for [`arrived`](Self::arrived), which whoever
    /// polls calls next.
    pub fn poll(&mut self, now: Instant) {
        self.now = now;
        // What poll has to say about a frame it did not understand is not
        // news.
        let _ = self.iface.poll(&mut self.set, now);
        self.lease_turn();
    }

    /// Make `m` the next message for the kernel of what the last
    /// [`poll`](Self::poll) brought: a `DATA` of the datagrams that
    /// arrived, taken off their sockets, and once there are no more of
    /// those an `EVENTS` of what became of the connections.
    pub fn arrived(&mut self, m: &mut Vec<u8>) -> Arrived {
        match self.datagrams(m) {
            Arrived::Nothing => self.events(m),
            // The connections have not been asked yet.
            Arrived::All if !self.streams.is_empty() => Arrived::More,
            arrived => arrived,
        }
    }

    /// Whether the messages [`arrived`](Self::arrived) made since this was
    /// last asked carried bytes for a program -- a datagram, or what came
    /// on a connection -- and not only news of the sockets: what a program
    /// may have an answer to.
    pub fn brought_bytes(&mut self) -> bool {
        core::mem::take(&mut self.brought)
    }

    /// Make `m` the `DATA` of the datagrams that arrived for the sockets,
    /// taking them off the sockets: all of them, or as many as fit one
    /// message.
    fn datagrams(&mut self, m: &mut Vec<u8>) -> Arrived {
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
                    self.brought = true;
                }
            }
        }
        if let Some(ear) = self.ear {
            let room = self.iface.device().capabilities().ip_mtu();
            let mut socket = self.set.get::<RawSocket>(ear);
            while socket.can_recv() {
                if m.len() + RECORD_LEN + room > MESSAGE_MAX {
                    return Arrived::More;
                }
                let Ok(packet) = socket.recv() else {
                    break;
                };
                if let Ok(header) = Ipv4Packet::new_checked(packet) {
                    push_record(m, HEARD, header.src_addr().0, 0, packet);
                    self.brought = true;
                }
            }
        }
        if m.len() > HEADER_LEN {
            Arrived::All
        } else {
            Arrived::Nothing
        }
    }

    /// How long until the stack has something to do that no frame and no
    /// message will remind it of: a neighbour to ask again, a segment to
    /// send again, a connection to give up on. `None` when there is
    /// nothing of the kind.
    pub fn idle_for(&self, now: Instant) -> Option<Duration> {
        self.iface.poll_delay(&self.set, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::*;
    use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr, UdpPacket};

    pub(super) fn message(op: u32, sock: u64, rest: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&0u32.to_le_bytes());
        m.extend_from_slice(&op.to_le_bytes());
        m.extend_from_slice(&sock.to_le_bytes());
        m.extend_from_slice(rest);
        m
    }

    pub(super) fn bind(sock: u64, port: u16) -> Vec<u8> {
        message(OP_BIND, sock, &port.to_le_bytes())
    }

    /// A `SEND` as a kernel that says no hop limit writes it.
    pub(super) fn send(sock: u64, to: Ipv4Address, port: u16, data: &[u8]) -> Vec<u8> {
        send_hops(sock, to, port, 0, data)
    }

    pub(super) fn send_hops(
        sock: u64,
        to: Ipv4Address,
        port: u16,
        hop_limit: u8,
        data: &[u8],
    ) -> Vec<u8> {
        let mut rest = to.0.to_vec();
        rest.extend_from_slice(&port.to_le_bytes());
        rest.extend_from_slice(&[hop_limit, 0]);
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
        assert_eq!(hear(7), [0, 0, 0, 0, 11, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            icmp(7, Ipv4Address([10, 0, 9, 1]), 64, &[8, 0]),
            [0, 0, 0, 0, 12, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 10, 0, 9, 1, 64, 0, 0, 0, 8, 0]
        );
        assert_eq!(HEARD, 0);
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

    fn hear(sock: u64) -> Vec<u8> {
        message(OP_HEAR, sock, &[])
    }

    fn icmp(sock: u64, to: Ipv4Address, hop_limit: u8, icmp: &[u8]) -> Vec<u8> {
        let mut rest = to.0.to_vec();
        rest.extend_from_slice(&[hop_limit, 0, 0, 0]);
        rest.extend_from_slice(icmp);
        message(OP_ICMP, sock, &rest)
    }

    /// An echo with this identifier, as a program writes it.
    fn echo(ident: u16, data: &[u8]) -> Vec<u8> {
        let repr = Icmpv4Repr::EchoRequest {
            ident,
            seq_no: 1,
            data,
        };
        let mut bytes = vec![0u8; repr.buffer_len()];
        repr.emit(
            &mut Icmpv4Packet::new_unchecked(&mut bytes[..]),
            &ChecksumCapabilities::default(),
        );
        bytes
    }

    /// The ICMP message of an IPv4 packet from THEIRS to us.
    fn heard_of(packet: &[u8]) -> Vec<u8> {
        let ip = Ipv4Packet::new_checked(packet).unwrap();
        assert!(ip.verify_checksum());
        assert_eq!(ip.protocol(), IpProtocol::Icmp);
        assert_eq!((ip.src_addr(), ip.dst_addr()), (THEIR_IP, OUR_IP));
        ip.payload().to_vec()
    }

    #[test]
    fn while_a_socket_hears_every_packet_of_icmp_goes_to_the_kernel_whole() {
        let (mut stack, window) = stack();
        // Nobody hears: the answer to an echo nobody sent is dropped.
        window.deliver(stack.device(), 0, &echo_reply(7, 1, b"early"));
        assert!(poll(&mut stack).is_empty());
        assert_eq!(stack.request(&hear(5)), Ok(false));
        assert_eq!(stack.request(&hear(6)), Ok(false));
        assert_eq!(stack.hearing(), 2);
        window.deliver(stack.device(), 1, &echo_reply(7, 1, b"pong"));
        // Once, for whichever sockets: socket 0, from THEIRS, no port.
        let got = poll(&mut stack);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].0, got[0].1, got[0].2), (HEARD, THEIR_IP.0, 0));
        let answer = heard_of(&got[0].3);
        let answer = Icmpv4Packet::new_checked(&answer[..]).unwrap();
        assert_eq!(
            Icmpv4Repr::parse(&answer, &ChecksumCapabilities::default()),
            Ok(Icmpv4Repr::EchoReply {
                ident: 7,
                seq_no: 1,
                data: b"pong"
            })
        );
        assert!(poll(&mut stack).is_empty());
        // An echo for us is heard too, and answered here all the same.
        window.deliver(stack.device(), 2, &echo_request(OUR_IP, 9, 3, b"you?"));
        let got = poll(&mut stack);
        assert_eq!(got.len(), 1);
        assert_eq!(heard_of(&got[0].3)[0], 8);
        let turn = turn(stack.device());
        let answered = turn
            .iter()
            .filter(|m| m.op == udrivers::net::OP_TX)
            .map(|m| window.sent(m))
            .filter(|frame| frame.len() > 14 + 20 && frame[14 + 9] == 1 && frame[14 + 20] == 0)
            .count();
        assert_eq!(answered, 1);
    }

    #[test]
    fn an_icmp_message_leaves_by_the_card_behind_a_header_of_ours() {
        let (mut stack, window) = stack();
        assert_eq!(stack.request(&hear(5)), Ok(false));
        let ping = echo(0x1234, b"anybody there?");
        assert_eq!(stack.request(&icmp(5, THEIR_IP, 9, &ping)), Ok(true));
        assert!(poll(&mut stack).is_empty());
        let turn = turn(stack.device());
        assert_eq!(turn.len(), 1);
        let ip = ipv4_of(&window.sent(&turn[0]));
        let ip = Ipv4Packet::new_checked(&ip[..]).unwrap();
        assert!(ip.verify_checksum());
        assert_eq!(ip.protocol(), IpProtocol::Icmp);
        assert_eq!((ip.src_addr(), ip.dst_addr()), (OUR_IP, THEIR_IP));
        assert_eq!(ip.hop_limit(), 9);
        // The message is the program's, to the byte.
        assert_eq!(ip.payload(), &ping[..]);
    }

    #[test]
    fn an_icmp_message_that_cannot_go_is_refused() {
        let (mut stack, _window) = stack();
        let ping = echo(1, b"x");
        // A socket that does not hear has nothing to send.
        assert_eq!(
            stack.request(&icmp(5, THEIR_IP, 64, &ping)),
            Err(Refused::NoSocket)
        );
        assert_eq!(stack.request(&bind(4, 7000)), Ok(false));
        assert_eq!(
            stack.request(&icmp(4, THEIR_IP, 64, &ping)),
            Err(Refused::NoSocket)
        );
        // A socket is one thing, and socket 0 is not a socket.
        assert_eq!(stack.request(&hear(4)), Err(Refused::Bind));
        assert_eq!(stack.request(&hear(HEARD)), Err(Refused::Bind));
        assert_eq!(stack.request(&hear(5)), Ok(false));
        assert_eq!(stack.request(&hear(5)), Err(Refused::Bind));
        assert_eq!(stack.request(&bind(5, 7001)), Err(Refused::Bind));
        assert_eq!(
            stack.request(&message(OP_HEAR, 6, &[0])),
            Err(Refused::Malformed)
        );
        assert_eq!(
            stack.request(&message(OP_ICMP, 5, &[0; 7])),
            Err(Refused::Malformed)
        );
        // No hops is nowhere.
        assert_eq!(
            stack.request(&icmp(5, THEIR_IP, 0, &ping)),
            Err(Refused::Malformed)
        );
        assert_eq!(
            stack.request(&icmp(5, Ipv4Address([192, 168, 1, 1]), 64, &ping)),
            Err(Refused::Unreachable)
        );
        // One frame carries a message of the datagram's length and its
        // UDP header's, and no more.
        let longest = echo(1, &vec![0x55; stack.max_datagram()]);
        assert_eq!(stack.request(&icmp(5, THEIR_IP, 64, &longest)), Ok(true));
        let longer = echo(1, &vec![0x55; stack.max_datagram() + 1]);
        assert_eq!(
            stack.request(&icmp(5, THEIR_IP, 64, &longer)),
            Err(Refused::TooLong)
        );
    }

    #[test]
    fn with_the_last_socket_that_hears_goes_the_ear() {
        let (mut stack, window) = stack();
        assert_eq!(stack.request(&hear(5)), Ok(false));
        assert_eq!(stack.request(&hear(6)), Ok(false));
        assert_eq!(stack.request(&message(OP_CLOSE, 5, &[])), Ok(false));
        window.deliver(stack.device(), 0, &echo_reply(7, 1, b"one"));
        assert_eq!(poll(&mut stack).len(), 1);
        assert_eq!(stack.request(&message(OP_CLOSE, 6, &[])), Ok(false));
        assert_eq!(stack.hearing(), 0);
        window.deliver(stack.device(), 1, &echo_reply(7, 2, b"two"));
        assert!(poll(&mut stack).is_empty());
        assert_eq!(
            stack.request(&icmp(6, THEIR_IP, 64, &echo(1, b"x"))),
            Err(Refused::NoSocket)
        );
        // And the next to hear, hears.
        assert_eq!(stack.request(&hear(7)), Ok(false));
        window.deliver(stack.device(), 2, &echo_reply(7, 3, b"three"));
        assert_eq!(poll(&mut stack).len(), 1);
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
    fn a_datagram_travels_as_many_hops_as_its_send_says() {
        let (mut stack, window) = stack();
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        // The hop limit and the datagram of each frame a turn sent, which
        // the card then finishes with.
        let left = |stack: &mut Stack<crate::port::Port>| -> Vec<(u8, Vec<u8>)> {
            assert!(poll(stack).is_empty());
            let mut frames = Vec::new();
            for m in turn(stack.device()) {
                if m.op == udrivers::net::OP_TX {
                    let frame = window.sent(&m);
                    frames.push((ipv4_of(&frame)[8], udp_of(&frame).2));
                }
                window.done(stack.device(), &m);
            }
            frames
        };
        let hops = |sent: &[(u8, &[u8])]| -> Vec<(u8, Vec<u8>)> {
            sent.iter().map(|(h, d)| (*h, d.to_vec())).collect()
        };
        // Not said: smoltcp's own.
        let said = send(5, THEIR_IP, 4000, b"any");
        assert_eq!(stack.request(&said), Ok(true));
        assert_eq!(left(&mut stack), hops(&[(64, b"any")]));
        // Said, and said again as another before the stack had a turn:
        // each leaves with its own, in order. What `traceroute` sends.
        // (Two at a time: the bench's card has two frames to send in.)
        let probes: [(u8, &[u8]); 4] = [(1, b"one"), (2, b"two"), (2, b"too"), (3, b"six")];
        for pair in probes.chunks(2) {
            for (limit, data) in pair {
                let said = send_hops(5, THEIR_IP, 4000, *limit, data);
                assert_eq!(stack.request(&said), Ok(true));
            }
            assert_eq!(left(&mut stack), hops(pair));
        }
        // Not said again, after it was: smoltcp's own again; and the most
        // there is.
        assert_eq!(stack.request(&said), Ok(true));
        assert_eq!(left(&mut stack), hops(&[(64, b"any")]));
        let far = send_hops(5, THEIR_IP, 4000, 255, b"far");
        assert_eq!(stack.request(&far), Ok(true));
        assert_eq!(left(&mut stack), hops(&[(255, b"far")]));
    }

    #[test]
    fn what_a_socket_sent_leaves_though_it_closes_at_once() {
        let (mut stack, window) = stack();
        assert_eq!(stack.request(&bind(5, 7000)), Ok(false));
        assert_eq!(stack.request(&send(5, THEIR_IP, 4000, b"last")), Ok(true));
        assert_eq!(stack.request(&send(5, THEIR_IP, 4001, b"words")), Ok(true));
        // The close comes before the stack had a turn.
        assert_eq!(stack.request(&message(OP_CLOSE, 5, &[])), Ok(false));
        assert_eq!(stack.sockets(), 0);
        assert!(poll(&mut stack).is_empty());
        let sent: Vec<_> = turn(stack.device())
            .iter()
            .map(|m| udp_of(&window.sent(m)))
            .collect();
        assert_eq!(
            sent,
            [
                (7000, 4000, b"last".to_vec()),
                (7000, 4001, b"words".to_vec())
            ]
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
        assert_eq!(stack.idle_for(Instant::from_millis(1)), None);
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
        assert!(!stack.brought_bytes(), "nothing given yet");
        assert_eq!(stack.arrived(&mut m), Arrived::More);
        assert!(stack.brought_bytes(), "a program has something to answer");
        assert!(m.len() <= MESSAGE_MAX);
        let first = records(&m);
        assert_eq!(first.len(), 44);
        assert_eq!(stack.arrived(&mut m), Arrived::All);
        let second = records(&m);
        assert_eq!(second.len(), 4);
        // In the order they arrived, none twice.
        let ports: Vec<u16> = first.iter().chain(&second).map(|r| r.2).collect();
        assert_eq!(ports, (4000..4048).collect::<Vec<_>>());
        assert!(stack.brought_bytes());
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);
        assert!(!stack.brought_bytes());
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
        assert!(stack.idle_for(Instant::from_millis(1)).is_some());
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
        assert_eq!(stack.idle_for(Instant::from_millis(2)), None);
    }
}
