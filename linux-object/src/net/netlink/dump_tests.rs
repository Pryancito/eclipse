//! `RTM_*` asked of a machine with cards in it: what `ip link`, `ip addr`,
//! `ip route`, `ip neigh` and `getifaddrs()` are told, checked against what
//! Linux (`net/core/rtnetlink.c`, `net/ipv4/devinet.c`, `net/ipv6/addrconf.c`,
//! `net/ipv4/fib_frontend.c`, `net/core/neighbour.c`) tells them of the
//! same cards.

use super::*;
use crate::net::ipv6_link_local_from_mac;
use crate::net::test_card::{a_machine, plug, Card};
use crate::net::IFF_NOARP;
use alloc::vec;
use zcore_drivers::wire::{EthernetAddress, Ipv6Cidr};

const NLMSG_HDR: usize = size_of::<NetlinkMessageHeader>();
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;

fn request(msg_type: NetlinkMessageType, flags: u16, body: &[u8], attrs: &[u8]) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.push_ext(NetlinkMessageHeader {
        nlmsg_len: 0,
        nlmsg_type: msg_type.into(),
        nlmsg_flags: NetlinkMessageFlags::from_bits_truncate(flags) | NetlinkMessageFlags::REQUEST,
        nlmsg_seq: 42,
        nlmsg_pid: 0,
    });
    msg.extend_from_slice(body);
    msg.align4();
    msg.extend_from_slice(attrs);
    msg.align4();
    msg.set_ext(0, msg.len() as u32);
    msg
}

/// A dump of `msg_type` for `family` (`rtgenmsg`).
fn dump(nl: &NetlinkSocketState, msg_type: NetlinkMessageType, family: u8) -> Vec<Vec<u8>> {
    let req = request(
        msg_type,
        NetlinkMessageFlags::DUMP.bits(),
        &[family, 0, 0, 0],
        &[],
    );
    asked(nl, &req)
}

fn asked(nl: &NetlinkSocketState, req: &[u8]) -> Vec<Vec<u8>> {
    assert_eq!(Socket::write(nl, req, None), Ok(req.len()));
    core::mem::take(&mut *nl.data.lock())
}

fn kind(msg: &[u8]) -> u16 {
    u16::from_ne_bytes([msg[4], msg[5]])
}

fn flags(msg: &[u8]) -> u16 {
    u16::from_ne_bytes([msg[6], msg[7]])
}

fn seq(msg: &[u8]) -> u32 {
    u32::from_ne_bytes([msg[8], msg[9], msg[10], msg[11]])
}

fn u32_at(msg: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([msg[at], msg[at + 1], msg[at + 2], msg[at + 3]])
}

/// The errno an `NLMSG_ERROR` carries (0 for an ACK).
fn errno_of(msg: &[u8]) -> i32 {
    assert_eq!(kind(msg), u16::from(NetlinkMessageType::Error));
    u32_at(msg, NLMSG_HDR) as i32
}

/// The one answer to a request that changes something.
fn acked(nl: &NetlinkSocketState, req: &[u8]) -> i32 {
    let replies = asked(nl, req);
    assert_eq!(replies.len(), 1, "one answer");
    errno_of(&replies[0])
}

/// The attributes of a message whose fixed part is `body` bytes long.
fn attrs(msg: &[u8], body: usize) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    let len = u32_at(msg, 0) as usize;
    assert_eq!(len, msg.len(), "nlmsg_len is the whole message");
    for_each_rtattr(&msg[..len], NLMSG_HDR + body, |t, p| {
        out.push((t, p.to_vec()))
    });
    out
}

fn attr(all: &[(u16, Vec<u8>)], t: u16) -> Option<&[u8]> {
    all.iter().find(|(k, _)| *k == t).map(|(_, p)| p.as_slice())
}

/// Every message of a dump is `NLM_F_MULTI` with the request's sequence,
/// and the last is `NLMSG_DONE` with a zero after its header.
fn check_dump(replies: &[Vec<u8>], each: NetlinkMessageType) -> usize {
    let (done, entries) = replies.split_last().expect("a dump ends with DONE");
    assert_eq!(kind(done), u16::from(NetlinkMessageType::Done));
    assert_eq!(u32_at(done, NLMSG_HDR), 0);
    for msg in replies {
        assert_eq!(seq(msg), 42);
        assert_ne!(flags(msg) & NetlinkMessageFlags::MULTI.bits(), 0);
        assert_eq!(msg.len() % 4, 0, "aligned");
    }
    for msg in entries {
        assert_eq!(kind(msg), u16::from(each));
    }
    entries.len()
}

fn info_of(msg: &[u8]) -> IfaceInfoMsg {
    read_struct(msg, NLMSG_HDR).unwrap()
}

/// `ip link`: one `RTM_NEWLINK` per card, numbered as `SIOCGIFINDEX`
/// numbers them, with its name, its type and its hardware addresses.
#[test]
fn every_card_is_a_link_with_its_name_and_hardware_address() {
    let _m = a_machine();
    let nl = NetlinkSocketState::default();
    let replies = dump(&nl, NetlinkMessageType::GetLink, 0);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewLink), 2);

    let eth = &replies[0];
    let info = info_of(eth);
    assert_eq!((info.ifi_index, info.ifi_type), (1, ARPHRD_ETHER));
    let up = IFF_UP | IFF_BROADCAST | IFF_MULTICAST | IFF_RUNNING | IFF_LOWER_UP;
    assert_eq!(info.ifi_flags & up, up);
    let a = attrs(eth, size_of::<IfaceInfoMsg>());
    assert_eq!(
        attr(&a, RouteAttrTypes::Ifname.into()),
        Some(&b"eth0\0"[..])
    );
    assert_eq!(
        attr(&a, RouteAttrTypes::Address.into()),
        Some(&[0x52, 0x54, 0, 0x12, 0x34, 0x56][..])
    );
    assert_eq!(
        attr(&a, RouteAttrTypes::Broadcast.into()),
        Some(&[0xff; 6][..])
    );

    let lo = &replies[1];
    let info = info_of(lo);
    assert_eq!((info.ifi_index, info.ifi_type), (2, ARPHRD_LOOPBACK));
    assert_ne!(info.ifi_flags & IFF_LOOPBACK, 0);
    assert_eq!(info.ifi_flags & IFF_BROADCAST, 0);
    let a = attrs(lo, size_of::<IfaceInfoMsg>());
    assert_eq!(attr(&a, RouteAttrTypes::Ifname.into()), Some(&b"lo\0"[..]));
    assert_eq!(attr(&a, RouteAttrTypes::Address.into()), Some(&[0; 6][..]));
}

/// The index a link is said with is the one `if_nametoindex` gives.
#[test]
fn a_links_index_is_the_one_the_ioctl_gives() {
    let _m = plug(vec![
        Card::ether("eth0", [2, 0, 0, 0, 0, 1]),
        Card::ether("eth1", [2, 0, 0, 0, 0, 2]),
        Card::loopback(),
    ]);
    let nl = NetlinkSocketState::default();
    let replies = dump(&nl, NetlinkMessageType::GetLink, 0);
    for msg in &replies[..replies.len() - 1] {
        let index = info_of(msg).ifi_index;
        let a = attrs(msg, size_of::<IfaceInfoMsg>());
        let name = attr(&a, RouteAttrTypes::Ifname.into()).unwrap();
        let name = core::str::from_utf8(&name[..name.len() - 1]).unwrap();
        let card = crate::net::iface_by_linux_ifindex(index as u32).unwrap();
        assert_eq!(card.ifname(), name);
    }
}

/// `IFLA_MTU` is the card's own (`rtnl_fill_ifinfo` puts `dev->mtu`), the
/// one `SIOCGIFMTU` gives: 65536 for `lo`, a jumbo card's 9000. `lo` is
/// `<LOOPBACK,UP,LOWER_UP>`: `loopback_setup` gives it no `IFF_NOARP`.
#[test]
fn a_links_mtu_is_the_cards_own_and_lo_has_no_noarp() {
    let _m = plug(vec![
        Card::ether("eth0", [2, 0, 0, 0, 0, 1]).with_mtu(9000),
        Card::loopback(),
    ]);
    let nl = NetlinkSocketState::default();
    let replies = dump(&nl, NetlinkMessageType::GetLink, 0);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewLink), 2);
    let mtu = |msg: &[u8]| {
        let a = attrs(msg, size_of::<IfaceInfoMsg>());
        let v = attr(&a, RouteAttrTypes::MTU.into()).expect("IFLA_MTU");
        u32::from_ne_bytes(core::convert::TryInto::try_into(v).unwrap())
    };
    assert_eq!(mtu(&replies[0]), 9000);
    assert_eq!(mtu(&replies[1]), 65536);
    let lo = info_of(&replies[1]);
    assert_eq!(lo.ifi_flags & IFF_NOARP, 0);
    let want = IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP;
    assert_eq!(lo.ifi_flags & want, want);
}

/// With no card at all, a dump is its `DONE` alone.
#[test]
fn a_machine_with_no_cards_dumps_nothing_but_done() {
    let _m = plug(vec![]);
    let nl = NetlinkSocketState::default();
    for t in [
        NetlinkMessageType::GetLink,
        NetlinkMessageType::GetAddr,
        NetlinkMessageType::GetRoute,
        NetlinkMessageType::GetNeigh,
    ] {
        let replies = dump(&nl, t, 0);
        assert_eq!(replies.len(), 1, "{t:?}");
        assert_eq!(kind(&replies[0]), u16::from(NetlinkMessageType::Done));
    }
}

fn addr_msg(msg: &[u8]) -> IfaceAddrMsg {
    read_struct(msg, NLMSG_HDR).unwrap()
}

/// `ip -4 addr`: each address of IPv4, with its prefix, the card's index,
/// its scope (`host` for the loopback's), and `IFA_LOCAL`, `IFA_ADDRESS`,
/// `IFA_LABEL` and `IFA_BROADCAST` as `devinet.c` fills them.
#[test]
fn the_ipv4_addresses_are_said_as_ip_shows_them() {
    let _m = a_machine();
    let nl = NetlinkSocketState::default();
    let replies = dump(&nl, NetlinkMessageType::GetAddr, AF_INET);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewAddr), 2);

    let eth = &replies[0];
    let m = addr_msg(eth);
    assert_eq!(
        (m.ifa_family, m.ifa_prefixlen, m.ifa_scope, m.ifa_index),
        (AF_INET, 24, 0, 1)
    );
    let a = attrs(eth, size_of::<IfaceAddrMsg>());
    assert_eq!(
        attr(&a, IfAddrAttrTypes::Local.into()),
        Some(&[10, 0, 2, 15][..])
    );
    assert_eq!(
        attr(&a, IfAddrAttrTypes::Address.into()),
        Some(&[10, 0, 2, 15][..])
    );
    assert_eq!(
        attr(&a, IfAddrAttrTypes::Label.into()),
        Some(&b"eth0\0"[..])
    );
    assert_eq!(
        attr(&a, IfAddrAttrTypes::Broadcast.into()),
        Some(&[10, 0, 2, 255][..])
    );

    let lo = addr_msg(&replies[1]);
    assert_eq!(
        (lo.ifa_family, lo.ifa_prefixlen, lo.ifa_scope, lo.ifa_index),
        (AF_INET, 8, 254, 2),
        "RT_SCOPE_HOST"
    );
}

/// `ip -6 addr`: only IPv6, and an Ethernet card has its link-local
/// address (`fe80::` and the EUI-64 of its MAC, `/64`, scope link) even
/// when nobody gave it one, as `addrconf` makes it when the card comes up.
#[test]
fn the_ipv6_dump_has_the_link_local_address_and_nothing_of_ipv4() {
    let m = a_machine();
    let nl = NetlinkSocketState::default();
    let replies = dump(&nl, NetlinkMessageType::GetAddr, AF_INET6);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewAddr), 1);
    let msg = &replies[0];
    let am = addr_msg(msg);
    assert_eq!(
        (am.ifa_family, am.ifa_prefixlen, am.ifa_scope, am.ifa_index),
        (AF_INET6, 64, 253, 1)
    );
    let a = attrs(msg, size_of::<IfaceAddrMsg>());
    let want = ipv6_link_local_from_mac(&EthernetAddress([0x52, 0x54, 0, 0x12, 0x34, 0x56]));
    assert_eq!(
        attr(&a, IfAddrAttrTypes::Address.into()),
        Some(want.as_bytes())
    );
    assert_eq!(
        want.as_bytes()[8..],
        [0x50, 0x54, 0, 0xff, 0xfe, 0x12, 0x34, 0x56]
    );
    let f = attr(&a, IfAddrAttrTypes::Flags.into()).unwrap();
    assert_ne!(
        u32::from_ne_bytes([f[0], f[1], f[2], f[3]]) & 0x80,
        0,
        "IFA_F_PERMANENT"
    );
    assert!(
        attr(&a, IfAddrAttrTypes::Broadcast.into()).is_none(),
        "IPv6 has none"
    );
    // It was given to the card, not made up for the dump.
    assert!(m
        .card("eth0")
        .told
        .lock()
        .addresses
        .contains(&IpCidr::Ipv6(Ipv6Cidr::new(want, 64))));

    // And an unfiltered dump has both families.
    let all = dump(&nl, NetlinkMessageType::GetAddr, 0);
    assert_eq!(check_dump(&all, NetlinkMessageType::NewAddr), 3);
}

/// `::1` is of the host and a global address is of the universe.
#[test]
fn an_ipv6_address_has_the_scope_of_its_kind() {
    let global = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 5);
    let _m = plug(vec![Card::loopback()
        .with_address(IpCidr::new(IpAddress::Ipv6(Ipv6Address::LOOPBACK), 128))
        .with_address(IpCidr::new(IpAddress::Ipv6(global), 64))]);
    let nl = NetlinkSocketState::default();
    let replies = dump(&nl, NetlinkMessageType::GetAddr, AF_INET6);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewAddr), 2);
    let scopes: Vec<(u8, u8)> = replies[..2]
        .iter()
        .map(|m| (addr_msg(m).ifa_prefixlen, addr_msg(m).ifa_scope))
        .collect();
    assert_eq!(scopes, [(128, 254), (64, 0)]);
}

/// A free slot of a driver (`0.0.0.0/0`) and the placeholder before DHCP
/// (`240.0.0.0`) are not addresses, and `ip addr flush` must not be shown
/// them, or it deletes them for ever.
#[test]
fn a_free_slot_and_the_placeholder_are_not_addresses() {
    let _m = plug(vec![Card::ether("eth0", [2, 0, 0, 0, 0, 1])
        .with_address(IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0))
        .with_address(IpCidr::new(IpAddress::v4(240, 0, 0, 0), 8))
        .with_address(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24))]);
    let nl = NetlinkSocketState::default();
    let replies = dump(&nl, NetlinkMessageType::GetAddr, AF_INET);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewAddr), 1);
    let a = attrs(&replies[0], size_of::<IfaceAddrMsg>());
    assert_eq!(
        attr(&a, IfAddrAttrTypes::Local.into()),
        Some(&[10, 0, 2, 15][..])
    );
}

fn ifaddr(ifindex: u32, family: u8, prefix: u8, addr: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut body = Vec::new();
    body.push_ext(IfaceAddrMsg {
        ifa_family: family,
        ifa_prefixlen: prefix,
        ifa_flags: 0,
        ifa_scope: 0,
        ifa_index: ifindex,
    });
    let mut a = Vec::new();
    push_rtattr_bytes(&mut a, IfAddrAttrTypes::Local.into(), addr);
    (body, a)
}

/// `ip addr add 192.168.7.9/16 dev eth0` and `ip addr del` of it: the card
/// has it and then has not, each answered with an ACK of 0 when one is
/// asked; adding it twice is `EEXIST`.
#[test]
fn an_address_is_added_to_a_card_and_taken_away() {
    let m = a_machine();
    let nl = NetlinkSocketState::default();
    let (body, a) = ifaddr(1, AF_INET, 16, &[192, 168, 7, 9]);
    let ack = NetlinkMessageFlags::ACK.bits();
    let add = request(NetlinkMessageType::NewAddr, ack, &body, &a);
    assert_eq!(acked(&nl, &add), 0);
    let cidr = IpCidr::new(IpAddress::v4(192, 168, 7, 9), 16);
    assert!(m.card("eth0").told.lock().addresses.contains(&cidr));
    assert_eq!(acked(&nl, &add), -(errno::EEXIST));

    let del = request(NetlinkMessageType::DelAddr, ack, &body, &a);
    assert_eq!(acked(&nl, &del), 0);
    assert!(!m.card("eth0").told.lock().addresses.contains(&cidr));
    // A success with no ACK asked is answered with nothing.
    let quiet = request(NetlinkMessageType::NewAddr, 0, &body, &a);
    assert!(asked(&nl, &quiet).is_empty());
}

/// `ip addr flush dev eth0` sends one `RTM_DELADDR` per address in one
/// write, and every one of them is done.
#[test]
fn a_batch_of_deletes_in_one_write_deletes_them_all() {
    let m = plug(vec![Card::ether("eth0", [2, 0, 0, 0, 0, 1])
        .with_address(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
        .with_address(IpCidr::new(IpAddress::v4(10, 0, 1, 1), 24))]);
    let nl = NetlinkSocketState::default();
    let ack = NetlinkMessageFlags::ACK.bits();
    let mut batch = Vec::new();
    for addr in [[10, 0, 0, 1], [10, 0, 1, 1]] {
        let (body, a) = ifaddr(1, AF_INET, 24, &addr);
        batch.extend(request(NetlinkMessageType::DelAddr, ack, &body, &a));
    }
    let replies = asked(&nl, &batch);
    assert_eq!(
        replies.iter().map(|r| errno_of(r)).collect::<Vec<_>>(),
        [0, 0]
    );
    assert!(m.card("eth0").told.lock().addresses.is_empty());
}

/// Deleting an address the card has not is the driver's refusal, said as
/// an errno and not as a success.
#[test]
fn deleting_an_address_the_card_has_not_is_refused() {
    let _m = a_machine();
    let nl = NetlinkSocketState::default();
    let (body, a) = ifaddr(1, AF_INET, 24, &[10, 9, 9, 9]);
    let del = request(NetlinkMessageType::DelAddr, 0, &body, &a);
    assert_eq!(acked(&nl, &del), -(errno::EINVAL));
    let (body, a) = ifaddr(7, AF_INET, 24, &[10, 9, 9, 9]);
    let del = request(NetlinkMessageType::DelAddr, 0, &body, &a);
    assert_eq!(acked(&nl, &del), -(errno::ENODEV));
}

/// An opener without `CAP_NET_ADMIN` may look (`rtnetlink_rcv_msg` lets
/// every `GET` through) and is told `EPERM` for anything else, which
/// changes nothing.
#[test]
fn without_net_admin_a_socket_may_look_but_not_change() {
    let m = a_machine();
    let nl = NetlinkSocketState::new_of(SocketType::SOCK_RAW, false);
    let replies = dump(&nl, NetlinkMessageType::GetLink, 0);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewLink), 2);
    let (body, a) = ifaddr(1, AF_INET, 16, &[192, 168, 7, 9]);
    let add = request(NetlinkMessageType::NewAddr, 0, &body, &a);
    assert_eq!(acked(&nl, &add), -(errno::EPERM));
    assert_eq!(m.card("eth0").told.lock().addresses.len(), 1);
}

fn rtmsg(family: u8, dst_len: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.push_ext(RouteMsg {
        rtm_family: family,
        rtm_dst_len: dst_len,
        rtm_src_len: 0,
        rtm_tos: 0,
        rtm_table: 254,
        rtm_protocol: 3,
        rtm_scope: 0,
        rtm_type: 1,
        rtm_flags: 0,
    });
    body
}

fn route_attrs(dst: Option<&[u8]>, gw: Option<&[u8]>, oif: u32) -> Vec<u8> {
    let mut a = Vec::new();
    if let Some(d) = dst {
        push_rtattr_bytes(&mut a, RTA_DST, d);
    }
    if let Some(g) = gw {
        push_rtattr_bytes(&mut a, RTA_GATEWAY, g);
    }
    if oif != 0 {
        push_rtattr_u32(&mut a, RTA_OIF, oif);
    }
    a
}

fn route_msg(msg: &[u8]) -> RouteMsg {
    read_struct(msg, NLMSG_HDR).unwrap()
}

/// `ip route`: the default route is said with no `RTA_DST` (that is what
/// makes `ip` print `default`), as a unicast route of the main table of
/// universe scope, with its gateway and its card.
#[test]
fn the_default_route_is_said_as_default_via_its_gateway() {
    let _m = a_machine();
    let nl = NetlinkSocketState::default();
    let replies = dump(&nl, NetlinkMessageType::GetRoute, AF_INET);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewRoute), 1);
    let msg = &replies[0];
    let r = route_msg(msg);
    assert_eq!(
        (
            r.rtm_family,
            r.rtm_dst_len,
            r.rtm_table,
            r.rtm_type,
            r.rtm_scope,
            r.rtm_flags
        ),
        (AF_INET, 0, 254, 1, 0, 0)
    );
    let a = attrs(msg, size_of::<RouteMsg>());
    assert!(attr(&a, RTA_DST).is_none());
    assert_eq!(attr(&a, RTA_GATEWAY), Some(&[10, 0, 2, 2][..]));
    assert_eq!(attr(&a, RTA_OIF), Some(&1u32.to_ne_bytes()[..]));
}

/// A route on the link (no gateway) is of link scope and says where it
/// goes.
#[test]
fn a_route_on_the_link_has_link_scope_and_its_destination() {
    let _m = plug(vec![Card::ether("eth0", [2, 0, 0, 0, 0, 1])
        .with_route(IpCidr::new(IpAddress::v4(10, 0, 2, 0), 24), None)]);
    let nl = NetlinkSocketState::default();
    let replies = dump(&nl, NetlinkMessageType::GetRoute, 0);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewRoute), 1);
    let r = route_msg(&replies[0]);
    assert_eq!((r.rtm_dst_len, r.rtm_scope), (24, 253));
    let a = attrs(&replies[0], size_of::<RouteMsg>());
    assert_eq!(attr(&a, RTA_DST), Some(&[10, 0, 2, 0][..]));
    assert!(attr(&a, RTA_GATEWAY).is_none());
}

/// `ip route add 192.168.50.0/24 via 10.0.2.1 dev eth0`, and the same
/// without `dev`: the card is the one whose network has the gateway.
#[test]
fn a_route_is_added_on_its_card_or_on_the_gateways() {
    let m = a_machine();
    let nl = NetlinkSocketState::default();
    let dst = IpCidr::new(IpAddress::v4(192, 168, 50, 0), 24);
    let gw = IpAddress::v4(10, 0, 2, 1);
    let add = request(
        NetlinkMessageType::NewRoute,
        0,
        &rtmsg(AF_INET, 24),
        &route_attrs(Some(&[192, 168, 50, 0]), Some(&[10, 0, 2, 1]), 1),
    );
    assert!(asked(&nl, &add).is_empty(), "a success with no ACK asked");
    assert!(m
        .card("eth0")
        .told
        .lock()
        .routes
        .iter()
        .any(|r| r.dst == dst && r.gateway == Some(gw)));

    let other = IpCidr::new(IpAddress::v4(192, 168, 60, 0), 24);
    let add = request(
        NetlinkMessageType::NewRoute,
        NetlinkMessageFlags::ACK.bits(),
        &rtmsg(AF_INET, 24),
        &route_attrs(Some(&[192, 168, 60, 0]), Some(&[10, 0, 2, 1]), 0),
    );
    assert_eq!(acked(&nl, &add), 0);
    assert!(m
        .card("eth0")
        .told
        .lock()
        .routes
        .iter()
        .any(|r| r.dst == other));

    // `ip route del 192.168.60.0/24`: no card named, found by who has it.
    let del = request(
        NetlinkMessageType::DelRoute,
        NetlinkMessageFlags::ACK.bits(),
        &rtmsg(AF_INET, 24),
        &route_attrs(Some(&[192, 168, 60, 0]), None, 0),
    );
    assert_eq!(acked(&nl, &del), 0);
    assert!(!m
        .card("eth0")
        .told
        .lock()
        .routes
        .iter()
        .any(|r| r.dst == other));
}

/// What `fib_frontend.c` refuses: a card that is not there (`ENODEV`), a
/// gateway of a family no card has (`ENETUNREACH`), a route with neither a card
/// nor a gateway (`EINVAL`), and a second default route on the link with
/// `NLM_F_EXCL` (`EEXIST`).
#[test]
fn the_routes_linux_refuses() {
    let _m = a_machine();
    let nl = NetlinkSocketState::default();
    let on = |a: Vec<u8>, flags: u16| {
        request(NetlinkMessageType::NewRoute, flags, &rtmsg(AF_INET, 24), &a)
    };
    let dst = [192, 168, 50, 0];
    assert_eq!(
        acked(&nl, &on(route_attrs(Some(&dst), None, 9), 0)),
        -(errno::ENODEV)
    );
    // A gateway of a family no card has an address of.
    let six = request(
        NetlinkMessageType::NewRoute,
        0,
        &rtmsg(AF_INET6, 64),
        &route_attrs(
            Some(&[0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some(&[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            0,
        ),
    );
    assert_eq!(acked(&nl, &six), -(errno::ENETUNREACH));
    assert_eq!(
        acked(&nl, &on(route_attrs(Some(&dst), None, 0), 0)),
        -(errno::EINVAL)
    );
    let excl = NetlinkMessageFlags::EXCL.bits() | NetlinkMessageFlags::CREATE.bits();
    let default = request(
        NetlinkMessageType::NewRoute,
        excl,
        &rtmsg(AF_INET, 0),
        &route_attrs(None, None, 1),
    );
    assert_eq!(acked(&nl, &default), -(errno::EEXIST));
    // A route to delete on a card that is not there.
    let del = request(
        NetlinkMessageType::DelRoute,
        0,
        &rtmsg(AF_INET, 24),
        &route_attrs(Some(&dst), None, 9),
    );
    assert_eq!(acked(&nl, &del), -(errno::ENODEV));
}

fn ndmsg(family: u8, ifindex: i32, state: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.push_ext(NeighMsg {
        ndm_family: family,
        ndm_pad1: 0,
        ndm_pad2: 0,
        ndm_ifindex: ifindex,
        ndm_state: state,
        ndm_flags: 0,
        ndm_type: 0,
    });
    body
}

/// `ip neigh add 10.0.2.7 lladdr 02:00:00:00:00:07 dev eth0 nud permanent`,
/// then `ip neigh`, then `ip neigh del`.
#[test]
fn a_neighbour_is_added_dumped_and_deleted() {
    let m = a_machine();
    let nl = NetlinkSocketState::default();
    let mut a = Vec::new();
    push_rtattr_bytes(&mut a, NDA_DST, &[10, 0, 2, 7]);
    push_rtattr_bytes(&mut a, NDA_LLADDR, &[2, 0, 0, 0, 0, 7]);
    let create = NetlinkMessageFlags::CREATE.bits() | NetlinkMessageFlags::EXCL.bits();
    let add = request(
        NetlinkMessageType::NewNeigh,
        create,
        &ndmsg(AF_INET, 1, NUD_PERMANENT),
        &a,
    );
    assert!(asked(&nl, &add).is_empty());
    assert_eq!(m.card("eth0").told.lock().neighbours.len(), 1);
    assert_eq!(
        acked(&nl, &add),
        -(errno::EEXIST),
        "NLM_F_EXCL and it is there"
    );

    let replies = dump(&nl, NetlinkMessageType::GetNeigh, AF_INET);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewNeigh), 1);
    let n: NeighMsg = read_struct(&replies[0], NLMSG_HDR).unwrap();
    assert_eq!(
        (n.ndm_family, n.ndm_ifindex, n.ndm_state),
        (AF_INET, 1, NUD_PERMANENT)
    );
    let got = attrs(&replies[0], size_of::<NeighMsg>());
    assert_eq!(attr(&got, NDA_DST), Some(&[10, 0, 2, 7][..]));
    assert_eq!(attr(&got, NDA_LLADDR), Some(&[2, 0, 0, 0, 0, 7][..]));
    // Asked for IPv6 only, there is none.
    let six = dump(&nl, NetlinkMessageType::GetNeigh, AF_INET6);
    assert_eq!(check_dump(&six, NetlinkMessageType::NewNeigh), 0);

    let del = request(NetlinkMessageType::DelNeigh, 0, &ndmsg(AF_INET, 1, 0), &a);
    assert!(asked(&nl, &del).is_empty());
    assert!(m.card("eth0").told.lock().neighbours.is_empty());
    assert_eq!(acked(&nl, &del), -(errno::ENOENT));
    let gone = request(
        NetlinkMessageType::NewNeigh,
        create,
        &ndmsg(AF_INET, 9, NUD_PERMANENT),
        &a,
    );
    assert_eq!(acked(&nl, &gone), -(errno::ENODEV));
}

/// A request the kernel does not know is `EOPNOTSUPP`, said even without
/// an ACK asked; `NLMSG_NOOP` is one.
#[test]
fn a_request_it_does_not_know_is_eopnotsupp() {
    let nl = NetlinkSocketState::default();
    let req = request(NetlinkMessageType::Noop, 0, &[0; 4], &[]);
    assert_eq!(acked(&nl, &req), -(errno::EOPNOTSUPP));
}

/// A write shorter than a header, or whose first message says it is
/// longer than the write, is `EINVAL`; a bad message after a good one
/// ends the batch and keeps what was done.
#[test]
fn a_write_that_is_not_a_message_is_einval() {
    let _m = a_machine();
    let nl = NetlinkSocketState::default();
    assert_eq!(Socket::write(&nl, &[0; 8], None), Err(LxError::EINVAL));
    let mut req = request(NetlinkMessageType::GetLink, 0, &[0; 4], &[]);
    req.set_ext(0, 200u32);
    assert_eq!(Socket::write(&nl, &req, None), Err(LxError::EINVAL));
    let mut batch = request(
        NetlinkMessageType::GetLink,
        NetlinkMessageFlags::DUMP.bits(),
        &[0; 4],
        &[],
    );
    let mut bad = request(NetlinkMessageType::GetLink, 0, &[0; 4], &[]);
    bad.set_ext(0, 8u32);
    batch.extend(bad);
    let replies = asked(&nl, &batch);
    assert_eq!(check_dump(&replies, NetlinkMessageType::NewLink), 2);
}

/// What a dump reads as: one message per `recv`, in order, and the
/// socket says it is readable until they are all read.
#[async_std::test]
async fn a_dump_is_read_one_message_at_a_time() {
    let _m = a_machine();
    let nl = NetlinkSocketState::default();
    let req = request(
        NetlinkMessageType::GetLink,
        NetlinkMessageFlags::DUMP.bits(),
        &[0; 4],
        &[],
    );
    assert_eq!(Socket::write(&nl, &req, None), Ok(req.len()));
    let mut kinds = Vec::new();
    nl.set_flags(OpenFlags::RDWR | OpenFlags::NON_BLOCK)
        .unwrap();
    loop {
        let mut buf = [0u8; 1024];
        match Socket::read(&nl, &mut buf).await.0 {
            Ok(n) => {
                assert_eq!(u32_at(&buf, 0) as usize, n, "a whole message");
                kinds.push(kind(&buf));
            }
            Err(e) => {
                assert_eq!(e, LxError::EAGAIN);
                break;
            }
        }
    }
    let link: u16 = NetlinkMessageType::NewLink.into();
    let done: u16 = NetlinkMessageType::Done.into();
    assert_eq!(kinds, [link, link, done]);
}
