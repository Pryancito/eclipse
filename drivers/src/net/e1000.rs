//! Intel PRO/1000 Network Adapter i.e. e1000 network driver
//! Datasheet: <https://www.intel.ca/content/dam/doc/datasheet/82574l-gbe-controller-datasheet.pdf>

use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::*;
use smoltcp::phy::{self, Checksum, DeviceCapabilities};
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result;

use super::{timer_now_as_micros, ProviderImpl};
use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::net::get_sockets;
use crate::scheme::{NetScheme, Scheme};
use crate::sync::Mutex;
use crate::{DeviceError, DeviceResult};
use isomorphic_drivers::net::ethernet::intel::e1000::E1000;
use isomorphic_drivers::net::ethernet::structs::EthernetAddress as DriverEthernetAddress;

#[derive(Clone)]
pub struct E1000Driver {
    pub hw: Arc<Mutex<E1000>>,
    pub stats: Arc<Mutex<NetStats>>,
}

#[derive(Clone)]
pub struct E1000Interface {
    iface: Arc<Mutex<Interface<'static, E1000Driver>>>,
    driver: E1000Driver,
    name: String,
    irq: usize,
    base: usize,
    poll_pending: Arc<core::sync::atomic::AtomicBool>,
    pub stats: Arc<Mutex<NetStats>>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl E1000Interface {
    fn ims_rearm(&self) {
        unsafe {
            mmio_write(self.base, E1000_IMS, (1 << 7) | (1 << 2)); // RXT0 | LSC
            let _ = mmio_read(self.base, E1000_IMS);
        }
    }

    /// Poll the interface WITHOUT re-arming IMS. The IRQ deferred job re-arms
    /// only AFTER clearing `poll_pending` (see `handle_irq`); re-arming inside
    /// the poll would unmask IMS while `poll_pending` is still true, so a packet
    /// arriving in that window fires an IRQ that hits the `else { ims_rearm() }`
    /// branch — which read-clears ICR without queuing a poll, dropping the RX
    /// frame until the next interrupt. The periodic `poll()` path re-arms itself.
    fn poll_inner(&self) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        // Mutex::lock() uses push_off/pop_off which already disables interrupts
        // for the duration of the critical section. Manual intr_off/on bypasses
        // the noff accounting and panics ("RefCell already borrowed") under SMP.
        let sockets = get_sockets();
        let res = {
            let mut sockets = sockets.lock();
            match self.iface.lock().poll(&mut sockets, timestamp) {
                Ok(p) => {
                    trace!("e1000 NetScheme poll: {:?}", p);
                    Ok(())
                }
                Err(err) => {
                    warn!("poll got err {}", err);
                    Err(DeviceError::IoError)
                }
            }
        };
        super::net_flush_deferred_packets();
        res
    }
}

impl Scheme for E1000Interface {
    fn name(&self) -> &str {
        "e1000"
    }

    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            return;
        }

        let icr = unsafe { mmio_read(self.base, E1000_ICR) };
        if icr == 0 {
            if !self.poll_pending.load(core::sync::atomic::Ordering::SeqCst) {
                self.ims_rearm();
            }
            return;
        }

        if !self.poll_pending.load(core::sync::atomic::Ordering::SeqCst) {
            self.poll_pending
                .store(true, core::sync::atomic::Ordering::SeqCst);
            unsafe {
                mmio_write(self.base, E1000_IMC, 0xffffffff);
                let _ = mmio_read(self.base, E1000_IMC);
            }
            let poll_pending = self.poll_pending.clone();
            let self_clone = self.clone();
            crate::utils::deferred_job::push_deferred_job(move || {
                // Drain WITHOUT re-arming, then clear poll_pending BEFORE
                // re-arming IMS so an IRQ that fires after ims_rearm() finds
                // poll_pending=false and queues a fresh poll, instead of hitting
                // the `else { ims_rearm() }` branch and dropping the RX cause.
                let _ = self_clone.poll_inner();
                poll_pending.store(false, core::sync::atomic::Ordering::SeqCst);
                self_clone.ims_rearm();
            });
        } else {
            self.ims_rearm();
        }
    }
}

impl NetScheme for E1000Interface {
    fn get_mac(&self) -> EthernetAddress {
        self.iface.lock().ethernet_addr()
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    // get ip addresses
    fn get_ip_address(&self) -> Vec<IpCidr> {
        self.ip_addrs.lock().clone()
    }

    fn seed_neighbor(
        &self,
        protocol: smoltcp::wire::IpAddress,
        hardware: smoltcp::wire::EthernetAddress,
    ) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        self.iface
            .lock()
            .seed_neighbor(protocol, hardware, timestamp);
        Ok(())
    }

    fn poll(&self) -> DeviceResult {
        // Periodic (non-IRQ) poll path: drain, then re-arm IMS. The IRQ path
        // calls poll_inner() directly and re-arms only after clearing
        // poll_pending (see handle_irq).
        let res = self.poll_inner();
        self.ims_rearm();
        res
    }

    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        // Try to read directly from hardware.
        if let Some(pkt) = self.driver.hw.lock().receive() {
            let n = pkt.len().min(buf.len());
            buf[..n].copy_from_slice(&pkt[..n]);
            Ok(n)
        } else {
            Err(DeviceError::NotReady)
        }
    }

    fn send(&self, data: &[u8]) -> DeviceResult<usize> {
        // send() re-checks descriptor ownership under this same lock, so a
        // full/in-flight ring returns NotReady instead of corrupting a slot.
        let mut driver = self.driver.hw.lock();
        if driver.send(data) {
            Ok(data.len())
        } else {
            Err(DeviceError::NotReady)
        }
    }

    fn can_recv(&self) -> bool {
        // Return true so callers always attempt recv(); actual receive will return NotReady if nothing.
        true
    }

    fn can_send(&self) -> bool {
        self.driver.hw.lock().can_send()
    }

    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        let mut iface = self.iface.lock();
        iface.update_ip_addrs(|addrs| {
            let mut set_primary = false;
            for slot in addrs.iter_mut() {
                if let IpCidr::Ipv4(_) = slot {
                    if !set_primary {
                        *slot = IpCidr::Ipv4(cidr);
                        set_primary = true;
                    } else {
                        *slot = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0));
                    }
                }
            }
            if !set_primary {
                if let Some(slot) = addrs.iter_mut().next() {
                    *slot = IpCidr::Ipv4(cidr);
                }
            }
        });
        *self.ip_addrs.lock() = iface.ip_addrs().to_vec();
        Ok(())
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

    fn add_route(&self, cidr: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        match gateway {
            Some(IpAddress::Ipv4(gw)) => {
                if cidr.prefix_len() == 0 {
                    let _ = iface.routes_mut().remove_default_ipv4_route();
                    iface
                        .routes_mut()
                        .add_default_ipv4_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                }
                let mut routes = self.routes.lock();
                routes.retain(|r| !(matches!(r.dst, IpCidr::Ipv4(_)) && r.dst.prefix_len() == 0));
                routes.push(RouteInfo {
                    dst: cidr,
                    gateway: Some(IpAddress::Ipv4(gw)),
                });
            }
            Some(IpAddress::Ipv6(gw)) => {
                if cidr.prefix_len() == 0 {
                    let _ = iface.routes_mut().remove_default_ipv6_route();
                    iface
                        .routes_mut()
                        .add_default_ipv6_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                }
                let mut routes = self.routes.lock();
                routes.retain(|r| !(matches!(r.dst, IpCidr::Ipv6(_)) && r.dst.prefix_len() == 0));
                routes.push(RouteInfo {
                    dst: cidr,
                    gateway: Some(IpAddress::Ipv6(gw)),
                });
            }
            None => {
                self.routes.lock().push(RouteInfo { dst: cidr, gateway });
            }
            _ => {}
        }
        Ok(())
    }

    fn del_route(&self, cidr: IpCidr, _gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        if cidr.prefix_len() == 0 {
            match cidr {
                IpCidr::Ipv4(_) => {
                    let _ = iface.routes_mut().remove_default_ipv4_route();
                }
                IpCidr::Ipv6(_) => {
                    let _ = iface.routes_mut().remove_default_ipv6_route();
                }
                _ => {}
            }
        }
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
        1500
    }
}

pub struct E1000RxToken {
    data: Vec<u8>,
    stats: Arc<Mutex<NetStats>>,
}

pub struct E1000TxToken {
    driver: E1000Driver,
    stats: Arc<Mutex<NetStats>>,
}

impl phy::Device<'_> for E1000Driver {
    type RxToken = E1000RxToken;
    type TxToken = E1000TxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        self.hw.lock().receive().map(|pkt| {
            (
                E1000RxToken {
                    data: pkt,
                    stats: self.stats.clone(),
                },
                E1000TxToken {
                    driver: self.clone(),
                    stats: self.stats.clone(),
                },
            )
        })
    }

    fn transmit(&mut self) -> Option<Self::TxToken> {
        if self.hw.lock().can_send() {
            Some(E1000TxToken {
                driver: self.clone(),
                stats: self.stats.clone(),
            })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        // Do NOT set max_burst_size. smoltcp clamps the TCP window to
        // `burst * MSS` and stores it in a u16: burst=64 → 64*1474=94336,
        // which wraps to 28800 and caps throughput at a few Mbps.
        caps.max_burst_size = None;
        caps
    }
}

impl phy::RxToken for E1000RxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut stats = self.stats.lock();
        stats.rx_packets += 1;
        stats.rx_bytes += self.data.len() as u64;
        drop(stats);

        // Dispatch to global packet tapping (AF_PACKET sockets) after smoltcp unlocks SOCKETS.
        super::net_defer_packet(&self.data);
        f(&mut self.data)
    }
}

impl phy::TxToken for E1000TxToken {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut buffer = [0u8; 1536];
        let result = f(&mut buffer[..len]);

        let mut driver = self.driver.hw.lock();
        let sent = driver.send(&buffer[..len]);
        drop(driver);

        // Only account a frame that was actually posted; if the descriptor was
        // still in flight, send() dropped it (smoltcp/TCP will retransmit)
        // rather than overwriting an in-flight slot.
        if sent {
            let mut stats = self.stats.lock();
            stats.tx_packets += 1;
            stats.tx_bytes += len as u64;
        }

        result
    }
}

pub fn init(
    name: String,
    irq: usize,
    header: usize,
    size: usize,
    index: usize,
) -> DeviceResult<E1000Interface> {
    info!("Probing e1000 {}", name);

    let mac: [u8; 6] = [0x54, 0x51, 0x9F, 0x71, 0xC0, index as u8];
    let ethernet_addr = EthernetAddress::from_bytes(&mac);
    let e1000 = E1000::new(header, size, ethernet_addr)?;
    let hw = Arc::new(Mutex::new(e1000));
    let stats = Arc::new(Mutex::new(NetStats::default()));
    let net_driver = E1000Driver {
        hw: hw.clone(),
        stats: stats.clone(),
    };

    let mut eui64 = [0u8; 8];
    eui64[0] = mac[0] ^ 2;
    eui64[1] = mac[1];
    eui64[2] = mac[2];
    eui64[3] = 0xff;
    eui64[4] = 0xfe;
    eui64[5] = mac[3];
    eui64[6] = mac[4];
    eui64[7] = mac[5];
    let link_local = Ipv6Address::new(
        0xfe80,
        0,
        0,
        0,
        (eui64[0] as u16) << 8 | eui64[1] as u16,
        (eui64[2] as u16) << 8 | eui64[3] as u16,
        (eui64[4] as u16) << 8 | eui64[5] as u16,
        (eui64[6] as u16) << 8 | eui64[7] as u16,
    );

    let ip_addrs = vec![
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::Ipv6(Ipv6Cidr::new(link_local, 64)),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
    ];
    static mut ROUTES_STORAGE: [Option<(IpCidr, Route)>; 4] = [None; 4];
    let routes = unsafe { Routes::new(&mut ROUTES_STORAGE[..]) };
    let neighbor_cache = NeighborCache::new(BTreeMap::new());

    let iface = InterfaceBuilder::new(net_driver.clone())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(neighbor_cache)
        .ip_addrs(ip_addrs.clone())
        .routes(routes)
        .finalize();

    crate::klog_info!("e1000 interface {} discovered", name);
    let e1000_iface = E1000Interface {
        iface: Arc::new(Mutex::new(iface)),
        driver: net_driver,
        name,
        irq,
        base: header,
        poll_pending: Arc::new(core::sync::atomic::AtomicBool::new(false)),
        stats,
        routes: Arc::new(Mutex::new(vec![])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    };

    Ok(e1000_iface)
}

pub struct E1000DriverPci;

impl PciDriver for E1000DriverPci {
    fn name(&self) -> &str {
        "e1000"
    }

    fn matched(&self, vendor_id: u16, device_id: u16) -> bool {
        vendor_id == 0x8086 && (device_id == 0x100e || device_id == 0x100f)
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[0] {
            if let Some(m) = mapper {
                m.query_or_map(addr as usize, 4096 * 8);
            }
            let vaddr = crate::bus::phys_to_virt(addr as usize);
            let name = crate::net::next_eth_ifname();
            let vector = irq.map(|idx| idx + 32).unwrap_or(0);
            let iface = init(name, vector, vaddr, len as usize, 0)?;
            Ok(Device::Net(Arc::new(iface)))
        } else {
            Err(crate::DeviceError::NotSupported)
        }
    }
}
