//! ICMP echo replies delivered from RX frames (same path as DHCP via `push_packet`).
//! smoltcp ingress is not relied on for ping RX.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use kernel_hal::net::get_net_device;
use lazy_static::lazy_static;
use lock::Mutex;
use smoltcp::wire::{IpCidr, IpProtocol, Ipv4Address, Ipv4Packet};

const RX_QUEUE_MAX: usize = 64;

struct IcmpRxPacket {
    src: Ipv4Address,
    data: Vec<u8>,
}

lazy_static! {
    static ref RX_QUEUE: Mutex<VecDeque<IcmpRxPacket>> = Mutex::new(VecDeque::new());
}

fn is_our_ipv4(addr: Ipv4Address) -> bool {
    if addr.is_unspecified() {
        return false;
    }
    for dev in get_net_device().iter() {
        for ip in dev.get_ip_address() {
            if let IpCidr::Ipv4(cidr) = ip {
                if cidr.prefix_len() > 0 && cidr.contains_addr(&addr) {
                    return true;
                }
            }
        }
    }
    false
}

/// Parse an Ethernet frame; queue ICMP echo replies addressed to us.
pub fn deliver_from_frame(frame: &[u8]) {
    if frame.len() < 14 + 20 {
        return;
    }
    let mut l2 = 14usize;
    let mut et = u16::from_be_bytes([frame[12], frame[13]]);
    if et == 0x8100 {
        if frame.len() < 18 + 20 {
            return;
        }
        l2 = 18;
        et = u16::from_be_bytes([frame[16], frame[17]]);
    }
    if et != 0x0800 {
        return;
    }
    let ip = &frame[l2..];
    if ip.len() < 20 {
        return;
    }
    let pkt = Ipv4Packet::new_unchecked(ip);
    if pkt.protocol() != IpProtocol::Icmp {
        return;
    }
    if !is_our_ipv4(pkt.dst_addr()) {
        return;
    }
    let payload = pkt.payload();
    if payload.is_empty() || payload[0] != 0 {
        // 0 = Echo Reply
        return;
    }
    let mut q = RX_QUEUE.lock();
    if q.len() >= RX_QUEUE_MAX {
        q.pop_front();
    }
    q.push_back(IcmpRxPacket {
        src: pkt.src_addr(),
        data: payload.to_vec(),
    });
}

pub fn pop() -> Option<(Vec<u8>, Ipv4Address)> {
    RX_QUEUE
        .lock()
        .pop_front()
        .map(|p| (p.data, p.src))
}

pub fn pending() -> bool {
    !RX_QUEUE.lock().is_empty()
}
