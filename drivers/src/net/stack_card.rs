//! The card of a network stack that is not the kernel's.
//!
//! When the stack runs as a process and every program's sockets are that
//! stack's, the card that carries them is not in the kernel's list:
//! `ifconfig`, `ip` and `/proc/net` showed `lo` and nothing else, and a
//! program that waits for a default route before it starts waited until it
//! gave up. A [`StackCard`] is that card in the list, to be looked at: a
//! name, the MAC, and the address and the gateway the stack says it has.
//!
//! Its address and its gateway are the stack's. The card shows what the
//! stack said of them last ([`StackCard::set`]): none before its lease, the
//! lease's after, another when it changes. To set them from here (`ip addr`,
//! `ip route`, `ifconfig`) is to ask the stack, through the same [`Wire`] a
//! frame goes by ([`Wire::configure`]): one IPv4 address, and the gateway
//! for what is not on its network. It is made when the stack says what its
//! card is and taken away when the stack is gone.
//!
//! A frame sent on it goes to the stack, which has the card and puts it on
//! the wire ([`Wire`]). It used to be refused, always and with
//! `NotSupported`, and that is what an `AF_PACKET` socket met on a boot where
//! the stack runs as a process: `udhcpc6`, which builds its own IPv6 and UDP
//! headers and sends the DHCPv6 SOLICIT through one, could not put a single
//! frame on the wire, and neither could `udhcpc` or a tcpdump. A card with no
//! wire -- one nobody has told where its stack is -- still refuses, as it
//! did.
//!
//! What does change is how much the card has carried, which only the stack
//! knows and says as it goes: the card is given the place those counts are
//! kept ([`Carried`]), and answers with what is there when it is asked
//! (`/proc/net/dev`, which is where `ifconfig` reads them). And its IPv6
//! addresses, which its link's router gives and takes away: the card is
//! given the place the stack's last word of them is kept ([`Sixes`]), and
//! shows them and the router after what the stack said at first.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use smoltcp::wire::{
    EthernetAddress, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr,
};

use super::ip_cfg;
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::sync::Mutex;
use crate::{DeviceError, DeviceResult};

/// What a card has carried so far, kept by whoever is told: the frames
/// and the bytes each way, and the ones that came to nothing. It only
/// grows, and it is not the card's: the stack that had the card may be
/// gone and the next one's card goes on from the same counts.
#[derive(Default)]
pub struct Carried {
    rx_bytes: AtomicU64,
    rx_packets: AtomicU64,
    rx_errors: AtomicU64,
    rx_dropped: AtomicU64,
    tx_bytes: AtomicU64,
    tx_packets: AtomicU64,
    tx_errors: AtomicU64,
    tx_dropped: AtomicU64,
}

impl Carried {
    /// `more` was carried since the last time.
    pub fn add(&self, more: &NetStats) {
        let pairs = [
            (&self.rx_bytes, more.rx_bytes),
            (&self.rx_packets, more.rx_packets),
            (&self.rx_errors, more.rx_errors),
            (&self.rx_dropped, more.rx_dropped),
            (&self.tx_bytes, more.tx_bytes),
            (&self.tx_packets, more.tx_packets),
            (&self.tx_errors, more.tx_errors),
            (&self.tx_dropped, more.tx_dropped),
        ];
        for (count, more) in pairs {
            if more != 0 {
                count.fetch_add(more, Ordering::Relaxed);
            }
        }
    }

    /// All of it, so far.
    pub fn so_far(&self) -> NetStats {
        NetStats {
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            rx_packets: self.rx_packets.load(Ordering::Relaxed),
            rx_errors: self.rx_errors.load(Ordering::Relaxed),
            rx_dropped: self.rx_dropped.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            tx_packets: self.tx_packets.load(Ordering::Relaxed),
            tx_errors: self.tx_errors.load(Ordering::Relaxed),
            tx_dropped: self.tx_dropped.load(Ordering::Relaxed),
        }
    }
}

/// What a card has of IPv6, kept by whoever is told: its addresses and
/// the router of its link, as the stack said them last. Nothing, until
/// it says, and once it is gone.
#[derive(Default)]
pub struct Sixes {
    said: Mutex<(Vec<Ipv6Cidr>, Option<Ipv6Address>)>,
}

impl Sixes {
    /// The card has `addresses` now, and what is for none of their
    /// networks goes to `router`.
    pub fn say(&self, addresses: Vec<Ipv6Cidr>, router: Option<Ipv6Address>) {
        *self.said.lock() = (addresses, router);
    }

    /// The stack that said them is gone.
    pub fn forget(&self) {
        self.say(Vec::new(), None);
    }

    /// The addresses, in the order they were said.
    pub fn addresses(&self) -> Vec<Ipv6Cidr> {
        self.said.lock().0.clone()
    }

    /// The router, when there is one.
    pub fn router(&self) -> Option<Ipv6Address> {
        self.said.lock().1
    }

    /// Whether `address` is one of the card's.
    pub fn has(&self, address: &Ipv6Address) -> bool {
        let said = self.said.lock();
        said.0.iter().any(|ours| ours.address() == *address)
    }

    /// Whether the card has a way to `address`: to a group, to a network
    /// of one of its addresses, or to anywhere while there is a router.
    /// Never while it has no address.
    pub fn reaches(&self, address: &Ipv6Address) -> bool {
        let said = self.said.lock();
        !said.0.is_empty()
            && (address.is_multicast()
                || said.1.is_some()
                || said.0.iter().any(|ours| ours.contains_addr(address)))
    }
}

/// Where a frame sent on a stack's card goes: to the stack, which has the
/// card and puts it on the wire.
///
/// The card is in the kernel and the stack is a process, so the frame
/// travels as a message of the protocol between them; what that is, is
/// whoever makes the card's to know (`linux-object`'s `user_stack`), and
/// the card only asks.
pub trait Wire: Send + Sync {
    /// `frame`, whole, for the stack's card `card` to carry. False when
    /// it did not go: there is no stack just now, or it has more unread
    /// than it should.
    fn put(&self, card: u64, frame: &[u8]) -> bool;

    /// Ask the stack for `change` on its card `card`. False when it was
    /// not asked: there is no stack just now, or the change is one no
    /// card of its has (a gateway that is not on the card's network).
    fn configure(&self, _card: u64, _change: Change) -> bool {
        false
    }
}

/// What can be asked of a stack about a card of its.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// This address and network, or none.
    Address(Option<Ipv4Cidr>),
    /// This gateway for what is not on the card's network, or none.
    Gateway(Option<Ipv4Address>),
}

/// The interface that shows the card of a stack that runs somewhere else.
pub struct StackCard {
    name: String,
    mac: EthernetAddress,
    /// The stack's number for the card: 0 for the one it serves with.
    card: u64,
    /// The address and the gateway the stack said last.
    said: Mutex<(Option<Ipv4Cidr>, Option<Ipv4Address>)>,
    /// What the stack says the card has carried.
    carried: Arc<Carried>,
    /// What the stack says the card has of IPv6.
    sixes: Arc<Sixes>,
    /// Where a frame sent on the card goes, when anyone has said.
    wire: Option<Arc<dyn Wire>>,
}

impl StackCard {
    /// The card `mac`, called `name`, with `address` on a network whose
    /// prefix is `prefix` bits (32 at most), and what is not on that
    /// network sent to `gateway`.
    pub fn new(
        name: String,
        mac: [u8; 6],
        address: [u8; 4],
        prefix: u8,
        gateway: Option<[u8; 4]>,
    ) -> Self {
        let address = (address != [0; 4] && prefix != 0)
            .then(|| Ipv4Cidr::new(Ipv4Address(address), prefix.min(32)));
        Self {
            name,
            mac: EthernetAddress(mac),
            card: 0,
            said: Mutex::new((address, gateway.map(Ipv4Address))),
            carried: Arc::new(Carried::default()),
            sixes: Arc::new(Sixes::default()),
            wire: None,
        }
    }

    /// The same card, as the stack's card `card`: what is asked of the
    /// stack about it says that number.
    pub fn numbered(mut self, card: u64) -> Self {
        self.card = card;
        self
    }

    /// The stack says the card has this address and this gateway now, or
    /// none.
    pub fn set(&self, address: Option<Ipv4Cidr>, gateway: Option<Ipv4Address>) {
        *self.said.lock() = (address, gateway);
    }

    /// Ask the stack for `change` on this card.
    fn ask(&self, change: Change) -> DeviceResult {
        let wire = self.wire.as_ref().ok_or(DeviceError::NotSupported)?;
        if wire.configure(self.card, change) {
            Ok(())
        } else {
            Err(DeviceError::NotReady)
        }
    }

    /// The same card, with what it has carried read from `carried`: where
    /// whoever hears the stack keeps what it says. Without it the card
    /// has carried nothing, as far as anyone here can tell.
    pub fn counting(mut self, carried: Arc<Carried>) -> Self {
        self.carried = carried;
        self
    }

    /// The same card, with `wire` to put on the wire what is sent on it:
    /// the way to the stack that has the card. Without it nothing can be
    /// sent on the card at all, which is how it was before there was one.
    pub fn through(mut self, wire: Arc<dyn Wire>) -> Self {
        self.wire = Some(wire);
        self
    }

    /// The same card, with its IPv6 addresses and router read from
    /// `sixes`: where whoever hears the stack keeps what it says of
    /// them. Without it the card has none.
    pub fn with_sixes(mut self, sixes: Arc<Sixes>) -> Self {
        self.sixes = sixes;
        self
    }

    /// The card as a stack says it in three numbers, or `None` when they
    /// say none: `prefix` the bits of its network's prefix (1 to 32, and 0
    /// with no address, from a stack whose card has none yet), `gateway`
    /// an address as a big-endian number (10.0.9.1 is 0x0a000901; 0 when
    /// there is no gateway) and `mac` the six bytes of the MAC the same
    /// way. A card with no MAC is not one to show, nor one with an
    /// address and no network or a network and no address.
    pub fn said(
        name: String,
        address: [u8; 4],
        prefix: usize,
        gateway: usize,
        mac: usize,
    ) -> Option<Self> {
        if prefix > 32 || (prefix == 0) != (address == [0; 4]) {
            return None;
        }
        let mac = (mac as u64).to_be_bytes();
        if mac[..2] != [0, 0] || mac[2..] == [0; 6] {
            return None;
        }
        let mac = [mac[2], mac[3], mac[4], mac[5], mac[6], mac[7]];
        // A gateway that is no IPv4 address is no gateway.
        let gateway = (gateway as u64 <= u64::from(u32::MAX) && gateway != 0)
            .then(|| (gateway as u32).to_be_bytes());
        let gateway = gateway.filter(|_| prefix != 0);
        Some(Self::new(name, mac, address, prefix as u8, gateway))
    }
}

/// What a [`StackCard`] answers as its driver's name: by it the kernel's
/// own stack knows a card that is not its to drive.
pub const NAME: &str = "stack-card";

impl Scheme for StackCard {
    fn name(&self) -> &str {
        NAME
    }
}

impl NetScheme for StackCard {
    fn recv(&self, _buf: &mut [u8]) -> DeviceResult<usize> {
        Err(DeviceError::NotReady)
    }

    /// To the stack, which has the card: `NotSupported` where nobody said
    /// how to reach it, `NotReady` when it would not take the frame just
    /// now (no stack serving, or more unread than it should have).
    fn send(&self, buf: &[u8]) -> DeviceResult<usize> {
        let wire = self.wire.as_ref().ok_or(DeviceError::NotSupported)?;
        if wire.put(self.card, buf) {
            Ok(buf.len())
        } else {
            Err(DeviceError::NotReady)
        }
    }

    fn can_recv(&self) -> bool {
        false
    }

    fn can_send(&self) -> bool {
        self.wire.is_some()
    }

    fn get_mac(&self) -> EthernetAddress {
        self.mac
    }

    fn ifname(&self) -> &str {
        &self.name
    }

    fn is_loopback(&self) -> bool {
        false
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    fn get_ip_address(&self) -> Vec<IpCidr> {
        let mut addrs: Vec<IpCidr> = self.said.lock().0.map(IpCidr::Ipv4).into_iter().collect();
        addrs.extend(self.sixes.addresses().into_iter().map(IpCidr::Ipv6));
        addrs
    }

    /// The card's one address of IPv4 is `cidr` from now on: asked of
    /// the stack, which says it back when it is.
    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        // 0.0.0.0 is no address: how `ifconfig` and a DHCP client's
        // script take the one there is away.
        if cidr.address().is_unspecified() {
            return match self.said.lock().0 {
                Some(_) => self.ask(Change::Address(None)),
                None => Ok(()),
            };
        }
        if cidr.prefix_len() == 0 || !cidr.address().is_unicast() {
            return Err(DeviceError::InvalidParam);
        }
        self.ask(Change::Address(Some(cidr)))
    }

    /// A card of the stack's has one address of IPv4: adding one is
    /// setting it. Its IPv6 addresses are its router's to give.
    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        match cidr {
            IpCidr::Ipv4(cidr) => self.set_ipv4_address(cidr),
            _ => Err(DeviceError::NotSupported),
        }
    }

    /// Take the card's address away, when `cidr` is it.
    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        let has = self.said.lock().0;
        match cidr {
            IpCidr::Ipv4(cidr) if has.is_some_and(|has| has.address() == cidr.address()) => {
                self.ask(Change::Address(None))
            }
            IpCidr::Ipv4(_) => Err(DeviceError::InvalidParam),
            _ => Err(DeviceError::NotSupported),
        }
    }

    /// The default route of IPv4 is the stack's gateway; it has no
    /// other route to add.
    fn add_route(&self, cidr: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        match (cidr, gateway) {
            (IpCidr::Ipv4(cidr), Some(IpAddress::Ipv4(gateway))) if cidr.prefix_len() == 0 => {
                // A gateway is a machine of the card's network.
                let near = self.said.lock().0.is_some_and(|address| {
                    address.contains_addr(&gateway) && address.address() != gateway
                });
                if !near {
                    return Err(DeviceError::InvalidParam);
                }
                self.ask(Change::Gateway(Some(gateway)))
            }
            _ => Err(DeviceError::NotSupported),
        }
    }

    fn del_route(&self, cidr: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        let has = self.said.lock().1;
        match (cidr, gateway) {
            (IpCidr::Ipv4(cidr), gateway) if cidr.prefix_len() == 0 => match gateway {
                Some(IpAddress::Ipv4(gateway)) if has != Some(gateway) => {
                    Err(DeviceError::InvalidParam)
                }
                Some(IpAddress::Ipv4(_)) | None if has.is_some() => self.ask(Change::Gateway(None)),
                _ => Err(DeviceError::InvalidParam),
            },
            _ => Err(DeviceError::NotSupported),
        }
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        // The default route, when the stack has a gateway.
        let (address, gateway) = *self.said.lock();
        let mut routes: Vec<RouteInfo> = gateway
            .filter(|_| address.is_some())
            .map(|gateway| RouteInfo {
                dst: IpCidr::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0),
                gateway: Some(IpAddress::Ipv4(gateway)),
            })
            .into_iter()
            .collect();
        routes.extend(self.sixes.router().map(|router| RouteInfo {
            dst: IpCidr::new(IpAddress::Ipv6(Ipv6Address::UNSPECIFIED), 0),
            gateway: Some(IpAddress::Ipv6(router)),
        }));
        ip_cfg::reported_routes(&routes, &self.get_ip_address())
    }

    fn get_stats(&self) -> NetStats {
        self.carried.so_far()
    }

    fn poll(&self) -> DeviceResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(gateway: Option<[u8; 4]>) -> StackCard {
        StackCard::new(
            "eth0".into(),
            [2, 0, 0, 0, 0, 7],
            [10, 0, 9, 15],
            24,
            gateway,
        )
    }

    /// A wire that keeps what was sent on it, and answers `takes`.
    struct Kept {
        takes: bool,
        sent: Mutex<Vec<(u64, Vec<u8>)>>,
    }

    /// A wire that keeps what was asked of the stack.
    #[derive(Default)]
    struct Asked(Mutex<Vec<(u64, Change)>>);

    impl Wire for Asked {
        fn put(&self, _card: u64, _frame: &[u8]) -> bool {
            false
        }

        fn configure(&self, card: u64, change: Change) -> bool {
            self.0.lock().push((card, change));
            true
        }
    }

    #[test]
    fn what_is_set_on_the_card_is_asked_of_the_stack_and_shown_when_it_says_so() {
        let wire = Arc::new(Asked::default());
        let c = card(Some([10, 0, 9, 1])).numbered(2).through(wire.clone());
        let other = Ipv4Cidr::new(Ipv4Address([192, 168, 7, 9]), 24);
        let router = Ipv4Address([192, 168, 7, 1]);
        let default = IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0);
        assert_eq!(c.set_ipv4_address(other), Ok(()));
        assert_eq!(c.add_ip_address(IpCidr::Ipv4(other)), Ok(()));
        // A gateway is on the network the card is shown on.
        assert_eq!(
            c.add_route(default, Some(IpAddress::Ipv4(router))),
            Err(DeviceError::InvalidParam)
        );
        let near = Ipv4Address([10, 0, 9, 7]);
        assert_eq!(c.add_route(default, Some(IpAddress::Ipv4(near))), Ok(()));
        assert_eq!(c.del_route(default, None), Ok(()));
        // Its own address can be taken away, and no other.
        assert_eq!(
            c.remove_ip_address(IpCidr::Ipv4(other)),
            Err(DeviceError::InvalidParam)
        );
        let own = IpCidr::new(IpAddress::v4(10, 0, 9, 15), 24);
        assert_eq!(c.remove_ip_address(own), Ok(()));
        assert_eq!(
            wire.0.lock().as_slice(),
            &[
                (2, Change::Address(Some(other))),
                (2, Change::Address(Some(other))),
                (2, Change::Gateway(Some(near))),
                (2, Change::Gateway(None)),
                (2, Change::Address(None)),
            ]
        );
        // 0.0.0.0 takes the address away, too, and is nothing to ask of
        // a card that has none.
        let none = Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 24);
        assert_eq!(c.set_ipv4_address(none), Ok(()));
        assert_eq!(wire.0.lock().len(), 6);
        c.set(None, None);
        assert_eq!(c.set_ipv4_address(none), Ok(()));
        assert_eq!(wire.0.lock().len(), 6);
        c.set(Some(Ipv4Cidr::new(Ipv4Address([10, 0, 9, 15]), 24)), None);
        // Asking changes nothing: the stack's word does.
        assert_eq!(c.get_ip_address(), [own]);
        c.set(Some(other), Some(router));
        assert_eq!(c.get_ip_address(), [IpCidr::Ipv4(other)]);
        assert_eq!(c.get_routes()[0].gateway, Some(IpAddress::Ipv4(router)));
        c.set(None, None);
        assert!(c.get_ip_address().is_empty() && c.get_routes().is_empty());

        // What a card of a stack does not have.
        let group = Ipv4Cidr::new(Ipv4Address([224, 0, 0, 1]), 24);
        assert_eq!(c.set_ipv4_address(group), Err(DeviceError::InvalidParam));
        let six = IpCidr::new(IpAddress::Ipv6(Ipv6Address::LOOPBACK), 128);
        assert_eq!(c.add_ip_address(six), Err(DeviceError::NotSupported));
        let net = IpCidr::new(IpAddress::v4(10, 9, 0, 0), 16);
        assert_eq!(
            c.add_route(net, Some(IpAddress::Ipv4(router))),
            Err(DeviceError::NotSupported)
        );
        assert_eq!(c.del_route(default, None), Err(DeviceError::InvalidParam));
    }

    impl Wire for Kept {
        fn put(&self, card: u64, frame: &[u8]) -> bool {
            if !self.takes {
                return false;
            }
            self.sent.lock().push((card, frame.to_vec()));
            true
        }
    }

    /// With a wire, a frame sent on the card goes to the stack whole.
    #[test]
    fn a_frame_sent_on_the_card_goes_to_the_stack() {
        let wire = Arc::new(Kept {
            takes: true,
            sent: Mutex::new(Vec::new()),
        });
        let c = card(None).through(wire.clone());
        assert!(c.can_send());
        let frame = [0x33u8, 0x33, 0, 1, 0, 2, 2, 0, 0, 0, 0, 7, 0x86, 0xdd, 1, 2];
        assert_eq!(c.send(&frame), Ok(frame.len()));
        // By the card it was sent on, which the stack knows by its number.
        let other = card(None).numbered(3).through(wire.clone());
        assert_eq!(other.send(&frame[..14]), Ok(14));
        assert_eq!(
            wire.sent.lock().as_slice(),
            &[(0, frame.to_vec()), (3, frame[..14].to_vec())]
        );
    }

    /// A stack that will not take it now is `NotReady` -- try again --
    /// and not `NotSupported`, which is a card with no stack at all.
    #[test]
    fn a_stack_that_does_not_take_the_frame_is_not_ready() {
        let wire = Arc::new(Kept {
            takes: false,
            sent: Mutex::new(Vec::new()),
        });
        let c = card(None).through(wire);
        assert!(c.can_send());
        assert_eq!(c.send(&[0u8; 64]), Err(DeviceError::NotReady));
    }

    #[test]
    fn it_shows_what_the_stack_said_and_carries_nothing() {
        let c = card(Some([10, 0, 9, 1]));
        assert_eq!(c.ifname(), "eth0");
        assert_eq!(c.get_ifname(), "eth0");
        assert!(!c.is_loopback());
        assert_eq!(c.get_mac(), EthernetAddress([2, 0, 0, 0, 0, 7]));
        assert_eq!(
            c.get_ip_address(),
            [IpCidr::new(IpAddress::v4(10, 0, 9, 15), 24)]
        );
        assert!(c.link_carrier_up());

        // The default route first and then its own network, as an
        // interface the kernel carries reports them.
        let routes = c.get_routes();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].dst, IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0));
        assert_eq!(routes[0].gateway, Some(IpAddress::v4(10, 0, 9, 1)));
        assert_eq!(routes[1].dst, IpCidr::new(IpAddress::v4(10, 0, 9, 0), 24));
        assert_eq!(routes[1].gateway, None);

        // Nothing goes through a card nobody said how to reach, and
        // nothing about it is set here.
        assert!(!c.can_recv() && !c.can_send());
        assert_eq!(c.recv(&mut [0u8; 64]), Err(DeviceError::NotReady));
        assert_eq!(c.send(&[0u8; 64]), Err(DeviceError::NotSupported));
        assert_eq!(c.poll(), Ok(()));
        let other = Ipv4Cidr::new(Ipv4Address([10, 0, 9, 16]), 24);
        assert_eq!(c.set_ipv4_address(other), Err(DeviceError::NotSupported));
        assert_eq!(
            c.add_route(IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0), None),
            Err(DeviceError::NotSupported)
        );
        let router = Some(IpAddress::v4(10, 0, 9, 1));
        assert_eq!(
            c.add_route(IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0), router),
            Err(DeviceError::NotSupported)
        );
        assert_eq!(c.get_ip_address().len(), 1);
    }

    #[test]
    fn what_a_stack_says_in_numbers_is_its_card_or_none() {
        let said = |address, prefix, gateway, mac| {
            StackCard::said("eth0".into(), address, prefix, gateway, mac)
        };
        let c = said([10, 0, 9, 15], 24, 0x0a00_0901, 0x5254_0012_3456).unwrap();
        assert_eq!(
            c.get_mac(),
            EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
        );
        assert_eq!(
            c.get_ip_address(),
            [IpCidr::new(IpAddress::v4(10, 0, 9, 15), 24)]
        );
        assert_eq!(c.get_routes()[0].gateway, Some(IpAddress::v4(10, 0, 9, 1)));
        // No gateway, and one that is no address, is a card without one.
        assert_eq!(
            said([10, 0, 9, 15], 32, 0, 0x5254_0012_3456)
                .unwrap()
                .get_routes()
                .len(),
            1
        );
        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            said([10, 0, 9, 15], 24, 1 << 32, 0x5254_0012_3456)
                .unwrap()
                .get_routes()
                .len(),
            1
        );
        // A card with no address yet is one, with nothing to show of it.
        let bare = said([0, 0, 0, 0], 0, 0x0a00_0901, 0x5254_0012_3456).unwrap();
        assert!(bare.get_ip_address().is_empty() && bare.get_routes().is_empty());
        // A stack that says nothing of its card (all zero), a prefix
        // that is none, an address with no network or a network with no
        // address, no MAC or more than a MAC: no card.
        assert!(said([0, 0, 0, 0], 0, 0, 0).is_none());
        assert!(said([10, 0, 9, 15], 0, 0, 0x5254_0012_3456).is_none());
        assert!(said([10, 0, 9, 15], 33, 0, 0x5254_0012_3456).is_none());
        assert!(said([0, 0, 0, 0], 24, 0, 0x5254_0012_3456).is_none());
        assert!(said([10, 0, 9, 15], 24, 0, 0).is_none());
        #[cfg(target_pointer_width = "64")]
        assert!(said([10, 0, 9, 15], 24, 0, 1 << 48).is_none());
    }

    #[test]
    fn it_has_carried_what_was_counted_for_it_and_the_next_card_goes_on() {
        // Nobody counts for it: nothing carried.
        let alone = card(None).get_stats();
        assert_eq!((alone.rx_packets, alone.tx_packets), (0, 0));

        let carried = Arc::new(Carried::default());
        let c = card(None).counting(carried.clone());
        carried.add(&NetStats {
            rx_bytes: 600,
            rx_packets: 4,
            tx_bytes: 420,
            tx_packets: 3,
            rx_errors: 1,
            ..NetStats::default()
        });
        carried.add(&NetStats {
            rx_bytes: 60,
            rx_packets: 1,
            tx_dropped: 2,
            ..NetStats::default()
        });
        let stats = c.get_stats();
        assert_eq!((stats.rx_bytes, stats.rx_packets), (660, 5));
        assert_eq!((stats.tx_bytes, stats.tx_packets), (420, 3));
        assert_eq!((stats.rx_errors, stats.rx_dropped), (1, 0));
        assert_eq!((stats.tx_errors, stats.tx_dropped), (0, 2));

        // The card of the stack that comes after starts where this one
        // is: the counts are the machine's interface's, not a process's.
        drop(c);
        let next = card(Some([10, 0, 9, 1])).counting(carried.clone());
        carried.add(&NetStats {
            tx_bytes: 80,
            tx_packets: 1,
            ..NetStats::default()
        });
        let stats = next.get_stats();
        assert_eq!((stats.rx_bytes, stats.rx_packets), (660, 5));
        assert_eq!((stats.tx_bytes, stats.tx_packets), (500, 4));
    }

    #[test]
    fn it_shows_the_ipv6_addresses_and_the_router_the_stack_said_last() {
        let sixes = Arc::new(Sixes::default());
        let c = card(Some([10, 0, 9, 1])).with_sixes(sixes.clone());
        let v4 = IpCidr::new(IpAddress::v4(10, 0, 9, 15), 24);
        assert_eq!(c.get_ip_address(), [v4]);
        assert_eq!(c.get_routes().len(), 2);

        // The link's own: an address, and its network.
        let link = Ipv6Cidr::new(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 7), 64);
        sixes.say(vec![link], None);
        assert_eq!(c.get_ip_address(), [v4, IpCidr::Ipv6(link)]);
        let routes = c.get_routes();
        assert_eq!(routes.len(), 3);
        assert_eq!(routes[2].dst, IpCidr::Ipv6(link.network()));
        assert_eq!(routes[2].gateway, None);

        // What the router gave, and the router: the default routes first.
        let given = Ipv6Cidr::new(Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 7), 64);
        let router = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
        sixes.say(vec![link, given], Some(router));
        assert_eq!(
            c.get_ip_address(),
            [v4, IpCidr::Ipv6(link), IpCidr::Ipv6(given)]
        );
        let routes = c.get_routes();
        assert_eq!(routes.len(), 5);
        assert_eq!(routes[0].gateway, Some(IpAddress::v4(10, 0, 9, 1)));
        assert_eq!(
            routes[1].dst,
            IpCidr::new(IpAddress::Ipv6(Ipv6Address::UNSPECIFIED), 0)
        );
        assert_eq!(routes[1].gateway, Some(IpAddress::Ipv6(router)));
        assert_eq!(routes[4].dst, IpCidr::Ipv6(given.network()));

        // What is the card's, and where it has a way to.
        let on_link = Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x99);
        let far = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let group = Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
        assert!(sixes.has(&given.address()) && sixes.has(&link.address()));
        assert!(!sixes.has(&on_link));
        assert!(sixes.reaches(&on_link) && sixes.reaches(&far) && sixes.reaches(&group));
        sixes.say(vec![link, given], None);
        assert!(sixes.reaches(&on_link) && sixes.reaches(&group));
        assert!(!sixes.reaches(&far));

        // The stack is gone: what it said first is all there is.
        sixes.forget();
        assert!(!sixes.has(&link.address()) && !sixes.reaches(&group));
        assert_eq!(c.get_ip_address(), [v4]);
        assert_eq!(c.get_routes().len(), 2);
    }

    #[test]
    fn without_a_gateway_there_is_only_its_own_network() {
        let routes = card(None).get_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].dst, IpCidr::new(IpAddress::v4(10, 0, 9, 0), 24));
        assert_eq!(routes[0].gateway, None);
    }
}
