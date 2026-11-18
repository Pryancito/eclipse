//! A card's other addresses and its routes to one network or another.
//!
//! A card of the stack's has a first address of IPv4 and a gateway
//! ([`super::cards`]), and the IPv6 addresses its link's router gives
//! (`SIX`). It can have more, because somebody on the machine says so
//! (`ip addr add`, `ip route add NETWORK via ROUTER`): other addresses,
//! of IPv4 and of IPv6, and routes to less than everything. They are the
//! stack's to have and are asked of it like the first
//! ([`StackLink::configure`]):
//!
//! ```text
//! ALSO   (kernel to stack)  8  u64 card  16  u8 4 or 6  17  u8 the bits of the prefix
//!                           18  u8 1 to have it, 0 to have it no more  19  u8 zero
//!                           20  u32 which wish this is
//!                           24  [u8; 16] the address (of IPv4: its four bytes, and zeroes)
//! ROUTE  (kernel to stack)  the same, the network at 24, and
//!                           40  [u8; 16] the router, or zeroes
//! ```
//!
//! and the stack says what the card has of them, all of it each time it
//! changes:
//!
//! ```text
//! MORE   (stack to kernel)  8  u64 card  16  one after another, to the end:
//!             0  u8 1 an address, 2 a route  1  u8 4 or 6  2  u8 prefix  3  five zero
//!             8  [u8; 16] the address or the network  24  [u8; 16] the router, or zeroes
//! ```
//!
//! with the addresses of IPv4 after the first and the routes to less
//! than everything: the first address and the gateway are in `CARD`, the
//! IPv6 addresses (the ones written by hand too) and their router in
//! `SIX`.
//!
//! What the kernel does with it: the interface shows them
//! (`StackCard`), an address of any card is this machine's (a socket
//! can be bound to it, what is sent there never leaves), and a
//! destination of IPv6 a route leads to is one the card has a way to.
//! As with the first address, the kernel shows what it asked for at
//! once and what the stack says when it does. Two wishes can follow
//! each other faster than the stack answers the first, and its answer
//! to the first would undo, for a moment, what is shown of the second:
//! `ip route add` through a router on the network of an address added
//! the line before was refused for it. So the wishes are numbered
//! ([`StackLink::wish`]), a `MORE` and a `SIX` say in their first four
//! bytes the last wish the stack had done when it said them, and one
//! from before the last wish is not listened to ([`StackLink::stale`]):
//! the one that answers the last wish follows it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use kernel_hal::drivers::net::stack_card::{Change, More, Sixes};
use zcore_drivers::wire::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address};

use super::{header, StackLink};

/// The operations of the messages.
pub const OP_ALSO: u32 = 36;
pub const OP_ROUTE: u32 = 37;
pub const OP_MORE: u32 = 38;

/// What each entry of a `MORE` takes, and how many it has at most.
pub const MORE_ENTRY_LEN: usize = 40;
pub const MORE_ENTRIES_MAX: usize = 64;
const AN_ADDRESS: u8 = 1;
const A_ROUTE: u8 = 2;

/// The sixteen bytes an address travels in.
fn sixteen(address: &IpAddress) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    let said = address.as_bytes();
    bytes[..said.len()].copy_from_slice(said);
    bytes
}

fn family(cidr: &IpCidr) -> u8 {
    match cidr {
        IpCidr::Ipv4(_) => 4,
        _ => 6,
    }
}

/// `ALSO`: card `card` is to have `address` too, or no more.
pub fn also_message(card: u64, address: IpCidr, have: bool, which: u32) -> Vec<u8> {
    let mut m = header(OP_ALSO, card, 24);
    m.extend_from_slice(&[family(&address), address.prefix_len(), have as u8, 0]);
    m.extend_from_slice(&which.to_le_bytes());
    m.extend_from_slice(&sixteen(&address.address()));
    m
}

/// `ROUTE`: card `card` is to send what is for `to` through `router`, or
/// no more.
pub fn route_message(
    card: u64,
    to: IpCidr,
    router: Option<IpAddress>,
    have: bool,
    which: u32,
) -> Vec<u8> {
    let mut m = header(OP_ROUTE, card, 40);
    m.extend_from_slice(&[family(&to), to.prefix_len(), have as u8, 0]);
    m.extend_from_slice(&which.to_le_bytes());
    m.extend_from_slice(&sixteen(&to.address()));
    m.extend_from_slice(&router.as_ref().map_or([0; 16], sixteen));
    m
}

/// What a card has besides its first address: its other addresses of
/// IPv4, and its routes.
pub type Said = (Vec<Ipv4Cidr>, Vec<(IpCidr, IpAddress)>);

/// What a `MORE` says, past its header: `None` when it is not whole or
/// names what no card has.
pub fn parse_more(said: &[u8]) -> Option<Said> {
    let (entries, rest) = said.as_chunks::<MORE_ENTRY_LEN>();
    if !rest.is_empty() || entries.len() > MORE_ENTRIES_MAX {
        return None;
    }
    let (mut addresses, mut routes) = (Vec::new(), Vec::new());
    for entry in entries {
        let (kind, family, prefix) = (entry[0], entry[1], entry[2]);
        if entry[3..8] != [0; 5] {
            return None;
        }
        let address = |bytes: &[u8]| match family {
            4 if bytes[4..] == [0; 12] => {
                Some(IpAddress::Ipv4(Ipv4Address::from_bytes(&bytes[..4])))
            }
            6 => Some(IpAddress::Ipv6(Ipv6Address::from_bytes(bytes))),
            _ => None,
        };
        let bits = if family == 4 { 32 } else { 128 };
        let (first, second) = (address(&entry[8..24])?, address(&entry[24..40])?);
        if prefix > bits {
            return None;
        }
        match (kind, first) {
            (AN_ADDRESS, IpAddress::Ipv4(first)) if first.is_unicast() && prefix != 0 => {
                addresses.push(Ipv4Cidr::new(first, prefix));
            }
            (A_ROUTE, first) if second.is_unicast() && prefix != 0 => {
                routes.push((IpCidr::new(first, prefix), second));
            }
            _ => return None,
        }
    }
    Some((addresses, routes))
}

impl StackLink {
    /// What the first card of the stack that serves now has of other
    /// addresses and routes, as it said last (`MORE`): for the
    /// interface that shows that card. Nothing while no stack serves.
    pub fn more(&self) -> Arc<More> {
        self.more.clone()
    }

    /// Where what `card` has of other addresses and routes is kept.
    pub(super) fn more_of(&self, card: u64) -> Arc<More> {
        match card {
            0 => self.more.clone(),
            card => self.kept_more(card),
        }
    }

    /// Where what `card` has of IPv6 is kept.
    pub(super) fn sixes_of(&self, card: u64) -> Arc<Sixes> {
        match card {
            0 => self.sixes.clone(),
            card => self.kept_sixes(card),
        }
    }

    /// `MORE`: the stack's card `card` has these from now on.
    pub(super) fn take_more(&self, card: u64, (addresses, routes): Said) {
        self.more_of(card).say(addresses, routes);
    }

    /// The number of the next wish: what the stack said before it had
    /// done that one is not listened to from now on.
    pub(super) fn next_wish(&self) -> u32 {
        self.wishes.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    /// Whether `m`, a `MORE` or a `SIX`, is from before the last wish:
    /// the stack had not done it yet when it said this.
    pub(super) fn stale(&self, m: &[u8]) -> bool {
        let done = u32::from_le_bytes([m[0], m[1], m[2], m[3]]);
        let last = self.wishes.load(Ordering::Acquire);
        // The numbers go round: behind is less than half of them away.
        done != last && last.wrapping_sub(done) < u32::MAX / 2
    }

    /// Show `card` as `change` asks, an address or a route more or less,
    /// until the stack says how it is; and the message that asks. `None`
    /// for a change that is not one of those.
    pub(super) fn wish(&self, card: u64, change: Change) -> Option<Vec<u8>> {
        let which = self.next_wish();
        match change {
            Change::Also(IpCidr::Ipv4(cidr), have) => {
                self.more_of(card).change(|addresses, _| {
                    addresses.retain(|a| a.address() != cidr.address());
                    if have {
                        addresses.push(cidr);
                    }
                });
                Some(also_message(card, IpCidr::Ipv4(cidr), have, which))
            }
            Change::Also(IpCidr::Ipv6(cidr), have) => {
                self.sixes_of(card).change(|addresses, _| {
                    addresses.retain(|a| a.address() != cidr.address());
                    if have {
                        addresses.push(cidr);
                    }
                });
                Some(also_message(card, IpCidr::Ipv6(cidr), have, which))
            }
            Change::Route(to, router, have) => {
                match (to, router) {
                    // To everything, of IPv6: the card's router.
                    (IpCidr::Ipv6(_), router) if to.prefix_len() == 0 => {
                        let router = router.and_then(|router| match router {
                            IpAddress::Ipv6(router) if have => Some(router),
                            _ => None,
                        });
                        self.sixes_of(card).change(|_, shown| *shown = router);
                    }
                    (to, router) => self.more_of(card).change(|_, routes| {
                        routes.retain(|(have, _)| *have != to);
                        if let (true, Some(router)) = (have, router) {
                            routes.push((to, router));
                        }
                    }),
                }
                Some(route_message(card, to, router, have, which))
            }
            _ => None,
        }
    }

    /// Whether `addr` is an address of a card that is not its first.
    pub(super) fn is_also(&self, addr: Ipv4Address) -> bool {
        self.more.has(&addr) || self.any_kept_more(|more| more.has(&addr))
    }

    /// The address after its first that a card speaks from to `to`: one
    /// whose network `to` is on.
    pub(super) fn also_toward(&self, to: Ipv4Address) -> Option<Ipv4Address> {
        let near = |more: &More| {
            let mut ours = more.addresses().into_iter();
            ours.find(|ours| ours.contains_addr(&to))
                .map(|ours| ours.address())
        };
        near(&self.more).or_else(|| self.find_kept_more(near))
    }

    /// Whether the machine has a way to `addr` as IPv6: its first card
    /// has, by a network of its own or its router, or by a route.
    pub(super) fn reaches6(&self, addr: &Ipv6Address) -> bool {
        self.sixes.reaches(addr)
            || (!self.sixes.addresses().is_empty() && self.more.has_way(&IpAddress::Ipv6(*addr)))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::super::cards::{card_message, Said as Card};
    use super::super::tests::{stack, STACK_ADDR};
    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;
    use alloc::vec;
    use zcore_drivers::wire::Ipv6Cidr;

    const MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x57];

    fn v4(a: [u8; 4], prefix: u8) -> IpCidr {
        IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address(a), prefix))
    }

    /// A `MORE` as the stack lays it out (`tools/eclipse-netstack`,
    /// `socks/more.rs`).
    fn more_message(card: u64, entries: &[(u8, IpCidr, Option<IpAddress>)]) -> Vec<u8> {
        more_after(0, card, entries)
    }

    /// The same, said after the stack had done wish number `done`.
    fn more_after(done: u32, card: u64, entries: &[(u8, IpCidr, Option<IpAddress>)]) -> Vec<u8> {
        let mut m = header(OP_MORE, card, entries.len() * MORE_ENTRY_LEN);
        m[..4].copy_from_slice(&done.to_le_bytes());
        for (kind, cidr, router) in entries {
            m.extend_from_slice(&[*kind, family(cidr), cidr.prefix_len(), 0, 0, 0, 0, 0]);
            m.extend_from_slice(&sixteen(&cidr.address()));
            m.extend_from_slice(&router.as_ref().map_or([0; 16], sixteen));
        }
        m
    }

    #[test]
    fn what_is_wished_of_a_card_is_laid_out_for_the_stack() {
        let m = also_message(3, v4([192, 168, 7, 9], 24), true, 0x0102);
        assert_eq!(m[4..8], OP_ALSO.to_le_bytes());
        assert_eq!(m[8..16], 3u64.to_le_bytes());
        let mut said = vec![4, 24, 1, 0, 2, 1, 0, 0, 192, 168, 7, 9];
        said.resize(24, 0);
        assert_eq!(m[16..], said[..]);
        let six = IpCidr::new(IpAddress::v6(0x2001, 0xdb8, 0, 0, 0, 0, 0, 5), 64);
        let m = also_message(0, six, false, 0);
        assert_eq!(m[16..24], [6, 64, 0, 0, 0, 0, 0, 0]);
        assert_eq!(m[24..28], [0x20, 0x01, 0x0d, 0xb8]);
        assert_eq!((m[39], m.len()), (5, 40));
        let through = IpAddress::v4(10, 0, 9, 7);
        let m = route_message(1, v4([172, 16, 0, 0], 12), Some(through), true, 5);
        assert_eq!(m[4..8], OP_ROUTE.to_le_bytes());
        assert_eq!(m[16..28], [4, 12, 1, 0, 5, 0, 0, 0, 172, 16, 0, 0]);
        assert_eq!((&m[40..44], m.len()), (&[10, 0, 9, 7][..], 56));
        let m = route_message(1, v4([172, 16, 0, 0], 12), None, false, 0);
        assert_eq!((m[18], &m[40..]), (0, &[0u8; 16][..]));
    }

    #[test]
    fn what_a_stack_says_a_card_has_more_is_read_or_refused_whole() {
        let other = v4([192, 168, 7, 9], 24);
        let to = v4([172, 16, 0, 0], 12);
        let through = IpAddress::v4(192, 168, 7, 1);
        let to6 = IpCidr::new(IpAddress::v6(0x2001, 0xdb8, 7, 0, 0, 0, 0, 0), 48);
        let through6 = IpAddress::v6(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let m = more_message(
            0,
            &[
                (AN_ADDRESS, other, None),
                (A_ROUTE, to, Some(through)),
                (A_ROUTE, to6, Some(through6)),
            ],
        );
        let IpCidr::Ipv4(other4) = other else {
            unreachable!()
        };
        assert_eq!(
            parse_more(&m[16..]),
            Some((vec![other4], vec![(to, through), (to6, through6)]))
        );
        assert_eq!(parse_more(&[]), Some((vec![], vec![])));
        // Cut short, a route with no router, an address of IPv6 (those
        // are said in `SIX`), a prefix of too many bits, a kind there is
        // none of.
        assert_eq!(parse_more(&m[16..m.len() - 1]), None);
        for bad in [
            more_message(0, &[(A_ROUTE, to, None)]),
            more_message(0, &[(AN_ADDRESS, to6, None)]),
            more_message(0, &[(3, other, None)]),
        ] {
            assert_eq!(parse_more(&bad[16..]), None, "{bad:?}");
        }
        let mut bad = more_message(0, &[(AN_ADDRESS, other, None)]);
        bad[18] = 33;
        assert_eq!(parse_more(&bad[16..]), None);
    }

    #[test]
    fn an_address_or_a_route_more_is_asked_shown_at_once_and_then_as_the_stack_says() {
        let _lock = LOCK.lock();
        let stack = stack();
        let link = stack.link.clone();
        let first = Card::first(MAC, STACK_ADDR, 24, 0);
        link.first_card(first, None);
        let other = Ipv4Cidr::new(Ipv4Address([192, 168, 7, 9]), 24);
        let also = Change::Also(IpCidr::Ipv4(other), true);
        assert!(link.configure(0, also));
        assert_eq!(
            stack.told(),
            Some(also_message(0, IpCidr::Ipv4(other), true, 1))
        );
        // Shown at once, and this machine's from then on.
        assert_eq!(link.more().addresses(), [other]);
        assert!(link.is_here(other.address()));
        assert!(!link.is_here(Ipv4Address([192, 168, 7, 1])));
        // What is on its network is spoken to from it.
        let near = Ipv4Address([192, 168, 7, 1]);
        assert_eq!(link.address_toward(near), other.address());
        assert_eq!(
            link.address_toward(Ipv4Address([1, 1, 1, 1])),
            Ipv4Address(STACK_ADDR)
        );
        // A gateway on that network is near, now.
        assert!(link.configure(0, Change::Gateway(Some(near))));
        stack.told();
        // A route, and one of IPv6 once the card has an address of it.
        let to = v4([172, 16, 0, 0], 12);
        let route = Change::Route(to, Some(near.into()), true);
        assert!(link.configure(0, route));
        assert_eq!(
            stack.told(),
            Some(route_message(0, to, Some(near.into()), true, 2))
        );
        assert_eq!(link.more().routes(), [(to, IpAddress::Ipv4(near))]);
        let far6 = Ipv6Address::new(0x2001, 0xdb8, 7, 0, 0, 0, 0, 9);
        assert!(!link.reaches6(&far6));
        let ours6 = Ipv6Cidr::new(Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 5), 64);
        assert!(link.configure(0, Change::Also(IpCidr::Ipv6(ours6), true)));
        assert_eq!(link.sixes().addresses(), [ours6]);
        let to6 = IpCidr::new(IpAddress::v6(0x2001, 0xdb8, 7, 0, 0, 0, 0, 0), 48);
        let through6 = IpAddress::v6(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        assert!(link.configure(0, Change::Route(to6, Some(through6), true)));
        assert!(link.reaches6(&far6));
        let default6 = IpCidr::new(IpAddress::Ipv6(Ipv6Address::UNSPECIFIED), 0);
        assert!(link.configure(0, Change::Route(default6, Some(through6), true)));
        assert!(link.sixes().router().is_some());
        assert!(link.configure(0, Change::Route(default6, None, false)));
        assert!(link.sixes().router().is_none());

        // What the stack said before it had done the last wish is not
        // listened to: it would undo what is shown of that one.
        stack.say(more_after(5, 0, &[]));
        link.pump();
        assert_eq!(link.more().addresses(), [other]);
        assert_eq!(link.more().routes().len(), 2);
        // What it says after is how it is, whatever was asked.
        stack.say(more_after(6, 0, &[(A_ROUTE, to, Some(near.into()))]));
        link.pump();
        assert!(link.more().addresses().is_empty());
        assert!(!link.is_here(other.address()));
        assert_eq!(link.more().routes().len(), 1);

        // Another card's are apart, and this machine's too.
        stack.say(card_message(
            2,
            &Card {
                mac: MAC,
                address: Some(Ipv4Cidr::new(Ipv4Address([10, 0, 4, 15]), 24)),
                gateway: None,
            },
        ));
        stack.say(more_after(
            6,
            2,
            &[(AN_ADDRESS, v4([10, 0, 5, 15], 24), None)],
        ));
        link.pump();
        assert!(link.is_here(Ipv4Address([10, 0, 5, 15])));
        assert_eq!(
            link.address_toward(Ipv4Address([10, 0, 5, 1])),
            Ipv4Address([10, 0, 5, 15])
        );
        assert!(link.configure(2, Change::Also(v4([10, 0, 5, 15], 24), false)));
        assert!(!link.is_here(Ipv4Address([10, 0, 5, 15])));

        // The stack is gone, and what its cards had with it.
        drop(stack);
        link.pump();
        assert!(link.more().routes().is_empty());
        assert!(link.sixes().addresses().is_empty());
        assert!(!link.configure(0, also));
    }

    #[test]
    fn a_socket_bound_to_an_address_of_a_cards_says_so_to_the_stack() {
        use super::super::tests::{at, ip};
        use super::super::{
            bind_message, connect_message, send_message, ServedTcpSocket, ServedUdpSocket, Takes,
            HEADER_LEN,
        };
        use crate::error::LxError;
        use crate::fs::{FileLike, OpenFlags};
        use crate::net::{Endpoint, Socket};
        use core::convert::TryInto;
        let _lock = LOCK.lock();
        let stack = stack();
        let link = stack.link.clone();
        link.first_card(Card::first(MAC, STACK_ADDR, 24, 0), None);
        let other = [192, 168, 7, 9];
        let beside = (AN_ADDRESS, v4([10, 0, 9, 77], 24), None);
        stack.say(more_message(
            0,
            &[beside, (AN_ADDRESS, v4(other, 24), None)],
        ));
        while stack.told().is_some() {}
        // Where the card's first address and another are on one network,
        // a socket bound to neither speaks from the first.
        assert_eq!(
            link.address_toward(Ipv4Address([10, 0, 9, 1])),
            Ipv4Address(STACK_ADDR)
        );

        // Not at an address nobody of this machine has.
        let s = ServedUdpSocket::new(link.clone(), false).unwrap();
        assert_eq!(
            Socket::bind(&s, Endpoint::Ip(at([192, 168, 8, 9], 5000))),
            Err(LxError::EADDRNOTAVAIL)
        );
        // At the card's other address: the stack is told which, and the
        // socket says it is there.
        assert_eq!(Socket::bind(&s, Endpoint::Ip(at(other, 5000))), Ok(0));
        let said = bind_message(s.sock, 5000, Takes::Four, Some(at(other, 0).addr));
        assert_eq!(said.len(), HEADER_LEN + 20);
        assert_eq!(said[16..24], [0x88, 0x13, 4, 4, 192, 168, 7, 9]);
        assert_eq!(stack.told(), Some(said));
        assert_eq!(ip(Socket::endpoint(&s).unwrap()), at(other, 5000));
        // What it sends goes to the stack, which has it leave from there.
        let far = Endpoint::Ip(at([10, 0, 9, 1], 53));
        assert_eq!(Socket::write(&s, b"hi", Some(far)), Ok(2));
        assert_eq!(
            stack.told(),
            Some(send_message(s.sock, [10, 0, 9, 1], 53, 64, b"hi"))
        );
        // At its first, the same.
        let first = ServedUdpSocket::new(link.clone(), false).unwrap();
        assert_eq!(
            Socket::bind(&first, Endpoint::Ip(at(STACK_ADDR, 5001))),
            Ok(0)
        );
        assert_eq!(
            stack.told(),
            Some(bind_message(
                first.sock,
                5001,
                Takes::Four,
                Some(at(STACK_ADDR, 0).addr)
            ))
        );

        // A connection from a socket bound there is from there: four
        // bytes after what a connect always said.
        let t = ServedTcpSocket::new(link.clone(), false).unwrap();
        FileLike::set_flags(&t, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(Socket::bind(&t, Endpoint::Ip(at(other, 6000))), Ok(0));
        assert_eq!(ip(Socket::endpoint(&t).unwrap()), at(other, 6000));
        let far = Endpoint::Ip(at([10, 0, 9, 1], 80));
        assert_eq!(
            async_std::task::block_on(Socket::connect(&t, far)),
            Err(LxError::EINPROGRESS)
        );
        let told = stack.told().unwrap();
        let (plain, from) = told.split_at(told.len() - 4);
        assert_eq!(from, other);
        let window = u32::from_le_bytes(plain[24..28].try_into().unwrap());
        let sock = u64::from_le_bytes(plain[8..16].try_into().unwrap());
        assert_eq!(
            plain,
            connect_message(sock, [10, 0, 9, 1], 80, 6000, window)
        );
        assert_eq!(ip(Socket::endpoint(&t).unwrap()), at(other, 6000));
    }
}
