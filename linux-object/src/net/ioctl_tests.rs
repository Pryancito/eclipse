//! The interface requests (`SIOC*`) asked of a machine with cards in it,
//! each answer checked against what Linux (`net/core/dev_ioctl.c`,
//! `net/ipv4/devinet.c`, `net/ipv4/arp.c`, `net/ipv4/fib_frontend.c`)
//! gives for the same card.

use super::test_card::{a_machine, plug, Card};
use super::*;
use alloc::vec;
use zcore_drivers::wire::{IpAddress, IpCidr, Ipv6Address};

fn ifreq(name: &str) -> IfReq {
    #[allow(unsafe_code)]
    let mut ifr: IfReq = unsafe { core::mem::zeroed() };
    ifr.ifr_name[..name.len()].copy_from_slice(name.as_bytes());
    ifr
}

fn ask(request: usize, ifr: &mut IfReq) -> LxResult<usize> {
    handle_net_ioctl(request, ifr as *mut IfReq as usize, 0, 0, false)
}

fn sin(addr: [u8; 4]) -> SockAddrIn {
    ipv4_sockaddr(Ipv4Address(addr))
}

#[allow(unsafe_code)]
fn addr_of(ifr: &IfReq) -> (u16, [u8; 4]) {
    let a = unsafe { ifr.ifr_ifru.addr };
    (a.sin_family, a.sin_addr.to_ne_bytes())
}

const AF_INET: u16 = 2;

/// An address of the kernel's half, which no caller may name.
const KERNEL: usize = 0xffff_8000_0000_0000;

/// The user-half bound applied to this thread, as it is on the machine,
/// for as long as the guard lives.
struct Strict;

impl Strict {
    fn on() -> Self {
        kernel_hal::user::set_strict_user_half(true);
        Self
    }
}

impl Drop for Strict {
    fn drop(&mut self) {
        kernel_hal::user::set_strict_user_half(false);
    }
}

#[test]
fn an_interface_is_numbered_from_one_in_the_order_of_the_machine() {
    let _m = a_machine();
    // Linux numbers `lo` 1; this machine puts its cards first, so that the
    // first is the first Ethernet one, and numbers in that order.
    for (name, index) in [("eth0", 1), ("lo", 2)] {
        let mut ifr = ifreq(name);
        assert_eq!(ask(SIOCGIFINDEX, &mut ifr), Ok(0));
        #[allow(unsafe_code)]
        let got = unsafe { ifr.ifr_ifru.ifindex };
        assert_eq!(got, index, "{name}");
        assert_eq!(iface_by_linux_ifindex(index as u32).unwrap().ifname(), name);
    }
    assert_eq!(iface_by_linux_ifindex(0).err(), Some(LxError::ENODEV));
    assert_eq!(iface_by_linux_ifindex(3).err(), Some(LxError::ENODEV));
    assert_eq!(link_scope_index(), 1, "the first card that is not lo");
}

/// `dev_ifsioc_locked`: a name no card has is `ENODEV`, for every request
/// that names one.
#[test]
fn a_card_the_machine_has_not_is_enodev() {
    let _m = a_machine();
    for request in [
        SIOCGIFINDEX,
        SIOCGIFFLAGS,
        SIOCSIFFLAGS,
        SIOCGIFADDR,
        SIOCSIFADDR,
        SIOCGIFBRDADDR,
        SIOCGIFDSTADDR,
        SIOCGIFNETMASK,
        SIOCSIFNETMASK,
        SIOCGIFHWADDR,
        SIOCGIFMAP,
        SIOCGIFMTU,
        SIOCGIFTXQLEN,
        SIOCGIFMETRIC,
    ] {
        let mut ifr = ifreq("wlan9");
        assert_eq!(ask(request, &mut ifr), Err(LxError::ENODEV), "{request:#x}");
    }
}

/// A name that is not UTF-8 names no card, and is not taken for one.
#[test]
fn a_name_that_is_not_text_is_einval() {
    let _m = a_machine();
    let mut ifr = ifreq("");
    ifr.ifr_name[..2].copy_from_slice(&[0xff, 0xfe]);
    assert_eq!(ask(SIOCGIFFLAGS, &mut ifr), Err(LxError::EINVAL));
    // Sixteen bytes with no NUL are read as the whole sixteen.
    let full = [b'x'; 16];
    assert_eq!(ifreq_name(&full), Ok("xxxxxxxxxxxxxxxx"));
}

/// `ifconfig` prints `flags=4163<UP,BROADCAST,RUNNING,MULTICAST>` for an
/// Ethernet card that is up, and the bit past sixteen (`IFF_LOWER_UP`)
/// does not fit `ifr_flags`, a `short`.
#[test]
fn an_ethernet_card_that_is_up_says_so() {
    let _m = a_machine();
    let mut ifr = ifreq("eth0");
    assert_eq!(ask(SIOCGIFFLAGS, &mut ifr), Ok(0));
    #[allow(unsafe_code)]
    let flags = unsafe { ifr.ifr_ifru.flags } as u16 as u32;
    assert_eq!(flags, IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST);
    assert_eq!(flags, 4163);

    let mut ifr = ifreq("lo");
    assert_eq!(ask(SIOCGIFFLAGS, &mut ifr), Ok(0));
    #[allow(unsafe_code)]
    let flags = unsafe { ifr.ifr_ifru.flags } as u16 as u32;
    assert_eq!(
        flags & (IFF_UP | IFF_LOOPBACK | IFF_RUNNING),
        IFF_UP | IFF_LOOPBACK | IFF_RUNNING
    );
    assert_eq!(flags & IFF_BROADCAST, 0, "lo has no broadcast");
}

/// `ifconfig eth0 promisc up`, then `ifconfig eth0 down`: the card is told
/// each of them.
#[test]
fn the_flags_that_are_set_reach_the_card() {
    let m = a_machine();
    let mut ifr = ifreq("eth0");
    ifr.ifr_ifru.flags = (IFF_UP | IFF_PROMISC | IFF_ALLMULTI) as i16;
    assert_eq!(ask(SIOCSIFFLAGS, &mut ifr), Ok(0));
    {
        let told = m.card("eth0").told.lock();
        assert!(told.promiscuous && told.allmulti);
        assert_eq!((told.ups, told.downs), (1, 0));
    }
    ifr.ifr_ifru.flags = 0;
    assert_eq!(ask(SIOCSIFFLAGS, &mut ifr), Ok(0));
    let told = m.card("eth0").told.lock();
    assert!(!told.promiscuous && !told.allmulti);
    assert_eq!((told.ups, told.downs), (1, 1));
}

/// `ifconfig eth0`: the address, its broadcast and its mask, each a
/// `sockaddr_in` of `AF_INET`.
#[test]
fn the_address_broadcast_and_mask_are_the_cards() {
    let _m = a_machine();
    for (request, want) in [
        (SIOCGIFADDR, [10, 0, 2, 15]),
        (SIOCGIFBRDADDR, [10, 0, 2, 255]),
        (SIOCGIFNETMASK, [255, 255, 255, 0]),
    ] {
        let mut ifr = ifreq("eth0");
        assert_eq!(ask(request, &mut ifr), Ok(0));
        assert_eq!(addr_of(&ifr), (AF_INET, want), "{request:#x}");
    }
    let mut ifr = ifreq("lo");
    assert_eq!(ask(SIOCGIFNETMASK, &mut ifr), Ok(0));
    assert_eq!(addr_of(&ifr), (AF_INET, [255, 0, 0, 0]));
}

/// `devinet_ioctl`: a card that is not point-to-point answers its own
/// address for the peer (`ifa_address == ifa_local`), and a card with no
/// IPv4 address has none of the four to give: `EADDRNOTAVAIL`, not
/// 0.0.0.0, and the caller's structure is left as it was.
#[test]
fn a_card_with_no_ipv4_address_has_none_to_give() {
    let _m = plug(vec![
        Card::ether("eth0", [2, 0, 0, 0, 0, 1])
            .with_address(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)),
        Card::ether("eth1", [2, 0, 0, 0, 0, 2]),
    ]);
    let mut ifr = ifreq("eth0");
    assert_eq!(ask(SIOCGIFDSTADDR, &mut ifr), Ok(0));
    assert_eq!(addr_of(&ifr), (AF_INET, [10, 0, 2, 15]));
    for request in [SIOCGIFADDR, SIOCGIFBRDADDR, SIOCGIFNETMASK, SIOCGIFDSTADDR] {
        let mut ifr = ifreq("eth1");
        ifr.ifr_ifru.addr = sin([9, 9, 9, 9]);
        assert_eq!(
            ask(request, &mut ifr),
            Err(LxError::EADDRNOTAVAIL),
            "{request:#x}"
        );
        assert_eq!(addr_of(&ifr).1, [9, 9, 9, 9], "{request:#x}");
    }
    // The card is there all the same.
    let mut ifr = ifreq("eth1");
    assert_eq!(ask(SIOCGIFFLAGS, &mut ifr), Ok(0));
}

/// `SIOCGIFCONF` lists IPv4 addresses (`inet_gifconf`), so a card with
/// none is not in it.
#[test]
fn the_list_leaves_out_a_card_with_no_ipv4_address() {
    let _m = plug(vec![
        Card::loopback(),
        Card::ether("eth0", [2, 0, 0, 0, 0, 1]),
        Card::ether("eth1", [2, 0, 0, 0, 0, 2])
            .with_address(IpCidr::new(IpAddress::v4(192, 168, 1, 5), 24)),
    ]);
    let mut list = [ifreq(""), ifreq(""), ifreq("")];
    let mut ifc = ifconf(core::mem::size_of_val(&list) as i32, &mut list);
    assert_eq!(ask_conf(&mut ifc), Ok(0));
    assert_eq!(ifc.ifc_len as usize, 2 * size_of::<IfReq>());
    assert_eq!(list[0].name(), "eth1");
    assert_eq!(addr_of(&list[0]), (AF_INET, [192, 168, 1, 5]));
    assert_eq!(list[1].name(), "lo");
    assert_eq!(list[2].name(), "");
}

/// `ifconfig eth0 192.168.7.9 netmask 255.255.0.0`: two requests, the
/// address and then the mask, and the card has both.
#[test]
fn an_address_and_then_a_mask_are_set_on_the_card() {
    let m = a_machine();
    let mut ifr = ifreq("eth0");
    ifr.ifr_ifru.addr = sin([192, 168, 7, 9]);
    assert_eq!(ask(SIOCSIFADDR, &mut ifr), Ok(0));
    ifr.ifr_ifru.addr = sin([255, 255, 0, 0]);
    assert_eq!(ask(SIOCSIFNETMASK, &mut ifr), Ok(0));
    assert_eq!(
        iface_ipv4_cidr(m.card("eth0")),
        Some(Ipv4Cidr::new(Ipv4Address([192, 168, 7, 9]), 16))
    );
    let mut ifr = ifreq("eth0");
    assert_eq!(ask(SIOCGIFBRDADDR, &mut ifr), Ok(0));
    assert_eq!(addr_of(&ifr).1, [192, 168, 255, 255]);
    // The broadcast follows from the two, and setting it is taken.
    ifr.ifr_ifru.addr = sin([192, 168, 255, 255]);
    assert_eq!(ask(SIOCSIFBRDADDR, &mut ifr), Ok(0));
}

/// `devinet_ioctl`: a mask that is not ones and then zeros is `EINVAL`, and
/// the card keeps the one it had.
#[test]
fn a_mask_with_a_hole_in_it_is_einval() {
    let m = a_machine();
    let mut ifr = ifreq("eth0");
    ifr.ifr_ifru.addr = sin([255, 0, 255, 0]);
    assert_eq!(ask(SIOCSIFNETMASK, &mut ifr), Err(LxError::EINVAL));
    assert_eq!(iface_ipv4_cidr(m.card("eth0")).unwrap().prefix_len(), 24);
    assert_eq!(prefix_len_from_netmask(Ipv4Address([0, 0, 0, 0])), Ok(0));
    assert_eq!(prefix_len_from_netmask(Ipv4Address([255; 4])), Ok(32));
    assert_eq!(
        prefix_len_from_netmask(Ipv4Address([255, 255, 255, 254])),
        Ok(31)
    );
    assert_eq!(
        prefix_len_from_netmask(Ipv4Address([0, 255, 255, 255])),
        Err(LxError::EINVAL)
    );
}

/// A driver that cannot be given an address says so, and so does the
/// request: the machine is not told the card has one when it has not.
#[test]
fn a_card_that_takes_no_address_is_einval() {
    let _m = plug(vec![Card::ether("eth0", [2, 0, 0, 0, 0, 1]).refusing()]);
    let mut ifr = ifreq("eth0");
    ifr.ifr_ifru.addr = sin([10, 1, 1, 1]);
    assert_eq!(ask(SIOCSIFADDR, &mut ifr), Err(LxError::EINVAL));
    ifr.ifr_ifru.addr = sin([255, 255, 255, 0]);
    assert_eq!(ask(SIOCSIFNETMASK, &mut ifr), Err(LxError::EINVAL));
}

/// `ifconfig` shows `ether 52:54:00:12:34:56` and `loop`: `ARPHRD_ETHER`
/// with the card's address, and `ARPHRD_LOOPBACK` with none.
#[test]
fn the_hardware_address_and_its_family() {
    let _m = a_machine();
    let mut ifr = ifreq("eth0");
    assert_eq!(ask(SIOCGIFHWADDR, &mut ifr), Ok(0));
    #[allow(unsafe_code)]
    let hw = unsafe { ifr.ifr_ifru.hwaddr };
    assert_eq!(hw.sa_family, ARPHRD_ETHER);
    assert_eq!(hw.sa_data[..6], [0x52, 0x54, 0, 0x12, 0x34, 0x56]);

    let mut ifr = ifreq("lo");
    assert_eq!(ask(SIOCGIFHWADDR, &mut ifr), Ok(0));
    #[allow(unsafe_code)]
    let hw = unsafe { ifr.ifr_ifru.hwaddr };
    assert_eq!(hw.sa_family, ARPHRD_LOOPBACK);
    assert_eq!(hw.sa_data[..6], [0; 6]);
}

/// `mtu 1500`, `mtu 65536` for `lo`, `txqueuelen 1000`, and a map of zeros
/// for a card with no ISA resources.
#[test]
fn the_mtu_the_queue_and_the_map() {
    let _m = plug(vec![
        Card::loopback(),
        Card::ether("eth0", [2, 0, 0, 0, 0, 1]).with_mtu(9000),
    ]);
    for (name, mtu) in [("lo", 65536), ("eth0", 9000)] {
        let mut ifr = ifreq(name);
        assert_eq!(ask(SIOCGIFMTU, &mut ifr), Ok(0));
        #[allow(unsafe_code)]
        let got = unsafe { ifr.ifr_ifru.ifmtu };
        assert_eq!(got, mtu, "{name}");
    }
    let mut ifr = ifreq("eth0");
    assert_eq!(ask(SIOCGIFTXQLEN, &mut ifr), Ok(0));
    #[allow(unsafe_code)]
    let qlen = unsafe { ifr.ifr_ifru.ifqlen };
    assert_eq!(qlen, 1000);

    let mut ifr = ifreq("eth0");
    ifr.ifr_ifru.ifru_pad = [u64::MAX; 3];
    assert_eq!(ask(SIOCGIFMAP, &mut ifr), Ok(0));
    #[allow(unsafe_code)]
    let map = unsafe { ifr.ifr_ifru.ifru_pad };
    assert_eq!(map, [0; 3]);
}

/// A request that only changes the machine leaves the caller's structure
/// as it was: nothing is written back.
#[test]
fn a_request_that_sets_does_not_write_back() {
    let _m = a_machine();
    let mut ifr = ifreq("eth0");
    ifr.ifr_ifru.addr = sin([10, 0, 2, 15]);
    let before = addr_of(&ifr);
    assert_eq!(ask(SIOCSIFADDR, &mut ifr), Ok(0));
    assert_eq!(addr_of(&ifr), before);
    assert!(net_ioctl_changes(SIOCSIFADDR) && net_ioctl_changes(SIOCADDRT));
    assert!(!net_ioctl_changes(SIOCGIFADDR) && !net_ioctl_changes(SIOCGIFCONF));
}

fn ifconf(len: i32, buf: &mut [IfReq]) -> IfConf {
    IfConf {
        ifc_len: len,
        ifc_buf: if buf.is_empty() {
            0
        } else {
            buf.as_mut_ptr() as usize
        },
    }
}

fn ask_conf(ifc: &mut IfConf) -> LxResult<usize> {
    handle_net_ioctl(SIOCGIFCONF, ifc as *mut IfConf as usize, 0, 0, false)
}

/// `SIOCGIFCONF` with room for every card: one `ifreq` each, named and
/// with its address, and `ifc_len` the bytes it filled.
#[test]
fn the_list_of_cards_names_each_with_its_address() {
    let _m = a_machine();
    let mut list = [ifreq(""), ifreq(""), ifreq("")];
    let mut ifc = ifconf(core::mem::size_of_val(&list) as i32, &mut list);
    assert_eq!(ask_conf(&mut ifc), Ok(0));
    assert_eq!(ifc.ifc_len as usize, 2 * size_of::<IfReq>());
    assert_eq!(ifc.ifc_buf, list.as_ptr() as usize, "the caller's own list");
    assert_eq!(list[0].name(), "eth0");
    assert_eq!(addr_of(&list[0]), (AF_INET, [10, 0, 2, 15]));
    assert_eq!(list[1].name(), "lo");
    assert_eq!(addr_of(&list[1]), (AF_INET, [127, 0, 0, 1]));
    assert_eq!(list[2].name(), "", "past ifc_len is not written");
}

/// With room for one, one; with room for less than one, none; and a list
/// of no length says how long one would be.
#[test]
fn a_short_list_is_filled_as_far_as_it_goes() {
    let _m = a_machine();
    let mut list = [ifreq(""), ifreq("")];
    let mut ifc = ifconf(size_of::<IfReq>() as i32 + 3, &mut list);
    assert_eq!(ask_conf(&mut ifc), Ok(0));
    assert_eq!(ifc.ifc_len as usize, size_of::<IfReq>());
    assert_eq!((list[0].name(), list[1].name()), ("eth0", ""));

    let mut ifc = ifconf(size_of::<IfReq>() as i32 - 1, &mut list);
    assert_eq!(ask_conf(&mut ifc), Ok(0));
    assert_eq!(ifc.ifc_len, 0);

    let mut ifc = ifconf(0, &mut []);
    assert_eq!(ask_conf(&mut ifc), Ok(0));
    assert_eq!(ifc.ifc_len as usize, 2 * size_of::<IfReq>());

    let mut ifc = ifconf(-1, &mut list);
    assert_eq!(ask_conf(&mut ifc), Err(LxError::EINVAL));
}

/// A list that is not the caller's to write is `EFAULT`, not a write into
/// whatever is there.
#[test]
fn a_list_at_an_address_that_is_not_the_callers_is_efault() {
    let _m = a_machine();
    let _strict = Strict::on();
    let mut ifc = IfConf {
        ifc_len: 2 * size_of::<IfReq>() as i32,
        ifc_buf: KERNEL,
    };
    assert_eq!(ask_conf(&mut ifc), Err(LxError::EFAULT));
}

fn rtentry(dst: [u8; 4], mask: [u8; 4], gateway: Option<[u8; 4]>, dev: Option<&[u8]>) -> RtEntry {
    #[allow(unsafe_code)]
    let mut rt: RtEntry = unsafe { core::mem::zeroed() };
    rt.rt_dst = sin(dst);
    rt.rt_genmask = sin(mask);
    rt.rt_flags = RTF_UP;
    if let Some(gateway) = gateway {
        rt.rt_gateway = sin(gateway);
        rt.rt_flags |= RTF_GATEWAY;
    }
    if let Some(dev) = dev {
        rt.rt_dev = dev.as_ptr() as *mut u8;
    }
    rt
}

fn route(request: usize, rt: &mut RtEntry) -> LxResult<usize> {
    handle_net_ioctl(request, rt as *mut RtEntry as usize, 0, 0, false)
}

/// `route add -net 192.168.50.0/24 gw 10.0.2.1 dev eth0`, and `route del`
/// of it again.
#[test]
fn a_route_is_added_to_the_card_it_names_and_taken_away() {
    let m = a_machine();
    let dev = b"eth0\0";
    let mut rt = rtentry(
        [192, 168, 50, 0],
        [255, 255, 255, 0],
        Some([10, 0, 2, 1]),
        Some(dev),
    );
    assert_eq!(route(SIOCADDRT, &mut rt), Ok(0));
    let want = IpCidr::new(IpAddress::v4(192, 168, 50, 0), 24);
    {
        let told = m.card("eth0").told.lock();
        let added = told.routes.iter().find(|r| r.dst == want).expect("route");
        assert_eq!(added.gateway, Some(IpAddress::v4(10, 0, 2, 1)));
    }
    assert_eq!(
        rt.rt_dev,
        dev.as_ptr() as *mut u8,
        "the caller's pointer stays"
    );
    assert_eq!(route(SIOCDELRT, &mut rt), Ok(0));
    assert!(!m
        .card("eth0")
        .told
        .lock()
        .routes
        .iter()
        .any(|r| r.dst == want));
}

/// With no `dev` the route goes on `eth0`; a route without `RTF_GATEWAY`
/// has no gateway, whatever `rt_gateway` holds.
#[test]
fn a_route_with_no_card_and_no_gateway() {
    let m = a_machine();
    let mut rt = rtentry([10, 9, 0, 0], [255, 255, 0, 0], None, None);
    rt.rt_gateway = sin([1, 2, 3, 4]);
    assert_eq!(route(SIOCADDRT, &mut rt), Ok(0));
    let told = m.card("eth0").told.lock();
    let added = told
        .routes
        .iter()
        .find(|r| r.dst == IpCidr::new(IpAddress::v4(10, 9, 0, 0), 16))
        .expect("route");
    assert_eq!(added.gateway, None);
}

/// A card's name that no card has is `ENODEV`; one with no end within
/// `IFNAMSIZ` is `EINVAL`; and a name at an address that is not the
/// caller's is `EFAULT`.
#[test]
fn a_route_on_a_card_that_is_not_there() {
    let _m = a_machine();
    let mut rt = rtentry([10, 9, 0, 0], [255, 255, 0, 0], None, Some(b"wlan9\0"));
    assert_eq!(route(SIOCADDRT, &mut rt), Err(LxError::ENODEV));
    let long = [b'e'; 20];
    let mut rt = rtentry([10, 9, 0, 0], [255, 255, 0, 0], None, Some(&long));
    assert_eq!(route(SIOCADDRT, &mut rt), Err(LxError::EINVAL));
    let mut rt = rtentry([10, 9, 0, 0], [255, 255, 0, 0], None, None);
    rt.rt_dev = KERNEL as *mut u8;
    let _strict = Strict::on();
    assert_eq!(route(SIOCADDRT, &mut rt), Err(LxError::EFAULT));
}

/// A card that keeps no routes refuses them, and so does the request.
#[test]
fn a_card_that_keeps_no_routes_refuses_them() {
    let _m = plug(vec![Card::ether("eth0", [2, 0, 0, 0, 0, 1]).refusing()]);
    let mut rt = rtentry([10, 9, 0, 0], [255, 255, 0, 0], None, None);
    assert_eq!(route(SIOCADDRT, &mut rt), Err(LxError::EIO));
    assert_eq!(route(SIOCDELRT, &mut rt), Err(LxError::EIO));
}

fn in6_rtmsg(dst: Ipv6Address, len: u16, gateway: Option<Ipv6Address>, ifindex: i32) -> In6RtMsg {
    #[allow(unsafe_code)]
    let mut rt: In6RtMsg = unsafe { core::mem::zeroed() };
    rt.rtmsg_dst = dst.0;
    rt.rtmsg_dst_len = len;
    rt.rtmsg_flags = RTF_UP as u32;
    if let Some(gateway) = gateway {
        rt.rtmsg_gateway = gateway.0;
        rt.rtmsg_flags |= RTF_GATEWAY as u32;
    }
    rt.rtmsg_ifindex = ifindex;
    rt
}

/// `route -A inet6 add 2001:db8::/32 gw fe80::1 dev eth0`: the IPv6 route
/// names its card by index, an `in6_rtmsg` of an IPv6 socket.
#[test]
fn an_ipv6_route_goes_on_the_card_of_its_index() {
    let m = a_machine();
    let dst = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0);
    let gw = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    let mut rt = in6_rtmsg(dst, 32, Some(gw), 1);
    let at = &mut rt as *mut In6RtMsg as usize;
    assert_eq!(handle_net_ioctl(SIOCADDRT, at, 0, 0, true), Ok(0));
    let want = IpCidr::new(IpAddress::Ipv6(dst), 32);
    assert!(m
        .card("eth0")
        .told
        .lock()
        .routes
        .iter()
        .any(|r| r.dst == want && r.gateway == Some(IpAddress::Ipv6(gw))));
    assert_eq!(handle_net_ioctl(SIOCDELRT, at, 0, 0, true), Ok(0));
    assert!(m
        .card("eth0")
        .told
        .lock()
        .routes
        .iter()
        .all(|r| r.dst != want));

    // Index 0 is `eth0`, and an index past the last card is `ENODEV`.
    let mut rt = in6_rtmsg(dst, 32, None, 0);
    let at = &mut rt as *mut In6RtMsg as usize;
    assert_eq!(handle_net_ioctl(SIOCADDRT, at, 0, 0, true), Ok(0));
    assert!(m
        .card("eth0")
        .told
        .lock()
        .routes
        .iter()
        .any(|r| r.dst == want && r.gateway.is_none()));
    let mut rt = in6_rtmsg(dst, 32, None, 9);
    let at = &mut rt as *mut In6RtMsg as usize;
    assert_eq!(
        handle_net_ioctl(SIOCADDRT, at, 0, 0, true),
        Err(LxError::ENODEV)
    );
    assert_eq!(
        handle_net_ioctl(SIOCDELRT, at, 0, 0, true),
        Err(LxError::ENODEV)
    );
}

fn arpreq(address: [u8; 4], hardware: [u8; 6], dev: &str) -> [u8; ARPREQ_LEN] {
    let mut req = [0u8; ARPREQ_LEN];
    req[0..2].copy_from_slice(&2u16.to_ne_bytes());
    req[4..8].copy_from_slice(&address);
    req[16..18].copy_from_slice(&ARPHRD_ETHER.to_ne_bytes());
    req[18..24].copy_from_slice(&hardware);
    req[52..52 + dev.len()].copy_from_slice(dev.as_bytes());
    req
}

fn arp(request: usize, req: &mut [u8; ARPREQ_LEN]) -> LxResult<usize> {
    handle_net_ioctl(request, req.as_mut_ptr() as usize, 0, 0, false)
}

/// `arp -s 10.0.2.7 02:00:00:00:00:07`, `arp -a`, `arp -d`: a neighbour
/// written by hand is said as complete and permanent, and once forgotten
/// it is `ENXIO`.
#[test]
fn a_neighbour_written_by_hand_is_said_and_forgotten() {
    let m = a_machine();
    let mac = [2, 0, 0, 0, 0, 7];
    let mut req = arpreq([10, 0, 2, 7], mac, "");
    assert_eq!(arp(SIOCSARP, &mut req), Ok(0));
    assert_eq!(
        m.card("eth0").told.lock().neighbours.len(),
        1,
        "on the card of its network"
    );

    let mut req = arpreq([10, 0, 2, 7], [0; 6], "eth0");
    assert_eq!(arp(SIOCGARP, &mut req), Ok(0));
    assert_eq!(req[18..24], mac);
    assert_eq!(u16::from_ne_bytes([req[16], req[17]]), ARPHRD_ETHER);
    let flags = u32::from_ne_bytes([req[32], req[33], req[34], req[35]]);
    assert_eq!(flags, ATF_COM | ATF_PERM);

    assert_eq!(arp(SIOCDARP, &mut req), Ok(0));
    assert_eq!(arp(SIOCGARP, &mut req), Err(LxError::ENXIO));
    assert_eq!(
        arp(SIOCDARP, &mut req),
        Err(LxError::ENXIO),
        "nothing to forget"
    );
}

/// A neighbour the card met by itself is complete but not permanent.
#[test]
fn a_neighbour_met_on_the_link_is_not_permanent() {
    let _m = plug(vec![Card::ether("eth0", [2, 0, 0, 0, 0, 1])
        .with_address(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24))
        .with_neighbour(
            IpAddress::v4(10, 0, 2, 2),
            [2, 0, 0, 0, 0, 2],
            5_000,
        )]);
    let mut req = arpreq([10, 0, 2, 2], [0; 6], "");
    assert_eq!(arp(SIOCGARP, &mut req), Ok(0));
    let flags = u32::from_ne_bytes([req[32], req[33], req[34], req[35]]);
    assert_eq!(flags, ATF_COM);
}

/// `arp_ioctl`: an address that is not `AF_INET` is `EPFNOSUPPORT`; one on
/// no card's network is `ENETUNREACH`; a card that is not there `ENODEV`.
#[test]
fn an_arp_request_the_machine_cannot_answer() {
    let _m = a_machine();
    let mut req = arpreq([10, 0, 2, 7], [0; 6], "");
    req[0..2].copy_from_slice(&10u16.to_ne_bytes());
    assert_eq!(arp(SIOCGARP, &mut req), Err(LxError::EPFNOSUPPORT));
    let mut req = arpreq([172, 16, 0, 1], [0; 6], "");
    assert_eq!(arp(SIOCGARP, &mut req), Err(LxError::ENETUNREACH));
    let mut req = arpreq([10, 0, 2, 7], [0; 6], "wlan9");
    assert_eq!(arp(SIOCGARP, &mut req), Err(LxError::ENODEV));
}

/// A card that keeps no neighbours: writing one is `EOPNOTSUPP`.
#[test]
fn a_card_that_keeps_no_neighbours_says_it_cannot() {
    let _m = plug(vec![Card::ether("eth0", [2, 0, 0, 0, 0, 1])
        .with_address(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24))
        .refusing()]);
    let mut req = arpreq([10, 0, 2, 7], [2, 0, 0, 0, 0, 7], "");
    assert_eq!(arp(SIOCSARP, &mut req), Err(LxError::EOPNOTSUPP));
}

/// The address of a card is the first IPv4 one it has that is an address
/// at all: not `0.0.0.0`, not one on no network, not the placeholder.
#[test]
fn the_cards_address_is_the_first_real_one() {
    let card = Card::ether("eth0", [2, 0, 0, 0, 0, 1])
        .with_address(IpCidr::new(IpAddress::v4(0, 0, 0, 0), 24))
        .with_address(IpCidr::new(IpAddress::v4(10, 1, 1, 1), 0))
        .with_address(IpCidr::new(IpAddress::v4(10, 1, 1, 2), 16));
    assert_eq!(
        iface_ipv4_cidr(&card),
        Some(Ipv4Cidr::new(Ipv4Address([10, 1, 1, 2]), 16))
    );
    assert_eq!(iface_ipv4_cidr(&Card::ether("eth1", [2; 6])), None);
    assert_eq!(ipv4_netmask(0), Ipv4Address([0; 4]));
    assert_eq!(ipv4_netmask(32), Ipv4Address([255; 4]));
    assert_eq!(
        ipv4_broadcast(Ipv4Address([10, 1, 2, 3]), 0),
        Ipv4Address([255; 4])
    );
    assert_eq!(
        ipv4_broadcast(Ipv4Address([10, 1, 2, 3]), 32),
        Ipv4Address([10, 1, 2, 3])
    );
}

/// The card a packet for an address leaves by: the one whose network has
/// it, the narrowest when several do, never the loopback; and otherwise
/// the first with an address.
#[test]
fn a_packet_leaves_by_the_card_of_its_network() {
    let _m = plug(vec![
        Card::loopback(),
        Card::ether("eth0", [2, 0, 0, 0, 0, 1])
            .with_address(IpCidr::new(IpAddress::v4(10, 0, 0, 5), 8)),
        Card::ether("eth1", [2, 0, 0, 0, 0, 2])
            .with_address(IpCidr::new(IpAddress::v4(10, 7, 0, 5), 16)),
    ]);
    let by = |a: [u8; 4]| netdev_for_ipv4(Ipv4Address(a)).map(|c| c.get_ifname());
    assert_eq!(by([10, 7, 3, 3]), Ok("eth1".into()));
    assert_eq!(by([10, 8, 3, 3]), Ok("eth0".into()));
    assert_eq!(by([8, 8, 8, 8]), Ok("eth0".into()));
    assert_eq!(by([127, 0, 0, 1]), Ok("eth0".into()), "never lo");
}

#[test]
fn with_no_card_that_has_an_address_nothing_leaves() {
    let _m = plug(vec![Card::loopback(), Card::ether("eth0", [2; 6])]);
    assert_eq!(
        netdev_for_ipv4(Ipv4Address([8, 8, 8, 8])).err(),
        Some(LxError::ENODEV)
    );
    assert_eq!(iface_by_name("eth0").unwrap().ifname(), "eth0");
}
