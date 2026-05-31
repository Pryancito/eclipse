//! ICMP/ICMPv6 echo replies delivered from RX frames (same path as DHCP via `push_packet`).
//! smoltcp ingress is not relied on for ping RX.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use kernel_hal::net::get_net_device;
use lazy_static::lazy_static;
use lock::Mutex;
use smoltcp::wire::{IpAddress, IpCidr, IpProtocol, Ipv4Packet};

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
                    if cidr.prefix_len() > 0 && cidr.address() == a {
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
        if ip.len() < 20 {
            return;
        }
        let pkt = Ipv4Packet::new_unchecked(ip);
        info!(
            "[icmp_rx] IPv4 packet: protocol={}, src={}, dst={}",
            pkt.protocol(),
            pkt.src_addr(),
            pkt.dst_addr()
        );
        if pkt.protocol() != IpProtocol::Icmp {
            return;
        }
        let src = IpAddress::Ipv4(pkt.src_addr());
        let dst = IpAddress::Ipv4(pkt.dst_addr());
        if !is_our_ip(dst) {
            info!("[icmp_rx] dst {} is not our IP", dst);
            return;
        }
        let payload = pkt.payload();
        if payload.is_empty() {
            info!("[icmp_rx] payload is empty");
            return;
        }
        info!(
            "[icmp_rx] ICMPv4 packet: type={}, code={}",
            payload[0],
            payload.get(1).unwrap_or(&0)
        );
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
        info!("[icmp_rx] queued ICMPv4 Echo Reply!");
    } else if et == 0x86dd {
        use smoltcp::wire::Ipv6Packet;
        let ip = &frame[l2..];
        if ip.len() < 40 {
            info!("[icmp_rx] et is IPv6 but ip len is too short: {}", ip.len());
            return;
        }
        let pkt = Ipv6Packet::new_unchecked(ip);
        info!(
            "[icmp_rx] IPv6 packet: next_header={}, src={}, dst={}",
            pkt.next_header(),
            pkt.src_addr(),
            pkt.dst_addr()
        );
        if pkt.next_header() != IpProtocol::Icmpv6 {
            return;
        }
        let icmp_bytes = pkt.payload();
        let icmp_pkt = smoltcp::wire::Icmpv6Packet::new_unchecked(icmp_bytes);
        let src = IpAddress::Ipv6(pkt.src_addr());
        let dst = IpAddress::Ipv6(pkt.dst_addr());
        let cs_ok = icmp_pkt.verify_checksum(&src, &dst);
        info!(
            "[icmp_rx] ICMPv6 packet length: {}, checksum field: 0x{:04x}, calculated cs_ok: {}, bytes: {:?}",
            icmp_bytes.len(),
            icmp_pkt.checksum(),
            cs_ok,
            icmp_bytes
        );
        if !is_our_ip(dst) {
            info!("[icmp_rx] dst {} is not our IP", dst);
            return;
        }
        let payload = pkt.payload();
        if payload.is_empty() {
            info!("[icmp_rx] payload is empty");
            return;
        }
        info!(
            "[icmp_rx] ICMPv6 packet: type={}, code={}",
            payload[0],
            payload.get(1).unwrap_or(&0)
        );
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
        info!("[icmp_rx] queued ICMPv6 Echo Reply!");
    }
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
