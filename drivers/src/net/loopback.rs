// smoltcp
use alloc::collections::VecDeque;
use smoltcp::{
    iface::Interface,
    phy::{self, DeviceCapabilities, Medium},
    time::Instant,
    Result,
};

use crate::net::get_sockets;
use alloc::sync::Arc;

use crate::sync::Mutex;
use alloc::string::String;

use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::{DeviceError, DeviceResult};

use alloc::vec::Vec;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Cidr};

pub struct LoopbackDevice {
    queue: VecDeque<Vec<u8>>,
    medium: Medium,
    stats: Arc<Mutex<NetStats>>,
}

impl LoopbackDevice {
    pub fn new(medium: Medium, stats: Arc<Mutex<NetStats>>) -> Self {
        Self {
            queue: VecDeque::new(),
            medium,
            stats,
        }
    }
}

impl<'a> phy::Device<'a> for LoopbackDevice {
    type RxToken = LoopbackRxToken;
    type TxToken = LoopbackTxToken<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 65535;
        caps.medium = self.medium;
        caps
    }

    fn receive(&'a mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        let stats = self.stats.clone();
        self.queue.pop_front().map(move |buffer| {
            let rx = LoopbackRxToken {
                buffer,
                stats: stats.clone(),
            };
            let tx = LoopbackTxToken {
                queue: &mut self.queue,
                stats,
            };
            (rx, tx)
        })
    }

    fn transmit(&'a mut self) -> Option<Self::TxToken> {
        Some(LoopbackTxToken {
            queue: &mut self.queue,
            stats: self.stats.clone(),
        })
    }
}

pub struct LoopbackRxToken {
    buffer: Vec<u8>,
    stats: Arc<Mutex<NetStats>>,
}

impl phy::RxToken for LoopbackRxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut stats = self.stats.lock();
        stats.rx_packets += 1;
        stats.rx_bytes += self.buffer.len() as u64;
        drop(stats);

        f(&mut self.buffer)
    }
}

pub static mut LOOPBACK_TX_CALLBACK: Option<fn(&[u8])> = None;

pub fn register_loopback_tx_callback(cb: fn(&[u8])) {
    unsafe {
        LOOPBACK_TX_CALLBACK = Some(cb);
    }
}

/// Upper bound on frames buffered in the loopback queue. `poll()` uses
/// `try_lock` and silently skips on contention, so without a cap a sender that
/// outruns the drain would grow the queue without bound and exhaust the kernel
/// heap. When full we drop the oldest frame (backpressure); TCP retransmits.
const LOOPBACK_QUEUE_MAX: usize = 256;

pub struct LoopbackTxToken<'a> {
    queue: &'a mut VecDeque<Vec<u8>>,
    stats: Arc<Mutex<NetStats>>,
}

impl<'a> phy::TxToken for LoopbackTxToken<'a> {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        // Refuse, do NOT clamp. `TxToken::consume` must hand the closure a
        // slice of exactly `len`; shortening it makes smoltcp's own
        // `emit_payload` `copy_from_slice` a longer payload into a shorter
        // buffer and panic inside the kernel. Reachable from userspace: the
        // loopback runs `Medium::Ip`, `UDP_SENDBUF` is 64 KiB, and this
        // smoltcp has no egress MTU clamp — a single 64 KiB `sendto` to
        // 127.0.0.1 asks for `len = 65536 + 28`.
        const MAX_TX_COPY: usize = 65536;
        if len > MAX_TX_COPY {
            // Count the refusal. `tx_errors` exists so that `/proc/net/dev` can
            // say a frame was thrown away, and this used to leave it at zero.
            self.stats.lock().tx_errors += 1;
            return Err(smoltcp::Error::Exhausted);
        }
        let mut buffer = alloc::vec![0u8; len];
        let result = f(&mut buffer)?;

        // Whether the cap below is about to throw a frame away. Read before the
        // counters so the whole update takes the stats lock once.
        let dropping = self.queue.len() >= LOOPBACK_QUEUE_MAX;

        let mut stats = self.stats.lock();
        stats.tx_packets += 1;
        stats.tx_bytes += len as u64;
        if dropping {
            // Dropping the OLDEST frame is still dropping a frame, and
            // `/proc/net/dev` is the only place anyone would find out that
            // 127.0.0.1 is shedding traffic. It read zero.
            stats.tx_dropped += 1;
        }
        drop(stats);

        unsafe {
            if let Some(cb) = LOOPBACK_TX_CALLBACK {
                cb(&buffer);
            }
        }

        if dropping {
            self.queue.pop_front();
        }
        self.queue.push_back(buffer);
        Ok(result)
    }
}

#[derive(Clone)]
pub struct LoopbackInterface {
    pub iface: Arc<Mutex<Interface<'static, LoopbackDevice>>>,
    pub name: String,
    pub stats: Arc<Mutex<NetStats>>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl Scheme for LoopbackInterface {
    fn name(&self) -> &str {
        "loopback"
    }

    fn handle_irq(&self, _cause: usize) {}
}

impl NetScheme for LoopbackInterface {
    fn recv(&self, _buf: &mut [u8]) -> DeviceResult<usize> {
        // The loopback has no hardware RX ring; frames are delivered through
        // `poll`. `netdev_drain_rx` skips this device by comparing its name
        // against the literal "loopback", so `unimplemented!()` here turned a
        // future rename (to the Linux-conventional "lo", say) into a kernel
        // panic on a routine RX drain.
        Err(DeviceError::NotSupported)
    }
    fn send(&self, buf: &[u8]) -> DeviceResult<usize> {
        let dropped = {
            let mut iface = self.iface.lock();
            let queue = &mut iface.device_mut().queue;
            let dropped = queue.len() >= LOOPBACK_QUEUE_MAX;
            if dropped {
                queue.pop_front();
            }
            queue.push_back(buf.to_vec());
            dropped
        };
        // This path touched NO counter at all, so every frame an `AF_PACKET`
        // socket put on the loopback was invisible in `/proc/net/dev` -- and so
        // was every frame the cap above threw away. The `TxToken` path next door
        // has always counted, which is what made the two disagree.
        let mut stats = self.stats.lock();
        stats.tx_packets += 1;
        stats.tx_bytes += buf.len() as u64;
        if dropped {
            stats.tx_dropped += 1;
        }
        Ok(buf.len())
    }
    fn poll(&self) -> DeviceResult {
        let timestamp = Instant::from_micros(crate::net::timer_now_as_micros() as i64);
        let Some(mut iface) = self.iface.try_lock() else {
            return Ok(());
        };
        let sockets = get_sockets();
        let Some(mut sockets) = sockets.try_lock() else {
            return Ok(());
        };
        match iface.poll(&mut sockets, timestamp) {
            Ok(_) => Ok(()),
            Err(err) => {
                debug!("poll got err {}", err);
                Err(DeviceError::IoError)
            }
        }
    }

    fn get_mac(&self) -> EthernetAddress {
        EthernetAddress::default()
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    fn get_ip_address(&self) -> Vec<IpCidr> {
        self.ip_addrs.lock().clone()
    }

    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        let mut iface = self.iface.lock();
        iface.update_ip_addrs(|addrs| {
            if addrs.contains(&cidr) {
                return;
            }
            for slot in addrs.iter_mut() {
                if (slot.address().is_unspecified() && slot.prefix_len() == 0)
                    || (slot.address() == IpAddress::v4(240, 0, 0, 0) && slot.prefix_len() == 32)
                {
                    *slot = cidr;
                    return;
                }
            }
            if let Some(slot) = addrs.iter_mut().last() {
                *slot = cidr;
            }
        });
        *self.ip_addrs.lock() = iface.ip_addrs().to_vec();
        Ok(())
    }

    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        let mut iface = self.iface.lock();
        iface.update_ip_addrs(|addrs| {
            for slot in addrs.iter_mut() {
                if *slot == cidr {
                    *slot = IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0);
                    return;
                }
            }
        });
        *self.ip_addrs.lock() = iface.ip_addrs().to_vec();
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
        let iface = self.iface.lock();
        let mut res = Vec::new();

        // 1. Add tracked routes
        res.extend(self.routes.lock().clone());

        // 2. Add direct routes
        for cidr in iface.ip_addrs() {
            match cidr {
                IpCidr::Ipv4(v4) if v4.prefix_len() > 0 => {
                    res.push(RouteInfo {
                        dst: IpCidr::Ipv4(v4.network()),
                        gateway: None,
                    });
                }
                IpCidr::Ipv6(v6) if v6.prefix_len() > 0 => {
                    res.push(RouteInfo {
                        dst: IpCidr::Ipv6(v6.network()),
                        gateway: None,
                    });
                }
                _ => {}
            }
        }
        res
    }

    fn get_stats(&self) -> NetStats {
        self.stats.lock().clone()
    }
    fn get_mtu(&self) -> usize {
        65535
    }
}

/// The loopback, 294 lines with no tests.
///
/// Everything that binds `127.0.0.1` goes through here, and its own comments
/// record two hazards it learned the hard way: a queue cap that drops the oldest
/// frame, and a `consume` that refuses an oversized length instead of clamping
/// it (clamping panicked the kernel from userspace). Neither of those was
/// counted anywhere, so `/proc/net/dev` -- the one place anyone would look to
/// find out that the loopback is shedding traffic -- read zero while it shed.
#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::iface::{InterfaceBuilder, Route, Routes};
    use smoltcp::phy::{Device, RxToken, TxToken};

    fn stats() -> Arc<Mutex<NetStats>> {
        Arc::new(Mutex::new(NetStats::default()))
    }

    fn at(micros: i64) -> Instant {
        Instant::from_micros(micros)
    }

    /// Put one frame of `len` bytes on the wire, filled with `fill`.
    fn transmit(dev: &mut LoopbackDevice, len: usize, fill: u8) -> Result<()> {
        let token = dev.transmit().expect("the loopback always has room");
        token.consume(at(0), len, |buf| {
            buf.fill(fill);
            Ok(())
        })
    }

    /// A loopback interface over its own device, the way `kernel-hal` builds it:
    /// `127.0.0.1/8` and `::1/128`, four route slots, no gateway.
    fn interface(stats: Arc<Mutex<NetStats>>) -> LoopbackInterface {
        let device = LoopbackDevice::new(Medium::Ip, stats.clone());
        let ip_addrs = alloc::vec![
            IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8),
            IpCidr::new(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 128),
        ];
        let routes_storage: &'static mut [Option<(IpCidr, Route)>] =
            alloc::boxed::Box::leak(alloc::vec![None; 4].into_boxed_slice());
        let iface = InterfaceBuilder::new(device)
            .ip_addrs(ip_addrs.clone())
            .routes(Routes::new(routes_storage))
            .finalize();
        LoopbackInterface {
            iface: Arc::new(Mutex::new(iface)),
            name: String::from("loopback"),
            stats,
            routes: Arc::new(Mutex::new(Vec::new())),
            ip_addrs: Arc::new(Mutex::new(ip_addrs)),
        }
    }

    /// What a loopback is for: the bytes handed to `transmit` are the bytes
    /// `receive` gives back, and both directions are counted.
    #[test]
    fn what_goes_in_comes_back_out_and_both_ends_are_counted() {
        let stats = stats();
        let mut dev = LoopbackDevice::new(Medium::Ip, stats.clone());
        transmit(&mut dev, 40, 0xa5).unwrap();
        {
            let s = stats.lock();
            assert_eq!((s.tx_packets, s.tx_bytes), (1, 40));
            assert_eq!((s.rx_packets, s.rx_bytes), (0, 0), "nothing received yet");
        }

        let (rx, _tx) = dev.receive().expect("the frame just sent must come back");
        rx.consume(at(1), |buf| {
            assert_eq!(buf.len(), 40);
            assert!(
                buf.iter().all(|b| *b == 0xa5),
                "the payload came back changed"
            );
            Ok(())
        })
        .unwrap();

        let s = stats.lock();
        assert_eq!((s.rx_packets, s.rx_bytes), (1, 40));
    }

    #[test]
    fn an_empty_loopback_has_nothing_to_receive() {
        let stats = stats();
        let mut dev = LoopbackDevice::new(Medium::Ip, stats);
        assert!(dev.receive().is_none());
    }

    /// Frames come back in the order they went out: a loopback that reordered
    /// would look like a lossy network to every TCP connection on the machine.
    #[test]
    fn frames_come_back_in_the_order_they_went_out() {
        let stats = stats();
        let mut dev = LoopbackDevice::new(Medium::Ip, stats);
        for i in 0..4u8 {
            transmit(&mut dev, 8, i).unwrap();
        }
        for i in 0..4u8 {
            let (rx, _tx) = dev.receive().expect("four went out, four must come back");
            rx.consume(at(0), |buf| {
                assert_eq!(buf[0], i, "the loopback reordered its queue");
                Ok(())
            })
            .unwrap();
        }
        assert!(dev.receive().is_none());
    }

    /// The oversized length is REFUSED, not clamped -- its own comment says
    /// clamping made smoltcp `copy_from_slice` a long payload into a short buffer
    /// and panic the kernel, reachable from a single 64 KiB `sendto` to
    /// 127.0.0.1. And the refusal is now counted.
    #[test]
    fn an_oversized_frame_is_refused_and_counted_as_an_error() {
        let stats = stats();
        let mut dev = LoopbackDevice::new(Medium::Ip, stats.clone());
        let token = dev.transmit().unwrap();
        let mut closure_ran = false;
        let result = token.consume(at(0), 65537, |_buf| {
            closure_ran = true;
            Ok(())
        });
        assert!(result.is_err(), "an oversized length must be refused");
        assert!(
            !closure_ran,
            "the closure must never see a buffer shorter than the length it asked for"
        );
        let s = stats.lock();
        assert_eq!(s.tx_errors, 1, "a refused frame must show up in tx_errors");
        assert_eq!(s.tx_packets, 0, "and must not count as sent");
    }

    /// 65536 is the largest length that is still accepted, so the boundary is
    /// where it says it is.
    #[test]
    fn the_largest_accepted_frame_is_exactly_the_limit() {
        let stats = stats();
        let mut dev = LoopbackDevice::new(Medium::Ip, stats.clone());
        transmit(&mut dev, 65536, 0).unwrap();
        assert_eq!(stats.lock().tx_errors, 0);
    }

    /// The cap keeps the queue bounded -- a sender that outruns the drain would
    /// otherwise eat the kernel heap -- and every frame it throws away is now
    /// counted. It used to be silent, which is the worst of both: the traffic
    /// disappears and `/proc/net/dev` says everything is fine.
    #[test]
    fn the_queue_cap_drops_the_oldest_frame_and_says_so() {
        let stats = stats();
        let mut dev = LoopbackDevice::new(Medium::Ip, stats.clone());
        for i in 0..(LOOPBACK_QUEUE_MAX + 3) {
            transmit(&mut dev, 4, i as u8).unwrap();
        }
        assert_eq!(
            dev.queue.len(),
            LOOPBACK_QUEUE_MAX,
            "the queue must stay capped"
        );
        assert_eq!(
            stats.lock().tx_dropped,
            3,
            "one tx_dropped per frame thrown away"
        );
        // And what survived is the TAIL, not the head: the three oldest went.
        let (rx, _tx) = dev.receive().unwrap();
        rx.consume(at(0), |buf| {
            assert_eq!(buf[0], 3, "the three oldest frames should be the ones gone");
            Ok(())
        })
        .unwrap();
    }

    /// `NetScheme::send` is the raw path -- what an `AF_PACKET` socket uses -- and
    /// it used to touch no counter at all. Two identical frames down the two
    /// paths must move the counters the same way, or `/proc/net/dev` depends on
    /// which socket sent the traffic.
    #[test]
    fn the_raw_send_path_counts_the_same_as_the_smoltcp_one() {
        let through_token = stats();
        let mut dev = LoopbackDevice::new(Medium::Ip, through_token.clone());
        transmit(&mut dev, 60, 0).unwrap();

        let through_send = stats();
        interface(through_send.clone()).send(&[0u8; 60]).unwrap();

        let (a, b) = (through_token.lock().clone(), through_send.lock().clone());
        assert_eq!(
            (a.tx_packets, a.tx_bytes),
            (b.tx_packets, b.tx_bytes),
            "the two send paths disagree about what they sent"
        );
    }

    #[test]
    fn the_raw_send_path_also_reports_what_the_cap_throws_away() {
        let stats = stats();
        let iface = interface(stats.clone());
        for _ in 0..(LOOPBACK_QUEUE_MAX + 2) {
            iface.send(&[0u8; 4]).unwrap();
        }
        assert_eq!(stats.lock().tx_dropped, 2);
    }

    /// `recv` answers `NotSupported` rather than panicking: `netdev_drain_rx`
    /// skips this device by comparing its name against the literal "loopback",
    /// so an `unimplemented!()` here turned a rename into a kernel panic on a
    /// routine RX drain.
    #[test]
    fn the_raw_receive_path_refuses_instead_of_panicking() {
        let iface = interface(stats());
        let mut buf = [0u8; 64];
        assert!(iface.recv(&mut buf).is_err());
    }

    /// The MTU the kernel advertises and the one the device reports have to be
    /// the same number, or something sizes a buffer by the wrong one.
    #[test]
    fn the_advertised_mtu_and_the_device_capability_agree() {
        let stats = stats();
        let mut dev = LoopbackDevice::new(Medium::Ip, stats.clone());
        let caps = dev.capabilities();
        assert_eq!(caps.max_transmission_unit, interface(stats).get_mtu());
        assert_eq!(caps.medium, Medium::Ip, "the loopback runs Medium::Ip");
    }

    /// The loopback has no hardware, so its MAC is all zeroes -- and that must
    /// stay true, because `netdev` code that special-cases the loopback reads it.
    #[test]
    fn the_loopback_has_no_hardware_address() {
        assert_eq!(
            interface(stats()).get_mac(),
            EthernetAddress::from_bytes(&[0; 6])
        );
    }

    #[test]
    fn a_new_address_lands_in_a_free_slot_and_the_old_ones_stay() {
        let iface = interface(stats());
        let before = iface.get_ip_address();
        iface
            .add_ip_address(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
            .unwrap();
        let after = iface.get_ip_address();
        assert!(
            after.contains(&IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24)),
            "the address was not added"
        );
        assert!(
            after.contains(&IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)),
            "adding an address must not cost you 127.0.0.1"
        );
        assert!(after.len() >= before.len());
    }

    #[test]
    fn adding_the_same_address_twice_does_not_duplicate_it() {
        let iface = interface(stats());
        let loops = IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8);
        iface.add_ip_address(loops).unwrap();
        assert_eq!(
            iface
                .get_ip_address()
                .iter()
                .filter(|a| **a == loops)
                .count(),
            1
        );
    }

    /// A removed address is blanked to `0.0.0.0/0`, which is the sentinel
    /// `add_ip_address` reuses. That is deliberate -- smoltcp's address storage
    /// is a fixed array -- so what has to hold is that the address itself is
    /// gone and that the blank slot is not mistaken for a route.
    #[test]
    fn a_removed_address_stops_being_reachable_and_leaves_no_route() {
        let iface = interface(stats());
        let v4 = IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8);
        iface.remove_ip_address(v4).unwrap();
        assert!(!iface.get_ip_address().contains(&v4), "still there");
        for route in iface.get_routes() {
            assert_ne!(
                route.dst,
                IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
                "the blanked slot must not become a default route"
            );
        }
    }

    /// The direct route for an address is derived from the address, not stored,
    /// so `127.0.0.1/8` has to produce `127.0.0.0/8`.
    #[test]
    fn a_configured_address_yields_its_own_network_as_a_direct_route() {
        let iface = interface(stats());
        let routes = iface.get_routes();
        assert!(
            routes.iter().any(|r| r.dst
                == IpCidr::Ipv4(Ipv4Cidr::new(
                    smoltcp::wire::Ipv4Address::new(127, 0, 0, 0),
                    8
                ))
                && r.gateway.is_none()),
            "127.0.0.1/8 must give a direct route to 127.0.0.0/8, got {:?}",
            routes
        );
    }

    #[test]
    fn a_route_added_and_removed_leaves_nothing_behind() {
        let iface = interface(stats());
        let dst = IpCidr::new(IpAddress::v4(192, 168, 1, 0), 24);
        let before = iface.get_routes().len();
        iface.add_route(dst, None).unwrap();
        assert_eq!(iface.get_routes().len(), before + 1);
        iface.del_route(dst, None).unwrap();
        assert_eq!(iface.get_routes().len(), before);
        assert!(!iface.get_routes().iter().any(|r| r.dst == dst));
    }

    #[test]
    fn the_interface_reports_the_name_it_was_given() {
        let iface = interface(stats());
        assert_eq!(iface.get_ifname(), "loopback");
        assert_eq!(Scheme::name(&iface), "loopback");
    }
}
