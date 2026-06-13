//! Software IPv4 → Ethernet cache fed from every dispatched RX frame.
//! DHCP uses `NetScheme::send` directly; ping uses this to reach the gateway
//! without waiting on smoltcp egress/neighbor state.

use alloc::collections::BTreeMap;
use lock::Mutex;
use smoltcp::wire::{ArpOperation, ArpPacket, ArpRepr, EthernetAddress, Ipv4Address};

use lazy_static::lazy_static;

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

/// Cap software ARP cache — every RX frame can learn a new entry.
const CACHE_MAX: usize = 512;

lazy_static! {
    static ref CACHE: Mutex<BTreeMap<Ipv4Address, EthernetAddress>> = Mutex::new(BTreeMap::new());
}

fn insert_bounded(
    map: &mut BTreeMap<Ipv4Address, EthernetAddress>,
    ip: Ipv4Address,
    mac: EthernetAddress,
) {
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
    if !src_mac.is_unicast() {
        return;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    match ethertype {
        0x0800 => {
            if frame.len() < 34 {
                return;
            }
            let ihl = ((frame[14] & 0x0f) as usize) * 4;
            if frame.len() < 14 + ihl + 4 {
                return;
            }
            let src = Ipv4Address::from_bytes(&frame[26..30]);
            // QEMU slirp DHCP can carry server IP (10.0.2.2) with our own L2 source — skip.
            if src.is_unicast() && !src.is_unspecified() && !is_local_mac(src_mac) {
                insert_bounded(&mut *CACHE.lock(), src, src_mac);
            }
        }
        0x0806 => {
            if frame.len() < 42 {
                return;
            }
            let arp = ArpPacket::new_unchecked(&frame[14..]);
            if let Ok(repr) = ArpRepr::parse(&arp) {
                if let ArpRepr::EthernetIpv4 {
                    operation,
                    source_protocol_addr,
                    source_hardware_addr,
                    ..
                } = repr
                {
                    if matches!(operation, ArpOperation::Request | ArpOperation::Reply)
                        && source_protocol_addr.is_unicast()
                    {
                        insert_bounded(
                            &mut *CACHE.lock(),
                            source_protocol_addr,
                            source_hardware_addr,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

pub fn lookup(dst: Ipv4Address) -> Option<EthernetAddress> {
    let mac = CACHE.lock().get(&dst).copied()?;
    if is_local_mac(mac) {
        return None;
    }
    Some(mac)
}

pub fn remove(dst: Ipv4Address) {
    CACHE.lock().remove(&dst);
}

pub fn clear() {
    CACHE.lock().clear();
}

pub fn insert(dst: Ipv4Address, mac: EthernetAddress) {
    if dst.is_unicast() {
        insert_bounded(&mut *CACHE.lock(), dst, mac);
    }
}

pub fn get_entries() -> alloc::vec::Vec<(Ipv4Address, EthernetAddress)> {
    CACHE.lock().iter().map(|(&ip, &mac)| (ip, mac)).collect()
}
