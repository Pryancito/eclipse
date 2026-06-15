//! AHCI (Advanced Host Controller Interface) Driver for Eclipse OS 2
//!
//! Adapted from the previous Eclipse OS AHCI driver to work with the new
//! zCore-based driver architecture.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::bus::{drivers_dma_alloc, drivers_timer_now_as_micros, phys_to_virt, virt_to_phys};
use crate::scheme::{BlockScheme, Scheme};
use crate::{Device, DeviceError, DeviceResult};
use alloc::sync::Arc;
use pci::{PCIDevice, BAR};

use lock::Mutex;

// --- HBA global register offsets ---
const HBA_GHC: usize = 0x04;
const HBA_PI: usize = 0x0C;
const HBA_BOHC: usize = 0x28; // BIOS/OS Handoff Control and Status

const GHC_AE: u32 = 1 << 31;
const GHC_HR: u32 = 1 << 0;

// BOHC bits (AHCI spec §10.6)
const BOHC_BOS: u32 = 1 << 0; // BIOS Owns Semaphore
const BOHC_OOS: u32 = 1 << 1; // OS Owns Semaphore
const BOHC_BB: u32 = 1 << 4; // BIOS Busy

// --- Per-port register offsets ---
const PORT_CLB: usize = 0x00;
const PORT_CLBU: usize = 0x04;
const PORT_FB: usize = 0x08;
const PORT_FBU: usize = 0x0C;
const PORT_IS: usize = 0x10;
const PORT_IE: usize = 0x14;
const PORT_CMD: usize = 0x18;
const PORT_TFD: usize = 0x20;
const PORT_SIG: usize = 0x24;
const PORT_SSTS: usize = 0x28;
const PORT_SCTL: usize = 0x2C;
const PORT_SERR: usize = 0x30;
const PORT_CI: usize = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_SUD: u32 = 1 << 1;
const CMD_POD: u32 = 1 << 2;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;

const ATA_DEV_BUSY: u8 = 0x80;
const ATA_DEV_DRQ: u8 = 0x08;

const HBA_SIG_ATA: u32 = 0x0000_0101;
const FIS_TYPE_REG_H2D: u8 = 0x27;

const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_READ_DMA: u8 = 0xC8;
const ATA_CMD_WRITE_DMA: u8 = 0xCA;
const ATA_CMD_IDENTIFY: u8 = 0xEC;
const ATA_CMD_FLUSH_CACHE: u8 = 0xE7;
const ATA_CMD_FLUSH_CACHE_EXT: u8 = 0xEA;

const SECTOR_SIZE: usize = 512;

// AHCI spec §4.2.2: each Command Header is 32 bytes; the Command List base
// address must be 1 KiB-aligned (provided by drivers_dma_alloc → 4 KiB page).
#[repr(C)]
struct CommandHeader {
    cfl: u8,
    pm: u8,
    prdtl: u16,
    prdbc: u32,
    ctba: u32,
    ctbau: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct PrdEntry {
    dba: u32,
    dbau: u32,
    _rsvd: u32,
    dbc_ioc: u32,
}

// The PRDT starts at offset 0x80 of the command table (AHCI spec §4.2.3).
// 56 entries × 16 B fill the table exactly to its 1 KiB stride, allowing one
// command to cover up to 56 non-contiguous pages (224 KiB) without growing
// the per-port DMA footprint.
const PRDT_MAX: usize = 56;

#[repr(C, align(1024))]
struct CommandTable {
    cfis: [u8; 64],
    acmd: [u8; 16],
    _rsvd: [u8; 48],
    prdt: [PrdEntry; PRDT_MAX],
}

const _: () = {
    assert!(core::mem::size_of::<CommandHeader>() == 32);
    assert!(core::mem::size_of::<PrdEntry>() == 16);
    assert!(core::mem::size_of::<CommandTable>() == 1024);
};

const CMD_SLOTS: usize = 32;
const CMD_TABLE_STRIDE: usize = 1024; // 1024-aligned to support strict controllers

// DMA memory layout (9 pages = 36 864 bytes):
//   [0..1024)      command list  : CMD_SLOTS × sizeof(CommandHeader) = 32 × 32 = 1024 B
//   [1024..1280)   FIS recv area : 256 B  (256-byte aligned ✓)
//   [4096..36864)  command tables: CMD_SLOTS × CMD_TABLE_STRIDE = 32 × 1024 = 32 768 B
const DMA_PAGES: usize = 9;

// AHCI spec §10.4.2: COMRESET must be asserted for at least 1 ms.
const COMRESET_US: u64 = 1_000;
// AHCI spec §10.4.2: device reinitialization after COMRESET must complete within
// 10 seconds; we give it 1 second.
const PHY_LINK_TIMEOUT_US: u64 = 1_000_000;
// Timeout for HBA global reset to self-clear (spec: 1 second).
const HBA_RESET_TIMEOUT_US: u64 = 1_000_000;
// Timeout for stop_engine: CR and FR must clear within 500 ms (AHCI spec).
const ENGINE_STOP_TIMEOUT_US: u64 = 500_000;
// Timeout for exec_cmd: 10 seconds covers slow spinning disks.
const CMD_TIMEOUT_US: u64 = 10_000_000;
// Shorter timeout for ATA IDENTIFY during init — 5 seconds is enough.
const IDENTIFY_TIMEOUT_US: u64 = 5_000_000;
// Timeout for port TFD-busy before issuing a command: 2 seconds.
const TFD_TIMEOUT_US: u64 = 2_000_000;
// FLUSH CACHE may need to drain a large write cache on spinning disks.
const FLUSH_TIMEOUT_US: u64 = 30_000_000;

// Pages for the bounce buffer used with non-page-aligned caller buffers.
// 32 pages = 128 KiB = 256 sectors per command (vs. 1 sector previously).
const BOUNCE_PAGES: usize = 32;
// Upper bound for a direct (zero-copy) transfer: one page per PRD entry.
const DIRECT_MAX_BYTES: usize = PRDT_MAX * 4096;
// 28-bit DMA commands carry an 8-bit sector count (0 means 256; we stay at 255).
const LBA28_MAX_SECTORS: usize = 255;

/// Busy-wait for `us` microseconds using the TSC-based driver timer.
#[inline]
fn udelay(us: u64) {
    let t0 = unsafe { drivers_timer_now_as_micros() };
    // Hard spin guard: prevents infinite loop if TSC-based timer does not advance
    // (e.g. cpu_frequency() returned a wrong value, or early-boot timer quirk).
    // At ~1 GHz effective spin rate, 10 M iterations ≈ 10 ms; at 4 GHz ≈ 2.5 ms.
    // This fires only when the timer is genuinely broken; normally the timer-based
    // condition exits first.
    const MAX_SPINS: u64 = 10_000_000;
    let mut spins = 0u64;
    while unsafe { drivers_timer_now_as_micros() }.wrapping_sub(t0) < us {
        spin_loop();
        spins = spins.wrapping_add(1);
        if spins >= MAX_SPINS {
            warn!(
                "[AHCI] udelay fallback hit ({}us requested — timer did not advance)",
                us
            );
            break;
        }
    }
}

/// Busy-wait until `pred()` returns true or `timeout_us` elapses.
/// Returns true if the condition was met, false on timeout.
/// Includes a hard spin-count cap so the function always terminates even
/// if `drivers_timer_now_as_micros()` returns a constant (broken timer).
#[inline]
fn wait_until<F: Fn() -> bool>(timeout_us: u64, pred: F) -> bool {
    let t0 = unsafe { drivers_timer_now_as_micros() };
    // Maximum iterations: 500 M is ~500 ms at 1 GHz, ~125 ms at 4 GHz.
    // We scale by the requested timeout to keep proportional coverage.
    // At worst this adds a 500 ms hard cap per call, which is acceptable
    // for a boot path but ensures we never hang the system indefinitely.
    let max_spins: u64 = 500_000_000u64.max(timeout_us.saturating_mul(500));
    let mut spins = 0u64;
    loop {
        if pred() {
            return true;
        }
        spin_loop();
        spins = spins.wrapping_add(1);
        if unsafe { drivers_timer_now_as_micros() }.wrapping_sub(t0) >= timeout_us {
            return false;
        }
        if spins >= max_spins {
            warn!(
                "[AHCI] wait_until fallback: timer stuck, forcing timeout after {} spins",
                spins
            );
            return false;
        }
    }
}

extern "C" {
    fn drivers_dma_dealloc(paddr: usize, pages: usize) -> i32;
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn clflush_range(vaddr: usize, len: usize) {
    use core::arch::x86_64::{_mm_clflush, _mm_mfence};
    unsafe {
        _mm_mfence();
    }
    let mut addr = vaddr & !63;
    let end = vaddr + len;
    while addr < end {
        unsafe {
            _mm_clflush(addr as *const u8);
        }
        addr += 64;
    }
    unsafe {
        _mm_mfence();
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn clflush_range(_vaddr: usize, _len: usize) {}

struct AhciPort {
    base: usize,
    port_idx: u32,
    cl_phys: u64,
    cl_virt: usize,
    fb_phys: u64,
    _fb_virt: usize,
    ct_phys: u64,
    ct_virt: usize,
    /// Device supports the 48-bit LBA feature set (IDENTIFY word 83 bit 10).
    lba48: bool,
}

impl AhciPort {
    fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write_reg(&self, offset: usize, val: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, val) }
    }

    fn stop_engine(&self) {
        self.write_reg(PORT_CMD, self.read_reg(PORT_CMD) & !CMD_ST);
        self.write_reg(PORT_CMD, self.read_reg(PORT_CMD) & !CMD_FRE);
        // AHCI spec requires CR and FR to clear within 500 ms of clearing ST/FRE.
        if !wait_until(ENGINE_STOP_TIMEOUT_US, || {
            self.read_reg(PORT_CMD) & (CMD_CR | CMD_FR) == 0
        }) {
            warn!(
                "[AHCI] Port {} stop_engine timeout (CR/FR stuck)",
                self.port_idx
            );
        }
    }

    fn start_engine(&self) {
        // AHCI spec: wait for CR to clear before setting FRE and ST.
        if !wait_until(ENGINE_STOP_TIMEOUT_US, || {
            self.read_reg(PORT_CMD) & CMD_CR == 0
        }) {
            warn!(
                "[AHCI] Port {} start_engine timeout (CR stuck)",
                self.port_idx
            );
        }
        self.write_reg(PORT_CMD, self.read_reg(PORT_CMD) | CMD_FRE | CMD_ST);
    }

    fn init(&self) -> bool {
        self.stop_engine();

        // Configure command list
        unsafe {
            let headers = self.cl_virt as *mut CommandHeader;
            core::ptr::write_bytes(
                headers as *mut u8,
                0,
                CMD_SLOTS * core::mem::size_of::<CommandHeader>(),
            );
            for i in 0..CMD_SLOTS {
                let h = &mut *headers.add(i);
                let ct_phys = self.ct_phys + (i * CMD_TABLE_STRIDE) as u64;
                h.ctba = ct_phys as u32;
                h.ctbau = (ct_phys >> 32) as u32;
            }
            // Zero the FIS receive area (size 256 bytes, located at cl_virt + 1024)
            core::ptr::write_bytes((self.cl_virt + 1024) as *mut u8, 0, 256);
        }
        clflush_range(
            self.cl_virt,
            CMD_SLOTS * core::mem::size_of::<CommandHeader>(),
        );
        clflush_range(self.cl_virt + 1024, 256);

        self.write_reg(PORT_CLB, self.cl_phys as u32);
        self.write_reg(PORT_CLBU, (self.cl_phys >> 32) as u32);
        self.write_reg(PORT_FB, self.fb_phys as u32);
        self.write_reg(PORT_FBU, (self.fb_phys >> 32) as u32);

        self.write_reg(PORT_IS, 0xFFFF_FFFF);
        self.write_reg(PORT_SERR, 0xFFFF_FFFF);
        self.write_reg(PORT_IE, 0);

        self.write_reg(
            PORT_CMD,
            self.read_reg(PORT_CMD) | CMD_POD | CMD_SUD | CMD_FRE,
        );

        // COMRESET: assert DET=1 for at least 1 ms, then release
        self.write_reg(PORT_SCTL, (self.read_reg(PORT_SCTL) & !0xF) | 1);
        udelay(COMRESET_US);
        self.write_reg(PORT_SCTL, self.read_reg(PORT_SCTL) & !0xF);

        // Wait for PHY to show some sign of device presence (DET != 0), max 50 ms.
        // This avoids waiting the full 1 second timeout on empty ports.
        if !wait_until(50_000, || self.read_reg(PORT_SSTS) & 0xF != 0) {
            return false;
        }

        // Wait for PHY to establish link (DET=3), max 1 second
        if !wait_until(PHY_LINK_TIMEOUT_US, || self.read_reg(PORT_SSTS) & 0xF == 3) {
            crate::klog_warn!("[AHCI] port {} PHY link timeout", self.port_idx);
            return false;
        }

        self.write_reg(PORT_SERR, 0xFFFF_FFFF);
        self.start_engine();
        true
    }

    fn reset_port(&self) {
        self.stop_engine();
        // COMRESET: set DET=1, wait ≥1 ms (AHCI spec §10.4.2), then clear DET
        self.write_reg(PORT_SCTL, (self.read_reg(PORT_SCTL) & !0xF) | 1);
        udelay(COMRESET_US);
        self.write_reg(PORT_SCTL, self.read_reg(PORT_SCTL) & !0xF);
        // Wait for PHY to re-establish link (DET=3), max 1 second
        if !wait_until(PHY_LINK_TIMEOUT_US, || self.read_reg(PORT_SSTS) & 0xF == 3) {
            warn!("[AHCI] Port {} reset_port: PHY link timeout", self.port_idx);
        }
        self.write_reg(PORT_SERR, 0xFFFF_FFFF);
        self.write_reg(PORT_IS, 0xFFFF_FFFF);
        self.start_engine();
    }

    fn exec_cmd(&self, slot: u32) -> DeviceResult {
        self.exec_cmd_with_timeout(slot, CMD_TIMEOUT_US)
    }

    fn exec_cmd_with_timeout(&self, slot: u32, timeout_us: u64) -> DeviceResult {
        fence(Ordering::SeqCst);
        self.write_reg(PORT_IS, 0xFFFF_FFFF);
        self.write_reg(PORT_CI, 1 << slot);

        if !wait_until(timeout_us, || {
            if self.read_reg(PORT_IS) & (1 << 30) != 0 {
                return true;
            }
            self.read_reg(PORT_CI) & (1 << slot) == 0
        }) {
            let tfd = self.read_reg(PORT_TFD);
            let ssts = self.read_reg(PORT_SSTS);
            let serr = self.read_reg(PORT_SERR);
            let is = self.read_reg(PORT_IS);
            let ci = self.read_reg(PORT_CI);
            crate::klog_err!(
                "[AHCI] port {} command timeout ({} ms), TFD={:#x}, SSTS={:#x}, SERR={:#x}, IS={:#x}, CI={:#x}",
                self.port_idx,
                timeout_us / 1000,
                tfd, ssts, serr, is, ci
            );
            self.reset_port();
            return Err(DeviceError::IoError);
        }

        if self.read_reg(PORT_IS) & (1 << 30) != 0 {
            let tfd = self.read_reg(PORT_TFD);
            let ssts = self.read_reg(PORT_SSTS);
            let serr = self.read_reg(PORT_SERR);
            let is = self.read_reg(PORT_IS);
            let ci = self.read_reg(PORT_CI);
            crate::klog_err!(
                "[AHCI] port {} task file error, TFD={:#x}, SSTS={:#x}, SERR={:#x}, IS={:#x}, CI={:#x}",
                self.port_idx, tfd, ssts, serr, is, ci
            );
            self.reset_port();
            return Err(DeviceError::IoError);
        }

        Ok(())
    }

    /// Issue one DMA read/write command covering `prds` (physical ranges,
    /// each a multiple of 512 bytes) starting at sector `lba`.
    fn rw_block(&self, lba: u64, prds: &[(u64, usize)], write: bool) -> DeviceResult {
        let slot = 0u32;
        let buf_len: usize = prds.iter().map(|p| p.1).sum();
        let count = buf_len / SECTOR_SIZE;
        if prds.is_empty() || prds.len() > PRDT_MAX || buf_len == 0 || buf_len % SECTOR_SIZE != 0 {
            return Err(DeviceError::InvalidParam);
        }
        if self.lba48 {
            if count > 65536 {
                return Err(DeviceError::InvalidParam);
            }
        } else if count > LBA28_MAX_SECTORS || lba + count as u64 > (1 << 28) {
            return Err(DeviceError::InvalidParam);
        }

        // Wait for port to become ready (TFD BUSY/DRQ clear), max 2 seconds
        if !wait_until(TFD_TIMEOUT_US, || {
            self.read_reg(PORT_TFD) & ((ATA_DEV_BUSY | ATA_DEV_DRQ) as u32) == 0
        }) {
            crate::klog_err!(
                "[AHCI] port {} TFD busy timeout before command",
                self.port_idx
            );
            self.reset_port();
            return Err(DeviceError::IoError);
        }

        unsafe {
            let cmd_table = self.ct_virt as *mut CommandTable;
            core::ptr::write_bytes(
                cmd_table as *mut u8,
                0,
                core::mem::size_of::<CommandTable>(),
            );

            let fis = (*cmd_table).cfis.as_mut_ptr();
            *fis.add(0) = FIS_TYPE_REG_H2D;
            *fis.add(1) = 0x80;
            *fis.add(2) = if write {
                if self.lba48 {
                    ATA_CMD_WRITE_DMA_EXT
                } else {
                    ATA_CMD_WRITE_DMA
                }
            } else {
                if self.lba48 {
                    ATA_CMD_READ_DMA_EXT
                } else {
                    ATA_CMD_READ_DMA
                }
            };
            if self.lba48 {
                *fis.add(4) = lba as u8;
                *fis.add(5) = (lba >> 8) as u8;
                *fis.add(6) = (lba >> 16) as u8;
                *fis.add(7) = 0x40;
                *fis.add(8) = (lba >> 24) as u8;
                *fis.add(9) = (lba >> 32) as u8;
                *fis.add(10) = (lba >> 40) as u8;
                // count == 65536 encodes as 0 per the ATA spec.
                *fis.add(12) = count as u8;
                *fis.add(13) = (count >> 8) as u8;
            } else {
                *fis.add(4) = lba as u8;
                *fis.add(5) = (lba >> 8) as u8;
                *fis.add(6) = (lba >> 16) as u8;
                *fis.add(7) = 0xE0 | ((lba >> 24) & 0x0F) as u8;
                *fis.add(12) = count as u8;
                *fis.add(13) = 0;
            }

            for (i, &(phys, len)) in prds.iter().enumerate() {
                let e = &mut (*cmd_table).prdt[i];
                e.dba = phys as u32;
                e.dbau = (phys >> 32) as u32;
                let ioc = if i == prds.len() - 1 { 1u32 << 31 } else { 0 };
                e.dbc_ioc = ((len as u32) - 1) | ioc;
            }

            let header = self.cl_virt as *mut CommandHeader;
            (*header).cfl = 5 | (if write { 1 << 6 } else { 0 });
            (*header).pm = 0; // Do NOT clear busy upon R_OK (c = 0) for data commands
            (*header).prdtl = prds.len() as u16;
            (*header).prdbc = 0;
        }
        clflush_range(self.ct_virt, core::mem::size_of::<CommandTable>());
        clflush_range(self.cl_virt, core::mem::size_of::<CommandHeader>());
        for &(phys, len) in prds {
            clflush_range(phys_to_virt(phys as usize), len);
        }

        let res = self.exec_cmd(slot);
        if res.is_ok() && !write {
            for &(phys, len) in prds {
                clflush_range(phys_to_virt(phys as usize), len);
            }
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        }
        res
    }

    /// ATA FLUSH CACHE — force the device write cache to non-volatile media.
    fn flush_cache(&self) -> DeviceResult {
        if !wait_until(TFD_TIMEOUT_US, || {
            self.read_reg(PORT_TFD) & ((ATA_DEV_BUSY | ATA_DEV_DRQ) as u32) == 0
        }) {
            crate::klog_err!(
                "[AHCI] port {} TFD busy timeout before flush",
                self.port_idx
            );
            self.reset_port();
            return Err(DeviceError::IoError);
        }

        unsafe {
            let cmd_table = self.ct_virt as *mut CommandTable;
            core::ptr::write_bytes(
                cmd_table as *mut u8,
                0,
                core::mem::size_of::<CommandTable>(),
            );
            let fis = (*cmd_table).cfis.as_mut_ptr();
            *fis.add(0) = FIS_TYPE_REG_H2D;
            *fis.add(1) = 0x80;
            *fis.add(2) = if self.lba48 {
                ATA_CMD_FLUSH_CACHE_EXT
            } else {
                ATA_CMD_FLUSH_CACHE
            };

            let header = self.cl_virt as *mut CommandHeader;
            (*header).cfl = 5;
            (*header).pm = 0;
            (*header).prdtl = 0;
            (*header).prdbc = 0;
        }
        clflush_range(self.ct_virt, core::mem::size_of::<CommandTable>());
        clflush_range(self.cl_virt, core::mem::size_of::<CommandHeader>());

        self.exec_cmd_with_timeout(0, FLUSH_TIMEOUT_US)
    }

    /// Returns `(sectors, lba48_supported)` from ATA IDENTIFY data.
    fn identify(&self) -> Option<(u64, bool)> {
        // Wait for port to become ready (TFD BUSY/DRQ clear), max 2 seconds
        if !wait_until(TFD_TIMEOUT_US, || {
            self.read_reg(PORT_TFD) & ((ATA_DEV_BUSY | ATA_DEV_DRQ) as u32) == 0
        }) {
            warn!(
                "[AHCI] Port {} TFD busy timeout before IDENTIFY",
                self.port_idx
            );
            self.reset_port();
            return None;
        }

        let slot = 0u32;
        let paddr = unsafe { drivers_dma_alloc(1) };
        let vaddr = phys_to_virt(paddr);

        unsafe {
            let cmd_table = self.ct_virt as *mut CommandTable;
            core::ptr::write_bytes(
                cmd_table as *mut u8,
                0,
                core::mem::size_of::<CommandTable>(),
            );
            core::ptr::write_bytes(vaddr as *mut u8, 0, 512);

            let fis = (*cmd_table).cfis.as_mut_ptr();
            *fis.add(0) = FIS_TYPE_REG_H2D;
            *fis.add(1) = 0x80;
            *fis.add(2) = ATA_CMD_IDENTIFY;
            *fis.add(7) = 0xA0; // Device (0xA0 is for master)

            (*cmd_table).prdt[0].dba = paddr as u32;
            (*cmd_table).prdt[0].dbau = (paddr >> 32) as u32;
            (*cmd_table).prdt[0].dbc_ioc = 511 | (1 << 31);

            let header = self.cl_virt as *mut CommandHeader;
            (*header).cfl = 5;
            (*header).pm = 0; // Do NOT clear busy upon R_OK (c = 0) for data commands
            (*header).prdtl = 1;
            (*header).prdbc = 0;
        }
        clflush_range(self.ct_virt, core::mem::size_of::<CommandTable>());
        clflush_range(self.cl_virt, core::mem::size_of::<CommandHeader>());
        clflush_range(vaddr, 512);

        if self
            .exec_cmd_with_timeout(slot, IDENTIFY_TIMEOUT_US)
            .is_err()
        {
            unsafe {
                drivers_dma_dealloc(paddr, 1);
            }
            return None;
        }

        clflush_range(vaddr, 512);
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);

        let id_ptr = vaddr as *const u16;
        let read_id = |idx: usize| -> u16 { unsafe { read_volatile(id_ptr.add(idx)) } };

        let lba48 = (read_id(100) as u64)
            | ((read_id(101) as u64) << 16)
            | ((read_id(102) as u64) << 32)
            | ((read_id(103) as u64) << 48);
        let lba28 = (read_id(60) as u64) | ((read_id(61) as u64) << 16);
        let lba48_supported = (read_id(83) & (1 << 10)) != 0;
        let sectors = if (lba48_supported || lba48 > lba28) && lba48 != 0 {
            lba48
        } else {
            lba28
        };

        crate::klog_warn!(
            "[AHCI] identify: lba48_supported={}, lba28={}, lba48={}, sectors={}, id[83]={:#x}, id[60]={:#x}, id[61]={:#x}, id[100]={:#x}",
            lba48_supported,
            lba28,
            lba48,
            sectors,
            read_id(83),
            read_id(60),
            read_id(61),
            read_id(100)
        );

        unsafe {
            drivers_dma_dealloc(paddr, 1);
        }

        Some((sectors, lba48_supported))
    }
}

/// Translate a virtually contiguous kernel buffer into physical ranges
/// suitable for a PRDT, coalescing physically adjacent pages. Returns the
/// number of ranges, or `None` if a page fails to translate or the buffer
/// would need more than `PRDT_MAX` entries (callers then fall back to the
/// bounce buffer).
fn build_prds(vaddr: usize, len: usize, prds: &mut [(u64, usize); PRDT_MAX]) -> Option<usize> {
    let mut n = 0usize;
    let mut va = vaddr;
    let end = vaddr + len;
    while va < end {
        let page_rem = 4096 - (va & 4095);
        let piece = page_rem.min(end - va);
        let pa = virt_to_phys(va) as u64;
        if pa == 0 {
            return None;
        }
        if n > 0 && prds[n - 1].0 + prds[n - 1].1 as u64 == pa {
            prds[n - 1].1 += piece;
        } else {
            if n == PRDT_MAX {
                return None;
            }
            prds[n] = (pa, piece);
            n += 1;
        }
        va += piece;
    }
    Some(n)
}

pub struct AhciInterface {
    name: String,
    port: Mutex<AhciPort>,
    capacity: u64,
    /// Page-aligned DMA bounce buffer for callers whose buffers are not
    /// page-aligned (`BOUNCE_PAGES` contiguous pages).
    bounce_phys: usize,
    bounce_virt: usize,
    bounce_len: usize,
}

impl AhciInterface {
    /// Reset the HBA once and enumerate **every** port that has a disk,
    /// returning one [`AhciInterface`] per attached SATA device. (The old
    /// `new` returned only the first disk, so Eclipse could never use a
    /// second SATA drive on the same controller.)
    pub fn new_all(base: usize, _irq: usize) -> DeviceResult<Vec<Self>> {
        unsafe {
            // AHCI spec §10.6: BIOS/OS Handoff.
            // On real hardware the BIOS may still own the HBA (BOHC.BOS=1).
            // Request ownership and wait for the BIOS to release before resetting.
            let bohc = read_volatile((base + HBA_BOHC) as *const u32);
            if bohc & BOHC_BOS != 0 {
                crate::klog_info!(
                    "[AHCI] BIOS owns HBA (BOHC={:#x}), requesting handoff",
                    bohc
                );
                write_volatile((base + HBA_BOHC) as *mut u32, bohc | BOHC_OOS);
                // Wait up to 25 ms for BIOS to clear BOS
                wait_until(25_000, || {
                    read_volatile((base + HBA_BOHC) as *const u32) & BOHC_BOS == 0
                });
                // If BIOS is still busy (BB=1) wait up to 2 s more
                if read_volatile((base + HBA_BOHC) as *const u32) & BOHC_BB != 0 {
                    wait_until(2_000_000, || {
                        read_volatile((base + HBA_BOHC) as *const u32) & BOHC_BB == 0
                    });
                }
                crate::klog_info!(
                    "[AHCI] BOHC after handoff: {:#x}",
                    read_volatile((base + HBA_BOHC) as *const u32)
                );
            }

            // Global HBA reset
            write_volatile((base + HBA_GHC) as *mut u32, GHC_AE);
            write_volatile((base + HBA_GHC) as *mut u32, GHC_AE | GHC_HR);
            // Wait for HBA Global Reset to self-clear (spec: ≤1 second)
            if !wait_until(HBA_RESET_TIMEOUT_US, || {
                read_volatile((base + HBA_GHC) as *const u32) & GHC_HR == 0
            }) {
                crate::klog_err!("[AHCI] HBA reset timeout — controller may not be functional");
            }
            // Re-enable AHCI mode (do NOT set GHC_IE — polling mode only)
            write_volatile((base + HBA_GHC) as *mut u32, GHC_AE);
            // Let the HBA and PHY layers stabilise (~1 ms)
            udelay(1_000);
        }

        let pi = unsafe { read_volatile((base + HBA_PI) as *const u32) };

        // After a global HBA reset the SATA link can take up to 2 seconds to
        // re-establish.  Wait until at least one implemented port shows DET≠0
        // before scanning individual ports, so we don't skip them prematurely.
        let _ = wait_until(2_000_000, || {
            for i in 0..32u32 {
                if pi & (1 << i) == 0 {
                    continue;
                }
                let pbase = base + 0x100 + (i as usize * 0x80);
                let det = unsafe { read_volatile((pbase + PORT_SSTS) as *const u32) } & 0xF;
                if det != 0 {
                    return true;
                }
            }
            false
        });

        let mut disks: Vec<Self> = Vec::new();
        for i in 0..32 {
            if pi & (1 << i) != 0 {
                let pbase = base + 0x100 + (i * 0x80);
                let dma_paddr = unsafe { drivers_dma_alloc(DMA_PAGES) };
                let dma_vaddr = phys_to_virt(dma_paddr);

                let mut port = AhciPort {
                    base: pbase,
                    port_idx: i as u32,
                    cl_phys: dma_paddr as u64,
                    cl_virt: dma_vaddr,
                    fb_phys: (dma_paddr + 1024) as u64,
                    _fb_virt: dma_vaddr + 1024,
                    ct_phys: (dma_paddr + 4096) as u64,
                    ct_virt: dma_vaddr + 4096,
                    // Assume LBA48 until IDENTIFY says otherwise; READ/WRITE DMA
                    // EXT is mandatory on every SATA device.
                    lba48: true,
                };

                // Skip ports that are explicitly disabled (DET=4).
                // Do NOT skip DET=0, because link negotiation might not have
                // completed yet or requires a COMRESET to start.
                let det = port.read_reg(PORT_SSTS) & 0xF;
                if det == 4 {
                    unsafe {
                        drivers_dma_dealloc(dma_paddr, DMA_PAGES);
                    }
                    continue;
                }

                if !port.init() {
                    unsafe {
                        drivers_dma_dealloc(dma_paddr, DMA_PAGES);
                    }
                    continue;
                }

                // Wait up to 1 second for PORT_SIG to become valid.
                let mut sig = 0;
                for _ in 0..100 {
                    sig = port.read_reg(PORT_SIG);
                    if sig != 0 && sig != 0xFFFF_FFFF {
                        break;
                    }
                    udelay(10_000); // 10 ms between polls
                }

                // Accept known ATA signature or any non-zero SIG when DET=3
                // (device present, PHY comms up). Some real controllers populate
                // SIG asynchronously and may show 0 even after link-up; IDENTIFY
                // will confirm whether a disk is actually there.
                let is_ata =
                    sig == HBA_SIG_ATA || (sig == 0 && port.read_reg(PORT_SSTS) & 0xF == 3);
                if is_ata {
                    if let Some((sectors, lba48)) = port.identify() {
                        port.lba48 = lba48;
                        if sectors == 0 {
                            crate::klog_warn!(
                                "[AHCI] port {} reported 0 sectors after IDENTIFY; skipping device",
                                i
                            );
                            unsafe {
                                drivers_dma_dealloc(dma_paddr, DMA_PAGES);
                            }
                            continue;
                        }
                        if sectors >= 2097152 {
                            warn!(
                                "ahci{}: SATA disk attached, {} sectors ({} GiB)",
                                i,
                                sectors,
                                sectors / 2097152
                            );
                        } else {
                            warn!(
                                "ahci{}: SATA disk attached, {} sectors ({} MiB)",
                                i,
                                sectors,
                                sectors / 2048
                            );
                        }
                        let bounce_phys = unsafe { drivers_dma_alloc(BOUNCE_PAGES) };
                        if bounce_phys == 0 {
                            unsafe {
                                drivers_dma_dealloc(dma_paddr, DMA_PAGES);
                            }
                            return Err(DeviceError::NoResources);
                        }
                        disks.push(Self {
                            name: format!("ahci-{}", i),
                            port: Mutex::new(port),
                            capacity: sectors,
                            bounce_phys,
                            bounce_virt: phys_to_virt(bounce_phys),
                            bounce_len: BOUNCE_PAGES * 4096,
                        });
                        continue;
                    } else {
                        crate::klog_warn!("[AHCI] port {} IDENTIFY failed", i);
                        unsafe {
                            drivers_dma_dealloc(dma_paddr, DMA_PAGES);
                        }
                    }
                } else {
                    crate::klog_warn!("[AHCI] port {} signature is not ATA: {:#x}", i, sig);
                    unsafe {
                        drivers_dma_dealloc(dma_paddr, DMA_PAGES);
                    }
                }
            }
        }

        if disks.is_empty() {
            Err(DeviceError::NoResources)
        } else {
            Ok(disks)
        }
    }
}

impl AhciInterface {
    /// Validate a request and return the sector count, or `InvalidParam`.
    fn check_request(&self, block_id: usize, len: usize) -> DeviceResult<usize> {
        if len == 0 || len % SECTOR_SIZE != 0 {
            return Err(DeviceError::InvalidParam);
        }
        let nsectors = len / SECTOR_SIZE;
        match block_id.checked_add(nsectors) {
            Some(end) if end as u64 <= self.capacity => Ok(nsectors),
            _ => Err(DeviceError::InvalidParam),
        }
    }

    /// Largest chunk a single command may move on this device.
    fn max_chunk(&self, lba48: bool, path_max: usize) -> usize {
        if lba48 {
            path_max
        } else {
            path_max.min(LBA28_MAX_SECTORS * SECTOR_SIZE)
        }
    }
}

impl BlockScheme for AhciInterface {
    // `block_id` indexes 512-byte sectors; `buf.len()` may be any multiple
    // of 512 and is transferred in as few commands as possible.
    fn read_block(&self, block_id: usize, read_buf: &mut [u8]) -> DeviceResult {
        self.check_request(block_id, read_buf.len())?;
        let port = self.port.lock();
        let mut lba = block_id as u64;
        let mut offset = 0usize;
        while offset < read_buf.len() {
            let remaining = read_buf.len() - offset;
            let ptr = unsafe { read_buf.as_ptr().add(offset) } as usize;
            let mut prds = [(0u64, 0usize); PRDT_MAX];
            let mut chunk = 0usize;
            if ptr % 4096 == 0 && false {
                // Zero-copy: DMA straight into the caller's buffer, page by page.
                let want = remaining.min(self.max_chunk(port.lba48, DIRECT_MAX_BYTES));
                if let Some(n) = build_prds(ptr, want, &mut prds) {
                    port.rw_block(lba, &prds[..n], false)?;
                    chunk = want;
                }
            }
            if chunk == 0 {
                chunk = remaining.min(self.max_chunk(port.lba48, self.bounce_len));
                port.rw_block(lba, &[(self.bounce_phys as u64, chunk)], false)?;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        self.bounce_virt as *const u8,
                        read_buf.as_mut_ptr().add(offset),
                        chunk,
                    );
                }
            }
            offset += chunk;
            lba += (chunk / SECTOR_SIZE) as u64;
        }
        Ok(())
    }

    fn write_block(&self, block_id: usize, write_buf: &[u8]) -> DeviceResult {
        self.check_request(block_id, write_buf.len())?;
        let port = self.port.lock();
        let mut lba = block_id as u64;
        let mut offset = 0usize;
        while offset < write_buf.len() {
            let remaining = write_buf.len() - offset;
            let ptr = unsafe { write_buf.as_ptr().add(offset) } as usize;
            let mut prds = [(0u64, 0usize); PRDT_MAX];
            let mut chunk = 0usize;
            if ptr % 4096 == 0 && false {
                let want = remaining.min(self.max_chunk(port.lba48, DIRECT_MAX_BYTES));
                if let Some(n) = build_prds(ptr, want, &mut prds) {
                    port.rw_block(lba, &prds[..n], true)?;
                    chunk = want;
                }
            }
            if chunk == 0 {
                chunk = remaining.min(self.max_chunk(port.lba48, self.bounce_len));
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        write_buf.as_ptr().add(offset),
                        self.bounce_virt as *mut u8,
                        chunk,
                    );
                }
                port.rw_block(lba, &[(self.bounce_phys as u64, chunk)], true)?;
            }
            offset += chunk;
            lba += (chunk / SECTOR_SIZE) as u64;
        }
        Ok(())
    }

    fn flush(&self) -> DeviceResult {
        self.port.lock().flush_cache()
    }

    fn block_count(&self) -> usize {
        self.capacity as usize
    }
}

impl Scheme for AhciInterface {
    fn name(&self) -> &str {
        &self.name
    }

    fn handle_irq(&self, _irq: usize) {}
}

pub struct AhciDriverPci;

impl PciDriver for AhciDriverPci {
    fn name(&self) -> &str {
        "ahci"
    }

    fn matched(&self, _vendor_id: u16, _device_id: u16) -> bool {
        false
    }

    fn matched_dev(&self, dev: &PCIDevice) -> bool {
        // Match standard AHCI (SATA, IDE, or RAID subclass in AHCI mode):
        // class=0x01 (mass storage), prog_if=0x01 (AHCI)
        dev.id.class == 0x01
            && dev.id.prog_if == 0x01
            && (dev.id.subclass == 0x06 || dev.id.subclass == 0x01 || dev.id.subclass == 0x04)
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        // AHCI ABAR lives in BAR5 (config offset 0x24). Some firmware/VMs (e.g.
        // VirtualBox) leave `dev.bars[5]` empty even though the register is valid.
        let (addr, len) = if let Some(BAR::Memory(a, l, _, _)) = dev.bars[5] {
            (a as usize, l as usize)
        } else {
            #[cfg(target_arch = "x86_64")]
            {
                use crate::bus::pci::{read_bar_addr, PortOpsImpl, BAR5_REG, PCI_ACCESS};
                let a =
                    unsafe { read_bar_addr(&PortOpsImpl, PCI_ACCESS, dev.loc, BAR5_REG) as usize };
                if a == 0 {
                    return Err(DeviceError::NotSupported);
                }
                (a, 4096 * 8)
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                return Err(DeviceError::NotSupported);
            }
        };

        if addr == 0 {
            return Err(DeviceError::NotSupported);
        }

        let map_len = len.max(4096 * 8);

        if let Some(m) = mapper {
            m.query_or_map(addr, map_len);
        }

        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        unsafe {
            let ops = &crate::bus::pci::PortOpsImpl;
            let am = crate::bus::pci::PCI_ACCESS;
            let pci_command = am.read16(ops, dev.loc, 0x04);
            am.write16(ops, dev.loc, 0x04, pci_command | 0x0004 | 0x0002 | 0x0001);
        }

        let vaddr = phys_to_virt(addr);
        let vector = irq.map(|idx| idx + 32).unwrap_or(33);
        let mut disks = AhciInterface::new_all(vaddr, vector)?;
        // The PCI probe framework returns a single `Device` per function, but
        // one AHCI controller can host several disks. Return the first here and
        // stash the rest; `pci::init` drains them after the bus scan so each
        // SATA disk is registered as its own block device.
        let first = disks.remove(0);
        for extra in disks {
            EXTRA_DISKS.lock().push(Device::Block(Arc::new(extra)));
        }
        Ok(Device::Block(Arc::new(first)))
    }
}

/// Additional SATA disks found on AHCI controllers beyond the first one on each
/// controller. The PCI probe returns a single `Device` per function, so the
/// extra disks are parked here and drained by `pci::init` after the bus scan.
static EXTRA_DISKS: Mutex<Vec<Device>> = Mutex::new(Vec::new());

/// Take (and clear) the AHCI disks discovered beyond the first per controller.
pub fn take_extra_disks() -> Vec<Device> {
    core::mem::take(&mut *EXTRA_DISKS.lock())
}
