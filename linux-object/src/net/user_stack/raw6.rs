//! Raw sockets of IPv6 whose stack is a process, of any protocol but
//! ICMPv6: what a program opens with `socket(AF_INET6, SOCK_RAW,
//! protocol)` to speak a protocol of its own. The ones of ICMPv6 are
//! `ping -6`'s ([`super::icmp6`]); the socket is [`ServedRawSocket`],
//! and this is what is different about it for this family.
//!
//! ```text
//! RAW6     (kernel to stack)  16  u8 protocol  17  three zero
//! PACKET6  (kernel to stack)  16  [u8; 16] to  32  u8 hop limit  33  u8 protocol
//!                             34  u16 where the sum goes  36  what the packet carries
//! ```
//!
//! No program writes the IPv6 header: such a socket writes what a
//! packet carries and reads what the packets of its protocol carry,
//! with who sent them (`rawv6_sendmsg`, `rawv6_recvmsg`). So the header
//! is the stack's to write, as for ICMPv6, and with it the choice of
//! the address the packet is from. A sum that covers the addresses is
//! the stack's to write too, where the program says it goes
//! (`IPV6_CHECKSUM`); the message says where, or [`NO_SUM`].
//!
//! From its `RAW6` to its `CLOSE` every IPv6 packet of the socket's
//! protocol the card brings goes to the kernel whole, as what is heard
//! of ICMPv6 does -- a record of a `DATA6` for socket 0 -- and the
//! kernel gives each raw socket what the packets of its protocol carry.
//! The stack still does with them what it did.
//!
//! A socket of `IPPROTO_RAW` only sends, and what it writes starts with
//! an IPv6 header of the program's: where to, how far and of which
//! protocol are read off it, and the rest is the stack's as for any
//! other.
//!
//! Not here: a sum that is wrong making what arrives nobody's
//! (`IPV6_CHECKSUM` is for what leaves), `IPV6_HDRINCL` on a socket of
//! another protocol, a socket bound to one address hearing only what is
//! for it or sending from it, and the headers between IPv6's and what a
//! packet carries.

use super::icmp::checksum;
use super::icmp6::{destination, int, HOPS_HERE, IPPROTO_IP, IPPROTO_IPV6, IPV6_CHECKSUM};
use super::icmp6::{IP_HEADER, SOL_RAW};
use super::raw::ServedRawSocket;
use super::*;
use alloc::borrow::Cow;

/// The operations of a message.
pub const OP_RAW6: u32 = 34;
pub const OP_PACKET6: u32 = 35;

/// What a `PACKET6` says for a packet with no sum to write.
pub const NO_SUM: u16 = 0xffff;

/// ICMPv6, whose raw sockets are [`super::icmp6`]'s.
pub(super) const ICMPV6: u8 = 58;
/// `IPPROTO_RAW`: a socket that only sends, and whose program writes
/// the header.
const IPPROTO_RAW: u8 = 255;
/// `IPV6_HDRINCL`.
const IPV6_HDRINCL: usize = 36;
/// What a datagram's header takes of what an IPv6 packet carries: the
/// stack says the longest datagram it carries as IPv4, and IPv6's
/// header is so much longer.
const UDP_HEADER: usize = 8;
const IPV6_MORE: usize = 20;

/// `RAW6`: socket `sock` speaks `protocol` of IPv6 from now on: a raw
/// socket that is not one of ICMPv6.
pub fn raw6_message(sock: u64, protocol: u8) -> Vec<u8> {
    let mut m = header(OP_RAW6, sock, 4);
    m.extend_from_slice(&[protocol, 0, 0, 0]);
    m
}

/// `PACKET6`: `carried`, of `protocol`, from socket `sock` to `addr`, to
/// travel `hop_limit` hops at most, which is not 0. Its sum is the
/// stack's to write at `sum_at` of `carried`, unless that is [`NO_SUM`].
pub fn packet6_message(
    sock: u64,
    addr: [u8; 16],
    hop_limit: u8,
    protocol: u8,
    sum_at: u16,
    carried: &[u8],
) -> Vec<u8> {
    let mut m = header(OP_PACKET6, sock, 20 + carried.len());
    m.extend_from_slice(&addr);
    m.extend_from_slice(&[hop_limit, protocol]);
    m.extend_from_slice(&sum_at.to_le_bytes());
    m.extend_from_slice(carried);
    m
}

/// What the IPv6 packet `packet` carries right behind its header: of
/// which protocol, and the hops the packet had left.
pub(super) fn carried_of(packet: &[u8]) -> Option<(u8, u8, &[u8])> {
    if packet.len() < IP_HEADER || packet[0] >> 4 != 6 {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    let carried = packet.get(IP_HEADER..IP_HEADER + len)?;
    Some((packet[6], packet[7], carried))
}

/// Write into `carried`, at `at`, the sum it has when it goes from
/// `from` to `to` as `protocol` (RFC 8200, 8.1).
fn seal(from: Ipv6Address, to: Ipv6Address, protocol: u8, carried: &mut [u8], at: usize) {
    carried[at..at + 2].copy_from_slice(&[0, 0]);
    let mut summed = Vec::with_capacity(IP_HEADER + carried.len());
    summed.extend_from_slice(&from.0);
    summed.extend_from_slice(&to.0);
    summed.extend_from_slice(&(carried.len() as u32).to_be_bytes());
    summed.extend_from_slice(&[0, 0, 0, protocol]);
    summed.extend_from_slice(carried);
    let sum = checksum(&summed);
    carried[at..at + 2].copy_from_slice(&sum.to_be_bytes());
}

/// The options of a raw socket of `AF_INET6`.
#[derive(Default)]
pub(super) struct Opts {
    /// The hops, and whether `recvmsg` says them.
    pub(super) hops: icmp6::Opts,
    /// `IPV6_CHECKSUM`: where in what a packet carries its sum is
    /// written, when the socket asked for one.
    sum_at: Option<u16>,
}

impl Shared {
    /// Queue `carried`, what an IPv6 packet of `protocol` from `from`
    /// carried that had `hops` left, when this is a raw socket of that
    /// protocol.
    fn deliver_raw6(&self, from: Ipv6Address, hops: u8, protocol: u8, carried: &[u8]) {
        if self.v6 && self.hears == Hears::Packets(protocol) {
            let from = IpEndpoint::new(IpAddress::Ipv6(from), 0);
            self.deliver_with(from, carried, hops);
        }
    }
}

impl StackLink {
    /// What an IPv6 packet of `protocol` from `from` carried, off the
    /// card or made here: to every raw socket of that protocol.
    pub(super) fn heard_raw6(&self, from: Ipv6Address, hops: u8, protocol: u8, carried: &[u8]) {
        for shared in self.hearing() {
            shared.deliver_raw6(from, hops, protocol, carried);
        }
    }
}

impl ServedRawSocket {
    /// What a socket of `IPPROTO_RAW` wrote, which starts with an IPv6
    /// header: where it is to, how far, of which protocol, and what it
    /// carries. What is no IPv6 header is `EINVAL`, and so is a packet
    /// with no hops to travel, which no stack carries.
    fn included(data: &[u8]) -> LxResult<(Ipv6Address, u8, u8, &[u8])> {
        if data.len() < IP_HEADER || data[0] >> 4 != 6 || data[7] == 0 {
            return Err(LxError::EINVAL);
        }
        let to = Ipv6Address::from_bytes(&data[24..40]);
        Ok((to, data[7], data[6], &data[IP_HEADER..]))
    }

    /// `write`, on a socket of `AF_INET6`.
    pub(super) fn write6(&self, data: &[u8], to: Option<Endpoint>) -> SysResult {
        // The length of what an IPv6 packet carries is 16 bits.
        if data.len() > usize::from(u16::MAX) {
            return Err(LxError::EMSGSIZE);
        }
        let (to, hop_limit, protocol, carried) = if self.protocol == IPPROTO_RAW {
            let (to, hops, protocol, carried) = Self::included(data)?;
            (
                destination(IpAddress::Ipv6(to))?,
                Some(hops),
                protocol,
                carried,
            )
        } else {
            let to = match to {
                Some(Endpoint::Ip(to)) => to,
                Some(_) => return Err(LxError::EAFNOSUPPORT),
                None => {
                    let remote = self.shared.inner.lock().remote;
                    remote.ok_or(LxError::EDESTADDRREQ)?
                }
            };
            (destination(to.addr)?, None, self.protocol, data)
        };
        let (hop_limit, sum_at) = {
            let six = self.six.lock();
            (
                hop_limit.unwrap_or_else(|| six.hops.hops_to(&to)),
                six.sum_at,
            )
        };
        // The sum has to have where to be written
        // (`rawv6_push_pending_frames`).
        if sum_at.is_some_and(|at| usize::from(at) + 2 > carried.len()) {
            return Err(LxError::EINVAL);
        }
        if self.link.is_here6(&to) {
            // Neither a frame nor the stack's: `lo` counts it, and the
            // raw sockets of its protocol hear it, from the address it
            // was sent to.
            let mut carried = Cow::Borrowed(carried);
            if let Some(at) = sum_at {
                seal(to, to, protocol, carried.to_mut(), usize::from(at));
            }
            crate::net::count_on_lo(carried.len(), IP_HEADER);
            self.link.heard_raw6(to, HOPS_HERE, protocol, &carried);
            return Ok(data.len());
        }
        // Nothing leaves the machine while no stack serves, nor for
        // where the card has no way to.
        if self.link.is_down() {
            return Err(self.link.down());
        }
        if !self.link.sixes.reaches(&to) {
            return Err(LxError::ENETUNREACH);
        }
        // What one frame carries behind an IPv6 header: the stack does
        // not fragment.
        let longest = self.link.max_datagram.load(Ordering::Acquire) + UDP_HEADER;
        if carried.len() + IPV6_MORE > longest {
            return Err(LxError::EMSGSIZE);
        }
        let sum_at = sum_at.unwrap_or(NO_SUM);
        let said = packet6_message(self.sock, to.0, hop_limit, protocol, sum_at, carried);
        self.link.tell(said, false)?;
        Ok(data.len())
    }

    /// `connect`, on a socket of `AF_INET6`.
    pub(super) fn connect6(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(peer) = endpoint else {
            return Err(LxError::EAFNOSUPPORT);
        };
        let peer = destination(peer.addr)?;
        // Who answers has no port to answer from.
        self.shared.inner.lock().remote = Some(IpEndpoint::new(IpAddress::Ipv6(peer), 0));
        Ok(0)
    }

    /// `bind`, on a socket of `AF_INET6`: it has no port, and a
    /// `sockaddr`'s is not looked at.
    pub(super) fn bind6(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(local) = endpoint else {
            return Err(LxError::EINVAL);
        };
        match local.addr {
            IpAddress::Unspecified => Ok(0),
            IpAddress::Ipv6(addr) if addr.is_unspecified() || self.link.is_here6(&addr) => Ok(0),
            IpAddress::Ipv6(_) => Err(LxError::EADDRNOTAVAIL),
            _ => Err(LxError::EAFNOSUPPORT),
        }
    }

    /// `setsockopt`, on a socket of `AF_INET6`: what it answers for an
    /// option that is one of these sockets', `None` for any other.
    pub(super) fn setsockopt6(&self, level: usize, opt: usize, data: &[u8]) -> Option<SysResult> {
        let done = |set: LxResult<()>| Some(set.map(|()| 0));
        match (level, opt) {
            // IPv4's are no options of a socket of this family.
            (IPPROTO_IP, _) => Some(Err(LxError::ENOPROTOOPT)),
            // Where the sum is written, at either level
            // (`rawv6_setsockopt`): nowhere when less than 0, and never
            // across two words (`do_rawv6_setsockopt`).
            (SOL_RAW | IPPROTO_IPV6, IPV6_CHECKSUM) => done(int(data).and_then(|at| {
                let at = match at {
                    at if at < 0 => None,
                    at if at & 1 != 0 => return Err(LxError::EINVAL),
                    at => Some(at.try_into().map_err(|_| LxError::EINVAL)?),
                };
                self.six.lock().sum_at = at;
                Ok(())
            })),
            // The header is the stack's to write.
            (SOL_RAW | IPPROTO_IPV6, IPV6_HDRINCL) => Some(Err(LxError::ENOPROTOOPT)),
            (IPPROTO_IPV6, _) => {
                let mut six = self.six.lock();
                let set = six.hops.set_hops(opt, data);
                done(set.or_else(|| six.hops.set_tell_hops(opt, data))?)
            }
            _ => None,
        }
    }

    /// `getsockopt`, on a socket of `AF_INET6`, of the options that are
    /// these sockets'.
    pub(super) fn getsockopt6(&self, level: usize, opt: usize) -> Option<Vec<u8>> {
        let six = self.six.lock();
        let said = match (level, opt) {
            (SOL_RAW | IPPROTO_IPV6, IPV6_CHECKSUM) => six.sum_at.map_or(-1, i32::from),
            (IPPROTO_IPV6, _) => six.hops.get(opt)?,
            _ => return None,
        };
        Some(said.to_ne_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{at6, data6, ip, stack, Stack, MAX_FOR_TESTS};
    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    const OSPF: u8 = 89;
    /// The card's addresses, a neighbour on its network, and somewhere
    /// past a router there is not.
    const LINK: &str = "fe80::7";
    const GIVEN: &str = "fec0::7";
    const NEAR: &str = "fec0::99";
    const FAR: &str = "2001:db8::1";
    const HERE: &str = "::1";

    fn six(addr: &str) -> Ipv6Address {
        match at6(addr, 0).addr {
            IpAddress::Ipv6(addr) => addr,
            _ => unreachable!(),
        }
    }

    /// A stack whose card has LINK and GIVEN, and no router.
    fn stack6() -> Stack {
        let stack = stack();
        let cards = [Ipv6Cidr::new(six(LINK), 64), Ipv6Cidr::new(six(GIVEN), 64)];
        stack.say(six_message(None, &cards));
        stack
    }

    fn socket(link: &Arc<StackLink>, protocol: u8) -> ServedRawSocket {
        let s = ServedRawSocket::new6(link.clone(), protocol).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    /// A destination, as `sendto` gives it: no port.
    fn to(addr: &str) -> Option<Endpoint> {
        Some(Endpoint::Ip(at6(addr, 0)))
    }

    /// What `recv` reads next, and who sent it.
    fn next(s: &ServedRawSocket) -> Option<(Vec<u8>, IpEndpoint)> {
        let mut buf = [0u8; 2048];
        match async_std::task::block_on(Socket::read(s, &mut buf)) {
            (Ok(n), from) => Some((buf[..n].to_vec(), ip(from))),
            (Err(LxError::EAGAIN), _) => None,
            (Err(e), _) => panic!("{:?}", e),
        }
    }

    /// The IPv6 packet that carries `carried`, of `protocol`, from
    /// `from` to `to` with `hops` left.
    fn packet(from: &str, to: &str, protocol: u8, hops: u8, carried: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x60, 0, 0, 0];
        packet.extend_from_slice(&(carried.len() as u16).to_be_bytes());
        packet.extend_from_slice(&[protocol, hops]);
        packet.extend_from_slice(&six(from).0);
        packet.extend_from_slice(&six(to).0);
        packet.extend_from_slice(carried);
        packet
    }

    fn int(value: i32) -> [u8; 4] {
        value.to_ne_bytes()
    }

    #[test]
    fn the_messages_of_raw_ipv6_are_laid_out_as_the_stack_reads_them() {
        assert_eq!((OP_RAW6, OP_PACKET6), (34, 35));
        assert_eq!(
            raw6_message(0x0102, 89),
            [0, 0, 0, 0, 34, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 89, 0, 0, 0]
        );
        let m = packet6_message(0x0102, six("fe80::1").0, 9, 89, 0x0304, &[7, 8]);
        let mut said = vec![0, 0, 0, 0, 35, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0];
        said.extend_from_slice(&six("fe80::1").0);
        said.extend_from_slice(&[9, 89, 4, 3, 7, 8]);
        assert_eq!(m, said);
    }

    #[test]
    fn a_raw_ipv6_socket_says_its_protocol_and_sends_what_a_packet_carries() {
        let _g = LOCK.lock();
        let stack = stack6();
        let s = socket(&stack.link, OSPF);
        assert_eq!(stack.told(), Some(raw6_message(s.sock, OSPF)));
        assert_eq!(s.so_domain(), Some(Domain::AF_INET6 as u32));
        assert_eq!(s.so_protocol(), Some(u32::from(OSPF)));
        assert!(!s.is_raw_ipv4());

        // What it writes is what the packet carries: 64 hops, no sum.
        assert_eq!(Socket::write(&s, b"hello", to(NEAR)), Ok(5));
        let said = packet6_message(s.sock, six(NEAR).0, 64, OSPF, NO_SUM, b"hello");
        assert_eq!(stack.told(), Some(said));
        // The hops it was told, to one and to a group; and a sum where
        // it was told, at either level.
        let hops = icmp6::IPV6_UNICAST_HOPS;
        assert_eq!(s.setsockopt(IPPROTO_IPV6, hops, &int(3)), Ok(0));
        assert_eq!(s.setsockopt(SOL_RAW, IPV6_CHECKSUM, &int(2)), Ok(0));
        assert_eq!(
            s.getsockopt_own(IPPROTO_IPV6, IPV6_CHECKSUM),
            Some(int(2).to_vec())
        );
        assert_eq!(s.getsockopt_own(IPPROTO_IPV6, hops), Some(int(3).to_vec()));
        assert_eq!(Socket::write(&s, b"hello", to(NEAR)), Ok(5));
        let said = packet6_message(s.sock, six(NEAR).0, 3, OSPF, 2, b"hello");
        assert_eq!(stack.told(), Some(said));
        assert_eq!(Socket::write(&s, b"hello", to("ff02::5")), Ok(5));
        let said = packet6_message(s.sock, six("ff02::5").0, 1, OSPF, 2, b"hello");
        assert_eq!(stack.told(), Some(said));
        // The sum has to fit; an odd place is none, and less than 0 is
        // no sum again.
        assert_eq!(Socket::write(&s, b"abc", to(NEAR)), Err(LxError::EINVAL));
        let sum = |at: i32| s.setsockopt(IPPROTO_IPV6, IPV6_CHECKSUM, &int(at));
        assert_eq!(sum(3), Err(LxError::EINVAL));
        assert_eq!(sum(-1), Ok(0));
        assert_eq!(
            s.getsockopt_own(SOL_RAW, IPV6_CHECKSUM),
            Some(int(-1).to_vec())
        );
        assert_eq!(Socket::write(&s, b"abc", to(NEAR)), Ok(3));
        assert!(stack.told().is_some());

        // To its peer, once it has one; to nobody before.
        let alone = socket(&stack.link, OSPF);
        assert!(stack.told().is_some());
        assert_eq!(
            Socket::write(&alone, b"x", None),
            Err(LxError::EDESTADDRREQ)
        );
        let connect = Socket::connect(&alone, Endpoint::Ip(at6(NEAR, 7)));
        assert_eq!(async_std::task::block_on(connect), Ok(0));
        assert_eq!(Socket::write(&alone, b"x", None), Ok(1));
        let said = packet6_message(alone.sock, six(NEAR).0, 64, OSPF, NO_SUM, b"x");
        assert_eq!(stack.told(), Some(said));

        // What cannot go is refused, and the stack is told nothing.
        let long = vec![0u8; MAX_FOR_TESTS + UDP_HEADER - IPV6_MORE + 1];
        assert_eq!(Socket::write(&s, &long, to(NEAR)), Err(LxError::EMSGSIZE));
        assert_eq!(
            Socket::write(&s, &long[..long.len() - 1], to(NEAR)),
            Ok(long.len() - 1)
        );
        assert!(stack.told().is_some());
        assert_eq!(Socket::write(&s, b"x", to(FAR)), Err(LxError::ENETUNREACH));
        let mapped = to("::ffff:10.0.2.2");
        assert_eq!(Socket::write(&s, b"x", mapped), Err(LxError::ENETUNREACH));
        let four = Some(Endpoint::Ip(IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Address([10, 0, 2, 2])),
            0,
        )));
        assert_eq!(Socket::write(&s, b"x", four), Err(LxError::EAFNOSUPPORT));
        // IPv4's options are not this family's, and the header is never
        // the program's.
        assert_eq!(s.setsockopt(0, 3, &int(1)), Err(LxError::ENOPROTOOPT));
        let hdrincl = s.setsockopt(IPPROTO_IPV6, IPV6_HDRINCL, &int(1));
        assert_eq!(hdrincl, Err(LxError::ENOPROTOOPT));
        assert_eq!(stack.told(), None);
        // An address of the machine's, or none.
        assert_eq!(Socket::bind(&s, Endpoint::Ip(at6(GIVEN, 0))), Ok(0));
        assert_eq!(Socket::bind(&s, Endpoint::Ip(at6("::", 0))), Ok(0));
        let elsewhere = Socket::bind(&s, Endpoint::Ip(at6(NEAR, 0)));
        assert_eq!(elsewhere, Err(LxError::EADDRNOTAVAIL));

        drop(s);
        assert!(stack.told().is_some());
    }

    #[test]
    fn a_raw_ipv6_socket_reads_what_the_packets_of_its_protocol_carry() {
        let _g = LOCK.lock();
        let stack = stack6();
        let (a, b) = (socket(&stack.link, OSPF), socket(&stack.link, OSPF));
        let other = socket(&stack.link, 90);
        let four = ServedRawSocket::new(stack.link.clone(), OSPF).unwrap();
        FileLike::set_flags(&four, OpenFlags::NON_BLOCK).unwrap();
        let hears = |protocol: u8, carried: &[u8]| {
            let packet = packet(NEAR, GIVEN, protocol, 57, carried);
            stack.say(data6(HEARD, NEAR, 0, &packet));
        };
        hears(OSPF, b"to both");
        hears(90, b"to the other");
        // ICMPv6 is not theirs, nor what is cut short.
        hears(ICMPV6, &[129, 0, 0, 0, 0, 0, 0, 0]);
        let mut short = packet(NEAR, GIVEN, OSPF, 57, b"cut short");
        short.truncate(44);
        stack.say(data6(HEARD, NEAR, 0, &short));
        for s in [&a, &b] {
            assert_eq!(next(s), Some((b"to both".to_vec(), at6(NEAR, 0))));
            assert_eq!(next(s), None);
        }
        assert_eq!(next(&other), Some((b"to the other".to_vec(), at6(NEAR, 0))));
        assert_eq!(next(&other), None);
        // A socket of IPv4 of the same protocol hears none of it.
        let mut buf = [0u8; 64];
        let read = async_std::task::block_on(Socket::read(&four, &mut buf));
        assert_eq!(read.0, Err(LxError::EAGAIN));

        // `recvmsg` says the hops to who asked.
        const IPV6_RECVHOPLIMIT: usize = 51;
        assert_eq!(
            a.setsockopt(IPPROTO_IPV6, IPV6_RECVHOPLIMIT, &int(1)),
            Ok(0)
        );
        hears(OSPF, b"again");
        assert_eq!(next(&a), Some((b"again".to_vec(), at6(NEAR, 0))));
        assert_eq!(a.recv_cmsgs(), [(41, 52, 57)]);
        assert_eq!(b.recv_cmsgs(), []);
    }

    #[test]
    fn what_a_raw_ipv6_socket_sends_this_machine_never_leaves_it() {
        let _g = LOCK.lock();
        let stack = stack6();
        let (a, b) = (socket(&stack.link, OSPF), socket(&stack.link, OSPF));
        while stack.told().is_some() {}
        assert_eq!(a.setsockopt(SOL_RAW, IPV6_CHECKSUM, &int(2)), Ok(0));
        for here in [HERE, GIVEN, LINK] {
            assert_eq!(Socket::write(&a, b"ab\0\0cd", to(here)), Ok(6));
            // Both hear it, from the address it was sent to, with the
            // sum where it was asked for.
            let mut sealed = b"ab\0\0cd".to_vec();
            seal(six(here), six(here), OSPF, &mut sealed, 2);
            assert_ne!(sealed[2..4], [0, 0]);
            for s in [&a, &b] {
                assert_eq!(next(s), Some((sealed.clone(), at6(here, 0))));
            }
        }
        // Nowhere is here (`rawv6_sendmsg`).
        assert_eq!(Socket::write(&b, b"plain", to("::")), Ok(5));
        assert_eq!(next(&a), Some((b"plain".to_vec(), at6(HERE, 0))));
        assert_eq!(stack.told(), None);
    }

    #[test]
    fn a_socket_of_ipproto_raw_of_ipv6_writes_a_header_that_is_read() {
        let _g = LOCK.lock();
        let stack = stack6();
        let s = socket(&stack.link, 255);
        assert_eq!(stack.told(), Some(raw6_message(s.sock, 255)));
        // Where to, how far and of which protocol are the header's;
        // where `sendto` says is not looked at.
        let whole = packet("::", NEAR, OSPF, 9, b"made by hand");
        assert_eq!(Socket::write(&s, &whole, to(FAR)), Ok(whole.len()));
        let said = packet6_message(s.sock, six(NEAR).0, 9, OSPF, NO_SUM, b"made by hand");
        assert_eq!(stack.told(), Some(said));
        // What is no header of IPv6, or goes no hops, is refused.
        assert_eq!(Socket::write(&s, &whole[..39], None), Err(LxError::EINVAL));
        let four = [0x45u8; 60];
        assert_eq!(Socket::write(&s, &four, None), Err(LxError::EINVAL));
        let still = packet("::", NEAR, OSPF, 0, b"x");
        assert_eq!(Socket::write(&s, &still, None), Err(LxError::EINVAL));
        assert_eq!(stack.told(), None);
    }
}
