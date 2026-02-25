//! AHCI (Advanced Host Controller Interface) Driver
//!
//! Direct MMIO implementation for SATA disk access on real hardware.
//! Supports Intel PCH SATA controllers (200 Series and others) and any
//! AHCI 1.0+ compliant HBA.
//!
//! Each active SATA port is registered as an independent block device
//! in the storage registry (disk:0, disk:1, …).
//!
//! References: AHCI 1.3.1 specification, ATA/ATAPI-8 ACS-3.

use spin::Mutex;
use alloc::sync::Arc;
use crate::{memory, serial, pci};

// ── HBA global register offsets (from ABAR) ──────────────────────────────────
const HBA_CAP:  usize = 0x00;  // Host Capabilities
const HBA_GHC:  usize = 0x04;  // Global Host Control
#[allow(dead_code)]
const HBA_IS:   usize = 0x08;  // Interrupt Status (pending ports) — written to clear
const HBA_PI:   usize = 0x0C;  // Ports Implemented bitmask
const HBA_VS:   usize = 0x10;  // Version

// GHC bits
const GHC_AE: u32 = 1 << 31;  // AHCI Enable
const GHC_HR: u32 = 1 << 0;   // HBA Reset

// ── Per-port register offsets (from port base = ABAR + 0x100 + port*0x80) ────
const PORT_CLB:  usize = 0x00;  // Command List Base (low 32-bit)
const PORT_CLBU: usize = 0x04;  // Command List Base (high 32-bit)
const PORT_FB:   usize = 0x08;  // FIS Base (low 32-bit)
const PORT_FBU:  usize = 0x0C;  // FIS Base (high 32-bit)
const PORT_IS:   usize = 0x10;  // Interrupt Status
#[allow(dead_code)]
const PORT_IE:   usize = 0x14;  // Interrupt Enable (not used; polling mode)
const PORT_CMD:  usize = 0x18;  // Command and Status
const PORT_TFD:  usize = 0x20;  // Task File Data
#[allow(dead_code)]
const PORT_SIG:  usize = 0x24;  // Signature (ATA=0x00000101, ATAPI=0xEB140101)
const PORT_SSTS: usize = 0x28;  // SATA Status  (DET / SPD / IPM)
const PORT_SCTL: usize = 0x2C;  // SATA Control (COMRESET, speed, ALPM, etc.)
const PORT_SERR: usize = 0x30;  // SATA Error
#[allow(dead_code)]
const PORT_SACT: usize = 0x34;  // SATA Active (NCQ — not used in this driver)
const PORT_CI:   usize = 0x38;  // Command Issue

// PORT_CMD bits
const CMD_ST:  u32 = 1 << 0;   // Start (command engine)
const CMD_SUD: u32 = 1 << 1;   // Spin-Up Device
const CMD_POD: u32 = 1 << 2;   // Power-On Device
const CMD_FRE: u32 = 1 << 4;   // FIS Receive Enable
const CMD_FR:  u32 = 1 << 14;  // FIS Receive Running (read-only)
const CMD_CR:  u32 = 1 << 15;  // Command List Running (read-only)

// PORT_IS error bits that abort the command
const IS_TFES: u32 = 1 << 30;  // Task File Error Status
const IS_HBFS: u32 = 1 << 29;  // Host Bus Fatal Error
const IS_HBDS: u32 = 1 << 28;  // Host Bus Data Error
const IS_IFS:  u32 = 1 << 27;  // Interface Fatal Error
const IS_ERRORS: u32 = IS_TFES | IS_HBFS | IS_HBDS | IS_IFS;

// ATA commands
const ATA_CMD_READ_DMA_EXT:  u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_IDENTIFY:      u8 = 0xEC;

// FIS types
const FIS_TYPE_REG_H2D: u8 = 0x27;

// Block dimensions
/// 512-byte ATA sectors
const SECTOR_SIZE: usize = 512;
/// Eclipse OS block size (4 KiB = 8 sectors)
pub const AHCI_BLOCK_SIZE: usize = 4096;
const SECTORS_PER_BLOCK: u64 = (AHCI_BLOCK_SIZE / SECTOR_SIZE) as u64;

// ── AHCI DMA structures (must match AHCI spec layout exactly) ────────────────

/// Command Header — 32 bytes.  32 headers form the 1 KiB Command List.
#[repr(C)]
struct CommandHeader {
    /// [5:0] CFL (FIS length in DWORDs), [6] ATAPI, [7] Write, [8] Prefetch
    flags:    u16,
    /// Number of PRD entries in the attached Command Table
    prdtl:    u16,
    /// PRD Byte Count — set by the HBA after the command completes
    prdbc:    u32,
    /// Command Table Base Address, low 32 bits (must be 128-byte aligned)
    ctba:     u32,
    /// Command Table Base Address, high 32 bits
    ctbau:    u32,
    reserved: [u32; 4],
}

/// Physical Region Descriptor (PRD) entry — 16 bytes.
#[repr(C)]
struct PrdEntry {
    dba:     u32,  // Data Buffer Base Address (low)
    dbau:    u32,  // Data Buffer Base Address (high)
    _rsvd:   u32,
    /// [21:0] = byte_count − 1.  Bit 31 = Interrupt on Completion (IOC).
    dbc_ioc: u32,
}

/// Command Table for a single command.
/// We always use exactly 1 PRD entry → total ≤ 256 bytes (128-byte aligned).
#[repr(C)]
struct CommandTable {
    cfis:  [u8; 64],       // Command FIS (only first 20 bytes used for H2D)
    acmd:  [u8; 16],       // ATAPI command (unused for regular ATA)
    _rsvd: [u8; 48],       // Reserved
    prdt:  [PrdEntry; 1],  // 1 × PRD entry
}

// ── Per-port DMA allocation ──────────────────────────────────────────────────

struct PortDma {
    // Command List: 32 × 32 bytes = 1 KiB, must be 1 KiB-aligned.
    cmd_list_virt: u64,
    cmd_list_phys: u64,
    // FIS receive buffer: 256 bytes, must be 256-byte aligned.
    fis_virt: u64,
    fis_phys: u64,
    // Command Table: sizeof(CommandTable) ≈ 144 bytes, must be 128-byte aligned.
    cmd_table_virt: u64,
    cmd_table_phys: u64,
}

// ── AhciPort — owns the MMIO registers and DMA structures for one SATA port ──

struct AhciPort {
    base:       u64,    // Virtual address of port MMIO registers
    dma:        PortDma,
    port_index: u32,
}

// Safety: AhciPort is accessed exclusively through Mutex<AhciDisk>.
unsafe impl Send for AhciPort {}
unsafe impl Sync for AhciPort {}

impl AhciPort {
    // ── Low-level register helpers ──

    #[inline]
    fn preg(&self, offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.base + offset as u64) as *const u32) }
    }

    #[inline]
    fn pwreg(&self, offset: usize, val: u32) {
        unsafe { core::ptr::write_volatile((self.base + offset as u64) as *mut u32, val) }
    }

    // ── Command engine control ──

    /// Stop command engine and FIS receive DMA.
    /// Must be called before reconfiguring CLB / FB registers.
    fn cmd_engine_stop(&self) {
        // Clear ST (stop command engine)
        self.pwreg(PORT_CMD, self.preg(PORT_CMD) & !CMD_ST);
        // Wait for CR (command list running) to clear — max ~500 ms
        let mut i = 5_000_000u32;
        while self.preg(PORT_CMD) & CMD_CR != 0 {
            if i == 0 { break; }
            i -= 1;
            core::hint::spin_loop();
        }
        // Clear FRE (disable FIS receive)
        self.pwreg(PORT_CMD, self.preg(PORT_CMD) & !CMD_FRE);
        // Wait for FR (FIS receive running) to clear
        let mut i = 5_000_000u32;
        while self.preg(PORT_CMD) & CMD_FR != 0 {
            if i == 0 { break; }
            i -= 1;
            core::hint::spin_loop();
        }
    }

    /// Enable FIS receive, then start the command engine.
    fn cmd_engine_start(&self) {
        // Ensure CR is already clear before asserting ST
        let mut i = 5_000_000u32;
        while self.preg(PORT_CMD) & CMD_CR != 0 {
            if i == 0 { break; }
            i -= 1;
            core::hint::spin_loop();
        }
        self.pwreg(PORT_CMD, self.preg(PORT_CMD) | CMD_FRE);
        self.pwreg(PORT_CMD, self.preg(PORT_CMD) | CMD_ST);
    }

    // ── Port initialization ──

    /// Full port initialisation sequence (AHCI spec §10.1.2).
    /// Returns `true` if a device is connected and the port is ready.
    fn init(&self) -> bool {
        // 1. Bring engine to idle
        self.cmd_engine_stop();

        // 2. Program Command List and FIS receive buffers
        self.pwreg(PORT_CLB,  (self.dma.cmd_list_phys & 0xFFFF_FFFF) as u32);
        self.pwreg(PORT_CLBU, (self.dma.cmd_list_phys >> 32) as u32);
        self.pwreg(PORT_FB,   (self.dma.fis_phys & 0xFFFF_FFFF) as u32);
        self.pwreg(PORT_FBU,  (self.dma.fis_phys >> 32) as u32);

        // 3. Clear all pending interrupts and errors
        self.pwreg(PORT_IS,   0xFFFF_FFFF);
        self.pwreg(PORT_SERR, 0xFFFF_FFFF);

        // 4. Power on / spin up (for hot-plug capable controllers)
        let cmd = self.preg(PORT_CMD) | CMD_POD | CMD_SUD;
        self.pwreg(PORT_CMD, cmd);

        // 5. Wake sleeping port: disable ALPM and assert COMRESET if DET≠3
        let sctl = self.preg(PORT_SCTL);
        // Set IPM=3 (bits [11:8] = 0x300) to disable Partial and Slumber transitions.
        // This prevents Intel AHCI from putting the link to sleep via ALPM,
        // which would otherwise cause DET to go non-3 under light traffic.
        self.pwreg(PORT_SCTL, (sctl & !0x0F00) | 0x0300);

        // If DET is not 3 (no PHY comms), issue a COMRESET to kick the link.
        let det = self.preg(PORT_SSTS) & 0x0F;
        if det != 3 {
            // Assert COMRESET: DET=1 in PxSCTL
            self.pwreg(PORT_SCTL, (self.preg(PORT_SCTL) & !0x0F) | 0x01);
            // Hold for ≥1 ms
            for _ in 0..1_000_000 { core::hint::spin_loop(); }
            // De-assert COMRESET: DET=0
            self.pwreg(PORT_SCTL, self.preg(PORT_SCTL) & !0x0F);
        }

        // 6. Wait for DET=3, IPM=1 (device present, active, PHY comms up) — up to 3 s
        let mut ready = false;
        let mut i = 30_000_000u32;
        loop {
            let ssts = self.preg(PORT_SSTS);
            if ssts & 0x0F == 3 {
                ready = true;
                break;
            }
            if i == 0 { break; }
            i -= 1;
            core::hint::spin_loop();
        }

        if !ready {
            serial::serial_print("[AHCI] Port ");
            serial::serial_print_dec(self.port_index as u64);
            serial::serial_print(": no device after init (SSTS=");
            serial::serial_print_hex(self.preg(PORT_SSTS) as u64);
            serial::serial_print(")\n");
            return false;
        }

        // 7. Clear any errors that accumulated during bring-up
        self.pwreg(PORT_SERR, 0xFFFF_FFFF);
        self.pwreg(PORT_IS,   0xFFFF_FFFF);

        // 8. Start command engine
        self.cmd_engine_start();

        true
    }

    // ── Command issue / polling ──

    /// Issue the command already built in slot 0 and spin-poll to completion.
    /// Returns `true` on success.
    fn exec_cmd(&self) -> bool {
        // Full memory barrier before touching the CI register
        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)); }

        // Clear interrupt status
        self.pwreg(PORT_IS, 0xFFFF_FFFF);

        // Issue slot 0
        self.pwreg(PORT_CI, 1);

        // Poll until the HBA clears bit 0 in PxCI (command consumed) — ~5 s
        let mut i = 50_000_000u32;
        loop {
            if self.preg(PORT_CI) & 1 == 0 { break; }

            let is = self.preg(PORT_IS);
            if is & IS_ERRORS != 0 {
                serial::serial_print("[AHCI] Port ");
                serial::serial_print_dec(self.port_index as u64);
                serial::serial_print(" error IS=");
                serial::serial_print_hex(is as u64);
                serial::serial_print(" TFD=");
                serial::serial_print_hex(self.preg(PORT_TFD) as u64);
                serial::serial_print("\n");
                self.pwreg(PORT_IS, 0xFFFF_FFFF);
                return false;
            }

            if i == 0 {
                serial::serial_print("[AHCI] Port ");
                serial::serial_print_dec(self.port_index as u64);
                serial::serial_print(" command timeout\n");
                return false;
            }
            i -= 1;
            core::hint::spin_loop();
        }

        // Re-check task file error after completion
        if self.preg(PORT_IS) & IS_TFES != 0 {
            serial::serial_print("[AHCI] Port ");
            serial::serial_print_dec(self.port_index as u64);
            serial::serial_print(" task file error after completion\n");
            return false;
        }

        true
    }

    // ── DMA read / write ──

    /// Build the H2D FIS + PRD entry + command header for slot 0, then
    /// issue and poll to completion.
    fn rw_dma(&self, lba: u64, buf_phys: u64, buf_len: usize, write: bool) -> bool {
        let sector_count = (buf_len / SECTOR_SIZE) as u32;
        if sector_count == 0 || sector_count > 65535 {
            return false;
        }

        unsafe {
            let cmd_list  = self.dma.cmd_list_virt  as *mut CommandHeader;
            let cmd_table = self.dma.cmd_table_virt as *mut CommandTable;

            // Zero command table
            core::ptr::write_bytes(cmd_table as *mut u8, 0,
                core::mem::size_of::<CommandTable>());

            // ── H2D Register FIS (20 bytes = 5 DWORDs) ──────────────────
            let f = (*cmd_table).cfis.as_mut_ptr();
            *f.add(0)  = FIS_TYPE_REG_H2D;
            *f.add(1)  = 0x80; // C=1: this is a command (not control) update
            *f.add(2)  = if write { ATA_CMD_WRITE_DMA_EXT } else { ATA_CMD_READ_DMA_EXT };
            *f.add(3)  = 0;    // features low
            *f.add(4)  =  lba        as u8; // LBA[7:0]
            *f.add(5)  = (lba >>  8) as u8; // LBA[15:8]
            *f.add(6)  = (lba >> 16) as u8; // LBA[23:16]
            *f.add(7)  = 0x40;              // Device: LBA-mode (bit 6)
            *f.add(8)  = (lba >> 24) as u8; // LBA[31:24]
            *f.add(9)  = (lba >> 32) as u8; // LBA[39:32]
            *f.add(10) = (lba >> 40) as u8; // LBA[47:40]
            *f.add(11) = 0;    // features high
            *f.add(12) =  sector_count       as u8; // count low
            *f.add(13) = (sector_count >> 8) as u8; // count high
            *f.add(14) = 0;    // ICC
            *f.add(15) = 0;    // control

            // ── PRD entry ────────────────────────────────────────────────
            (*cmd_table).prdt[0].dba     = (buf_phys & 0xFFFF_FFFF) as u32;
            (*cmd_table).prdt[0].dbau    = (buf_phys >> 32) as u32;
            (*cmd_table).prdt[0]._rsvd   = 0;
            // DBC = byte_count − 1; IOC = 1 (interrupt on completion — ignored in polling mode)
            (*cmd_table).prdt[0].dbc_ioc = (buf_len as u32 - 1) | (1 << 31);

            // ── Command Header in slot 0 ─────────────────────────────────
            // CFL = 5 (H2D FIS is 20 bytes / 4 = 5 DWORDs)
            // W = 1 for writes, 0 for reads
            let w_bit: u16 = if write { 1 << 6 } else { 0 };
            (*cmd_list).flags    = 5 | w_bit;
            (*cmd_list).prdtl    = 1;
            (*cmd_list).prdbc    = 0;
            (*cmd_list).ctba     = (self.dma.cmd_table_phys & 0xFFFF_FFFF) as u32;
            (*cmd_list).ctbau    = (self.dma.cmd_table_phys >> 32) as u32;
            (*cmd_list).reserved = [0; 4];
        }

        self.exec_cmd()
    }

    /// Read `buffer.len()` bytes (must be a multiple of 512) starting at `lba`.
    pub fn read_sectors(&self, lba: u64, buffer: &mut [u8]) -> bool {
        if buffer.len() % SECTOR_SIZE != 0 { return false; }
        let phys = memory::virt_to_phys(buffer.as_ptr() as u64);
        self.rw_dma(lba, phys, buffer.len(), false)
    }

    /// Write `buffer.len()` bytes (must be a multiple of 512) starting at `lba`.
    pub fn write_sectors(&self, lba: u64, buffer: &[u8]) -> bool {
        if buffer.len() % SECTOR_SIZE != 0 { return false; }
        let phys = memory::virt_to_phys(buffer.as_ptr() as u64);
        self.rw_dma(lba, phys, buffer.len(), true)
    }

    // ── IDENTIFY DEVICE ──────────────────────────────────────────────────────

    /// Issue ATA IDENTIFY DEVICE and return the total LBA-48 sector count.
    /// Returns `None` if IDENTIFY fails or returns zero.
    pub fn identify(&self) -> Option<u64> {
        // Allocate a 512-byte, 512-byte-aligned DMA buffer
        let (id_virt, id_phys) = memory::alloc_dma_buffer(SECTOR_SIZE, SECTOR_SIZE)?;
        unsafe { core::ptr::write_bytes(id_virt, 0, SECTOR_SIZE); }

        unsafe {
            let cmd_list  = self.dma.cmd_list_virt  as *mut CommandHeader;
            let cmd_table = self.dma.cmd_table_virt as *mut CommandTable;

            core::ptr::write_bytes(cmd_table as *mut u8, 0,
                core::mem::size_of::<CommandTable>());

            let f = (*cmd_table).cfis.as_mut_ptr();
            *f.add(0) = FIS_TYPE_REG_H2D;
            *f.add(1) = 0x80;               // C=1
            *f.add(2) = ATA_CMD_IDENTIFY;
            *f.add(7) = 0xA0;               // Drive select

            (*cmd_table).prdt[0].dba     = (id_phys & 0xFFFF_FFFF) as u32;
            (*cmd_table).prdt[0].dbau    = (id_phys >> 32) as u32;
            (*cmd_table).prdt[0]._rsvd   = 0;
            (*cmd_table).prdt[0].dbc_ioc = (SECTOR_SIZE as u32 - 1) | (1 << 31);

            (*cmd_list).flags    = 5;       // CFL=5, W=0
            (*cmd_list).prdtl    = 1;
            (*cmd_list).prdbc    = 0;
            (*cmd_list).ctba     = (self.dma.cmd_table_phys & 0xFFFF_FFFF) as u32;
            (*cmd_list).ctbau    = (self.dma.cmd_table_phys >> 32) as u32;
            (*cmd_list).reserved = [0; 4];
        }

        if !self.exec_cmd() {
            return None;
        }

        // IDENTIFY data: words 100–103 hold the 48-bit LBA max address (little-endian)
        // Word 100 is at byte offset 200, word 103 at byte offset 206.
        let id = unsafe { core::slice::from_raw_parts(id_virt as *const u8, SECTOR_SIZE) };
        let lba48 = u64::from_le_bytes([
            id[200], id[201], id[202], id[203],
            id[204], id[205], id[206], id[207],
        ]);

        if lba48 == 0 { None } else { Some(lba48) }
    }
}

// ── AhciDisk — implements storage::BlockDevice for one AHCI port ─────────────

struct AhciDisk {
    port:     Mutex<AhciPort>,
    capacity: u64,  // in 4 KiB blocks
}

impl crate::storage::BlockDevice for AhciDisk {
    fn read(&self, block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        let lba = block * SECTORS_PER_BLOCK;
        if self.port.lock().read_sectors(lba, buffer) { Ok(()) }
        else { Err("AHCI read failed") }
    }

    fn write(&self, block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        let lba = block * SECTORS_PER_BLOCK;
        if self.port.lock().write_sectors(lba, buffer) { Ok(()) }
        else { Err("AHCI write failed") }
    }

    fn capacity(&self) -> u64 { self.capacity }

    fn name(&self) -> &'static str { "AHCI" }
}

// ── DMA helpers ──────────────────────────────────────────────────────────────

/// Allocate a zero-initialised, physically-contiguous DMA buffer.
/// Panics if the heap is exhausted (unrecoverable at this init stage).
fn alloc_dma(size: usize, align: usize) -> (u64, u64) {
    match memory::alloc_dma_buffer(size, align) {
        Some((ptr, phys)) => {
            unsafe { core::ptr::write_bytes(ptr, 0, size); }
            (ptr as u64, phys)
        }
        None => panic!("AHCI: DMA allocation failed (OOM)"),
    }
}

// ── HBA MMIO helpers (non-port) ──────────────────────────────────────────────

#[inline]
fn hreg(base: u64, offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + offset as u64) as *const u32) }
}

#[inline]
fn hwreg(base: u64, offset: usize, val: u32) {
    unsafe { core::ptr::write_volatile((base + offset as u64) as *mut u32, val) }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Initialise AHCI for every SATA controller found by PCI enumeration.
/// Registers each active port as an independent block device.
pub fn init() {
    let controllers = pci::find_all_sata_ahci();
    if controllers.is_empty() {
        serial::serial_print("[AHCI] No SATA AHCI controllers found on PCI bus\n");
        return;
    }
    for (idx, dev) in controllers.iter().enumerate() {
        serial::serial_print("[AHCI] Initialising controller ");
        serial::serial_print_dec(idx as u64);
        serial::serial_print(" (PCI ");
        serial::serial_print_dec(dev.bus as u64);
        serial::serial_print(":");
        serial::serial_print_dec(dev.device as u64);
        serial::serial_print(".");
        serial::serial_print_dec(dev.function as u64);
        serial::serial_print(")\n");
        init_controller(dev);
    }
}

fn init_controller(dev: &pci::PciDevice) {
    // ── 1. Locate the AHCI Base Address Register (ABAR = BAR5) ──────────────
    // AHCI spec §3.1: "The AHCI memory registers are mapped at BAR5."
    let abar_raw = unsafe { pci::get_bar(dev, 5) };
    // Mask PCI BAR attribute bits [3:0]
    let abar = abar_raw & !0xFu64;
    if abar == 0 {
        serial::serial_print("[AHCI] BAR5 is zero — skipping controller\n");
        return;
    }

    serial::serial_print("[AHCI] ABAR phys=");
    serial::serial_print_hex(abar);
    serial::serial_print("\n");

    // ── 2. Enable PCI Bus-Master DMA and Memory decode ───────────────────────
    unsafe { pci::enable_device(dev, true); }

    // ── 3. Map AHCI MMIO region into the kernel virtual address space ────────
    // AHCI register space: 0x100 (global) + 32 ports × 0x80 = 0x1100 bytes.
    // We map 8 KiB to be safe.
    let base = memory::map_mmio_range(abar, 0x2000);
    if base == 0 {
        serial::serial_print("[AHCI] MMIO mapping failed — skipping controller\n");
        return;
    }

    // ── 4. Global HBA Reset (AHCI spec §10.4.3) ──────────────────────────────
    // First assert AHCI Enable so we can write the GHC register at all.
    hwreg(base, HBA_GHC, GHC_AE);
    // Assert HBA Reset
    hwreg(base, HBA_GHC, GHC_AE | GHC_HR);
    // Spec: HR must clear within 1 second.
    let mut timeout = 10_000_000u32;
    while hreg(base, HBA_GHC) & GHC_HR != 0 {
        if timeout == 0 {
            serial::serial_print("[AHCI] HBA reset timed out — skipping controller\n");
            return;
        }
        timeout -= 1;
        core::hint::spin_loop();
    }
    // Re-assert AHCI Enable (some controllers clear AE on reset)
    hwreg(base, HBA_GHC, GHC_AE);

    // Let the HBA and PHY layers stabilise (~1 ms on real hardware)
    for _ in 0..1_000_000 { core::hint::spin_loop(); }

    // ── 5. Read capabilities and port bitmask ────────────────────────────────
    let cap = hreg(base, HBA_CAP);
    let pi  = hreg(base, HBA_PI);
    let vs  = hreg(base, HBA_VS);

    serial::serial_print("[AHCI] Version=");
    serial::serial_print_hex(vs as u64);
    serial::serial_print(" CAP=");
    serial::serial_print_hex(cap as u64);
    serial::serial_print(" PI=");
    serial::serial_print_hex(pi as u64);
    serial::serial_print("\n");

    // ── 6. Initialise every implemented port ─────────────────────────────────
    let mut registered = 0usize;

    for port_idx in 0u32..32 {
        if pi & (1 << port_idx) == 0 { continue; }

        let port_base = base + 0x100 + (port_idx as u64 * 0x80);
        let ssts = unsafe {
            core::ptr::read_volatile((port_base + PORT_SSTS as u64) as *const u32)
        };
        let det = ssts & 0x0F;

        serial::serial_print("[AHCI] Port ");
        serial::serial_print_dec(port_idx as u64);
        serial::serial_print(" SSTS=");
        serial::serial_print_hex(ssts as u64);
        serial::serial_print("\n");

        // DET=0: no device.  DET=4: offline.  DET=1 or 3: present (init will sort it out).
        if det == 0 || det == 4 { continue; }

        // Allocate aligned DMA structures for this port
        let (cl_virt, cl_phys) = alloc_dma(1024, 1024); // 1 KiB command list
        let (fis_virt, fis_phys) = alloc_dma(256, 256);  // 256 B FIS buffer
        // Command table: one PRD entry → sizeof = 128 + 16 = 144 bytes; align to 128.
        let ct_size = core::mem::size_of::<CommandTable>();
        let (ct_virt, ct_phys) = alloc_dma(ct_size.max(128), 128);

        let port = AhciPort {
            base: port_base,
            dma: PortDma {
                cmd_list_virt: cl_virt,
                cmd_list_phys: cl_phys,
                fis_virt,
                fis_phys,
                cmd_table_virt: ct_virt,
                cmd_table_phys: ct_phys,
            },
            port_index: port_idx,
        };

        if !port.init() {
            serial::serial_print("[AHCI] Port ");
            serial::serial_print_dec(port_idx as u64);
            serial::serial_print(" init failed — skipping\n");
            continue;
        }

        // IDENTIFY to get real capacity
        let capacity_sectors = port.identify().unwrap_or(0);
        let capacity_blocks = if capacity_sectors > 0 {
            capacity_sectors / SECTORS_PER_BLOCK
        } else {
            // IDENTIFY failed or returned zero; use max to avoid blocking reads
            u64::MAX / SECTORS_PER_BLOCK
        };

        serial::serial_print("[AHCI] Port ");
        serial::serial_print_dec(port_idx as u64);
        serial::serial_print(" ready — ");
        serial::serial_print_dec(capacity_sectors);
        serial::serial_print(" sectors (");
        serial::serial_print_dec(capacity_sectors / 2048); // MiB
        serial::serial_print(" MiB)\n");

        let disk: Arc<dyn crate::storage::BlockDevice> = Arc::new(AhciDisk {
            port:     Mutex::new(port),
            capacity: capacity_blocks,
        });
        crate::storage::register_device(disk);
        registered += 1;
    }

    serial::serial_print("[AHCI] Controller done — registered ");
    serial::serial_print_dec(registered as u64);
    serial::serial_print(" disk(s)\n");
}

// ── Legacy helpers (kept for any caller that used the old API) ────────────────

/// Read one 4 KiB block from the first registered storage device.
pub fn read_block(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    if buffer.len() != AHCI_BLOCK_SIZE {
        return Err("Buffer must be 4096 bytes");
    }
    crate::storage::get_device(0)
        .ok_or("No AHCI device registered")?
        .read(block_num, buffer)
}

/// Write one 4 KiB block to the first registered storage device.
pub fn write_block(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    if buffer.len() != AHCI_BLOCK_SIZE {
        return Err("Buffer must be 4096 bytes");
    }
    crate::storage::get_device(0)
        .ok_or("No AHCI device registered")?
        .write(block_num, buffer)
}

/// Returns `true` if at least one storage device has been registered.
pub fn is_available() -> bool {
    crate::storage::device_count() > 0
}
