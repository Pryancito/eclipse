//! The neighbours of the stack's cards: the machines of each link whose
//! hardware address the stack has learnt, by ARP or by the neighbour
//! discovery of IPv6. The kernel has no stack and meets nobody, so the
//! stack says them, and `/proc/net/arp` and `ip neigh` show what it
//! said.
//!
//! ```text
//! NEIGH  (stack to kernel)  16  one after another, to the end:
//!                               [1] family: 4 or 6
//!                               [1] 1 written by hand, 0 learnt
//!                               [6] hardware address
//!                               [4] milliseconds it is still taken as
//!                                   there for; 0: it was, once, or it
//!                                   was written by hand
//!                               [16] address; of IPv4 in the first four
//! ```
//!
//! Its sock field is the card's number, as in a `CARD`. The stack says
//! all of a card's neighbours each time, and only when there is news of
//! them: one more, one at another hardware address, or one heard from
//! again when half its time was gone. Between two of those the kernel
//! counts the time down by itself: each is kept with the moment it stops
//! being taken as there, and shown as reachable until then and as stale
//! after. They are kept where the card's other addresses and routes are
//! (`More`), and go with them when the stack does.
//!
//! One can be written by hand (`ip neigh replace`, `arp -s`) or
//! forgotten (`ip neigh del`, `arp -d`), which is asked of the stack:
//!
//! ```text
//! NEIGHBOUR (kernel to stack) 16  [1] family: 4 or 6
//!                                 [1] 1 to write it, 0 to forget it
//!                                 [6] hardware address; zeroes to forget
//!                                 [16] address; of IPv4 in the first four
//! ```
//!
//! It is shown as asked at once, and as the stack says from its next
//! `NEIGH`, which follows every one of these.

use alloc::vec::Vec;
use zcore_drivers::scheme::NeighbourInfo;
use zcore_drivers::wire::{IpAddress, Ipv4Address, Ipv6Address};

/// The operations of the messages.
pub const OP_NEIGH: u32 = 39;
pub const OP_NEIGHBOUR: u32 = 40;

/// `NEIGHBOUR`: for the card `card`, `address` is at `hardware` by
/// hand, or with none is forgotten.
pub fn neighbour_message(card: u64, address: &IpAddress, hardware: Option<[u8; 6]>) -> Vec<u8> {
    let mut m = Vec::with_capacity(40);
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&OP_NEIGHBOUR.to_le_bytes());
    m.extend_from_slice(&card.to_le_bytes());
    let family = match address {
        IpAddress::Ipv4(_) => 4,
        _ => 6,
    };
    m.extend_from_slice(&[family, hardware.is_some() as u8]);
    m.extend_from_slice(&hardware.unwrap_or_default());
    let mut sixteen = [0u8; 16];
    let said = address.as_bytes();
    sixteen[..said.len()].copy_from_slice(said);
    m.extend_from_slice(&sixteen);
    m
}

/// What each entry of a `NEIGH` takes, and how many it has at most.
pub const NEIGH_ENTRY_LEN: usize = 28;
pub const NEIGH_ENTRIES_MAX: usize = 64;

/// What a `NEIGH` heard at `now`, in milliseconds since boot, says past
/// its header: `None` when it is not whole or names what is no
/// neighbour.
pub fn parse_neigh(said: &[u8], now: u64) -> Option<Vec<NeighbourInfo>> {
    let (entries, rest) = said.as_chunks::<NEIGH_ENTRY_LEN>();
    if !rest.is_empty() || entries.len() > NEIGH_ENTRIES_MAX {
        return None;
    }
    let mut met = Vec::with_capacity(entries.len());
    for entry in entries {
        let address = match (entry[0], entry[1]) {
            (4, 0 | 1) if entry[16..] == [0; 12] => {
                IpAddress::Ipv4(Ipv4Address::from_bytes(&entry[12..16]))
            }
            (6, 0 | 1) => IpAddress::Ipv6(Ipv6Address::from_bytes(&entry[12..])),
            _ => return None,
        };
        let fixed = entry[1] == 1;
        let mut hardware = [0u8; 6];
        hardware.copy_from_slice(&entry[2..8]);
        // A group's address is nobody's, and neither is one of no
        // machine.
        if !address.is_unicast() || hardware[0] & 1 != 0 || hardware == [0; 6] {
            return None;
        }
        let left = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
        let until_ms = match left {
            _ if fixed => NeighbourInfo::FIXED,
            0 => 0,
            left => now.saturating_add(left as u64),
        };
        met.push(NeighbourInfo {
            address,
            hardware,
            until_ms,
        });
    }
    Some(met)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(family: u8, hardware: [u8; 6], left: u32, address: &[u8]) -> Vec<u8> {
        let mut e = alloc::vec![family, 0];
        e.extend_from_slice(&hardware);
        e.extend_from_slice(&left.to_le_bytes());
        e.extend_from_slice(address);
        e.resize(NEIGH_ENTRY_LEN, 0);
        e
    }

    #[test]
    fn a_neighbour_is_written_by_hand_in_these_bytes_and_read_back_as_fixed() {
        const MAC: [u8; 6] = [2, 0, 0, 0, 0, 0x77];
        // The same bytes `tools/eclipse-netstack/src/socks/neigh.rs` reads.
        let four = IpAddress::Ipv4(Ipv4Address::new(10, 0, 9, 77));
        let m = neighbour_message(3, &four, Some(MAC));
        let mut wanted = alloc::vec![0, 0, 0, 0, 40, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0];
        wanted.extend_from_slice(&[4, 1, 2, 0, 0, 0, 0, 0x77, 10, 0, 9, 77]);
        wanted.resize(40, 0);
        assert_eq!(m, wanted);
        let forget = neighbour_message(3, &four, None);
        assert_eq!(forget[16..28], [4, 0, 0, 0, 0, 0, 0, 0, 10, 0, 9, 77]);
        let six = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x77);
        let m = neighbour_message(0, &IpAddress::Ipv6(six), Some(MAC));
        assert_eq!(m[16..18], [6, 1]);
        assert_eq!(m[24..40], *six.as_bytes());

        // What the stack says of one written by hand: no time to count.
        let mut said = entry(4, MAC, 0, &[10, 0, 9, 77]);
        said[1] = 1;
        let met = parse_neigh(&said, 1_000).unwrap();
        assert!(met[0].fixed());
        assert_eq!((met[0].address, met[0].hardware), (four, MAC));
        // And of one it learnt, never that.
        let met = parse_neigh(&entry(4, MAC, u32::MAX, &[10, 0, 9, 77]), u64::MAX - 5).unwrap();
        assert!(!met[0].fixed() || met[0].until_ms == u64::MAX);
        said[1] = 2;
        assert_eq!(parse_neigh(&said, 1_000), None);
    }

    #[test]
    fn what_the_stack_says_of_its_neighbours_is_read_whole_or_not_at_all() {
        const MAC: [u8; 6] = [0x52, 0x55, 10, 0, 2, 2];
        assert_eq!(parse_neigh(&[], 5), Some(Vec::new()));
        let six: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
        let mut said = entry(4, MAC, 60_000, &[10, 0, 2, 2]);
        said.extend(entry(6, MAC, 0, six.as_bytes()));
        assert_eq!(
            parse_neigh(&said, 1_000),
            Some(alloc::vec![
                NeighbourInfo {
                    address: IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2)),
                    hardware: MAC,
                    until_ms: 61_000,
                },
                // It was there, once.
                NeighbourInfo {
                    address: IpAddress::Ipv6(six),
                    hardware: MAC,
                    until_ms: 0,
                },
            ])
        );
        // Not whole.
        assert_eq!(parse_neigh(&said[..said.len() - 1], 0), None);
        // No family, a group, nobody's hardware address, an address of
        // IPv4 that goes on past its four bytes, and too many.
        assert_eq!(parse_neigh(&entry(5, MAC, 1, &[10, 0, 2, 2]), 0), None);
        assert_eq!(parse_neigh(&entry(4, MAC, 1, &[224, 0, 0, 1]), 0), None);
        assert_eq!(parse_neigh(&entry(4, [0; 6], 1, &[10, 0, 2, 2]), 0), None);
        assert_eq!(
            parse_neigh(&entry(4, [0xff; 6], 1, &[10, 0, 2, 2]), 0),
            None
        );
        assert_eq!(parse_neigh(&entry(4, MAC, 1, &[10, 0, 2, 2, 9]), 0), None);
        let many = entry(4, MAC, 1, &[10, 0, 2, 2]).repeat(NEIGH_ENTRIES_MAX + 1);
        assert_eq!(parse_neigh(&many, 0), None);
    }
}
