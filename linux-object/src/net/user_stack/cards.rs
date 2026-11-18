//! The cards of the stack that runs as a process, and their addresses.
//!
//! The stack gives the kernel its sockets with one card, its first
//! (`eclipse_netstack_serve`), which it says there with the address it
//! has then, if it has one. A machine with more cards has a stack for
//! each in the one process (`tools/eclipse-netstack`, `cards`). Each
//! card is said with a message of its own, the others at the start and
//! every one whenever its address or its gateway changes:
//!
//! ```text
//! CARD  (stack to kernel)  8  u64 card  16  [u8; 6] MAC  22  u8 prefix  23  u8 zero
//!                          24  [u8; 4] address  28  [u8; 4] gateway
//! ```
//!
//! `card` is a number of the stack's for that card, and 0 is the card it
//! serves with. No address (all zero) is a card that has none yet, or
//! lost its own; no gateway is all zero. What a card has carried
//! (`COUNTS`) and what it has of IPv6 (`SIX`) are said with the same
//! number where a socket's would be. So are the frames it heard
//! (`FRAMES`), which only who listens on its interface or on every one
//! hears, and a frame for it to carry (`FRAME`, kernel to stack) says
//! the card it was sent on the same way.
//!
//! What the kernel does with it: on a boot where every program's sockets
//! are the stack's, each card is an interface of the machine, with its
//! address, its gateway, its counts and its IPv6 addresses, under the
//! name it had while the kernel drove it. The first card's address is
//! the stack's here. Another's is this machine's too: what is sent there
//! is for another program here, and a socket can be bound to it. And a
//! connection, or a connected datagram socket, to a machine on that
//! card's network says that card's address as its own, which is the one
//! it leaves by. The cards go with the stack; the next one says its own.
//!
//! The address and the gateway are the stack's to have and anybody's to
//! ask for: what `ip addr`, `ip route` and `ifconfig` set on such an
//! interface is asked of the stack ([`StackLink::configure`]),
//!
//! ```text
//! ADDRESS  (kernel to stack)  8  u64 card  16  [u8; 4] address  20  u8 prefix  21  three zero
//! GATEWAY  (kernel to stack)  8  u64 card  16  [u8; 4] gateway
//! ```
//!
//! all zero for none. The stack then has that address and asks no server
//! for one any more, and says the card again (`CARD`) as it is. The
//! kernel shows what it asked for at once, and what the stack says when
//! it does.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryInto;
use kernel_hal::drivers::net::stack_card::{Carried, Change, More, Sixes, StackCard};
use kernel_hal::drivers::prelude::Device;
use kernel_hal::drivers::scheme::{NetScheme, NetStats};
use kernel_hal::sync::Mutex;
use zcore_drivers::wire::{IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};

use super::{header, StackLink, COUNTS};

/// The operations of the messages.
pub const OP_CARD: u32 = 31;
pub const OP_ADDRESS: u32 = 32;
pub const OP_GATEWAY: u32 = 33;

/// `ADDRESS`: card `card` is to have `address`, or none.
pub fn address_message(card: u64, address: Option<Ipv4Cidr>) -> Vec<u8> {
    let mut m = header(OP_ADDRESS, card, 8);
    m.extend_from_slice(&address.map_or([0; 4], |a| a.address().0));
    m.extend_from_slice(&[address.map_or(0, |a| a.prefix_len()), 0, 0, 0]);
    m
}

/// `GATEWAY`: card `card` is to send what is not for its network to
/// `gateway`, or nowhere.
pub fn gateway_message(card: u64, gateway: Option<Ipv4Address>) -> Vec<u8> {
    let mut m = header(OP_GATEWAY, card, 4);
    m.extend_from_slice(&gateway.map_or([0; 4], |g| g.0));
    m
}
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

impl Said {
    /// The card a stack gives the kernel its sockets with, as that call
    /// says it: `prefix` the bits of its network's prefix and `gateway`
    /// an address as a big-endian number, each 0 for none.
    pub fn first(mac: [u8; 6], address: [u8; 4], prefix: usize, gateway: usize) -> Said {
        let address = (address != [0; 4] && (1..=32).contains(&prefix))
            .then(|| Ipv4Cidr::new(Ipv4Address(address), prefix as u8));
        let gateway = (gateway != 0 && gateway as u64 <= u64::from(u32::MAX))
            .then(|| Ipv4Address((gateway as u32).to_be_bytes()));
        Said {
            mac,
            address,
            gateway: gateway.filter(|_| address.is_some()),
        }
    }
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

/// A card of the stack's, as it said it last.
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
    more: Arc<More>,
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
            more: Arc::new(More::default()),
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
        let mut now = self.others.now.lock();
        if card == 0 {
            let address = said
                .address
                .map_or(0, |a| u32::from_be_bytes(a.address().0));
            self.address
                .store(address, core::sync::atomic::Ordering::Release);
        }
        if let Some(other) = now.get_mut(&card) {
            if other.said != said {
                other.said = said;
                if let Some(shown) = &other.shown {
                    shown.set(said.address, said.gateway);
                }
            }
            return;
        }
        // The first card is put among the machine's interfaces by who
        // gave the kernel its sockets ([`StackLink::first_card`]).
        let shown = (card != 0).then(|| self.show(card, &said)).flatten();
        now.insert(
            card,
            Other {
                said,
                counted: [0; COUNTS],
                shown,
            },
        );
    }

    /// The stack that serves now gave the kernel its sockets with a card
    /// that is `said`, which `shown` shows among the machine's
    /// interfaces, when anything does.
    pub fn first_card(&self, said: Said, shown: Option<Arc<StackCard>>) {
        let mut now = self.others.now.lock();
        match now.get_mut(&0) {
            // The stack has said it again already: that is the later word.
            Some(first) => {
                if let Some(shown) = &shown {
                    shown.set(first.said.address, first.said.gateway);
                }
                first.shown = shown;
            }
            None => {
                now.insert(
                    0,
                    Other {
                        said,
                        counted: [0; COUNTS],
                        shown,
                    },
                );
            }
        }
    }

    /// Ask the stack for `change` on its card `card`, and show it as
    /// asked until the stack says how it is. `false` when it was not
    /// asked: no stack serves, it has no such card, or the gateway is
    /// not on the card's network.
    pub fn configure(&self, card: u64, change: Change) -> bool {
        let said = self.others.now.lock().get(&card).map(|other| other.said);
        let Some(mut said) = said else {
            return false;
        };
        // A gateway is a machine of a network of the card's: of its
        // first address or of another.
        let others = self.more_of(card).addresses();
        let near = |first: Option<Ipv4Cidr>, gateway: &Ipv4Address| {
            let mut ours = first.iter().chain(&others);
            ours.any(|a| a.contains_addr(gateway) && a.address() != *gateway)
        };
        let message = match change {
            Change::Address(address) => {
                said.address = address;
                said.gateway = said.gateway.filter(|gateway| near(address, gateway));
                address_message(card, address)
            }
            Change::Gateway(gateway) => {
                if gateway
                    .as_ref()
                    .is_some_and(|gateway| !near(said.address, gateway))
                {
                    return false;
                }
                said.gateway = gateway;
                gateway_message(card, gateway)
            }
            // The first address goes: what comes in its place, another
            // of the card's or one a server gives, is the stack's to
            // say.
            Change::Also(IpCidr::Ipv4(cidr), false)
                if said.address.is_some_and(|a| a.address() == cidr.address()) =>
            {
                said.address = None;
                said.gateway = None;
                super::more::also_message(card, IpCidr::Ipv4(cidr), false, self.next_wish())
            }
            wish => match self.wish(card, wish) {
                Some(message) => message,
                None => return false,
            },
        };
        if self.tell(message, true).is_err() {
            return false;
        }
        self.take_card(card, said);
        true
    }

    /// Put `card` among the machine's interfaces.
    fn show(&self, card: u64, said: &Said) -> Option<Arc<StackCard>> {
        let (name, carried, sixes, more) = self.others.keep(card, |kept| {
            // The name it had while the kernel drove it; one of its own
            // otherwise, and the same from then on.
            let name = name_for(said.mac)
                .or_else(|| kept.name.clone().filter(|name| !named(name)))
                .unwrap_or_else(kernel_hal::drivers::net::next_eth_ifname);
            kept.name = Some(name.clone());
            let said = (kept.sixes.clone(), kept.more.clone());
            (name, kept.carried.clone(), said.0, said.1)
        });
        let wire: Arc<dyn kernel_hal::drivers::net::stack_card::Wire> = super::machine();
        let shown = Arc::new(
            StackCard::new(
                name.clone(),
                said.mac,
                said.address.map_or([0; 4], |a| a.address().0),
                said.address.map_or(0, |a| a.prefix_len()),
                said.gateway.map(|gateway| gateway.0),
            )
            .numbered(card)
            .counting(carried)
            .with_sixes(sixes)
            .with_more(more)
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
        for (card, other) in gone {
            // The first card's interface is taken away by who put it.
            if card != 0 {
                hide(other.shown);
            }
        }
        for kept in self.others.kept.lock().values() {
            kept.sixes.forget();
            kept.more.forget();
        }
    }

    /// The interface that shows `card`, as Linux numbers them: 0 when
    /// none does, which is a frame for whoever listens on every one.
    pub(super) fn ifindex_of(&self, card: u64) -> u32 {
        let shown = self
            .others
            .now
            .lock()
            .get(&card)
            .and_then(|other| other.shown.clone());
        let Some(shown) = shown else {
            return 0;
        };
        let name = shown.get_ifname();
        let at = crate::net::get_net_device()
            .iter()
            .position(|interface| interface.get_ifname() == name);
        at.map_or(0, |at| at as u32 + 1)
    }

    /// Where what card `card`, not the first, has of other addresses
    /// and routes is kept, and of IPv6.
    pub(super) fn kept_more(&self, card: u64) -> Arc<More> {
        self.others.keep(card, |kept| kept.more.clone())
    }

    pub(super) fn kept_sixes(&self, card: u64) -> Arc<Sixes> {
        self.others.keep(card, |kept| kept.sixes.clone())
    }

    /// Whether what some card that is not the first has `is`.
    pub(super) fn any_kept_more(&self, is: impl Fn(&More) -> bool) -> bool {
        self.others.kept.lock().values().any(|kept| is(&kept.more))
    }

    /// The first thing `find` finds in what a card that is not the
    /// first has.
    pub(super) fn find_kept_more<T>(&self, find: impl Fn(&More) -> Option<T>) -> Option<T> {
        let kept = self.others.kept.lock();
        kept.values().find_map(|kept| find(&kept.more))
    }

    /// Whether `addr` is the address of a card that is not the first.
    pub(super) fn is_another(&self, addr: Ipv4Address) -> bool {
        let now = self.others.now.lock();
        let mut others = now.iter().filter(|(card, _)| **card != 0);
        others.any(|(_, other)| other.said.address.is_some_and(|a| a.address() == addr))
    }

    /// The address this machine speaks from to `to`: the one of the card
    /// whose network `to` is on, and the first card's to anywhere else.
    pub(super) fn address_toward(&self, to: Ipv4Address) -> Ipv4Address {
        let now = self.others.now.lock();
        let others = now.iter().filter(|(card, _)| **card != 0);
        let mut near = others.filter_map(|(_, other)| other.said.address);
        near.find(|address| address.contains_addr(&to))
            .map(|address| address.address())
            // Then an address a card has after its first.
            .or_else(|| self.also_toward(to))
            .unwrap_or_else(|| self.address())
    }

    /// The other cards, as they were said: for tests and for whoever
    /// lists them.
    pub fn other_cards(&self) -> Vec<(u64, Said)> {
        let now = self.others.now.lock();
        let others = now.iter().filter(|(card, _)| **card != 0);
        others.map(|(card, other)| (*card, other.said)).collect()
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
        // The card the stack serves with is the first: what is said of
        // it is the address the machine speaks from, and no other card.
        stack.say(card_message(0, &said([10, 0, 9, 77], 24, None)));
        assert_eq!(link.other_cards(), [(2, lost)]);
        assert_eq!(link.address(), Ipv4Address([10, 0, 9, 77]));
        assert!(link.is_here(Ipv4Address([10, 0, 9, 77])) && !link.is_here(first));
        stack.say(card_message(0, &lost));
        assert!(link.address().is_unspecified());
        assert!(!link.is_here(Ipv4Address::UNSPECIFIED));
    }

    #[test]
    fn what_is_set_on_a_card_is_asked_of_the_stack_that_has_it() {
        let _lock = LOCK.lock();
        let stack = stack();
        let link = &stack.link;
        let to = Ipv4Cidr::new(Ipv4Address([10, 0, 4, 77]), 24);
        // A card the stack has not said is nobody's to set.
        assert!(!link.configure(2, Change::Address(Some(to))));
        assert_eq!(stack.told(), None);

        stack.say(card_message(
            2,
            &said([10, 0, 4, 15], 24, Some([10, 0, 4, 2])),
        ));
        assert!(link.configure(2, Change::Address(Some(to))));
        assert_eq!(stack.told(), Some(address_message(2, Some(to))));
        // Shown as asked, with the gateway that is still near.
        assert_eq!(
            link.other_cards(),
            [(2, said([10, 0, 4, 77], 24, Some([10, 0, 4, 2])))]
        );
        assert!(link.is_here(to.address()));

        // A gateway on the card's network, and not the card itself.
        let far = Ipv4Address([10, 0, 5, 1]);
        assert!(!link.configure(2, Change::Gateway(Some(far))));
        assert!(!link.configure(2, Change::Gateway(Some(to.address()))));
        assert_eq!(stack.told(), None);
        let near = Ipv4Address([10, 0, 4, 1]);
        assert!(link.configure(2, Change::Gateway(Some(near))));
        assert_eq!(stack.told(), Some(gateway_message(2, Some(near))));
        assert_eq!(
            link.other_cards(),
            [(2, said([10, 0, 4, 77], 24, Some([10, 0, 4, 1])))]
        );
        assert!(link.configure(2, Change::Gateway(None)));
        assert_eq!(stack.told(), Some(gateway_message(2, None)));

        // Another network takes the gateway with it; no address, too.
        let moved = Ipv4Cidr::new(Ipv4Address([192, 168, 7, 9]), 16);
        assert!(link.configure(2, Change::Gateway(Some(near))));
        assert!(link.configure(2, Change::Address(Some(moved))));
        assert_eq!(link.other_cards(), [(2, said(moved.address().0, 16, None))]);
        assert!(link.configure(2, Change::Address(None)));
        assert!(!link.is_here(moved.address()));
        assert!(
            !link.configure(2, Change::Gateway(Some(near))),
            "no network"
        );

        // What the stack says after is how it is, whatever was asked.
        stack.say(card_message(2, &said([10, 0, 4, 15], 24, None)));
        assert_eq!(link.other_cards(), [(2, said([10, 0, 4, 15], 24, None))]);
    }

    #[test]
    fn the_first_card_is_set_as_the_others_and_is_the_machines_address() {
        let _lock = LOCK.lock();
        let stack = stack();
        let link = stack.link.clone();
        // As who gives the kernel its sockets says it: no address yet.
        assert_eq!(
            Said::first(MAC, [0; 4], 0, 0x0a00_0902),
            Said {
                mac: MAC,
                address: None,
                gateway: None
            }
        );
        let first = Said::first(MAC, STACK_ADDR, 24, 0x0a00_0902);
        assert_eq!(first, said(STACK_ADDR, 24, Some([10, 0, 9, 2])));
        link.first_card(first, None);
        assert!(link.other_cards().is_empty());

        let to = Ipv4Cidr::new(Ipv4Address([10, 0, 9, 77]), 24);
        assert!(link.configure(0, Change::Address(Some(to))));
        assert_eq!(stack.told(), Some(address_message(0, Some(to))));
        assert_eq!(link.address(), to.address());
        assert!(link.configure(0, Change::Address(None)));
        assert!(link.address().is_unspecified());

        // The stack is gone: there is no card to set until the next.
        drop(stack);
        link.pump();
        assert!(!link.configure(0, Change::Address(Some(to))));
    }

    #[test]
    fn what_is_asked_of_a_card_is_laid_out_for_the_stack() {
        let to = Ipv4Cidr::new(Ipv4Address([10, 0, 4, 77]), 24);
        let m = address_message(3, Some(to));
        assert_eq!(m[4..8], OP_ADDRESS.to_le_bytes());
        assert_eq!(m[8..16], 3u64.to_le_bytes());
        assert_eq!(m[16..], [10, 0, 4, 77, 24, 0, 0, 0]);
        assert_eq!(address_message(3, None)[16..], [0; 8]);
        let m = gateway_message(1, Some(Ipv4Address([10, 0, 4, 2])));
        assert_eq!(m[4..8], OP_GATEWAY.to_le_bytes());
        assert_eq!(m[8..16], 1u64.to_le_bytes());
        assert_eq!(m[16..], [10, 0, 4, 2]);
        assert_eq!(gateway_message(1, None)[16..], [0; 4]);
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
