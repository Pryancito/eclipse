//! A network interface whose card is driven somewhere else.
//!
//! Everything a NIC driver in this crate does above its registers -- the
//! smoltcp interface, the address and route tables, the statistics, the
//! AF_PACKET tap -- with the registers replaced by a [`FramePort`]: something
//! that hands over whole Ethernet frames and takes them. The port of a card
//! driven by a userspace process is in the loader (`eclipse_net_serve`); this
//! half knows nothing about processes.
//!
//! There is no interrupt here. Whoever owns the port calls
//! [`ServedInterface::frames_arrived`] when it has frames to give (and then
//! wakes the tasks asleep on sockets), and the socket paths poll the
//! interface like any other.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::*;
use smoltcp::phy::{self, DeviceCapabilities};
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result;

use super::{ip_cfg, timer_now_as_micros};
use crate::net::get_sockets;
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::sync::Mutex;
use crate::{DeviceError, DeviceResult};

/// The longest frame an interface takes or gives: an Ethernet header and a
/// 1500-byte payload, without the checksum the card adds.
pub const MAX_FRAME: usize = 1514;

/// Whether `len` is the length of a frame that can be sent: not empty, and
/// no longer than [`MAX_FRAME`]. A longer one is refused, never clipped.
pub fn frame_len_ok(len: usize) -> bool {
    (1..=MAX_FRAME).contains(&len)
}

/// The wire side of a [`ServedInterface`].
pub trait FramePort: Send + Sync {
    /// The next frame that came in, if there is one.
    fn receive(&self) -> Option<Vec<u8>>;
    /// Whether [`send`](Self::send) would take a frame now.
    fn can_send(&self) -> bool;
    /// Put a frame on the wire. `false` when it could not be taken: the
    /// caller keeps it and tries again.
    fn send(&self, frame: &[u8]) -> bool;
}

/// The link-local IPv6 address of a card with this MAC (modified EUI-64).
pub fn link_local(mac: &[u8; 6]) -> Ipv6Address {
    let pair = |a: u8, b: u8| (a as u16) << 8 | b as u16;
    Ipv6Address::new(
        0xfe80,
        0,
        0,
        0,
        pair(mac[0] ^ 2, mac[1]),
        pair(mac[2], 0xff),
        pair(0xfe, mac[3]),
        pair(mac[4], mac[5]),
    )
}

#[derive(Clone)]
pub struct ServedDriver {
    port: Arc<dyn FramePort>,
    stats: Arc<Mutex<NetStats>>,
}

pub struct ServedRxToken {
    data: Vec<u8>,
    stats: Arc<Mutex<NetStats>>,
}

pub struct ServedTxToken {
    driver: ServedDriver,
}

impl phy::Device<'_> for ServedDriver {
    type RxToken = ServedRxToken;
    type TxToken = ServedTxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        // No frame without room to answer it. smoltcp answers some frames
        // on the spot, through the token handed out here (a TCP segment's
        // ACK among them), and counts the answer as sent whatever becomes
        // of it: a burst that outran the transmit slots lost its ACKs for
        // good, and the peer sat out its retransmit timer, 1.5 s at a time.
        // The frame waits in the port until a slot comes back.
        if !self.port.can_send() {
            return None;
        }
        self.port.receive().map(|data| {
            (
                ServedRxToken {
                    data,
                    stats: self.stats.clone(),
                },
                ServedTxToken {
                    driver: self.clone(),
                },
            )
        })
    }

    fn transmit(&mut self) -> Option<Self::TxToken> {
        self.port.can_send().then(|| ServedTxToken {
            driver: self.clone(),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = MAX_FRAME;
        // `max_burst_size` stays unset: smoltcp clamps the TCP window to
        // `burst * MSS` through a u16, and a burst of 64 wraps it (the trap
        // `e1000.rs` documents).
        caps.max_burst_size = None;
        caps
    }
}

impl phy::RxToken for ServedRxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        {
            let mut stats = self.stats.lock();
            stats.rx_packets += 1;
            stats.rx_bytes += self.data.len() as u64;
        }
        // AF_PACKET taps, after smoltcp has let go of SOCKETS.
        super::net_defer_packet(&self.data);
        f(&mut self.data)
    }
}

impl phy::TxToken for ServedTxToken {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut frame = vec![0u8; len];
        let result = f(&mut frame)?;
        if !frame_len_ok(len) || !self.driver.port.send(&frame) {
            // Never report a dropped frame as sent: smoltcp has already
            // advanced its state for the segment, and `Exhausted` is what
            // makes it emit the segment again (see `pcnet32.rs`).
            self.driver.stats.lock().tx_errors += 1;
            return Err(smoltcp::Error::Exhausted);
        }
        let mut stats = self.driver.stats.lock();
        stats.tx_packets += 1;
        stats.tx_bytes += len as u64;
        Ok(result)
    }
}

/// The interface: a [`NetScheme`] like any card's.
#[derive(Clone)]
pub struct ServedInterface {
    iface: Arc<Mutex<Interface<'static, ServedDriver>>>,
    driver: ServedDriver,
    name: String,
    stats: Arc<Mutex<NetStats>>,
    routes: Arc<Mutex<Vec<RouteInfo>>>,
    ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl ServedInterface {
    /// An interface called `name`, with this MAC, over `port`.
    pub fn new(name: String, mac: [u8; 6], port: Arc<dyn FramePort>) -> Self {
        let stats = Arc::new(Mutex::new(NetStats::default()));
        let driver = ServedDriver {
            port,
            stats: stats.clone(),
        };
        let ip_addrs = vec![
            IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            IpCidr::Ipv6(Ipv6Cidr::new(link_local(&mac), 64)),
            IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        ];
        // Each interface its own route storage, for the life of the kernel.
        let routes_storage: &'static mut [Option<(IpCidr, Route)>] =
            Box::leak(vec![None; 4].into_boxed_slice());
        let iface = InterfaceBuilder::new(driver.clone())
            .ethernet_addr(EthernetAddress(mac))
            .neighbor_cache(NeighborCache::new(BTreeMap::new()))
            .ip_addrs(ip_addrs.clone())
            .routes(Routes::new(routes_storage))
            .finalize();
        Self {
            iface: Arc::new(Mutex::new(iface)),
            driver,
            name,
            stats,
            routes: Arc::new(Mutex::new(vec![])),
            ip_addrs: Arc::new(Mutex::new(ip_addrs)),
        }
    }

    /// The port has frames to give: run them through the stack. Waking
    /// whoever sleeps on a socket is the caller's, which knows how.
    ///
    /// `false` when a socket call had the stack at that moment and nothing
    /// was run: the frames wait in the port for the next poll, which may be
    /// this again.
    pub fn frames_arrived(&self) -> bool {
        self.poll_stack().0
    }

    fn poll_inner(&self) -> DeviceResult {
        self.poll_stack().1
    }

    /// Whether the stack was free to poll, and what the poll said.
    fn poll_stack(&self) -> (bool, DeviceResult) {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        let sockets = get_sockets();
        // `try_lock`, not a blocking acquire: a socket syscall that holds
        // the set may be what called this, and whoever holds it drives the
        // poll on release (the deadlock `e1000.rs` records).
        let res = match (sockets.try_lock(), self.iface.try_lock()) {
            (Some(mut sockets), Some(mut iface)) => match iface.poll(&mut sockets, timestamp) {
                Ok(_) => (true, Ok(())),
                Err(err) => {
                    warn!("[served-net] poll got err {}", err);
                    (true, Err(DeviceError::IoError))
                }
            },
            _ => (false, Ok(())),
        };
        super::net_flush_deferred_packets();
        res
    }
}

impl Scheme for ServedInterface {
    fn name(&self) -> &str {
        "served-net"
    }
}

impl NetScheme for ServedInterface {
    fn get_mac(&self) -> EthernetAddress {
        self.iface.lock().ethernet_addr()
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
        self.ip_addrs.lock().clone()
    }

    fn seed_neighbor(&self, protocol: IpAddress, hardware: EthernetAddress) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        ip_cfg::seed_neighbor(&mut self.iface.lock(), protocol, hardware, timestamp)
    }

    fn poll(&self) -> DeviceResult {
        self.poll_inner()
    }

    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        match self.driver.port.receive() {
            Some(frame) => {
                let n = frame.len().min(buf.len());
                buf[..n].copy_from_slice(&frame[..n]);
                Ok(n)
            }
            None => Err(DeviceError::NotReady),
        }
    }

    fn send(&self, data: &[u8]) -> DeviceResult<usize> {
        if !frame_len_ok(data.len()) {
            return Err(DeviceError::InvalidParam);
        }
        if self.driver.port.send(data) {
            Ok(data.len())
        } else {
            Err(DeviceError::NotReady)
        }
    }

    fn can_recv(&self) -> bool {
        true
    }

    fn can_send(&self) -> bool {
        self.driver.port.can_send()
    }

    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        ip_cfg::set_ipv4_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
    }

    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        ip_cfg::add_ip_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
    }

    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        ip_cfg::remove_ip_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
    }

    fn add_route(&self, cidr: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        ip_cfg::add_route(
            &mut self.iface.lock(),
            &mut self.routes.lock(),
            cidr,
            gateway,
        )
    }

    fn del_route(&self, cidr: IpCidr, _gateway: Option<IpAddress>) -> DeviceResult {
        ip_cfg::del_route(&mut self.iface.lock(), &mut self.routes.lock(), cidr)
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        let iface = self.iface.lock();
        ip_cfg::reported_routes(&self.routes.lock(), iface.ip_addrs())
    }

    fn get_stats(&self) -> NetStats {
        self.stats.lock().clone()
    }

    fn get_mtu(&self) -> usize {
        1500
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::VecDeque;

    /// A port that keeps what is sent and gives what it was told to.
    #[derive(Default)]
    struct Wire {
        incoming: Mutex<VecDeque<Vec<u8>>>,
        sent: Mutex<Vec<Vec<u8>>>,
        full: core::sync::atomic::AtomicBool,
    }

    impl FramePort for Wire {
        fn receive(&self) -> Option<Vec<u8>> {
            self.incoming.lock().pop_front()
        }
        fn can_send(&self) -> bool {
            !self.full.load(core::sync::atomic::Ordering::Relaxed)
        }
        fn send(&self, frame: &[u8]) -> bool {
            if !self.can_send() {
                return false;
            }
            self.sent.lock().push(frame.to_vec());
            true
        }
    }

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x57];

    #[test]
    fn only_a_whole_frame_that_fits_is_sent() {
        assert!(!frame_len_ok(0));
        assert!(frame_len_ok(1));
        assert!(frame_len_ok(MAX_FRAME));
        assert!(!frame_len_ok(MAX_FRAME + 1));
    }

    #[test]
    fn the_link_local_address_is_the_modified_eui64_of_the_mac() {
        assert_eq!(
            link_local(&MAC),
            Ipv6Address::new(0xfe80, 0, 0, 0, 0x5054, 0x00ff, 0xfe12, 0x3457)
        );
    }

    #[test]
    fn the_interface_carries_its_name_and_mac() {
        let nic = ServedInterface::new("eth7".into(), MAC, Arc::new(Wire::default()));
        assert_eq!(nic.ifname(), "eth7");
        assert_eq!(nic.get_mac(), EthernetAddress(MAC));
        assert!(!nic.is_loopback());
        assert_eq!(nic.get_mtu(), 1500);
    }

    #[test]
    fn a_raw_frame_goes_to_the_port_and_is_counted_only_by_the_stack() {
        let wire = Arc::new(Wire::default());
        let nic = ServedInterface::new("eth7".into(), MAC, wire.clone());
        assert_eq!(nic.send(&[0xab; 60]), Ok(60));
        assert_eq!(wire.sent.lock().len(), 1);
        assert_eq!(nic.send(&[]), Err(DeviceError::InvalidParam));
        assert_eq!(
            nic.send(&[0; MAX_FRAME + 1]),
            Err(DeviceError::InvalidParam)
        );
        wire.full.store(true, core::sync::atomic::Ordering::Relaxed);
        assert_eq!(nic.send(&[0xab; 60]), Err(DeviceError::NotReady));
        assert!(!nic.can_send());
    }

    #[test]
    fn a_raw_receive_takes_the_next_frame_or_says_not_ready() {
        let wire = Arc::new(Wire::default());
        let nic = ServedInterface::new("eth7".into(), MAC, wire.clone());
        let mut buf = [0u8; 64];
        assert_eq!(nic.recv(&mut buf), Err(DeviceError::NotReady));
        wire.incoming.lock().push_back(vec![7; 42]);
        assert_eq!(nic.recv(&mut buf), Ok(42));
        assert_eq!(&buf[..42], &[7; 42]);
    }

    #[test]
    fn the_stack_takes_no_frame_it_could_not_answer() {
        use smoltcp::phy::Device as _;
        let wire = Arc::new(Wire::default());
        let mut driver = ServedDriver {
            port: wire.clone(),
            stats: Arc::new(Mutex::new(NetStats::default())),
        };
        wire.incoming.lock().push_back(vec![7; 60]);
        wire.full.store(true, core::sync::atomic::Ordering::Relaxed);
        assert!(driver.receive().is_none());
        // Held back, not dropped.
        assert_eq!(wire.incoming.lock().len(), 1);
        wire.full
            .store(false, core::sync::atomic::Ordering::Relaxed);
        assert!(driver.receive().is_some());
        assert!(wire.incoming.lock().is_empty());
    }

    #[test]
    fn an_address_set_on_the_interface_is_reported() {
        let nic = ServedInterface::new("eth7".into(), MAC, Arc::new(Wire::default()));
        let cidr = Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24);
        nic.set_ipv4_address(cidr).unwrap();
        assert!(nic.get_ip_address().contains(&IpCidr::Ipv4(cidr)));
    }
}
