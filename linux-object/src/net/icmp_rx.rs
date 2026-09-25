//! ICMP/ICMPv6 echo replies delivered from RX frames (same path as DHCP via `push_packet`).
//! smoltcp ingress is not relied on for ping RX.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use kernel_hal::net::get_net_device;
use lazy_static::lazy_static;
use lock::Mutex;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    Icmpv4Packet, Icmpv6Packet, Icmpv6Repr, IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv4Packet,
    Ipv6Packet,
};
/// Pre-DHCP sentinel still present in older images; not a usable host address.
pub fn is_ipv4_placeholder(addr: Ipv4Address) -> bool {
    addr == Ipv4Address::new(240, 0, 0, 0)
}

const RX_QUEUE_MAX: usize = 64;
const MAX_ICMP_PAYLOAD: usize = 1280;

struct IcmpRxPacket {
    src: IpAddress,
    data: Vec<u8>,
}

lazy_static! {
    static ref RX_QUEUE: Mutex<VecDeque<IcmpRxPacket>> = Mutex::new(VecDeque::new());
}

fn is_our_ip(addr: IpAddress) -> bool {
    if addr.is_unspecified() {
        return false;
    }
    match addr {
        IpAddress::Ipv4(a) if a.is_loopback() => return true,
        IpAddress::Ipv6(a) if a.is_loopback() => return true,
        _ => {}
    }
    for dev in get_net_device().iter() {
        for ip in dev.get_ip_address() {
            match (ip, addr) {
                (IpCidr::Ipv4(cidr), IpAddress::Ipv4(a))
                    if !is_ipv4_placeholder(cidr.address())
                        && cidr.prefix_len() > 0
                        && cidr.address() == a =>
                {
                    return true;
                }
                (IpCidr::Ipv6(cidr), IpAddress::Ipv6(a))
                    if cidr.prefix_len() > 0 && cidr.address() == a =>
                {
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}

/// Parse an Ethernet frame; queue ICMP / ICMPv6 echo replies addressed to us.
pub fn deliver_from_frame(frame: &[u8]) {
    // Ethernet header, optionally one 802.1Q VLAN tag, then IP.
    let Some((l2, et)) = crate::net::packet::eth_l2_header_len(frame) else {
        return;
    };
    if et == 0x0800 {
        let ip = &frame[l2..];
        let Ok(pkt) = Ipv4Packet::new_checked(ip) else {
            return;
        };
        if pkt.protocol() != IpProtocol::Icmp {
            return;
        }
        let src = IpAddress::Ipv4(pkt.src_addr());
        let dst = IpAddress::Ipv4(pkt.dst_addr());
        if !is_our_ip(dst) {
            return;
        }
        let payload = pkt.payload();
        if payload.is_empty() {
            return;
        }
        if payload[0] != 0 {
            // 0 = Echo Reply
            return;
        }
        // Verify the ICMPv4 checksum to reject corrupt/truncated frames.
        let Ok(icmp_pkt) = Icmpv4Packet::new_checked(payload) else {
            return;
        };
        if !icmp_pkt.verify_checksum() {
            return;
        }
        let mut q = RX_QUEUE.lock();
        if q.len() >= RX_QUEUE_MAX {
            q.pop_front();
        }
        q.push_back(IcmpRxPacket {
            src,
            data: payload[..payload.len().min(MAX_ICMP_PAYLOAD)].to_vec(),
        });
        drop(q);
        kernel_hal::net::wake_net_rx_waiters();
    } else if et == 0x86dd {
        let ip = &frame[l2..];
        let Ok(pkt) = Ipv6Packet::new_checked(ip) else {
            return;
        };
        if pkt.next_header() != IpProtocol::Icmpv6 {
            return;
        }
        let Ok(icmp_pkt) = smoltcp::wire::Icmpv6Packet::new_checked(pkt.payload()) else {
            return;
        };
        let src = IpAddress::Ipv6(pkt.src_addr());
        let dst = IpAddress::Ipv6(pkt.dst_addr());
        if !icmp_pkt.verify_checksum(&src, &dst) {
            return;
        }
        if !is_our_ip(dst) {
            return;
        }
        let payload = pkt.payload();
        if payload.is_empty() {
            return;
        }
        if payload[0] != 129 {
            // 129 = Echo Reply
            return;
        }
        let mut q = RX_QUEUE.lock();
        if q.len() >= RX_QUEUE_MAX {
            q.pop_front();
        }
        q.push_back(IcmpRxPacket {
            src,
            data: payload[..payload.len().min(MAX_ICMP_PAYLOAD)].to_vec(),
        });
        drop(q);
        kernel_hal::net::wake_net_rx_waiters();
    }
}

fn finalize_icmp_echo_reply_v4(icmp: &mut [u8]) {
    if icmp.len() < 8 {
        return;
    }
    if icmp[0] == 8 {
        icmp[0] = 0;
    }
    let mut pkt = Icmpv4Packet::new_unchecked(icmp);
    pkt.fill_checksum();
}

fn finalize_icmp_echo_reply_v6(src: IpAddress, icmp: &mut Vec<u8>) {
    if icmp.len() < 8 {
        return;
    }
    let IpAddress::Ipv6(dst_v6) = src else {
        return;
    };
    if icmp[0] == 128 {
        icmp[0] = 129;
    }
    let ident = u16::from_be_bytes([icmp[4], icmp[5]]);
    let seq_no = u16::from_be_bytes([icmp[6], icmp[7]]);
    let echo_data = icmp[8..].to_vec();
    let repr = Icmpv6Repr::EchoReply {
        ident,
        seq_no,
        data: &echo_data,
    };
    let mut out = vec![0u8; repr.buffer_len()];
    let mut pkt = Icmpv6Packet::new_unchecked(&mut out);
    repr.emit(
        &IpAddress::Ipv6(dst_v6),
        &IpAddress::Ipv6(dst_v6),
        &mut pkt,
        &ChecksumCapabilities::default(),
    );
    *icmp = out;
}

/// Queue an ICMP echo reply locally (self-ping / loopback shortcut).
pub fn queue_echo_reply(src: IpAddress, mut icmp: Vec<u8>) {
    if icmp.is_empty() {
        return;
    }
    match src {
        IpAddress::Ipv4(_) => finalize_icmp_echo_reply_v4(&mut icmp),
        IpAddress::Ipv6(_) => finalize_icmp_echo_reply_v6(src, &mut icmp),
        _ => {}
    }
    let mut q = RX_QUEUE.lock();
    if q.len() >= RX_QUEUE_MAX {
        q.pop_front();
    }
    q.push_back(IcmpRxPacket { src, data: icmp });
    drop(q);
    kernel_hal::net::wake_net_rx_waiters();
}

pub fn pop_for(ipv6: bool, remote: Option<IpAddress>) -> Option<(Vec<u8>, IpAddress)> {
    let mut q = RX_QUEUE.lock();
    let idx = q.iter().position(|pkt| {
        let family_ok = matches!(
            (ipv6, pkt.src),
            (true, IpAddress::Ipv6(_)) | (false, IpAddress::Ipv4(_))
        );
        if !family_ok {
            return false;
        }
        match remote {
            Some(remote_ip) => pkt.src == remote_ip,
            None => true,
        }
    })?;
    q.remove(idx).map(|p| (p.data, p.src))
}

pub fn pending() -> bool {
    !RX_QUEUE.lock().is_empty()
}

/// Returns true if there is at least one queued reply matching the given address family.
/// More precise than `pending()` — avoids spurious wakeups when replies from the
/// opposite family (e.g. ICMPv6 while waiting for ICMPv4) are in the queue.
pub fn pending_for(ipv6: bool) -> bool {
    let q = RX_QUEUE.lock();
    q.iter().any(|pkt| {
        matches!(
            (ipv6, pkt.src),
            (true, IpAddress::Ipv6(_)) | (false, IpAddress::Ipv4(_))
        )
    })
}

/// Build a full IPv4 frame (header + ICMP) for `SOCK_RAW` recv (BusyBox ping as root).
pub fn wrap_icmpv4_raw_frame(src_addr: Ipv4Address, dst_addr: Ipv4Address, icmp: &[u8]) -> Vec<u8> {
    let total = 20 + icmp.len();
    let mut buf = vec![0u8; total];
    let mut pkt = Ipv4Packet::new_unchecked(&mut buf);
    pkt.set_version(4);
    pkt.set_header_len(20);
    pkt.set_total_len(total as u16);
    pkt.set_protocol(IpProtocol::Icmp);
    pkt.set_src_addr(src_addr);
    pkt.set_dst_addr(dst_addr);
    pkt.set_hop_limit(64);
    pkt.payload_mut().copy_from_slice(icmp);
    pkt.fill_checksum();
    buf
}

/// Dequeue an echo reply and wrap it for `SOCK_RAW` + `IPPROTO_ICMP` read(2).
pub fn pop_ipv4_raw_reply(remote: Option<IpAddress>, buf: &mut [u8]) -> Option<(usize, IpAddress)> {
    let (icmp, peer) = pop_for(false, remote)?;
    let IpAddress::Ipv4(peer_v4) = peer else {
        return None;
    };
    let our = crate::net::select_ipv4_for_dst(peer_v4);
    if our.is_unspecified() {
        return None;
    }
    // SOCK_RAW + IPPROTO_ICMP delivers a full IPv4 datagram as received:
    // source = peer that sent the reply, destination = us.
    let frame = wrap_icmpv4_raw_frame(peer_v4, our, &icmp);
    let n = frame.len().min(buf.len());
    buf[..n].copy_from_slice(&frame[..n]);
    Some((n, peer))
}

#[cfg(test)]
mod tests {
    //! Echo replies pulled straight out of RX frames, which is how ping RX
    //! works here (smoltcp ingress is not relied on). The third of the four
    //! parsers `push_packet` feeds the same frame to.

    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;
    use alloc::vec;
    use smoltcp::wire::Ipv6Address;

    const PEER: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    fn eth(ethertype: u16, vlan: bool, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0x52u8, 0x54, 0x00, 0xaa, 0xbb, 0xcc];
        f.extend_from_slice(&PEER);
        if vlan {
            f.extend_from_slice(&0x8100u16.to_be_bytes());
            f.extend_from_slice(&0x0064u16.to_be_bytes());
        }
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    /// An 8-byte ICMPv4 echo message (`ty` 8 = request, 0 = reply).
    fn icmpv4(ty: u8, ident: u16, seq: u16, checksum: bool) -> Vec<u8> {
        let mut icmp = vec![0u8; 8];
        icmp[0] = ty;
        icmp[4..6].copy_from_slice(&ident.to_be_bytes());
        icmp[6..8].copy_from_slice(&seq.to_be_bytes());
        if checksum {
            Icmpv4Packet::new_unchecked(&mut icmp[..]).fill_checksum();
        }
        icmp
    }

    /// An IPv4 datagram from `src` to `dst` carrying `icmp`.
    fn ipv4(src: Ipv4Address, dst: Ipv4Address, icmp: &[u8]) -> Vec<u8> {
        let mut buf = wrap_icmpv4_raw_frame(src, dst, icmp);
        // `wrap_icmpv4_raw_frame` is the same header the raw socket hands out.
        let len = buf.len();
        debug_assert_eq!(len, 20 + icmp.len());
        buf.truncate(len);
        buf
    }

    fn drain() {
        while pop_for(false, None).is_some() || pop_for(true, None).is_some() {}
    }

    #[test]
    fn a_vlan_tagged_echo_reply_is_delivered_like_an_untagged_one() {
        let _g = LOCK.lock();
        drain();
        let peer = Ipv4Address::new(10, 0, 2, 2);
        let us = Ipv4Address::new(127, 0, 0, 1);
        for vlan in [false, true] {
            deliver_from_frame(&eth(0x0800, vlan, &ipv4(peer, us, &icmpv4(0, 7, 1, true))));
            assert!(pending_for(false), "vlan={}", vlan);
            let (data, src) = pop_for(false, None).expect("queued");
            assert_eq!(src, IpAddress::Ipv4(peer));
            assert_eq!(&data[4..8], &[0, 7, 0, 1], "ident and seq survive");
        }
        assert!(!pending(), "the queue is drained");
        drain();
    }

    #[test]
    fn a_reply_for_somebody_else_or_with_a_broken_checksum_is_dropped() {
        let _g = LOCK.lock();
        drain();
        let peer = Ipv4Address::new(10, 0, 2, 2);
        let us = Ipv4Address::new(127, 0, 0, 1);

        // Not addressed to any of our addresses.
        deliver_from_frame(&eth(
            0x0800,
            true,
            &ipv4(peer, Ipv4Address::new(10, 9, 9, 9), &icmpv4(0, 7, 1, true)),
        ));
        // A checksum nobody filled in.
        deliver_from_frame(&eth(0x0800, true, &ipv4(peer, us, &icmpv4(0, 7, 1, false))));
        // An echo *request* is not a reply: answering it is smoltcp's job.
        deliver_from_frame(&eth(0x0800, false, &ipv4(peer, us, &icmpv4(8, 7, 1, true))));
        // An IPv4 header that claims more than the frame carries.
        let mut short = eth(0x0800, false, &ipv4(peer, us, &icmpv4(0, 7, 1, true)));
        short.truncate(14 + 20);
        deliver_from_frame(&short);
        // A tag with nothing behind it, and a run of runt frames.
        deliver_from_frame(&eth(0x8100, false, &[]));
        for n in 0..20 {
            deliver_from_frame(&vec![0u8; n]);
        }
        assert!(!pending(), "nothing was queued");
        drain();
    }

    #[test]
    fn pop_for_only_hands_out_the_family_and_the_peer_it_is_asked_for() {
        let _g = LOCK.lock();
        drain();
        let a = IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2));
        let b = IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 3));
        let v6 = IpAddress::Ipv6(Ipv6Address::LOOPBACK);
        queue_echo_reply(a, icmpv4(8, 1, 1, false));
        queue_echo_reply(b, icmpv4(8, 2, 2, false));

        // A socket connected to `b` must not be handed `a`'s reply.
        assert!(pending_for(false));
        assert!(!pending_for(true), "no IPv6 in the queue");
        assert_eq!(pop_for(true, None), None, "the other family waits");
        let (data, src) = pop_for(false, Some(b)).expect("b");
        assert_eq!(src, b);
        assert_eq!(&data[4..6], &2u16.to_be_bytes());
        assert_eq!(pop_for(false, Some(b)), None, "only one was queued");
        // `a`'s is still there, and an unfiltered pop finds it.
        assert!(pop_for(false, None).is_some());
        assert!(!pending());

        queue_echo_reply(v6, vec![128, 0, 0, 0, 0, 3, 0, 3]);
        assert!(pending_for(true));
        assert!(
            !pending_for(false),
            "an ICMPv6 reply never wakes an ICMPv4 ping"
        );
        assert!(pop_for(true, Some(v6)).is_some());
        drain();
    }

    #[test]
    fn a_locally_queued_echo_request_comes_back_as_a_reply_with_a_valid_checksum() {
        let _g = LOCK.lock();
        drain();
        let lo = IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1));
        // The self-ping shortcut: `raw::write` hands the request straight back,
        // so the type has to become 0 and the checksum has to be recomputed or
        // BusyBox ping refuses the reply.
        queue_echo_reply(lo, icmpv4(8, 0x1234, 5, false));
        let (data, src) = pop_for(false, None).expect("queued");
        assert_eq!(src, lo);
        assert_eq!(data[0], 0, "echo request became echo reply");
        assert!(
            Icmpv4Packet::new_checked(&data[..])
                .expect("well formed")
                .verify_checksum(),
            "{:02x?}",
            data
        );
        assert_eq!(&data[4..8], &[0x12, 0x34, 0, 5], "ident and seq kept");

        // An ICMPv6 request likewise becomes 129 with its own pseudo-header
        // checksum.
        let lo6 = IpAddress::Ipv6(Ipv6Address::LOOPBACK);
        queue_echo_reply(lo6, vec![128, 0, 0, 0, 0xab, 0xcd, 0, 9]);
        let (data, _) = pop_for(true, None).expect("queued");
        assert_eq!(data[0], 129);
        assert!(
            Icmpv6Packet::new_checked(&data[..])
                .expect("well formed")
                .verify_checksum(&lo6, &lo6),
            "{:02x?}",
            data
        );
        assert_eq!(&data[4..8], &[0xab, 0xcd, 0, 9]);
        drain();
    }

    #[test]
    fn the_queue_never_grows_past_its_cap_and_drops_the_oldest() {
        let _g = LOCK.lock();
        drain();
        let peer = IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2));
        for i in 0..(RX_QUEUE_MAX + 4) {
            queue_echo_reply(peer, icmpv4(0, i as u16, i as u16, false));
        }
        assert_eq!(RX_QUEUE.lock().len(), RX_QUEUE_MAX);
        // The four oldest are gone, so the first one out is number 4.
        let (data, _) = pop_for(false, None).expect("queued");
        assert_eq!(&data[4..6], &4u16.to_be_bytes());
        drain();
    }

    #[test]
    fn a_reply_longer_than_the_payload_cap_is_truncated_not_refused() {
        let _g = LOCK.lock();
        drain();
        let peer = Ipv4Address::new(10, 0, 2, 2);
        let us = Ipv4Address::new(127, 0, 0, 1);
        let mut icmp = vec![0u8; MAX_ICMP_PAYLOAD + 64];
        icmp[0] = 0;
        Icmpv4Packet::new_unchecked(&mut icmp[..]).fill_checksum();
        deliver_from_frame(&eth(0x0800, true, &ipv4(peer, us, &icmp)));
        let (data, _) = pop_for(false, None).expect("queued");
        assert_eq!(data.len(), MAX_ICMP_PAYLOAD);
        drain();
    }

    #[test]
    fn the_pre_dhcp_sentinel_is_never_one_of_our_addresses() {
        assert!(is_ipv4_placeholder(Ipv4Address::new(240, 0, 0, 0)));
        assert!(!is_ipv4_placeholder(Ipv4Address::new(240, 0, 0, 1)));
        assert!(!is_ipv4_placeholder(Ipv4Address::UNSPECIFIED));
        // The unspecified address is nobody's, and loopback is always ours.
        assert!(!is_our_ip(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)));
        assert!(!is_our_ip(IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)));
        assert!(is_our_ip(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1))));
        assert!(is_our_ip(IpAddress::Ipv6(Ipv6Address::LOOPBACK)));
    }
}
