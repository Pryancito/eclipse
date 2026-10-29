//! La interfaz: lo que engancha los anillos a smoltcp.
//!
//! Igual de no validado que todo lo demas de este driver: aqui no hay
//! XGMAC y QEMU no lo emula. Solo existe cuando la escotilla
//! `xgbe.enable` esta abierta.

use super::*;
use crate::net::{get_sockets, ip_cfg, timer_now_as_micros};
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::{Device, DeviceError, DeviceResult};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;
use smoltcp::iface::*;
use smoltcp::phy::{self, DeviceCapabilities};
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result;

/// The largest frame this interface puts on the wire. The standard one: the
/// bring-up configures an MTU of 1500 and nothing here asks for jumbo.
const MAX_FRAME: usize = 1514;
/// Where a transmit frame is staged when it fits on the stack.
const TX_SCRATCH_LEN: usize = 1536;

/// Can a frame of `len` bytes go out?
///
/// Refused rather than clipped, for the reason `pcnet32` spells out: a
/// truncated frame goes out corrupt while the caller is told it all went.
#[inline]
pub const fn tx_frame_postable(len: usize) -> bool {
    len > 0 && len <= MAX_FRAME
}

fn tx_stage<'a>(
    len: usize,
    scratch: &'a mut [u8; TX_SCRATCH_LEN],
    heap: &'a mut Vec<u8>,
) -> &'a mut [u8] {
    if len <= TX_SCRATCH_LEN {
        &mut scratch[..len]
    } else {
        heap.resize(len, 0);
        &mut heap[..]
    }
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

/// A frame that has arrived.
pub struct XgbeRxToken {
    data: Vec<u8>,
    stats: Arc<Mutex<NetStats>>,
}

/// Permission to put one frame out.
pub struct XgbeTxToken {
    driver: XgbeDriver,
    stats: Arc<Mutex<NetStats>>,
}

impl phy::Device<'_> for XgbeDriver {
    type RxToken = XgbeRxToken;
    type TxToken = XgbeTxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        self.hw.lock().receive().map(|pkt| {
            (
                XgbeRxToken {
                    data: pkt,
                    stats: self.stats.clone(),
                },
                XgbeTxToken {
                    driver: self.clone(),
                    stats: self.stats.clone(),
                },
            )
        })
    }

    fn transmit(&mut self) -> Option<Self::TxToken> {
        if self.hw.lock().can_send() {
            Some(XgbeTxToken {
                driver: self.clone(),
                stats: self.stats.clone(),
            })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = MAX_FRAME;
        // Left unset deliberately: smoltcp clamps the TCP window to
        // `burst * MSS` through a u16, so a burst of 64 wraps and caps the
        // connection at a few Mbps. Same trap as `e1000.rs` and `pcnet32.rs`.
        caps.max_burst_size = None;
        caps
    }
}

impl phy::RxToken for XgbeRxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        {
            let mut stats = self.stats.lock();
            stats.rx_packets += 1;
            stats.rx_bytes += self.data.len() as u64;
        }
        crate::net::net_defer_packet(&self.data);
        f(&mut self.data)
    }
}

impl phy::TxToken for XgbeTxToken {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut scratch = [0u8; TX_SCRATCH_LEN];
        let mut heap = Vec::new();
        let buf: &mut [u8] = tx_stage(len, &mut scratch, &mut heap);
        let result = f(buf)?;

        let sent = self.driver.hw.lock().send(buf);
        if !sent {
            // Never report a dropped frame as sent: smoltcp emits the ACK for
            // a received segment through the TxToken paired with its RxToken
            // and has already advanced `remote_last_ack`, so an `Ok` here
            // loses that ACK for good.
            self.stats.lock().tx_errors += 1;
            return Err(smoltcp::Error::Exhausted);
        }
        let mut stats = self.stats.lock();
        stats.tx_packets += 1;
        stats.tx_bytes += len as u64;
        Ok(result)
    }
}

/// The interface the rest of the kernel sees.
#[derive(Clone)]
pub struct XgbeInterface {
    iface: Arc<Mutex<Interface<'static, XgbeDriver>>>,
    driver: XgbeDriver,
    name: String,
    irq: usize,
    poll_pending: Arc<AtomicBool>,
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
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        let sockets = get_sockets();
        // `try_lock`, not a blocking acquire: this runs from a deferred job
        // and a socket syscall drains that queue while holding the socket
        // set, so a blocking acquire deadlocks on this IRQ-off kernel. The
        // same shape `e1000.rs` and `pcnet32.rs` both settled on.
        let res = match (sockets.try_lock(), self.iface.try_lock()) {
            (Some(mut sockets), Some(mut iface)) => match iface.poll(&mut sockets, timestamp) {
                Ok(_) => Ok(()),
                Err(err) => {
                    warn!("[xgbe] poll got err {}", err);
                    Err(DeviceError::IoError)
                }
            },
            _ => Ok(()),
        };
        crate::net::net_flush_deferred_packets();
        res
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

    fn seed_neighbor(
        &self,
        protocol: smoltcp::wire::IpAddress,
        hardware: EthernetAddress,
    ) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        ip_cfg::seed_neighbor(&mut self.iface.lock(), protocol, hardware, timestamp)
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
        ip_cfg::set_ipv4_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
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
    // Per-NIC route storage: a shared `static mut` would alias if two of
    // these probed and corrupt both route tables.
    let routes_storage: &'static mut [Option<(IpCidr, Route)>] =
        Box::leak(vec![None; 4].into_boxed_slice());
    let iface = InterfaceBuilder::new(driver.clone())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(NeighborCache::new(BTreeMap::new()))
        .ip_addrs(ip_addrs.clone())
        .routes(Routes::new(routes_storage))
        .finalize();

    crate::klog_info!("xgbe interface {} discovered", name);
    Ok(XgbeInterface {
        iface: Arc::new(Mutex::new(iface)),
        driver,
        name,
        irq,
        poll_pending: Arc::new(AtomicBool::new(false)),
        stats,
        routes: Arc::new(Mutex::new(vec![])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    })
}

/// Wrap it as a device for the registry.
pub fn as_device(iface: XgbeInterface) -> Device {
    Device::Net(Arc::new(iface))
}
