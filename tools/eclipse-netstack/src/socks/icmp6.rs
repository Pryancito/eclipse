//! ICMPv6 for the kernel's sockets: what `ping -6` opens.
//!
//! The same bargain as for IPv4 ([`super`]), said with the longer
//! address:
//!
//! ```text
//! ICMP6   (kernel to stack)  16  [u8; 16] to  32  u8 hop limit  33  three zero  36  the ICMPv6 message
//! HEAR6   (kernel to stack)
//! ```
//!
//! A socket that hears ICMPv6 says so with `HEAR6`, and is gone with its
//! `CLOSE`. While any does, every IPv6 packet of ICMPv6 the card brings
//! with its sum right goes to the kernel whole, as a record of a `DATA6`
//! for socket 0 and port 0: which socket it is for is the kernel's to
//! say, and the header tells it how many hops the packet had left.
//!
//! An `ICMP6` is a message to send for such a socket, behind an IPv6
//! header written here. Which of the card's addresses it is from is the
//! stack's to choose, as for a datagram, and ICMPv6's sum covers that
//! address: so the stack writes the sum too, and what the message has in
//! its place is not looked at.
//!
//! The stack answers the echoes that arrive, heard or not, and goes on
//! reading what the routers say ([`super::six`]).

use std::collections::{BTreeMap, VecDeque};

use smoltcp::phy::Device;
use smoltcp::socket::{RawPacketMetadata, RawSocket, RawSocketBuffer, SocketHandle};
use smoltcp::wire::{
    Icmpv6Packet, IpAddress, IpProtocol, IpRepr, IpVersion, Ipv6Address, Ipv6Packet,
};

use super::{push_record6, start_data6, Arrived, Refused, Stack};
use super::{DATAGRAM6_AT, HEADER_LEN, HEARD, MESSAGE_MAX, RECORD6_LEN};
use super::{RX_DATAGRAMS, TX_DATAGRAMS};

/// The operations of a message.
pub const OP_ICMP6: u32 = 21;
pub const OP_HEAR6: u32 = 22;

/// The IPv6 header a message travels behind.
const IP_HEADER: usize = 40;
/// What every ICMPv6 message starts with: type, code and sum.
const ICMP_HEADER: usize = 4;

/// Whether `packet`, a whole IPv6 packet of ICMPv6, has its sum right.
fn sum_right(packet: &[u8]) -> bool {
    let Ok(ip) = Ipv6Packet::new_checked(packet) else {
        return false;
    };
    let message = ip.payload();
    let (from, to) = (
        IpAddress::Ipv6(ip.src_addr()),
        IpAddress::Ipv6(ip.dst_addr()),
    );
    message.len() >= ICMP_HEADER && Icmpv6Packet::new_unchecked(message).verify_checksum(&from, &to)
}

/// The sockets that hear ICMPv6, and what was heard for them.
#[derive(Default)]
pub(super) struct Hearing6 {
    /// What each sends by: a socket of whole IPv6 packets of ICMPv6 that
    /// none reaches, its own, as for IPv4.
    socks: BTreeMap<u64, SocketHandle>,
    /// What the card brought that the kernel has not been told.
    heard: VecDeque<Vec<u8>>,
}

impl Hearing6 {
    /// Whether `sock` is a socket that hears ICMPv6.
    pub(super) fn knows(&self, sock: u64) -> bool {
        self.socks.contains_key(&sock)
    }

    /// How many sockets hear ICMPv6.
    pub(super) fn len(&self) -> usize {
        self.socks.len()
    }

    /// Whether there is something heard to tell the kernel.
    pub(super) fn waits(&self) -> bool {
        !self.heard.is_empty()
    }

    /// `packet`, a whole IPv6 packet of ICMPv6, came off the card: it is
    /// kept for the kernel while a socket hears, when its sum is right
    /// and there is room. Past [`RX_DATAGRAMS`] it is lost, as on a
    /// socket's queue.
    pub(super) fn keep(&mut self, packet: &[u8]) {
        if self.socks.is_empty() || self.heard.len() >= RX_DATAGRAMS || !sum_right(packet) {
            return;
        }
        self.heard.push_back(packet.to_vec());
    }
}

impl<D: for<'d> Device<'d>> Stack<D> {
    /// The longest ICMPv6 message this stack carries: what fits one
    /// frame.
    fn max_icmp6(&self) -> usize {
        self.iface.device().capabilities().ip_mtu() - IP_HEADER
    }

    /// How many sockets hear ICMPv6.
    pub fn hearing6(&self) -> usize {
        self.hearing6.len()
    }

    /// `HEAR6`: `sock` hears ICMPv6 from now on.
    pub(super) fn hear6(&mut self, sock: u64) -> Result<bool, Refused> {
        if sock == HEARD || self.knows(sock) {
            return Err(Refused::Bind);
        }
        let room = self.iface.device().capabilities().ip_mtu();
        let buffer = |packets: usize| {
            RawSocketBuffer::new(
                vec![RawPacketMetadata::EMPTY; packets],
                vec![0u8; packets * room],
            )
        };
        let mouth = RawSocket::new(
            IpVersion::Ipv6,
            IpProtocol::Icmpv6,
            buffer(0),
            buffer(TX_DATAGRAMS),
        );
        self.hearing6.socks.insert(sock, self.set.add(mouth));
        Ok(false)
    }

    /// `ICMP6`: a message of `sock`'s to send.
    pub(super) fn icmp6(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        if m.len() < DATAGRAM6_AT + ICMP_HEADER {
            return Err(Refused::Malformed);
        }
        let handle = *self.hearing6.socks.get(&sock).ok_or(Refused::NoSocket)?;
        let to = Ipv6Address::from_bytes(&m[16..32]);
        let hop_limit = m[32];
        let message = &m[DATAGRAM6_AT..];
        if hop_limit == 0 {
            return Err(Refused::Malformed);
        }
        if message.len() > self.max_icmp6() {
            return Err(Refused::TooLong);
        }
        if !self.reaches6(to) {
            return Err(Refused::Unreachable);
        }
        // From the address smoltcp would send a datagram there from.
        let asked = IpRepr::Unspecified {
            src_addr: IpAddress::Unspecified,
            dst_addr: IpAddress::Ipv6(to),
            protocol: IpProtocol::Icmpv6,
            payload_len: message.len(),
            hop_limit,
        };
        let Ok(IpRepr::Ipv6(header)) = asked.lower(self.iface.ip_addrs()) else {
            return Err(Refused::Unreachable);
        };
        let mut socket = self.set.get::<RawSocket>(handle);
        let packet = socket
            .send(IP_HEADER + message.len())
            .map_err(|_| Refused::Full)?;
        let (head, body) = packet.split_at_mut(IP_HEADER);
        header.emit(&mut Ipv6Packet::new_unchecked(head));
        body.copy_from_slice(message);
        let (from, to) = (IpAddress::Ipv6(header.src_addr), IpAddress::Ipv6(to));
        Icmpv6Packet::new_unchecked(body).fill_checksum(&from, &to);
        Ok(true)
    }

    /// `CLOSE` of a socket that hears ICMPv6, if `sock` is one. What it
    /// sent and can leave now does; with the last of them, what was
    /// heard and not told is nobody's.
    pub(super) fn deaf6(&mut self, sock: u64) -> bool {
        let Some(handle) = self.hearing6.socks.remove(&sock) else {
            return false;
        };
        let _ = self.iface.poll(&mut self.set, self.now);
        self.set.remove(handle);
        if self.hearing6.socks.is_empty() {
            self.hearing6.heard.clear();
        }
        true
    }

    /// Make `m` the `DATA6` of what was heard of ICMPv6: all of it, or
    /// as much as fits one message. `m` is left as it was when there is
    /// nothing.
    pub(super) fn heard6(&mut self, m: &mut Vec<u8>) -> Arrived {
        if self.hearing6.heard.is_empty() {
            return Arrived::Nothing;
        }
        start_data6(m);
        while let Some(packet) = self.hearing6.heard.front() {
            if m.len() + RECORD6_LEN + packet.len() > MESSAGE_MAX {
                return Arrived::More;
            }
            // The header's source, which `keep` saw is whole.
            let mut from = [0u8; 16];
            from.copy_from_slice(&packet[8..24]);
            push_record6(m, HEARD, from, 0, packet);
            self.hearing6.heard.pop_front();
            self.brought = true;
        }
        debug_assert!(m.len() > HEADER_LEN);
        Arrived::All
    }
}
