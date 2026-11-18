use crate::sync::Mutex;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::wire::*;

use super::realtek::rtl8211f::{self, RTL8211F};
use super::{ip_cfg, timer_now_as_micros, ProviderImpl, PAGE_SIZE};

use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::{DeviceError, DeviceResult};

#[derive(Clone)]
pub struct RTLxDriver(Arc<Mutex<RTL8211F<ProviderImpl>>>);

#[derive(Clone)]
pub struct RTLxInterface {
    mac: EthernetAddress,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
    pub stats: Arc<Mutex<NetStats>>,
    pub driver: RTLxDriver,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub name: String,
    pub irq: usize,
    /// Who the card's frames go to while it is lent to the stack that
    /// runs as a process ([`super::lent`]); empty, nobody has them.
    pub lent: super::lent::Lent,
}

impl RTLxInterface {
    /// The next good frame on the ring. A frame the GMAC marked bad comes
    /// back empty and is no frame: the next one is looked for, so that it
    /// does not sit on the ring until another arrives.
    fn take_frame(&self) -> Option<Vec<u8>> {
        let mut driver = self.driver.0.lock();
        while driver.can_recv() {
            let (frame, _count) = driver.geth_recv(1);
            if !frame.is_empty() {
                return Some(frame);
            }
        }
        None
    }

    /// The look at a lent card: what its interrupt would have done, with
    /// the frames going to the borrower and not to the kernel's stack.
    /// `false` when the card is not lent.
    fn look_lent(&self) -> bool {
        if !self.lent.is_on() {
            return false;
        }
        // The slots of what was sent, before the borrower sends more.
        self.driver.0.lock().tx_complete();
        let handed = self.lent.hand_over(|| self.take_frame());
        self.driver.0.lock().int_enable();
        handed
    }
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

        // A card lent to a stack that is not the kernel's: the card is
        // silenced and the borrower is woken to look at it itself
        // (`NetScheme::poll`, which arms it again), in its own time and
        // not in this interrupt.
        if status == rtl8211f::TxDmaIrqStatus::HandleTxRx as i32 && self.lent.is_on() {
            self.driver.0.lock().int_disable();
            if !self.lent.interrupted() {
                // Given back meanwhile: whoever has it now looks.
                self.driver.0.lock().int_enable();
            }
            return;
        }

        if status == rtl8211f::TxDmaIrqStatus::HandleTxRx as i32 {
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
            // No stack above a card that is not lent: the ring is
            // emptied and the frames are shown to who listens for them.
            super::lent::tap_only(&self.stats, || self.take_frame());
            self.driver.0.lock().int_enable();
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
    fn lend(&self, taker: Option<Arc<dyn super::lent::FrameTaker>>) -> DeviceResult {
        self.lent.set(taker)
    }

    fn can_lend(&self) -> bool {
        true
    }

    // Never run lent on its board.
    fn lends_unasked(&self) -> bool {
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
        self.ip_addrs.lock().clone()
    }

    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        ip_cfg::set_ipv4_address(&mut self.ip_addrs.lock(), cidr)
    }

    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        ip_cfg::add_ip_address(&mut self.ip_addrs.lock(), cidr)
    }

    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        ip_cfg::remove_ip_address(&mut self.ip_addrs.lock(), cidr)
    }

    fn add_route(&self, cidr: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        ip_cfg::add_route(&mut self.routes.lock(), cidr, gateway)
    }

    fn del_route(&self, cidr: IpCidr, _gateway: Option<IpAddress>) -> DeviceResult {
        ip_cfg::del_route(&mut self.routes.lock(), cidr)
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        ip_cfg::reported_routes(&self.routes.lock(), &self.ip_addrs.lock())
    }

    fn poll(&self) -> DeviceResult {
        if !self.look_lent() {
            // Not lent: no stack above it.
            super::lent::tap_only(&self.stats, || self.take_frame());
        }
        Ok(())
    }

    fn get_stats(&self) -> NetStats {
        self.stats.lock().clone()
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

    info!("rtl8211f interface up with addr 192.168.0.123/24");
    info!("rtl8211f interface up with route 192.168.0.1/24");
    let rtl8211f_iface = RTLxInterface {
        mac: ethernet_addr,
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
        stats: Arc::new(Mutex::new(NetStats::default())),
        driver: net_driver,
        routes: Arc::new(Mutex::new(vec![RouteInfo {
            dst: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            gateway: Some(IpAddress::Ipv4(default_gateway)),
        }])),
        name: String::from("rtl8211f"),
        irq,
        lent: super::lent::Lent::default(),
    };

    Ok(rtl8211f_iface)
}

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

    /// The interface the D1 boots with, over the faked GMAC.
    ///
    /// A `fake::with_phy` guard must already be alive: `rtlx_init` brings the MAC
    /// and the PHY up, and with no fake installed the first register read
    /// dereferences the literal address `0x0450_0048`.
    fn interface(irq: usize) -> RTLxInterface {
        rtlx_init(irq, |_paddr, _size| Some(0)).expect("a healthy GMAC must come up")
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

    // ------------------------------------------------------------ transmitting

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

    /// With every slot taken, a new address makes the list longer and costs
    /// nothing: not the address the board boots at, not the IPv6 link-local it
    /// needs for neighbour discovery, and not the two added before it.
    #[test]
    fn an_address_added_with_every_slot_taken_grows_the_list() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let primary = IpCidr::new(IpAddress::v4(192, 168, 0, 123), 24);
        let link_local = iface
            .get_ip_address()
            .into_iter()
            .find(|a| matches!(a, IpCidr::Ipv6(_)))
            .expect("the GMAC comes up with a link-local address");
        let first = IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24);
        let second = IpCidr::new(IpAddress::v4(10, 0, 1, 1), 24);
        iface.add_ip_address(first).unwrap();
        iface.add_ip_address(second).unwrap();
        // Four slots, and all four are taken now: the two the init configures
        // plus the two blanks these filled.
        assert_eq!(iface.get_ip_address().len(), 4);
        let third = IpCidr::new(IpAddress::v4(10, 0, 2, 1), 24);

        iface.add_ip_address(third).unwrap();

        let addrs = iface.get_ip_address();
        assert_eq!(addrs.len(), 5, "the list did not grow: {:?}", addrs);
        for want in [primary, link_local, first, second, third] {
            assert!(addrs.contains(&want), "{} went missing: {:?}", want, addrs);
        }
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

    // ----------------------------------------------- the bring-up it survives
    //
    // `rtlx_init` is written to come back with an error rather than take the
    // boot down, and the driver's own tests prove each failure happens --
    // `a_gmac_whose_soft_reset_never_completes_fails_the_bring_up_not_the_boot`
    // says in so many words that "`rtlx::init` propagates this error precisely
    // so the board still boots without networking". Nothing proved that it
    // does.

    /// The second window is checked the same way as the first. Two guards that
    /// read alike and only one of them exercised: swap the messages, or drop
    /// the `?`, and nothing said so.
    #[test]
    fn the_second_window_that_could_not_be_mapped_stops_the_bring_up_too() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        assert_eq!(
            rtlx_init(7, |paddr, _size| {
                if paddr == rtl8211f::PINCTRL_GPIO_BASE as usize {
                    Some(0)
                } else {
                    None
                }
            })
            .err(),
            Some(DeviceError::NoResources),
            "the system-configuration window is as necessary as the pin controller"
        );
    }

    /// A GMAC that never leaves soft reset is a clock or a power rail that is
    /// not up yet. `open` says so, and the bring-up has to hand that on: an
    /// `unwrap()` in here panicked the boot of a board whose networking was
    /// the only thing wrong with it.
    #[test]
    fn a_gmac_that_will_not_reset_stops_the_bring_up_and_not_the_boot() {
        let _dev = fake::with_phy(Phy::gigabit_partner()).with_mac_stuck_in_reset();
        assert_eq!(
            rtlx_init(7, |_paddr, _size| Some(0)).err(),
            Some(DeviceError::IoError),
            "a MAC that never leaves soft reset must come back as an error"
        );
    }

    /// Negotiation failing is NOT one of those, and the difference is the
    /// whole point: the speed and duplex are decided by the link partner, so a
    /// failure there is a cable or a switch, not the board. The interface comes
    /// up anyway and the link watchdog or the next cable insertion can still
    /// bring the carrier up.
    #[test]
    fn a_link_that_will_not_negotiate_still_brings_the_interface_up() {
        let mut phy = Phy::gigabit_partner();
        // Master/slave resolution failed: the two ends could not agree who
        // drives the clock, which `adjust_link` refuses to guess at.
        phy.regs[crate::net::realtek::mii::MII_STAT1000 as usize] =
            crate::net::realtek::mii::LPA_1000MSFAIL;
        let _dev = fake::with_phy(phy);

        let iface = rtlx_init(7, |_paddr, _size| Some(0))
            .expect("a link that will not negotiate is not a reason to drop the NIC");

        assert_eq!(iface.ifname(), "rtl8211f");
    }

    // ------------------------------------------------------------- the names

    /// The kernel asks for three of these and used to be told none of them.
    /// `Scheme::name` is the driver's name and `ifname` the interface's, which
    /// on this board happen to read alike and are not the same thing;
    /// `is_loopback` decides whether the stack bothers with an ARP cache for
    /// it.
    #[test]
    fn the_interface_answers_to_the_names_the_kernel_asks_for() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        assert_eq!(Scheme::name(&iface), "rtl8211f");
        assert_eq!(iface.ifname(), "rtl8211f");
        assert_eq!(iface.ifname(), iface.get_ifname());
        assert!(
            !iface.is_loopback(),
            "a NIC that says it is the loopback gets no neighbour resolution"
        );
    }

    // ------------------------------- storage the board does not boot with
    //
    // `rtlx_init` builds four address slots, four route slots and an OWNED
    // address list. Three paths cannot be reached through it: a list with no
    // IPv4 slot, a list that cannot grow, and a routing table with no room for
    // a default route. Each is a shape the same code has to handle, since
    // nothing in `add_ip_address` or `add_route` knows which caller built the
    // interface.

    /// Removing an address the interface does not have takes nothing away.
    /// `ip addr del` on a stale address is routine, and a run off the end of
    /// the list that blanked a slot anyway would take the address the board
    /// booted with.
    #[test]
    fn removing_an_address_that_is_not_there_changes_nothing() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let before = iface.get_ip_address();

        iface
            .remove_ip_address(IpCidr::new(IpAddress::v4(10, 9, 9, 9), 24))
            .expect("removing an address that is not there is not an error");

        assert_eq!(iface.get_ip_address(), before);
    }

    // --------------------------------------------------------------- routing

    /// Only one default route of a family can be in force, so a new one
    /// replaces the one before it -- and only that one. The purge used to run
    /// for every gatewayed route, so the static route in a DHCP lease took
    /// `0.0.0.0/0` out of the table the kernel reports while smoltcp went on
    /// routing through it.
    ///
    /// The purge looks at the family too: an IPv6 default route sitting in the
    /// table is not what a new IPv4 one replaces.
    #[test]
    fn a_new_default_route_replaces_only_the_one_of_its_own_family() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let v6_default = IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 0));
        let v6_gateway = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
        iface
            .add_route(v6_default, Some(IpAddress::Ipv6(v6_gateway)))
            .unwrap();

        let v4_gateway = IpAddress::v4(192, 168, 0, 9);
        iface
            .add_route(IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0), Some(v4_gateway))
            .unwrap();

        let routes = iface.get_routes();
        let defaults: alloc::vec::Vec<_> = routes
            .iter()
            .filter(|r| r.gateway.is_some() && r.dst.prefix_len() == 0)
            .collect();
        assert_eq!(
            defaults.len(),
            2,
            "the new IPv4 default took the IPv6 one with it: {:?}",
            routes
        );
        assert!(
            defaults
                .iter()
                .any(|r| r.gateway == Some(IpAddress::Ipv6(v6_gateway))),
            "{:?}",
            routes
        );
        assert!(
            defaults.iter().any(|r| r.gateway == Some(v4_gateway)),
            "{:?}",
            routes
        );
    }

    /// And the IPv6 purge is the IPv4 one over again, with the families
    /// swapped: a second IPv6 default route replaces the first and leaves the
    /// IPv4 one where it is.
    #[test]
    fn a_new_ipv6_default_route_replaces_only_the_ipv6_one() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let v6_default = IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 0));
        let first = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
        let second = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 3);
        iface
            .add_route(v6_default, Some(IpAddress::Ipv6(first)))
            .unwrap();

        iface
            .add_route(v6_default, Some(IpAddress::Ipv6(second)))
            .unwrap();

        let routes = iface.get_routes();
        let gateways: alloc::vec::Vec<_> = routes.iter().filter_map(|r| r.gateway).collect();
        assert!(
            gateways.contains(&IpAddress::Ipv6(second)),
            "the new IPv6 gateway is not in the table: {:?}",
            routes
        );
        assert!(
            !gateways.contains(&IpAddress::Ipv6(first)),
            "two IPv6 default routes at once: {:?}",
            routes
        );
        // The route the board booted with is IPv4 and has nothing to do with
        // either of them.
        assert!(
            gateways.contains(&IpAddress::v4(192, 168, 0, 1)),
            "the IPv6 purge took the IPv4 default route: {:?}",
            routes
        );
    }

    /// A gateway that is neither an IPv4 nor an IPv6 address is
    /// `IpAddress::Unspecified`, the placeholder for storage whose address is
    /// not assigned yet, and a route through it is dropped without a word --
    /// it does not even reach the table the kernel reports. Worth holding:
    /// this is the answer, and the arm below it in `del_route` looks like the
    /// same thing and is not.
    #[test]
    fn a_route_through_an_unspecified_gateway_is_dropped() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let iface = interface(7);
        let before = iface.get_routes().len();

        iface
            .add_route(
                IpCidr::new(IpAddress::v4(10, 0, 0, 0), 8),
                Some(IpAddress::Unspecified),
            )
            .expect("an unspecified gateway is not an error");

        assert_eq!(
            iface.get_routes().len(),
            before,
            "a route nobody can use was recorded"
        );
    }

    // ------------------------------------------- the poll and what it wakes

    // The error arm of `iface.poll()` is left alone, in all three places it
    // appears here (`poll`, `handle_irq` and the loopback's `poll`). smoltcp
    // only returns `Err` out of `socket_egress`, which iterates the socket set
    // and returns `Ok(false)` when it is empty; the process-wide `SOCKETS` this
    // crate polls is empty in the test binary, and a test that added a socket
    // to it would be adding one to every other test in the binary as well.
}
