//! The groups of IPv6 the machine is a member of, and saying so: MLD.
//!
//! As with IPv4's ([`super::groups`]), which sockets asked for a group
//! (`IPV6_JOIN_GROUP`) is the kernel's to keep, and what it tells the
//! stack is about the machine and about no socket:
//!
//! ```text
//! JOIN6   (kernel to stack)  16  [u8; 16] group
//! LEAVE6  (kernel to stack)  16  [u8; 16] group
//! ```
//!
//! A `JOIN6` when the first socket becomes a member of the group, a
//! `LEAVE6` when the last one is no more. Between the two, what arrives
//! for the group goes where what arrives for the card's own addresses
//! goes, to the socket that has the port; what arrives for a group the
//! machine is no member of is nobody's and is dropped, as Linux drops
//! it. Every machine is a member of `ff02::1` without saying so, and of
//! the solicited-node group of each of its addresses, where the
//! neighbours ask for it.
//!
//! On the network the stack speaks MLD, version 2 (RFC 3810), by hand:
//! smoltcp can read and write its messages and sends none.
//!
//! - When the machine becomes a member of a group, or is one no more, a
//!   report says the change (a record `CHANGE_TO_EXCLUDE` of no sources
//!   to join, `CHANGE_TO_INCLUDE` of none to leave), and says it again a
//!   second later: one may be lost. The solicited-node groups are said
//!   too, as the addresses come and go: a switch that listens to MLD
//!   would otherwise not pass on what the neighbours ask.
//! - When a router asks (a query, of every group or of one), a report
//!   says what the machine is a member of (`MODE_IS_EXCLUDE`).
//!
//! Reports go to `ff02::16` from the card's address on the link, one
//! hop, behind a hop-by-hop header with a router alert, which is how
//! they and the queries travel: the socket they are sent and heard by is
//! one of whole IPv6 packets with that header. Nothing is said of
//! `ff02::1` nor of a group that does not leave the machine.
//!
//! Not done: the answers of version 1 (a router that only speaks it is
//! not answered), waiting a random time before answering, and
//! memberships by source.

use std::collections::BTreeSet;

use smoltcp::phy::Device;
use smoltcp::socket::{RawPacketMetadata, RawSocket, RawSocketBuffer, SocketHandle};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{Icmpv6Packet, Ipv6Repr};
use smoltcp::wire::{IpAddress, IpCidr, IpProtocol, IpVersion, Ipv6Address, Ipv6Packet};

use super::six::link_local;
use super::{Refused, Stack, HEADER_LEN};

/// The operations of a message that are about a group of IPv6.
pub const OP_JOIN6: u32 = 24;
pub const OP_LEAVE6: u32 = 25;

/// Where the reports go: the routers that speak MLD version 2.
pub const REPORTS_TO: Ipv6Address =
    Ipv6Address([0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x16]);

/// ICMPv6: a query and a report of version 2.
const QUERY: u8 = 130;
const REPORT: u8 = 143;
/// The shortest query of version 2.
const QUERY_LEN: usize = 28;
/// What a record of a report says of its group: that the machine is a
/// member, that it is one no more, that it became one.
pub const IS_MEMBER: u8 = 2;
pub const LEFT: u8 = 3;
pub const JOINED: u8 = 4;
/// The hop-by-hop header of every message of MLD: ICMPv6 is next, eight
/// bytes, a router alert that says MLD, and two of padding.
const ALERT: [u8; 8] = [58, 0, 5, 2, 0, 0, 1, 0];
const IP_HEADER: usize = 40;
/// A report before its records, and a record with no sources.
const REPORT_HEADER: usize = 8;
const RECORD_LEN: usize = 20;
/// The records one report carries at most: what fits the smallest
/// packet a link of IPv6 carries (1280 bytes).
const RECORDS: usize = 60;
/// How long after a change is said it is said again.
const AGAIN: Duration = Duration::from_secs(1);
/// The packets of MLD that wait to be read, and to be sent.
const HEARD: usize = 8;
const TO_SAY: usize = 4;

/// The stack's side of the groups of IPv6.
#[derive(Default)]
pub(super) struct Mld {
    /// The socket the queries reach and the reports leave by: on a stack
    /// that speaks IPv6.
    ear: Option<SocketHandle>,
    /// The groups the machine is a member of, as the kernel said.
    groups: BTreeSet<Ipv6Address>,
    /// The solicited-node groups of the card's addresses, as last said.
    solicited: BTreeSet<Ipv6Address>,
    /// What there is to say, and from when.
    owed: Vec<Owed>,
}

/// Something to say of a group.
#[derive(Clone, Copy)]
struct Owed {
    /// From when.
    at: Instant,
    group: Ipv6Address,
    /// What a record says of it.
    what: u8,
    /// Whether it is said once again, [`AGAIN`] after it was said: a
    /// change is, an answer is not.
    again: bool,
}

impl Mld {
    /// When the next thing is to be said, when anything is.
    pub(super) fn next(&self) -> Option<Instant> {
        self.owed.iter().map(|owed| owed.at).min()
    }

    /// Whether the machine is a member of `group` by what is kept here.
    fn has(&self, group: &Ipv6Address) -> bool {
        self.groups.contains(group) || self.solicited.contains(group)
    }
}

/// Whether anything is said of `group` on the network: not of
/// everybody's, nor of one that does not leave the machine.
fn said_of(group: &Ipv6Address) -> bool {
    *group != Ipv6Address::LINK_LOCAL_ALL_NODES && group.0[1] & 0x0f >= 2
}

/// The solicited-node group of `address`: where a neighbour asks for it.
fn solicited_node(address: &Ipv6Address) -> Ipv6Address {
    let mut group = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, 0, 0, 0];
    group[13..].copy_from_slice(&address.0[13..]);
    Ipv6Address(group)
}

/// A report from `from`: a whole IPv6 packet that says of each group of
/// `records` what goes with it.
pub fn report(from: Ipv6Address, records: &[(Ipv6Address, u8)]) -> Vec<u8> {
    let message_len = REPORT_HEADER + RECORD_LEN * records.len();
    let ip = Ipv6Repr {
        src_addr: from,
        dst_addr: REPORTS_TO,
        next_header: IpProtocol::HopByHop,
        payload_len: ALERT.len() + message_len,
        hop_limit: 1,
    };
    let mut packet = vec![0u8; IP_HEADER + ALERT.len() + message_len];
    let (header, rest) = packet.split_at_mut(IP_HEADER);
    ip.emit(&mut Ipv6Packet::new_unchecked(header));
    let (alert, message) = rest.split_at_mut(ALERT.len());
    alert.copy_from_slice(&ALERT);
    message[0] = REPORT;
    message[6..8].copy_from_slice(&(records.len() as u16).to_be_bytes());
    let (written, _) = message[REPORT_HEADER..].as_chunks_mut::<RECORD_LEN>();
    for (record, (group, what)) in written.iter_mut().zip(records) {
        record[0] = *what;
        record[4..].copy_from_slice(&group.0);
    }
    Icmpv6Packet::new_unchecked(message)
        .fill_checksum(&IpAddress::Ipv6(from), &IpAddress::Ipv6(REPORTS_TO));
    packet
}

/// The group `packet`, a whole IPv6 packet, asks about when it is a
/// query to answer: from an address of the link, never through a router,
/// with its sum right. The address of no group when it asks about every
/// one.
pub fn query(packet: &[u8]) -> Option<Ipv6Address> {
    let ip = Ipv6Packet::new_checked(packet).ok()?;
    let (from, to) = (ip.src_addr(), ip.dst_addr());
    if ip.next_header() != IpProtocol::HopByHop || ip.hop_limit() != 1 || !from.is_link_local() {
        return None;
    }
    // The hop-by-hop header says what follows it and how long it is, in
    // eights of bytes beyond the first eight.
    let rest = ip.payload();
    if rest.len() < 2 || rest[0] != 58 {
        return None;
    }
    let message = rest.get((usize::from(rest[1]) + 1) * 8..)?;
    if message.len() < QUERY_LEN || message[0] != QUERY || message[1] != 0 {
        return None;
    }
    let icmp = Icmpv6Packet::new_checked(message).ok()?;
    if !icmp.verify_checksum(&IpAddress::Ipv6(from), &IpAddress::Ipv6(to)) {
        return None;
    }
    let group = Ipv6Address::from_bytes(&message[8..24]);
    (group.is_unspecified() || group.is_multicast()).then_some(group)
}

impl<D: for<'d> Device<'d>> Stack<D> {
    /// How many groups of IPv6 the kernel said the machine is a member
    /// of.
    pub fn groups6(&self) -> usize {
        self.mld.groups.len()
    }

    /// `JOIN6` or `LEAVE6`: the machine is a member of the group `m`
    /// names from now on, or no more. One that changes nothing (a group
    /// joined twice, or left without being joined) is done already.
    pub(super) fn member6(&mut self, m: &[u8], join: bool) -> Result<bool, Refused> {
        let group: [u8; 16] = m[HEADER_LEN..].try_into().map_err(|_| Refused::Malformed)?;
        let group = Ipv6Address(group);
        if !group.is_multicast() {
            return Err(Refused::Malformed);
        }
        let changed = if join {
            self.mld.groups.insert(group)
        } else {
            self.mld.groups.remove(&group)
        };
        if !changed {
            return Ok(false);
        }
        // An interface with no room for groups hears every one.
        let _ = if join {
            self.iface.join_multicast_group(group, self.now)
        } else {
            self.iface.leave_multicast_group(group, self.now)
        };
        // The solicited-node group of an address stays the machine's.
        if self.mld.solicited.contains(&group) {
            return Ok(false);
        }
        Ok(self.mld_changed(group, join))
    }

    /// The machine became a member of `group`, or is one no more: that
    /// is to be said now and once again. `false` when nothing is said of
    /// it.
    fn mld_changed(&mut self, group: Ipv6Address, member: bool) -> bool {
        if self.mld.ear.is_none() || !said_of(&group) {
            return false;
        }
        // What was to be said of it before is not true any more.
        self.mld.owed.retain(|owed| owed.group != group);
        self.mld.owed.push(Owed {
            at: self.now,
            group,
            what: if member { JOINED } else { LEFT },
            again: true,
        });
        true
    }

    /// Speak MLD, on a stack that has just started to speak IPv6: the
    /// groups the kernel said before are said now.
    pub(super) fn mld_speak(&mut self) {
        if self.mld.ear.is_some() {
            return;
        }
        let room = self.iface.device().capabilities().ip_mtu();
        let buffer = |packets: usize| {
            RawSocketBuffer::new(
                vec![RawPacketMetadata::EMPTY; packets],
                vec![0u8; packets * room],
            )
        };
        let ear = RawSocket::new(
            IpVersion::Ipv6,
            IpProtocol::HopByHop,
            buffer(HEARD),
            buffer(TO_SAY),
        );
        self.mld.ear = Some(self.set.add(ear));
        let groups: Vec<Ipv6Address> = self.mld.groups.iter().copied().collect();
        for group in groups {
            self.mld_changed(group, true);
        }
        self.mld_addresses();
    }

    /// The card's addresses of IPv6 changed: so may have the
    /// solicited-node groups the machine is a member of.
    pub(super) fn mld_addresses(&mut self) {
        if self.mld.ear.is_none() {
            return;
        }
        let now: BTreeSet<Ipv6Address> = self
            .iface
            .ip_addrs()
            .iter()
            .filter_map(|cidr| match cidr {
                IpCidr::Ipv6(cidr) => Some(solicited_node(&cidr.address())),
                _ => None,
            })
            .collect();
        let was = core::mem::replace(&mut self.mld.solicited, now.clone());
        for group in now.symmetric_difference(&was) {
            // One the kernel asked for too is the machine's either way.
            if !self.mld.groups.contains(group) {
                self.mld_changed(*group, now.contains(group));
            }
        }
    }

    /// Before a poll: say what is due.
    pub(super) fn mld_say(&mut self) {
        let now = self.now;
        let Some(ear) = self.mld.ear else {
            return;
        };
        if !self.mld.owed.iter().any(|owed| owed.at <= now) {
            return;
        }
        let (mut due, later): (Vec<_>, Vec<_>) = core::mem::take(&mut self.mld.owed)
            .into_iter()
            .partition(|owed| owed.at <= now);
        self.mld.owed = later;
        // Each group once, however much of it was due.
        let mut seen = BTreeSet::new();
        due.retain(|owed| seen.insert(owed.group));
        let from = link_local(self.iface.ethernet_addr());
        let mut socket = self.set.get::<RawSocket>(ear);
        let mut left = &due[..];
        while !left.is_empty() {
            let (some, rest) = left.split_at(left.len().min(RECORDS));
            let records: Vec<_> = some.iter().map(|owed| (owed.group, owed.what)).collect();
            let said = report(from, &records);
            match socket.send(said.len()) {
                Ok(room) => room.copy_from_slice(&said),
                Err(_) => break,
            }
            // A change is said once more, counted from when it was said
            // and not from when it was due: a poll that comes late does
            // not make one of the two.
            let again = some.iter().filter(|owed| owed.again).map(|owed| Owed {
                at: now + AGAIN,
                again: false,
                ..*owed
            });
            self.mld.owed.extend(again);
            left = rest;
        }
        // What there was no room for waits for the next poll.
        self.mld.owed.extend_from_slice(left);
    }

    /// After a poll: answer the routers that asked.
    pub(super) fn mld_turn(&mut self) {
        let Some(ear) = self.mld.ear else {
            return;
        };
        loop {
            let asked = match self.set.get::<RawSocket>(ear).recv() {
                Ok(packet) => query(packet),
                Err(_) => break,
            };
            let Some(asked) = asked else {
                continue;
            };
            let all = asked.is_unspecified();
            let groups: Vec<Ipv6Address> = if all {
                let both = self.mld.groups.union(&self.mld.solicited);
                both.copied().filter(said_of).collect()
            } else if self.mld.has(&asked) && said_of(&asked) {
                vec![asked]
            } else {
                Vec::new()
            };
            for group in groups {
                // Once, however many ask in a turn; a change that waits
                // to be said says as much.
                if !self.mld.owed.iter().any(|owed| owed.group == group) {
                    self.mld.owed.push(Owed {
                        at: self.now,
                        group,
                        what: IS_MEMBER,
                        again: false,
                    });
                }
            }
        }
    }

    /// How long until something of MLD is to be said.
    pub(super) fn mld_idle(&self, now: Instant) -> Option<Duration> {
        let next = self.mld.next()?;
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
    use crate::socks::six::tests::{every_frame, group_card, ip6_frame, is_mld, records6};
    use crate::socks::six::tests::{stack, udp6_frame, OURS_LINK, ROUTER};
    use crate::socks::tests::{bind, message};
    use crate::socks::Arrived;
    use smoltcp::wire::{EthernetAddress, EthernetFrame};

    const GROUP: Ipv6Address =
        Ipv6Address([0xff, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x12, 0x34]);
    const OTHER: Ipv6Address =
        Ipv6Address([0xff, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x43, 0x21]);
    const EVERYBODY: Ipv6Address = Ipv6Address::LINK_LOCAL_ALL_NODES;
    /// Where the neighbours ask for our addresses: they all end as the
    /// card does.
    const ASKED_AT: Ipv6Address = Ipv6Address([
        0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, 0x12, 0x34, 0x57,
    ]);

    fn join(group: Ipv6Address) -> Vec<u8> {
        message(OP_JOIN6, 0, &group.0)
    }

    fn leave(group: Ipv6Address) -> Vec<u8> {
        message(OP_LEAVE6, 0, &group.0)
    }

    /// What a report of ours on the wire says: the groups and what goes
    /// with each. It left as a report must.
    fn said(frame: &[u8]) -> Vec<(Ipv6Address, u8)> {
        let eth = EthernetFrame::new_checked(frame).unwrap();
        assert_eq!(eth.src_addr(), OURS);
        assert_eq!(eth.dst_addr(), EthernetAddress([0x33, 0x33, 0, 0, 0, 0x16]));
        let ip = Ipv6Packet::new_checked(eth.payload()).unwrap();
        assert_eq!((ip.src_addr(), ip.dst_addr()), (OURS_LINK, REPORTS_TO));
        assert_eq!(
            (ip.next_header(), ip.hop_limit()),
            (IpProtocol::HopByHop, 1)
        );
        let (alert, message) = ip.payload().split_at(8);
        assert_eq!(alert, ALERT);
        let icmp = Icmpv6Packet::new_checked(message).unwrap();
        assert!(icmp.verify_checksum(&OURS_LINK.into(), &REPORTS_TO.into()));
        assert_eq!(message[..2], [REPORT, 0]);
        let records = usize::from(u16::from_be_bytes([message[6], message[7]]));
        assert_eq!(message.len(), REPORT_HEADER + RECORD_LEN * records);
        let (records, _) = message[REPORT_HEADER..].as_chunks::<RECORD_LEN>();
        records
            .iter()
            .map(|record| {
                // No data of its own and no sources.
                assert_eq!(record[1..4], [0, 0, 0]);
                (Ipv6Address::from_bytes(&record[4..]), record[0])
            })
            .collect()
    }

    /// Poll at `ms`, and what the reports that left say, one after
    /// another.
    fn reports(
        stack: &mut Stack<crate::port::Port>,
        window: &Window,
        ms: i64,
    ) -> Vec<(Ipv6Address, u8)> {
        let sent = every_frame(stack, window, ms);
        let reports = sent.iter().filter(|frame| is_mld(frame));
        reports.flat_map(|frame| said(frame)).collect()
    }

    /// A router asks about `group` (about every one, when it is no
    /// group's address), from `from` to `to`, to travel `hops`.
    fn asks(from: Ipv6Address, to: Ipv6Address, hops: u8, group: Ipv6Address) -> Vec<u8> {
        let mut behind = ALERT.to_vec();
        // An answer within a second; the querier's own numbers.
        behind.extend_from_slice(&[QUERY, 0, 0, 0, 0x03, 0xe8, 0, 0]);
        behind.extend_from_slice(&group.0);
        behind.extend_from_slice(&[2, 125, 0, 0]);
        Icmpv6Packet::new_unchecked(&mut behind[ALERT.len()..])
            .fill_checksum(&from.into(), &to.into());
        ip6_frame(from, to, hops, IpProtocol::HopByHop, &behind)
    }

    /// The datagrams that arrived for the sockets, as IPv6.
    fn arrived(stack: &mut Stack<crate::port::Port>) -> Vec<(u64, Vec<u8>)> {
        let mut m = Vec::new();
        if stack.arrived(&mut m) == Arrived::Nothing {
            return Vec::new();
        }
        let records = records6(&m).into_iter();
        records.map(|(sock, _, _, data)| (sock, data)).collect()
    }

    #[test]
    fn the_messages_about_a_group_of_ipv6_are_these_bytes() {
        // What `linux-object/src/net/user_stack/groups6.rs` writes.
        let mut m = vec![0, 0, 0, 0, 24, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        m.extend_from_slice(&GROUP.0);
        assert_eq!(join(GROUP), m);
        m[4] = 25;
        assert_eq!(leave(GROUP), m);

        let (mut stack, _window) = stack();
        // Not a group's address, and cut short or too long.
        assert_eq!(
            stack.request(&message(OP_JOIN6, 0, &OURS_LINK.0)),
            Err(Refused::Malformed)
        );
        for m in [&join(GROUP)[..31], &[&join(GROUP)[..], &[0]].concat()] {
            assert_eq!(stack.request(m), Err(Refused::Malformed));
        }
        assert_eq!(stack.groups6(), 0);

        // A report, byte by byte: the header of IPv6, the router alert,
        // and one record for each group.
        let packet = report(OURS_LINK, &[(GROUP, JOINED), (OTHER, LEFT)]);
        let mut bytes = vec![0x60, 0, 0, 0, 0, 56, 0, 1];
        bytes.extend_from_slice(&OURS_LINK.0);
        bytes.extend_from_slice(&[0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x16]);
        bytes.extend_from_slice(&[58, 0, 5, 2, 0, 0, 1, 0]);
        bytes.extend_from_slice(&[143, 0, 0, 0, 0, 0, 0, 2]);
        bytes.extend_from_slice(&[4, 0, 0, 0]);
        bytes.extend_from_slice(&GROUP.0);
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes.extend_from_slice(&OTHER.0);
        assert_eq!(packet.len(), bytes.len());
        // All but the sum, which is right.
        assert_eq!(packet[..50], bytes[..50]);
        assert_eq!(packet[52..], bytes[52..]);
        let icmp = Icmpv6Packet::new_checked(&packet[48..]).unwrap();
        assert!(icmp.verify_checksum(&OURS_LINK.into(), &REPORTS_TO.into()));

        // And a query is read back.
        let everything = Ipv6Address::UNSPECIFIED;
        let frame = asks(ROUTER, EVERYBODY, 1, everything);
        assert_eq!(query(&frame[14..]), Some(everything));
        let frame = asks(ROUTER, GROUP, 1, GROUP);
        assert_eq!(query(&frame[14..]), Some(GROUP));
        assert_eq!(query(&packet), None);
    }

    #[test]
    fn a_card_says_the_group_its_neighbours_ask_for_it_at() {
        let (mut stack, window) = stack();
        // With its first frames, and once again a second later.
        assert_eq!(reports(&mut stack, &window, 0), [(ASKED_AT, JOINED)]);
        assert_eq!(stack.idle_for(Instant::from_millis(0)), Some(AGAIN));
        assert_eq!(reports(&mut stack, &window, 999), []);
        assert_eq!(reports(&mut stack, &window, 1000), [(ASKED_AT, JOINED)]);
        assert_eq!(reports(&mut stack, &window, 2000), []);
        assert_eq!(stack.groups6(), 0);

        // A stack whose first poll comes when the clock is far from
        // where it started, as the machine's is: twice all the same.
        let Bench { window, iface, .. } = Bench::new();
        let mut late = Stack::new(iface);
        late.speak_six();
        assert_eq!(reports(&mut late, &window, 3000), [(ASKED_AT, JOINED)]);
        assert_eq!(reports(&mut late, &window, 3999), []);
        assert_eq!(reports(&mut late, &window, 4000), [(ASKED_AT, JOINED)]);
        assert_eq!(reports(&mut late, &window, 5000), []);

        // A stack that does not speak IPv6 says nothing, and keeps what
        // the kernel says for when it does.
        let Bench { window, iface, .. } = Bench::new();
        let mut quiet = Stack::new(iface);
        assert_eq!(quiet.request(&join(GROUP)), Ok(false));
        assert_eq!(reports(&mut quiet, &window, 0), []);
        assert_eq!(quiet.idle_for(Instant::from_millis(0)), None);
        quiet.speak_six();
        assert_eq!(
            reports(&mut quiet, &window, 10),
            [(GROUP, JOINED), (ASKED_AT, JOINED)]
        );
    }

    #[test]
    fn a_group_joined_or_left_is_said_twice() {
        let (mut stack, window) = stack();
        reports(&mut stack, &window, 0);
        reports(&mut stack, &window, 1000);

        assert_eq!(stack.request(&join(GROUP)), Ok(true));
        assert_eq!(stack.groups6(), 1);
        let at = |ms| Instant::from_millis(ms);
        assert_eq!(stack.mld_idle(at(1000)), Some(Duration::ZERO));
        assert_eq!(reports(&mut stack, &window, 1000), [(GROUP, JOINED)]);
        // The machine is a member already.
        assert_eq!(stack.request(&join(GROUP)), Ok(false));
        assert_eq!(reports(&mut stack, &window, 1500), []);
        assert_eq!(stack.mld_idle(at(1500)), Some(Duration::from_millis(500)));
        assert_eq!(reports(&mut stack, &window, 2000), [(GROUP, JOINED)]);
        assert_eq!(reports(&mut stack, &window, 3000), []);
        assert_eq!(stack.mld_idle(at(3000)), None);

        // Two groups in a turn travel in one report; one that is left
        // before it was said again is only said to be left.
        assert_eq!(stack.request(&join(OTHER)), Ok(true));
        assert_eq!(stack.request(&leave(GROUP)), Ok(true));
        assert_eq!(stack.request(&leave(GROUP)), Ok(false));
        let sent = every_frame(&mut stack, &window, 3000);
        assert_eq!(sent.len(), 1);
        assert_eq!(said(&sent[0]), [(OTHER, JOINED), (GROUP, LEFT)]);
        assert_eq!(stack.request(&leave(OTHER)), Ok(true));
        assert_eq!(reports(&mut stack, &window, 3500), [(OTHER, LEFT)]);
        assert_eq!(reports(&mut stack, &window, 4000), [(GROUP, LEFT)]);
        assert_eq!(reports(&mut stack, &window, 4500), [(OTHER, LEFT)]);
        assert_eq!(reports(&mut stack, &window, 9000), []);
        assert_eq!(stack.groups6(), 0);

        // The second time is a second after the first, however late the
        // poll that said the first came: the two are never one.
        assert_eq!(stack.request(&join(GROUP)), Ok(true));
        assert_eq!(reports(&mut stack, &window, 12_000), [(GROUP, JOINED)]);
        assert_eq!(reports(&mut stack, &window, 12_999), []);
        assert_eq!(reports(&mut stack, &window, 13_000), [(GROUP, JOINED)]);
        assert_eq!(reports(&mut stack, &window, 14_000), []);
        assert_eq!(stack.request(&leave(GROUP)), Ok(true));
        reports(&mut stack, &window, 15_000);
        reports(&mut stack, &window, 16_000);

        // Nothing is said of everybody's group, of one that does not
        // leave the machine, nor of the one the neighbours ask at, which
        // is the machine's whether a program asks for it or not.
        let inside = Ipv6Address::new(0xff01, 0, 0, 0, 0, 0, 0, 0x55);
        for group in [EVERYBODY, inside, ASKED_AT] {
            assert_eq!(stack.request(&join(group)), Ok(false));
            assert_eq!(stack.request(&leave(group)), Ok(false));
        }
        assert_eq!(reports(&mut stack, &window, 20_000), []);
    }

    #[test]
    fn what_arrives_for_a_group_is_heard_while_the_machine_is_a_member() {
        let (mut stack, window) = stack();
        reports(&mut stack, &window, 0);
        assert_eq!(stack.request(&bind(7, 5000)), Ok(false));
        let from = (ROUTER, 4000);
        let mut ms = 10;
        let mut hears = |stack: &mut Stack<crate::port::Port>, to: Ipv6Address, data: &[u8]| {
            let frame = udp6_frame(from, (to, 5000), data);
            assert_eq!(
                EthernetFrame::new_checked(&frame[..]).unwrap().dst_addr(),
                {
                    if to.is_multicast() {
                        group_card(to)
                    } else {
                        OURS
                    }
                }
            );
            window.deliver(stack.device(), 0, &frame);
            ms += 10;
            every_frame(stack, &window, ms);
            arrived(stack)
        };
        // The card's own address and everybody's group, always.
        assert_eq!(
            hears(&mut stack, OURS_LINK, b"ours"),
            [(7, b"ours".to_vec())]
        );
        assert_eq!(hears(&mut stack, EVERYBODY, b"all"), [(7, b"all".to_vec())]);
        // A group, from when the kernel says the machine is a member
        // until it says it is one no more.
        assert_eq!(hears(&mut stack, GROUP, b"early"), []);
        assert_eq!(stack.request(&join(GROUP)), Ok(true));
        assert_eq!(
            hears(&mut stack, GROUP, b"member"),
            [(7, b"member".to_vec())]
        );
        assert_eq!(hears(&mut stack, OTHER, b"other"), []);
        assert_eq!(stack.request(&leave(GROUP)), Ok(true));
        assert_eq!(hears(&mut stack, GROUP, b"late"), []);
        // And what is for nobody is not refused: nothing is said to who
        // sends to a group.
        assert_eq!(stack.request(&join(GROUP)), Ok(true));
        let frame = udp6_frame(from, (GROUP, 5001), b"nobody");
        window.deliver(stack.device(), 0, &frame);
        let sent = every_frame(&mut stack, &window, 500);
        assert!(sent.iter().all(|frame| is_mld(frame)), "{sent:?}");
    }

    #[test]
    fn a_router_that_asks_is_told_the_groups() {
        let (mut stack, window) = stack();
        reports(&mut stack, &window, 0);
        reports(&mut stack, &window, 1000);
        assert_eq!(stack.request(&join(GROUP)), Ok(true));
        reports(&mut stack, &window, 2000);
        reports(&mut stack, &window, 3000);
        let everything = Ipv6Address::UNSPECIFIED;

        // About every group: one report with all of them, at the next
        // poll, which is due at once.
        window.deliver(stack.device(), 0, &asks(ROUTER, EVERYBODY, 1, everything));
        assert_eq!(reports(&mut stack, &window, 10_000), []);
        assert_eq!(
            stack.idle_for(Instant::from_millis(10_000)),
            Some(Duration::ZERO)
        );
        let sent = every_frame(&mut stack, &window, 10_000);
        assert_eq!(sent.len(), 1);
        assert_eq!(said(&sent[0]), [(ASKED_AT, IS_MEMBER), (GROUP, IS_MEMBER)]);
        assert_eq!(reports(&mut stack, &window, 11_000), []);
        assert_eq!(stack.mld_idle(Instant::from_millis(11_000)), None);

        // About one, asked at its own address: that one, when the
        // machine is a member.
        for (group, told) in [
            (GROUP, vec![(GROUP, IS_MEMBER)]),
            (ASKED_AT, vec![(ASKED_AT, IS_MEMBER)]),
            (OTHER, vec![]),
        ] {
            window.deliver(stack.device(), 0, &asks(ROUTER, group, 1, group));
            reports(&mut stack, &window, 12_000);
            assert_eq!(reports(&mut stack, &window, 12_000), told, "{group}");
        }

        // Not answered: one that crossed a router, one from an address
        // that is not of the link, one with its sum wrong, and what is
        // no query.
        let far = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 1);
        let mut wrong = asks(ROUTER, EVERYBODY, 1, everything);
        wrong[14 + 40 + 8 + 2] ^= 0x55;
        let mut report = asks(ROUTER, EVERYBODY, 1, everything);
        report[14 + 40 + 8] = REPORT;
        for frame in [
            asks(ROUTER, EVERYBODY, 64, everything),
            asks(far, EVERYBODY, 1, everything),
            wrong,
            report,
        ] {
            window.deliver(stack.device(), 0, &frame);
            assert_eq!(reports(&mut stack, &window, 13_000), []);
            assert_eq!(reports(&mut stack, &window, 13_000), []);
        }

        // A group that is about to be said is not said twice for being
        // asked about.
        assert_eq!(stack.request(&join(OTHER)), Ok(true));
        window.deliver(stack.device(), 0, &asks(ROUTER, OTHER, 1, OTHER));
        assert_eq!(reports(&mut stack, &window, 13_000), [(OTHER, JOINED)]);
        assert_eq!(reports(&mut stack, &window, 13_000), []);
        assert_eq!(reports(&mut stack, &window, 14_000), [(OTHER, JOINED)]);
        assert_eq!(reports(&mut stack, &window, 15_000), []);
    }
}
