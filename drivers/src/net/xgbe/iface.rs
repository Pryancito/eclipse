//! La interfaz: lo que engancha los anillos a smoltcp.
//!
//! Igual de no validado que todo lo demas de este driver: aqui no hay
//! XGMAC y QEMU no lo emula. Solo existe cuando la escotilla
//! `xgbe.enable` esta abierta.

use super::*;
use crate::net::ip_cfg;
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::wire::*;
use crate::{Device, DeviceError, DeviceResult};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;

/// The largest frame this interface puts on the wire. The standard one: the
/// bring-up configures an MTU of 1500 and nothing here asks for jumbo.
const MAX_FRAME: usize = 1514;
/// Can a frame of `len` bytes go out?
///
/// Refused rather than clipped, for the reason `pcnet32` spells out: a
/// truncated frame goes out corrupt while the caller is told it all went.
#[inline]
pub const fn tx_frame_postable(len: usize) -> bool {
    len > 0 && len <= MAX_FRAME
}

/// The registers and the rings, together, behind one lock.
pub struct XgbeHw {
    acc: Bar0,
    rings: XgbeRings,
    channel: u32,
    snps_ver: u8,
}

impl XgbeHw {
    /// Take ownership of an already-brought-up device.
    pub fn new(acc: Bar0, rings: XgbeRings, snps_ver: u8) -> Self {
        Self {
            acc,
            rings,
            channel: 0,
            snps_ver,
        }
    }

    /// One frame, or nothing.
    pub fn receive(&mut self) -> Option<Vec<u8>> {
        self.rings.recv(&self.acc)
    }

    /// Room for another frame?
    pub fn can_send(&mut self) -> bool {
        self.rings.reclaim_tx();
        self.rings.can_send()
    }

    /// Put a frame on the wire. `false` means it did not go.
    pub fn send(&mut self, data: &[u8]) -> bool {
        self.rings.reclaim_tx();
        self.rings.send(&self.acc, data).is_ok()
    }

    /// Acknowledge whatever the channel was reporting, and say what it was.
    ///
    /// Returns `None` when the channel had nothing to say, which happens on a
    /// shared INTx line: another device's interrupt brings the handler here
    /// too, and acknowledging a status of zero would be harmless but claiming
    /// it would not be.
    pub fn ack_irq(&mut self) -> Option<DmaStatus> {
        let sr = ack_channel_interrupts(&self.acc, self.channel);
        let status = decode_dma_status(sr);
        if dma_status_is_quiet(&status) {
            None
        } else {
            Some(status)
        }
    }

    /// Stop the channel interrupting while a bottom half runs.
    pub fn mask_irqs(&mut self) {
        disable_channel_interrupts(&self.acc, self.channel);
    }

    /// Let it interrupt again.
    pub fn unmask_irqs(&mut self) {
        enable_channel_interrupts(&self.acc, self.channel, self.snps_ver, true, true);
    }
}

/// A handle on the hardware that smoltcp can hold.
#[derive(Clone)]
pub struct XgbeDriver {
    pub hw: Arc<Mutex<XgbeHw>>,
    pub stats: Arc<Mutex<NetStats>>,
}

/// The interface the rest of the kernel sees.
#[derive(Clone)]
pub struct XgbeInterface {
    mac: EthernetAddress,
    driver: XgbeDriver,
    name: String,
    irq: usize,
    poll_pending: Arc<AtomicBool>,
    /// Who the card's frames go to while it is lent to the stack that
    /// runs as a process ([`crate::net::lent`]); empty, nobody has them.
    lent: crate::net::lent::Lent,
    /// Counters, shared with the tokens.
    pub stats: Arc<Mutex<NetStats>>,
    /// Routes this interface owns.
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    /// Addresses this interface owns.
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl XgbeInterface {
    /// Drain smoltcp without touching the interrupt mask; the IRQ path owns
    /// the mask through its guard.
    fn poll_inner(&self) -> DeviceResult {
        // A card lent to the stack that runs as a process: its frames go
        // to the borrower as they are. One that is not lent has no stack
        // above it.
        if !self.lent.hand_over(|| self.driver.hw.lock().receive()) {
            crate::net::lent::tap_only(&self.stats, || self.driver.hw.lock().receive());
        }
        Ok(())
    }
}

impl Scheme for XgbeInterface {
    fn name(&self) -> &str {
        "xgbe"
    }

    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            return;
        }
        let pending = self.driver.hw.lock().ack_irq();
        let Some(status) = pending else {
            // Not ours. The mask is only lifted if no bottom half owns it.
            if !self.poll_pending.load(Ordering::SeqCst) {
                self.driver.hw.lock().unmask_irqs();
            }
            return;
        };
        if status.fatal_bus_error {
            // Nothing short of a reset recovers, and a reset here would pull
            // the rings out from under smoltcp. Say so once per interrupt
            // rather than pretending the frame counters still mean anything.
            warn!("[xgbe] fatal bus error reported by the DMA channel");
        }
        if status.rx_buffer_unavailable {
            self.stats.lock().rx_errors += 1;
        }

        if self.poll_pending.load(Ordering::SeqCst) {
            return;
        }
        // A lent card has no bottom half: a deferred job runs when the CPU
        // idles or a kernel socket waits, and with the card lent nobody
        // waits on one. The card stays masked and the borrower is woken to
        // look at it itself (`NetScheme::poll`, which unmasks it).
        if self.lent.is_on() {
            self.driver.hw.lock().mask_irqs();
            if !self.lent.interrupted() {
                // Given back meanwhile: whoever has it now looks.
                self.driver.hw.lock().unmask_irqs();
            }
            return;
        }

        self.poll_pending.store(true, Ordering::SeqCst);
        self.driver.hw.lock().mask_irqs();

        // The guard is built outside the closure and moved in, so the mask is
        // lifted whether the job runs or is dropped unexecuted: the deferred
        // queue evicts under pressure, and clearing the flag in the body left
        // an evicted bottom half with the NIC masked forever.
        struct PollPendingGuard {
            pending: Arc<AtomicBool>,
            iface: XgbeInterface,
        }
        impl Drop for PollPendingGuard {
            fn drop(&mut self) {
                self.pending.store(false, Ordering::SeqCst);
                self.iface.driver.hw.lock().unmask_irqs();
            }
        }
        let guard = PollPendingGuard {
            pending: self.poll_pending.clone(),
            iface: self.clone(),
        };
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job_front(move || {
            let guard = guard;
            let _ = me.poll_inner();
            drop(guard);
        });
    }
}

impl NetScheme for XgbeInterface {
    fn lend(&self, taker: Option<Arc<dyn crate::net::lent::FrameTaker>>) -> DeviceResult {
        self.lent.set(taker)
    }

    fn can_lend(&self) -> bool {
        true
    }

    // Never run lent on its machine, and it has no interrupt registered:
    // the borrower looks at it now and then.
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

    fn poll(&self) -> DeviceResult {
        let res = self.poll_inner();
        self.driver.hw.lock().unmask_irqs();
        res
    }

    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        match self.driver.hw.lock().receive() {
            Some(pkt) => {
                let n = pkt.len().min(buf.len());
                buf[..n].copy_from_slice(&pkt[..n]);
                Ok(n)
            }
            None => Err(DeviceError::NotReady),
        }
    }

    fn send(&self, data: &[u8]) -> DeviceResult<usize> {
        if !tx_frame_postable(data.len()) {
            return Err(DeviceError::InvalidParam);
        }
        if self.driver.hw.lock().send(data) {
            Ok(data.len())
        } else {
            Err(DeviceError::NotReady)
        }
    }

    fn can_recv(&self) -> bool {
        true
    }

    fn can_send(&self) -> bool {
        self.driver.hw.lock().can_send()
    }

    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        ip_cfg::set_ipv4_address(&mut self.ip_addrs.lock(), cidr)
    }
}

/// Build the interface around a device the bring-up has already configured.
pub fn new_interface(
    name: String,
    irq: usize,
    acc: Bar0,
    rings: XgbeRings,
    snps_ver: u8,
    mac: [u8; 6],
) -> DeviceResult<XgbeInterface> {
    let stats = Arc::new(Mutex::new(NetStats::default()));
    let driver = XgbeDriver {
        hw: Arc::new(Mutex::new(XgbeHw::new(acc, rings, snps_ver))),
        stats: stats.clone(),
    };
    let ethernet_addr = EthernetAddress::from_bytes(&mac);
    let ip_addrs = vec![IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0)];
    crate::klog_info!("xgbe interface {} discovered", name);
    Ok(XgbeInterface {
        mac: ethernet_addr,
        driver,
        name,
        irq,
        poll_pending: Arc::new(AtomicBool::new(false)),
        lent: crate::net::lent::Lent::default(),
        stats,
        routes: Arc::new(Mutex::new(vec![])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    })
}

/// Wrap it as a device for the registry.
pub fn as_device(iface: XgbeInterface) -> Device {
    Device::Net(Arc::new(iface))
}
