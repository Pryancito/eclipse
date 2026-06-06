//! Intel e1000 / e1000e NIC driver.
//!
//! Port of Little Kernel [`dev/net/e1000`](https://github.com/littlekernel/lk/tree/wip/minip/dev/net/e1000)
//! (MIT) into Eclipse's `NetScheme` + smoltcp integration.  Covers 82574L (QEMU), i210, and
//! PCH-integrated i219 (`8086:15b8`).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{compiler_fence, fence, AtomicBool, Ordering};

use lock::Mutex;
use pci::{BAR, Location, PCIDevice};
use smoltcp::iface::*;
use smoltcp::phy::{self, DeviceCapabilities};
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result as SmolResult;

use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
use crate::bus::pci_drivers::PciDriver;
use crate::builder::IoMapper;
use crate::net::get_sockets;
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::utils::dma::DmaRegion;
use crate::utils::dma_sync::{dma_sync_region, dma_sync_rx_desc_span, DmaSyncDir};
use crate::{Device, DeviceError, DeviceResult};

use super::timer_now_as_micros;

const RING_LEN: usize = 64;
const BUF_SIZE: usize = 2048;

// ---------------------------------------------------------------------------
// MMIO register indices (byte offset / 4), from LK e1000_hw.h
// ---------------------------------------------------------------------------
const E1000_CTRL: usize = 0x0000 / 4;
const E1000_STATUS: usize = 0x0008 / 4;
const E1000_EERD: usize = 0x0014 / 4;
const E1000_CTRL_EXT: usize = 0x0018 / 4;
const E1000_ICR: usize = 0x00C0 / 4;
const E1000_ITR: usize = 0x00C4 / 4;
const E1000_IMS: usize = 0x00D0 / 4;
const E1000_IMC: usize = 0x00D8 / 4;
const E1000_IAM: usize = 0x00E0 / 4;
const E1000_EITR0: usize = 0x1680 / 4;
const E1000_RCTL: usize = 0x0100 / 4;
const E1000_FCRTL: usize = 0x2160 / 4;
const E1000_FCRTH: usize = 0x2168 / 4;
const E1000_TCTL: usize = 0x0400 / 4;
const E1000_TIPG: usize = 0x0410 / 4;
const E1000_RDBAL: usize = 0x2800 / 4;
const E1000_RDBAH: usize = 0x2804 / 4;
const E1000_RDLEN: usize = 0x2808 / 4;
const E1000_RDH: usize = 0x2810 / 4;
const E1000_RDT: usize = 0x2818 / 4;
const E1000_RDTR: usize = 0x2820 / 4;
const E1000_RADV: usize = 0x282C / 4;
const E1000_RSRPD: usize = 0x2C00 / 4;
const E1000_SRRCTL: usize = 0x0280C / 4;
const E1000_RXDCTL: usize = 0x02828 / 4;
const E1000_RXCSUM: usize = 0x5000 / 4;
const E1000_RFCTL: usize = 0x5008 / 4;
const E1000_TDBAL: usize = 0x3800 / 4;
const E1000_TDBAH: usize = 0x3804 / 4;
const E1000_TDLEN: usize = 0x3808 / 4;
const E1000_TDH: usize = 0x3810 / 4;
const E1000_TDT: usize = 0x3818 / 4;
const E1000_RAL0: usize = 0x5400 / 4;
const E1000_RAH0: usize = 0x5404 / 4;

const CTRL_SLU: u32 = 1 << 6;
const CTRL_ASDE: u32 = 1 << 5;
const CTRL_FD: u32 = 1 << 0;
const STATUS_LU: u32 = 1 << 1;

const ICR_TXDW: u32 = 1 << 0;
const ICR_TXQE: u32 = 1 << 1;
const ICR_LSC: u32 = 1 << 2;
const ICR_RXO: u32 = 1 << 6;
const ICR_RXTO: u32 = 1 << 7;

const RXD_EXT_DD: u32 = 0x01;
const RXD_EXT_EOP: u32 = 0x02;

const IMS_RX: u32 = ICR_RXTO | ICR_RXO;
const IMS_TX: u32 = ICR_TXQE | ICR_TXDW;
const IMS_LSC: u32 = ICR_LSC;

const RCTL_EN: u32 = 1 << 1;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_LPE: u32 = 1 << 5;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;
const RCTL_SBP: u32 = 1 << 2;
const RCTL_MO_MASK: u32 = 0x3 << 12;
const RCTL_RX_SZ_MASK: u32 = 0x3 << 16;

const RFCTL_EXTEN: u32 = 1 << 15;
const RFCTL_NFSW_DIS: u32 = 1 << 6;
const RFCTL_NFSR_DIS: u32 = 1 << 7;

const RXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
const RXDCTL_DMA_BURST: u32 = 0x0100_0000 | (4 << 16) | (4 << 8) | 0x20;

const SRRCTL_BSIZE_2K: u32 = 2;
const SRRCTL_DROP_EN: u32 = 1 << 31;
const SRRCTL_DESCTYPE_MASK: u32 = 0x0E00_0000;

const RDTR_FPD: u32 = 1 << 31;
/// Linux `E1000_RX_BUFFER_WRITE` — keep one slot empty; post at most this many at boot.
const RX_BOOT_POST_MAX: usize = RING_LEN - 2;

// LK: EN | UPE | MPE | BAM | BSIZE=2048 (bits 16:17 = 0)
const RCTL_ENABLE: u32 = RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM;

// ---------------------------------------------------------------------------
// RX/TX descriptors (16 bytes). Driver only writes `addr`; HW write-back in `wb`.
// ---------------------------------------------------------------------------
#[repr(C, align(16))]
#[derive(Copy, Clone)]
struct RxDesc {
    addr: u64,
    wb: u64,
}
const _RX_DESC_SIZE_OK: () = assert!(size_of::<RxDesc>() == 16);

#[repr(C)]
#[derive(Copy, Clone)]
struct TxDesc {
    addr: u64,
    length: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

#[inline]
unsafe fn mmio_read(base: usize, reg: usize) -> u32 {
    core::ptr::read_volatile((base + reg * 4) as *const u32)
}

#[inline]
unsafe fn mmio_write(base: usize, reg: usize, val: u32) {
    core::ptr::write_volatile((base + reg * 4) as *mut u32, val);
}

#[inline]
unsafe fn mmio_write_flush(base: usize, reg: usize, val: u32) {
    mmio_write(base, reg, val);
    let _ = mmio_read(base, reg);
}

#[inline]
unsafe fn program_srrctl_rx_queue0(base: usize) {
    let mut v = mmio_read(base, E1000_SRRCTL);
    v &= !SRRCTL_DESCTYPE_MASK;
    v |= SRRCTL_BSIZE_2K | SRRCTL_DROP_EN;
    mmio_write(base, E1000_SRRCTL, v);
    let _ = mmio_read(base, E1000_SRRCTL);
}

#[inline]
unsafe fn desc_copy16(dst: *mut u8, src: *const u8) {
    let d = dst as *mut u64;
    let s = src as *const u64;
    core::ptr::write_volatile(d, core::ptr::read_volatile(s));
    core::ptr::write_volatile(d.add(1), core::ptr::read_volatile(s.add(1)));
}

fn is_e1000e_device(device_id: u16) -> bool {
    matches!(
        device_id,
        0x10d3 | 0x10f5 | 0x150c | 0x1533 | 0x1539 | 0x157b | 0x157c | 0x0d4d | 0x0d53
            | 0x0d55 | 0x0dc5..=0x0dc8 | 0x1502..=0x1503 | 0x153a..=0x153b | 0x155a | 0x1559
            | 0x15a0..=0x15a3 | 0x156f..=0x1570 | 0x15b7..=0x15be | 0x15d6..=0x15d8 | 0x15df..=0x15e2
            | 0x15e3 | 0x15f4..=0x15fc | 0x1a1c..=0x1a1f | 0x550a..=0x5511 | 0x57a0..=0x57a1
            | 0x57b3..=0x57ba | 0x0d4c..=0x0d4f
    )
}

fn matched_device(vendor_id: u16, device_id: u16) -> bool {
    vendor_id == 0x8086 && is_e1000e_device(device_id)
}

/// PCH-SPT (I219) and later: RXDCTL.QUEUE_ENABLE required for RX DMA.
fn is_pch_spt_or_later(device_id: u16) -> bool {
    matches!(
        device_id,
        0x156f..=0x1570
            | 0x15b7..=0x15be
            | 0x15d6..=0x15d8
            | 0x15e3
            | 0x0d4c..=0x0d4f
            | 0x15f4..=0x15fc
            | 0x1a1c..=0x1a1f
            | 0x0dc5..=0x0dc8
            | 0x550a..=0x5511
            | 0x57a0..=0x57a1
            | 0x57b3..=0x57ba
            | 0x15df..=0x15e2
            | 0x0d53
            | 0x0d55
            | 0x15f9
            | 0x15fa
    )
}

#[inline]
unsafe fn rctl_rx_bits(base: usize) -> u32 {
    let mut rctl = mmio_read(base, E1000_RCTL);
    rctl &= !(RCTL_MO_MASK | 0xC0);
    rctl |= RCTL_BAM | RCTL_UPE | RCTL_MPE | RCTL_SECRC;
    rctl &= !(RCTL_SBP | RCTL_RX_SZ_MASK | RCTL_EN);
    rctl |= RCTL_LPE;
    rctl
}

// ---------------------------------------------------------------------------
// Hardware backend
// ---------------------------------------------------------------------------
const RX_DESC_SIZE: usize = size_of::<RxDesc>();

pub struct E1000eHw {
    base: usize,
    pci_loc: Location,
    device_id: u16,
    is_e1000e: bool,
    mac: [u8; 6],
    /// PAT UC at alloc — skip clflush on descriptor/payload paths.
    dma_coherent: bool,
    /// I219/PCH write-back uses staterr+length in `RxDesc::wb` (RFCTL_EXTEN).
    use_extended_wb: bool,

    rx_ring: DmaRegion,
    rx_buf_pool: DmaRegion,
    /// Software clean index — never read [`E1000_RDH`] for receive progress.
    rx_last_head: usize,

    tx_ring: DmaRegion,
    tx_buf_pool: DmaRegion,
    tx_last_head: usize,
    tx_tail: usize,

    /// Multi-descriptor frame assembly (LK `rx_pending_pkt_`).
    rx_pending: Option<Vec<u8>>,

    stats: NetStats,
    rx_poll_budget: u8,
}

impl E1000eHw {
    fn udelay(us: u64) {
        let t0 = timer_now_as_micros();
        let mut spins = 0u64;
        while timer_now_as_micros().wrapping_sub(t0) < us {
            core::hint::spin_loop();
            spins = spins.wrapping_add(1);
            if spins >= 10_000_000 {
                break;
            }
        }
    }

    #[inline]
    fn rx_buf_vaddr(&self, i: usize) -> usize {
        self.rx_buf_pool.vaddr() + i * BUF_SIZE
    }

    #[inline]
    fn tx_buf_vaddr(&self, i: usize) -> usize {
        self.tx_buf_pool.vaddr() + i * BUF_SIZE
    }

    fn read_eeprom(&self, offset: u8) -> u16 {
        unsafe {
            let val = if self.is_e1000e {
                mmio_write(self.base, E1000_EERD, (offset as u32) << 2 | 1);
                let mut timeout = 10_000;
                let mut v = 0u32;
                while timeout > 0 {
                    v = mmio_read(self.base, E1000_EERD);
                    if v & (1 << 1) != 0 {
                        break;
                    }
                    timeout -= 1;
                }
                if timeout == 0 {
                    return 0xffff;
                }
                v
            } else {
                mmio_write(self.base, E1000_EERD, (offset as u32) << 8 | 1);
                let mut timeout = 10_000;
                let mut v = 0u32;
                while timeout > 0 {
                    v = mmio_read(self.base, E1000_EERD);
                    if v & (1 << 4) != 0 {
                        break;
                    }
                    timeout -= 1;
                }
                if timeout == 0 {
                    return 0xffff;
                }
                v
            };
            (val >> 16) as u16
        }
    }

    fn read_mac(&mut self) {
        let w0 = self.read_eeprom(0);
        let w1 = self.read_eeprom(1);
        let w2 = self.read_eeprom(2);
        self.mac[0] = (w0 & 0xff) as u8;
        self.mac[1] = (w0 >> 8) as u8;
        self.mac[2] = (w1 & 0xff) as u8;
        self.mac[3] = (w1 >> 8) as u8;
        self.mac[4] = (w2 & 0xff) as u8;
        self.mac[5] = (w2 >> 8) as u8;

        let invalid = self.mac.iter().all(|&b| b == 0xff);
        if invalid {
            unsafe {
                let ral = mmio_read(self.base, E1000_RAL0);
                let rah = mmio_read(self.base, E1000_RAH0);
                if rah & (1u32 << 31) != 0 {
                    self.mac[0] = (ral >> 0) as u8;
                    self.mac[1] = (ral >> 8) as u8;
                    self.mac[2] = (ral >> 16) as u8;
                    self.mac[3] = (ral >> 24) as u8;
                    self.mac[4] = (rah >> 0) as u8;
                    self.mac[5] = (rah >> 8) as u8;
                } else {
                    crate::klog_warn!("[e1000e] unable to read MAC address\n");
                }
            }
        }
    }

    fn program_mac_filter(&self) {
        unsafe {
            let mut ral = 0u32;
            let mut rah = 0u32;
            for i in 0..4 {
                ral |= (self.mac[i] as u32) << (i * 8);
            }
            for i in 0..2 {
                rah |= (self.mac[i + 4] as u32) << (i * 8);
            }
            mmio_write(self.base, E1000_RAL0, ral);
            mmio_write(self.base, E1000_RAH0, rah | (1 << 31));
        }
    }

    #[inline]
    unsafe fn clear_rx_desc_wb(desc_ptr: *mut RxDesc) {
        compiler_fence(Ordering::SeqCst);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*desc_ptr).wb), 0);
        fence(Ordering::Release);
    }

    #[inline]
    unsafe fn read_rx_wb_u64(desc_ptr: *const RxDesc) -> u64 {
        #[cfg(target_arch = "x86_64")]
        core::arch::x86_64::_mm_lfence();
        fence(Ordering::Acquire);
        core::ptr::read_volatile(core::ptr::addr_of!((*desc_ptr).wb))
    }

    #[inline]
    fn parse_rx_wb_ext(wb: u64) -> Option<(u32, usize)> {
        let staterr = wb as u32;
        if staterr & RXD_EXT_DD == 0 {
            return None;
        }
        let len = (wb >> 32) as u16 as usize;
        Some((staterr, len))
    }

    #[inline]
    fn parse_rx_wb_legacy(wb: u64) -> Option<(u32, usize)> {
        let len = (wb & 0xFFFF) as usize;
        let status = ((wb >> 32) & 0xFF) as u8;
        if status & 0x01 == 0 {
            return None;
        }
        Some((status as u32, len))
    }

    unsafe fn post_rx_desc(&mut self, slot: usize, ring_idx: usize) {
        let dst = self.rx_ring.as_ptr::<RxDesc>().add(ring_idx);
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*dst).addr),
            (self.rx_buf_pool.paddr() + slot * BUF_SIZE) as u64,
        );
        Self::clear_rx_desc_wb(dst);
    }

    unsafe fn reinit_rx_ring(&mut self) {
        for i in 0..RING_LEN {
            self.post_rx_desc(i, i);
        }
        self.flush_rx_ring_to_device(0, RING_LEN);
    }

    unsafe fn rx_doorbell(&self, last_idx: usize) {
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);
        mmio_write_flush(self.base, E1000_RDT, last_idx as u32);
    }

    unsafe fn kick_rx_writeback(&self) {
        mmio_write(self.base, E1000_RDTR, RDTR_FPD);
        let _ = mmio_read(self.base, E1000_RDTR);
        mmio_write(self.base, E1000_RDTR, 0);
        let _ = mmio_read(self.base, E1000_RDTR);
    }

    /// Post `count` RX buffers starting at ring index 0; ring RDT doorbell once at end.
    unsafe fn post_rx_boot_buffers(&mut self, count: usize) {
        let n = count.min(RX_BOOT_POST_MAX);
        for i in 0..n {
            self.post_rx_desc(i, i);
        }
        if n > 0 {
            self.flush_rx_ring_to_device(0, n);
            self.rx_doorbell(n - 1);
        }
    }

    fn flush_rx_ring_to_device(&self, start_idx: usize, count: usize) {
        dma_sync_rx_desc_span(
            &self.rx_ring,
            self.dma_coherent,
            start_idx,
            count,
            RX_DESC_SIZE,
            DmaSyncDir::ToDevice,
        );
    }

    fn sync_rx_desc_from_device(&self, idx: usize) {
        dma_sync_rx_desc_span(
            &self.rx_ring,
            self.dma_coherent,
            idx,
            1,
            RX_DESC_SIZE,
            DmaSyncDir::FromDevice,
        );
    }

    /// Return buffer `slot` to the same ring slot and doorbell RDT (Linux/OSDev I219 path).
    fn repost_rx_buffer(&mut self, slot: usize) {
        unsafe {
            self.post_rx_desc(slot, slot);
            self.flush_rx_ring_to_device(slot, 1);
            self.rx_doorbell(slot);
        }
    }

    fn fill_rx_ring(&mut self) {
        self.rx_last_head = 0;
        unsafe {
            self.post_rx_boot_buffers(RX_BOOT_POST_MAX);
        }
    }

    unsafe fn arm_rx_pch(&mut self) {
        self.rx_last_head = 0;

        mmio_write(self.base, E1000_RXCSUM, 0);
        let mut rfctl = mmio_read(self.base, E1000_RFCTL);
        rfctl |= RFCTL_EXTEN | RFCTL_NFSW_DIS | RFCTL_NFSR_DIS;
        mmio_write(self.base, E1000_RFCTL, rfctl);
        self.use_extended_wb = mmio_read(self.base, E1000_RFCTL) & RFCTL_EXTEN != 0;

        let rctl = rctl_rx_bits(self.base);
        mmio_write(self.base, E1000_RCTL, rctl);

        // Descriptors in RAM only — no RDT until QUEUE_ENABLE is latched.
        self.reinit_rx_ring();
        program_srrctl_rx_queue0(self.base);

        let mut rxdctl = mmio_read(self.base, E1000_RXDCTL);
        rxdctl &= 0xFFFF_C000;
        rxdctl |= RXDCTL_DMA_BURST;
        mmio_write(self.base, E1000_RXDCTL, rxdctl);
        let _ = mmio_read(self.base, E1000_RXDCTL);

        let mut rxd = mmio_read(self.base, E1000_RXDCTL);
        rxd |= RXDCTL_QUEUE_ENABLE;
        mmio_write(self.base, E1000_RXDCTL, rxd);
        for _ in 0..100 {
            if mmio_read(self.base, E1000_RXDCTL) & RXDCTL_QUEUE_ENABLE != 0 {
                break;
            }
            Self::udelay(100);
        }

        // RDT doorbell while RCTL.EN is still clear — I219 ignores post-EN RDT on empty ring.
        self.post_rx_boot_buffers(RX_BOOT_POST_MAX);

        let rdh = mmio_read(self.base, E1000_RDH);
        let rdt = mmio_read(self.base, E1000_RDT);
        if rdh == rdt {
            crate::klog_warn!(
                "e1000e: arm_rx empty ring RDH=RDT={} RXDCTL={:#x}\n",
                rdh,
                mmio_read(self.base, E1000_RXDCTL)
            );
        }

        mmio_write_flush(self.base, E1000_RCTL, rctl | RCTL_EN);
        let rctl_rb = mmio_read(self.base, E1000_RCTL);
        if rctl_rb & RCTL_EN == 0 {
            crate::klog_warn!("e1000e: arm_rx RCTL.EN did not latch ({:#x})\n", rctl_rb);
        }

        self.kick_rx_writeback();
        crate::klog_warn!(
            "e1000e: arm_rx RDH={} RDT={} RXDCTL={:#x} ext_wb={}\n",
            mmio_read(self.base, E1000_RDH),
            mmio_read(self.base, E1000_RDT),
            mmio_read(self.base, E1000_RXDCTL),
            self.use_extended_wb
        );
    }

    fn setup_irq_rate(&self) {
        unsafe {
            const IRQ_RATE: u32 = 10_000;
            let itr = 1_000_000 / IRQ_RATE * 4;
            mmio_write(self.base, E1000_ITR, itr);
            if self.is_e1000e {
                for reg in [E1000_EITR0, E1000_EITR0 + 1, E1000_EITR0 + 2, E1000_EITR0 + 3, E1000_EITR0 + 4] {
                    mmio_write(self.base, reg, itr);
                }
            }
        }
    }

    fn ims_rearm(&self) {
        unsafe {
            mmio_write(self.base, E1000_IMS, IMS_RX | IMS_TX | IMS_LSC);
            let _ = mmio_read(self.base, E1000_IMS);
            fence(Ordering::SeqCst);
        }
    }

    pub fn init_hw(
        base: usize,
        pci_loc: Location,
        device_id: u16,
        dma_coherent: bool,
        rx_ring: DmaRegion,
        tx_ring: DmaRegion,
        rx_buf_pool: DmaRegion,
        tx_buf_pool: DmaRegion,
    ) -> DeviceResult<Self> {
        let is_e1000e = is_e1000e_device(device_id);
        let mut hw = Self {
            base,
            pci_loc,
            device_id,
            is_e1000e,
            mac: [0; 6],
            dma_coherent,
            use_extended_wb: false,
            rx_ring,
            rx_buf_pool,
            rx_last_head: 0,
            tx_ring,
            tx_buf_pool,
            tx_last_head: 0,
            tx_tail: 0,
            rx_pending: None,
            stats: NetStats::default(),
            rx_poll_budget: 32,
        };

        unsafe {
            mmio_write(base, E1000_IMC, 0xffff);
            let _ = mmio_read(base, E1000_IMC);

            if is_e1000e {
                let ctrl_ext = mmio_read(base, E1000_CTRL_EXT);
                mmio_write(base, E1000_CTRL_EXT, ctrl_ext | (1 << 27)); // IAME
                mmio_write(base, E1000_IAM, 0);
            }

            if crate::net::e1000e_pch::is_pch_device(device_id) {
                let _ = crate::net::e1000e_pch::bringup_link(base, pci_loc, device_id);
            }

            hw.setup_irq_rate();

            mmio_write(base, E1000_RCTL, 0);
            mmio_write(base, E1000_TCTL, 0);

            let ctrl = mmio_read(base, E1000_CTRL);
            mmio_write(base, E1000_CTRL, ctrl | CTRL_SLU | CTRL_ASDE | CTRL_FD);

            hw.read_mac();
            hw.program_mac_filter();

            let rx_p = hw.rx_ring.paddr() as u64;
            mmio_write(base, E1000_RDBAL, rx_p as u32);
            mmio_write(base, E1000_RDBAH, (rx_p >> 32) as u32);
            mmio_write(base, E1000_RDLEN, (RING_LEN * size_of::<RxDesc>()) as u32);
            mmio_write(base, E1000_RDH, 0);
            mmio_write(base, E1000_RDT, 0);
            mmio_write(base, E1000_RDTR, 0);
            mmio_write(base, E1000_RADV, 0);
            mmio_write(base, E1000_RSRPD, 0);
            mmio_write(base, E1000_FCRTL, 0);
            mmio_write(base, E1000_FCRTH, 0);

            if is_pch_spt_or_later(device_id) {
                hw.arm_rx_pch();
            } else {
                // 82574L/QEMU and other conventional e1000e: legacy WB layout only.
                // RFCTL_EXTEN must not be forced here — readback can lie while HW still
                // writes len/status at the legacy offsets, breaking DD detection.
                hw.use_extended_wb = false;
                hw.fill_rx_ring();
                mmio_write(base, E1000_RCTL, RCTL_ENABLE);
            }

            let tx_p = hw.tx_ring.paddr() as u64;
            mmio_write(base, E1000_TDBAL, tx_p as u32);
            mmio_write(base, E1000_TDBAH, (tx_p >> 32) as u32);
            mmio_write(base, E1000_TDLEN, (RING_LEN * size_of::<TxDesc>()) as u32);
            mmio_write(base, E1000_TDH, 0);
            mmio_write(base, E1000_TDT, 0);

            mmio_write(base, E1000_TIPG, (6 << 20) | (8 << 10) | 8);
            mmio_write(base, E1000_TCTL, (1 << 3) | (1 << 1)); // PSP | EN

            let _ = mmio_read(base, E1000_ICR);
            hw.ims_rearm();
        }

        let rctl = unsafe { mmio_read(base, E1000_RCTL) };
        crate::klog_warn!(
            "e1000e: init dev={:#06x} RCTL={:#x} (BAM={}) ext_wb={} dma_uc={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
            device_id as u32,
            rctl,
            rctl & RCTL_BAM != 0,
            hw.use_extended_wb,
            hw.dma_coherent,
            hw.mac[0],
            hw.mac[1],
            hw.mac[2],
            hw.mac[3],
            hw.mac[4],
            hw.mac[5]
        );

        Ok(hw)
    }

    pub fn link_up(&self) -> bool {
        unsafe { mmio_read(self.base, E1000_STATUS) & STATUS_LU != 0 }
    }

    fn clean_tx(&mut self) {
        unsafe {
            let tdh = mmio_read(self.base, E1000_TDH) as usize % RING_LEN;
            while self.tx_last_head != tdh {
                let desc = self.tx_ring.as_ptr::<TxDesc>().add(self.tx_last_head);
                core::ptr::write_volatile(&mut (*desc).status, 0);
                self.tx_last_head = (self.tx_last_head + 1) % RING_LEN;
            }
        }
    }

    fn drain_rx_descriptors(&mut self) -> Option<Vec<u8>> {
        if self.rx_poll_budget == 0 {
            return None;
        }

        unsafe {
            let idx = self.rx_last_head;
            self.sync_rx_desc_from_device(idx);

            let desc_ptr = self.rx_ring.as_ptr::<RxDesc>().add(idx);
            let mut wb = 0u64;
            let mut parsed = None;
            for attempt in 0..4 {
                if attempt > 0 {
                    Self::udelay(10);
                    self.sync_rx_desc_from_device(idx);
                }
                wb = Self::read_rx_wb_u64(desc_ptr);
                parsed = if self.use_extended_wb {
                    Self::parse_rx_wb_ext(wb)
                } else {
                    Self::parse_rx_wb_legacy(wb)
                };
                if parsed.is_some() {
                    break;
                }
            }
            let (staterr, len) = match parsed {
                Some(v) => v,
                None => return None,
            };

            compiler_fence(Ordering::Acquire);

            let eop = if self.use_extended_wb {
                staterr & RXD_EXT_EOP != 0
            } else {
                staterr & 0x02 != 0
            };
            // Legacy: errors byte @ +13. Extended (I219): old driver relied on len/EOP only.
            let rx_errors = if self.use_extended_wb {
                0
            } else {
                ((wb >> 40) & 0xFF) as u32
            };

            let mut completed: Option<Vec<u8>> = None;

            if rx_errors == 0 {
                if len > 0 && len <= BUF_SIZE {
                    dma_sync_region(
                        &self.rx_buf_pool,
                        self.dma_coherent,
                        idx * BUF_SIZE,
                        len,
                        DmaSyncDir::FromDevice,
                    );
                    let frag = core::slice::from_raw_parts(
                        self.rx_buf_vaddr(idx) as *const u8,
                        len,
                    );
                    if let Some(ref mut pending) = self.rx_pending {
                        if pending.len() + frag.len() <= 65536 {
                            pending.extend_from_slice(frag);
                            if eop {
                                completed = self.rx_pending.take();
                            }
                        } else {
                            self.rx_pending = None;
                            self.stats.rx_dropped += 1;
                        }
                    } else if eop {
                        completed = Some(frag.to_vec());
                    } else {
                        self.rx_pending = Some(frag.to_vec());
                    }
                } else {
                    self.rx_pending = None;
                    self.stats.rx_dropped += 1;
                }
            } else {
                self.rx_pending = None;
                self.stats.rx_errors += 1;
            }

            Self::clear_rx_desc_wb(desc_ptr as *mut RxDesc);

            self.rx_last_head = (idx + 1) % RING_LEN;
            self.repost_rx_buffer(idx);

            if let Some(pkt) = completed {
                self.rx_poll_budget = self.rx_poll_budget.saturating_sub(1);
                self.stats.rx_packets += 1;
                self.stats.rx_bytes += pkt.len() as u64;
                return Some(pkt);
            }
        }
        None
    }

    pub fn receive(&mut self) -> Option<Vec<u8>> {
        self.clean_tx();
        self.drain_rx_descriptors()
    }

    pub fn can_send(&mut self) -> bool {
        self.clean_tx();
        (self.tx_tail + 1) % RING_LEN != self.tx_last_head
    }

    pub fn send(&mut self, data: &[u8]) -> DeviceResult<()> {
        if data.len() > BUF_SIZE {
            return Err(DeviceError::IoError);
        }
        if !self.can_send() {
            return Err(DeviceError::NotReady);
        }

        let idx = self.tx_tail;
        unsafe {
            let dst = core::slice::from_raw_parts_mut(self.tx_buf_vaddr(idx) as *mut u8, data.len());
            dst.copy_from_slice(data);

            let td = TxDesc {
                addr: (self.tx_buf_pool.paddr() + idx * BUF_SIZE) as u64,
                length: data.len() as u16,
                cso: 0,
                cmd: (1 << 3) | (1 << 1) | (1 << 0), // RS | IFCS | EOP
                status: 0,
                css: 0,
                special: 0,
            };
            let txd = self.tx_ring.as_ptr::<TxDesc>().add(idx);
            desc_copy16(txd as *mut u8, &td as *const TxDesc as *const u8);

            self.tx_tail = (self.tx_tail + 1) % RING_LEN;
            mmio_write(self.base, E1000_TDT, self.tx_tail as u32);
        }

        self.stats.tx_packets += 1;
        self.stats.tx_bytes += data.len() as u64;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// smoltcp + NetScheme wrappers
// ---------------------------------------------------------------------------
#[derive(Clone)]
pub struct E1000eDriver {
    pub hw: Arc<Mutex<E1000eHw>>,
    stats: Arc<Mutex<NetStats>>,
}

#[derive(Clone)]
pub struct E1000eInterface {
    iface: Arc<Mutex<Interface<'static, E1000eDriver>>>,
    driver: E1000eDriver,
    name: String,
    irq: usize,
    base: usize,
    poll_pending: Arc<AtomicBool>,
    routes: Arc<Mutex<Vec<RouteInfo>>>,
    ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl E1000eInterface {
    fn ims_rearm(&self) {
        unsafe {
            mmio_write(self.base, E1000_IMS, IMS_RX | IMS_TX | IMS_LSC);
            let _ = mmio_read(self.base, E1000_IMS);
            fence(Ordering::SeqCst);
        }
    }
}

impl Scheme for E1000eInterface {
    fn name(&self) -> &str {
        "e1000e"
    }

    /// Minimal IRQ path (LK / [`e1000::E1000Interface`]): read ICR, mask IMS, queue one
    /// deferred poll.  No `hw.lock()`, no `pulse_signal` — avoids `RefCell already borrowed`
    /// when `poll()` holds `SOCKETS` and this IRQ fires nested.
    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            return;
        }

        let icr = unsafe { mmio_read(self.base, E1000_ICR) };
        if icr == 0 {
            if !self.poll_pending.load(Ordering::SeqCst) {
                self.ims_rearm();
            }
            return;
        }

        if self.poll_pending.load(Ordering::SeqCst) {
            self.ims_rearm();
            return;
        }

        self.poll_pending.store(true, Ordering::SeqCst);
        unsafe {
            mmio_write(self.base, E1000_IMC, 0xffff);
            let _ = mmio_read(self.base, E1000_IMC);
        }

        let poll_pending = self.poll_pending.clone();
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job(move || {
            let _ = me.poll();
            poll_pending.store(false, Ordering::SeqCst);
        });
    }
}

impl NetScheme for E1000eInterface {
    fn get_mac(&self) -> EthernetAddress {
        self.iface.lock().ethernet_addr()
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    fn get_ip_address(&self) -> Vec<IpCidr> {
        self.ip_addrs.lock().clone()
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

    fn seed_neighbor(
        &self,
        protocol: smoltcp::wire::IpAddress,
        hardware: EthernetAddress,
    ) -> DeviceResult {
        let ts = Instant::from_micros(timer_now_as_micros() as i64);
        self.iface.lock().seed_neighbor(protocol, hardware, ts);
        Ok(())
    }

    fn refresh_link(&self) -> DeviceResult {
        Ok(())
    }

    fn link_carrier_up(&self) -> bool {
        self.driver.hw.lock().link_up()
    }

    fn poll(&self) -> DeviceResult {
        let ts = Instant::from_micros(timer_now_as_micros() as i64);
        {
            let mut hw = self.driver.hw.lock();
            hw.rx_poll_budget = if hw.device_id == 0x10d3 { 8 } else { 32 };
        }

        // Keep IRQs off while SOCKETS + iface are locked (rtlx pattern).
        let intr_was_on = super::intr_get();
        if intr_was_on {
            super::intr_off();
        }
        let sockets = get_sockets();
        {
            let mut sockets = sockets.lock();
            let _ = self.iface.lock().poll(&mut sockets, ts);
        }
        if intr_was_on {
            super::intr_on();
        }

        {
            let mut hw = self.driver.hw.lock();
            for _ in 0..16 {
                if hw.rx_poll_budget == 0 {
                    hw.rx_poll_budget = 8;
                }
                match hw.receive() {
                    Some(pkt) => {
                        drop(hw);
                        super::net_dispatch_packet(&pkt);
                        hw = self.driver.hw.lock();
                    }
                    None => break,
                }
            }
        }
        self.ims_rearm();
        crate::pulse::pulse_signal(crate::pulse::PULSE_NET_RX);
        super::wake_net_rx_waiters();
        Ok(())
    }

    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        let pkt = {
            let mut hw = self.driver.hw.lock();
            if hw.rx_poll_budget == 0 {
                hw.rx_poll_budget = 16;
            }
            hw.receive()
        };
        if let Some(pkt) = pkt {
            let n = pkt.len().min(buf.len());
            buf[..n].copy_from_slice(&pkt[..n]);
            Ok(n)
        } else {
            Err(DeviceError::NotReady)
        }
    }

    fn send(&self, data: &[u8]) -> DeviceResult<usize> {
        let mut hw = self.driver.hw.lock();
        hw.send(data)?;
        Ok(data.len())
    }

    fn can_recv(&self) -> bool {
        true
    }

    fn can_send(&self) -> bool {
        self.driver.hw.lock().can_send()
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

    fn del_route(&self, cidr: IpCidr, _gateway: Option<IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        if cidr.prefix_len() == 0 {
            match cidr {
                IpCidr::Ipv4(_) => {
                    let _ = iface.routes_mut().remove_default_ipv4_route();
                }
                IpCidr::Ipv6(_) => {}
                _ => {}
            }
        }
        self.routes.lock().retain(|r| r.dst != cidr);
        Ok(())
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        let iface = self.iface.lock();
        let mut res = self.routes.lock().clone();
        for cidr in iface.ip_addrs() {
            match cidr {
                IpCidr::Ipv4(v4) => {
                    if v4.prefix_len() > 0 && v4.address().0[0] != 240 {
                        res.push(RouteInfo {
                            dst: IpCidr::Ipv4(v4.network()),
                            gateway: None,
                        });
                    }
                }
                IpCidr::Ipv6(v6) => {
                    if v6.prefix_len() > 0 {
                        res.push(RouteInfo {
                            dst: IpCidr::Ipv6(v6.network()),
                            gateway: None,
                        });
                    }
                }
                _ => {}
            }
        }
        res
    }

    fn get_stats(&self) -> NetStats {
        let hw = self.driver.hw.lock();
        let mut out = self.driver.stats.lock().clone();
        out.rx_bytes = hw.stats.rx_bytes;
        out.rx_packets = hw.stats.rx_packets;
        out.tx_bytes = hw.stats.tx_bytes;
        out.tx_packets = hw.stats.tx_packets;
        out.rx_errors = hw.stats.rx_errors;
        out.rx_dropped = hw.stats.rx_dropped;
        out
    }

    fn get_mtu(&self) -> usize {
        1500
    }
}

pub struct E1000eRxToken {
    data: Vec<u8>,
}

pub struct E1000eTxToken(E1000eDriver);

impl phy::Device<'_> for E1000eDriver {
    type RxToken = E1000eRxToken;
    type TxToken = E1000eTxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        self.hw.lock().receive().map(|pkt| {
            (
                E1000eRxToken { data: pkt },
                E1000eTxToken(self.clone()),
            )
        })
    }

    fn transmit(&mut self) -> Option<Self::TxToken> {
        if self.hw.lock().can_send() {
            Some(E1000eTxToken(self.clone()))
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(RING_LEN);
        caps
    }
}

impl phy::RxToken for E1000eRxToken {
    fn consume<R, F>(self, _ts: Instant, f: F) -> SmolResult<R>
    where
        F: FnOnce(&mut [u8]) -> SmolResult<R>,
    {
        let mut data = self.data;
        super::net_dispatch_packet(&data);
        f(&mut data)
    }
}

impl phy::TxToken for E1000eTxToken {
    fn consume<R, F>(self, _ts: Instant, len: usize, f: F) -> SmolResult<R>
    where
        F: FnOnce(&mut [u8]) -> SmolResult<R>,
    {
        let len = len.min(BUF_SIZE);
        let mut buf = vec![0u8; len];
        let result = f(&mut buf)?;
        self.0
            .hw
            .lock()
            .send(&buf)
            .map_err(|_| smoltcp::Error::Exhausted)?;
        Ok(result)
    }
}

pub fn init(
    name: String,
    pci: &PCIDevice,
    irq: usize,
    vaddr: usize,
    _index: usize,
) -> DeviceResult<E1000eInterface> {
    let (rx_ring, rx_uc) = DmaRegion::alloc_uninit_try_coherent(RING_LEN * RX_DESC_SIZE)
        .ok_or(DeviceError::DmaError)?;
    let (tx_ring, tx_uc) = DmaRegion::alloc_uninit_try_coherent(RING_LEN * size_of::<TxDesc>())
        .ok_or(DeviceError::DmaError)?;
    let (rx_buf_pool, rx_pool_uc) = DmaRegion::alloc_uninit_try_coherent(RING_LEN * BUF_SIZE)
        .ok_or(DeviceError::DmaError)?;
    let (tx_buf_pool, tx_pool_uc) = DmaRegion::alloc_uninit_try_coherent(RING_LEN * BUF_SIZE)
        .ok_or(DeviceError::DmaError)?;

    let probe_coherent = rx_uc && tx_uc && rx_pool_uc && tx_pool_uc;
    let dma_coherent = probe_coherent && pci.id.device_id != 0x10d3;

    let hw = E1000eHw::init_hw(
        vaddr,
        pci.loc,
        pci.id.device_id,
        dma_coherent,
        rx_ring,
        tx_ring,
        rx_buf_pool,
        tx_buf_pool,
    )?;

    let mac_bytes = hw.mac;
    crate::klog_warn!(
        "e1000e: {} {:#x}:{:#x} link={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
        name,
        pci.id.vendor_id,
        pci.id.device_id,
        if hw.link_up() { "up" } else { "down" },
        mac_bytes[0],
        mac_bytes[1],
        mac_bytes[2],
        mac_bytes[3],
        mac_bytes[4],
        mac_bytes[5]
    );

    let hw_arc = Arc::new(Mutex::new(hw));
    let stats = Arc::new(Mutex::new(NetStats::default()));
    let driver = E1000eDriver {
        hw: hw_arc,
        stats: stats.clone(),
    };

    let ethernet_addr = EthernetAddress::from_bytes(&mac_bytes);
    let mut eui64 = [0u8; 8];
    eui64[0] = mac_bytes[0] ^ 2;
    eui64[1] = mac_bytes[1];
    eui64[2] = mac_bytes[2];
    eui64[3] = 0xff;
    eui64[4] = 0xfe;
    eui64[5] = mac_bytes[3];
    eui64[6] = mac_bytes[4];
    eui64[7] = mac_bytes[5];
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
    let default_v4_gw = Ipv4Address::new(0, 0, 0, 0);
    static mut ROUTES_STORAGE: [Option<(IpCidr, Route)>; 4] = [None; 4];
    let mut routes = unsafe { Routes::new(&mut ROUTES_STORAGE[..]) };
    routes.add_default_ipv4_route(default_v4_gw).unwrap();
    let neighbor_cache = NeighborCache::new(BTreeMap::new());

    let iface = InterfaceBuilder::new(driver.clone())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(neighbor_cache)
        .ip_addrs(ip_addrs.clone())
        .routes(routes)
        .finalize();

    Ok(E1000eInterface {
        iface: Arc::new(Mutex::new(iface)),
        driver,
        name,
        irq,
        base: vaddr,
        poll_pending: Arc::new(AtomicBool::new(false)),
        routes: Arc::new(Mutex::new(vec![RouteInfo {
            dst: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            gateway: Some(IpAddress::Ipv4(default_v4_gw)),
        }])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    })
}

pub struct E1000eDriverPci;

impl PciDriver for E1000eDriverPci {
    fn name(&self) -> &str {
        "e1000e"
    }

    fn matched(&self, vendor_id: u16, device_id: u16) -> bool {
        matched_device(vendor_id, device_id)
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        let bar0_addr = if let Some(BAR::Memory(a, _, _, _)) = dev.bars[0] {
            a as usize
        } else {
            return Err(DeviceError::IoError);
        };

        if let Some(m) = mapper {
            m.query_or_map(bar0_addr, 128 * 1024);
        }

        unsafe {
            let mut cmd = PCI_ACCESS.read16(&PortOpsImpl, dev.loc, 0x04);
            cmd |= 0x0004 | 0x0002;
            PCI_ACCESS.write16(&PortOpsImpl, dev.loc, 0x04, cmd);
        }

        let vaddr = crate::net::phys_to_virt(bar0_addr);
        let name = alloc::format!("eth{}", dev.loc.bus);
        let vector = irq.map(|idx| idx + 32).unwrap_or(0);
        let iface = Arc::new(init(name, dev, vector, vaddr, 0)?);
        if vector != 0 {
            crate::net::pci_note_pending_msi(vector, iface.clone());
        }
        Ok(Device::Net(iface))
    }
}
