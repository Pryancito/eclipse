//! The groups the machine is a member of: multicast.
//!
//! A datagram sent to a group's address (224.0.0.0/4) is for every
//! machine that says it is a member, and a machine says so for the
//! programs that asked (`IP_ADD_MEMBERSHIP`). Which sockets asked is the
//! kernel's to keep, as every option of a socket is. What it tells the
//! stack is about the machine, and about no socket (the header's socket
//! is zero):
//!
//! ```text
//! JOIN    (kernel to stack)  16  [u8; 4] group
//! LEAVE   (kernel to stack)  16  [u8; 4] group
//! ```
//!
//! A `JOIN` when the first socket becomes a member of the group, a
//! `LEAVE` when the last one is no more. Between the two the stack
//! answers for the group on the network (IGMP, version 2: a report when
//! it joins and when a router asks, a leave when it leaves) and what
//! arrives for the group goes where what arrives for the card's own
//! address goes: to the socket that has the port. Every machine is a
//! member of 224.0.0.1 without saying so, and nothing is said of it.
//!
//! IGMP is said from an address. A stack that waits for its lease keeps
//! the groups and says them when the address comes; when the address
//! changes they are left from the old one and joined from the new.
//!
//! Sending to a group asks for no membership: the datagram leaves by the
//! card to the address Ethernet has for the group, as many hops as its
//! `SEND` says (the kernel says 1 unless the program said otherwise:
//! `IP_MULTICAST_TTL`). The stack does not hear what it sends; a copy for
//! the members in this machine is the kernel's to give
//! (`IP_MULTICAST_LOOP`).

use smoltcp::phy::Device;
use smoltcp::wire::Ipv4Address;

use super::{Refused, Stack, HEADER_LEN};

/// The operations of a message that are about a group.
pub const OP_JOIN: u32 = 14;
pub const OP_LEAVE: u32 = 15;

/// The group of every machine on the network.
pub const EVERYBODY: Ipv4Address = Ipv4Address::MULTICAST_ALL_SYSTEMS;

impl<D: for<'d> Device<'d>> Stack<D> {
    /// How many groups the kernel said the machine is a member of.
    pub fn groups(&self) -> usize {
        self.groups.len()
    }

    /// `JOIN` or `LEAVE`: the machine is a member of the group `m` names
    /// from now on, or no more. One that changes nothing (a group joined
    /// twice, or left without being joined) is done already.
    pub(super) fn member(&mut self, m: &[u8], join: bool) -> Result<bool, Refused> {
        let group: [u8; 4] = m[HEADER_LEN..].try_into().map_err(|_| Refused::Malformed)?;
        let group = Ipv4Address(group);
        if !group.is_multicast() {
            return Err(Refused::Malformed);
        }
        let changed = if join {
            self.groups.insert(group)
        } else {
            self.groups.remove(&group)
        };
        Ok(changed && self.say(group, join))
    }

    /// Say on the network that the machine is a member of `group`, or
    /// that it is no more, and hear what arrives for it or stop hearing.
    /// `false` when nothing was said: there is no address to say it
    /// from, the group is everybody's, or the card has no room for a
    /// frame (a router that asks is answered all the same).
    fn say(&mut self, group: Ipv4Address, member: bool) -> bool {
        if group == EVERYBODY || self.address().is_none() {
            return false;
        }
        let said = if member {
            self.iface.join_multicast_group(group, self.now)
        } else {
            self.iface.leave_multicast_group(group, self.now)
        };
        said == Ok(true)
    }

    /// Say every group again, joined or left, around a change of the
    /// card's address.
    pub(super) fn say_groups(&mut self, member: bool) {
        let groups: Vec<Ipv4Address> = self.groups.iter().copied().collect();
        for group in groups {
            self.say(group, member);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::*;
    use crate::socks::tests::{bind, message, send_hops};
    use crate::socks::{parse_header, Arrived, OP_DATA, RECORD_LEN};
    use smoltcp::time::Instant;
    use smoltcp::wire::{
        DhcpMessageType, EthernetFrame, EthernetProtocol, IgmpRepr, IgmpVersion, IpProtocol,
        Ipv4Packet, UdpPacket,
    };

    const GROUP: Ipv4Address = Ipv4Address([239, 1, 2, 3]);
    const OTHER: Ipv4Address = Ipv4Address([239, 9, 9, 9]);

    fn join(group: Ipv4Address) -> Vec<u8> {
        message(OP_JOIN, 0, &group.0)
    }

    fn leave(group: Ipv4Address) -> Vec<u8> {
        message(OP_LEAVE, 0, &group.0)
    }

    /// A stack over a bench's card, which has not met THEIRS.
    fn stack() -> (Stack<crate::port::Port>, Window) {
        let Bench { window, iface, .. } = Bench::new();
        (Stack::new(iface), window)
    }

    /// The frames the stack has put on the wire, the card done with each.
    fn sent(stack: &mut Stack<crate::port::Port>, window: &Window) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for m in &turn(stack.device()) {
            if m.op == udrivers::net::OP_TX {
                out.push(window.sent(m));
                window.done(stack.device(), m);
            }
        }
        out
    }

    /// Poll at `ms`, and what that put on the wire.
    fn frames(stack: &mut Stack<crate::port::Port>, window: &Window, ms: i64) -> Vec<Vec<u8>> {
        stack.poll(Instant::from_millis(ms));
        sent(stack, window)
    }

    /// Poll at `ms`, and the datagrams the kernel is told arrived: the
    /// socket of each and what it carried.
    fn arrived(stack: &mut Stack<crate::port::Port>, ms: i64) -> Vec<(u64, Vec<u8>)> {
        stack.poll(Instant::from_millis(ms));
        let mut m = Vec::new();
        if stack.arrived(&mut m) == Arrived::Nothing {
            return Vec::new();
        }
        assert_eq!(parse_header(&m), Some((OP_DATA, 0)));
        let (mut rest, mut out) = (&m[HEADER_LEN..], Vec::new());
        while !rest.is_empty() {
            let sock = u64::from_le_bytes(rest[..8].try_into().unwrap());
            // From THEIRS, whoever it was for.
            assert_eq!(rest[8..12], THEIR_IP.0);
            let len = u16::from_le_bytes([rest[14], rest[15]]) as usize;
            out.push((sock, rest[RECORD_LEN..RECORD_LEN + len].to_vec()));
            rest = &rest[RECORD_LEN + len..];
        }
        out
    }

    fn report(group: Ipv4Address) -> Option<(IgmpRepr, Ipv4Address)> {
        let version = IgmpVersion::Version2;
        Some((
            IgmpRepr::MembershipReport {
                group_addr: group,
                version,
            },
            group,
        ))
    }

    #[test]
    fn a_datagram_for_a_group_leaves_for_the_card_its_members_hear_at() {
        let (mut stack, window) = stack();
        stack.request(&bind(1, 5000)).unwrap();
        // No membership asked, and nobody asked where the group is.
        let hello = send_hops(1, GROUP, 6000, 1, b"hello");
        assert_eq!(stack.request(&hello), Ok(true));
        let sent = frames(&mut stack, &window, 1);
        assert_eq!(sent.len(), 1);
        let frame = EthernetFrame::new_checked(&sent[0][..]).unwrap();
        assert_eq!(frame.ethertype(), EthernetProtocol::Ipv4);
        assert_eq!(frame.src_addr(), OURS);
        assert_eq!(frame.dst_addr().0, [0x01, 0x00, 0x5e, 1, 2, 3]);
        let ip = Ipv4Packet::new_checked(frame.payload()).unwrap();
        assert!(ip.verify_checksum());
        assert_eq!(ip.protocol(), IpProtocol::Udp);
        assert_eq!((ip.src_addr(), ip.dst_addr()), (OUR_IP, GROUP));
        // As far as it was told, which the kernel says is the next
        // machine unless the program said otherwise.
        assert_eq!(ip.hop_limit(), 1);
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert!(udp.verify_checksum(&OUR_IP.into(), &GROUP.into()));
        assert_eq!((udp.src_port(), udp.dst_port()), (5000, 6000));
        assert_eq!(udp.payload(), b"hello");
        // The high bit of the second byte is not in Ethernet's address.
        let high = Ipv4Address([224, 129, 2, 3]);
        let far = send_hops(1, high, 6000, 9, b"x");
        assert_eq!(stack.request(&far), Ok(true));
        let sent = frames(&mut stack, &window, 2);
        assert_eq!(sent.len(), 1);
        assert_eq!(&sent[0][..6], &[0x01, 0x00, 0x5e, 1, 2, 3]);
        assert_eq!(sent[0][14 + 8], 9);
        // What was sent is not heard here: the kernel has the copy for
        // this machine's members.
        assert!(arrived(&mut stack, 3).is_empty());
    }

    #[test]
    fn nothing_goes_to_a_group_from_a_card_with_no_address_and_no_connection_ever() {
        let (mut stack, window) = stack();
        stack.ask_for_lease();
        stack.request(&bind(1, 5000)).unwrap();
        assert_eq!(
            stack.request(&send_hops(1, GROUP, 6000, 1, b"x")),
            Err(Refused::Unreachable)
        );
        // Only the question for a lease.
        assert_eq!(frames(&mut stack, &window, 1).len(), 1);
        // A group is not somebody to connect to.
        let (mut stack, _window) = self::stack();
        let mut to = GROUP.0.to_vec();
        to.extend_from_slice(&80u16.to_le_bytes());
        to.extend_from_slice(&49152u16.to_le_bytes());
        to.extend_from_slice(&65536u32.to_le_bytes());
        let connect = message(crate::socks::tcp::OP_CONNECT, 7, &to);
        assert_eq!(stack.request(&connect), Ok(true));
        let mut m = Vec::new();
        stack.poll(Instant::from_millis(1));
        assert_eq!(stack.arrived(&mut m), Arrived::All);
        assert_eq!(parse_header(&m).unwrap().0, crate::socks::tcp::OP_EVENTS);
        let what = u16::from_le_bytes([m[HEADER_LEN + 8], m[HEADER_LEN + 9]]);
        assert_eq!(what, crate::socks::tcp::EV_FAILED);
    }

    #[test]
    fn a_group_that_is_joined_is_said_and_heard_until_it_is_left() {
        let (mut stack, window) = stack();
        stack.request(&bind(1, 5000)).unwrap();
        // Not a member: what is for the group is not for us.
        window.deliver(stack.device(), 0, &group_frame(GROUP, 4000, 5000, b"early"));
        assert!(arrived(&mut stack, 1).is_empty());

        assert_eq!(stack.request(&join(GROUP)), Ok(true));
        assert_eq!(stack.groups(), 1);
        // Said at once, to the group itself.
        let said = sent(&mut stack, &window);
        assert_eq!(said.len(), 1);
        assert_eq!(igmp_of(&said[0]), report(GROUP));
        // Twice is once.
        assert_eq!(stack.request(&join(GROUP)), Ok(false));
        assert!(sent(&mut stack, &window).is_empty());

        // Heard by the socket that has the port, and by no other.
        stack.request(&bind(2, 5001)).unwrap();
        window.deliver(stack.device(), 0, &group_frame(GROUP, 4000, 5000, b"hello"));
        window.deliver(stack.device(), 1, &group_frame(OTHER, 4000, 5000, b"other"));
        window.deliver(
            stack.device(),
            2,
            &group_frame(GROUP, 4000, 5002, b"nobody"),
        );
        assert_eq!(arrived(&mut stack, 2), [(1, b"hello".to_vec())]);

        // Left: said to the routers, and heard no more.
        assert_eq!(stack.request(&leave(GROUP)), Ok(true));
        assert_eq!(stack.groups(), 0);
        let said = sent(&mut stack, &window);
        assert_eq!(said.len(), 1);
        assert_eq!(
            igmp_of(&said[0]),
            Some((
                IgmpRepr::LeaveGroup { group_addr: GROUP },
                Ipv4Address::MULTICAST_ALL_ROUTERS
            ))
        );
        assert_eq!(stack.request(&leave(GROUP)), Ok(false));
        window.deliver(stack.device(), 0, &group_frame(GROUP, 4000, 5000, b"late"));
        assert!(arrived(&mut stack, 3).is_empty());
        assert!(sent(&mut stack, &window).is_empty());
    }

    #[test]
    fn what_is_no_group_is_not_joined_and_everybodys_is_not_said() {
        let (mut stack, window) = stack();
        // The same bytes `linux-object/src/net/user_stack/groups.rs`
        // builds, written out: neither side can move a field alone.
        assert_eq!(
            join(GROUP),
            [0, 0, 0, 0, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 239, 1, 2, 3]
        );
        assert_eq!(
            leave(GROUP),
            [0, 0, 0, 0, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 239, 1, 2, 3]
        );
        for not in [OUR_IP, Ipv4Address::BROADCAST, Ipv4Address::UNSPECIFIED] {
            assert_eq!(stack.request(&join(not)), Err(Refused::Malformed));
            assert_eq!(stack.request(&leave(not)), Err(Refused::Malformed));
        }
        // Short, and long.
        assert_eq!(
            stack.request(&message(OP_JOIN, 0, &GROUP.0[..3])),
            Err(Refused::Malformed)
        );
        assert_eq!(
            stack.request(&message(OP_JOIN, 0, &[239, 1, 2, 3, 0])),
            Err(Refused::Malformed)
        );
        assert_eq!(stack.groups(), 0);
        // Every machine is a member of 224.0.0.1 and none says so: what
        // arrives for it is heard, joined or not.
        stack.request(&bind(1, 5000)).unwrap();
        window.deliver(
            stack.device(),
            0,
            &group_frame(EVERYBODY, 4000, 5000, b"all"),
        );
        assert_eq!(arrived(&mut stack, 1), [(1, b"all".to_vec())]);
        assert_eq!(stack.request(&join(EVERYBODY)), Ok(false));
        assert_eq!(stack.request(&leave(EVERYBODY)), Ok(false));
        assert!(sent(&mut stack, &window).is_empty());
    }

    #[test]
    fn a_router_that_asks_is_told_every_group() {
        let (mut stack, window) = stack();
        stack.request(&join(GROUP)).unwrap();
        stack.request(&join(OTHER)).unwrap();
        assert_eq!(sent(&mut stack, &window).len(), 2);
        window.deliver(stack.device(), 0, &groups_query());
        // Not at once: within the time the router gave, one after
        // another.
        assert!(frames(&mut stack, &window, 1).is_empty());
        let mut told = Vec::new();
        for ms in (100..=1100).step_by(100) {
            for frame in frames(&mut stack, &window, ms) {
                told.push(igmp_of(&frame));
            }
        }
        assert_eq!(told, [report(GROUP), report(OTHER)]);
    }

    #[test]
    fn the_groups_wait_for_the_cards_address_and_are_said_from_it() {
        let (mut stack, window) = stack();
        stack.ask_for_lease();
        // Asked while there is no address: kept, and nothing said.
        assert_eq!(stack.request(&join(GROUP)), Ok(false));
        assert_eq!(stack.groups(), 1);
        let asked = frames(&mut stack, &window, 1);
        assert_eq!(asked.len(), 1);
        let (kind, xid, _) = dhcp_of(&asked[0]);
        assert_eq!(kind, DhcpMessageType::Discover);
        let offer = Offered {
            address: OUR_IP,
            mask: Ipv4Address([255, 255, 255, 0]),
            router: None,
            dns: [None; 3],
            seconds: 3600,
        };
        window.deliver(
            stack.device(),
            0,
            &dhcp_frame(DhcpMessageType::Offer, xid, &offer),
        );
        let asked = frames(&mut stack, &window, 2);
        assert_eq!(asked.len(), 1);
        let (kind, xid, _) = dhcp_of(&asked[0]);
        assert_eq!(kind, DhcpMessageType::Request);
        window.deliver(
            stack.device(),
            1,
            &dhcp_frame(DhcpMessageType::Ack, xid, &offer),
        );
        // With the address, the group: said from it, and heard.
        let said = frames(&mut stack, &window, 3);
        assert_eq!(stack.address().map(|cidr| cidr.address()), Some(OUR_IP));
        let said: Vec<_> = said.iter().filter_map(|frame| igmp_of(frame)).collect();
        assert_eq!(said, [report(GROUP).unwrap()]);
        stack.request(&bind(1, 5000)).unwrap();
        window.deliver(stack.device(), 2, &group_frame(GROUP, 4000, 5000, b"hello"));
        assert_eq!(arrived(&mut stack, 4), [(1, b"hello".to_vec())]);
    }
}
