//! More than one address on a card, and more ways out than the gateway.
//!
//! A card has one address of IPv4 that a lease gives or somebody writes
//! ([`super::lease`]), and the addresses of IPv6 its link's router makes
//! ([`super::six`]). Somebody on the machine can give it others (`ip
//! addr add`), of either kind, and say where what is for some network
//! goes (`ip route add NETWORK via ROUTER`), besides where everything
//! else does. The kernel asks, with a message about no socket that names
//! the card where a socket would be:
//!
//! ```text
//! ALSO   (kernel to stack)  16  u8 4 or 6  17  u8 the bits of the prefix
//!                           18  u8 1 to have it, 0 to have it no more  19  u8 zero
//!                           20  u32 which wish of the kernel's this is
//!                           24  [u8; 16] the address (of IPv4: its four bytes, and zeroes)
//! ROUTE  (kernel to stack)  the same, the network at 24, and
//!                           40  [u8; 16] the router, or zeroes
//! ```
//!
//! What they come to:
//!
//! * An address of IPv4 for a card that has none is its first, as if it
//!   had been set ([`Stack::write_address`]): no server is asked for one
//!   any more. For a card that has one it is another, and the first
//!   stays as it is, a lease's included. The first is what the card
//!   speaks from to where no address of its is nearer.
//! * Taking the first away leaves the next in its place, except on a
//!   card that asks a server for its own: that one asks again
//!   ([`Stack::ask_again`]) and keeps the others.
//! * An address of IPv6 is one more of the card's until it is taken
//!   away; the link's own cannot be.
//! * A route is to a network, through a machine of a network of the
//!   card's; the route to everything is the gateway (IPv4) or the
//!   router (IPv6), which a router of the link no longer changes once
//!   it is written by hand. The route that fits most bits wins.
//! * When an address goes, so do the gateway and the routes whose way
//!   out was on its network.
//!
//! The kernel shows the card, so it is told what the card has of this,
//! all of it each time it changes:
//!
//! ```text
//! MORE   (stack to kernel)  16  one after another, to the end:
//!             0  u8 1 an address, 2 a route  1  u8 4 or 6  2  u8 prefix  3  five zero
//!             8  [u8; 16] the address or the network  24  [u8; 16] the router, or zeroes
//! ```
//!
//! with the addresses of IPv4 after the first (the first is in `CARD`,
//! the ones of IPv6 in `SIX`) and the routes to less than everything.
//!
//! The kernel shows what it wished at once, and two wishes can follow
//! each other faster than the first is answered: what the stack says
//! after the first would undo, for a moment, what the kernel shows of
//! the second, and a router on the network of an address just added
//! was refused for it. So the kernel numbers its wishes, and a `MORE`
//! and a `SIX` say in their first four bytes the last wish the stack
//! had done when it said them ([`Stack::wished`]): the kernel does not
//! listen to one from before its last wish.

use managed::ManagedSlice;
use smoltcp::iface::Route;
use smoltcp::phy::Device;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};

use super::{parse_header, Refused, Stack, HEADER_LEN};

/// The operations of the messages.
pub const OP_ALSO: u32 = 36;
pub const OP_ROUTE: u32 = 37;
pub const OP_MORE: u32 = 38;

/// The addresses of IPv4 a card has after its first, at most.
pub const ADDRESSES: usize = 8;
/// The routes to less than everything, at most.
pub const ROUTES: usize = 16;

/// What each entry of a `MORE` takes.
pub const MORE_ENTRY_LEN: usize = 40;
const AN_ADDRESS: u8 = 1;
const A_ROUTE: u8 = 2;

/// What the kernel asks of a card with an `ALSO` or a `ROUTE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wish {
    /// To have this address too (`true`), or no more.
    Also(IpCidr, bool),
    /// To send what is for this network through this router (`true`),
    /// or no more: then the router may go unsaid.
    Route(IpCidr, Option<IpAddress>, bool),
}

/// The address and prefix in sixteen bytes that `family` says how to
/// read: `None` for a family there is none of or a prefix of more bits
/// than its addresses, or bytes past an address of IPv4.
fn cidr_of(family: u8, prefix: u8, bytes: &[u8]) -> Option<IpCidr> {
    match family {
        4 if prefix <= 32 && bytes[4..] == [0; 12] => Some(IpCidr::Ipv4(Ipv4Cidr::new(
            Ipv4Address::from_bytes(&bytes[..4]),
            prefix,
        ))),
        6 if prefix <= 128 => Some(IpCidr::Ipv6(Ipv6Cidr::new(
            Ipv6Address::from_bytes(bytes),
            prefix,
        ))),
        _ => None,
    }
}

/// The sixteen bytes an address travels in.
fn sixteen(address: &IpAddress) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    let said = address.as_bytes();
    bytes[..said.len()].copy_from_slice(said);
    bytes
}

/// What `m` asks, and of which card, when it is an `ALSO` or a `ROUTE`:
/// `Err` for one that names what no card has.
pub fn wished(m: &[u8]) -> Option<Result<(u64, Wish, u32), Refused>> {
    let (op, card) = parse_header(m)?;
    if op != OP_ALSO && op != OP_ROUTE {
        return None;
    }
    let said = &m[HEADER_LEN..];
    let whole = if op == OP_ALSO { 24 } else { 40 };
    let wish = || {
        if said.len() != whole || said[3] != 0 || said[2] > 1 {
            return None;
        }
        let (family, have) = (said[0], said[2] == 1);
        let which = u32::from_le_bytes([said[4], said[5], said[6], said[7]]);
        let numbered = |wish: Wish| (card, wish, which);
        let first = cidr_of(family, said[1], &said[8..24])?;
        if op == OP_ALSO {
            let address = first.address();
            let machine = address.is_unicast() && !address.is_loopback();
            let wish = Wish::Also(first, have);
            return (machine && first.prefix_len() > 0).then(|| numbered(wish));
        }
        let router = cidr_of(family, 0, &said[24..40])?.address();
        let router = (!router.is_unspecified()).then_some(router);
        (router.is_some() || !have).then(|| numbered(Wish::Route(network(first), router, have)))
    };
    Some(wish().ok_or(Refused::Malformed))
}

/// `cidr` with nothing past its prefix: the network it names.
pub fn network(cidr: IpCidr) -> IpCidr {
    match cidr {
        IpCidr::Ipv4(cidr) => IpCidr::Ipv4(cidr.network()),
        IpCidr::Ipv6(cidr) => IpCidr::Ipv6(cidr.network()),
        #[allow(unreachable_patterns)]
        other => other,
    }
}

/// The stack's side of the routes.
#[derive(Default)]
pub(super) struct More {
    /// The routes to less than everything: the network and its router.
    ways: Vec<(IpCidr, IpAddress)>,
    /// Whether the other addresses or the routes changed since
    /// [`Stack::more_news`] last asked.
    news: bool,
    /// The last wish of the kernel's that was done here, or refused.
    wished: u32,
}

impl<D: for<'d> Device<'d> + crate::port::Listened> Stack<D> {
    /// The card's addresses of IPv4 after its first.
    pub fn others(&self) -> Vec<Ipv4Cidr> {
        let after = self.iface.ip_addrs().iter().skip(1);
        after
            .filter_map(|cidr| match cidr {
                IpCidr::Ipv4(cidr) => Some(*cidr),
                _ => None,
            })
            .collect()
    }

    /// The routes to less than everything.
    pub fn ways(&self) -> &[(IpCidr, IpAddress)] {
        &self.more.ways
    }

    /// Whether a route to less than everything is the way to `addr`.
    pub fn has_way(&self, addr: IpAddress) -> bool {
        self.more.ways.iter().any(|(to, _)| to.contains_addr(&addr))
    }

    /// Whether the other addresses or the routes changed since this was
    /// last asked.
    pub fn more_news(&mut self) -> bool {
        core::mem::take(&mut self.more.news)
    }

    /// The kernel's wish number `which` was done, or refused: what is
    /// said of the card from now on is from after it.
    pub fn wish_done(&mut self, which: u32) {
        self.more.wished = which;
    }

    /// The last wish of the kernel's that was done here.
    pub fn wished(&self) -> u32 {
        self.more.wished
    }

    /// The `MORE` that says them.
    pub fn more_message(&self) -> Vec<u8> {
        let mut m = vec![0u8; HEADER_LEN];
        m[..4].copy_from_slice(&self.more.wished.to_le_bytes());
        m[4..8].copy_from_slice(&OP_MORE.to_le_bytes());
        let mut entry = |kind: u8, cidr: IpCidr, router: Option<&IpAddress>| {
            let family = if matches!(cidr, IpCidr::Ipv4(_)) {
                4
            } else {
                6
            };
            m.extend_from_slice(&[kind, family, cidr.prefix_len(), 0, 0, 0, 0, 0]);
            m.extend_from_slice(&sixteen(&cidr.address()));
            m.extend_from_slice(&router.map_or([0; 16], sixteen));
        };
        for other in self.others() {
            entry(AN_ADDRESS, IpCidr::Ipv4(other), None);
        }
        for (to, router) in &self.more.ways {
            entry(A_ROUTE, *to, Some(router));
        }
        m
    }

    /// Whether `router` is a machine to send through: on a network of
    /// one of the card's addresses of IPv4, and not the card.
    pub(super) fn near(&self, router: Ipv4Address) -> bool {
        let mut ours = self.iface.ip_addrs().iter().filter_map(|cidr| match cidr {
            IpCidr::Ipv4(cidr) if !cidr.address().is_unspecified() => Some(*cidr),
            _ => None,
        });
        router.is_unicast()
            && !self.iface.has_ip_addr(router)
            && ours.any(|cidr| cidr.contains_addr(&router))
    }

    fn near_any(&self, router: &IpAddress) -> bool {
        match router {
            IpAddress::Ipv4(router) => self.near(*router),
            IpAddress::Ipv6(router) => self.near6(*router),
            _ => false,
        }
    }

    /// Do what the kernel wishes of the card. `Err`, and nothing
    /// changed, for what the card cannot have: a router that is not
    /// near, an address or a route past what there is room for, one to
    /// take away that is not there.
    pub fn grant(&mut self, wish: Wish) -> Result<(), Refused> {
        let done = match wish {
            Wish::Also(IpCidr::Ipv4(cidr), true) => self.also(cidr),
            Wish::Also(IpCidr::Ipv4(cidr), false) => self.no_more(cidr.address()),
            Wish::Also(IpCidr::Ipv6(cidr), true) => self.write_six(cidr),
            Wish::Also(IpCidr::Ipv6(cidr), false) => self.unwrite_six(cidr.address()),
            Wish::Route(to, router, have) if to.prefix_len() == 0 => match (to, router) {
                (IpCidr::Ipv4(_), Some(IpAddress::Ipv4(router))) if have => {
                    self.write_gateway(Some(router))
                }
                (IpCidr::Ipv6(_), Some(IpAddress::Ipv6(router))) if have => {
                    self.write_router(Some(router))
                }
                (IpCidr::Ipv4(_), _) if !have => self.gateway.is_some() && self.set_gateway(None),
                (IpCidr::Ipv6(_), _) if !have => self.write_router(None),
                _ => false,
            },
            Wish::Route(to, Some(router), true) => self.way(to, router),
            Wish::Route(to, router, false) => self.no_way(to, router),
            _ => false,
        };
        if done {
            Ok(())
        } else {
            Err(Refused::Unreachable)
        }
    }

    /// One more address of IPv4, or the first of a card that has none.
    fn also(&mut self, cidr: Ipv4Cidr) -> bool {
        if self.iface.has_ip_addr(cidr.address()) {
            return false;
        }
        if self.address().is_none() {
            self.write_address(Some(cidr));
            return true;
        }
        if self.others().len() >= ADDRESSES {
            return false;
        }
        self.iface.update_ip_addrs(|addrs| {
            // The list is the `Vec` whoever built the interface gave it.
            if let ManagedSlice::Owned(addrs) = addrs {
                addrs.push(IpCidr::Ipv4(cidr));
            }
        });
        self.more.news = true;
        true
    }

    /// An address of IPv4 less.
    fn no_more(&mut self, address: Ipv4Address) -> bool {
        let first = self.address().is_some_and(|a| a.address() == address);
        if first && self.leases() {
            self.ask_again();
            return true;
        }
        if first {
            // The next takes its place, when there is one.
            let next = self.others().first().copied();
            if let Some(next) = next {
                self.drop_other(next.address());
            }
            self.write_address(next);
            return true;
        }
        if !self.others().iter().any(|o| o.address() == address) {
            return false;
        }
        self.drop_other(address);
        self.prune_ways();
        true
    }

    fn drop_other(&mut self, address: Ipv4Address) {
        let address = IpAddress::Ipv4(address);
        self.iface.update_ip_addrs(|addrs| {
            if let ManagedSlice::Owned(addrs) = addrs {
                let mut at = 0;
                addrs.retain(|have| {
                    at += 1;
                    at == 1 || have.address() != address
                });
            }
        });
        self.more.news = true;
    }

    /// What is for `to` goes through `router` from now on.
    fn way(&mut self, to: IpCidr, router: IpAddress) -> bool {
        let same = matches!(
            (&to, &router),
            (IpCidr::Ipv4(_), IpAddress::Ipv4(_)) | (IpCidr::Ipv6(_), IpAddress::Ipv6(_))
        );
        let known = self.more.ways.iter().position(|(have, _)| *have == to);
        if !same || !self.near_any(&router) || (known.is_none() && self.more.ways.len() >= ROUTES) {
            return false;
        }
        let route = Route {
            via_router: router,
            preferred_until: None,
            expires_at: None,
        };
        let mut put = false;
        self.iface
            .routes_mut()
            .update(|routes| put = routes.insert(to, route).is_ok());
        if !put {
            return false;
        }
        match known {
            Some(at) => self.more.ways[at].1 = router,
            None => self.more.ways.push((to, router)),
        }
        self.more.news = true;
        true
    }

    /// What is for `to` goes that way no more: through `router`, when
    /// one is said.
    fn no_way(&mut self, to: IpCidr, router: Option<IpAddress>) -> bool {
        let is = |(have, through): &(IpCidr, IpAddress)| {
            *have == to && router.is_none_or(|router| router == *through)
        };
        let Some(at) = self.more.ways.iter().position(is) else {
            return false;
        };
        self.more.ways.remove(at);
        self.iface.routes_mut().update(|routes| {
            routes.remove(&to);
        });
        self.more.news = true;
        true
    }

    /// After an address went: the gateway and the routes whose way out
    /// was on its network go with it.
    pub(super) fn prune_ways(&mut self) {
        if self.gateway.is_some_and(|gateway| !self.near(gateway)) {
            self.set_gateway(None);
        }
        let gone: Vec<IpCidr> = self
            .more
            .ways
            .iter()
            .filter(|(_, router)| !self.near_any(router))
            .map(|(to, _)| *to)
            .collect();
        for to in gone {
            self.no_way(to, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::*;
    use crate::socks::tests::{bind, send};
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetFrame, Ipv4Packet};

    const GATEWAY: Ipv4Address = Ipv4Address([10, 0, 9, 2]);
    const FAR: Ipv4Address = Ipv4Address([192, 0, 2, 7]);

    fn stack() -> (Stack<crate::port::Port>, Window) {
        let Bench { window, iface, .. } = Bench::new();
        (Stack::new(iface), window)
    }

    fn v4(a: [u8; 4], prefix: u8) -> IpCidr {
        IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address(a), prefix))
    }

    fn v6(text: &str, prefix: u8) -> IpCidr {
        IpCidr::Ipv6(Ipv6Cidr::new(text.parse().unwrap(), prefix))
    }

    /// The message the kernel asks with, as `linux-object`'s
    /// `user_stack/more.rs` writes it.
    fn ask(op: u32, card: u64, have: bool, first: IpCidr, router: Option<IpAddress>) -> Vec<u8> {
        let mut m = vec![0u8; HEADER_LEN];
        m[4..8].copy_from_slice(&op.to_le_bytes());
        m[8..16].copy_from_slice(&card.to_le_bytes());
        let family = if matches!(first, IpCidr::Ipv4(_)) {
            4
        } else {
            6
        };
        m.extend_from_slice(&[family, first.prefix_len(), have as u8, 0, 9, 0, 0, 0]);
        m.extend_from_slice(&sixteen(&first.address()));
        if op == OP_ROUTE {
            m.extend_from_slice(&router.as_ref().map_or([0; 16], sixteen));
        }
        m
    }

    fn frames(stack: &mut Stack<crate::port::Port>, window: &Window, ms: i64) -> Vec<Vec<u8>> {
        stack.poll(Instant::from_millis(ms));
        let turn = turn(stack.device());
        let mut out = Vec::new();
        for m in &turn {
            if m.op == udrivers::net::OP_TX {
                out.push(window.sent(m));
                window.done(stack.device(), m);
            }
        }
        out
    }

    #[test]
    fn what_the_kernel_wishes_is_read_as_it_writes_it() {
        let other = v4([192, 168, 7, 9], 24);
        assert_eq!(
            wished(&ask(OP_ALSO, 2, true, other, None)),
            Some(Ok((2, Wish::Also(other, true), 9)))
        );
        let six = v6("2001:db8::5", 64);
        assert_eq!(
            wished(&ask(OP_ALSO, 0, false, six, None)),
            Some(Ok((0, Wish::Also(six, false), 9)))
        );
        // A route is to a network: what is past the prefix is not said.
        let through = IpAddress::Ipv4(GATEWAY);
        assert_eq!(
            wished(&ask(
                OP_ROUTE,
                1,
                true,
                v4([192, 0, 2, 7], 24),
                Some(through)
            )),
            Some(Ok((
                1,
                Wish::Route(v4([192, 0, 2, 0], 24), Some(through), true),
                9
            )))
        );
        // Taken away with no router said; had with none, never.
        let to = v6("2001:db8:7::", 48);
        assert_eq!(
            wished(&ask(OP_ROUTE, 0, false, to, None)),
            Some(Ok((0, Wish::Route(to, None, false), 9)))
        );
        assert_eq!(
            wished(&ask(OP_ROUTE, 0, true, to, None)),
            Some(Err(Refused::Malformed))
        );
        // No machine's address, a prefix of too many bits, a family
        // there is none of, a message cut short.
        for bad in [
            ask(OP_ALSO, 0, true, v4([224, 0, 0, 1], 4), None),
            ask(OP_ALSO, 0, true, v4([127, 0, 0, 1], 8), None),
            ask(OP_ALSO, 0, true, v4([10, 0, 9, 9], 0), None),
            ask(OP_ALSO, 0, true, v6("ff02::1", 64), None),
        ] {
            assert_eq!(wished(&bad), Some(Err(Refused::Malformed)), "{bad:?}");
        }
        let mut bad = ask(OP_ALSO, 0, true, other, None);
        bad[HEADER_LEN + 1] = 33;
        assert_eq!(wished(&bad), Some(Err(Refused::Malformed)));
        bad[HEADER_LEN + 1] = 24;
        bad[HEADER_LEN] = 5;
        assert_eq!(wished(&bad), Some(Err(Refused::Malformed)));
        bad[HEADER_LEN] = 4;
        bad.pop();
        assert_eq!(wished(&bad), Some(Err(Refused::Malformed)));
        // Any other message is not one of these.
        assert_eq!(wished(&bind(1, 5000)), None);
    }

    #[test]
    fn a_card_has_more_addresses_than_its_first_and_answers_for_each() {
        let (mut stack, window) = stack();
        let first = Ipv4Cidr::new(OUR_IP, 24);
        let other = Ipv4Cidr::new(Ipv4Address([192, 168, 7, 9]), 24);
        assert_eq!(stack.grant(Wish::Also(IpCidr::Ipv4(other), true)), Ok(()));
        assert_eq!(stack.address(), Some(first));
        assert_eq!(stack.others(), [other]);
        assert!(stack.more_news() && !stack.more_news());
        // Twice is once.
        assert_eq!(
            stack.grant(Wish::Also(IpCidr::Ipv4(other), true)),
            Err(Refused::Unreachable)
        );
        // Its network is the card's: reached with nobody between.
        assert!(stack.on_link(Ipv4Address([192, 168, 7, 1])));
        stack.request(&bind(1, 5000)).unwrap();
        let near = Ipv4Address([192, 168, 7, 1]);
        assert_eq!(stack.request(&send(1, near, 53, b"near")), Ok(true));
        let sent = frames(&mut stack, &window, 1);
        assert_eq!(sent.len(), 1);
        assert!(is_arp_request_for(&sent[0], near));
        // The message that says it.
        stack.wish_done(7);
        let m = stack.more_message();
        assert_eq!(parse_header(&m), Some((OP_MORE, 0)));
        // From after the kernel's seventh wish.
        assert_eq!(m[..4], 7u32.to_le_bytes());
        assert_eq!(
            stack.sixes().message(stack.wished())[..4],
            7u32.to_le_bytes()
        );
        let mut entry = vec![AN_ADDRESS, 4, 24, 0, 0, 0, 0, 0, 192, 168, 7, 9];
        entry.resize(MORE_ENTRY_LEN, 0);
        assert_eq!(m[HEADER_LEN..], entry[..]);
        // Taking the first away leaves the other in its place.
        assert_eq!(stack.grant(Wish::Also(IpCidr::Ipv4(first), false)), Ok(()));
        assert_eq!(stack.address(), Some(other));
        assert!(stack.others().is_empty());
        assert!(!stack.on_link(THEIR_IP));
        // And the last leaves none; one that is not there is not taken.
        assert_eq!(
            stack.grant(Wish::Also(IpCidr::Ipv4(first), false)),
            Err(Refused::Unreachable)
        );
        assert_eq!(stack.grant(Wish::Also(IpCidr::Ipv4(other), false)), Ok(()));
        assert_eq!(stack.address(), None);
        // An address for a card with none is its first.
        assert_eq!(stack.grant(Wish::Also(IpCidr::Ipv4(other), true)), Ok(()));
        assert_eq!(stack.address(), Some(other));
        // No more than there is room for.
        for n in 0..ADDRESSES as u8 {
            let more = v4([172, 16, n, 1], 24);
            assert_eq!(stack.grant(Wish::Also(more, true)), Ok(()));
        }
        assert_eq!(
            stack.grant(Wish::Also(v4([172, 17, 0, 1], 24), true)),
            Err(Refused::Unreachable)
        );
    }

    #[test]
    fn a_card_that_leases_keeps_its_lease_and_its_other_addresses() {
        let (mut stack, _window) = stack();
        stack.ask_for_lease();
        // Its first, while it has none: nobody is asked any more.
        let written = v4([192, 168, 7, 9], 24);
        assert_eq!(stack.grant(Wish::Also(written, true)), Ok(()));
        assert!(!stack.leases());
        // Asking again, another is only another, and taking the first
        // away asks again and leaves the other.
        stack.ask_again();
        assert!(stack.leases() && stack.address().is_none());
        stack.put_address(Ipv4Cidr::new(OUR_IP, 24));
        let other = v4([192, 168, 8, 9], 24);
        assert_eq!(stack.grant(Wish::Also(other, true)), Ok(()));
        assert!(stack.leases());
        assert_eq!(stack.grant(Wish::Also(v4(OUR_IP.0, 24), false)), Ok(()));
        assert!(stack.leases());
        assert_eq!(stack.address(), None);
        assert_eq!(stack.others().len(), 1);
    }

    #[test]
    fn what_is_for_a_network_goes_through_its_router_and_the_rest_by_the_gateway() {
        let (mut stack, window) = stack();
        let router = Ipv4Address([10, 0, 9, 7]);
        let to = v4([192, 0, 2, 0], 24);
        stack.request(&bind(1, 5000)).unwrap();
        assert_eq!(
            stack.request(&send(1, FAR, 53, b"far")),
            Err(Refused::Unreachable)
        );
        // A router is a machine of a network of the card's.
        let away = IpAddress::v4(10, 0, 8, 7);
        assert_eq!(
            stack.grant(Wish::Route(to, Some(away), true)),
            Err(Refused::Unreachable)
        );
        assert_eq!(
            stack.grant(Wish::Route(to, Some(IpAddress::Ipv4(OUR_IP)), true)),
            Err(Refused::Unreachable)
        );
        assert_eq!(
            stack.grant(Wish::Route(to, Some(IpAddress::Ipv4(router)), true)),
            Ok(())
        );
        assert_eq!(stack.ways(), [(to, IpAddress::Ipv4(router))]);
        assert!(stack.has_way(IpAddress::Ipv4(FAR)) && stack.reaches(FAR));
        assert!(!stack.reaches(Ipv4Address([192, 0, 3, 7])));
        // Who is asked for is the router.
        assert_eq!(stack.request(&send(1, FAR, 53, b"far")), Ok(true));
        let sent = frames(&mut stack, &window, 1);
        assert_eq!(sent.len(), 1);
        assert!(is_arp_request_for(&sent[0], router));
        window.deliver(stack.device(), 0, &arp_reply_from(router));
        let sent = frames(&mut stack, &window, 2);
        assert_eq!(sent.len(), 1);
        let frame = EthernetFrame::new_checked(&sent[0][..]).unwrap();
        let ip = Ipv4Packet::new_checked(frame.payload()).unwrap();
        assert_eq!((ip.src_addr(), ip.dst_addr()), (OUR_IP, FAR));
        // The rest goes by the gateway, and the network by its router
        // still: the route that fits most bits.
        assert_eq!(
            stack.grant(Wish::Route(v4([0; 4], 0), Some(GATEWAY.into()), true)),
            Ok(())
        );
        assert_eq!(stack.gateway(), Some(GATEWAY));
        let elsewhere = Ipv4Address([198, 51, 100, 1]);
        assert_eq!(stack.request(&send(1, elsewhere, 53, b"x")), Ok(true));
        let sent = frames(&mut stack, &window, 3);
        assert!(is_arp_request_for(&sent[0], GATEWAY));
        // The message that says it: the route, and not the gateway.
        let m = stack.more_message();
        let mut entry = vec![A_ROUTE, 4, 24, 0, 0, 0, 0, 0, 192, 0, 2, 0];
        entry.resize(24, 0);
        entry.extend_from_slice(&[10, 0, 9, 7]);
        entry.resize(MORE_ENTRY_LEN, 0);
        assert_eq!(m[HEADER_LEN..], entry[..]);
        // Taken away through another router it stays; through its own,
        // or through none said, it goes.
        assert_eq!(
            stack.grant(Wish::Route(to, Some(GATEWAY.into()), false)),
            Err(Refused::Unreachable)
        );
        assert_eq!(stack.grant(Wish::Route(to, None, false)), Ok(()));
        assert!(stack.ways().is_empty());
        assert_eq!(stack.grant(Wish::Route(v4([0; 4], 0), None, false)), Ok(()));
        assert_eq!(stack.gateway(), None);
        assert_eq!(
            stack.request(&send(1, FAR, 53, b"far")),
            Err(Refused::Unreachable)
        );
    }

    #[test]
    fn a_route_goes_with_the_address_whose_network_its_router_was_on() {
        let (mut stack, _window) = stack();
        let other = v4([192, 168, 7, 9], 24);
        let through = IpAddress::v4(192, 168, 7, 1);
        let to = v4([172, 16, 0, 0], 12);
        assert_eq!(stack.grant(Wish::Also(other, true)), Ok(()));
        assert_eq!(stack.grant(Wish::Route(to, Some(through), true)), Ok(()));
        assert_eq!(
            stack.grant(Wish::Route(v4([0; 4], 0), Some(through), true)),
            Ok(())
        );
        // The first address goes: the other is the first now, and the
        // ways through its network stay.
        assert_eq!(stack.grant(Wish::Also(v4(OUR_IP.0, 24), false)), Ok(()));
        assert_eq!(stack.ways().len(), 1);
        assert!(stack.gateway().is_some());
        assert_eq!(stack.grant(Wish::Also(other, false)), Ok(()));
        assert!(stack.ways().is_empty());
        assert_eq!(stack.gateway(), None);
        assert!(!stack.reaches(Ipv4Address([172, 16, 1, 1])));
        // No more routes than there is room for.
        assert_eq!(stack.grant(Wish::Also(other, true)), Ok(()));
        for n in 0..ROUTES as u8 {
            let to = v4([172, 16, n, 0], 24);
            assert_eq!(stack.grant(Wish::Route(to, Some(through), true)), Ok(()));
        }
        assert_eq!(
            stack.grant(Wish::Route(v4([172, 17, 0, 0], 24), Some(through), true)),
            Err(Refused::Unreachable)
        );
        // One there is can be sent another way.
        let another = IpAddress::v4(192, 168, 7, 2);
        let to = v4([172, 16, 0, 0], 24);
        assert_eq!(stack.grant(Wish::Route(to, Some(another), true)), Ok(()));
        assert_eq!(stack.ways().len(), ROUTES);
        assert!(stack.ways().contains(&(to, another)));
    }

    #[test]
    fn an_address_and_a_router_of_ipv6_are_written_by_hand() {
        let (mut stack, _window) = stack();
        let ours = v6("2001:db8::5", 64);
        // Not on a stack that speaks no IPv6.
        assert_eq!(
            stack.grant(Wish::Also(ours, true)),
            Err(Refused::Unreachable)
        );
        stack.speak_six();
        assert!(stack.six_news());
        assert_eq!(stack.grant(Wish::Also(ours, true)), Ok(()));
        assert!(stack.six_news());
        let link = stack.sixes().addresses[0];
        let IpCidr::Ipv6(written) = ours else {
            unreachable!()
        };
        assert_eq!(stack.sixes().addresses, [link, written]);
        assert!(stack.on_link6("2001:db8::9".parse().unwrap()));
        let far: Ipv6Address = "2001:db8:ffff::1".parse().unwrap();
        assert!(!stack.reaches6(far));
        // A router on the link, or on the address's network; no other.
        let default = v6("::", 0);
        let away: Ipv6Address = "2001:db8:1::1".parse().unwrap();
        assert_eq!(
            stack.grant(Wish::Route(default, Some(away.into()), true)),
            Err(Refused::Unreachable)
        );
        let router: Ipv6Address = "fe80::1".parse().unwrap();
        assert_eq!(
            stack.grant(Wish::Route(default, Some(router.into()), true)),
            Ok(())
        );
        assert_eq!(stack.sixes().router, Some(router));
        assert!(stack.reaches6(far));
        // A network of its own way.
        let to = v6("2001:db8:7::", 48);
        let through: Ipv6Address = "2001:db8::1".parse().unwrap();
        assert_eq!(
            stack.grant(Wish::Route(to, Some(through.into()), true)),
            Ok(())
        );
        assert!(stack.has_way("2001:db8:7::9".parse::<Ipv6Address>().unwrap().into()));
        // The router goes: only the network's way is left.
        assert_eq!(stack.grant(Wish::Route(default, None, false)), Ok(()));
        assert_eq!(stack.sixes().router, None);
        assert!(!stack.reaches6(far));
        assert!(stack.reaches6("2001:db8:7::9".parse().unwrap()));
        // The address goes, and the way through its network with it.
        // The link's own stays, whoever asks.
        assert_eq!(stack.grant(Wish::Also(ours, false)), Ok(()));
        assert_eq!(stack.sixes().addresses, [link]);
        assert!(stack.ways().is_empty());
        assert_eq!(
            stack.grant(Wish::Also(IpCidr::Ipv6(link), false)),
            Err(Refused::Unreachable)
        );
        assert_eq!(
            stack.grant(Wish::Also(ours, false)),
            Err(Refused::Unreachable)
        );
    }
}
