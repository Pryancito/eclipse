//! Raw sockets of IPv6 for the kernel's sockets: what a program opens
//! with `socket(AF_INET6, SOCK_RAW, protocol)` when the protocol is not
//! ICMPv6, which has its own bargain ([`super::icmp6`]).
//!
//! ```text
//! RAW6     (kernel to stack)  16  u8 protocol  17  three zero
//! PACKET6  (kernel to stack)  16  [u8; 16] to  32  u8 hop limit  33  u8 protocol
//!                             34  u16 where the sum goes  36  what the packet carries
//! ```
//!
//! A socket that speaks a protocol says so with `RAW6`, and is gone with
//! its `CLOSE`. While any does, every IPv6 packet of that protocol the
//! card brings goes to the kernel whole, as a record of a `DATA6` for
//! socket 0 and port 0, like what is heard of ICMPv6: which sockets it
//! is for is the kernel's to say, by the next header its header names.
//! The stack goes on doing with the packet what it did. Protocol 255 is
//! the one nothing arrives for: a socket of it only sends.
//!
//! A `PACKET6` is what a packet carries, to send for such a socket
//! behind an IPv6 header written here, as for ICMPv6: no program writes
//! that header, and which of the card's addresses the packet is from is
//! the stack's to choose. A sum that covers that address is the stack's
//! to write too (`IPV6_CHECKSUM`): the message says how far into what
//! the packet carries its two bytes are, or [`NO_SUM`].

use std::collections::{BTreeMap, VecDeque};

use smoltcp::phy::Device;
use smoltcp::socket::{RawPacketMetadata, RawSocket, RawSocketBuffer, SocketHandle};
use smoltcp::wire::{IpAddress, IpProtocol, IpRepr, IpVersion, Ipv6Address, Ipv6Packet};

use super::{Refused, Stack};
use super::{DATAGRAM6_AT, HEARD, RX_DATAGRAMS, TX_DATAGRAMS};

/// The operations of a message.
pub const OP_RAW6: u32 = 34;
pub const OP_PACKET6: u32 = 35;

/// What a `PACKET6` says for a packet with no sum to write.
pub const NO_SUM: u16 = 0xffff;

/// ICMPv6, whose sockets say `HEAR6`.
const ICMP6: u8 = 58;
/// `IPPROTO_RAW`: no packet is of it.
const NONE: u8 = 255;
/// How many protocols one socket sends packets of.
const MOUTHS: usize = 4;
/// The IPv6 header a packet starts with.
const IP_HEADER: usize = 40;

/// A socket of whole IPv6 packets of `protocol`, the longest of `room`
/// bytes, that holds so many that arrived and so many to send.
fn socket(protocol: u8, room: usize, arrived: usize, to_send: usize) -> RawSocket<'static> {
    let buffer = |packets: usize| {
        RawSocketBuffer::new(
            vec![RawPacketMetadata::EMPTY; packets],
            vec![0u8; packets * room],
        )
    };
    RawSocket::new(
        IpVersion::Ipv6,
        IpProtocol::from(protocol),
        buffer(arrived),
        buffer(to_send),
    )
}

/// The sum of the Internet over `bytes`, not yet turned over, added to
/// `sum`.
fn add(mut sum: u32, bytes: &[u8]) -> u32 {
    for pair in bytes.chunks(2) {
        let low = pair.get(1).copied().unwrap_or(0);
        sum += u32::from(u16::from_be_bytes([pair[0], low]));
    }
    sum
}

/// The sum that IPv6 asks of what a packet carries (RFC 8200, 8.1):
/// over the addresses, the length, the protocol and `carried`, whose
/// place for the sum holds zero.
pub fn sum6(from: &[u8; 16], to: &[u8; 16], protocol: u8, carried: &[u8]) -> u16 {
    let mut sum = add(add(0, from), to);
    sum = add(sum, &(carried.len() as u32).to_be_bytes());
    sum = add(sum, &[0, 0, 0, protocol]);
    sum = add(sum, carried);
    while sum >> 16 != 0 {
        sum = (sum >> 16) + (sum & 0xffff);
    }
    !(sum as u16)
}

/// A raw socket of the kernel's.
struct Raw {
    protocol: u8,
    /// What it sends by, one for each protocol it has sent a packet of.
    mouths: Vec<(u8, SocketHandle)>,
}

/// The raw sockets of IPv6, and what hears for them.
#[derive(Default)]
pub(super) struct Raws6 {
    socks: BTreeMap<u64, Raw>,
    /// For each protocol a socket speaks, the socket every packet of it
    /// reaches, and how many speak it.
    ears: BTreeMap<u8, (SocketHandle, usize)>,
    /// What the card brought that the kernel has not been told.
    pub(super) heard: VecDeque<Vec<u8>>,
}

impl Raws6 {
    /// Whether `sock` is a raw socket of IPv6.
    pub(super) fn knows(&self, sock: u64) -> bool {
        self.socks.contains_key(&sock)
    }
}

impl<D: for<'d> Device<'d> + crate::port::Listened> Stack<D> {
    /// How many raw sockets of IPv6 there are.
    pub fn raws6(&self) -> usize {
        self.raws6.socks.len()
    }

    /// `RAW6`: `sock` speaks the protocol the message names from now on.
    pub(super) fn raw6_open(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        let &[protocol, 0, 0, 0] = &m[super::HEADER_LEN..] else {
            return Err(Refused::Malformed);
        };
        if protocol == ICMP6 {
            return Err(Refused::Malformed);
        }
        if sock == HEARD || self.knows(sock) {
            return Err(Refused::Bind);
        }
        if protocol != NONE {
            let room = self.iface.device().capabilities().ip_mtu();
            let set = &mut self.set;
            let ear = self
                .raws6
                .ears
                .entry(protocol)
                .or_insert_with(|| (set.add(socket(protocol, room, RX_DATAGRAMS, 0)), 0));
            ear.1 += 1;
        }
        let mouths = Vec::new();
        self.raws6.socks.insert(sock, Raw { protocol, mouths });
        Ok(false)
    }

    /// `PACKET6`: what a packet of `sock`'s carries, to send.
    pub(super) fn raw6_packet(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        if m.len() < DATAGRAM6_AT {
            return Err(Refused::Malformed);
        }
        if !self.raws6.knows(sock) {
            return Err(Refused::NoSocket);
        }
        let to = Ipv6Address::from_bytes(&m[16..32]);
        let (hop_limit, protocol) = (m[32], m[33]);
        let sum_at = u16::from_le_bytes([m[34], m[35]]);
        let carried = &m[DATAGRAM6_AT..];
        if hop_limit == 0 {
            return Err(Refused::Malformed);
        }
        if sum_at != NO_SUM && usize::from(sum_at) + 2 > carried.len() {
            return Err(Refused::Malformed);
        }
        let room = self.iface.device().capabilities().ip_mtu();
        if IP_HEADER + carried.len() > room {
            return Err(Refused::TooLong);
        }
        if !self.reaches6(to) {
            return Err(Refused::Unreachable);
        }
        // From the address smoltcp would send a datagram there from.
        let asked = IpRepr::Unspecified {
            src_addr: IpAddress::Unspecified,
            dst_addr: IpAddress::Ipv6(to),
            protocol: IpProtocol::from(protocol),
            payload_len: carried.len(),
            hop_limit,
        };
        let Ok(IpRepr::Ipv6(header)) = asked.lower(self.iface.ip_addrs()) else {
            return Err(Refused::Unreachable);
        };
        let raw = self.raws6.socks.get_mut(&sock).ok_or(Refused::NoSocket)?;
        let handle = match raw.mouths.iter().find(|(of, _)| *of == protocol) {
            Some(&(_, handle)) => handle,
            None if raw.mouths.len() < MOUTHS => {
                let handle = self.set.add(socket(protocol, room, 0, TX_DATAGRAMS));
                raw.mouths.push((protocol, handle));
                handle
            }
            None => return Err(Refused::Full),
        };
        let mut socket = self.set.get::<RawSocket>(handle);
        let packet = socket
            .send(IP_HEADER + carried.len())
            .map_err(|_| Refused::Full)?;
        let (head, body) = packet.split_at_mut(IP_HEADER);
        header.emit(&mut Ipv6Packet::new_unchecked(head));
        body.copy_from_slice(carried);
        if sum_at != NO_SUM {
            let at = usize::from(sum_at);
            body[at..at + 2].fill(0);
            let sum = sum6(&header.src_addr.0, &to.0, protocol, body);
            body[at..at + 2].copy_from_slice(&sum.to_be_bytes());
        }
        Ok(true)
    }

    /// `CLOSE` of a raw socket of IPv6, if `sock` is one. What it sent
    /// and can leave now does; with the last that speaks its protocol,
    /// what arrived of it is nobody's.
    pub(super) fn raw6_close(&mut self, sock: u64) -> bool {
        let Some(raw) = self.raws6.socks.remove(&sock) else {
            return false;
        };
        let _ = self.iface.poll(&mut self.set, self.now);
        for (_, handle) in raw.mouths {
            self.set.remove(handle);
        }
        if let Some(ear) = self.raws6.ears.get_mut(&raw.protocol) {
            ear.1 -= 1;
            if ear.1 == 0 {
                let (handle, _) = self.raws6.ears.remove(&raw.protocol).expect("it is there");
                self.set.remove(handle);
                let of = |packet: &Vec<u8>| packet[6] == raw.protocol;
                self.raws6.heard.retain(|packet| !of(packet));
            }
        }
        true
    }

    /// After a poll: what arrived for the raw sockets of IPv6 comes off
    /// the ears, to wait for the kernel with what was heard of ICMPv6.
    /// Past [`RX_DATAGRAMS`] a packet is lost, as on a socket's queue.
    pub(super) fn raw6_turn(&mut self) {
        for &(handle, _) in self.raws6.ears.values() {
            let mut socket = self.set.get::<RawSocket>(handle);
            while let Ok(packet) = socket.recv() {
                if packet.len() >= IP_HEADER && self.raws6.heard.len() < RX_DATAGRAMS {
                    self.raws6.heard.push_back(packet.to_vec());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::*;
    use crate::socks::six::tests::{frames, here_is, ip6_frame, records6, stack, OURS_LINK};
    use crate::socks::tests::{bind, message};
    use crate::socks::{Arrived, OP_CLOSE, OP_HEAR};
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetFrame, UdpPacket};

    const UDP: u8 = 17;
    const NEAR: Ipv6Address = Ipv6Address([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

    /// A `RAW6` as the kernel writes it.
    fn raw6_open(sock: u64, protocol: u8) -> Vec<u8> {
        message(OP_RAW6, sock, &[protocol, 0, 0, 0])
    }

    /// A `PACKET6` as the kernel writes it.
    fn packet6(
        sock: u64,
        to: Ipv6Address,
        hops: u8,
        protocol: u8,
        sum_at: u16,
        carried: &[u8],
    ) -> Vec<u8> {
        let mut rest = to.0.to_vec();
        rest.extend_from_slice(&[hops, protocol]);
        rest.extend_from_slice(&sum_at.to_le_bytes());
        rest.extend_from_slice(carried);
        message(OP_PACKET6, sock, &rest)
    }

    /// A datagram from port 1 to port 2 as a program writes it by hand,
    /// with rubbish where its sum goes.
    fn datagram(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0, 1, 0, 2];
        out.extend_from_slice(&((8 + data.len()) as u16).to_be_bytes());
        out.extend_from_slice(&[0xde, 0xad]);
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn the_messages_of_raw_ipv6_are_laid_out_as_the_kernel_lays_them_out() {
        assert_eq!((OP_RAW6, OP_PACKET6), (34, 35));
        assert_eq!(
            raw6_open(0x0102, 89),
            [0, 0, 0, 0, 34, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 89, 0, 0, 0]
        );
        let m = packet6(0x0102, NEAR, 9, 89, 0x0304, &[7, 8]);
        let mut said = vec![0, 0, 0, 0, 35, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0];
        said.extend_from_slice(&NEAR.0);
        said.extend_from_slice(&[9, 89, 4, 3, 7, 8]);
        assert_eq!(m, said);
    }

    #[test]
    fn a_raw_packet_of_ipv6_leaves_behind_a_header_of_ours_with_the_sum_it_asked_for() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        assert_eq!(stack.request(&raw6_open(5, UDP)), Ok(false));
        assert_eq!(stack.raws6(), 1);
        let carried = datagram(b"by hand");
        // The neighbour is asked for first; then the packet goes, from
        // the address that goes there, with the sum written where the
        // socket said.
        assert_eq!(
            stack.request(&packet6(5, NEAR, 9, UDP, 6, &carried)),
            Ok(true)
        );
        assert_eq!(frames(&mut stack, &window, 1000).len(), 1);
        window.deliver(stack.device(), 0, &here_is(NEAR, OURS_LINK));
        let sent = frames(&mut stack, &window, 1010);
        assert_eq!(sent.len(), 1);
        let eth = EthernetFrame::new_checked(&sent[0][..]).unwrap();
        let ip = Ipv6Packet::new_checked(eth.payload()).unwrap();
        assert_eq!((ip.src_addr(), ip.dst_addr()), (OURS_LINK, NEAR));
        assert_eq!((ip.hop_limit(), u8::from(ip.next_header())), (9, UDP));
        assert_eq!(ip.payload()[..6], carried[..6]);
        assert_eq!(ip.payload()[8..], carried[8..]);
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert!(udp.verify_checksum(&IpAddress::Ipv6(OURS_LINK), &IpAddress::Ipv6(NEAR)));

        // With no sum asked for, and of another protocol than the
        // socket's: as it was made.
        let odd = [1u8, 2, 3, 4, 5];
        assert_eq!(
            stack.request(&packet6(5, NEAR, 64, 89, NO_SUM, &odd)),
            Ok(true)
        );
        let sent = frames(&mut stack, &window, 1020);
        assert_eq!(sent.len(), 1);
        let ip = Ipv6Packet::new_checked(&sent[0][14..]).unwrap();
        assert_eq!((u8::from(ip.next_header()), ip.payload()), (89, &odd[..]));

        // What cannot be done is refused, and nothing leaves.
        let room = stack.max_datagram6() + 8;
        assert_eq!(stack.request(&bind(7, 5000)), Ok(false));
        assert_eq!(stack.request(&message(OP_HEAR, 4, &[])), Ok(false));
        let good = packet6(5, NEAR, 64, UDP, 6, &carried);
        for (m, why) in [
            (packet6(8, NEAR, 64, UDP, 6, &carried), Refused::NoSocket),
            (packet6(7, NEAR, 64, UDP, 6, &carried), Refused::NoSocket),
            (packet6(5, NEAR, 0, UDP, 6, &carried), Refused::Malformed),
            // The sum has to fit in what the packet carries.
            (packet6(5, NEAR, 64, UDP, 14, &carried), Refused::Malformed),
            (good[..35].to_vec(), Refused::Malformed),
            (
                packet6(5, Ipv6Address::UNSPECIFIED, 64, UDP, 6, &carried),
                Refused::Unreachable,
            ),
            (
                packet6(5, NEAR, 64, UDP, NO_SUM, &vec![0; room + 1]),
                Refused::TooLong,
            ),
            (raw6_open(6, ICMP6), Refused::Malformed),
            (raw6_open(5, 6), Refused::Bind),
            (raw6_open(7, 6), Refused::Bind),
            (raw6_open(4, 6), Refused::Bind),
            (raw6_open(HEARD, 6), Refused::Bind),
            (message(OP_RAW6, 6, &[6, 0, 0]), Refused::Malformed),
        ] {
            assert_eq!(stack.request(&m), Err(why), "{m:?}");
        }
        assert_eq!(frames(&mut stack, &window, 1030).len(), 0);
        // The longest that fits does go.
        let longest = packet6(5, NEAR, 64, UDP, NO_SUM, &vec![0; room]);
        assert_eq!(stack.request(&longest), Ok(true));
        assert_eq!(frames(&mut stack, &window, 1040).len(), 1);
    }

    #[test]
    fn what_arrives_of_a_raw_ipv6_sockets_protocol_is_told_whole() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        let frame = |protocol: u8, carried: &[u8]| {
            ip6_frame(NEAR, OURS_LINK, 57, IpProtocol::from(protocol), carried)
        };
        let mut m = Vec::new();
        // Nobody speaks the protocol: nothing is told.
        window.deliver(stack.device(), 0, &frame(89, b"early"));
        stack.poll(Instant::from_millis(200));
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);

        assert_eq!(stack.request(&raw6_open(5, 89)), Ok(false));
        assert_eq!(stack.request(&raw6_open(6, 89)), Ok(false));
        // One that only sends.
        assert_eq!(stack.request(&raw6_open(7, 255)), Ok(false));
        assert_eq!(stack.raws6(), 3);
        // Once, for whichever sockets: socket 0, from who sent it, no
        // port, the packet as it came. Of another protocol, nothing.
        let heard = frame(89, b"hello, neighbours");
        for (ms, frame) in [
            (300, &heard),
            (310, &frame(90, b"not this")),
            (320, &frame(255, b"nor this")),
        ] {
            window.deliver(stack.device(), 0, frame);
            stack.poll(Instant::from_millis(ms));
        }
        assert_eq!(stack.arrived(&mut m), Arrived::All);
        assert_eq!(records6(&m), [(HEARD, NEAR, 0, heard[14..].to_vec())]);
        assert!(stack.brought_bytes());
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);

        // While one still speaks it, it is heard; after the last, what
        // waited is nobody's, and the numbers are free.
        assert_eq!(stack.request(&message(OP_CLOSE, 5, &[])), Ok(false));
        window.deliver(stack.device(), 0, &frame(89, b"for the other"));
        stack.poll(Instant::from_millis(400));
        assert_eq!(stack.arrived(&mut m), Arrived::All);
        assert_eq!(records6(&m).len(), 1);
        window.deliver(stack.device(), 0, &frame(89, b"late"));
        stack.poll(Instant::from_millis(500));
        assert_eq!(stack.request(&message(OP_CLOSE, 6, &[])), Ok(false));
        assert_eq!(stack.request(&message(OP_CLOSE, 7, &[])), Ok(false));
        assert_eq!(stack.raws6(), 0);
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);
        window.deliver(stack.device(), 0, &frame(89, b"later"));
        stack.poll(Instant::from_millis(600));
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);
        assert_eq!(stack.request(&raw6_open(5, 89)), Ok(false));
    }
}
