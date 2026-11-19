//! The neighbours of the card, said to the kernel, which shows them in
//! `/proc/net/arp` and to `ip neigh`: the machines of the link whose
//! hardware address the stack has learnt, of IPv4 by ARP and of IPv6 by
//! neighbour discovery.
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
//! Its sock field is the card's number, as in a `CARD`. All of them each
//! time, and only when there is news of them: one more, one at another
//! hardware address, or one heard from again when half its time was gone
//! (smoltcp's `NeighborCache::changes`). So a neighbour that keeps
//! talking costs a message every half minute, and one that went quiet
//! none: the kernel counts its time down by itself.
//!
//! And one written by hand (`ip neigh replace ... nud permanent`, `arp
//! -s`), or forgotten (`ip neigh del`, `arp -d`), which the kernel asks
//! of the card:
//!
//! ```text
//! NEIGHBOUR (kernel to stack) 16  [1] family: 4 or 6
//!                                 [1] 1 to write it, 0 to forget it
//!                                 [6] hardware address; zeroes to forget
//!                                 [16] address; of IPv4 in the first four
//! ```
//!
//! One written by hand is not asked for on the link, does not grow
//! old, and stays as written whatever is heard of it. Forgetting any,
//! written or learnt, has the next packet for it ask again.

use smoltcp::phy::Device;
use smoltcp::wire::{EthernetAddress, IpAddress, Ipv4Address, Ipv6Address};

use super::{parse_header, Refused, Stack, HEADER_LEN};

/// The operations of the messages.
pub const OP_NEIGH: u32 = 39;
pub const OP_NEIGHBOUR: u32 = 40;

/// What a `NEIGHBOUR` says past its header.
pub const NEIGHBOUR_LEN: usize = 24;

/// What the kernel asks of a card with a `NEIGHBOUR`: that this
/// address is at this hardware address, or that it is forgotten.
pub type Wish = (IpAddress, Option<EthernetAddress>);

/// What `m` asks, and of which card, when it is a `NEIGHBOUR`: `Err`
/// for one that names no machine.
pub fn wished(m: &[u8]) -> Option<Result<(u64, Wish), Refused>> {
    let (op, card) = parse_header(m)?;
    if op != OP_NEIGHBOUR {
        return None;
    }
    let said = &m[HEADER_LEN..];
    let wish = || {
        if said.len() != NEIGHBOUR_LEN || said[1] > 1 {
            return None;
        }
        let address = match said[0] {
            4 if said[12..] == [0; 12] => IpAddress::Ipv4(Ipv4Address::from_bytes(&said[8..12])),
            6 => IpAddress::Ipv6(Ipv6Address::from_bytes(&said[8..])),
            _ => return None,
        };
        let hardware = EthernetAddress::from_bytes(&said[2..8]);
        let at = match said[1] {
            1 if hardware.is_unicast() => Some(hardware),
            0 if hardware == EthernetAddress([0; 6]) => None,
            _ => return None,
        };
        address.is_unicast().then_some((card, (address, at)))
    };
    Some(wish().ok_or(Refused::Malformed))
}

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

    /// Do what a `NEIGHBOUR` asks. Refused when as many as a `NEIGH`
    /// can say are written by hand already; forgetting one that is not
    /// known is done.
    pub fn neighbour(&mut self, (address, at): Wish) -> Result<(), Refused> {
        match at {
            Some(at) => {
                let room = self.iface.fixed_neighbors() < NEIGH_ENTRIES_MAX / 2;
                let known = self
                    .iface
                    .neighbors()
                    .any(|(a, _, until)| a == address && until.is_none());
                if !(known || room) || !self.iface.fix_neighbor(address, at) {
                    return Err(Refused::Full);
                }
            }
            None => {
                self.iface.forget_neighbor(&address);
            }
        }
        Ok(())
    }

    /// The `NEIGH` that says them, as they are at the last poll.
    pub fn neigh_message(&self) -> Vec<u8> {
        let mut all: Vec<_> = self.iface.neighbors().collect();
        if all.len() > NEIGH_ENTRIES_MAX {
            // The ones written by hand, then the ones with most time left.
            all.sort_by_key(|(_, _, until)| {
                core::cmp::Reverse(until.map_or(i64::MAX, |until| until.total_micros()))
            });
            all.truncate(NEIGH_ENTRIES_MAX);
        }
        let mut m = vec![0u8; HEADER_LEN];
        m[4..8].copy_from_slice(&OP_NEIGH.to_le_bytes());
        for (address, hardware, until) in all {
            let left = match until {
                Some(until) if until > self.now => {
                    (until - self.now).total_millis().min(u32::MAX as u64) as u32
                }
                _ => 0,
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
            m.extend_from_slice(&[family, until.is_none() as u8]);
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
    use smoltcp::wire::Ipv6Address;

    /// A `NEIGHBOUR` for card 0.
    fn ask(family: u8, have: u8, hardware: [u8; 6], address: &[u8]) -> Vec<u8> {
        let mut m = vec![0u8; HEADER_LEN];
        m[4..8].copy_from_slice(&OP_NEIGHBOUR.to_le_bytes());
        m.extend_from_slice(&[family, have]);
        m.extend_from_slice(&hardware);
        let mut sixteen = [0u8; 16];
        sixteen[..address.len()].copy_from_slice(address);
        m.extend_from_slice(&sixteen);
        m
    }

    #[test]
    fn a_neighbour_is_written_by_hand_and_forgotten() {
        let Bench { iface, .. } = Bench::new();
        let mut stack = Stack::new(iface);
        let four = IpAddress::Ipv4(Ipv4Address([10, 0, 9, 77]));
        let mac = [2, 0, 0, 0, 0, 0x77];
        // These bytes are what the kernel writes
        // (`linux-object/src/net/user_stack/neigh.rs`).
        let m = ask(4, 1, mac, &[10, 0, 9, 77]);
        assert_eq!(m.len(), HEADER_LEN + NEIGHBOUR_LEN);
        assert_eq!(
            m[HEADER_LEN..HEADER_LEN + 12],
            [4, 1, 2, 0, 0, 0, 0, 0x77, 10, 0, 9, 77]
        );
        let (card, wish) = wished(&m).unwrap().unwrap();
        assert_eq!((card, wish), (0, (four, Some(EthernetAddress(mac)))));
        assert!(!stack.neigh_news());
        assert_eq!(stack.neighbour(wish), Ok(()));
        // It is news, and said as written by hand, with no time to count.
        assert!(stack.neigh_news());
        let said = stack.neigh_message();
        let entry = &said[HEADER_LEN..];
        assert_eq!(entry.len(), NEIGH_ENTRY_LEN);
        assert_eq!(entry[..2], [4, 1]);
        assert_eq!(entry[2..8], mac);
        assert_eq!(entry[8..12], [0; 4]);
        assert_eq!(entry[12..16], [10, 0, 9, 77]);
        // What is heard of it on the link does not move it.
        let at = Instant::from_millis(5_000);
        stack
            .iface
            .seed_neighbor(four, EthernetAddress([2, 0, 0, 0, 0, 1]), at);
        assert!(!stack.neigh_news());
        assert_eq!(stack.neigh_message()[HEADER_LEN + 2..HEADER_LEN + 8], mac);

        // Forgotten, and that is news too; once more, it is nothing.
        let (_, wish) = wished(&ask(4, 0, [0; 6], &[10, 0, 9, 77]))
            .unwrap()
            .unwrap();
        assert_eq!(wish, (four, None));
        assert_eq!(stack.neighbour(wish), Ok(()));
        assert!(stack.neigh_news());
        assert_eq!(stack.neigh_message().len(), HEADER_LEN);
        assert_eq!(stack.neighbour(wish), Ok(()));
        assert!(!stack.neigh_news());

        // One of IPv6.
        let six: Ipv6Address = "fe80::77".parse().unwrap();
        let (_, wish) = wished(&ask(6, 1, mac, six.as_bytes())).unwrap().unwrap();
        assert_eq!(wish, (IpAddress::Ipv6(six), Some(EthernetAddress(mac))));

        // What names no machine, or says too much or too little.
        for bad in [
            ask(4, 1, [0xff; 6], &[10, 0, 9, 77]),
            ask(4, 1, [1, 0, 0, 0, 0, 1], &[10, 0, 9, 77]),
            ask(4, 1, mac, &[224, 0, 0, 1]),
            ask(4, 1, mac, &[0; 4]),
            ask(4, 0, mac, &[10, 0, 9, 77]),
            ask(4, 2, mac, &[10, 0, 9, 77]),
            ask(5, 1, mac, &[10, 0, 9, 77]),
            ask(4, 1, mac, &[10, 0, 9, 77, 1]),
            ask(
                6,
                1,
                mac,
                "ff02::1".parse::<Ipv6Address>().unwrap().as_bytes(),
            ),
        ] {
            assert_eq!(wished(&bad), Some(Err(Refused::Malformed)));
        }
        let mut short = ask(4, 1, mac, &[10, 0, 9, 77]);
        short.pop();
        assert_eq!(wished(&short), Some(Err(Refused::Malformed)));
        // Another message is not one of these.
        assert_eq!(wished(&stack.neigh_message()), None);

        // As many by hand as fit, and no more; one of them again is not
        // one more.
        let nth = |n: u8| {
            (
                IpAddress::Ipv4(Ipv4Address([10, 0, 9, 100 + n])),
                Some(EthernetAddress(mac)),
            )
        };
        for n in 0..(NEIGH_ENTRIES_MAX / 2) as u8 {
            assert_eq!(stack.neighbour(nth(n)), Ok(()));
        }
        assert_eq!(stack.neighbour(nth(100)), Err(Refused::Full));
        assert_eq!(stack.neighbour(nth(3)), Ok(()));
    }

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
