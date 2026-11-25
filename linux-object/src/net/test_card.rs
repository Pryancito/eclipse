//! A network card for the tests: plugged into the device list for the
//! duration of a test and remembering what it is told, so that what the
//! kernel says of the machine's network (the `SIOC*` requests, netlink,
//! `/sys/class/net`, `/proc/net`) can be checked against what Linux says
//! of the same card.
//!
//! The device list is process-wide and cargo runs a crate's tests in
//! threads, so [`plug`] takes [`super::NET_TEST_LOCK`] and holds it until
//! the cards are unplugged again.

use alloc::{string::String, sync::Arc, vec::Vec};
use lock::Mutex;
use zcore_drivers::scheme::{NeighbourInfo, NetScheme, NetStats, RouteInfo, Scheme};
use zcore_drivers::wire::{EthernetAddress, IpAddress, IpCidr, Ipv4Cidr};
use zcore_drivers::{Device, DeviceError, DeviceResult};

/// What a card has been told.
#[derive(Default)]
pub(crate) struct Told {
    pub addresses: Vec<IpCidr>,
    pub routes: Vec<RouteInfo>,
    pub neighbours: Vec<NeighbourInfo>,
    pub promiscuous: bool,
    pub allmulti: bool,
    /// How many times it was brought up, and down.
    pub ups: usize,
    pub downs: usize,
}

pub(crate) struct Card {
    name: String,
    mac: [u8; 6],
    loopback: bool,
    mtu: usize,
    carrier: bool,
    stats: NetStats,
    /// Whether it refuses to have addresses and routes set, like a driver
    /// that does not answer those.
    refuses: bool,
    pub told: Mutex<Told>,
}

#[allow(dead_code)] // each test uses the parts it needs
impl Card {
    /// An Ethernet card called `name`, with the hardware address `mac`.
    pub fn ether(name: &str, mac: [u8; 6]) -> Self {
        Self {
            name: String::from(name),
            mac,
            loopback: false,
            mtu: 1500,
            carrier: true,
            stats: NetStats::default(),
            refuses: false,
            told: Mutex::new(Told::default()),
        }
    }

    /// The loopback, `lo`, with `127.0.0.1/8`.
    pub fn loopback() -> Self {
        let card = Self {
            mtu: 65536,
            loopback: true,
            ..Self::ether("lo", [0; 6])
        };
        card.told
            .lock()
            .addresses
            .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
        card
    }

    pub fn with_address(self, cidr: IpCidr) -> Self {
        self.told.lock().addresses.push(cidr);
        self
    }

    pub fn with_route(self, dst: IpCidr, gateway: Option<IpAddress>) -> Self {
        self.told.lock().routes.push(RouteInfo { dst, gateway });
        self
    }

    pub fn with_neighbour(self, address: IpAddress, hardware: [u8; 6], until_ms: u64) -> Self {
        self.told.lock().neighbours.push(NeighbourInfo {
            address,
            hardware,
            until_ms,
        });
        self
    }

    pub fn with_mtu(mut self, mtu: usize) -> Self {
        self.mtu = mtu;
        self
    }

    pub fn without_carrier(mut self) -> Self {
        self.carrier = false;
        self
    }

    pub fn with_stats(mut self, stats: NetStats) -> Self {
        self.stats = stats;
        self
    }

    pub fn refusing(mut self) -> Self {
        self.refuses = true;
        self
    }

    fn refuse(&self) -> DeviceResult {
        if self.refuses {
            Err(DeviceError::NotSupported)
        } else {
            Ok(())
        }
    }
}

impl Scheme for Card {
    fn name(&self) -> &str {
        "test-card"
    }
}

impl NetScheme for Card {
    fn recv(&self, _buf: &mut [u8]) -> DeviceResult<usize> {
        Err(DeviceError::NotReady)
    }
    fn send(&self, buf: &[u8]) -> DeviceResult<usize> {
        Ok(buf.len())
    }
    fn get_mac(&self) -> EthernetAddress {
        EthernetAddress(self.mac)
    }
    fn ifname(&self) -> &str {
        &self.name
    }
    fn is_loopback(&self) -> bool {
        self.loopback
    }
    fn get_ip_address(&self) -> Vec<IpCidr> {
        self.told.lock().addresses.clone()
    }
    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        self.refuse()?;
        let mut told = self.told.lock();
        told.addresses.retain(|a| !matches!(a, IpCidr::Ipv4(_)));
        told.addresses.insert(0, IpCidr::Ipv4(cidr));
        Ok(())
    }
    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        self.refuse()?;
        let mut told = self.told.lock();
        if told.addresses.contains(&cidr) {
            return Err(DeviceError::AlreadyExists);
        }
        told.addresses.push(cidr);
        Ok(())
    }
    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        self.refuse()?;
        let mut told = self.told.lock();
        let before = told.addresses.len();
        told.addresses.retain(|a| *a != cidr);
        if told.addresses.len() == before {
            return Err(DeviceError::InvalidParam);
        }
        Ok(())
    }
    fn add_route(&self, dst: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        self.refuse()?;
        self.told.lock().routes.push(RouteInfo { dst, gateway });
        Ok(())
    }
    fn del_route(&self, dst: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        self.refuse()?;
        let mut told = self.told.lock();
        let before = told.routes.len();
        told.routes
            .retain(|r| !(r.dst == dst && (gateway.is_none() || r.gateway == gateway)));
        if told.routes.len() == before {
            return Err(DeviceError::InvalidParam);
        }
        Ok(())
    }
    fn get_routes(&self) -> Vec<RouteInfo> {
        self.told.lock().routes.clone()
    }
    fn get_neighbours(&self) -> Vec<NeighbourInfo> {
        self.told.lock().neighbours.clone()
    }
    fn set_neighbour(&self, address: IpAddress, hardware: Option<[u8; 6]>) -> DeviceResult {
        self.refuse()?;
        let mut told = self.told.lock();
        told.neighbours.retain(|n| n.address != address);
        if let Some(hardware) = hardware {
            told.neighbours.push(NeighbourInfo {
                address,
                hardware,
                until_ms: NeighbourInfo::FIXED,
            });
        }
        Ok(())
    }
    fn get_stats(&self) -> NetStats {
        self.stats.clone()
    }
    fn get_mtu(&self) -> usize {
        self.mtu
    }
    fn poll(&self) -> DeviceResult {
        Ok(())
    }
    fn refresh_link(&self) -> DeviceResult {
        self.told.lock().ups += 1;
        Ok(())
    }
    fn link_carrier_up(&self) -> bool {
        self.carrier
    }
    fn admin_down(&self) -> DeviceResult {
        self.told.lock().downs += 1;
        Ok(())
    }
    fn set_promiscuous(&self, on: bool) -> DeviceResult {
        self.told.lock().promiscuous = on;
        Ok(())
    }
    fn set_allmulti(&self, on: bool) -> DeviceResult {
        self.told.lock().allmulti = on;
        Ok(())
    }
}

/// The cards a test plugged in.
///
/// The machine puts every card that is not the loopback before it, so that
/// its first is the first Ethernet one, whatever order they came in: the
/// interface indexes follow that order and not the one given here. Linux
/// gives `lo` index 1, always.
pub(crate) struct Plugged {
    pub cards: Vec<Arc<Card>>,
    devs: Vec<Device>,
    _lock: lock::MutexGuard<'static, ()>,
}

impl Drop for Plugged {
    fn drop(&mut self) {
        for dev in self.devs.drain(..) {
            kernel_hal::drivers::remove_device_hosted(&dev);
        }
    }
}

impl Plugged {
    /// The card called `name`.
    pub fn card(&self, name: &str) -> &Card {
        self.cards
            .iter()
            .find(|c| c.name == name)
            .expect("a card the test plugged in")
    }
}

/// Plug `cards` in, after whatever is plugged in already (nothing, in a
/// host test).
pub(crate) fn plug(cards: Vec<Card>) -> Plugged {
    let lock = super::NET_TEST_LOCK.lock();
    let mut plugged = Plugged {
        cards: Vec::new(),
        devs: Vec::new(),
        _lock: lock,
    };
    for card in cards {
        let card = Arc::new(card);
        let dev = Device::Net(card.clone());
        kernel_hal::drivers::add_device_hosted(dev.clone());
        plugged.cards.push(card);
        plugged.devs.push(dev);
    }
    plugged
}

/// A machine as most are: the loopback and one Ethernet card, `eth0`, at
/// `10.0.2.15/24` behind `10.0.2.2`.
pub(crate) fn a_machine() -> Plugged {
    plug(alloc::vec![
        Card::loopback(),
        Card::ether("eth0", [0x52, 0x54, 0, 0x12, 0x34, 0x56])
            .with_address(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24))
            .with_route(
                IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
                Some(IpAddress::v4(10, 0, 2, 2)),
            ),
    ])
}
