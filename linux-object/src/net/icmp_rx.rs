//! ICMP/ICMPv6 echo replies delivered from RX frames (same path as DHCP via `push_packet`).
//! smoltcp ingress is not relied on for ping RX.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use kernel_hal::net::get_net_device;
use lazy_static::lazy_static;
use lock::Mutex;
use smoltcp::wire::{IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv4Packet, Ipv6Packet};

/// Pre-DHCP sentinel still present in older images; not a usable host address.
pub fn is_ipv4_placeholder(addr: Ipv4Address) -> bool {
    addr == Ipv4Address::new(240, 0, 0, 0)
}

const RX_QUEUE_MAX: usize = 64;

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
                (IpCidr::Ipv4(cidr), IpAddress::Ipv4(a)) => {
                    if !is_ipv4_placeholder(cidr.address())
                        && cidr.prefix_len() > 0
                        && cidr.address() == a
                    {
                        return true;
                    }
                }
                (IpCidr::Ipv6(cidr), IpAddress::Ipv6(a)) => {
                    if cidr.prefix_len() > 0 && cidr.address() == a {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

/// Parse an Ethernet frame; queue ICMP / ICMPv6 echo replies addressed to us.
pub fn deliver_from_frame(frame: &[u8]) {
    if frame.len() < 14 {
        return;
    }
    let mut l2 = 14usize;
    let mut et = u16::from_be_bytes([frame[12], frame[13]]);
    if et == 0x8100 {
        if frame.len() < 18 {
            return;
        }
        l2 = 18;
        et = u16::from_be_bytes([frame[16], frame[17]]);
    }
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
        let mut q = RX_QUEUE.lock();
        if q.len() >= RX_QUEUE_MAX {
            q.pop_front();
        }
        q.push_back(IcmpRxPacket {
            src,
            data: payload.to_vec(),
        });
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
            data: payload.to_vec(),
        });
    }
}

/// Queue an ICMP echo reply locally (self-ping / loopback shortcut).
pub fn queue_echo_reply(src: IpAddress, mut icmp: Vec<u8>) {
    if icmp.is_empty() {
        return;
    }
    let reply_type = match src {
        IpAddress::Ipv6(_) => 129u8,
        _ => 0u8,
    };
    if icmp[0] == 8 || icmp[0] == 128 {
        icmp[0] = reply_type;
    }
    let mut q = RX_QUEUE.lock();
    if q.len() >= RX_QUEUE_MAX {
        q.pop_front();
    }
    q.push_back(IcmpRxPacket { src, data: icmp });
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
