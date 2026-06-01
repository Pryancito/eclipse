//! Software IPv6 → Ethernet cache fed from every dispatched RX frame.

use alloc::collections::BTreeMap;
use lazy_static::lazy_static;
use lock::Mutex;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    EthernetAddress, Icmpv6Packet, Icmpv6Repr, IpAddress, Ipv6Address, Ipv6Packet, NdiscRepr,
};

lazy_static! {
    static ref LOCAL_MACS: Mutex<alloc::vec::Vec<EthernetAddress>> =
        Mutex::new(alloc::vec::Vec::new());
}

/// Refresh cached NIC MACs (call after probe / NewAddr).
pub fn refresh_local_macs(macs: alloc::vec::Vec<EthernetAddress>) {
    *LOCAL_MACS.lock() = macs;
}

fn is_local_mac(mac: EthernetAddress) -> bool {
    LOCAL_MACS.lock().iter().any(|m| *m == mac)
}

const CACHE_MAX: usize = 512;

lazy_static! {
    static ref CACHE: Mutex<BTreeMap<Ipv6Address, EthernetAddress>> = Mutex::new(BTreeMap::new());
}

fn insert_bounded(map: &mut BTreeMap<Ipv6Address, EthernetAddress>, ip: Ipv6Address, mac: EthernetAddress) {
    if map.len() >= CACHE_MAX && !map.contains_key(&ip) {
        if let Some(old) = map.keys().next().copied() {
            map.remove(&old);
        }
    }
    map.insert(ip, mac);
}

/// Learn mappings from a complete Ethernet frame (called from `push_packet`).
pub fn learn_from_frame(frame: &[u8]) {
    if frame.len() < 14 {
        return;
    }
    let src_mac = EthernetAddress::from_bytes(&frame[6..12]);
    if !src_mac.is_unicast() || is_local_mac(src_mac) {
        return;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != 0x86dd {
        return;
    }
    let ipv6 = match Ipv6Packet::new_checked(&frame[14..]) {
        Ok(pkt) => pkt,
        Err(_) => return,
    };
    let src_ip = ipv6.src_addr();
    if src_ip.is_unicast() && !src_ip.is_unspecified() {
        insert_bounded(&mut *CACHE.lock(), src_ip, src_mac);
    }

    if ipv6.next_header() != smoltcp::wire::IpProtocol::Icmpv6 {
        return;
    }
    let icmp = match Icmpv6Packet::new_checked(ipv6.payload()) {
        Ok(pkt) => pkt,
        Err(_) => return,
    };
    let repr = match Icmpv6Repr::parse(
        &IpAddress::Ipv6(ipv6.src_addr()),
        &IpAddress::Ipv6(ipv6.dst_addr()),
        &icmp,
        &ChecksumCapabilities::default(),
    ) {
        Ok(r) => r,
        Err(_) => return,
    };

    match repr {
        Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
            target_addr,
            lladdr,
            ..
        })
        | Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
            target_addr,
            lladdr,
            ..
        }) => {
            if target_addr.is_unicast() && !target_addr.is_unspecified() {
                insert_bounded(
                    &mut *CACHE.lock(),
                    target_addr,
                    lladdr.unwrap_or(src_mac),
                );
            }
        }
        _ => {}
    }
}

pub fn lookup(dst: Ipv6Address) -> Option<EthernetAddress> {
    let mac = CACHE.lock().get(&dst).copied()?;
    if is_local_mac(mac) {
        return None;
    }
    Some(mac)
}

pub fn clear() {
    CACHE.lock().clear();
}

pub fn get_entries() -> alloc::vec::Vec<(Ipv6Address, EthernetAddress)> {
    CACHE.lock().iter().map(|(&ip, &mac)| (ip, mac)).collect()
}
