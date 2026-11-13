//! Where the stack's address comes from when nobody wrote it down, and
//! where what is not for the card's network goes.
//!
//! The gateway is one address, the default route: a destination outside
//! the card's network is sent to it, and without one such a destination is
//! refused where it is asked for ([`Stack::reaches`]). It is given
//! ([`Stack::set_gateway`]) or it comes with the lease.
//!
//! The lease is smoltcp's DHCP client, one more socket of the stack's:
//! [`Stack::ask_for_lease`] adds it, and from then on every
//! [`poll`](Stack::poll) that ends with the server's answer changed puts
//! the address on the interface and the router in the route, or takes
//! both away when the lease is lost. Whoever has the stack asks
//! [`lease_news`](Stack::lease_news) after a poll: what to do about a new
//! address (tell the kernel, say so) is not the stack's to know.

use smoltcp::phy::Device;
use smoltcp::socket::{Dhcpv4Event, Dhcpv4Socket, SocketHandle};
use smoltcp::wire::{IpCidr, Ipv4Address, Ipv4Cidr};

use super::Stack;

/// What a DHCP server gave this stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The card's address and its network.
    pub address: Ipv4Cidr,
    /// The router, when the server named one.
    pub gateway: Option<Ipv4Address>,
    /// The name servers it named, in its order.
    pub dns: Vec<Ipv4Address>,
}

/// Where a stack's address comes from, as its command line says:
/// `ADDR/PREFIX`, `ADDR/PREFIX,GATEWAY` or `dhcp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    /// Written there, with the gateway when there is one.
    Written(Ipv4Cidr, Option<Ipv4Address>),
    /// Asked of a DHCP server.
    Leased,
}

impl Addressing {
    /// What `word` says, when it says one of the three. The address is
    /// one a machine can have, and the gateway another on its network:
    /// one that is not could never be asked where it is.
    pub fn parse(word: &str) -> Option<Addressing> {
        if word == "dhcp" {
            return Some(Addressing::Leased);
        }
        let (address, gateway) = match word.split_once(',') {
            Some((address, gateway)) => (address, Some(gateway.parse::<Ipv4Address>().ok()?)),
            None => (word, None),
        };
        let address = address.parse::<Ipv4Cidr>().ok()?;
        let ours = address.address();
        let near = |gateway: Ipv4Address| {
            gateway.is_unicast() && gateway != ours && address.contains_addr(&gateway)
        };
        (ours.is_unicast() && !ours.is_loopback() && gateway.is_none_or(near))
            .then_some(Addressing::Written(address, gateway))
    }
}

/// The stack's side of asking for an address.
#[derive(Default)]
pub(super) struct Leasing {
    /// The client, on a stack that asked.
    client: Option<SocketHandle>,
    /// What the server gave, while it holds.
    held: Option<Lease>,
    /// Whether that changed since [`Stack::lease_news`] last asked.
    news: bool,
}

/// No address: what an interface has before its lease, and after losing
/// it.
const NO_ADDRESS: Ipv4Cidr = Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0);

impl<D: for<'d> Device<'d>> Stack<D> {
    /// The card's address and network, when it has one.
    pub fn address(&self) -> Option<Ipv4Cidr> {
        self.iface.ip_addrs().iter().find_map(|cidr| match cidr {
            IpCidr::Ipv4(cidr) if !cidr.address().is_unspecified() => Some(*cidr),
            _ => None,
        })
    }

    /// Where what is not for the card's network goes.
    pub fn gateway(&self) -> Option<Ipv4Address> {
        self.gateway
    }

    /// Send what is not for the card's network to `gateway`, or nowhere.
    /// `false` when the interface was built with no room for a route, and
    /// then there is none.
    pub fn set_gateway(&mut self, gateway: Option<Ipv4Address>) -> bool {
        let routes = self.iface.routes_mut();
        let set = match gateway {
            Some(gateway) => routes.add_default_ipv4_route(gateway).is_ok(),
            None => {
                routes.remove_default_ipv4_route();
                true
            }
        };
        self.gateway = gateway.filter(|_| set);
        set
    }

    /// Ask a DHCP server for the card's address, and keep asking: the
    /// interface has whatever the server says from then on, starting with
    /// no address. Once.
    pub fn ask_for_lease(&mut self) {
        if self.lease.client.is_none() {
            self.put_address(NO_ADDRESS);
            self.set_gateway(None);
            self.lease.client = Some(self.set.add(Dhcpv4Socket::new()));
        }
    }

    /// Whether the address is a server's to give.
    pub fn leases(&self) -> bool {
        self.lease.client.is_some()
    }

    /// What the server gave, while it holds.
    pub fn lease(&self) -> Option<&Lease> {
        self.lease.held.as_ref()
    }

    /// When the lease changed since this was last asked: what it is now,
    /// or `None` for one that was lost.
    pub fn lease_news(&mut self) -> Option<Option<Lease>> {
        core::mem::take(&mut self.lease.news).then(|| self.lease.held.clone())
    }

    fn put_address(&mut self, address: Ipv4Cidr) {
        self.iface.update_ip_addrs(|addrs| {
            if let Some(first) = addrs.iter_mut().next() {
                *first = IpCidr::Ipv4(address);
            }
        });
    }

    /// After a poll: when the client has something new to say, make the
    /// interface what it says.
    pub(super) fn lease_turn(&mut self) {
        let Some(client) = self.lease.client else {
            return;
        };
        let held = match self.set.get::<Dhcpv4Socket>(client).poll() {
            None => return,
            Some(Dhcpv4Event::Deconfigured) => None,
            Some(Dhcpv4Event::Configured(config)) => Some(Lease {
                address: config.address,
                gateway: config.router,
                dns: config.dns_servers.iter().flatten().copied().collect(),
            }),
        };
        // The client starts by saying it has nothing, which is not news.
        if held == self.lease.held {
            return;
        }
        // The groups are said from an address: left from the one that
        // goes, joined from the one that comes.
        self.say_groups(false);
        self.put_address(held.as_ref().map_or(NO_ADDRESS, |lease| lease.address));
        self.set_gateway(held.as_ref().and_then(|lease| lease.gateway));
        self.say_groups(true);
        self.lease.held = held;
        self.lease.news = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::*;
    use crate::socks::tests::{bind, send};
    use crate::socks::Refused;
    use smoltcp::time::Instant;
    use smoltcp::wire::{DhcpMessageType, EthernetFrame, IpProtocol, Ipv4Packet};

    const GATEWAY: Ipv4Address = Ipv4Address([10, 0, 9, 2]);
    const FAR: Ipv4Address = Ipv4Address([192, 0, 2, 7]);

    /// A stack over a bench's card with room for a route, which THEIRS has
    /// not met.
    fn stack() -> (Stack<crate::port::Port>, Window) {
        let Bench { window, iface, .. } = Bench::new();
        (Stack::new(iface), window)
    }

    /// Poll at `ms`, and the frames that put on the wire, the card done
    /// with each.
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
    fn the_command_line_writes_the_address_or_says_to_ask() {
        let cidr = Ipv4Cidr::new(OUR_IP, 24);
        for (word, said) in [
            ("dhcp", Some(Addressing::Leased)),
            ("10.0.9.15/24", Some(Addressing::Written(cidr, None))),
            (
                "10.0.9.15/24,10.0.9.2",
                Some(Addressing::Written(cidr, Some(GATEWAY))),
            ),
            // A gateway that is this machine, or on another network, or
            // nobody's; an address nobody can have; and what is no word.
            ("10.0.9.15/24,10.0.9.15", None),
            ("10.0.9.15/24,10.0.8.2", None),
            ("10.0.9.15/24,10.0.9.255x", None),
            ("10.0.9.15/24,", None),
            ("10.0.9.15/24,255.255.255.255", None),
            ("0.0.0.0/0", None),
            ("127.0.0.1/8", None),
            ("224.0.0.1/4", None),
            ("10.0.9.15", None),
            ("DHCP", None),
            ("", None),
        ] {
            assert_eq!(Addressing::parse(word), said, "{word:?}");
        }
    }

    #[test]
    fn what_is_not_for_the_cards_network_goes_to_the_gateway() {
        let (mut stack, window) = stack();
        stack.request(&bind(1, 5000)).unwrap();
        // No gateway: nowhere to send it, and it is not queued.
        assert_eq!(
            stack.request(&send(1, FAR, 53, b"far")),
            Err(Refused::Unreachable)
        );
        assert!(stack.set_gateway(Some(GATEWAY)));
        assert_eq!(stack.gateway(), Some(GATEWAY));
        assert_eq!(stack.request(&send(1, FAR, 53, b"far")), Ok(true));
        // Who is asked for is the gateway, not the far machine.
        let sent = frames(&mut stack, &window, 1);
        assert_eq!(sent.len(), 1);
        assert!(is_arp_request_for(&sent[0], GATEWAY));
        // The gateway answers (it is THEIRS, at another address), and the
        // datagram goes to its card with the far machine's address on it.
        window.deliver(stack.device(), 0, &arp_reply_from(GATEWAY));
        let sent = frames(&mut stack, &window, 2);
        assert_eq!(sent.len(), 1);
        let frame = EthernetFrame::new_checked(&sent[0][..]).unwrap();
        assert_eq!(frame.dst_addr(), THEIRS);
        let ip = Ipv4Packet::new_checked(frame.payload()).unwrap();
        assert_eq!((ip.src_addr(), ip.dst_addr()), (OUR_IP, FAR));
        assert_eq!(ip.protocol(), IpProtocol::Udp);

        // Never this machine or nobody, gateway or not. (A group is not
        // the gateway's business: it goes by the card, in `groups`.)
        for nowhere in [[127, 0, 0, 1], [0, 0, 0, 0]] {
            assert_eq!(
                stack.request(&send(1, Ipv4Address(nowhere), 53, b"x")),
                Err(Refused::Unreachable),
                "{nowhere:?}"
            );
        }
        // And without the gateway again, nowhere.
        assert!(stack.set_gateway(None));
        assert_eq!(
            stack.request(&send(1, FAR, 53, b"far")),
            Err(Refused::Unreachable)
        );
    }

    #[test]
    fn a_lease_gives_the_address_the_gateway_and_the_name_servers() {
        let (mut stack, window) = stack();
        stack.ask_for_lease();
        assert!(stack.leases());
        assert_eq!(stack.address(), None);
        assert_eq!(stack.lease_news(), None);
        // With no address nothing is reachable, the card's old network
        // included.
        stack.request(&bind(1, 5000)).unwrap();
        assert_eq!(
            stack.request(&send(1, THEIR_IP, 53, b"x")),
            Err(Refused::Unreachable)
        );

        // It asks everybody.
        let sent = frames(&mut stack, &window, 1);
        assert_eq!(sent.len(), 1);
        let (kind, xid, _) = dhcp_of(&sent[0]);
        assert_eq!(kind, DhcpMessageType::Discover);
        // The server offers, and it asks for what was offered.
        let offer = Offered {
            address: OUR_IP,
            mask: Ipv4Address([255, 255, 255, 0]),
            router: Some(GATEWAY),
            dns: [Some(Ipv4Address([10, 0, 9, 3])), None, None],
            seconds: 600,
        };
        window.deliver(
            stack.device(),
            0,
            &dhcp_frame(DhcpMessageType::Offer, xid, &offer),
        );
        let sent = frames(&mut stack, &window, 2);
        assert_eq!(sent.len(), 1);
        let (kind, xid, asked) = dhcp_of(&sent[0]);
        assert_eq!((kind, asked), (DhcpMessageType::Request, Some(OUR_IP)));
        assert_eq!(stack.lease_news(), None, "an offer is not a lease");

        // The server agrees: the interface is what it said.
        window.deliver(
            stack.device(),
            1,
            &dhcp_frame(DhcpMessageType::Ack, xid, &offer),
        );
        assert!(frames(&mut stack, &window, 3).is_empty());
        let lease = Lease {
            address: Ipv4Cidr::new(OUR_IP, 24),
            gateway: Some(GATEWAY),
            dns: vec![Ipv4Address([10, 0, 9, 3])],
        };
        assert_eq!(stack.lease_news(), Some(Some(lease.clone())));
        assert_eq!(stack.lease_news(), None, "told once");
        assert_eq!(stack.lease(), Some(&lease));
        assert_eq!(stack.address(), Some(lease.address));
        assert_eq!(stack.gateway(), Some(GATEWAY));
        assert_eq!(stack.request(&send(1, THEIR_IP, 53, b"near")), Ok(true));
        assert_eq!(stack.request(&send(1, FAR, 53, b"far")), Ok(true));

        // Nobody renews it: when it runs out the address goes, and what it
        // made reachable with it.
        let mut lost = None;
        for second in 1..=700 {
            frames(&mut stack, &window, 3 + second * 1000);
            if let Some(news) = stack.lease_news() {
                lost = Some((second, news));
                break;
            }
        }
        let (second, news) = lost.expect("a lease nobody renews is lost");
        assert_eq!(news, None);
        assert!((590..=610).contains(&second), "lost after {second} s");
        assert_eq!(stack.address(), None);
        assert_eq!(stack.gateway(), None);
        assert_eq!(
            stack.request(&send(1, FAR, 53, b"far")),
            Err(Refused::Unreachable)
        );
    }

    /// The stack built by hand has a gateway only when given one, and it
    /// stays through an interface that never asks for a lease.
    #[test]
    fn a_stack_with_a_written_address_never_asks_for_one() {
        let (mut stack, window) = stack();
        assert!(!stack.leases());
        assert_eq!(stack.address(), Some(Ipv4Cidr::new(OUR_IP, 24)));
        assert!(frames(&mut stack, &window, 1).is_empty());
        assert_eq!(stack.lease_news(), None);
        assert_eq!(stack.lease(), None);
    }
}
