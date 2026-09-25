//! Software IPv4 → Ethernet cache fed from every dispatched RX frame.
//! DHCP uses `NetScheme::send` directly; ping uses this to reach the gateway
//! without waiting on smoltcp egress/neighbor state.

use alloc::collections::BTreeMap;
use lock::Mutex;
use smoltcp::wire::{ArpOperation, ArpPacket, ArpRepr, EthernetAddress, Ipv4Address, Ipv4Packet};

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
    LOCAL_MACS.lock().contains(&mac)
}

/// Cap software ARP cache — every RX frame can learn a new entry.
const CACHE_MAX: usize = 512;

/// How long a learned entry stays valid (ms). After this it is treated as stale
/// and re-resolved, so a peer that changed MAC (reboot/re-home) — or a spoofed
/// entry — does not stick forever.
const REACHABLE_MS: u64 = 60_000;

fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
}

/// Monotonic insertion order, which is what eviction needs.
///
/// The learn timestamp is a millisecond count, and a flood of frames learns
/// hundreds of entries inside one millisecond -- exactly the case eviction was
/// written for. With every timestamp equal, `min_by_key` returns the first
/// entry `BTreeMap::iter` yields, which is the numerically smallest address:
/// the very "flush the gateway by flooding spoofed frames" the comment below
/// says this does not do. A counter never ties.
static LEARN_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn next_seq() -> u64 {
    LEARN_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

lazy_static! {
    /// value = (MAC, learn timestamp in ms for TTL, insertion order for LRU).
    static ref CACHE: Mutex<BTreeMap<Ipv4Address, (EthernetAddress, u64, u64)>> =
        Mutex::new(BTreeMap::new());
}

fn insert_bounded(
    map: &mut BTreeMap<Ipv4Address, (EthernetAddress, u64, u64)>,
    ip: Ipv4Address,
    mac: EthernetAddress,
) {
    if map.len() >= CACHE_MAX && !map.contains_key(&ip) {
        // Evict the OLDEST entry (by learn time), not the numerically smallest
        // IP: the latter let an attacker deterministically flush a chosen entry
        // (e.g. the gateway) by flooding spoofed frames with higher source IPs.
        if let Some(old) = map
            .iter()
            .min_by_key(|(_, (_, _, seq))| *seq)
            .map(|(&ip, _)| ip)
        {
            map.remove(&old);
        }
    }
    map.insert(ip, (mac, now_ms(), next_seq()));
}

/// Learn mappings from a complete Ethernet frame (called from `push_packet`).
pub fn learn_from_frame(frame: &[u8]) {
    // Same L2 parse as the other three parsers `push_packet` feeds
    // (`ndp_cache`, `icmp_rx`, `ra`): 14 bytes, or 18 with an 802.1Q tag.
    let Some((l2, ethertype)) = crate::net::packet::eth_l2_header_len(frame) else {
        return;
    };
    let src_mac = EthernetAddress::from_bytes(&frame[6..12]);
    // QEMU slirp DHCP can carry the server IP (10.0.2.2) with our own L2
    // source, and a frame we sent that the link reflects back would teach a
    // peer's IP our own MAC. Neither is a mapping worth a cache slot, whether
    // it arrives as IPv4 or as ARP, so the filter sits here and not in one
    // branch (`ndp_cache` does the same).
    if !src_mac.is_unicast() || is_local_mac(src_mac) {
        return;
    }
    match ethertype {
        0x0800 => {
            let Ok(pkt) = Ipv4Packet::new_checked(&frame[l2..]) else {
                return;
            };
            let src = pkt.src_addr();
            if src.is_unicast() && !src.is_unspecified() {
                insert_bounded(&mut CACHE.lock(), src, src_mac);
            }
        }
        0x0806 => {
            let Ok(arp) = ArpPacket::new_checked(&frame[l2..]) else {
                return;
            };
            if let Ok(ArpRepr::EthernetIpv4 {
                operation,
                source_protocol_addr,
                source_hardware_addr,
                ..
            }) = ArpRepr::parse(&arp)
            {
                // `is_unicast` is what keeps an RFC 5227 ARP probe out: every
                // DHCP client on the link sends one, with a sender protocol
                // address of 0.0.0.0, and smoltcp counts the unspecified
                // address as neither unicast nor broadcast nor multicast.
                if matches!(operation, ArpOperation::Request | ArpOperation::Reply)
                    && source_protocol_addr.is_unicast()
                {
                    insert_bounded(
                        &mut CACHE.lock(),
                        source_protocol_addr,
                        source_hardware_addr,
                    );
                }
            }
        }
        _ => {}
    }
}

pub fn lookup(dst: Ipv4Address) -> Option<EthernetAddress> {
    let mut cache = CACHE.lock();
    let (mac, ts, _) = *cache.get(&dst)?;
    // Expire stale entries so a changed/spoofed MAC is re-resolved.
    if now_ms().saturating_sub(ts) > REACHABLE_MS {
        cache.remove(&dst);
        return None;
    }
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
        insert_bounded(&mut CACHE.lock(), dst, mac);
    }
}

pub fn get_entries() -> alloc::vec::Vec<(Ipv4Address, EthernetAddress)> {
    CACHE
        .lock()
        .iter()
        .map(|(&ip, &(mac, _, _))| (ip, mac))
        .collect()
}

#[cfg(test)]
mod tests {
    //! The software ARP cache, which is what `mod.rs` asks for the next-hop
    //! MAC before it hand-builds an IPv4 frame. It is fed from `push_packet`
    //! alongside `ndp_cache`, `icmp_rx` and `ra`; those three parsed the L2
    //! header the same way and this one did not.

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::net::NET_TEST_LOCK as LOCK;

    const PEER: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const OURS: [u8; 6] = [0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc];

    fn mac(bytes: [u8; 6]) -> EthernetAddress {
        EthernetAddress(bytes)
    }

    /// An Ethernet frame, with one 802.1Q tag when `vlan` is set.
    fn eth(src: [u8; 6], ethertype: u16, vlan: bool, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0xffu8; 6];
        f.extend_from_slice(&src);
        if vlan {
            f.extend_from_slice(&0x8100u16.to_be_bytes());
            f.extend_from_slice(&0x0064u16.to_be_bytes()); // PCP/DEI/VID 100
        }
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(payload);
        // Pad to the 60-byte Ethernet minimum, as a real NIC delivers it.
        while f.len() < 60 {
            f.push(0);
        }
        f
    }

    /// A minimal IPv4 datagram (no payload) from `src`.
    fn ipv4(src: Ipv4Address) -> Vec<u8> {
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&20u16.to_be_bytes());
        ip[8] = 64;
        ip[9] = 17; // UDP
        ip[12..16].copy_from_slice(src.as_bytes());
        ip[16..20].copy_from_slice(&[10, 0, 2, 15]);
        ip
    }

    fn arp(op: u16, sha: [u8; 6], spa: Ipv4Address) -> Vec<u8> {
        let mut a = vec![0u8; 28];
        a[..2].copy_from_slice(&1u16.to_be_bytes()); // Ethernet
        a[2..4].copy_from_slice(&0x0800u16.to_be_bytes());
        a[4] = 6;
        a[5] = 4;
        a[6..8].copy_from_slice(&op.to_be_bytes());
        a[8..14].copy_from_slice(&sha);
        a[14..18].copy_from_slice(spa.as_bytes());
        a[24..28].copy_from_slice(&[10, 0, 2, 15]);
        a
    }

    fn fresh() {
        clear();
        refresh_local_macs(Vec::new());
    }

    #[test]
    fn a_vlan_tagged_ipv4_frame_teaches_the_cache_like_an_untagged_one() {
        let _g = LOCK.lock();
        fresh();
        let peer = Ipv4Address::new(10, 0, 2, 2);

        // The untagged frame always worked.
        learn_from_frame(&eth(PEER, 0x0800, false, &ipv4(peer)));
        assert_eq!(lookup(peer), Some(mac(PEER)));

        // The same frame with one 802.1Q tag taught nothing, because this was
        // the only one of the four parsers `push_packet` feeds that did not
        // know about the tag: the EtherType read as 0x8100 and fell off the
        // end of the match. On a tagged link every IPv4 next-hop lookup in
        // `mod.rs` then came back None and no frame could be addressed.
        clear();
        assert_eq!(lookup(peer), None);
        learn_from_frame(&eth(PEER, 0x0800, true, &ipv4(peer)));
        assert_eq!(lookup(peer), Some(mac(PEER)));
        fresh();
    }

    #[test]
    fn a_vlan_tagged_arp_reply_teaches_the_cache_like_an_untagged_one() {
        let _g = LOCK.lock();
        fresh();
        let gw = Ipv4Address::new(192, 168, 1, 1);

        learn_from_frame(&eth(PEER, 0x0806, true, &arp(2, PEER, gw)));
        assert_eq!(lookup(gw), Some(mac(PEER)), "tagged ARP reply");

        clear();
        learn_from_frame(&eth(PEER, 0x0806, true, &arp(1, PEER, gw)));
        assert_eq!(lookup(gw), Some(mac(PEER)), "tagged ARP request");
        fresh();
    }

    #[test]
    fn an_arp_probe_with_an_unspecified_sender_is_not_learned() {
        let _g = LOCK.lock();
        fresh();
        // RFC 5227 duplicate-address detection: every DHCP client on the link
        // sends an ARP request whose sender protocol address is 0.0.0.0, which
        // is not an address any lookup can ask for. Both ways in refuse it, and
        // what refuses it is `is_unicast` -- smoltcp counts the unspecified
        // address as neither unicast nor broadcast nor multicast -- so this
        // test is what keeps a looser guard from letting it through.
        learn_from_frame(&eth(
            PEER,
            0x0806,
            false,
            &arp(1, PEER, Ipv4Address::UNSPECIFIED),
        ));
        assert_eq!(lookup(Ipv4Address::UNSPECIFIED), None);
        assert!(get_entries().is_empty(), "{:?}", get_entries());

        // The same rule on the way in through `insert`.
        insert(Ipv4Address::UNSPECIFIED, mac(PEER));
        assert!(get_entries().is_empty(), "{:?}", get_entries());
        fresh();
    }

    #[test]
    fn a_frame_carrying_our_own_mac_is_refused_on_both_branches() {
        let _g = LOCK.lock();
        fresh();
        refresh_local_macs(vec![mac(OURS)]);
        let peer = Ipv4Address::new(10, 0, 2, 2);

        // The IPv4 branch has always filtered this (QEMU slirp puts the DHCP
        // server IP behind our own L2 source)...
        learn_from_frame(&eth(OURS, 0x0800, false, &ipv4(peer)));
        assert!(get_entries().is_empty(), "ipv4: {:?}", get_entries());

        // ...and the ARP branch did not, so a reflected or hairpinned ARP of
        // ours took a slot for a mapping `lookup` then refuses anyway.
        learn_from_frame(&eth(OURS, 0x0806, false, &arp(2, OURS, peer)));
        assert!(get_entries().is_empty(), "arp: {:?}", get_entries());
        fresh();
    }

    #[test]
    fn a_short_or_malformed_frame_is_refused_without_panicking() {
        let _g = LOCK.lock();
        fresh();
        let peer = Ipv4Address::new(10, 0, 2, 2);
        for n in 0..14 {
            learn_from_frame(&vec![0u8; n]);
        }
        // A tag and nothing behind it.
        learn_from_frame(
            &[0u8; 6]
                .iter()
                .chain(PEER.iter())
                .copied()
                .chain([0x81, 0x00])
                .collect::<Vec<_>>(),
        );
        // An IPv4 header whose total length is below its own header length.
        let mut bad = eth(PEER, 0x0800, false, &ipv4(peer));
        bad[16..18].copy_from_slice(&8u16.to_be_bytes());
        learn_from_frame(&bad);
        // An ARP body truncated to 20 of its 28 bytes.
        learn_from_frame(&{
            let mut f = eth(PEER, 0x0806, false, &arp(2, PEER, peer));
            f.truncate(14 + 20);
            f
        });
        assert!(get_entries().is_empty(), "{:?}", get_entries());
        fresh();
    }

    #[test]
    fn the_cache_evicts_the_entry_learned_first_not_the_lowest_address() {
        let mut map = BTreeMap::new();
        // The oldest entry in the cache is some peer we have not heard from in
        // a while, at a high address.
        let stale = Ipv4Address::new(10, 255, 255, 254);
        insert_bounded(&mut map, stale, mac(PEER));
        // The gateway is almost always a low address, learned after it.
        let gateway = Ipv4Address::new(10, 0, 0, 1);
        insert_bounded(&mut map, gateway, mac(PEER));
        // Then a flood of spoofed frames with higher source IPs fills the rest.
        for i in 2..CACHE_MAX {
            let ip = Ipv4Address::new(10, (i >> 8) as u8, (i & 0xff) as u8, 2);
            insert_bounded(&mut map, ip, mac(OURS));
        }
        assert_eq!(map.len(), CACHE_MAX);
        assert!(map.contains_key(&gateway));

        // One more entry has to evict the oldest. Ordering by the learn
        // timestamp cannot tell these apart: it is a millisecond count and the
        // whole flood fits inside one tick, so every entry ties and
        // `BTreeMap::iter` hands `min_by_key` the numerically smallest address
        // -- the gateway. Ordering by insertion sequence never ties.
        let flood = Ipv4Address::new(10, 255, 255, 3);
        insert_bounded(&mut map, flood, mac(OURS));
        assert_eq!(map.len(), CACHE_MAX);
        assert!(
            map.contains_key(&gateway),
            "a flood of higher source IPs must not flush the gateway"
        );
        assert!(!map.contains_key(&stale), "the entry learned first goes");
        assert!(map.contains_key(&flood), "the newest is in");

        // Refreshing an entry already present evicts nobody, and moves it to
        // the back of the eviction order.
        let len = map.len();
        insert_bounded(&mut map, gateway, mac(OURS));
        assert_eq!(map.len(), len);
        assert_eq!(map.get(&gateway).map(|&(m, _, _)| m), Some(mac(OURS)));
        insert_bounded(&mut map, Ipv4Address::new(172, 16, 0, 1), mac(PEER));
        assert!(
            map.contains_key(&gateway),
            "refreshed, so no longer the oldest"
        );
    }

    #[test]
    fn lookup_refuses_an_entry_that_resolves_to_one_of_our_own_macs() {
        let _g = LOCK.lock();
        fresh();
        let peer = Ipv4Address::new(10, 0, 2, 2);
        insert(peer, mac(OURS));
        assert_eq!(lookup(peer), Some(mac(OURS)));
        // Once the NIC list knows that MAC is ours, sending there would be a
        // loop, so the answer has to disappear.
        refresh_local_macs(vec![mac(OURS)]);
        assert_eq!(lookup(peer), None);
        // ...but the entry itself is untouched, unlike the TTL path.
        assert_eq!(get_entries().len(), 1);
        remove(peer);
        assert!(get_entries().is_empty());
        fresh();
    }
}
