//! Several cards, one kernel: which card's stack a message of the
//! kernel's is for.
//!
//! A machine with more than one card has a [`Stack`] for each, in the one
//! process, and the kernel goes on speaking as if to one: it has one
//! channel, numbers every socket and gives out every port itself, and is
//! told one card, the first that had an address (the one it shows). What
//! arrives by any card is told on that channel as it is. What the kernel
//! says goes:
//!
//! * to every stack, when it makes or ends a socket that waits for what
//!   arrives: `BIND`, the `LISTEN`s, `HEAR`, `HEAR6`, `RAW`, the groups,
//!   `TAP`, `CLOSE`. A datagram for a port is taken whichever card
//!   brought it, and a connection is accepted on any.
//! * to one stack, when it has somewhere to go (`SEND`, `CONNECT`,
//!   `ICMP`, `PACKET` and the same of IPv6): the stack whose card is on
//!   the network of the destination; failing that the stack of the card
//!   the kernel shows, if it has a way there (its gateway), and failing
//!   that the first that has. One that no stack reaches goes to the shown
//!   card's, which refuses it as it would alone. What is for everybody or
//!   for a group leaves by every card that can send it, except a
//!   connection, which is made once.
//! * to the stack that has the connection, when it is about one
//!   (`WRITE`, `SHUTDOWN`, `WINDOW`). The connections that arrive are
//!   numbered apart by each stack ([`Stack::number_from`]), so a number is
//!   one stack's.
//! * to the shown card's stack, a frame to put on the wire (`FRAME`).
//!
//! What this does not do: the kernel shows one interface, so a program
//! sees one address (`getsockname`, `ifconfig`) though it is reached at
//! each card's, the frames of every card are that interface's to a packet
//! socket, and an IPv6 address of the link (`fe80::`) is taken to be on
//! the shown card's link.

use smoltcp::phy::Device;
use smoltcp::wire::{Ipv4Address, Ipv6Address};

use crate::port::Listened;
use crate::socks::icmp6::OP_ICMP6;
use crate::socks::raw::OP_PACKET;
use crate::socks::tcp::{OP_CONNECT, OP_CONNECT6, OP_SHUTDOWN, OP_WINDOW, OP_WRITE};
use crate::socks::{parse_header, Refused, Stack};
use crate::socks::{OP_FRAME, OP_ICMP, OP_SEND, OP_SEND6};

/// The most cards one process has stacks for: a bit each in what
/// [`request`] answers.
pub const MAX: usize = 8;

/// Where the numbers of the connections that arrive start for the stack
/// in place `slot` ([`Stack::number_from`]).
pub fn numbers_of(slot: usize) -> u64 {
    (slot as u64) << 48
}

/// Whom a message is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whom {
    /// Every stack.
    All,
    /// The stack that has the connection.
    Owner(u64),
    /// The stack that reaches this address; every one that does, for an
    /// address that is many machines', unless `once`.
    To { addr: Ipv4Address, once: bool },
    /// The same, of IPv6.
    To6 { addr: Ipv6Address, once: bool },
    /// The stack of the card the kernel shows.
    Shown,
}

/// Whom `m` is for. A message too short to say is everybody's to refuse.
pub fn whom(m: &[u8]) -> Whom {
    let Some((op, sock)) = parse_header(m) else {
        return Whom::Shown;
    };
    let four = |at: usize| m.get(at..at + 4).map(Ipv4Address::from_bytes);
    let sixteen = |at: usize| m.get(at..at + 16).map(Ipv6Address::from_bytes);
    let to =
        |addr: Option<Ipv4Address>, once| addr.map_or(Whom::Shown, |addr| Whom::To { addr, once });
    let to6 =
        |addr: Option<Ipv6Address>, once| addr.map_or(Whom::Shown, |addr| Whom::To6 { addr, once });
    match op {
        OP_SEND | OP_ICMP => to(four(16), false),
        OP_CONNECT => to(four(16), true),
        // The destination of the header the packet starts with.
        OP_PACKET => to(four(32), false),
        OP_SEND6 | OP_ICMP6 => to6(sixteen(16), false),
        OP_CONNECT6 => to6(sixteen(16), true),
        OP_WRITE | OP_SHUTDOWN | OP_WINDOW => Whom::Owner(sock),
        OP_FRAME => Whom::Shown,
        _ => Whom::All,
    }
}

/// The stacks `m` is for, a bit for each place in `stacks`; `shown` is
/// the place of the one whose card the kernel shows. Never none: what is
/// for nobody goes to the shown one, to be refused.
pub fn places<D>(stacks: &[&mut Stack<D>], shown: usize, m: &[u8]) -> u32
where
    D: for<'d> Device<'d> + Listened,
{
    let all = || (1u32 << stacks.len()) - 1;
    let only = 1u32 << shown;
    // One of them: on the network, then the shown one, then any.
    let one = |near: &dyn Fn(&Stack<D>) -> bool, reaches: &dyn Fn(&Stack<D>) -> bool| {
        let first = |is: &dyn Fn(&Stack<D>) -> bool| {
            stacks
                .iter()
                .position(|stack| is(stack))
                .map(|at| 1u32 << at)
        };
        first(near)
            .or_else(|| reaches(stacks[shown]).then_some(only))
            .or_else(|| first(reaches))
            .unwrap_or(only)
    };
    // Each of them that can send it.
    let each = |reaches: &dyn Fn(&Stack<D>) -> bool| {
        let mask = stacks
            .iter()
            .enumerate()
            .filter(|(_, stack)| reaches(stack))
            .fold(0, |mask, (at, _)| mask | 1u32 << at);
        if mask == 0 {
            only
        } else {
            mask
        }
    };
    match whom(m) {
        Whom::All => all(),
        Whom::Shown => only,
        Whom::Owner(sock) => each(&|stack| stack.has_stream(sock)),
        Whom::To { addr, once } if !once && (addr.is_broadcast() || addr.is_multicast()) => {
            each(&|stack| stack.reaches(addr))
        }
        Whom::To { addr, .. } => one(&|stack| stack.on_link(addr), &|stack| stack.reaches(addr)),
        Whom::To6 { addr, once } if !once && addr.is_multicast() => {
            each(&|stack| stack.reaches6(addr))
        }
        Whom::To6 { addr, .. } => one(&|stack| stack.on_link6(addr), &|stack| stack.reaches6(addr)),
    }
}

/// Do what a message from the kernel says, in the stacks it is for
/// ([`places`]): a bit for each stack that was left with something to
/// send or to tell the kernel ([`Stack::request`]), or why none of them
/// took it.
pub fn request<D>(stacks: &mut [&mut Stack<D>], shown: usize, m: &[u8]) -> Result<u32, Refused>
where
    D: for<'d> Device<'d> + Listened,
{
    debug_assert!((1..=MAX).contains(&stacks.len()) && shown < stacks.len());
    let places = places(stacks, shown, m);
    let (mut left, mut taken, mut refused) = (0, false, None);
    for (at, stack) in stacks.iter_mut().enumerate() {
        if places & 1 << at == 0 {
            continue;
        }
        match stack.request(m) {
            Ok(true) => (left, taken) = (left | 1 << at, true),
            Ok(false) => taken = true,
            Err(why) => refused = Some(why),
        }
    }
    match refused {
        Some(why) if !taken => Err(why),
        _ => Ok(left),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::Bench;
    use crate::socks::tests::{bind, message, send};
    use crate::socks::{OP_BIND, OP_CLOSE, OP_HEAR, OP_TAP};
    use smoltcp::wire::{IpAddress, IpCidr};

    type Card = Stack<crate::port::Port>;

    /// A stack whose card is `addr`/24, with this gateway.
    fn card(addr: [u8; 4], gateway: Option<[u8; 4]>) -> Card {
        let Bench { mut iface, .. } = Bench::new();
        iface.update_ip_addrs(|addrs| {
            addrs[0] = IpCidr::new(IpAddress::v4(addr[0], addr[1], addr[2], addr[3]), 24)
        });
        let mut stack = Stack::new(iface);
        stack.set_gateway(gateway.map(Ipv4Address));
        stack
    }

    /// A card with no address yet: it asks for one.
    fn unaddressed() -> Card {
        let Bench { mut iface, .. } = Bench::new();
        iface.update_ip_addrs(|addrs| addrs[0] = IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0));
        Stack::new(iface)
    }

    fn to(addr: [u8; 4]) -> Vec<u8> {
        send(7, Ipv4Address(addr), 9, b"x")
    }

    #[test]
    fn what_makes_or_ends_a_socket_is_every_stacks() {
        for op in [OP_BIND, OP_HEAR, OP_TAP, OP_CLOSE] {
            assert_eq!(whom(&message(op, 7, &[])), Whom::All);
        }
        assert_eq!(whom(&message(OP_WRITE, 7, b"abc")), Whom::Owner(7));
        assert_eq!(whom(&message(OP_FRAME, 0, &[0; 60])), Whom::Shown);
        assert_eq!(
            whom(&to([10, 0, 9, 1])),
            Whom::To {
                addr: Ipv4Address([10, 0, 9, 1]),
                once: false
            }
        );
        // The destination a packet's own header names.
        let mut packet = vec![0u8; 20];
        packet[16..20].copy_from_slice(&[10, 1, 2, 3]);
        assert_eq!(
            whom(&message(OP_PACKET, 7, &packet)),
            Whom::To {
                addr: Ipv4Address([10, 1, 2, 3]),
                once: false
            }
        );
        // Too short to say where.
        assert_eq!(whom(&message(OP_SEND, 7, &[10, 0])), Whom::Shown);
        assert_eq!(whom(&[1, 2, 3]), Whom::Shown);
    }

    #[test]
    fn a_destination_goes_by_the_card_on_its_network_and_the_rest_by_the_shown_one() {
        let mut a = card([10, 0, 2, 15], Some([10, 0, 2, 2]));
        let mut b = card([192, 168, 7, 15], Some([192, 168, 7, 1]));
        let stacks = [&mut a, &mut b];
        // On a card's network: that card, whichever is shown.
        for shown in [0, 1] {
            assert_eq!(places(&stacks, shown, &to([10, 0, 2, 2])), 0b01);
            assert_eq!(places(&stacks, shown, &to([192, 168, 7, 9])), 0b10);
        }
        // Anywhere else: the shown card's gateway.
        assert_eq!(places(&stacks, 0, &to([1, 1, 1, 1])), 0b01);
        assert_eq!(places(&stacks, 1, &to([1, 1, 1, 1])), 0b10);
        // Everybody, by both; a connection to everybody, once.
        assert_eq!(places(&stacks, 0, &to([255, 255, 255, 255])), 0b11);
        let mut connect = to([255, 255, 255, 255]);
        connect[4..8].copy_from_slice(&OP_CONNECT.to_le_bytes());
        assert_eq!(places(&stacks, 1, &connect), 0b10);
        // What makes a socket, both.
        assert_eq!(places(&stacks, 0, &bind(7, 4000)), 0b11);
    }

    #[test]
    fn a_card_with_no_way_there_is_not_asked_while_another_has_one() {
        // The shown card has no gateway, and the other has.
        let mut a = card([10, 0, 2, 15], None);
        let mut b = card([192, 168, 7, 15], Some([192, 168, 7, 1]));
        let mut c = unaddressed();
        let stacks = [&mut a, &mut b, &mut c];
        assert_eq!(places(&stacks, 0, &to([1, 1, 1, 1])), 0b010);
        assert_eq!(places(&stacks, 2, &to([1, 1, 1, 1])), 0b010);
        // A card with no address is on no network, and sends to nobody.
        assert_eq!(places(&stacks, 2, &to([10, 0, 2, 2])), 0b001);
        assert_eq!(places(&stacks, 0, &to([255, 255, 255, 255])), 0b111);
        // Nobody reaches it: the shown one, which says so.
        let mut a = card([10, 0, 2, 15], None);
        let mut b = card([192, 168, 7, 15], None);
        let mut stacks = [&mut a, &mut b];
        assert_eq!(places(&stacks, 1, &to([1, 1, 1, 1])), 0b10);
        request(&mut stacks, 1, &bind(7, 4000)).unwrap();
        assert_eq!(
            request(&mut stacks, 1, &to([1, 1, 1, 1])),
            Err(Refused::Unreachable)
        );
    }

    #[test]
    fn a_socket_is_in_every_stack_and_its_datagram_leaves_by_one() {
        let mut a = card([10, 0, 2, 15], Some([10, 0, 2, 2]));
        let mut b = card([192, 168, 7, 15], None);
        let mut stacks = [&mut a, &mut b];
        assert_eq!(request(&mut stacks, 0, &bind(7, 4000)), Ok(0));
        assert_eq!(request(&mut stacks, 0, &to([192, 168, 7, 9])), Ok(0b10));
        assert_eq!(request(&mut stacks, 0, &to([8, 8, 8, 8])), Ok(0b01));
        assert_eq!((stacks[0].sockets(), stacks[1].sockets()), (1, 1));
        // A second `BIND` of it is refused by both.
        assert_eq!(request(&mut stacks, 0, &bind(7, 4000)), Err(Refused::Bind));
        assert_eq!(request(&mut stacks, 0, &message(OP_CLOSE, 7, &[])), Ok(0));
        assert_eq!((stacks[0].sockets(), stacks[1].sockets()), (0, 0));
    }

    #[test]
    fn what_is_about_a_connection_goes_to_the_stack_that_has_it() {
        let mut a = card([10, 0, 2, 15], Some([10, 0, 2, 2]));
        let mut b = card([192, 168, 7, 15], None);
        let mut stacks = [&mut a, &mut b];
        // CONNECT: to, port, local port, window.
        let mut said = vec![192, 168, 7, 9];
        said.extend_from_slice(&80u16.to_le_bytes());
        said.extend_from_slice(&40000u16.to_le_bytes());
        said.extend_from_slice(&65535u32.to_le_bytes());
        assert_eq!(
            request(&mut stacks, 0, &message(OP_CONNECT, 9, &said)),
            Ok(0b10)
        );
        assert!(!stacks[0].has_stream(9) && stacks[1].has_stream(9));
        assert_eq!(places(&stacks, 0, &message(OP_SHUTDOWN, 9, &[])), 0b10);
        // One nobody has: the shown stack says so.
        assert_eq!(places(&stacks, 0, &message(OP_SHUTDOWN, 5, &[])), 0b01);
        assert_eq!(
            request(&mut stacks, 0, &message(OP_WRITE, 5, b"abc")),
            Err(Refused::NoSocket)
        );
        // And each stack numbers what it accepts apart.
        assert_ne!(numbers_of(0), numbers_of(1));
        assert_eq!(numbers_of(MAX - 1) >> 63, 0);
    }
}
