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
    LOCAL_MACS.lock().contains(&mac)
}

const CACHE_MAX: usize = 512;

/// How long a learned entry stays valid (ms) before it is re-resolved.
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
    static ref CACHE: Mutex<BTreeMap<Ipv6Address, (EthernetAddress, u64, u64)>> =
        Mutex::new(BTreeMap::new());
}

fn insert_bounded(
    map: &mut BTreeMap<Ipv6Address, (EthernetAddress, u64, u64)>,
    ip: Ipv6Address,
    mac: EthernetAddress,
) {
    if map.len() >= CACHE_MAX && !map.contains_key(&ip) {
        // Evict the OLDEST entry (by learn time), not the numerically smallest
        // IP, so an attacker cannot deterministically flush a chosen entry.
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
    // Ethernet header, optionally one 802.1Q VLAN tag, then IPv6.
    let Some((l2, ethertype)) = crate::net::packet::eth_l2_header_len(frame) else {
        return;
    };
    let src_mac = EthernetAddress::from_bytes(&frame[6..12]);
    if !src_mac.is_unicast() || is_local_mac(src_mac) {
        return;
    }
    if ethertype != 0x86dd {
        return;
    }
    let ipv6 = match Ipv6Packet::new_checked(&frame[l2..]) {
        Ok(pkt) => pkt,
        Err(_) => return,
    };
    let src_ip = ipv6.src_addr();
    if src_ip.is_unicast() && !src_ip.is_unspecified() {
        insert_bounded(&mut CACHE.lock(), src_ip, src_mac);
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
        // Neighbor Advertisement: `target_addr` is the address the sender OWNS
        // and `lladdr` is its Target Link-Layer Address, so `target_addr ->
        // lladdr` is the correct mapping.
        Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
            target_addr,
            lladdr,
            ..
        }) if target_addr.is_unicast() && !target_addr.is_unspecified() => {
            insert_bounded(&mut CACHE.lock(), target_addr, lladdr.unwrap_or(src_mac));
        }
        // Neighbor Solicitation: `target_addr` is the address being QUERIED (it
        // is NOT owned by the sender) and `lladdr` is the sender's Source
        // Link-Layer Address. Learning `target_addr -> lladdr` here would map the
        // queried IP to the querier's MAC -- a bogus/poisoned entry that redirects
        // traffic for `target_addr` to whoever solicited it, and corrupts the
        // cache even under benign NS traffic. The correct `src_ip -> src_mac`
        // mapping is already learned from the IPv6 header above.
        _ => {}
    }
}

pub fn lookup(dst: Ipv6Address) -> Option<EthernetAddress> {
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

pub fn clear() {
    CACHE.lock().clear();
}

pub fn get_entries() -> alloc::vec::Vec<(Ipv6Address, EthernetAddress)> {
    CACHE
        .lock()
        .iter()
        .map(|(&ip, &(mac, _, _))| (ip, mac))
        .collect()
}

#[cfg(test)]
mod tests {
    //! The IPv6 half of the neighbour cache: same shape as `arp_cache`, and
    //! the two must answer the L2 header the same way because `push_packet`
    //! hands them both the same frame.

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use smoltcp::wire::{Icmpv6Repr, Ipv6Repr, NdiscNeighborFlags, NdiscRepr};

    use crate::net::NET_TEST_LOCK as LOCK;

    const PEER: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const OURS: [u8; 6] = [0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc];

    fn mac(bytes: [u8; 6]) -> EthernetAddress {
        EthernetAddress(bytes)
    }

    fn v6(a: u16, b: u16) -> Ipv6Address {
        Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, a, b)
    }

    /// An Ethernet frame carrying `payload` as IPv6, optionally 802.1Q tagged.
    fn eth6(src_mac_bytes: [u8; 6], vlan: bool, ipv6: &[u8]) -> Vec<u8> {
        let mut f = vec![0x33u8, 0x33, 0, 0, 0, 1];
        f.extend_from_slice(&src_mac_bytes);
        if vlan {
            f.extend_from_slice(&0x8100u16.to_be_bytes());
            f.extend_from_slice(&0x0064u16.to_be_bytes());
        }
        f.extend_from_slice(&0x86ddu16.to_be_bytes());
        f.extend_from_slice(ipv6);
        f
    }

    /// An IPv6 datagram with an arbitrary next header and no payload.
    fn plain_ipv6(src: Ipv6Address, dst: Ipv6Address) -> Vec<u8> {
        let repr = Ipv6Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: smoltcp::wire::IpProtocol::Tcp,
            payload_len: 0,
            hop_limit: 64,
        };
        let mut buf = vec![0u8; repr.buffer_len()];
        repr.emit(&mut Ipv6Packet::new_unchecked(&mut buf));
        buf
    }

    /// An IPv6 datagram carrying one ICMPv6 neighbour-discovery message.
    fn ndisc(src: Ipv6Address, dst: Ipv6Address, ndisc: NdiscRepr<'_>) -> Vec<u8> {
        let icmp = Icmpv6Repr::Ndisc(ndisc);
        let ip = Ipv6Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: smoltcp::wire::IpProtocol::Icmpv6,
            payload_len: icmp.buffer_len(),
            hop_limit: 255,
        };
        let mut buf = vec![0u8; ip.buffer_len() + icmp.buffer_len()];
        let mut pkt = Ipv6Packet::new_unchecked(&mut buf);
        ip.emit(&mut pkt);
        icmp.emit(
            &IpAddress::Ipv6(src),
            &IpAddress::Ipv6(dst),
            &mut Icmpv6Packet::new_unchecked(pkt.payload_mut()),
            &ChecksumCapabilities::default(),
        );
        buf
    }

    fn fresh() {
        clear();
        refresh_local_macs(Vec::new());
    }

    #[test]
    fn a_vlan_tagged_ipv6_frame_teaches_the_cache_like_an_untagged_one() {
        let _g = LOCK.lock();
        fresh();
        let peer = v6(0, 1);
        for vlan in [false, true] {
            clear();
            learn_from_frame(&eth6(PEER, vlan, &plain_ipv6(peer, v6(0, 2))));
            assert_eq!(lookup(peer), Some(mac(PEER)), "vlan={}", vlan);
        }
        fresh();
    }

    #[test]
    fn a_neighbor_advertisement_teaches_the_address_its_sender_owns() {
        let _g = LOCK.lock();
        fresh();
        let router = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let owned = v6(0, 0xaa);
        let advert_mac = [0x52, 0x54, 0x00, 0x99, 0x88, 0x77];
        // NA: `target_addr` is the address the sender owns and `lladdr` its
        // Target Link-Layer Address, so the mapping is target -> lladdr, not
        // target -> the frame's source MAC.
        learn_from_frame(&eth6(
            PEER,
            true,
            &ndisc(
                router,
                v6(0, 2),
                NdiscRepr::NeighborAdvert {
                    flags: NdiscNeighborFlags::SOLICITED | NdiscNeighborFlags::OVERRIDE,
                    target_addr: owned,
                    lladdr: Some(mac(advert_mac)),
                },
            ),
        ));
        assert_eq!(lookup(owned), Some(mac(advert_mac)));
        // The IPv6 header still teaches its own source.
        assert_eq!(lookup(router), Some(mac(PEER)));
        fresh();
    }

    #[test]
    fn a_neighbor_solicitation_never_teaches_the_address_it_is_asking_about() {
        let _g = LOCK.lock();
        fresh();
        let querier = v6(0, 7);
        let queried = v6(0, 0xaa);
        // NS: `target_addr` is the address being LOOKED UP -- the sender does
        // not own it -- and `lladdr` is the sender's own Source Link-Layer
        // Address. Learning target -> lladdr here would point every packet for
        // `queried` at whoever asked about it, which is a cache poisoning any
        // host on the link can do with one legal solicitation, and which
        // ordinary NS traffic would do by accident.
        learn_from_frame(&eth6(
            PEER,
            false,
            &ndisc(
                querier,
                queried.solicited_node(),
                NdiscRepr::NeighborSolicit {
                    target_addr: queried,
                    lladdr: Some(mac(PEER)),
                },
            ),
        ));
        assert_eq!(lookup(queried), None, "the queried address is not learned");
        assert_eq!(lookup(querier), Some(mac(PEER)), "only the real source is");
        fresh();
    }

    #[test]
    fn frames_that_are_not_ipv6_or_that_are_ours_teach_nothing() {
        let _g = LOCK.lock();
        fresh();
        refresh_local_macs(vec![mac(OURS)]);
        let peer = v6(0, 1);

        // Our own MAC, tagged or not.
        for vlan in [false, true] {
            learn_from_frame(&eth6(OURS, vlan, &plain_ipv6(peer, v6(0, 2))));
        }
        // An IPv4 frame, an unknown EtherType, a tag with nothing behind it,
        // a truncated IPv6 header and a run of short frames.
        let mut v4 = eth6(PEER, false, &plain_ipv6(peer, v6(0, 2)));
        v4[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        learn_from_frame(&v4);
        let mut unknown = v4.clone();
        unknown[12..14].copy_from_slice(&0x88f7u16.to_be_bytes());
        learn_from_frame(&unknown);
        learn_from_frame(&eth6(PEER, false, &plain_ipv6(peer, v6(0, 2)))[..40]);
        for n in 0..20 {
            learn_from_frame(&vec![0u8; n]);
        }
        assert!(get_entries().is_empty(), "{:?}", get_entries());

        // A broadcast/multicast source is never a neighbour either.
        learn_from_frame(&eth6([0xff; 6], false, &plain_ipv6(peer, v6(0, 2))));
        assert!(get_entries().is_empty(), "{:?}", get_entries());
        fresh();
    }

    #[test]
    fn the_unspecified_source_of_duplicate_address_detection_is_not_learned() {
        let _g = LOCK.lock();
        fresh();
        // RFC 4862 DAD sends its solicitation from `::`, which is not an
        // address anyone can be reached at.
        learn_from_frame(&eth6(
            PEER,
            false,
            &plain_ipv6(Ipv6Address::UNSPECIFIED, v6(0, 2)),
        ));
        assert_eq!(lookup(Ipv6Address::UNSPECIFIED), None);
        assert!(get_entries().is_empty(), "{:?}", get_entries());
        fresh();
    }

    #[test]
    fn the_cache_evicts_the_entry_learned_first_not_the_lowest_address() {
        let mut map = BTreeMap::new();
        let stale = v6(0xffff, 0xfffe);
        insert_bounded(&mut map, stale, mac(PEER));
        // The router is the low address, and the one an attacker wants gone.
        let router = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        insert_bounded(&mut map, router, mac(PEER));
        for i in 2..CACHE_MAX {
            insert_bounded(&mut map, v6(1, i as u16), mac(OURS));
        }
        assert_eq!(map.len(), CACHE_MAX);

        // Same reasoning as the IPv4 cache: the learn timestamp is a
        // millisecond count, a flood ties every one of them, and a tie sends
        // `min_by_key` to the numerically smallest address.
        insert_bounded(&mut map, v6(0xffff, 3), mac(OURS));
        assert_eq!(map.len(), CACHE_MAX);
        assert!(
            map.contains_key(&router),
            "a flood must not flush the router"
        );
        assert!(!map.contains_key(&stale), "the entry learned first goes");
    }

    #[test]
    fn lookup_refuses_an_entry_pointing_at_one_of_our_own_macs() {
        let _g = LOCK.lock();
        fresh();
        let peer = v6(0, 1);
        learn_from_frame(&eth6(OURS, false, &plain_ipv6(peer, v6(0, 2))));
        assert_eq!(lookup(peer), Some(mac(OURS)), "no NIC list yet");
        refresh_local_macs(vec![mac(OURS)]);
        assert_eq!(lookup(peer), None, "sending there would be a loop");
        fresh();
    }
}
