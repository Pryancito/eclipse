//! The neighbours of the card, said to the kernel, which shows them in
//! `/proc/net/arp` and to `ip neigh`: the machines of the link whose
//! hardware address the stack has learnt, of IPv4 by ARP and of IPv6 by
//! neighbour discovery.
//!
//! ```text
//! NEIGH  (stack to kernel)  16  one after another, to the end:
//!                               [1] family: 4 or 6
//!                               [1] zero
//!                               [6] hardware address
//!                               [4] milliseconds it is still taken as
//!                                   there for; 0: it was, once
//!                               [16] address; of IPv4 in the first four
//! ```
//!
//! Its sock field is the card's number, as in a `CARD`. All of them each
//! time, and only when there is news of them: one more, one at another
//! hardware address, or one heard from again when half its time was gone
//! (smoltcp's `NeighborCache::changes`). So a neighbour that keeps
//! talking costs a message every half minute, and one that went quiet
//! none: the kernel counts its time down by itself.

use smoltcp::phy::Device;
use smoltcp::wire::IpAddress;

use super::{Stack, HEADER_LEN};

/// The operation of the message.
pub const OP_NEIGH: u32 = 39;

/// What each entry of a `NEIGH` takes, and how many it has at most: the
/// ones with the most time left, when there are more.
pub const NEIGH_ENTRY_LEN: usize = 28;
pub const NEIGH_ENTRIES_MAX: usize = 64;

impl<D: for<'d> Device<'d> + crate::port::Listened> Stack<D> {
    /// Whether there is news of the neighbours since this was last
    /// asked.
    pub fn neigh_news(&mut self) -> bool {
        let now = self.iface.neighbors_changes();
        core::mem::replace(&mut self.neigh_said, now) != now
    }

    /// The `NEIGH` that says them, as they are at the last poll.
    pub fn neigh_message(&self) -> Vec<u8> {
        let mut all: Vec<_> = self.iface.neighbors().collect();
        if all.len() > NEIGH_ENTRIES_MAX {
            all.sort_by_key(|(_, _, until)| core::cmp::Reverse(*until));
            all.truncate(NEIGH_ENTRIES_MAX);
        }
        let mut m = vec![0u8; HEADER_LEN];
        m[4..8].copy_from_slice(&OP_NEIGH.to_le_bytes());
        for (address, hardware, until) in all {
            let left = if until > self.now {
                (until - self.now).total_millis().min(u32::MAX as u64) as u32
            } else {
                0
            };
            let mut sixteen = [0u8; 16];
            let family = match address {
                IpAddress::Ipv4(address) => {
                    sixteen[..4].copy_from_slice(address.as_bytes());
                    4
                }
                IpAddress::Ipv6(address) => {
                    sixteen.copy_from_slice(address.as_bytes());
                    6
                }
                _ => continue,
            };
            m.extend_from_slice(&[family, 0]);
            m.extend_from_slice(hardware.as_bytes());
            m.extend_from_slice(&left.to_le_bytes());
            m.extend_from_slice(&sixteen);
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::*;
    use crate::socks::parse_header;
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetAddress, Ipv4Address, Ipv6Address};

    #[test]
    fn the_neighbours_are_said_when_there_is_news_of_them() {
        let Bench { iface, .. } = Bench::new();
        let mut stack = Stack::new(iface);
        // Nobody yet, and that is no news.
        assert!(!stack.neigh_news());
        assert_eq!(stack.neigh_message().len(), HEADER_LEN);

        let four = IpAddress::Ipv4(Ipv4Address([10, 0, 9, 2]));
        let mac = EthernetAddress([2, 0, 0, 0, 0, 9]);
        let at = |ms| Instant::from_millis(ms);
        stack.iface.seed_neighbor(four, mac, at(1_000));
        stack.now = at(1_000);
        assert!(stack.neigh_news());
        assert!(!stack.neigh_news(), "said once");
        let m = stack.neigh_message();
        assert_eq!(parse_header(&m), Some((OP_NEIGH, 0)));
        assert_eq!(m.len(), HEADER_LEN + NEIGH_ENTRY_LEN);
        let entry = &m[HEADER_LEN..];
        assert_eq!(entry[..2], [4, 0]);
        assert_eq!(entry[2..8], [2, 0, 0, 0, 0, 9]);
        assert_eq!(entry[8..12], 60_000u32.to_le_bytes());
        assert_eq!(entry[12..16], [10, 0, 9, 2]);
        assert_eq!(entry[16..28], [0; 12]);

        // It keeps talking: not news every time, but once half its time
        // is gone.
        stack.iface.seed_neighbor(four, mac, at(20_000));
        assert!(!stack.neigh_news());
        stack.iface.seed_neighbor(four, mac, at(55_000));
        assert!(stack.neigh_news());
        // At another hardware address it is news at once.
        let moved = EthernetAddress([2, 0, 0, 0, 0, 7]);
        stack.iface.seed_neighbor(four, moved, at(56_000));
        assert!(stack.neigh_news());

        // One of IPv6 beside it, and the first one's time run out.
        let six: Ipv6Address = "fe80::2".parse().unwrap();
        stack
            .iface
            .seed_neighbor(IpAddress::Ipv6(six), mac, at(100_000));
        stack.now = at(120_000);
        assert!(stack.neigh_news());
        let m = stack.neigh_message();
        assert_eq!(m.len(), HEADER_LEN + 2 * NEIGH_ENTRY_LEN);
        let (first, second) = m[HEADER_LEN..].split_at(NEIGH_ENTRY_LEN);
        assert_eq!(first[..2], [4, 0]);
        assert_eq!(first[2..8], [2, 0, 0, 0, 0, 7]);
        assert_eq!(first[8..12], [0; 4], "it was there, once");
        assert_eq!(second[..2], [6, 0]);
        assert_eq!(second[8..12], 40_000u32.to_le_bytes());
        assert_eq!(second[12..28], *six.as_bytes());
    }
}
