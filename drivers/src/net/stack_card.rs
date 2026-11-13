//! The card of a network stack that is not the kernel's.
//!
//! When the stack runs as a process and every program's sockets are that
//! stack's, the card that carries them is not in the kernel's list:
//! `ifconfig`, `ip` and `/proc/net` showed `lo` and nothing else, and a
//! program that waits for a default route before it starts waited until it
//! gave up. A [`StackCard`] is that card in the list, to be looked at: a
//! name, the MAC, and the address and the gateway the stack says it has.
//!
//! It carries nothing. No frame arrives on it and one sent on it is refused,
//! because the frames are the stack's; its address and its routes cannot be
//! set from here, because they are the stack's too. It is made when the stack
//! says what its card is and taken away when the stack is gone, and what it
//! says never changes in between: a stack whose address changes starts over.
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
}

/// The interface that shows the card of a stack that runs somewhere else.
pub struct StackCard {
    name: String,
    mac: EthernetAddress,
    addrs: Vec<IpCidr>,
    /// The default route, when the stack has a gateway.
    routes: Vec<RouteInfo>,
    /// What the stack says the card has carried.
    carried: Arc<Carried>,
    /// What the stack says the card has of IPv6.
    sixes: Arc<Sixes>,
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
        let routes = gateway
            .map(|gateway| RouteInfo {
                dst: IpCidr::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0),
                gateway: Some(IpAddress::Ipv4(Ipv4Address(gateway))),
            })
            .into_iter()
            .collect();
        let address = Ipv4Cidr::new(Ipv4Address(address), prefix.min(32));
        Self {
            name,
            mac: EthernetAddress(mac),
            addrs: vec![IpCidr::Ipv4(address)],
            routes,
            carried: Arc::new(Carried::default()),
            sixes: Arc::new(Sixes::default()),
        }
    }

    /// The same card, with what it has carried read from `carried`: where
    /// whoever hears the stack keeps what it says. Without it the card
    /// has carried nothing, as far as anyone here can tell.
    pub fn counting(mut self, carried: Arc<Carried>) -> Self {
        self.carried = carried;
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
    /// from a stack that says nothing of its card), `gateway` an address
    /// as a big-endian number (10.0.9.1 is 0x0a000901; 0 when there is no
    /// gateway) and `mac` the six bytes of the MAC the same way. A card
    /// with no address or no MAC is not one to show.
    pub fn said(
        name: String,
        address: [u8; 4],
        prefix: usize,
        gateway: usize,
        mac: usize,
    ) -> Option<Self> {
        if !(1..=32).contains(&prefix) || address == [0; 4] {
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
        Some(Self::new(name, mac, address, prefix as u8, gateway))
    }
}

impl Scheme for StackCard {
    fn name(&self) -> &str {
        "stack-card"
    }
}

impl NetScheme for StackCard {
    fn recv(&self, _buf: &mut [u8]) -> DeviceResult<usize> {
        Err(DeviceError::NotReady)
    }

    fn send(&self, _buf: &[u8]) -> DeviceResult<usize> {
        Err(DeviceError::NotSupported)
    }

    fn can_recv(&self) -> bool {
        false
    }

    fn can_send(&self) -> bool {
        false
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
        let mut addrs = self.addrs.clone();
        addrs.extend(self.sixes.addresses().into_iter().map(IpCidr::Ipv6));
        addrs
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        let mut routes = self.routes.clone();
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

        // Nothing goes through it and nothing about it is set here.
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
        // A stack that says nothing of its card (all zero, as the ones
        // before this said), a prefix that is none, no address, no MAC or
        // more than a MAC: no card.
        assert!(said([10, 0, 9, 15], 0, 0, 0).is_none());
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

        // The stack is gone: what it said first is all there is.
        sixes.forget();
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
