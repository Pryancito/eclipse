//! The other cards of the stack that runs as a process.
//!
//! The stack is told to the kernel with one card, the one it gives the
//! kernel its sockets with (`eclipse_netstack_serve`), and that card is
//! the stack's address here. A machine with more cards has a stack for
//! each in the one process (`tools/eclipse-netstack`, `cards`), and each
//! of the others is said with a message of its own once it has an
//! address, and again when that changes:
//!
//! ```text
//! CARD  (stack to kernel)  8  u64 card  16  [u8; 6] MAC  22  u8 prefix  23  u8 zero
//!                          24  [u8; 4] address  28  [u8; 4] gateway
//! ```
//!
//! `card` is a number of the stack's for that card, never 0, which is
//! the card it serves with. No address (all zero) is a card that lost
//! its own; no gateway is all zero. What a card has carried (`COUNTS`)
//! and what it has of IPv6 (`SIX`) are said with the same number where
//! a socket's would be, and 0 there is the first card, as it was.
//!
//! What the kernel does with it: on a boot where every program's sockets
//! are the stack's, the card is one more interface of the machine, with
//! its address, its gateway, its counts and its IPv6 addresses, under
//! the name it had while the kernel drove it. Its address is this
//! machine's, like the first card's: what is sent there is for another
//! program here, and a socket can be bound to it. And a connection, or a
//! connected datagram socket, to a machine on that card's network says
//! that card's address as its own, which is the one it leaves by. The
//! cards go with the stack; the next one says its own.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryInto;
use kernel_hal::drivers::net::stack_card::{Carried, Sixes, StackCard};
use kernel_hal::drivers::prelude::Device;
use kernel_hal::drivers::scheme::NetStats;
use kernel_hal::sync::Mutex;
use log::warn;
use smoltcp::wire::{Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};

use super::{header, StackLink, COUNTS};

/// The operation of the message.
pub const OP_CARD: u32 = 31;
/// What a `CARD` says past its header.
const CARD_LEN: usize = 16;

/// What a `CARD` says of a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Said {
    pub mac: [u8; 6],
    /// The card's address and its network, or `None` when it has lost
    /// them.
    pub address: Option<Ipv4Cidr>,
    pub gateway: Option<Ipv4Address>,
}

/// `CARD`: the stack's card `card` is this.
pub fn card_message(card: u64, said: &Said) -> Vec<u8> {
    let mut m = header(OP_CARD, card, CARD_LEN);
    m.extend_from_slice(&said.mac);
    m.extend_from_slice(&[said.address.map_or(0, |a| a.prefix_len()), 0]);
    m.extend_from_slice(&said.address.map_or([0; 4], |a| a.address().0));
    m.extend_from_slice(&said.gateway.map_or([0; 4], |g| g.0));
    m
}

/// What a `CARD` says, past its header: `None` when it is not whole or
/// names what no card has: no MAC, an address that is no machine's, a
/// prefix of more bits than an address.
pub fn parse_card(said: &[u8]) -> Option<Said> {
    let said: &[u8; CARD_LEN] = said.get(..CARD_LEN)?.try_into().ok()?;
    let mac: [u8; 6] = said[..6].try_into().ok()?;
    let prefix = said[6];
    let address = Ipv4Address::from_bytes(&said[8..12]);
    let gateway = Ipv4Address::from_bytes(&said[12..16]);
    if mac == [0; 6] || prefix > 32 {
        return None;
    }
    let address = match (address.is_unspecified(), prefix) {
        (true, _) => None,
        (false, 0) => return None,
        (false, _) if !address.is_unicast() || address.is_loopback() => return None,
        (false, _) => Some(Ipv4Cidr::new(address, prefix)),
    };
    Some(Said {
        mac,
        address,
        gateway: (!gateway.is_unspecified()).then_some(gateway),
    })
}

/// A card of the stack's that is not its first, as it said it last.
struct Other {
    said: Said,
    /// The last `COUNTS` of it.
    counted: [u64; COUNTS],
    /// The interface that shows it, where the machine's are the stack's.
    shown: Option<Arc<StackCard>>,
}

/// What is kept of a card through the stacks that have it: its name,
/// what it has carried, and where its IPv6 addresses are said.
struct Kept {
    name: Option<String>,
    carried: Arc<Carried>,
    sixes: Arc<Sixes>,
}

/// The other cards of the stack that serves now, by the stack's numbers,
/// and what is kept of each for the next.
#[derive(Default)]
pub(super) struct Others {
    now: Mutex<BTreeMap<u64, Other>>,
    kept: Mutex<BTreeMap<u64, Kept>>,
}

/// The names the kernel's cards had when they were lent, by their MAC:
/// what a card of the stack's is shown as.
static LENT: Mutex<Vec<([u8; 6], String)>> = Mutex::new(Vec::new());

/// The kernel has lent the card `mac`, which was called `name`.
pub fn note_lent(mac: [u8; 6], name: &str) {
    let mut lent = LENT.lock();
    if !lent.iter().any(|(m, n)| *m == mac && n == name) {
        lent.push((mac, String::from(name)));
    }
}

/// Whether an interface of the machine is called `name` now.
fn named(name: &str) -> bool {
    kernel_hal::drivers::all_net()
        .as_vec()
        .iter()
        .any(|card| card.get_ifname() == name)
}

/// The name the card `mac` had while the kernel drove it, when it was
/// lent ([`note_lent`]) and no interface has that name now.
pub fn name_for(mac: [u8; 6]) -> Option<String> {
    let lent = LENT.lock();
    let mut names = lent.iter().filter(|(m, _)| *m == mac).map(|(_, n)| n);
    names.find(|name| !named(name)).cloned()
}

/// How much each count of `now` is past `counted`, which becomes `now`.
pub(super) fn grown(counted: &mut [u64; COUNTS], now: [u64; COUNTS]) -> NetStats {
    let mut more = [0u64; COUNTS];
    for ((more, now), was) in more.iter_mut().zip(now).zip(counted.iter()) {
        // A count that goes back is a stack that lost count: nothing is
        // taken away for it.
        *more = now.saturating_sub(*was);
    }
    *counted = now;
    let [rx_bytes, rx_packets, rx_errors, rx_dropped, tx_bytes, tx_packets, tx_errors, tx_dropped] =
        more;
    NetStats {
        rx_bytes,
        rx_packets,
        tx_bytes,
        tx_packets,
        rx_errors,
        rx_dropped,
        tx_errors,
        tx_dropped,
    }
}

impl Others {
    fn keep<T>(&self, card: u64, with: impl FnOnce(&mut Kept) -> T) -> T {
        let mut kept = self.kept.lock();
        with(kept.entry(card).or_insert_with(|| Kept {
            name: None,
            carried: Arc::new(Carried::default()),
            sixes: Arc::new(Sixes::default()),
        }))
    }
}

/// Take `card`'s interface away, if it was shown.
fn hide(shown: Option<Arc<StackCard>>) {
    if let Some(card) = shown {
        let _ = kernel_hal::drivers::hotplug::remove(&Device::Net(card));
    }
}

impl StackLink {
    /// `CARD`: the stack's card `card` is `said` from now on.
    pub(super) fn take_card(&self, card: u64, said: Said) {
        if card == 0 {
            warn!("user stack: a CARD for the card it serves with");
            return;
        }
        let mut now = self.others.now.lock();
        let was = now.remove(&card);
        let counted = was.as_ref().map_or([0; COUNTS], |was| was.counted);
        if was.as_ref().is_some_and(|was| was.said == said) {
            now.extend(was.map(|was| (card, was)));
            return;
        }
        hide(was.and_then(|was| was.shown));
        let shown = said
            .address
            .filter(|_| super::is_for_all())
            .and_then(|address| self.show(card, &said, address));
        now.insert(
            card,
            Other {
                said,
                counted,
                shown,
            },
        );
    }

    /// Put `card` among the machine's interfaces.
    fn show(&self, card: u64, said: &Said, address: Ipv4Cidr) -> Option<Arc<StackCard>> {
        let (name, carried, sixes) = self.others.keep(card, |kept| {
            // The name it had while the kernel drove it; one of its own
            // otherwise, and the same from then on.
            let name = name_for(said.mac)
                .or_else(|| kept.name.clone().filter(|name| !named(name)))
                .unwrap_or_else(kernel_hal::drivers::net::next_eth_ifname);
            kept.name = Some(name.clone());
            (name, kept.carried.clone(), kept.sixes.clone())
        });
        let wire: Arc<dyn kernel_hal::drivers::net::stack_card::Wire> = super::machine();
        let shown = Arc::new(
            StackCard::new(
                name.clone(),
                said.mac,
                address.address().0,
                address.prefix_len(),
                said.gateway.map(|gateway| gateway.0),
            )
            .counting(carried)
            .with_sixes(sixes)
            .through(wire),
        );
        if !kernel_hal::drivers::hotplug::add(Device::Net(shown.clone())) {
            return None;
        }
        kernel_hal::klog_info!("stack card: {} is another card of the stack process", name);
        Some(shown)
    }

    /// `COUNTS` of a card that is not the first.
    pub(super) fn take_counts_of(&self, card: u64, now: [u64; COUNTS]) {
        let more = match self.others.now.lock().get_mut(&card) {
            Some(other) => grown(&mut other.counted, now),
            // Not said yet, or without an address: counted when it is.
            None => return,
        };
        self.others.keep(card, |kept| kept.carried.add(&more));
    }

    /// `SIX` of a card that is not the first.
    pub(super) fn take_six_of(
        &self,
        card: u64,
        router: Option<Ipv6Address>,
        addresses: Vec<Ipv6Cidr>,
    ) {
        self.others
            .keep(card, |kept| kept.sixes.say(addresses, router));
    }

    /// The stack is gone, and its cards with it.
    pub(super) fn forget_cards(&self) {
        let gone = core::mem::take(&mut *self.others.now.lock());
        for (_, other) in gone {
            hide(other.shown);
        }
        for kept in self.others.kept.lock().values() {
            kept.sixes.forget();
        }
    }

    /// Whether `addr` is the address of a card that is not the first.
    pub(super) fn is_another(&self, addr: Ipv4Address) -> bool {
        let now = self.others.now.lock();
        now.values()
            .any(|other| other.said.address.is_some_and(|a| a.address() == addr))
    }

    /// The address this machine speaks from to `to`: the one of the card
    /// whose network `to` is on, and the first card's to anywhere else.
    pub(super) fn address_toward(&self, to: Ipv4Address) -> Ipv4Address {
        let now = self.others.now.lock();
        let near = now.values().filter_map(|other| other.said.address);
        near.clone()
            .find(|address| address.contains_addr(&to))
            .map_or_else(|| self.address(), |address| address.address())
    }

    /// The other cards, as they were said: for tests and for whoever
    /// lists them.
    pub fn other_cards(&self) -> Vec<(u64, Said)> {
        let now = self.others.now.lock();
        now.iter()
            .map(|(card, other)| (*card, other.said))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::super::tests::{stack, stack_of, STACK_ADDR};
    use super::super::{counts_message, six_message};
    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    const MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x57];

    fn said(address: [u8; 4], prefix: u8, gateway: Option<[u8; 4]>) -> Said {
        Said {
            mac: MAC,
            address: Some(Ipv4Cidr::new(Ipv4Address(address), prefix)),
            gateway: gateway.map(Ipv4Address),
        }
    }

    #[test]
    fn a_card_is_said_as_the_stack_lays_it_out() {
        let card = said([10, 0, 4, 15], 24, Some([10, 0, 4, 2]));
        let m = card_message(2, &card);
        assert_eq!(m[4..8], OP_CARD.to_le_bytes());
        assert_eq!(m[8..16], 2u64.to_le_bytes());
        assert_eq!(
            m[16..],
            [0x52, 0x54, 0, 0x12, 0x34, 0x57, 24, 0, 10, 0, 4, 15, 10, 0, 4, 2]
        );
        assert_eq!(parse_card(&m[16..]), Some(card));
        // A card that lost its address, and one with no gateway.
        let lost = Said {
            mac: MAC,
            address: None,
            gateway: None,
        };
        assert_eq!(parse_card(&card_message(2, &lost)[16..]), Some(lost));
        // What is no card.
        assert_eq!(parse_card(&m[16..31]), None, "short");
        let mut none = m[16..].to_vec();
        none[..6].copy_from_slice(&[0; 6]);
        assert_eq!(parse_card(&none), None, "no MAC");
        let mut wide = m[16..].to_vec();
        wide[6] = 33;
        assert_eq!(parse_card(&wide), None, "a prefix of 33 bits");
        let mut bare = m[16..].to_vec();
        bare[6] = 0;
        assert_eq!(parse_card(&bare), None, "an address on no network");
        let mut group = m[16..].to_vec();
        group[8] = 224;
        assert_eq!(parse_card(&group), None, "a group is no card's address");
    }

    #[test]
    fn another_cards_address_is_this_machines_and_what_leaves_by_it_says_so() {
        let _lock = LOCK.lock();
        let stack = stack();
        let link = &stack.link;
        let first = Ipv4Address(STACK_ADDR);
        let other = Ipv4Address([10, 0, 4, 15]);
        assert!(!link.is_here(other));
        assert_eq!(link.address_toward(Ipv4Address([10, 0, 4, 2])), first);

        stack.say(card_message(2, &said(other.0, 24, None)));
        assert_eq!(link.other_cards(), [(2, said(other.0, 24, None))]);
        assert!(link.is_here(other) && link.is_here(first));
        // On its network, from its address; anywhere else from the
        // first card's.
        assert_eq!(link.address_toward(Ipv4Address([10, 0, 4, 2])), other);
        assert_eq!(link.address_toward(Ipv4Address([1, 1, 1, 1])), first);

        // Another address for it, and then none.
        let moved = Ipv4Address([192, 168, 7, 9]);
        stack.say(card_message(2, &said(moved.0, 16, None)));
        assert!(!link.is_here(other) && link.is_here(moved));
        assert_eq!(link.address_toward(Ipv4Address([192, 168, 200, 1])), moved);
        let lost = Said {
            mac: MAC,
            address: None,
            gateway: None,
        };
        stack.say(card_message(2, &lost));
        assert!(!link.is_here(moved));
        assert_eq!(link.address_toward(Ipv4Address([192, 168, 200, 1])), first);
        // The card the stack serves with is not said this way.
        stack.say(card_message(0, &said(other.0, 24, None)));
        assert_eq!(link.other_cards(), [(2, lost)]);
    }

    #[test]
    fn each_card_counts_for_itself_and_goes_with_the_stack() {
        let _lock = LOCK.lock();
        let stack = stack();
        let link = stack.link.clone();
        let other = said([10, 0, 4, 15], 24, None);
        stack.say(card_message(2, &other));
        let mut counts = counts_message(&[600, 4, 0, 0, 420, 3, 0, 0]);
        counts[8..16].copy_from_slice(&2u64.to_le_bytes());
        stack.say(counts);
        let carried = |card| link.others.keep(card, |kept| kept.carried.so_far());
        assert_eq!((carried(2).rx_bytes, carried(2).tx_packets), (600, 3));
        // Not the first card's, whose counts are apart.
        assert_eq!(link.carried().so_far().rx_bytes, 0);
        stack.say(counts_message(&[50, 1, 0, 0, 0, 0, 0, 0]));
        assert_eq!(link.carried().so_far().rx_bytes, 50);
        assert_eq!(carried(2).rx_bytes, 600);
        // Its IPv6 addresses, too.
        let address: core::net::Ipv6Addr = "fe80::1".parse().unwrap();
        let address = Ipv6Cidr::new(Ipv6Address(address.octets()), 64);
        let mut six = six_message(None, &[address]);
        six[8..16].copy_from_slice(&2u64.to_le_bytes());
        stack.say(six);
        let sixes = |card| link.others.keep(card, |kept| kept.sixes.addresses());
        assert_eq!(sixes(2), [address]);
        assert!(link.sixes().addresses().is_empty());

        // The stack dies: its cards are nobody's, and the next one's
        // counts go on from what was carried.
        drop(stack);
        link.pump();
        assert!(link.other_cards().is_empty());
        assert!(!link.is_here(Ipv4Address([10, 0, 4, 15])));
        assert!(sixes(2).is_empty());
        let next = stack_of(&link);
        next.say(card_message(2, &other));
        let mut counts = counts_message(&[40, 1, 0, 0, 0, 0, 0, 0]);
        counts[8..16].copy_from_slice(&2u64.to_le_bytes());
        next.say(counts);
        assert_eq!(carried(2).rx_bytes, 640);
    }
}
