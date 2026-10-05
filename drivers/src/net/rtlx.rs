use crate::sync::Mutex;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::*;
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
// use smoltcp::socket::SocketSet;
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result;

use super::realtek::rtl8211f::{self, RTL8211F};
use super::{timer_now_as_micros, ProviderImpl, PAGE_SIZE};

/// Stack scratch for [`RTLxTxToken::consume`]; covers the advertised MTU
/// (1514) with headroom. Oversized requests are refused, never clipped.
const TX_SCRATCH_LEN: usize = 1536;

use crate::net::get_sockets;
use crate::scheme::{NetScheme, RouteInfo, Scheme};
use crate::{DeviceError, DeviceResult};

#[derive(Clone)]
pub struct RTLxDriver(Arc<Mutex<RTL8211F<ProviderImpl>>>);

#[derive(Clone)]
pub struct RTLxInterface {
    pub iface: Arc<Mutex<Interface<'static, RTLxDriver>>>,
    pub driver: RTLxDriver,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub name: String,
    pub irq: usize,
}

impl Scheme for RTLxInterface {
    fn name(&self) -> &str {
        "rtl8211f"
    }

    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            // not ours, skip it
            return;
        }

        // Reading the status ACKS the interrupt (`interrupt_status` writes the
        // bits back), so whatever comes out of here is the only record left of
        // why the NIC raised the line.
        let status = self.driver.0.lock().interrupt_status();

        if status == rtl8211f::TxDmaIrqStatus::HandleTxRx as i32 {
            let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
            let sockets = get_sockets();
            let mut sockets = sockets.lock();

            {
                let mut driver = self.driver.0.lock();
                driver.int_disable();
                // Reclaim the descriptors the TX DMA has finished with, before
                // the poll below wants to post more. `tx_complete` is the only
                // thing that advances `tx_clean`, `can_send` computes the free
                // slots from it -- and apart from here, `tx_complete` is called
                // from inside `geth_send`, which is the very thing `can_send`
                // gates. So a ring that fills up while the DMA is not completing
                // answers "full" to every caller afterwards and has no way back:
                // the doorbell that would restart the DMA (`tx_poll`) is rung
                // from inside `geth_send` too. The driver's own handler does this
                // first, marked "why? from Linux driver"; the handler the kernel
                // actually installs is this one, and it did not.
                driver.tx_complete();
            }
            match self.iface.lock().poll(&mut sockets, timestamp) {
                Ok(b) => {
                    debug!("nic poll, is changed ?: {}", b);
                }
                Err(err) => {
                    error!("poll got err {}", err);
                }
            }
            self.driver.0.lock().int_enable();
            drop(sockets);
            // Drain the AF_PACKET tap queue and wake blocked socket readers.
            // `RTLxRxToken::consume` calls `net_defer_packet` for every frame,
            // but nothing on this driver ever flushed that queue, so an
            // AF_PACKET consumer (the DHCP client) received nothing at all on
            // riscv64 and blocked readers only woke on the fallback park
            // timer. Both e1000 drivers do this after every poll.
            super::net_flush_deferred_packets();
            super::wake_net_rx_waiters();
        } else if status == rtl8211f::TxDmaIrqStatus::TxHardError as i32
            || status == rtl8211f::TxDmaIrqStatus::TxHardErrorBumpTc as i32
        {
            // TX_STOP_INT / TX_UNF_INT: the transmit DMA has stopped. `int_enable`
            // arms exactly `RX_INT | TX_UNF_INT`, so this is one of only two
            // interrupts this NIC can raise -- and it used to fall off the end of
            // the `if` without so much as a log line, having already been acked.
            // That is how the stall above becomes permanent: the DMA stops, every
            // descriptor keeps OWN set, `tx_complete` reclaims nothing, the ring
            // fills, and the NIC never transmits again with nothing anywhere
            // saying why. Reclaim what did complete and ring the doorbell so the
            // DMA picks the ring up again.
            //
            // Not the threshold bump that `TxHardErrorBumpTc` is named after:
            // raising the store-and-forward threshold needs a GMAC that can be
            // watched doing it, and this restart is what makes the difference
            // between a NIC that comes back and one that does not.
            error!(
                "rtl8211f: transmit DMA stopped (status {}), restarting it",
                status
            );
            let mut driver = self.driver.0.lock();
            driver.tx_complete();
            driver.tx_poll();
        }
    }
}

impl NetScheme for RTLxInterface {
    fn get_mac(&self) -> EthernetAddress {
        self.iface.lock().ethernet_addr()
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    fn get_ip_address(&self) -> Vec<IpCidr> {
        Vec::from(self.iface.lock().ip_addrs())
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
        Ok(())
    }

    fn add_route(&self, cidr: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        match gateway {
            Some(IpAddress::Ipv4(gw)) => {
                let mut routes = self.routes.lock();
                if cidr.prefix_len() == 0 {
                    iface
                        .routes_mut()
                        .add_default_ipv4_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                    // Only one default route can be in force, so a new one
                    // replaces the one already there. A route to a named
                    // network replaces nothing -- and this purge used to run
                    // for every gatewayed route, so the static route in a DHCP
                    // lease took 0.0.0.0/0 out of the table the kernel reports
                    // while smoltcp went on routing through it.
                    routes
                        .retain(|r| !(matches!(r.dst, IpCidr::Ipv4(_)) && r.dst.prefix_len() == 0));
                }
                routes.push(RouteInfo {
                    dst: cidr,
                    gateway: Some(IpAddress::Ipv4(gw)),
                });
            }
            Some(IpAddress::Ipv6(gw)) => {
                let mut routes = self.routes.lock();
                if cidr.prefix_len() == 0 {
                    iface
                        .routes_mut()
                        .add_default_ipv6_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                    // Only one default route can be in force, so a new one
                    // replaces the one already there. A route to a named
                    // network replaces nothing -- and this purge used to run
                    // for every gatewayed route, so the static route in a DHCP
                    // lease took ::/0 out of the table the kernel reports
                    // while smoltcp went on routing through it.
                    routes
                        .retain(|r| !(matches!(r.dst, IpCidr::Ipv6(_)) && r.dst.prefix_len() == 0));
                }
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

    fn del_route(&self, cidr: IpCidr, _gateway: Option<IpAddress>) -> DeviceResult {
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

        res.extend(self.routes.lock().clone());

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

    fn poll(&self) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        // Disable interrupts while holding the SOCKETS and iface locks.
        // On real hardware the NIC fires a hardware interrupt as soon as a
        // frame lands in the DMA ring.  If that interrupt is delivered while
        // this thread already holds SOCKETS, handle_irq() will try to acquire
        // the same lock and spin forever, dead-locking the system.
        // The kernel-sync Mutex already keeps interrupts off for the duration
        // of the locked critical section (push_off/pop_off), so the NIC IRQ
        // cannot reenter while SOCKETS is held. Manual intr_off/on here would
        // desync the noff accounting and panic ("pop_off" / "RefCell already
        // borrowed") under SMP, so we rely on the Mutex alone.
        let sockets = get_sockets();
        let mut sockets = sockets.lock();
        let result = self.iface.lock().poll(&mut sockets, timestamp);
        // Release the SOCKETS guard promptly so interrupts (disabled by the
        // lock) are re-enabled as soon as the critical section ends.
        drop(sockets);
        // Same as `handle_irq`: flush the AF_PACKET tap queue and wake RX
        // waiters now that the smoltcp locks are released.
        super::net_flush_deferred_packets();
        match result {
            Ok(b) => {
                debug!("nic poll, is changed ?: {}", b);
                if b {
                    super::wake_net_rx_waiters();
                }
                Ok(())
            }
            Err(err) => {
                error!("poll got err {}", err);
                Err(DeviceError::IoError)
            }
        }
    }

    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        // One acquisition for the question and for the answer, like `send` right
        // below already does. Two of them let another CPU take the frame in
        // between, and `geth_recv` would then walk a ring whose cursor had moved.
        let mut driver = self.driver.0.lock();
        if !driver.can_recv() {
            return Err(DeviceError::NotReady);
        }
        let (vec_recv, _rxcount) = driver.geth_recv(1);
        // An empty answer is not a zero-byte frame, it is no frame: `geth_recv`
        // spends the budget on a frame the GMAC marked bad (CRC error, runt, MII
        // error -- routine on a marginal cable, which is the state this board
        // boots in) and hands back an empty `Vec`. `Ok(0)` for that is
        // end-of-file to whatever called `read`.
        if vec_recv.is_empty() {
            return Err(DeviceError::NotReady);
        }
        // `copy_from_slice` panics unless the lengths match; the caller's
        // buffer is MTU-sized while `vec_recv` is the actual frame, so copy
        // only the received bytes and return that count (cf. e1000e::recv).
        let n = vec_recv.len().min(buf.len());
        buf[..n].copy_from_slice(&vec_recv[..n]);
        Ok(n)
    }

    fn send(&self, data: &[u8]) -> DeviceResult<usize> {
        // Hold the lock across can_send() and geth_send() so another CPU cannot
        // consume the slot in between (TOCTOU) and have geth_send() post into an
        // in-flight descriptor. Propagate the error instead of unwrap()-panicking
        // on an over-size frame.
        let mut driver = self.driver.0.lock();
        if !driver.can_send() {
            return Err(DeviceError::NotReady);
        }
        driver.geth_send(data).map_err(|_| DeviceError::IoError)?;
        Ok(data.len())
    }
    fn get_mtu(&self) -> usize {
        1500
    }
}

pub struct RTLxRxToken(Vec<u8>);
pub struct RTLxTxToken(RTLxDriver);

impl<'a> Device<'a> for RTLxDriver {
    type RxToken = RTLxRxToken;
    type TxToken = RTLxTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(64);
        caps.medium = Medium::Ethernet;
        caps
    }

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        // One acquisition, as in `NetScheme::recv` and for the same two reasons.
        // The second one bites on a single CPU as well: an empty answer used to
        // become an `RxToken` all the same, so a frame the GMAC marked bad turned
        // into a zero-length "frame" tapped to every AF_PACKET listener and
        // handed to smoltcp as an Ethernet header.
        let mut driver = self.0.lock();
        if !driver.can_recv() {
            return None;
        }
        //这里每次只接收一个网络包
        let (vec_recv, _rxcount) = driver.geth_recv(1);
        if vec_recv.is_empty() {
            return None;
        }
        drop(driver);
        Some((RTLxRxToken(vec_recv), RTLxTxToken(self.clone())))
    }

    fn transmit(&mut self) -> Option<Self::TxToken> {
        if self.0.lock().can_send() {
            Some(RTLxTxToken(self.clone()))
        } else {
            None
        }
    }
}

impl phy::RxToken for RTLxRxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        // Dispatch to global packet tapping (AF_PACKET sockets)
        super::net_defer_packet(&self.0);
        f(&mut self.0)
    }
}

impl phy::TxToken for RTLxTxToken {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        // `len` comes from the IP layer, which this smoltcp never clamps to
        // the device MTU (and it has no fragmentation), so indexing a fixed
        // stack array with it panics the kernel on any oversized datagram —
        // a single UDP `sendto` is enough. Refuse instead.
        let mut buffer = [0u8; TX_SCRATCH_LEN];
        if len > TX_SCRATCH_LEN {
            return Err(smoltcp::Error::Exhausted);
        }
        let result = f(&mut buffer[..len]);
        if result.is_ok() {
            // Re-check ownership under the SAME lock as the send: transmit()
            // gated on can_send() under a different lock acquisition, so on SMP
            // another CPU can consume the slot in between.
            let mut driver = (self.0).0.lock();
            // Reclaim before asking. `can_send` reads the free-slot count off
            // `tx_clean`, which only `tx_complete` advances -- see `handle_irq`
            // for how a ring that fills up stays full.
            driver.tx_complete();
            // And when the slot really is gone, say so. This used to drop the
            // frame and hand smoltcp back the closure's `Ok`, which tells the
            // stack the frame went out. The e1000e paid for exactly that on real
            // hardware and its own comment records the bill: the ingress path
            // emits the ACK or window update for a received segment through the
            // TxToken paired with that RxToken, having ALREADY advanced
            // `remote_last_ack`/`remote_last_win`, so a dropped ACK is never
            // re-emitted and the peer waits on a window it believes is closed.
            // `Exhausted` is the answer smoltcp is built for: `socket_egress`
            // breaks out of the burst and keeps the segment for the next poll.
            if !driver.can_send() {
                return Err(smoltcp::Error::Exhausted);
            }
            if driver.geth_send(&buffer[..len]).is_err() {
                error!("rtl8211f: the GMAC refused a {}-byte frame", len);
                return Err(smoltcp::Error::Exhausted);
            }
        }
        result
    }
}

pub fn rtlx_init<F: Fn(usize, usize) -> Option<usize>>(
    irq: usize,
    mapper: F,
) -> DeviceResult<RTLxInterface> {
    // `mapper` answers `None` when the window could not be mapped, and both
    // answers used to go on the floor. `Option` is `#[must_use]` and would have
    // said so, but `#![allow(unused)]` covers this whole subtree. The driver
    // reaches into both blocks while coming up a few lines below -- the pin
    // controller to put the pins in RGMII mode, the system-configuration block
    // for the PHY's clock -- so an unmapped window was a fault in early boot
    // rather than a board that boots without networking. Which is what the
    // caller is ready for: it drops the node and carries on.
    mapper(rtl8211f::PINCTRL_GPIO_BASE as usize, PAGE_SIZE * 2).ok_or_else(|| {
        warn!("rtlx: could not map the pin controller");
        DeviceError::NoResources
    })?;
    mapper(rtl8211f::SYS_CFG_BASE as usize, PAGE_SIZE * 2).ok_or_else(|| {
        warn!("rtlx: could not map the system configuration block");
        DeviceError::NoResources
    })?;

    let mut rtl8211f = RTL8211F::<ProviderImpl>::new(&[0u8; 6]);
    let mac = rtl8211f.get_umac();
    //启动前请为D1插上网线
    warn!("Please plug in the Ethernet cable");

    // Propagate instead of `unwrap()`. Both of these fail on conditions that
    // are not the kernel's fault and must not take the whole boot down: a GMAC
    // that does not come out of soft reset (clock/power not up yet), and a
    // master/slave resolution failure, which is decided by the link partner's
    // PHY. Returning an error leaves the board booting without networking.
    rtl8211f.open().map_err(|e| {
        warn!("rtlx: GMAC open failed: {}", e);
        DeviceError::IoError
    })?;
    rtl8211f.set_rx_mode();
    if let Err(e) = rtl8211f.adjust_link() {
        // Not fatal: the link watchdog / a later cable insertion can still
        // bring it up, and a NIC with no carrier is better than no boot.
        warn!("rtlx: link negotiation failed: {}", e);
    }

    let net_driver = RTLxDriver(Arc::new(Mutex::new(rtl8211f)));

    let ethernet_addr = EthernetAddress::from_bytes(&mac);

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
        IpCidr::new(IpAddress::v4(192, 168, 0, 123), 24),
        IpCidr::Ipv6(Ipv6Cidr::new(link_local, 64)),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
    ];
    let default_gateway = Ipv4Address::new(192, 168, 0, 1);
    // Per-NIC storage — avoid the shared `static mut` aliasing bug fixed in e1000e.
    let routes_storage: &'static mut [Option<(IpCidr, Route)>] =
        Box::leak(vec![None; 4].into_boxed_slice());
    let mut routes = Routes::new(routes_storage);
    routes.add_default_ipv4_route(default_gateway).unwrap();
    let neighbor_cache = NeighborCache::new(BTreeMap::new());
    let iface = InterfaceBuilder::new(net_driver.clone())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(neighbor_cache)
        .ip_addrs(ip_addrs)
        .routes(routes)
        .finalize();

    info!("rtl8211f interface up with addr 192.168.0.123/24");
    info!("rtl8211f interface up with route 192.168.0.1/24");
    let rtl8211f_iface = RTLxInterface {
        iface: Arc::new(Mutex::new(iface)),
        driver: net_driver,
        routes: Arc::new(Mutex::new(vec![RouteInfo {
            dst: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            gateway: Some(IpAddress::Ipv4(default_gateway)),
        }])),
        name: String::from("rtl8211f"),
        irq,
    };

    Ok(rtl8211f_iface)
}

//TODO: Global SocketSet
// lazy_static::lazy_static! {
//     pub static ref SOCKETS: Mutex<SocketSet<'static>> =
//         Mutex::new(SocketSet::new(vec![]));
// }

/// The glue between the D1's GMAC and smoltcp, 465 lines with no tests.
///
/// It is the half of the RTL8211F story the kernel actually runs: the driver
/// beside it moves bytes, and this decides when to ask it to. Both were behind a
/// bare `target_arch = "riscv64"`, so no `cargo test` ever compiled either, and
/// what was in here was the interrupt handler that acks the NIC's interrupts and
/// then does nothing with half of them.
#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::cell::RefCell;
    use rtl8211f::fake::{self, Phy};
    use smoltcp::phy::{RxToken, TxToken};

    /// The interface the D1 boots with, over the faked GMAC.
    ///
    /// A `fake::with_phy` guard must already be alive: `rtlx_init` brings the MAC
    /// and the PHY up, and with no fake installed the first register read
    /// dereferences the literal address `0x0450_0048`.
    fn interface(irq: usize) -> RTLxInterface {
        rtlx_init(irq, |_paddr, _size| Some(0)).expect("a healthy GMAC must come up")
    }

    fn at(micros: i64) -> Instant {
        Instant::from_micros(micros)
    }

    /// Leave `payload` in the next receive slot, the way the GMAC leaves a good
    /// frame.
    fn stage(iface: &RTLxInterface, payload: &[u8]) {
        fake::stage_rx_frame(&mut iface.driver.0.lock(), 0, payload);
    }

    /// Leave a frame in the next receive slot with the error summary set, which
    /// is what a CRC error, a runt or an MII error looks like to the driver.
    fn stage_bad(iface: &RTLxInterface, payload: &[u8]) {
        fake::stage_bad_rx(&mut iface.driver.0.lock(), 0, payload);
    }

    /// Every transmit slot handed to the hardware and none completed: the state a
    /// transmit DMA that has stopped leaves the ring in.
    fn stall_tx_ring(iface: &RTLxInterface) {
        fake::fill_tx_ring(&mut iface.driver.0.lock());
    }

    /// The same full ring, but with every descriptor finished -- so the only
    /// thing between it and a free slot is somebody calling `tx_complete`.
    fn full_but_finished_tx_ring(iface: &RTLxInterface) {
        let mut nic = iface.driver.0.lock();
        fake::fill_tx_ring(&mut nic);
        fake::complete_whole_tx_ring(&mut nic);
    }

    // ------------------------------------------------------------ bringing up

    /// The two windows are mapped before anything touches them, and in that
    /// order. The driver reads the pin controller to put the pins in RGMII mode
    /// and the system-configuration block for the PHY's clock.
    #[test]
    fn both_windows_the_gmac_needs_are_mapped_and_nothing_else_is() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let asked: RefCell<Vec<(usize, usize)>> = RefCell::new(Vec::new());
        let _iface = rtlx_init(7, |paddr, size| {
            asked.borrow_mut().push((paddr, size));
            Some(0)
        })
        .expect("a healthy GMAC must come up");

        assert_eq!(
            asked.into_inner(),
            alloc::vec![
                (rtl8211f::PINCTRL_GPIO_BASE as usize, PAGE_SIZE * 2),
                (rtl8211f::SYS_CFG_BASE as usize, PAGE_SIZE * 2),
            ],
            "both windows, two pages each"
        );
    }

    /// The answer `mapper` gives when it could not map the window used to go on
    /// the floor -- `Option` is `#[must_use]`, but `#![allow(unused)]` covers
    /// this subtree. The caller is ready for the error: it drops the node and
    /// boots on without networking, which is better than faulting on an
    /// unmapped register.
    #[test]
    fn a_window_that_could_not_be_mapped_stops_the_bring_up() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        assert_eq!(
            rtlx_init(7, |_paddr, _size| None).err(),
            Some(DeviceError::NoResources),
            "a window that could not be mapped must come back as an error"
        );
    }

    /// The second window is only asked for once the first one answered.
    #[test]
    fn the_second_window_is_not_asked_for_after_the_first_one_failed() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let calls = RefCell::new(0usize);
        let _ = rtlx_init(7, |_paddr, _size| {
            *calls.borrow_mut() += 1;
            None
        });
        assert_eq!(calls.into_inner(), 1);
    }

    #[test]
    fn the_interface_comes_up_with_the_address_the_gmac_reports() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let mac = iface.driver.0.lock().get_umac();
        assert_eq!(iface.get_mac(), EthernetAddress::from_bytes(&mac));
        assert_eq!(iface.get_ifname(), "rtl8211f");
    }

    /// The link-local address is a hand-rolled modified EUI-64: the U/L bit of
    /// the first octet flipped, `ff:fe` in the middle, `fe80::/64` in front. Get
    /// any of the three wrong and every IPv6 neighbour discovery on the board
    /// answers for an address that is not its own.
    #[test]
    fn the_link_local_address_is_the_eui64_of_that_address() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let mac = iface.driver.0.lock().get_umac();

        let v6 = iface
            .get_ip_address()
            .into_iter()
            .find_map(|cidr| match cidr {
                IpCidr::Ipv6(v6) => Some(v6),
                _ => None,
            })
            .expect("a link-local address must be configured");

        let expected = Ipv6Address::new(
            0xfe80,
            0,
            0,
            0,
            ((mac[0] ^ 2) as u16) << 8 | mac[1] as u16,
            (mac[2] as u16) << 8 | 0xff,
            0xfe00 | mac[3] as u16,
            (mac[4] as u16) << 8 | mac[5] as u16,
        );
        assert_eq!(v6.address(), expected);
        assert_eq!(v6.prefix_len(), 64);
        assert_ne!(
            v6.address().as_bytes()[8],
            mac[0],
            "the U/L bit has to be flipped, or this is not a modified EUI-64"
        );
    }

    /// A second default route replaces the first instead of piling up beside it.
    #[test]
    fn a_new_default_route_replaces_the_one_the_board_booted_with() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let gateway = Ipv4Address::new(10, 0, 0, 1);
        iface
            .add_route(
                IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
                Some(IpAddress::Ipv4(gateway)),
            )
            .unwrap();

        let defaults: Vec<_> = iface
            .get_routes()
            .into_iter()
            .filter(|r| matches!(r.dst, IpCidr::Ipv4(_)) && r.dst.prefix_len() == 0)
            .collect();
        assert_eq!(defaults.len(), 1, "one default route, not two");
        assert_eq!(defaults[0].gateway, Some(IpAddress::Ipv4(gateway)));
    }

    /// Each NIC gets its own route storage. They used to share one `static mut`,
    /// which is the aliasing bug the e1000e already paid for.
    #[test]
    fn two_nics_do_not_share_their_routing_table() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let first = interface(7);
        let second = interface(8);

        let gateway = Ipv4Address::new(10, 0, 0, 1);
        first
            .add_route(
                IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
                Some(IpAddress::Ipv4(gateway)),
            )
            .unwrap();

        assert!(
            !second
                .get_routes()
                .iter()
                .any(|r| r.gateway == Some(IpAddress::Ipv4(gateway))),
            "a route added to one NIC must not appear on the other"
        );
    }

    // -------------------------------------------------------- the interrupt

    /// Reading the status register acks the interrupt, so a handler that reads it
    /// for a line that is not its own eats somebody else's interrupt.
    #[test]
    fn an_interrupt_from_another_device_is_left_for_whoever_owns_it() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        fake::raise_rx_interrupt();

        iface.handle_irq(9);

        assert!(
            fake::rx_interrupt_pending(),
            "the interrupt must still be pending for its real owner"
        );
    }

    /// The one that matters. `tx_complete` is the only thing that advances
    /// `tx_clean`, `can_send` counts the free slots from it, and apart from the
    /// interrupt handler `tx_complete` is only called from inside `geth_send` --
    /// which `can_send` gates. So a ring that fills up while the DMA is not
    /// completing answers "full" for ever: this handler is the only way out, and
    /// it was not reclaiming anything.
    #[test]
    fn a_receive_interrupt_reclaims_the_descriptors_the_dma_finished_with() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        full_but_finished_tx_ring(&iface);
        assert!(
            !iface.driver.0.lock().can_send(),
            "a ring with no free slot cannot send, whatever the descriptors say"
        );

        fake::raise_rx_interrupt();
        iface.handle_irq(7);

        assert!(
            iface.driver.0.lock().can_send(),
            "the handler must reclaim the finished descriptors: nothing else \
             will, and without it this NIC never transmits again"
        );
    }

    /// TX_UNF_INT is one of exactly two interrupts `int_enable` arms, and it used
    /// to fall off the end of the `if` without a log line, already acked. That is
    /// what makes the stall above permanent: the DMA stops, nothing completes,
    /// the ring fills, and the doorbell that would restart the DMA is only rung
    /// from inside `geth_send`, which the full ring keeps unreachable.
    #[test]
    fn a_transmit_underflow_restarts_the_dma_instead_of_being_swallowed() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        full_but_finished_tx_ring(&iface);
        fake::clear_tx_doorbell();

        fake::raise_tx_underflow();
        iface.handle_irq(7);

        assert!(
            fake::tx_doorbell_rung(),
            "the poll-demand doorbell is what picks a stopped transmit DMA back up"
        );
        assert!(
            iface.driver.0.lock().can_send(),
            "and the slots the DMA did finish with have to come back"
        );
    }

    /// TX_STOP_INT says the same thing about the same DMA.
    #[test]
    fn a_stopped_transmit_dma_is_restarted_too() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        full_but_finished_tx_ring(&iface);
        fake::clear_tx_doorbell();

        fake::raise_tx_stopped();
        iface.handle_irq(7);

        assert!(fake::tx_doorbell_rung());
    }

    /// An interrupt with nothing set in the status register is somebody else's or
    /// nothing at all, and must not be treated as a transmit error.
    #[test]
    fn an_interrupt_with_nothing_pending_changes_nothing() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        full_but_finished_tx_ring(&iface);
        fake::clear_tx_doorbell();

        iface.handle_irq(7);

        assert!(!fake::tx_doorbell_rung(), "nothing to restart");
        assert_eq!(
            fake::tx_cursors(&iface.driver.0.lock()).0,
            0,
            "and nothing to reclaim"
        );
    }

    /// The handler turns interrupts off across the smoltcp poll. Leaving them off
    /// is a NIC that never raises another one.
    #[test]
    fn the_interrupts_the_poll_turned_off_are_armed_again_afterwards() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        fake::raise_rx_interrupt();

        iface.handle_irq(7);

        assert_ne!(
            fake::interrupts_armed(),
            0,
            "the handler must arm the GMAC again before it returns"
        );
    }

    // ----------------------------------------------------- what smoltcp sees

    /// `geth_recv` spends its budget dropping a frame the GMAC marked bad and
    /// hands back an empty `Vec`. That used to become an `RxToken` all the same:
    /// a zero-length "frame" offered to every AF_PACKET listener and handed to
    /// smoltcp as an Ethernet header. A bad frame is routine on a marginal
    /// cable, which is the state this board boots in.
    #[test]
    fn a_frame_the_gmac_marked_bad_is_no_frame_at_all() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        stage_bad(&iface, &[0xa5u8; 64]);

        let mut driver = iface.driver.clone();
        assert!(
            driver.receive().is_none(),
            "an empty answer from `geth_recv` is no frame, not a zero-length one"
        );
    }

    #[test]
    fn the_frame_in_the_ring_is_the_frame_the_token_hands_over() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let payload = [0x5au8; 100];
        stage(&iface, &payload);

        let mut driver = iface.driver.clone();
        let (rx, _tx) = driver.receive().expect("a good frame must come back");
        rx.consume(at(0), |buf| {
            assert_eq!(buf, &payload[..], "the payload came back changed");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_empty_ring_offers_nothing_to_receive() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let mut driver = iface.driver.clone();
        assert!(driver.receive().is_none());
    }

    /// Two numbers that have to move together: smoltcp's MTU counts the Ethernet
    /// header, the scheme's does not.
    #[test]
    fn the_advertised_mtu_is_the_ethernet_header_plus_what_the_scheme_reports() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let caps = iface.driver.capabilities();
        assert_eq!(caps.max_transmission_unit, iface.get_mtu() + 14);
        assert_eq!(caps.medium, Medium::Ethernet);
    }

    // ------------------------------------------------------------ transmitting

    /// The length comes from the IP layer, which this smoltcp never clamps to the
    /// device MTU and cannot fragment, so indexing the fixed scratch with it
    /// panicked the kernel from a single oversized `sendto`.
    #[test]
    fn a_frame_too_big_for_the_scratch_is_refused_before_the_closure_writes_it() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let token = RTLxTxToken(iface.driver.clone());

        let mut closure_ran = false;
        let result: Result<()> = token.consume(at(0), TX_SCRATCH_LEN + 1, |_buf| {
            closure_ran = true;
            Ok(())
        });

        assert_eq!(result, Err(smoltcp::Error::Exhausted));
        assert!(
            !closure_ran,
            "the closure must never see a buffer shorter than the length it asked for"
        );
    }

    /// The lie. Dropping the frame and handing smoltcp back the closure's `Ok`
    /// tells the stack it went out. The e1000e paid for that on real hardware:
    /// the ACK for a received segment is emitted through the TxToken paired with
    /// that RxToken, after smoltcp has already advanced its window state, so a
    /// dropped ACK is never re-emitted and the transfer stalls.
    #[test]
    fn a_frame_the_ring_could_not_take_is_reported_as_exhausted_not_as_sent() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        stall_tx_ring(&iface);
        let token = RTLxTxToken(iface.driver.clone());

        let result: Result<()> = token.consume(at(0), 64, |buf| {
            buf.fill(0xa5);
            Ok(())
        });

        assert_eq!(
            result,
            Err(smoltcp::Error::Exhausted),
            "`Exhausted` is what smoltcp is built for: it keeps the segment and \
             retries on the next poll"
        );
    }

    /// And the ring is only full once the descriptors the DMA finished with have
    /// been reclaimed, which is the same `tx_complete` the interrupt handler owes.
    #[test]
    fn the_token_reclaims_what_the_dma_finished_before_it_calls_the_ring_full() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        full_but_finished_tx_ring(&iface);
        let token = RTLxTxToken(iface.driver.clone());

        let result: Result<()> = token.consume(at(0), 64, |buf| {
            buf.fill(0xa5);
            Ok(())
        });

        assert!(
            result.is_ok(),
            "every slot had been finished with; reclaiming them is what makes one free"
        );
    }

    #[test]
    fn the_bytes_the_closure_wrote_are_the_bytes_that_reach_the_ring() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let frame: Vec<u8> = (0..100u32).map(|i| (i % 251) as u8).collect();
        let token = RTLxTxToken(iface.driver.clone());

        let written = frame.clone();
        token
            .consume(at(0), written.len(), |buf| {
                buf.copy_from_slice(&written);
                Ok(())
            })
            .unwrap();

        assert_eq!(
            fake::tx_buffer(&iface.driver.0.lock(), 0, frame.len()),
            frame,
            "the frame the closure filled in is the frame handed to the GMAC"
        );
    }

    /// A closure that fails posts nothing: the scratch it half-filled is not a
    /// frame.
    #[test]
    fn a_closure_that_fails_posts_nothing() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let token = RTLxTxToken(iface.driver.clone());

        let result: Result<()> = token.consume(at(0), 64, |_buf| Err(smoltcp::Error::Illegal));

        assert_eq!(result, Err(smoltcp::Error::Illegal));
        assert_eq!(
            fake::tx_cursors(&iface.driver.0.lock()),
            (0, 0),
            "the ring cursors must not have moved"
        );
    }

    // ----------------------------------------------------- what userspace sees

    /// `Ok(0)` from a read is end-of-file, and a frame the GMAC threw away is not
    /// the end of anything.
    #[test]
    fn a_frame_the_gmac_marked_bad_is_not_an_end_of_file() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        stage_bad(&iface, &[0xa5u8; 64]);

        let mut buf = [0u8; 1514];
        assert_eq!(iface.recv(&mut buf), Err(DeviceError::NotReady));
    }

    #[test]
    fn recv_takes_the_bytes_that_arrived_and_leaves_the_rest_alone() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        stage(&iface, &[0x77u8; 200]);

        let mut buf = [0u8; 1514];
        assert_eq!(iface.recv(&mut buf).unwrap(), 200);
        assert!(buf[..200].iter().all(|b| *b == 0x77));
        assert!(
            buf[200..].iter().all(|b| *b == 0),
            "nothing past the frame may be written"
        );
    }

    /// `copy_from_slice` panics unless the lengths match, so a frame larger than
    /// the caller's buffer has to be clipped to it.
    #[test]
    fn recv_into_a_buffer_smaller_than_the_frame_takes_what_fits() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        stage(&iface, &[0x33u8; 300]);

        let mut buf = [0u8; 64];
        assert_eq!(iface.recv(&mut buf).unwrap(), 64);
        assert!(buf.iter().all(|b| *b == 0x33));
    }

    #[test]
    fn recv_on_an_empty_ring_is_not_ready() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let mut buf = [0u8; 1514];
        assert_eq!(iface.recv(&mut buf), Err(DeviceError::NotReady));
    }

    #[test]
    fn send_reports_the_whole_frame_as_sent() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let frame = [0x11u8; 128];
        assert_eq!(iface.send(&frame).unwrap(), frame.len());
        assert_eq!(
            fake::tx_buffer(&iface.driver.0.lock(), 0, frame.len()),
            &frame[..]
        );
    }

    /// An empty write is shorter than an Ethernet header, and the descriptor fill
    /// loop in `geth_send` is `while len != 0`: it would ring the doorbell over a
    /// descriptor holding the previous frame's length and never advance the
    /// cursor, desynchronising the ring for good. The error has to come back out.
    #[test]
    fn send_of_an_empty_buffer_is_refused_and_the_ring_does_not_move() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);

        assert_eq!(iface.send(&[]), Err(DeviceError::IoError));
        assert_eq!(fake::tx_cursors(&iface.driver.0.lock()), (0, 0));
    }

    #[test]
    fn send_on_a_stalled_ring_is_not_ready_rather_than_a_lie() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        stall_tx_ring(&iface);

        assert_eq!(iface.send(&[0x11u8; 64]), Err(DeviceError::NotReady));
    }

    // --------------------------------------------------------- the addresses

    /// What smoltcp's own routing table has for `dst`, which is a different
    /// thing from the list `get_routes` reports: the scheme keeps its own copy
    /// for `/proc/net/route`, and the stack routes by this one.
    fn gateway_in_the_stack(iface: &RTLxInterface, dst: IpCidr) -> Option<IpAddress> {
        let mut found = None;
        iface
            .iface
            .lock()
            .routes_mut()
            .update(|routes| found = routes.get(&dst).map(|route| route.via_router));
        found
    }

    /// The address lands in the first IPv4 slot and nowhere else. The walk has
    /// to tell the slot it is setting from the ones it is blanking, and if it
    /// cannot, every spare slot ends up holding the same address -- so the NIC
    /// answers ARP for it three times over and `ip addr` shows it three times.
    #[test]
    fn setting_the_address_fills_one_slot_and_leaves_the_link_local_alone() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let cidr = Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 5), 24);
        iface.set_ipv4_address(cidr).unwrap();

        let addrs = iface.get_ip_address();
        assert_eq!(
            addrs.iter().filter(|a| **a == IpCidr::Ipv4(cidr)).count(),
            1,
            "the address is configured more than once: {:?}",
            addrs
        );
        assert!(
            addrs.iter().any(|a| matches!(a, IpCidr::Ipv6(_))),
            "the link-local address was blanked with the spare slots: {:?}",
            addrs
        );
    }

    /// Two addresses added one after the other take two slots. The free-slot
    /// test is a pair of forms -- an unspecified address with a zero prefix, or
    /// the `240.0.0.0/32` placeholder -- and a slot is free if it matches
    /// EITHER. Ask for both and no slot is ever free, so every address goes to
    /// the fallback and overwrites the one added before it.
    #[test]
    fn two_addresses_added_in_a_row_take_two_slots() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let first = IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24);
        let second = IpCidr::new(IpAddress::v4(10, 0, 1, 1), 24);
        iface.add_ip_address(first).unwrap();
        iface.add_ip_address(second).unwrap();

        let addrs = iface.get_ip_address();
        assert!(
            addrs.contains(&first),
            "the second address overwrote the first: {:?}",
            addrs
        );
        assert!(addrs.contains(&second), "{:?}", addrs);
    }

    /// A zero prefix on its own does not make a slot free. `0.0.0.0/0` is the
    /// empty slot the interface boots with; `10.0.0.1/0` is an address somebody
    /// configured, and the next `add_ip_address` must not write over it.
    #[test]
    fn an_address_with_a_zero_prefix_is_still_an_address() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let wide = IpCidr::new(IpAddress::v4(10, 0, 0, 1), 0);
        iface.add_ip_address(wide).unwrap();
        iface
            .add_ip_address(IpCidr::new(IpAddress::v4(10, 0, 1, 1), 24))
            .unwrap();

        let addrs = iface.get_ip_address();
        assert!(
            addrs.contains(&wide),
            "an address with a zero prefix was taken for an empty slot: {:?}",
            addrs
        );
    }

    #[test]
    fn adding_the_same_address_twice_does_not_duplicate_it() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let cidr = IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24);
        iface.add_ip_address(cidr).unwrap();
        iface.add_ip_address(cidr).unwrap();

        let addrs = iface.get_ip_address();
        assert_eq!(
            addrs.iter().filter(|a| **a == cidr).count(),
            1,
            "{:?}",
            addrs
        );
    }

    /// With every slot taken, a new address replaces the last one -- the most
    /// recently added -- and not the first, which is the address the board is
    /// actually reachable at.
    #[test]
    fn an_address_added_with_every_slot_taken_replaces_the_newest() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let primary = IpCidr::new(IpAddress::v4(192, 168, 0, 123), 24);
        iface
            .add_ip_address(IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24))
            .unwrap();
        iface
            .add_ip_address(IpCidr::new(IpAddress::v4(10, 0, 1, 1), 24))
            .unwrap();
        // Four slots, and all four are taken now.
        let third = IpCidr::new(IpAddress::v4(10, 0, 2, 1), 24);
        iface.add_ip_address(third).unwrap();

        let addrs = iface.get_ip_address();
        assert!(
            addrs.contains(&primary),
            "the address the board boots at was overwritten: {:?}",
            addrs
        );
        assert!(addrs.contains(&third), "{:?}", addrs);
    }

    /// A removed address leaves no route behind. The slot is blanked to
    /// `0.0.0.0/0`, which `get_routes` skips because its prefix is zero; blank
    /// it to anything carrying a prefix and the interface claims a direct route
    /// to a network nobody configured.
    #[test]
    fn removing_an_address_takes_its_direct_route_with_it() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        iface
            .remove_ip_address(IpCidr::new(IpAddress::v4(192, 168, 0, 123), 24))
            .unwrap();

        let direct: Vec<_> = iface
            .get_routes()
            .into_iter()
            .filter(|r| r.gateway.is_none())
            .map(|r| r.dst)
            .collect();
        assert!(
            !direct.iter().any(|dst| matches!(dst, IpCidr::Ipv4(_))),
            "the removed address still has a direct route: {:?}",
            direct
        );
    }

    /// A direct route is to the NETWORK, not to the address: the board answers
    /// at `192.168.0.123/24`, and what it can reach without a gateway is
    /// `192.168.0.0/24`. Report the address and every other host on the wire is
    /// off-link.
    #[test]
    fn the_direct_route_of_an_address_is_its_network() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let networks: Vec<_> = iface
            .get_routes()
            .into_iter()
            .filter(|r| r.gateway.is_none() && matches!(r.dst, IpCidr::Ipv4(_)))
            .map(|r| r.dst)
            .collect();
        assert_eq!(
            networks,
            alloc::vec![IpCidr::new(IpAddress::v4(192, 168, 0, 0), 24)],
            "the two spare 0.0.0.0/0 slots are not networks, and the boot \
             address is reachable as its network"
        );
    }

    // ------------------------------------------------------------ the routes

    /// A route to a named network is not a default route, and adding one must
    /// leave the default route where it is. Only one default route can be in
    /// force, so a new one replaces the old -- but that purge ran for every
    /// gatewayed route, so the static route in a DHCP lease took `0.0.0.0/0`
    /// out of the table the kernel reports while smoltcp went on routing
    /// through it.
    #[test]
    fn a_route_to_a_network_does_not_delete_the_default_route() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        iface
            .add_route(
                IpCidr::new(IpAddress::v4(10, 0, 0, 0), 8),
                Some(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2))),
            )
            .unwrap();

        let defaults: Vec<_> = iface
            .get_routes()
            .into_iter()
            .filter(|r| matches!(r.dst, IpCidr::Ipv4(_)) && r.dst.prefix_len() == 0)
            .collect();
        assert_eq!(
            defaults.len(),
            1,
            "a route to 10.0.0.0/8 took the default route with it"
        );
        assert_eq!(
            defaults[0].gateway,
            Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 0, 1))),
            "and it is still the gateway the board booted with"
        );
    }

    /// Deleting a route takes that route out and leaves the rest alone.
    #[test]
    fn a_deleted_route_is_the_only_one_that_goes() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let on_link = IpCidr::new(IpAddress::v4(10, 0, 0, 0), 8);
        iface.add_route(on_link, None).unwrap();
        iface.del_route(on_link, None).unwrap();

        let dsts: Vec<_> = iface.get_routes().into_iter().map(|r| r.dst).collect();
        assert!(
            !dsts.contains(&on_link),
            "the deleted route is still reported: {:?}",
            dsts
        );
        assert!(
            dsts.contains(&IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0)),
            "deleting one route took the default route with it: {:?}",
            dsts
        );
    }

    /// Deleting the default route takes it out of smoltcp's own table too, not
    /// only out of the list the scheme reports. Leave it in and the kernel says
    /// there is no gateway while the stack keeps sending through the old one.
    #[test]
    fn deleting_the_default_route_takes_it_out_of_the_stacks_own_table() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let default = IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0);
        assert_eq!(
            gateway_in_the_stack(&iface, default),
            Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 0, 1))),
            "the board boots with a default route in the stack"
        );

        iface.del_route(default, None).unwrap();
        assert_eq!(
            gateway_in_the_stack(&iface, default),
            None,
            "the stack still routes through the gateway the kernel deleted"
        );
    }

    // ------------------------------------------------------- the poll and tap

    /// The frames the AF_PACKET tap was handed, in the order it got them.
    static TAPPED: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

    fn tap(frame: &[u8]) {
        TAPPED.lock().push(frame.to_vec());
    }

    /// A received frame reaches an AF_PACKET tap, and no reader is woken for a
    /// frame no socket wanted.
    ///
    /// `RTLxRxToken::consume` queues every frame while smoltcp holds `SOCKETS`,
    /// and nothing on this driver ever emptied that queue: the DHCP client on
    /// the D1 saw no frames at all, and blocked readers only woke on the
    /// fallback park timer.
    #[test]
    fn a_received_frame_reaches_an_af_packet_tap() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        crate::net::tests::alone_with_the_statics(|| {
            TAPPED.lock().clear();
            crate::utils::host_hooks::reset();
            let iface = interface(7);
            let payload = [0xdeu8, 0xad, 0xbe, 0xef];
            stage(&iface, &payload);
            crate::net::set_packet_callback(tap);

            iface.poll().expect("a malformed frame is not a poll error");

            assert_eq!(
                TAPPED.lock().clone(),
                alloc::vec![payload.to_vec()],
                "the frame never left the deferred queue"
            );
            assert_eq!(
                crate::utils::host_hooks::WAKE_CALLS.load(core::sync::atomic::Ordering::SeqCst),
                0,
                "nothing a socket is waiting for happened, so nobody is woken"
            );
        })
    }

    // ------------------------------------------------------------ transmitting

    /// A ring with no free slot offers no transmit token. Hand one out anyway
    /// and smoltcp writes a frame into a closure whose `geth_send` then refuses
    /// it, which is a frame the stack believes it sent.
    #[test]
    fn a_stalled_ring_offers_no_transmit_token() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        stall_tx_ring(&iface);
        let mut driver = iface.driver.clone();
        assert!(
            driver.transmit().is_none(),
            "a full ring handed out a token"
        );
    }

    /// A new default route reaches smoltcp's own table, not only the list the
    /// kernel reports. Nothing tied the two together, so they could disagree:
    /// `/proc/net/route` naming one gateway and every packet going to another.
    #[test]
    fn a_new_default_route_is_the_one_the_stack_routes_through() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let default = IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0);
        let gateway = Ipv4Address::new(10, 0, 0, 1);
        iface
            .add_route(default, Some(IpAddress::Ipv4(gateway)))
            .unwrap();

        assert_eq!(
            gateway_in_the_stack(&iface, default),
            Some(IpAddress::Ipv4(gateway)),
            "the stack still routes through the gateway the board booted with"
        );
    }

    /// The same for IPv6: a route to a named prefix is not a default route and
    /// must leave the default route alone.
    #[test]
    fn an_ipv6_route_to_a_prefix_does_not_delete_the_ipv6_default_route() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let gateway = IpAddress::Ipv6(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
        iface
            .add_route(
                IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 0)),
                Some(gateway),
            )
            .unwrap();
        iface
            .add_route(
                IpCidr::Ipv6(Ipv6Cidr::new(
                    Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0),
                    32,
                )),
                Some(gateway),
            )
            .unwrap();

        let defaults: Vec<_> = iface
            .get_routes()
            .into_iter()
            .filter(|r| matches!(r.dst, IpCidr::Ipv6(_)) && r.dst.prefix_len() == 0)
            .collect();
        assert_eq!(
            defaults.len(),
            1,
            "a route to 2001:db8::/32 took the default route with it"
        );
        assert_eq!(defaults[0].gateway, Some(gateway));
    }

    /// And the same for a frame the interrupt handler took: `handle_irq` polls
    /// with `SOCKETS` held too, so it has its own call to the flush. That is
    /// the one the board's DHCP client depended on, since on riscv64 nothing
    /// else ever polls before the lease has to be asked for.
    #[test]
    fn a_frame_the_interrupt_handler_took_reaches_the_tap_too() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        crate::net::tests::alone_with_the_statics(|| {
            TAPPED.lock().clear();
            let iface = interface(7);
            let payload = [0x11u8, 0x22, 0x33, 0x44];
            stage(&iface, &payload);
            crate::net::set_packet_callback(tap);

            fake::raise_rx_interrupt();
            iface.handle_irq(7);

            assert_eq!(
                TAPPED.lock().clone(),
                alloc::vec![payload.to_vec()],
                "the frame never left the deferred queue"
            );
        })
    }

    /// A prefix of one is still a network and gets its direct route. Only a
    /// prefix of ZERO means an address with no network of its own, which is
    /// what the two spare slots the interface boots with are.
    #[test]
    fn the_widest_prefix_that_is_still_a_network_gets_a_direct_route() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        iface
            .add_ip_address(IpCidr::new(IpAddress::v4(128, 0, 0, 1), 1))
            .unwrap();

        let networks: Vec<_> = iface
            .get_routes()
            .into_iter()
            .filter(|r| r.gateway.is_none())
            .map(|r| r.dst)
            .collect();
        assert!(
            networks.contains(&IpCidr::new(IpAddress::v4(128, 0, 0, 0), 1)),
            "a /1 address has a network like any other: {:?}",
            networks
        );
    }

    /// A frame of exactly `TX_SCRATCH_LEN` bytes fits in the scratch, so the
    /// closure gets to write it. Refuse at the limit rather than above it and
    /// the largest frame the token has room for never reaches the closure at
    /// all -- and the caller is told `Exhausted`, which means "try again
    /// later" for a frame that will never fit.
    #[test]
    fn a_frame_that_exactly_fills_the_scratch_reaches_the_closure() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let mut driver = iface.driver.clone();
        let token = driver.transmit().expect("an empty ring has room");

        let mut asked = 0usize;
        let out = token.consume(at(0), TX_SCRATCH_LEN, |buf| {
            asked = buf.len();
            buf.fill(0x5a);
            Ok(())
        });

        assert_eq!(asked, TX_SCRATCH_LEN, "the closure was never called");
        // And the GMAC takes it: its own per-buffer limit is wider than the
        // scratch, so the scratch is the only thing deciding here.
        assert!(out.is_ok(), "{:?}", out.err());
    }

    /// The frame posted is as long as the frame the closure wrote. The
    /// descriptor carries the length in its low eleven bits and the scratch is
    /// `TX_SCRATCH_LEN` bytes whatever the frame is, so posting the whole
    /// scratch puts a 60-byte ARP on the wire as a 1536-byte frame -- which
    /// every peer on it drops, while the ring and the counters say it was sent.
    #[test]
    fn the_frame_posted_is_as_long_as_the_frame_the_closure_wrote() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let mut driver = iface.driver.clone();
        let token = driver.transmit().expect("an empty ring has room");
        token
            .consume(at(0), 60, |buf| {
                buf.fill(0xa5);
                Ok(())
            })
            .unwrap();

        assert_eq!(
            fake::tx_frame_len(&iface.driver.0.lock(), 0),
            60,
            "the descriptor announces a length the closure never wrote"
        );
    }
}
