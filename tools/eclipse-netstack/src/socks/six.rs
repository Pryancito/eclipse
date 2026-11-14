//! The card's IPv6 addresses, which nobody writes down and no server
//! leases: the stack makes them.
//!
//! One is the card's own on its link, `fe80::` and the MAC
//! ([`link_local`]), which it has from [`Stack::speak_six`] on. The
//! others come from the link's router: the stack asks whether there is
//! one (a router solicitation, three times at most, four seconds apart,
//! while none has answered), and a router says so by itself every few
//! minutes (a router advertisement). What an advertisement says is read here
//! ([`advert`]), by hand: smoltcp's reading refuses the whole message for
//! one option it does not know, and routers send plenty of those. Of what
//! it says the stack takes two things:
//!
//! - the router itself, where what is for no network of the card's goes,
//!   for as long as it says it is one. The first one heard is kept while
//!   it lasts, and when it is one no more another is asked for.
//! - each prefix of 64 bits that the router says an address may be made
//!   from: the address is the prefix and the MAC, for as long as the
//!   router says the prefix is good, and its network is the card's.
//!   [`PREFIXES`] of them at most.
//!
//! smoltcp does the rest once the interface has the addresses: it answers
//! the neighbours that ask where one is, and the echo requests.
//!
//! The kernel shows the card, so it is told what the card has, all of it
//! each time it changes, in a message about no socket:
//!
//! ```text
//! SIX     (stack to kernel)  16  [u8; 16] the router, or zeroes  32  one address after another, to the end:
//!             0  [u8; 16] address  16  u8 the bits of its network's prefix  17  seven zero
//! ```
//!
//! A datagram socket's datagrams travel as IPv6 when the kernel says so
//! (`SEND6`, `DATA6`, in [`super`]), and so does a connection
//! (`CONNECT6`, `LISTEN6`, `ACCEPTED6`, in [`super::tcp`]). What a
//! socket of ICMPv6 hears is read off the same socket as the
//! advertisements ([`super::icmp6`]).

use managed::ManagedSlice;
use smoltcp::phy::{ChecksumCapabilities, Device};
use smoltcp::socket::{RawPacketMetadata, RawSocket, RawSocketBuffer, SocketHandle};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    EthernetAddress, Icmpv6Packet, Icmpv6Repr, IpAddress, IpCidr, IpProtocol, IpVersion,
    Ipv6Address, Ipv6Cidr, Ipv6Packet, Ipv6Repr, NdiscRepr,
};

use super::{Stack, HEADER_LEN};

/// The message that says the card's IPv6 addresses.
pub const OP_SIX: u32 = 16;
/// Where the addresses start in a `SIX`.
pub const SIX_ADDRESSES_AT: usize = HEADER_LEN + 16;
/// What each address of a `SIX` takes.
pub const SIX_ADDRESS_LEN: usize = 24;

/// The addresses made from a router's prefixes, at most.
pub const PREFIXES: usize = 4;
/// How many times a router is asked for, while none has answered.
const ASKS: u8 = 3;
/// How long between two of those.
const ASK_EVERY: Duration = Duration::from_secs(4);
/// What a router's word cannot shorten an address's time below, unless
/// it is shorter already (RFC 4862, 5.5.3): whoever can send a frame on
/// the link could otherwise take every address away with one.
const AT_LEAST: Duration = Duration::from_secs(2 * 60 * 60);
/// A lifetime that does not end.
const FOREVER: u32 = u32::MAX;
/// The bits of a prefix an address is made from, and of its network.
const PREFIX_BITS: u8 = 64;

/// The messages that arrive in one turn, for the stack to read and for
/// the sockets that hear ICMPv6, and the ones the stack has to send:
/// solicitations are rare.
const HEARD: usize = 64;
const TO_SAY: usize = 2;

/// ICMPv6: a router advertisement, and where its options start.
const ROUTER_ADVERT: u8 = 134;
const ADVERT_OPTIONS_AT: usize = 16;
/// The option that names a prefix, its length, and the flag that says an
/// address may be made from it.
const PREFIX_INFORMATION: u8 = 3;
const PREFIX_INFORMATION_LEN: usize = 32;
const AUTONOMOUS: u8 = 0x40;
/// What a message of the neighbours' protocol travels with: nothing that
/// crossed a router has it.
const ON_LINK: u8 = 255;
const IP_HEADER: usize = 40;

/// The address a card with this MAC has on its link: `fe80::` and the MAC
/// with `ff:fe` in its middle and its second bit turned.
pub fn link_local(mac: EthernetAddress) -> Ipv6Address {
    with_card(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), mac)
}

/// The address of `prefix`'s first 64 bits and the card `mac`.
fn with_card(prefix: Ipv6Address, mac: EthernetAddress) -> Ipv6Address {
    let m = mac.0;
    let mut bytes = prefix.0;
    bytes[8..].copy_from_slice(&[m[0] ^ 2, m[1], m[2], 0xff, 0xfe, m[3], m[4], m[5]]);
    Ipv6Address(bytes)
}

/// What a router advertisement says, of what the stack takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advert {
    /// Who sent it.
    pub router: Ipv6Address,
    /// For how many seconds it is a router to send through: 0 when it is
    /// none.
    pub lifetime: u16,
    /// The prefixes an address may be made from, and for how many seconds
    /// each is good.
    pub prefixes: Vec<(Ipv6Address, u32)>,
}

/// What `packet`, a whole IPv6 packet of ICMP, says when it is a router
/// advertisement to believe: from an address of the link, never through
/// a router, and with its sum right.
pub fn advert(packet: &[u8]) -> Option<Advert> {
    let ip = Ipv6Packet::new_checked(packet).ok()?;
    let (router, to) = (ip.src_addr(), ip.dst_addr());
    if ip.next_header() != IpProtocol::Icmpv6 || ip.hop_limit() != ON_LINK {
        return None;
    }
    if !router.is_link_local() {
        return None;
    }
    let message = ip.payload();
    if message.len() < ADVERT_OPTIONS_AT || message[0] != ROUTER_ADVERT || message[1] != 0 {
        return None;
    }
    let icmp = Icmpv6Packet::new_checked(message).ok()?;
    if !icmp.verify_checksum(&IpAddress::Ipv6(router), &IpAddress::Ipv6(to)) {
        return None;
    }
    let lifetime = u16::from_be_bytes([message[6], message[7]]);
    let mut prefixes = Vec::new();
    let mut options = &message[ADVERT_OPTIONS_AT..];
    // Each option says its kind and its length in eights of bytes; one
    // that says none, or more than there is, ends the reading.
    while options.len() >= 2 {
        let len = usize::from(options[1]) * 8;
        if len == 0 || len > options.len() {
            break;
        }
        let (option, rest) = options.split_at(len);
        options = rest;
        if option[0] != PREFIX_INFORMATION || len < PREFIX_INFORMATION_LEN {
            continue;
        }
        let prefix = Ipv6Address::from_bytes(&option[16..32]);
        let valid = u32::from_be_bytes([option[4], option[5], option[6], option[7]]);
        // An address is made from 64 bits of prefix, and not from the
        // link's own or a group's.
        let usable = option[2] == PREFIX_BITS
            && option[3] & AUTONOMOUS != 0
            && prefix.is_unicast()
            && !prefix.is_link_local();
        if usable && prefixes.len() < PREFIXES {
            prefixes.push((prefix, valid));
        }
    }
    Some(Advert {
        router,
        lifetime,
        prefixes,
    })
}

/// What a card has of IPv6: its addresses, the link's first, and the
/// router when there is one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sixes {
    pub addresses: Vec<Ipv6Cidr>,
    pub router: Option<Ipv6Address>,
}

impl Sixes {
    /// The `SIX` that says them.
    pub fn message(&self) -> Vec<u8> {
        let mut m = vec![0u8; SIX_ADDRESSES_AT];
        m[4..8].copy_from_slice(&OP_SIX.to_le_bytes());
        if let Some(router) = self.router {
            m[HEADER_LEN..].copy_from_slice(router.as_bytes());
        }
        for address in &self.addresses {
            m.extend_from_slice(address.address().as_bytes());
            m.extend_from_slice(&[address.prefix_len(), 0, 0, 0, 0, 0, 0, 0]);
        }
        m
    }
}

/// An address made from a router's prefix.
struct Given {
    address: Ipv6Address,
    /// When it stops being good, if ever.
    until: Option<Instant>,
}

/// The stack's side of having IPv6 addresses.
#[derive(Default)]
pub(super) struct Six {
    /// The socket every IPv6 packet of ICMP reaches, where the
    /// advertisements are read and the solicitations sent: on a stack
    /// that speaks IPv6.
    ear: Option<SocketHandle>,
    /// The router, and until when it is one.
    router: Option<(Ipv6Address, Instant)>,
    given: Vec<Given>,
    /// How many times a router has been asked for, and when the next is.
    asked: u8,
    ask_at: Option<Instant>,
    /// Whether the addresses or the router changed since
    /// [`Stack::six_news`] last asked.
    news: bool,
}

impl Six {
    /// When there is a router to ask for, if there is.
    fn asks_at(&self) -> Option<Instant> {
        if self.ear.is_some() && self.router.is_none() && self.asked < ASKS {
            self.ask_at
        } else {
            None
        }
    }

    /// The first time something of this ends or is due, when anything
    /// is.
    fn next(&self) -> Option<Instant> {
        let router = self.router.map(|(_, until)| until);
        let given = self.given.iter().filter_map(|given| given.until);
        self.asks_at().into_iter().chain(router).chain(given).min()
    }
}

impl<D: for<'d> Device<'d>> Stack<D> {
    /// Have IPv6 addresses on the card: its own on the link from now on,
    /// and what the link's router gives. Once.
    pub fn speak_six(&mut self) {
        if self.six.ear.is_some() {
            return;
        }
        let ours = link_local(self.iface.ethernet_addr());
        self.put_six(ours);
        let room = self.iface.device().capabilities().ip_mtu();
        let buffer = |packets: usize| {
            RawSocketBuffer::new(
                vec![RawPacketMetadata::EMPTY; packets],
                vec![0u8; packets * room],
            )
        };
        let ear = RawSocket::new(
            IpVersion::Ipv6,
            IpProtocol::Icmpv6,
            buffer(HEARD),
            buffer(TO_SAY),
        );
        self.six.ear = Some(self.set.add(ear));
        self.six.ask_at = Some(self.now);
        self.six.news = true;
    }

    /// Whether a datagram for `addr` can be sent as IPv6: to a group, to
    /// a network of the card's, or to anywhere else while there is a
    /// router. As for IPv4, one that cannot go is not queued.
    pub(super) fn reaches6(&self, addr: Ipv6Address) -> bool {
        if self.six.ear.is_none() || addr.is_unspecified() || addr.is_loopback() {
            return false;
        }
        addr.is_multicast()
            || self.six.router.is_some()
            || self.iface.ip_addrs().iter().any(|cidr| match cidr {
                IpCidr::Ipv6(cidr) => cidr.contains_addr(&addr),
                _ => false,
            })
    }

    /// What the card has of IPv6.
    pub fn sixes(&self) -> Sixes {
        let addresses = self.iface.ip_addrs().iter().filter_map(|cidr| match cidr {
            IpCidr::Ipv6(cidr) => Some(*cidr),
            _ => None,
        });
        Sixes {
            addresses: addresses.collect(),
            router: self.six.router.map(|(router, _)| router),
        }
    }

    /// Whether that changed since this was last asked.
    pub fn six_news(&mut self) -> bool {
        core::mem::take(&mut self.six.news)
    }

    fn put_six(&mut self, address: Ipv6Address) {
        let cidr = IpCidr::Ipv6(Ipv6Cidr::new(address, PREFIX_BITS));
        self.iface.update_ip_addrs(|addrs| {
            // The list is the `Vec` whoever built the interface gave it.
            if let ManagedSlice::Owned(addrs) = addrs {
                if !addrs.contains(&cidr) {
                    addrs.push(cidr);
                }
            }
        });
    }

    fn take_six(&mut self, address: Ipv6Address) {
        let cidr = IpCidr::Ipv6(Ipv6Cidr::new(address, PREFIX_BITS));
        self.iface.update_ip_addrs(|addrs| {
            if let ManagedSlice::Owned(addrs) = addrs {
                addrs.retain(|have| *have != cidr);
            }
        });
    }

    /// Before a poll: ask for a router when it is time to.
    pub(super) fn six_ask(&mut self) {
        let (Some(ear), Some(at)) = (self.six.ear, self.six.asks_at()) else {
            return;
        };
        if self.now < at {
            return;
        }
        // Sent or not (the card may have no room): the next is later.
        self.six.asked += 1;
        self.six.ask_at = Some(self.now + ASK_EVERY);
        let mac = self.iface.ethernet_addr();
        let (from, to) = (link_local(mac), Ipv6Address::LINK_LOCAL_ALL_ROUTERS);
        let ask = Icmpv6Repr::Ndisc(NdiscRepr::RouterSolicit { lladdr: Some(mac) });
        let ip = Ipv6Repr {
            src_addr: from,
            dst_addr: to,
            next_header: IpProtocol::Icmpv6,
            payload_len: ask.buffer_len(),
            hop_limit: ON_LINK,
        };
        let mut socket = self.set.get::<RawSocket>(ear);
        if let Ok(packet) = socket.send(IP_HEADER + ask.buffer_len()) {
            let (header, message) = packet.split_at_mut(IP_HEADER);
            ip.emit(&mut Ipv6Packet::new_unchecked(header));
            ask.emit(
                &IpAddress::Ipv6(from),
                &IpAddress::Ipv6(to),
                &mut Icmpv6Packet::new_unchecked(message),
                &ChecksumCapabilities::default(),
            );
        }
    }

    /// After a poll: read what the routers said, and let go of what has
    /// lasted as long as it was said to.
    pub(super) fn six_turn(&mut self) {
        let Some(ear) = self.six.ear else {
            return;
        };
        loop {
            let said = match self.set.get::<RawSocket>(ear).recv() {
                Ok(packet) => {
                    // All of it is the kernel's to hear, while a socket
                    // of ICMPv6 does.
                    self.hearing6.keep(packet);
                    advert(packet)
                }
                Err(_) => break,
            };
            if let Some(said) = said {
                self.six_heard(&said);
            }
        }
        let now = self.now;
        if self.six.router.is_some_and(|(_, until)| until <= now) {
            self.set_router(None);
        }
        let over = |given: &Given| given.until.is_some_and(|until| until <= now);
        while let Some(at) = self.six.given.iter().position(over) {
            let gone = self.six.given.remove(at);
            self.take_six(gone.address);
            self.six.news = true;
        }
    }

    /// Send what is for no network of the card's to `router` until
    /// `until`, or nowhere.
    fn set_router(&mut self, router: Option<(Ipv6Address, Instant)>) {
        let was = self.six.router.map(|(router, _)| router);
        let now = router.map(|(router, _)| router);
        if was != now {
            let routes = self.iface.routes_mut();
            routes.remove_default_ipv6_route();
            if let Some(router) = now {
                // An interface built with no room for a route has none.
                if routes.add_default_ipv6_route(router).is_err() {
                    self.six.router = None;
                    self.six.news |= was.is_some();
                    return;
                }
            }
            self.six.news = true;
        }
        self.six.router = router;
        if was.is_some() && router.is_none() {
            // There may be another: it is asked for as at first.
            self.six.asked = 0;
            self.six.ask_at = Some(self.now);
        }
    }

    /// Take what a router's advertisement says.
    fn six_heard(&mut self, said: &Advert) {
        let now = self.now;
        let ours = self.six.router.map(|(router, _)| router);
        let lasts = now + Duration::from_secs(u64::from(said.lifetime));
        match (ours, said.lifetime) {
            // It is no router any more.
            (Some(ours), 0) if ours == said.router => self.set_router(None),
            (_, 0) => {}
            // The one there is stays while it lasts.
            (Some(ours), _) if ours != said.router => {}
            _ => self.set_router(Some((said.router, lasts))),
        }
        let mac = self.iface.ethernet_addr();
        for &(prefix, valid) in &said.prefixes {
            let address = with_card(prefix, mac);
            let until = (valid != FOREVER).then(|| now + Duration::from_secs(u64::from(valid)));
            let room = self.six.given.len() < PREFIXES;
            match self.six.given.iter_mut().find(|g| g.address == address) {
                Some(given) => {
                    // Longer is taken as said; shorter, only down to
                    // AT_LEAST from now.
                    let floor = now + AT_LEAST;
                    let taken = match (until, given.until) {
                        (None, _) => true,
                        (Some(said), have) => said > floor || have.is_some_and(|have| said > have),
                    };
                    if taken {
                        given.until = until;
                    } else if given.until.is_none_or(|have| have > floor) {
                        given.until = Some(floor);
                    }
                }
                None if valid != 0 && room => {
                    self.six.given.push(Given { address, until });
                    self.put_six(address);
                    self.six.news = true;
                }
                None => {}
            }
        }
    }

    /// How long until an address or the router ends, or a router is to
    /// be asked for again.
    pub(super) fn six_idle(&self, now: Instant) -> Option<Duration> {
        let next = self.six.next()?;
        Some(if next > now {
            next - now
        } else {
            Duration::ZERO
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::*;
    use crate::socks::icmp6::{OP_HEAR6, OP_ICMP6};
    use crate::socks::tcp::WHY_UNREACHABLE;
    use crate::socks::tcp::{ACCEPTED_BIT, EV_ACCEPTED6, EV_BYTES, EV_CONNECTED, EV_FAILED};
    use crate::socks::tcp::{OP_CONNECT6, OP_EVENTS, OP_LISTEN, OP_LISTEN6, OP_WRITE};
    use crate::socks::tests::{bind, message, records};
    use crate::socks::{parse_header, Arrived, Refused};
    use crate::socks::{HEARD as HEARD_SOCK, OP_CLOSE, OP_HEAR};
    use crate::socks::{OP_DATA6, OP_SEND6, RECORD6_LEN, RECORD_LEN};
    use smoltcp::wire::{EthernetFrame, EthernetProtocol, EthernetRepr, NdiscNeighborFlags};
    use smoltcp::wire::{TcpControl, TcpPacket, TcpRepr, TcpSeqNumber};
    use smoltcp::wire::{UdpPacket, UdpRepr};

    /// Our address on the link, and the one made from the router's prefix.
    const OURS_LINK: Ipv6Address = Ipv6Address([
        0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0x50, 0x54, 0x00, 0xff, 0xfe, 0x12, 0x34, 0x57,
    ]);
    const OURS_GIVEN: Ipv6Address = Ipv6Address([
        0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0x50, 0x54, 0x00, 0xff, 0xfe, 0x12, 0x34, 0x57,
    ]);
    const PREFIX: Ipv6Address = Ipv6Address([0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    /// THEIRS on the link: the router.
    const ROUTER: Ipv6Address = Ipv6Address([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    const ALL_NODES: Ipv6Address = Ipv6Address::LINK_LOCAL_ALL_NODES;
    const ON_LINK_FLAG: u8 = 0x80;

    /// A stack over a bench's card that speaks IPv6, with room for a
    /// route.
    fn stack() -> (Stack<crate::port::Port>, Window) {
        let Bench { window, iface, .. } = Bench::new();
        let mut stack = Stack::new(iface);
        stack.speak_six();
        (stack, window)
    }

    /// Poll at `ms`, and the frames that put on the wire, the card done
    /// with each.
    fn frames(stack: &mut Stack<crate::port::Port>, window: &Window, ms: i64) -> Vec<Vec<u8>> {
        stack.poll(Instant::from_millis(ms));
        let turn = turn(stack.device());
        let mut out = Vec::new();
        for m in &turn {
            if m.op == udrivers::net::OP_TX {
                out.push(window.sent(m));
                window.done(stack.device(), m);
            }
        }
        out
    }

    /// The card of the group `group`: `33:33` and its last four bytes.
    fn group_card(group: Ipv6Address) -> EthernetAddress {
        let g = group.0;
        EthernetAddress([0x33, 0x33, g[12], g[13], g[14], g[15]])
    }

    /// THEIRS sends `message`, ICMPv6 with its sum to be written, from
    /// `from` to `to`, to travel `hops`.
    fn six_frame(from: Ipv6Address, to: Ipv6Address, hops: u8, message: &[u8]) -> Vec<u8> {
        let mut bytes = ip6_frame(from, to, hops, IpProtocol::Icmpv6, message);
        let at = bytes.len() - message.len();
        let mut icmp = Icmpv6Packet::new_unchecked(&mut bytes[at..]);
        icmp.fill_checksum(&IpAddress::Ipv6(from), &IpAddress::Ipv6(to));
        bytes
    }

    /// THEIRS sends `payload`, of `protocol`, from `from` to `to`, to
    /// travel `hops`.
    fn ip6_frame(
        from: Ipv6Address,
        to: Ipv6Address,
        hops: u8,
        protocol: IpProtocol,
        message: &[u8],
    ) -> Vec<u8> {
        let eth = EthernetRepr {
            src_addr: THEIRS,
            dst_addr: if to.is_unicast() {
                OURS
            } else {
                group_card(to)
            },
            ethertype: EthernetProtocol::Ipv6,
        };
        let ip = Ipv6Repr {
            src_addr: from,
            dst_addr: to,
            next_header: protocol,
            payload_len: message.len(),
            hop_limit: hops,
        };
        let mut bytes = vec![0u8; eth.buffer_len() + IP_HEADER + message.len()];
        let mut frame = EthernetFrame::new_unchecked(&mut bytes[..]);
        eth.emit(&mut frame);
        let (header, payload) = frame.payload_mut().split_at_mut(IP_HEADER);
        ip.emit(&mut Ipv6Packet::new_unchecked(header));
        payload.copy_from_slice(message);
        bytes
    }

    /// THEIRS sends the datagram `data` from `from` to `to`, as IPv6.
    fn udp6_frame(from: (Ipv6Address, u16), to: (Ipv6Address, u16), data: &[u8]) -> Vec<u8> {
        let udp = UdpRepr {
            src_port: from.1,
            dst_port: to.1,
        };
        let mut datagram = vec![0u8; 8 + data.len()];
        udp.emit(
            &mut UdpPacket::new_unchecked(&mut datagram[..]),
            &IpAddress::Ipv6(from.0),
            &IpAddress::Ipv6(to.0),
            data.len(),
            |room| room.copy_from_slice(data),
            &ChecksumCapabilities::default(),
        );
        ip6_frame(from.0, to.0, 64, IpProtocol::Udp, &datagram)
    }

    /// A datagram of ours on the wire as IPv6: the card it is for, who
    /// it is from and for, the hops, and what it carries, its sum
    /// checked.
    type Datagram6 = (
        EthernetAddress,
        (Ipv6Address, u16),
        (Ipv6Address, u16),
        u8,
        Vec<u8>,
    );

    fn udp6_of(frame: &[u8]) -> Datagram6 {
        let eth = EthernetFrame::new_checked(frame).unwrap();
        assert_eq!(eth.src_addr(), OURS);
        assert_eq!(eth.ethertype(), EthernetProtocol::Ipv6);
        let ip = Ipv6Packet::new_checked(eth.payload()).unwrap();
        assert_eq!(ip.next_header(), IpProtocol::Udp);
        let (from, to) = (ip.src_addr(), ip.dst_addr());
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert!(udp.verify_checksum(&IpAddress::Ipv6(from), &IpAddress::Ipv6(to)));
        (
            eth.dst_addr(),
            (from, udp.src_port()),
            (to, udp.dst_port()),
            ip.hop_limit(),
            udp.payload().to_vec(),
        )
    }

    /// THEIRS says that `address` is at its card, to `to`.
    fn here_is(address: Ipv6Address, to: Ipv6Address) -> Vec<u8> {
        let said = Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
            flags: NdiscNeighborFlags::SOLICITED | NdiscNeighborFlags::OVERRIDE,
            target_addr: address,
            lladdr: Some(THEIRS),
        });
        six_frame(address, to, ON_LINK, &six_message(address, to, &said))
    }

    /// A `SEND6` as the kernel writes it.
    fn send6(sock: u64, to: Ipv6Address, port: u16, hops: u8, data: &[u8]) -> Vec<u8> {
        let mut rest = to.0.to_vec();
        rest.extend_from_slice(&port.to_le_bytes());
        rest.extend_from_slice(&[hops, 0]);
        rest.extend_from_slice(data);
        message(OP_SEND6, sock, &rest)
    }

    /// The records of a `DATA6`.
    fn records6(m: &[u8]) -> Vec<(u64, Ipv6Address, u16, Vec<u8>)> {
        assert_eq!(parse_header(m), Some((OP_DATA6, 0)));
        let (mut rest, mut out) = (&m[HEADER_LEN..], Vec::new());
        while !rest.is_empty() {
            let sock = u64::from_le_bytes(rest[..8].try_into().unwrap());
            let addr = Ipv6Address::from_bytes(&rest[8..24]);
            let port = u16::from_le_bytes([rest[24], rest[25]]);
            let len = u16::from_le_bytes([rest[26], rest[27]]) as usize;
            out.push((
                sock,
                addr,
                port,
                rest[RECORD6_LEN..RECORD6_LEN + len].to_vec(),
            ));
            rest = &rest[RECORD6_LEN + len..];
        }
        out
    }

    /// A segment as IPv6: who it is from and for, what it says, where its
    /// numbers are and what it carries.
    #[derive(Debug, PartialEq)]
    struct Segment6 {
        from: (Ipv6Address, u16),
        to: (Ipv6Address, u16),
        control: TcpControl,
        seq: u32,
        ack: Option<u32>,
        data: Vec<u8>,
    }

    /// THEIRS sends `segment`.
    fn tcp6_frame(segment: &Segment6) -> Vec<u8> {
        let tcp = TcpRepr {
            src_port: segment.from.1,
            dst_port: segment.to.1,
            control: segment.control,
            seq_number: TcpSeqNumber(segment.seq as i32),
            ack_number: segment.ack.map(|ack| TcpSeqNumber(ack as i32)),
            window_len: u16::MAX,
            window_scale: None,
            max_seg_size: (segment.control == TcpControl::Syn).then_some(1440),
            sack_permitted: false,
            sack_ranges: [None; 3],
            payload: &segment.data,
        };
        let mut bytes = vec![0u8; tcp.buffer_len()];
        tcp.emit(
            &mut TcpPacket::new_unchecked(&mut bytes[..]),
            &IpAddress::Ipv6(segment.from.0),
            &IpAddress::Ipv6(segment.to.0),
            &ChecksumCapabilities::default(),
        );
        ip6_frame(segment.from.0, segment.to.0, 64, IpProtocol::Tcp, &bytes)
    }

    /// A segment of ours on the wire as IPv6, for THEIRS's card, its sum
    /// checked.
    fn tcp6_of(frame: &[u8]) -> Segment6 {
        let eth = EthernetFrame::new_checked(frame).unwrap();
        assert_eq!((eth.src_addr(), eth.dst_addr()), (OURS, THEIRS));
        assert_eq!(eth.ethertype(), EthernetProtocol::Ipv6);
        let ip = Ipv6Packet::new_checked(eth.payload()).unwrap();
        assert_eq!(ip.next_header(), IpProtocol::Tcp);
        let (from, to) = (ip.src_addr(), ip.dst_addr());
        let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
        let tcp = TcpRepr::parse(
            &tcp,
            &IpAddress::Ipv6(from),
            &IpAddress::Ipv6(to),
            &ChecksumCapabilities::default(),
        )
        .unwrap();
        Segment6 {
            from: (from, tcp.src_port),
            to: (to, tcp.dst_port),
            control: tcp.control,
            seq: tcp.seq_number.0 as u32,
            ack: tcp.ack_number.map(|ack| ack.0 as u32),
            data: tcp.payload.to_vec(),
        }
    }

    /// A `CONNECT6` as the kernel writes it.
    fn connect6(sock: u64, to: Ipv6Address, port: u16, local: u16, window: u32) -> Vec<u8> {
        let mut rest = to.0.to_vec();
        rest.extend_from_slice(&port.to_le_bytes());
        rest.extend_from_slice(&local.to_le_bytes());
        rest.extend_from_slice(&window.to_le_bytes());
        message(OP_CONNECT6, sock, &rest)
    }

    /// A `LISTEN` or a `LISTEN6`, as the kernel writes them.
    fn listen(op: u32, sock: u64, port: u16, backlog: u16, window: u32) -> Vec<u8> {
        let mut rest = port.to_le_bytes().to_vec();
        rest.extend_from_slice(&backlog.to_le_bytes());
        rest.extend_from_slice(&window.to_le_bytes());
        message(op, sock, &rest)
    }

    /// What the stack tells the kernel of its stream sockets now: the
    /// records of its `EVENTS`.
    fn events(stack: &mut Stack<crate::port::Port>) -> Vec<(u64, u16, u32, Vec<u8>)> {
        let (mut m, mut out) = (Vec::new(), Vec::new());
        while stack.arrived(&mut m) != Arrived::Nothing {
            assert_eq!(parse_header(&m), Some((OP_EVENTS, 0)));
            let mut rest = &m[HEADER_LEN..];
            while !rest.is_empty() {
                let sock = u64::from_le_bytes(rest[..8].try_into().unwrap());
                let what = u16::from_le_bytes([rest[8], rest[9]]);
                let len = u16::from_le_bytes([rest[10], rest[11]]) as usize;
                let number = u32::from_le_bytes(rest[12..16].try_into().unwrap());
                out.push((
                    sock,
                    what,
                    number,
                    rest[RECORD_LEN..RECORD_LEN + len].to_vec(),
                ));
                rest = &rest[RECORD_LEN + len..];
            }
        }
        out
    }

    /// A message of ICMPv6 as smoltcp writes it, from `from` to `to`.
    fn six_message(from: Ipv6Address, to: Ipv6Address, repr: &Icmpv6Repr) -> Vec<u8> {
        let mut message = vec![0u8; repr.buffer_len()];
        repr.emit(
            &IpAddress::Ipv6(from),
            &IpAddress::Ipv6(to),
            &mut Icmpv6Packet::new_unchecked(&mut message[..]),
            &ChecksumCapabilities::default(),
        );
        message
    }

    /// What a frame of ours carries: the cards, the addresses, the hops,
    /// and the ICMPv6 message, its sum checked.
    struct Sent {
        to_card: EthernetAddress,
        from: Ipv6Address,
        to: Ipv6Address,
        hops: u8,
        message: Vec<u8>,
    }

    fn six_of(frame: &[u8]) -> Sent {
        let eth = EthernetFrame::new_checked(frame).unwrap();
        assert_eq!(eth.src_addr(), OURS);
        assert_eq!(eth.ethertype(), EthernetProtocol::Ipv6);
        let ip = Ipv6Packet::new_checked(eth.payload()).unwrap();
        assert_eq!(ip.next_header(), IpProtocol::Icmpv6);
        let (from, to) = (ip.src_addr(), ip.dst_addr());
        let icmp = Icmpv6Packet::new_checked(ip.payload()).unwrap();
        assert!(icmp.verify_checksum(&IpAddress::Ipv6(from), &IpAddress::Ipv6(to)));
        Sent {
            to_card: eth.dst_addr(),
            from,
            to,
            hops: ip.hop_limit(),
            message: ip.payload().to_vec(),
        }
    }

    /// One prefix of a router advertisement: its bits, its flags, and for
    /// how many seconds it is good.
    type Prefix = (Ipv6Address, u8, u8, u32);

    /// A router advertisement's message: a router for `lifetime` seconds,
    /// with its card, `prefixes`, and a name server, which is an option
    /// smoltcp does not know.
    fn advert_message(lifetime: u16, prefixes: &[Prefix]) -> Vec<u8> {
        let mut m = vec![ROUTER_ADVERT, 0, 0, 0, 64, 0];
        m.extend_from_slice(&lifetime.to_be_bytes());
        m.extend_from_slice(&[0; 8]);
        // Its card.
        m.extend_from_slice(&[1, 1]);
        m.extend_from_slice(&THEIRS.0);
        for &(prefix, bits, flags, valid) in prefixes {
            m.extend_from_slice(&[PREFIX_INFORMATION, 4, bits, flags]);
            m.extend_from_slice(&valid.to_be_bytes());
            m.extend_from_slice(&valid.to_be_bytes());
            m.extend_from_slice(&[0; 4]);
            m.extend_from_slice(prefix.as_bytes());
        }
        // A name server.
        m.extend_from_slice(&[25, 3, 0, 0]);
        m.extend_from_slice(&3600u32.to_be_bytes());
        m.extend_from_slice(Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 3).as_bytes());
        m
    }

    /// The router says it is one for `lifetime` seconds and that PREFIX
    /// is good for `valid`.
    fn advertise(stack: &mut Stack<crate::port::Port>, window: &Window, lifetime: u16, valid: u32) {
        let message = advert_message(lifetime, &[(PREFIX, 64, AUTONOMOUS | ON_LINK_FLAG, valid)]);
        let frame = six_frame(ROUTER, ALL_NODES, ON_LINK, &message);
        window.deliver(stack.device(), 0, &frame);
    }

    fn cidr(address: Ipv6Address) -> Ipv6Cidr {
        Ipv6Cidr::new(address, 64)
    }

    #[test]
    fn the_links_address_is_the_mac_with_fffe_in_its_middle() {
        assert_eq!(link_local(OURS), OURS_LINK);
        // And the stack has it from the moment it speaks IPv6, which is
        // news.
        let (mut stack, _) = stack();
        assert_eq!(
            stack.sixes(),
            Sixes {
                addresses: vec![cidr(OURS_LINK)],
                router: None
            }
        );
        assert!(stack.six_news());
        assert!(!stack.six_news());
        // One that does not has none, and asks for no router.
        let Bench { window, iface, .. } = Bench::new();
        let mut quiet = Stack::new(iface);
        assert_eq!(quiet.sixes(), Sixes::default());
        assert!(frames(&mut quiet, &window, 0).is_empty());
        assert_eq!(quiet.idle_for(Instant::from_millis(0)), None);
    }

    #[test]
    fn a_router_is_asked_for_three_times_and_no_more() {
        let (mut stack, window) = stack();
        let mut asked = Vec::new();
        for ms in [0, 1000, 3999, 4000, 8000, 12_000, 16_000, 60_000] {
            let sent = frames(&mut stack, &window, ms);
            assert!(sent.len() <= 1);
            for frame in &sent {
                let ask = six_of(frame);
                assert_eq!(ask.to_card, EthernetAddress([0x33, 0x33, 0, 0, 0, 2]));
                assert_eq!(
                    (ask.from, ask.to),
                    (OURS_LINK, Ipv6Address::LINK_LOCAL_ALL_ROUTERS)
                );
                assert_eq!(ask.hops, 255);
                // "Is there a router?", from our card.
                let mut said = vec![133, 0, 0, 0, 0, 0, 0, 0, 1, 1];
                said.extend_from_slice(&OURS.0);
                assert_eq!(ask.message[..2], said[..2]);
                assert_eq!(ask.message[4..], said[4..]);
                asked.push(ms);
            }
            // Nothing else to wake for but the next of them.
            let idle = stack.idle_for(Instant::from_millis(ms));
            let next = [4000, 8000].iter().find(|&&at| at > ms);
            assert_eq!(
                idle,
                next.map(|&at| Duration::from_millis((at - ms) as u64))
            );
        }
        assert_eq!(asked, [0, 4000, 8000]);
    }

    #[test]
    fn what_a_router_advertises_is_the_cards_address_and_its_way_out() {
        let (mut stack, window) = stack();
        assert_eq!(frames(&mut stack, &window, 0).len(), 1);
        assert!(stack.six_news());

        advertise(&mut stack, &window, 1800, 86_400);
        assert!(frames(&mut stack, &window, 100).is_empty());
        let sixes = stack.sixes();
        assert_eq!(
            sixes,
            Sixes {
                addresses: vec![cidr(OURS_LINK), cidr(OURS_GIVEN)],
                router: Some(ROUTER)
            }
        );
        assert!(stack.six_news());
        assert!(!stack.six_news());

        // What the kernel is told.
        let m = sixes.message();
        assert_eq!(m.len(), SIX_ADDRESSES_AT + 2 * SIX_ADDRESS_LEN);
        assert_eq!(m[..4], [0; 4]);
        assert_eq!(m[4..8], OP_SIX.to_le_bytes());
        assert_eq!(m[8..16], [0; 8]);
        assert_eq!(m[16..32], ROUTER.0);
        assert_eq!(m[32..48], OURS_LINK.0);
        assert_eq!(m[48..56], [64, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(m[56..72], OURS_GIVEN.0);
        assert_eq!(m[72..80], [64, 0, 0, 0, 0, 0, 0, 0]);
        // And with no router, zeroes.
        let alone = Sixes {
            router: None,
            ..sixes
        };
        assert_eq!(alone.message()[16..32], [0; 16]);

        // There is a router: it is not asked for again. The same word
        // again is not news.
        advertise(&mut stack, &window, 1800, 86_400);
        for ms in [4000, 8000, 12_000] {
            assert!(frames(&mut stack, &window, ms).is_empty());
        }
        assert!(!stack.six_news());
        assert_eq!(stack.sixes().addresses.len(), 2);
    }

    #[test]
    fn an_advertisement_not_to_believe_changes_nothing() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        assert!(stack.six_news());
        let good = advert_message(1800, &[(PREFIX, 64, AUTONOMOUS, 86_400)]);
        // One that crossed a router; one from no address of the link; one
        // whose sum is wrong; and one that is another message.
        let far = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 2);
        let mut wrong = six_frame(ROUTER, ALL_NODES, ON_LINK, &good);
        *wrong.last_mut().unwrap() ^= 1;
        let mut other = good.clone();
        other[0] = 133;
        for frame in [
            six_frame(ROUTER, ALL_NODES, 64, &good),
            six_frame(far, ALL_NODES, ON_LINK, &good),
            wrong,
            six_frame(ROUTER, ALL_NODES, ON_LINK, &other),
        ] {
            window.deliver(stack.device(), 0, &frame);
            frames(&mut stack, &window, 100);
            assert!(!stack.six_news());
            assert_eq!(stack.sixes().addresses, [cidr(OURS_LINK)]);
            assert_eq!(stack.sixes().router, None);
        }

        // A prefix no address is made from -- of other than 64 bits, one
        // that does not say an address may be, the link's own, a group's
        // -- gives none, and its router is still one.
        let group = Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 0);
        let link = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0);
        let none = advert_message(
            1800,
            &[
                (PREFIX, 48, AUTONOMOUS, 86_400),
                (PREFIX, 64, ON_LINK_FLAG, 86_400),
                (link, 64, AUTONOMOUS, 86_400),
                (group, 64, AUTONOMOUS, 86_400),
                (PREFIX, 64, AUTONOMOUS, 0),
            ],
        );
        window.deliver(
            stack.device(),
            0,
            &six_frame(ROUTER, ALL_NODES, ON_LINK, &none),
        );
        frames(&mut stack, &window, 200);
        assert!(stack.six_news());
        assert_eq!(stack.sixes().addresses, [cidr(OURS_LINK)]);
        assert_eq!(stack.sixes().router, Some(ROUTER));

        // More prefixes than the stack keeps: the first PREFIXES.
        let many: Vec<Prefix> = (1..=6)
            .map(|n| {
                let prefix = Ipv6Address::new(0x2001, 0xdb8, 0, n, 0, 0, 0, 0);
                (prefix, 64, AUTONOMOUS, 86_400)
            })
            .collect();
        let frame = six_frame(ROUTER, ALL_NODES, ON_LINK, &advert_message(1800, &many));
        window.deliver(stack.device(), 0, &frame);
        frames(&mut stack, &window, 300);
        let addresses = stack.sixes().addresses;
        assert_eq!(addresses.len(), 1 + PREFIXES);
        let last = Ipv6Address::new(0x2001, 0xdb8, 0, 4, 0x5054, 0x00ff, 0xfe12, 0x3457);
        assert_eq!(addresses[PREFIXES], cidr(last));
    }

    #[test]
    fn a_router_and_its_addresses_last_as_long_as_it_says() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        advertise(&mut stack, &window, 30, 60);
        frames(&mut stack, &window, 1000);
        assert!(stack.six_news());
        assert_eq!(stack.sixes().router, Some(ROUTER));
        // The router's end is the next thing to wake for.
        let at = |ms: i64| Instant::from_millis(ms);
        assert_eq!(stack.idle_for(at(1000)), Some(Duration::from_secs(30)));

        frames(&mut stack, &window, 30_999);
        assert!(!stack.six_news());
        frames(&mut stack, &window, 31_000);
        assert!(stack.six_news());
        assert_eq!(stack.sixes().router, None);
        assert_eq!(stack.sixes().addresses.len(), 2);
        // There may be another: one is asked for, as at first.
        assert_eq!(stack.idle_for(at(31_000)), Some(Duration::ZERO));
        assert_eq!(frames(&mut stack, &window, 31_000).len(), 1);
        assert_eq!(stack.idle_for(at(31_000)), Some(Duration::from_secs(4)));

        assert_eq!(frames(&mut stack, &window, 61_000).len(), 1);
        assert!(stack.six_news());
        assert_eq!(stack.sixes().addresses, [cidr(OURS_LINK)]);
        assert_eq!(stack.idle_for(at(61_000)), Some(Duration::from_secs(4)));

        // It says again that it is one, and for a prefix good for three
        // hours; then, that the prefix is good for ten seconds, which
        // takes it down to two hours and no further; and then that it is
        // no router, which is so at once.
        advertise(&mut stack, &window, 1800, 3 * 3600);
        frames(&mut stack, &window, 100_000);
        assert!(stack.six_news());
        assert_eq!(stack.sixes().addresses.len(), 2);
        advertise(&mut stack, &window, 1800, 10);
        frames(&mut stack, &window, 101_000);
        frames(&mut stack, &window, 101_000 + 3_600_000);
        assert_eq!(stack.sixes().addresses.len(), 2);
        advertise(&mut stack, &window, 0, 10);
        frames(&mut stack, &window, 102_000 + 3_600_000);
        assert!(stack.six_news());
        assert_eq!(stack.sixes().router, None);
        frames(&mut stack, &window, 101_000 + 7_200_000);
        assert!(stack.six_news());
        assert_eq!(stack.sixes().addresses, [cidr(OURS_LINK)]);
    }

    #[test]
    fn a_neighbour_that_asks_where_an_address_is_is_told() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        advertise(&mut stack, &window, 1800, 86_400);
        frames(&mut stack, &window, 100);
        let theirs = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let mut ms = 200;
        // Asked of whoever may have it, and of the address itself.
        for ours in [OURS_LINK, OURS_GIVEN] {
            for to in [ours.solicited_node(), ours] {
                let ask = Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
                    target_addr: ours,
                    lladdr: Some(THEIRS),
                });
                let frame = six_frame(theirs, to, ON_LINK, &six_message(theirs, to, &ask));
                window.deliver(stack.device(), 0, &frame);
                let sent = frames(&mut stack, &window, ms);
                ms += 100;
                assert_eq!(sent.len(), 1);
                let answer = six_of(&sent[0]);
                assert_eq!(answer.to_card, THEIRS);
                assert_eq!((answer.from, answer.to, answer.hops), (ours, theirs, 255));
                let said = Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
                    flags: NdiscNeighborFlags::SOLICITED,
                    target_addr: ours,
                    lladdr: Some(OURS),
                });
                assert_eq!(answer.message, six_message(ours, theirs, &said));
            }
        }
        // For an address that is not the card's, nothing.
        let other = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        let ask = Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
            target_addr: other,
            lladdr: Some(THEIRS),
        });
        let to = other.solicited_node();
        let frame = six_frame(theirs, to, ON_LINK, &six_message(theirs, to, &ask));
        window.deliver(stack.device(), 0, &frame);
        assert!(frames(&mut stack, &window, ms).is_empty());

        // And an echo request for one of the card's is answered from it.
        let echo = |reply: bool| {
            let (ident, seq_no, data) = (7, 1, &b"is anybody there"[..]);
            if reply {
                Icmpv6Repr::EchoReply {
                    ident,
                    seq_no,
                    data,
                }
            } else {
                Icmpv6Repr::EchoRequest {
                    ident,
                    seq_no,
                    data,
                }
            }
        };
        for ours in [OURS_LINK, OURS_GIVEN] {
            let request = six_message(theirs, ours, &echo(false));
            window.deliver(stack.device(), 0, &six_frame(theirs, ours, 64, &request));
            ms += 100;
            let sent = frames(&mut stack, &window, ms);
            assert_eq!(sent.len(), 1);
            let answer = six_of(&sent[0]);
            assert_eq!(answer.to_card, THEIRS);
            assert_eq!((answer.from, answer.to), (ours, theirs));
            assert_eq!(answer.message, six_message(ours, theirs, &echo(true)));
        }
    }

    #[test]
    fn a_datagram_travels_as_ipv6_when_the_kernel_says_so() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        advertise(&mut stack, &window, 1800, 86_400);
        frames(&mut stack, &window, 100);
        assert_eq!(stack.request(&bind(7, 5000)), Ok(false));
        let near = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        let far = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let link = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let mut ms = 1000;
        // To a neighbour on the network the router gave, from the
        // address it gave; to one of the link, from the link's; and to
        // anywhere else by the router, from the one it gave. Each
        // neighbour is asked for first, from the address that sends.
        for (to, through, from, hops) in [
            (near, near, OURS_GIVEN, 0),
            (link, link, OURS_LINK, 3),
            (far, ROUTER, OURS_GIVEN, 200),
        ] {
            assert_eq!(stack.request(&send6(7, to, 7, hops, b"over six")), Ok(true));
            let sent = frames(&mut stack, &window, ms);
            assert_eq!(sent.len(), 1);
            let ask = six_of(&sent[0]);
            assert_eq!((ask.from, ask.to), (from, through.solicited_node()));
            assert_eq!(ask.message[0], 135);
            assert_eq!(ask.message[8..24], through.0);
            window.deliver(stack.device(), 0, &here_is(through, from));
            let sent = frames(&mut stack, &window, ms + 10);
            assert_eq!(sent.len(), 1);
            let hops = if hops == 0 { 64 } else { hops };
            assert_eq!(
                udp6_of(&sent[0]),
                (THEIRS, (from, 5000), (to, 7), hops, b"over six".to_vec())
            );
            ms += 1000;
        }
        // To everybody on the link, at the card every one of them hears
        // at, with nobody to ask for.
        let all = Ipv6Address::LINK_LOCAL_ALL_NODES;
        assert_eq!(stack.request(&send6(7, all, 7, 1, b"anyone")), Ok(true));
        let sent = frames(&mut stack, &window, ms);
        assert_eq!(sent.len(), 1);
        assert_eq!(
            udp6_of(&sent[0]),
            (
                group_card(all),
                (OURS_LINK, 5000),
                (all, 7),
                1,
                b"anyone".to_vec()
            )
        );

        // What cannot be done is refused, and nothing leaves.
        let longest = stack.max_datagram6();
        assert_eq!(longest, stack.max_datagram() - 20);
        let short = &send6(7, near, 7, 0, b"")[..35];
        for (m, why) in [
            (send6(8, near, 7, 0, b"x"), Refused::NoSocket),
            (send6(7, near, 0, 0, b"x"), Refused::Unreachable),
            (
                send6(7, Ipv6Address::UNSPECIFIED, 7, 0, b"x"),
                Refused::Unreachable,
            ),
            (
                send6(7, Ipv6Address::LOOPBACK, 7, 0, b"x"),
                Refused::Unreachable,
            ),
            (
                send6(7, near, 7, 0, &vec![0; longest + 1]),
                Refused::TooLong,
            ),
            (short.to_vec(), Refused::Malformed),
        ] {
            assert_eq!(stack.request(&m), Err(why));
        }
        assert_eq!(
            stack.request(&send6(7, near, 7, 0, &vec![0; longest])),
            Ok(true)
        );
        assert_eq!(frames(&mut stack, &window, ms + 100).len(), 1);
        assert!(frames(&mut stack, &window, ms + 200).is_empty());
    }

    #[test]
    fn without_a_router_only_the_link_is_reached_and_without_ipv6_nothing() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        assert_eq!(stack.request(&bind(7, 5000)), Ok(false));
        let far = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let link = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        assert_eq!(
            stack.request(&send6(7, far, 7, 0, b"x")),
            Err(Refused::Unreachable)
        );
        assert_eq!(stack.request(&send6(7, link, 7, 0, b"x")), Ok(true));
        assert_eq!(frames(&mut stack, &window, 100).len(), 1);

        let Bench { iface, .. } = Bench::new();
        let mut quiet = Stack::new(iface);
        assert_eq!(quiet.request(&bind(7, 5000)), Ok(false));
        assert_eq!(
            quiet.request(&send6(7, link, 7, 0, b"x")),
            Err(Refused::Unreachable)
        );
    }

    #[test]
    fn what_arrives_as_ipv6_is_told_with_who_sent_it_in_the_order_it_arrived() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        advertise(&mut stack, &window, 1800, 86_400);
        frames(&mut stack, &window, 100);
        assert_eq!(stack.request(&bind(7, 5000)), Ok(false));
        assert_eq!(stack.request(&bind(8, 5001)), Ok(false));
        let theirs = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        let v6 = |port: u16, data: &[u8]| udp6_frame((theirs, 4000), (OURS_GIVEN, port), data);

        // One datagram: one `DATA6`, and nothing after it.
        let mut m = Vec::new();
        window.deliver(stack.device(), 0, &v6(5000, b"first"));
        stack.poll(Instant::from_millis(200));
        assert_eq!(stack.arrived(&mut m), Arrived::More);
        assert_eq!(records6(&m), [(7, theirs, 4000, b"first".to_vec())]);
        assert!(stack.brought_bytes());
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);

        // Both kinds for one socket, and another socket's after: each
        // message is of one kind, and a socket's are in their order.
        window.deliver(stack.device(), 0, &udp_frame(4000, 5000, b"four"));
        window.deliver(stack.device(), 1, &v6(5000, b"six"));
        window.deliver(stack.device(), 2, &udp_frame(4000, 5000, b"four again"));
        window.deliver(stack.device(), 3, &v6(5001, b"six for the other"));
        stack.poll(Instant::from_millis(300));
        assert_eq!(stack.arrived(&mut m), Arrived::More);
        assert_eq!(records(&m), [(7, THEIR_IP.0, 4000, b"four".to_vec())]);
        assert_eq!(stack.arrived(&mut m), Arrived::More);
        assert_eq!(records6(&m), [(7, theirs, 4000, b"six".to_vec())]);
        assert_eq!(stack.arrived(&mut m), Arrived::More);
        assert_eq!(records(&m), [(7, THEIR_IP.0, 4000, b"four again".to_vec())]);
        assert_eq!(stack.arrived(&mut m), Arrived::More);
        assert_eq!(
            records6(&m),
            [(8, theirs, 4000, b"six for the other".to_vec())]
        );
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);

        // For a port nobody has, nothing is told.
        window.deliver(stack.device(), 0, &v6(5002, b"nobody"));
        stack.poll(Instant::from_millis(400));
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);
    }

    #[test]
    fn a_connect_travels_as_ipv6_from_the_address_that_goes_there() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        advertise(&mut stack, &window, 1800, 86_400);
        frames(&mut stack, &window, 100);
        let near = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        let link = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);

        // The message, as the kernel writes it.
        let said = connect6(7, near, 80, 49152, 0x0004_0000);
        assert_eq!(
            said[..16],
            [0, 0, 0, 0, 19, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(said[16..32], near.0);
        assert_eq!(said[32..], [80, 0, 0, 0xc0, 0, 0, 4, 0]);

        // To a neighbour on the network the router gave, from the address
        // it gave, and to one of the link, from the link's: the neighbour
        // is asked for, and then the connection is knocked for. The
        // kernel is told which address it was made from.
        let mut ms = 1000;
        for (sock, to, from, local) in [(7, near, OURS_GIVEN, 50000), (8, link, OURS_LINK, 50001)] {
            assert_eq!(
                stack.request(&connect6(sock, to, 80, local, 4096)),
                Ok(true)
            );
            let sent = frames(&mut stack, &window, ms);
            assert_eq!(sent.len(), 1);
            let ask = six_of(&sent[0]);
            assert_eq!((ask.from, ask.to), (from, to.solicited_node()));
            window.deliver(stack.device(), 0, &here_is(to, from));
            let sent = frames(&mut stack, &window, ms + 10);
            assert_eq!(sent.len(), 1);
            let syn = tcp6_of(&sent[0]);
            assert_eq!((syn.from, syn.to), ((from, local), (to, 80)));
            assert_eq!((syn.control, syn.ack), (TcpControl::Syn, None));
            assert_eq!(events(&mut stack), []);
            let answer = Segment6 {
                from: (to, 80),
                to: (from, local),
                control: TcpControl::Syn,
                seq: 7000,
                ack: Some(syn.seq.wrapping_add(1)),
                data: Vec::new(),
            };
            window.deliver(stack.device(), 0, &tcp6_frame(&answer));
            let sent = frames(&mut stack, &window, ms + 20);
            assert_eq!(sent.len(), 1);
            let ack = tcp6_of(&sent[0]);
            assert_eq!((ack.control, ack.ack), (TcpControl::None, Some(7001)));
            let told = events(&mut stack);
            assert_eq!(told.len(), 1);
            assert_eq!((told[0].0, told[0].1), (sock, EV_CONNECTED));
            assert_eq!(told[0].3, from.0);
            // And it is a connection like any other.
            assert_eq!(stack.request(&message(OP_WRITE, sock, b"hola")), Ok(true));
            let sent = frames(&mut stack, &window, ms + 30);
            assert_eq!(sent.len(), 1);
            let bytes = tcp6_of(&sent[0]);
            assert_eq!((bytes.from, bytes.to), ((from, local), (to, 80)));
            assert_eq!(bytes.data, b"hola");
            let heard = Segment6 {
                control: TcpControl::None,
                seq: 7001,
                ack: Some(bytes.seq.wrapping_add(4)),
                ..answer
            };
            window.deliver(stack.device(), 0, &tcp6_frame(&heard));
            assert!(frames(&mut stack, &window, ms + 40).is_empty());
            ms += 1000;
        }

        // Nobody to connect to at a group's address, and a message cut
        // short is none.
        let group = Ipv6Address::LINK_LOCAL_ALL_NODES;
        assert_eq!(
            stack.request(&connect6(9, group, 80, 50002, 4096)),
            Ok(true)
        );
        assert!(frames(&mut stack, &window, ms).is_empty());
        assert_eq!(
            events(&mut stack),
            [(9, EV_FAILED, WHY_UNREACHABLE, Vec::new())]
        );
        let short = &connect6(9, near, 80, 50002, 4096)[..39];
        assert_eq!(stack.request(short), Err(Refused::Malformed));
        assert_eq!(
            stack.request(&connect6(7, near, 81, 50003, 4096)),
            Err(Refused::Bind),
            "a socket that is connected"
        );
    }

    #[test]
    fn a_connect_with_no_way_as_ipv6_fails_at_once() {
        // With no router, further than the link there is no way; and
        // with no IPv6 at all there is none anywhere.
        let far = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let link = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        assert_eq!(stack.request(&connect6(7, far, 80, 50000, 4096)), Ok(true));
        assert!(frames(&mut stack, &window, 10).is_empty());
        assert_eq!(
            events(&mut stack),
            [(7, EV_FAILED, WHY_UNREACHABLE, Vec::new())]
        );
        assert_eq!(stack.request(&connect6(7, link, 80, 50000, 4096)), Ok(true));
        assert_eq!(frames(&mut stack, &window, 20).len(), 1, "asks for it");

        let Bench { window, iface, .. } = Bench::new();
        let mut stack = Stack::new(iface);
        assert_eq!(stack.request(&connect6(7, link, 80, 50000, 4096)), Ok(true));
        assert!(frames(&mut stack, &window, 10).is_empty());
        assert_eq!(
            events(&mut stack),
            [(7, EV_FAILED, WHY_UNREACHABLE, Vec::new())]
        );
    }

    #[test]
    fn a_listener_takes_what_arrives_as_ipv6_when_the_kernel_says_so() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        advertise(&mut stack, &window, 1800, 86_400);
        frames(&mut stack, &window, 100);
        let near = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        assert_eq!(
            listen(OP_LISTEN6, 7, 8080, 8, 0x0004_0000),
            [0, 0, 0, 0, 20, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0x90, 0x1f, 8, 0, 0, 0, 4, 0]
        );
        // One socket listens for both, as a socket of IPv6 does, and one
        // for IPv4 alone.
        let both = listen(OP_LISTEN6, 7, 8080, 2, 4096);
        assert_eq!(stack.request(&both), Ok(false));
        let four = listen(OP_LISTEN, 8, 8081, 2, 4096);
        assert_eq!(stack.request(&four), Ok(false));

        // Who knocks as IPv6 at the first is answered, once it is known
        // where it is, from the address it knocked at.
        let knock = |port: u16, to: u16| Segment6 {
            from: (near, port),
            to: (OURS_GIVEN, to),
            control: TcpControl::Syn,
            seq: 9000,
            ack: None,
            data: Vec::new(),
        };
        window.deliver(stack.device(), 0, &tcp6_frame(&knock(40000, 8080)));
        let sent = frames(&mut stack, &window, 1000);
        assert_eq!(sent.len(), 1);
        let ask = six_of(&sent[0]);
        assert_eq!((ask.from, ask.to), (OURS_GIVEN, near.solicited_node()));
        window.deliver(stack.device(), 0, &here_is(near, OURS_GIVEN));
        let sent = frames(&mut stack, &window, 1010);
        assert_eq!(sent.len(), 1);
        let answer = tcp6_of(&sent[0]);
        assert_eq!(
            (answer.from, answer.to),
            ((OURS_GIVEN, 8080), (near, 40000))
        );
        assert_eq!((answer.control, answer.ack), (TcpControl::Syn, Some(9001)));
        assert_eq!(events(&mut stack), [], "not made yet");
        // Made, it is told with where it came from and where it came to,
        // and what it brought follows.
        let first = Segment6 {
            control: TcpControl::None,
            seq: 9001,
            ack: Some(answer.seq.wrapping_add(1)),
            data: b"hola".to_vec(),
            ..knock(40000, 8080)
        };
        window.deliver(stack.device(), 0, &tcp6_frame(&first));
        frames(&mut stack, &window, 1020);
        let new = ACCEPTED_BIT | 1;
        let mut accepted = new.to_le_bytes().to_vec();
        accepted.extend_from_slice(&near.0);
        accepted.extend_from_slice(&[0x40, 0x9c, 0, 0]);
        accepted.extend_from_slice(&OURS_GIVEN.0);
        assert_eq!(accepted.len(), 44);
        let told = events(&mut stack);
        assert_eq!(told.len(), 2);
        assert_eq!(
            (told[0].0, told[0].1, &told[0].3),
            (7, EV_ACCEPTED6, &accepted)
        );
        assert_eq!(told[1], (new, EV_BYTES, 0, b"hola".to_vec()));

        // Who knocks as IPv6 at the listener of IPv4 is refused, and the
        // kernel hears nothing of it.
        window.deliver(stack.device(), 0, &tcp6_frame(&knock(40001, 8081)));
        let sent = frames(&mut stack, &window, 1030);
        assert_eq!(sent.len(), 1);
        let refused = tcp6_of(&sent[0]);
        assert_eq!(
            (refused.from, refused.to, refused.control),
            ((OURS_GIVEN, 8081), (near, 40001), TcpControl::Rst)
        );
        assert_eq!(events(&mut stack), []);
    }

    /// An `ICMP6` as the kernel writes it.
    fn icmp6(sock: u64, to: Ipv6Address, hops: u8, icmp: &[u8]) -> Vec<u8> {
        let mut rest = to.0.to_vec();
        rest.extend_from_slice(&[hops, 0, 0, 0]);
        rest.extend_from_slice(icmp);
        message(OP_ICMP6, sock, &rest)
    }

    /// An echo, or its answer, with this identifier and sequence 1, as a
    /// program writes it: the sum is not its to write.
    fn echo6(kind: u8, ident: u16, data: &[u8]) -> Vec<u8> {
        let mut m = vec![kind, 0, 0xde, 0xad];
        m.extend_from_slice(&ident.to_be_bytes());
        m.extend_from_slice(&[0, 1]);
        m.extend_from_slice(data);
        m
    }

    const ECHO: u8 = 128;
    const ECHO_REPLY: u8 = 129;

    #[test]
    fn the_messages_of_icmpv6_are_laid_out_as_the_kernel_lays_them_out() {
        assert_eq!((OP_ICMP6, OP_HEAR6), (21, 22));
        let to = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        let m = icmp6(0x0102, to, 9, &[128, 0, 0, 0]);
        let mut said = vec![0, 0, 0, 0, 21, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0];
        said.extend_from_slice(&[0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x99]);
        said.extend_from_slice(&[9, 0, 0, 0, 128, 0, 0, 0]);
        assert_eq!(m, said);
    }

    #[test]
    fn an_icmpv6_message_leaves_behind_a_header_of_ours_with_its_sum_written() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        advertise(&mut stack, &window, 1800, 86_400);
        frames(&mut stack, &window, 100);
        assert_eq!(stack.request(&message(OP_HEAR6, 9, &[])), Ok(false));
        assert_eq!(stack.hearing6(), 1);
        let near = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        let far = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let link = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let ping = echo6(ECHO, 0x4242, b"anybody?");
        let mut ms = 1000;
        // As a datagram: from the address that goes there, the neighbour
        // asked for first. The message is the program's but for the sum,
        // which is the stack's to write (`six_of` checks it).
        for (to, through, from, hops) in [
            (near, near, OURS_GIVEN, 64),
            (link, link, OURS_LINK, 3),
            (far, ROUTER, OURS_GIVEN, 200),
        ] {
            assert_eq!(stack.request(&icmp6(9, to, hops, &ping)), Ok(true));
            let sent = frames(&mut stack, &window, ms);
            assert_eq!(sent.len(), 1);
            let ask = six_of(&sent[0]);
            assert_eq!((ask.from, ask.to), (from, through.solicited_node()));
            window.deliver(stack.device(), 0, &here_is(through, from));
            let sent = frames(&mut stack, &window, ms + 10);
            assert_eq!(sent.len(), 1);
            let sent = six_of(&sent[0]);
            assert_eq!((sent.to_card, sent.from, sent.to), (THEIRS, from, to));
            assert_eq!(sent.hops, hops);
            assert_eq!(sent.message[..2], ping[..2]);
            assert_ne!(sent.message[2..4], ping[2..4]);
            assert_eq!(sent.message[4..], ping[4..]);
            ms += 1000;
        }
        // To everybody on the link, from the link's address, with nobody
        // to ask for; and whatever its type.
        let all = Ipv6Address::LINK_LOCAL_ALL_NODES;
        let odd = [200u8, 7, 0, 0, 1, 2, 3, 4];
        assert_eq!(stack.request(&icmp6(9, all, 1, &odd)), Ok(true));
        let sent = frames(&mut stack, &window, ms);
        assert_eq!(sent.len(), 1);
        let sent = six_of(&sent[0]);
        assert_eq!(
            (sent.to_card, sent.from, sent.to, sent.hops),
            (group_card(all), OURS_LINK, all, 1)
        );
        assert_eq!(sent.message[..2], odd[..2]);
        assert_eq!(sent.message[4..], odd[4..]);

        // What cannot be done is refused, and nothing leaves.
        let longest = stack.max_datagram6() + 8;
        assert_eq!(stack.request(&bind(7, 5000)), Ok(false));
        assert_eq!(stack.request(&message(OP_HEAR, 5, &[])), Ok(false));
        let short = &icmp6(9, near, 64, &ping)[..39];
        for (m, why) in [
            (icmp6(8, near, 64, &ping), Refused::NoSocket),
            // A datagram socket has no ICMPv6 to send, nor one that hears
            // as IPv4.
            (icmp6(7, near, 64, &ping), Refused::NoSocket),
            (icmp6(5, near, 64, &ping), Refused::NoSocket),
            (icmp6(9, near, 0, &ping), Refused::Malformed),
            (short.to_vec(), Refused::Malformed),
            (
                icmp6(9, Ipv6Address::UNSPECIFIED, 64, &ping),
                Refused::Unreachable,
            ),
            (
                icmp6(9, Ipv6Address::LOOPBACK, 64, &ping),
                Refused::Unreachable,
            ),
            (icmp6(9, near, 64, &vec![0; longest + 1]), Refused::TooLong),
            // A socket's number is one socket's.
            (message(OP_HEAR6, 9, &[]), Refused::Bind),
            (message(OP_HEAR6, 7, &[]), Refused::Bind),
            (message(OP_HEAR6, 5, &[]), Refused::Bind),
            (message(OP_HEAR6, 0, &[]), Refused::Bind),
            (message(OP_HEAR, 9, &[]), Refused::Bind),
        ] {
            assert_eq!(stack.request(&m), Err(why));
        }
        assert!(frames(&mut stack, &window, ms + 100).is_empty());
        // The longest fills a frame.
        assert_eq!(
            stack.request(&icmp6(9, near, 64, &vec![0; longest])),
            Ok(true)
        );
        let sent = frames(&mut stack, &window, ms + 200);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].len(), 14 + 1500);

        // Without a router only the link is reached, and without IPv6
        // nothing.
        let Bench { window, iface, .. } = Bench::new();
        let mut alone = Stack::new(iface);
        alone.speak_six();
        frames(&mut alone, &window, 0);
        assert_eq!(alone.request(&message(OP_HEAR6, 9, &[])), Ok(false));
        assert_eq!(
            alone.request(&icmp6(9, far, 64, &ping)),
            Err(Refused::Unreachable)
        );
        assert_eq!(alone.request(&icmp6(9, link, 64, &ping)), Ok(true));
        assert_eq!(frames(&mut alone, &window, 100).len(), 1);
        let Bench { iface, .. } = Bench::new();
        let mut quiet = Stack::new(iface);
        assert_eq!(quiet.request(&message(OP_HEAR6, 9, &[])), Ok(false));
        assert_eq!(
            quiet.request(&icmp6(9, link, 64, &ping)),
            Err(Refused::Unreachable)
        );
    }

    /// The IPv6 packet a frame carries.
    fn packet_of(frame: &[u8]) -> Vec<u8> {
        frame[14..].to_vec()
    }

    #[test]
    fn while_a_socket_hears_every_packet_of_icmpv6_goes_to_the_kernel_whole() {
        let (mut stack, window) = stack();
        frames(&mut stack, &window, 0);
        advertise(&mut stack, &window, 1800, 86_400);
        frames(&mut stack, &window, 100);
        let near = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        let pong = |data: &[u8]| six_frame(near, OURS_GIVEN, 57, &echo6(ECHO_REPLY, 7, data));
        let mut m = Vec::new();
        // Nobody hears: the answer to an echo nobody sent is dropped.
        window.deliver(stack.device(), 0, &pong(b"early"));
        stack.poll(Instant::from_millis(200));
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);

        assert_eq!(stack.request(&message(OP_HEAR6, 5, &[])), Ok(false));
        assert_eq!(stack.request(&message(OP_HEAR6, 6, &[])), Ok(false));
        assert_eq!(stack.hearing6(), 2);
        // Once, for whichever sockets: socket 0, from who sent it, no
        // port, and the packet as it came, with the hops it had left.
        let frame = pong(b"pong");
        window.deliver(stack.device(), 0, &frame);
        stack.poll(Instant::from_millis(300));
        assert_eq!(stack.arrived(&mut m), Arrived::All);
        assert_eq!(records6(&m), [(HEARD_SOCK, near, 0, packet_of(&frame))]);
        assert_eq!(packet_of(&frame)[7], 57);
        assert!(stack.brought_bytes());
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);

        // One with its sum wrong is nobody's.
        let mut wrong = pong(b"wrong");
        let last = wrong.len() - 1;
        wrong[last] ^= 1;
        window.deliver(stack.device(), 0, &wrong);
        stack.poll(Instant::from_millis(400));
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);

        // What a router says is heard too, and read here all the same;
        // and an echo for us is heard and answered here.
        let advert = advert_message(0, &[]);
        let advert = six_frame(ROUTER, ALL_NODES, ON_LINK, &advert);
        let ping = six_frame(near, OURS_GIVEN, 64, &echo6(ECHO, 9, b"you?"));
        window.deliver(stack.device(), 0, &advert);
        window.deliver(stack.device(), 1, &ping);
        let sent = frames(&mut stack, &window, 500);
        assert_eq!(stack.arrived(&mut m), Arrived::All);
        assert_eq!(
            records6(&m),
            [
                (HEARD_SOCK, ROUTER, 0, packet_of(&advert)),
                (HEARD_SOCK, near, 0, packet_of(&ping)),
            ]
        );
        assert_eq!(stack.sixes().router, None);
        // The neighbour is asked for, and what it says is heard like
        // the rest; its next echo is answered.
        // (The router said it is none, so another is asked for: 133.)
        let kinds = |sent: &[Vec<u8>]| -> Vec<u8> {
            let kinds = sent.iter().map(|frame| six_of(frame).message[0]);
            kinds.filter(|kind| *kind != 133).collect()
        };
        assert_eq!(kinds(&sent), [135]);
        let found = here_is(near, OURS_GIVEN);
        window.deliver(stack.device(), 0, &found);
        window.deliver(stack.device(), 1, &ping);
        let sent = frames(&mut stack, &window, 510);
        assert_eq!(kinds(&sent), [ECHO_REPLY]);
        assert_eq!(stack.arrived(&mut m), Arrived::All);
        assert_eq!(
            records6(&m),
            [
                (HEARD_SOCK, near, 0, packet_of(&found)),
                (HEARD_SOCK, near, 0, packet_of(&ping)),
            ]
        );

        // With what is heard of IPv4 in the same turn, each in a message
        // of its kind.
        assert_eq!(stack.request(&message(OP_HEAR, 4, &[])), Ok(false));
        let frame = pong(b"six");
        window.deliver(stack.device(), 0, &echo_reply(7, 1, b"four"));
        window.deliver(stack.device(), 1, &frame);
        stack.poll(Instant::from_millis(600));
        assert_eq!(stack.arrived(&mut m), Arrived::More);
        assert_eq!(records(&m).len(), 1);
        assert_eq!(stack.arrived(&mut m), Arrived::All);
        assert_eq!(records6(&m), [(HEARD_SOCK, near, 0, packet_of(&frame))]);
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);

        // With the last socket that hears, the kernel hears no more, and
        // what it was not told yet is nobody's.
        assert_eq!(stack.request(&message(OP_CLOSE, 5, &[])), Ok(false));
        window.deliver(stack.device(), 0, &pong(b"for the other"));
        stack.poll(Instant::from_millis(700));
        assert_eq!(stack.request(&message(OP_CLOSE, 6, &[])), Ok(false));
        assert_eq!(stack.hearing6(), 0);
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);
        window.deliver(stack.device(), 0, &pong(b"late"));
        stack.poll(Instant::from_millis(800));
        assert_eq!(stack.arrived(&mut m), Arrived::Nothing);
        // The number is free.
        assert_eq!(stack.request(&message(OP_HEAR6, 5, &[])), Ok(false));
    }
}
