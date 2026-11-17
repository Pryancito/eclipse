//! `lo`, which is only shown.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};

use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::sync::Mutex;
use crate::{DeviceError, DeviceResult};

/// Who is shown the frames an `AF_PACKET` socket puts on `lo`.
pub static mut LOOPBACK_TX_CALLBACK: Option<fn(&[u8])> = None;

pub fn register_loopback_tx_callback(cb: fn(&[u8])) {
    unsafe {
        LOOPBACK_TX_CALLBACK = Some(cb);
    }
}

/// What the programs of this machine said to each other without a frame
/// being made of it: packets and bytes, as the loopback would have counted
/// them. Sockets that are joined inside the kernel
/// (`linux_object::net::user_stack`) pass nothing through an interface, so
/// `lo` read zero in `/proc/net/dev` and `ifconfig` while megabytes went
/// by. Atomics and not the `NetStats` lock: this is on the path of every
/// local write.
#[cfg(not(test))]
static LOCAL: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

// Per thread under test, for the reason `LOOPBACK_MOVED` is.
#[cfg(test)]
std::thread_local! {
    static LOCAL: [AtomicU64; 2] = const { [AtomicU64::new(0), AtomicU64::new(0)] };
}

#[cfg(not(test))]
#[inline]
fn with_local<R>(body: impl FnOnce(&[AtomicU64; 2]) -> R) -> R {
    body(&LOCAL)
}

#[cfg(test)]
#[inline]
fn with_local<R>(body: impl FnOnce(&[AtomicU64; 2]) -> R) -> R {
    LOCAL.with(body)
}

/// Count `packets` of `bytes` in all that went from one program of this
/// machine to another: the loopback shows them as sent and as received,
/// which is what a loopback does with everything.
#[inline]
pub fn count_local(packets: u64, bytes: u64) {
    with_local(|local| {
        local[0].fetch_add(packets, Ordering::Relaxed);
        local[1].fetch_add(bytes, Ordering::Relaxed);
    });
}

/// What [`count_local`] was told so far: packets, bytes.
pub fn local_counted() -> (u64, u64) {
    with_local(|local| {
        (
            local[0].load(Ordering::Relaxed),
            local[1].load(Ordering::Relaxed),
        )
    })
}

/// `stats` with what was counted by [`count_local`] added, both ways.
fn with_local_traffic(mut stats: NetStats) -> NetStats {
    let (packets, bytes) = local_counted();
    stats.rx_packets += packets;
    stats.tx_packets += packets;
    stats.rx_bytes += bytes;
    stats.tx_bytes += bytes;
    stats
}

/// The networks reached directly through `addrs`.
fn direct_routes(addrs: &[IpCidr], res: &mut Vec<RouteInfo>) {
    for cidr in addrs {
        match cidr {
            IpCidr::Ipv4(v4) if v4.prefix_len() > 0 => res.push(RouteInfo {
                dst: IpCidr::Ipv4(v4.network()),
                gateway: None,
            }),
            IpCidr::Ipv6(v6) if v6.prefix_len() > 0 => res.push(RouteInfo {
                dst: IpCidr::Ipv6(v6.network()),
                gateway: None,
            }),
            _ => {}
        }
    }
}

/// `lo`: a name, its addresses, its routes and its counters, and no stack
/// behind them.
///
/// What goes to 127.0.0.1 there, or to an address of the machine, is
/// joined between the two sockets and never is a packet
/// (`linux_object::net::user_stack`), and an echo request to it is
/// answered where it is sent. This is what `ifconfig`, `ip`,
/// `/proc/net/dev` and netlink are shown.
pub struct ShownLoopback {
    name: String,
    /// What was counted before this took the place of another `lo`, and
    /// the frames an `AF_PACKET` socket puts here.
    stats: Mutex<NetStats>,
    routes: Mutex<Vec<RouteInfo>>,
    ip_addrs: Mutex<Vec<IpCidr>>,
}

impl Default for ShownLoopback {
    fn default() -> Self {
        Self::new()
    }
}

impl ShownLoopback {
    /// `lo` with the addresses every machine has: 127.0.0.1/8 and ::1.
    pub fn new() -> Self {
        ShownLoopback {
            name: String::from("lo"),
            stats: Mutex::new(NetStats::default()),
            routes: Mutex::new(Vec::new()),
            ip_addrs: Mutex::new(alloc::vec![
                IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8),
                IpCidr::new(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 128),
            ]),
        }
    }
}

impl Scheme for ShownLoopback {
    fn name(&self) -> &str {
        "loopback"
    }

    fn handle_irq(&self, _cause: usize) {}
}

impl NetScheme for ShownLoopback {
    fn recv(&self, _buf: &mut [u8]) -> DeviceResult<usize> {
        Err(DeviceError::NotSupported)
    }

    /// A frame an `AF_PACKET` socket puts on `lo`: counted, shown to who
    /// listens on every interface, and gone. No stack is behind this
    /// interface to take it.
    fn send(&self, buf: &[u8]) -> DeviceResult<usize> {
        {
            let mut stats = self.stats.lock();
            stats.tx_packets += 1;
            stats.tx_bytes += buf.len() as u64;
            stats.rx_packets += 1;
            stats.rx_bytes += buf.len() as u64;
        }
        unsafe {
            if let Some(cb) = LOOPBACK_TX_CALLBACK {
                cb(buf);
            }
        }
        Ok(buf.len())
    }

    /// Nothing to carry.
    fn poll(&self) -> DeviceResult {
        Ok(())
    }

    fn get_mac(&self) -> EthernetAddress {
        EthernetAddress::default()
    }

    fn ifname(&self) -> &str {
        &self.name
    }

    fn is_loopback(&self) -> bool {
        true
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    fn get_ip_address(&self) -> Vec<IpCidr> {
        self.ip_addrs.lock().clone()
    }

    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        let mut addrs = self.ip_addrs.lock();
        if !addrs.contains(&cidr) {
            addrs.push(cidr);
        }
        Ok(())
    }

    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        self.ip_addrs.lock().retain(|have| *have != cidr);
        Ok(())
    }

    fn add_route(&self, cidr: IpCidr, gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        self.routes.lock().push(RouteInfo { dst: cidr, gateway });
        Ok(())
    }

    fn del_route(&self, cidr: IpCidr, _gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        self.routes.lock().retain(|r| r.dst != cidr);
        Ok(())
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        let mut res = self.routes.lock().clone();
        direct_routes(&self.ip_addrs.lock(), &mut res);
        res
    }

    fn get_stats(&self) -> NetStats {
        with_local_traffic(self.stats.lock().clone())
    }

    fn get_mtu(&self) -> usize {
        65535
    }
}

#[cfg(test)]
mod shown_tests {
    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> IpCidr {
        IpCidr::new(IpAddress::v4(a, b, c, d), prefix)
    }

    #[test]
    fn it_is_lo_with_the_addresses_every_machine_has() {
        let lo = ShownLoopback::new();
        assert_eq!(lo.ifname(), "lo");
        assert!(lo.is_loopback());
        assert_eq!(lo.get_mtu(), 65535);
        assert_eq!(
            lo.get_ip_address(),
            alloc::vec![
                v4(127, 0, 0, 1, 8),
                IpCidr::new(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 128)
            ]
        );
        // The network of 127/8 and the host route of ::1.
        let routes = lo.get_routes();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].dst, v4(127, 0, 0, 0, 8));
        assert!(lo.poll().is_ok());
        assert!(lo.recv(&mut [0u8; 8]).is_err());
    }

    #[test]
    fn addresses_and_routes_are_kept_as_they_are_told() {
        let lo = ShownLoopback::new();
        let extra = v4(10, 9, 8, 7, 24);
        lo.add_ip_address(extra).unwrap();
        lo.add_ip_address(extra).unwrap();
        assert_eq!(lo.get_ip_address().len(), 3, "told twice, kept once");
        assert!(lo.get_routes().iter().any(|r| r.dst == v4(10, 9, 8, 0, 24)));
        lo.remove_ip_address(extra).unwrap();
        assert_eq!(lo.get_ip_address().len(), 2);

        let net = v4(192, 168, 7, 0, 24);
        lo.add_route(net, None).unwrap();
        assert!(lo.get_routes().iter().any(|r| r.dst == net));
        lo.del_route(net, None).unwrap();
        assert!(!lo.get_routes().iter().any(|r| r.dst == net));
    }

    /// What the joined sockets say to each other is counted as sent and
    /// as received, on this `lo` and on the one of smoltcp.
    #[test]
    fn local_traffic_is_shown_both_ways() {
        let lo = ShownLoopback::new();
        let before = lo.get_stats();
        count_local(1, 96);
        count_local(2, 1000);
        let after = lo.get_stats();
        assert_eq!(after.rx_packets - before.rx_packets, 3);
        assert_eq!(after.tx_packets - before.tx_packets, 3);
        assert_eq!(after.rx_bytes - before.rx_bytes, 1096);
        assert_eq!(after.tx_bytes - before.tx_bytes, 1096);
        assert_eq!(after.tx_dropped, 0);
    }

    /// A frame an `AF_PACKET` socket puts on it is counted and goes
    /// nowhere.
    #[test]
    fn a_frame_sent_on_it_is_counted_and_gone() {
        let lo = ShownLoopback::new();
        let before = lo.get_stats();
        assert_eq!(lo.send(&[0u8; 60]).unwrap(), 60);
        let after = lo.get_stats();
        assert_eq!(after.tx_packets - before.tx_packets, 1);
        assert_eq!(after.rx_packets - before.rx_packets, 1);
        assert_eq!(after.tx_bytes - before.tx_bytes, 60);
    }
}
