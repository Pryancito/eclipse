//! AHCI (Advanced Host Controller Interface) Driver
//!
//! Direct MMIO implementation for SATA disk access on real hardware.
//! Supports Intel PCH SATA controllers (200 Series and others) and any
//! AHCI 1.0+ compliant HBA.
//!
//! Each active SATA port is registered as an independent block device
//! in the storage registry (disk:0, disk:1, …).
//!
//! References:
//!   - AHCI 1.3.1 specification
//!   - ATA/ATAPI-8 ACS-3
//!   - Redox OS ahcid driver (MIT License)

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
#[allow(dead_code)]
const HBA_CAP2: usize = 0x24;  // Host Capabilities Extended
#[allow(dead_code)]
const HBA_BOHC: usize = 0x28;  // BIOS/OS Handoff Control

// GHC bits
const GHC_AE: u32 = 1 << 31;  // AHCI Enable
/// GHC.IE would enable PCI interrupt generation, but we use polling mode and
/// intentionally do NOT set this bit to prevent IRQ storms on real hardware.
#[allow(dead_code)]
const GHC_IE: u32 = 1 << 1;   // Interrupt Enable (global) — NOT used (polling mode)
const GHC_HR: u32 = 1 << 0;   // HBA Reset

// ── Per-port register offsets (from port base = ABAR + 0x100 + port*0x80) ────
const PORT_CLB:  usize = 0x00;  // Command List Base (low 32-bit)
const PORT_CLBU: usize = 0x04;  // Command List Base (high 32-bit)
const PORT_FB:   usize = 0x08;  // FIS Base (low 32-bit)
const PORT_FBU:  usize = 0x0C;  // FIS Base (high 32-bit)
const PORT_IS:   usize = 0x10;  // Interrupt Status
const PORT_IE:   usize = 0x14;  // Interrupt Enable
const PORT_CMD:  usize = 0x18;  // Command and Status
const PORT_TFD:  usize = 0x20;  // Task File Data
const PORT_SIG:  usize = 0x24;  // Signature (ATA=0x00000101, ATAPI=0xEB140101)
const PORT_SSTS: usize = 0x28;  // SATA Status  (DET / SPD / IPM)
const PORT_SCTL: usize = 0x2C;  // SATA Control (COMRESET, speed, ALPM, etc.)
const PORT_SERR: usize = 0x30;  // SATA Error
#[allow(dead_code)]
const PORT_SACT: usize = 0x34;  // SATA Active (NCQ)
const PORT_CI:   usize = 0x38;  // Command Issue

// PORT_CMD bits
const CMD_ST:  u32 = 1 << 0;   // Start (command engine)
const CMD_SUD: u32 = 1 << 1;   // Spin-Up Device
const CMD_POD: u32 = 1 << 2;   // Power-On Device
const CMD_FRE: u32 = 1 << 4;   // FIS Receive Enable
const CMD_FR:  u32 = 1 << 14;  // FIS Receive Running (read-only)
const CMD_CR:  u32 = 1 << 15;  // Command List Running (read-only)

// PORT_IS error bits
const IS_TFES: u32 = 1 << 30;  // Task File Error Status
const IS_HBFS: u32 = 1 << 29;  // Host Bus Fatal Error
const IS_HBDS: u32 = 1 << 28;  // Host Bus Data Error
const IS_IFS:  u32 = 1 << 27;  // Interface Fatal Error
const IS_ERRORS: u32 = IS_TFES | IS_HBFS | IS_HBDS | IS_IFS;

// PORT_IE bits that we enable (mirrors IS error bits + Device-to-Host Register FIS)
// 0b10111 = DHRS(0) | PSS(1) | DSS(2) | SDBS(3) | UFS(4) → same as Redox
const PORT_IE_MASK: u32 = 0b10111;

// ATA commands
const ATA_CMD_READ_DMA_EXT:     u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT:    u8 = 0x35;
const ATA_CMD_IDENTIFY:         u8 = 0xEC;
const ATA_CMD_IDENTIFY_PACKET:  u8 = 0xA1;
#[allow(dead_code)]
const ATA_CMD_PACKET:           u8 = 0xA0;

// ATA device status bits
const ATA_DEV_BUSY: u8 = 0x80;  // BSY
const ATA_DEV_DRQ:  u8 = 0x08;  // DRQ

// FIS types
const FIS_TYPE_REG_H2D: u8 = 0x27;

// HBA port signatures
const HBA_SIG_ATA:   u32 = 0x0000_0101;  // SATA drive
const HBA_SIG_ATAPI: u32 = 0xEB14_0101;  // SATAPI drive (ATAPI)
const HBA_SIG_PM:    u32 = 0x9669_0101;  // Port Multiplier
const HBA_SIG_SEMB:  u32 = 0xC33C_0101;  // Enclosure Management Bridge

// HBA SATA Status: device present and in communication (DET field)
const HBA_SSTS_PRESENT: u32 = 0x3;

// Block dimensions
/// 512-byte ATA sectors
const SECTOR_SIZE: usize = 512;
/// Eclipse OS block size (4 KiB = 8 sectors)
pub const AHCI_BLOCK_SIZE: usize = 4096;
const SECTORS_PER_BLOCK: u64 = (AHCI_BLOCK_SIZE / SECTOR_SIZE) as u64;

// Number of command slots in the command list (AHCI spec: 1–32)
const CMD_SLOTS: usize = 32;

// Spin-loop delay budgets.
// On a 3 GHz CPU, spin_loop() takes roughly 3–10 ns per iteration.
/// Iterations used to hold COMRESET asserted for ≥ 1 ms before releasing.
const COMRESET_HOLD_ITERATIONS: u32 = 1_000_000;
/// Iterations used after HBA reset to let the controller and PHY stabilise.
const HBA_STABILIZE_ITERATIONS: u32 = 1_000_000;
/// Maximum iterations to wait for any port to show DET≠0 after HBA reset (~2 s on 3 GHz).
const HBA_LINK_WAIT_ITERATIONS: u32 = 200_000_000;
/// Maximum iterations to wait for TFD.BSY=0 after DET=3 (~1 s on 3 GHz).
const TFD_BSY_WAIT_ITERATIONS: u32 = 100_000_000;
/// Maximum iterations for command completion polling (~5 s on 3 GHz).
const CMD_POLL_ITERATIONS: u32 = 50_000_000;

// ── Port type (derived from signature register) ───────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum HbaPortType {
    None,
    Sata,
    Satapi,
    PortMultiplier,
    Semb,
    Unknown(u32),
}

// ── AHCI DMA structures (must match AHCI spec layout exactly) ────────────────

/// Command Header — 32 bytes.  CMD_SLOTS headers form the 1 KiB Command List.
#[repr(C)]
struct CommandHeader {
    /// [4:0] CFL (FIS length in DWORDs), [6] ATAPI, [7] Write, [8] Prefetch, [10] Reset, [13] Clear-Busy
    cfl:      u8,
    /// Port multiplier / Reset / BIST / Clear-Busy flags
    pm:       u8,
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
    acmd:  [u8; 16],       // ATAPI command (12 or 16 bytes)
    _rsvd: [u8; 48],       // Reserved
    prdt:  [PrdEntry; 1],  // 1 × PRD entry
}

// ── Per-port DMA allocation ───────────────────────────────────────────────────

struct PortDma {
    // Command List: CMD_SLOTS × 32 bytes = 1 KiB, must be 1 KiB-aligned.
    cmd_list_virt: u64,
    cmd_list_phys: u64,
    // FIS receive buffer: 256 bytes, must be 256-byte aligned.
    fis_virt: u64,
    fis_phys: u64,
    // CMD_SLOTS Command Tables, each sizeof(CommandTable) ≈ 144 bytes, aligned to 128.
    // We allocate them as a contiguous array.
    cmd_tables_virt: u64,
    cmd_tables_phys: u64,
}

// Command table stride: must be ≥ sizeof(CommandTable) and 128-byte aligned.
const CMD_TABLE_STRIDE: usize = (core::mem::size_of::<CommandTable>() + 127) & !127;

// ── AhciPort — owns the MMIO registers and DMA structures for one SATA port ──

struct AhciPort {
    base:       u64,    // Virtual address of port MMIO registers
    dma:        PortDma,
    port_index: u32,
    port_type:  HbaPortType,
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
        // Wait for CR (command list running) and FR (FIS receive running) to clear — max ~500 ms
        let mut i = 5_000_000u32;
        while self.preg(PORT_CMD) & (CMD_CR | CMD_FR) != 0 {
            if i == 0 { break; }
            i -= 1;
            crate::cpu::pause();
        }
        // Clear FRE (disable FIS receive)
        self.pwreg(PORT_CMD, self.preg(PORT_CMD) & !CMD_FRE);
        // Wait for FR to clear
        let mut i = 5_000_000u32;
        while self.preg(PORT_CMD) & CMD_FR != 0 {
            if i == 0 { break; }
            i -= 1;
            crate::cpu::pause();
        }
    }

    /// Enable FIS receive (if not already on), then start the command engine.
    fn cmd_engine_start(&self) {
        // Ensure CR is already clear before asserting ST
        let mut i = 5_000_000u32;
        while self.preg(PORT_CMD) & CMD_CR != 0 {
            if i == 0 { break; }
            i -= 1;
            crate::cpu::pause();
        }
        // FRE should already be active from init(); set it anyway just in case.
        self.pwreg(PORT_CMD, self.preg(PORT_CMD) | CMD_FRE | CMD_ST);
    }

    // ── Free-slot finder ──

    /// Find the index of an available command slot (bit clear in PxCI | PxSACT).
    /// Returns None if all slots are busy.
    fn free_slot(&self) -> Option<u32> {
        let busy = unsafe {
            let sact = core::ptr::read_volatile((self.base + PORT_SACT as u64) as *const u32);
            let ci   = core::ptr::read_volatile((self.base + PORT_CI   as u64) as *const u32);
            sact | ci
        };
        for i in 0..CMD_SLOTS as u32 {
            if busy & (1 << i) == 0 {
                return Some(i);
            }
        }
        None
    }

    // ── Port initialization ──

    /// Full port initialisation sequence (AHCI spec §10.1.2).
    /// Returns `true` if a device is connected and the port is ready.
    fn init(&self) -> bool {
        serial::serial_print("[AHCI]   Stopping engine...\n");
        // 1. Bring engine to idle
        self.cmd_engine_stop();

        // 2. Program all 32 Command Headers with their Command Table addresses
        //    (Redox style: pre-wire all slots at init time)
        unsafe {
            let cmd_list = self.dma.cmd_list_virt as *mut CommandHeader;
            for i in 0..CMD_SLOTS {
                let hdr = &mut *cmd_list.add(i);
                let ct_phys = self.dma.cmd_tables_phys + (i * CMD_TABLE_STRIDE) as u64;
                hdr.cfl      = 0;
                hdr.pm       = 0;
                hdr.prdtl    = 0;
                hdr.prdbc    = 0;
                hdr.ctba     = (ct_phys & 0xFFFF_FFFF) as u32;
                hdr.ctbau    = (ct_phys >> 32) as u32;
                hdr.reserved = [0; 4];
            }
        }

        // 3. Program Command List and FIS receive buffer base addresses
        self.pwreg(PORT_CLB,  (self.dma.cmd_list_phys & 0xFFFF_FFFF) as u32);
        self.pwreg(PORT_CLBU, (self.dma.cmd_list_phys >> 32) as u32);
        self.pwreg(PORT_FB,   (self.dma.fis_phys & 0xFFFF_FFFF) as u32);
        self.pwreg(PORT_FBU,  (self.dma.fis_phys >> 32) as u32);

        // 4. Clear all pending interrupts and errors
        let is = self.preg(PORT_IS);
        self.pwreg(PORT_IS,   is);
        self.pwreg(PORT_SERR, 0xFFFF_FFFF);

        // 5. Enable port interrupts (0b10111 = same as Redox)
        self.pwreg(PORT_IE, PORT_IE_MASK);

        // 6. Power on / spin up / enable FIS receive.
        //
        //    CRITICAL: CMD_FRE *must* be set HERE, before we wait for TFD.BSY
        //    to clear.  Without FRE the HBA cannot capture the D2H Register FIS
        //    that the device sends after reset — so TFD.BSY will never clear.
        //    (AHCI spec §10.1.2, step 4: enable FRE before asserting SUD/POD)
        let cmd = self.preg(PORT_CMD) | CMD_POD | CMD_SUD | CMD_FRE;
        self.pwreg(PORT_CMD, cmd);

        // 7. Disable Partial / Slumber power management (ALPM)
        //    Set SCTL.IPM = 3 (bits [11:8] = 0x300) to disable both Partial and Slumber,
        //    and clear SCTL.DET (bits [3:0]) so we don't disturb the active link.
        let sctl = self.preg(PORT_SCTL) & !0x0F0F;
        self.pwreg(PORT_SCTL, sctl | 0x0300);

        // 8. COMRESET — always issue it to guarantee a clean device reset.
        //
        //    Even if DET is already 3 (link was up before HBA reset), the
        //    global HBA reset may have left the port registers in an
        //    inconsistent state.  A COMRESET brings the device back cleanly.
        //    The sequence is: assert DET=1 for ≥1 ms, then clear DET=0.
        //
        //    We do NOT limit SPD here (leave SPD=0 = auto-negotiate)
        //    because restricting to Gen1 can cause some SSDs to re-enum
        //    at a lower speed than the OS later expects.
        self.pwreg(PORT_SCTL, (self.preg(PORT_SCTL) & !0x0F) | 0x01); // DET=1
        for _ in 0..COMRESET_HOLD_ITERATIONS { crate::cpu::pause(); }
        self.pwreg(PORT_SCTL, self.preg(PORT_SCTL) & !0x0F); // DET=0

        serial::serial_print("[AHCI]   Waiting for link (DET=3)...\n");
        // 9. Wait for DET=3 (device present, PHY comms up).
        //    On real hardware after COMRESET the PHY re-negotiates the link;
        //    this typically takes 50–300 ms for SSDs and HDDs.
        //    50_000_000 spins (~250 ms on a 3 GHz CPU) is sufficient for all
        //    standard SATA devices.  Reducing from 500 M avoids wasting ~2.5 s
        //    per empty port when all implemented ports are probed.
        let mut ready = false;
        let mut i = 50_000_000u32;
        loop {
            let ssts = self.preg(PORT_SSTS);
            if ssts & 0x0F == 3 {
                ready = true;
                break;
            }
            if i == 0 { break; }
            i -= 1;
            crate::cpu::pause();
        }

        if !ready {
            serial::serial_print("[AHCI] Port ");
            serial::serial_print_dec(self.port_index as u64);
            serial::serial_print(": no device after init (SSTS=0x");
            serial::serial_print_hex(self.preg(PORT_SSTS) as u64);
            serial::serial_print(" SERR=0x");
            serial::serial_print_hex(self.preg(PORT_SERR) as u64);
            serial::serial_print(")\n");
            return false;
        }
        let ssts_ok = self.preg(PORT_SSTS);
        serial::serial_print("[AHCI]   Link up SSTS=0x");
        serial::serial_print_hex(ssts_ok as u64);
        serial::serial_print("\n");

        // 9.5. Wait for TFD.STS.BSY and TFD.STS.DRQ to clear.
        //
        //      Now that FRE is active, the device can DMA its D2H Register FIS
        //      into our FIS buffer and update TFD.  On real hardware this can
        //      take 100–500 ms; in QEMU it is nearly instantaneous.
        let tfd_initial = self.preg(PORT_TFD);
        serial::serial_print("[AHCI]   Waiting for TFD.BSY/DRQ (TFD=0x");
        serial::serial_print_hex(tfd_initial as u64);
        serial::serial_print(")...\n");

        let mut tfd_timeout = 50_000_000u32; // ~500 ms on 3 GHz
        loop {
            let tfd = self.preg(PORT_TFD);
            if tfd & ((ATA_DEV_BUSY | ATA_DEV_DRQ) as u32) == 0 {
                serial::serial_print("[AHCI]   TFD clear (TFD=0x");
                serial::serial_print_hex(tfd as u64);
                serial::serial_print(")\n");
                break;
            }
            if tfd_timeout == 0 {
                serial::serial_print("[AHCI] Port ");
                serial::serial_print_dec(self.port_index as u64);
                serial::serial_print(": TFD BSY/DRQ stuck (TFD=0x");
                serial::serial_print_hex(tfd as u64);
                serial::serial_print(" SERR=0x");
                serial::serial_print_hex(self.preg(PORT_SERR) as u64);
                serial::serial_print("), proceeding anyway\n");
                break;
            }
            tfd_timeout -= 1;
            crate::cpu::pause();
        }

        // 10. Clear errors accumulated during bring-up
        self.pwreg(PORT_SERR, 0xFFFF_FFFF);
        self.pwreg(PORT_IS,   0xFFFF_FFFF);

        // 11. Start command engine (ST=1; FRE is already set from step 6)
        self.cmd_engine_start();
        serial::serial_print("[AHCI]   Engine started.\n");

        true
    }

    // ── Command issue / polling ──

    /// Build command in the given slot, issue it, and spin-poll to completion.
    /// Returns `true` on success.
    fn exec_cmd(&self, slot: u32) -> bool {
        // mfence ensures all prior writes to the command table are visible to the
        // HBA's DMA engine before we set PxCI.
        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)); }

        // Clear interrupt status
        self.pwreg(PORT_IS, 0xFFFF_FFFF);

        // Issue slot
        self.pwreg(PORT_CI, 1 << slot);

        // Poll until the HBA clears the bit in PxCI (command consumed)
        let mut i = CMD_POLL_ITERATIONS;
        loop {
            if self.preg(PORT_CI) & (1 << slot) == 0 { break; }

            let is = self.preg(PORT_IS);
            if is & IS_ERRORS != 0 {
                serial::serial_print("[AHCI] Port ");
                serial::serial_print_dec(self.port_index as u64);
                serial::serial_print(" error IS=");
                serial::serial_print_hex(is as u64);
                serial::serial_print(" TFD=");
                serial::serial_print_hex(self.preg(PORT_TFD) as u64);
                serial::serial_print(" SERR=");
                serial::serial_print_hex(self.preg(PORT_SERR) as u64);
                serial::serial_print("\n");
                self.pwreg(PORT_IS,   0xFFFF_FFFF);
                self.pwreg(PORT_SERR, 0xFFFF_FFFF);
                return false;
            }

            if i == 0 {
                serial::serial_print("[AHCI] Port ");
                serial::serial_print_dec(self.port_index as u64);
                serial::serial_print(" command timeout (slot=");
                serial::serial_print_dec(slot as u64);
                serial::serial_print(")\n");
                return false;
            }
            i -= 1;
            crate::cpu::pause();
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

    /// Build the H2D FIS + PRD entry + command header for the given slot, then
    /// issue and poll to completion.
    fn rw_dma(&self, lba: u64, buf_phys: u64, buf_len: usize, write: bool) -> bool {
        let sector_count = (buf_len / SECTOR_SIZE) as u32;
        if sector_count == 0 || sector_count > 65535 {
            return false;
        }

        let slot = match self.free_slot() {
            Some(s) => s,
            None => {
                serial::serial_print("[AHCI] Port ");
                serial::serial_print_dec(self.port_index as u64);
                serial::serial_print(": no free command slot\n");
                return false;
            }
        };

        // Wait for TFD.BSY / TFD.DRQ to clear before issuing command
        {
            let mut w = 1_000_000u32;
            loop {
                let tfd = self.preg(PORT_TFD);
                if tfd & ((ATA_DEV_BUSY | ATA_DEV_DRQ) as u32) == 0 { break; }
                if w == 0 { break; }
                w -= 1;
                crate::cpu::pause();
            }
        }

        unsafe {
            let cmd_list  = self.dma.cmd_list_virt as *mut CommandHeader;
            let cmd_table = (self.dma.cmd_tables_virt + (slot as usize * CMD_TABLE_STRIDE) as u64)
                            as *mut CommandTable;

            // Zero command table
            core::ptr::write_bytes(cmd_table as *mut u8, 0,
                core::mem::size_of::<CommandTable>());

            // ── H2D Register FIS (20 bytes = 5 DWORDs) ──────────────────
            let f = (*cmd_table).cfis.as_mut_ptr();
            *f.add(0)  = FIS_TYPE_REG_H2D;
            *f.add(1)  = 0x80; // C=1: command (not control) update
            *f.add(2)  = if write { ATA_CMD_WRITE_DMA_EXT } else { ATA_CMD_READ_DMA_EXT };
            *f.add(3)  = 0;    // features low
            *f.add(4)  =  lba        as u8; // LBA[7:0]
            *f.add(5)  = (lba >>  8) as u8; // LBA[15:8]
            *f.add(6)  = (lba >> 16) as u8; // LBA[23:16]
            *f.add(7)  = 1 << 6;            // Device: LBA-mode (bit 6)
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
            // DBC = byte_count − 1; IOC = 1 (interrupt on completion)
            (*cmd_table).prdt[0].dbc_ioc = (buf_len as u32 - 1) | (1 << 31);

            // ── Command Header in the chosen slot ────────────────────────
            // CFL = 5 (H2D FIS is 20 bytes / 4 = 5 DWORDs)
            // Bit 6 = Write flag; bit 7 = ATAPI
            let hdr = &mut *cmd_list.add(slot as usize);
            let w_bit: u8 = if write { 1 << 6 } else { 0 };
            hdr.cfl   = 5 | w_bit;
            hdr.pm    = 0;
            hdr.prdtl = 1;
            hdr.prdbc = 0;
            // ctba / ctbau were pre-wired during init(); no need to set again,
            // but we do so here for clarity.
            let ct_phys = self.dma.cmd_tables_phys + (slot as usize * CMD_TABLE_STRIDE) as u64;
            hdr.ctba  = (ct_phys & 0xFFFF_FFFF) as u32;
            hdr.ctbau = (ct_phys >> 32) as u32;
        }

        self.exec_cmd(slot)
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

    /// Issue ATA IDENTIFY (or IDENTIFY PACKET for ATAPI) and return the total
    /// number of addressable sectors (LBA-48 preferred, LBA-28 as fallback).
    /// Also emits model / firmware / serial information to serial log.
    /// Returns `None` if IDENTIFY fails or returns zero capacity.
    pub fn identify(&self) -> Option<u64> {
        let identify_cmd = match self.port_type {
            HbaPortType::Satapi => ATA_CMD_IDENTIFY_PACKET,
            _                   => ATA_CMD_IDENTIFY,
        };

        // Allocate a 512-byte, 512-byte-aligned DMA buffer
        let (id_virt, id_phys) = memory::alloc_dma_buffer(SECTOR_SIZE, SECTOR_SIZE)?;
        unsafe { core::ptr::write_bytes(id_virt, 0, SECTOR_SIZE); }

        let slot = self.free_slot()?;

        {
            // Wait for TFD clear
            let mut w = 1_000_000u32;
            loop {
                let tfd = self.preg(PORT_TFD);
                if tfd & ((ATA_DEV_BUSY | ATA_DEV_DRQ) as u32) == 0 { break; }
                if w == 0 { break; }
                w -= 1;
                crate::cpu::pause();
            }
        }

        unsafe {
            let cmd_list  = self.dma.cmd_list_virt as *mut CommandHeader;
            let cmd_table = (self.dma.cmd_tables_virt + (slot as usize * CMD_TABLE_STRIDE) as u64)
                            as *mut CommandTable;

            core::ptr::write_bytes(cmd_table as *mut u8, 0,
                core::mem::size_of::<CommandTable>());

            let f = (*cmd_table).cfis.as_mut_ptr();
            *f.add(0) = FIS_TYPE_REG_H2D;
            *f.add(1) = 0x80;               // C=1
            *f.add(2) = identify_cmd;
            *f.add(7) = 0;                  // Device register: 0 for IDENTIFY

            (*cmd_table).prdt[0].dba     = (id_phys & 0xFFFF_FFFF) as u32;
            (*cmd_table).prdt[0].dbau    = (id_phys >> 32) as u32;
            (*cmd_table).prdt[0]._rsvd   = 0;
            (*cmd_table).prdt[0].dbc_ioc = (SECTOR_SIZE as u32 - 1) | (1 << 31);

            let hdr = &mut *cmd_list.add(slot as usize);
            hdr.cfl   = 5;       // CFL=5, W=0
            hdr.pm    = 0;
            hdr.prdtl = 1;
            hdr.prdbc = 0;
            let ct_phys = self.dma.cmd_tables_phys + (slot as usize * CMD_TABLE_STRIDE) as u64;
            hdr.ctba  = (ct_phys & 0xFFFF_FFFF) as u32;
            hdr.ctbau = (ct_phys >> 32) as u32;
        }

        if !self.exec_cmd(slot) {
            return None;
        }

        // Parse IDENTIFY data (array of u16 words, little-endian)
        let id = unsafe { core::slice::from_raw_parts(id_virt as *const u16, 256) };

        // Log human-readable device info ─────────────────────────────────────
        // Serial number: words 10–19 (byte-swapped per word)
        // Firmware rev : words 23–26
        // Model name   : words 27–46
        print_id_string("Serial",   id, 10, 20);
        print_id_string("Firmware", id, 23, 27);
        print_id_string("Model",    id, 27, 47);

        // LBA-48 capacity at words 100–103
        let lba48 = (id[100] as u64)
            | ((id[101] as u64) << 16)
            | ((id[102] as u64) << 32)
            | ((id[103] as u64) << 48);

        // LBA-28 fallback at words 60–61
        let lba28 = (id[60] as u64) | ((id[61] as u64) << 16);

        let sectors = if lba48 != 0 { lba48 } else { lba28 };

        serial::serial_print("[AHCI] Port ");
        serial::serial_print_dec(self.port_index as u64);
        if lba48 != 0 {
            serial::serial_print(" LBA-48 sectors=");
        } else {
            serial::serial_print(" LBA-28 sectors=");
        }
        serial::serial_print_dec(sectors);
        serial::serial_print(" (");
        serial::serial_print_dec(sectors / 2048); // MiB
        serial::serial_print(" MiB)\n");

        if sectors == 0 { None } else { Some(sectors) }
    }

    // ── Port type probe ───────────────────────────────────────────────────────

    /// Determine device type from PxSSTS and PxSIG.
    /// Call this AFTER port.init() so the device has had a chance to send its
    /// D2H Register FIS and populate the SIG register.
    ///
    /// If the SIG is still 0 / unrecognised but DET=3 (device present),
    /// we treat it as a regular SATA drive and let IDENTIFY sort it out.
    fn probe(base: u64) -> HbaPortType {
        let ssts = unsafe {
            core::ptr::read_volatile((base + PORT_SSTS as u64) as *const u32)
        };
        let det = ssts & 0x0F;
        if det == 0 || det == 4 {
            return HbaPortType::None;
        }

        // The SIG register may need a moment to be written by the device.
        // Retry up to ~50 ms.
        let sig = Self::read_sig_with_retry(base);

        serial::serial_print("[AHCI]   SIG=0x");
        serial::serial_print_hex(sig as u64);
        serial::serial_print("\n");

        match sig {
            HBA_SIG_ATA   => HbaPortType::Sata,
            HBA_SIG_ATAPI => HbaPortType::Satapi,
            HBA_SIG_PM    => HbaPortType::PortMultiplier,
            HBA_SIG_SEMB  => HbaPortType::Semb,
            // 0x0000_0000 or unrecognised: device present (DET=3) → assume SATA.
            // IDENTIFY will confirm or reject.
            _ => {
                serial::serial_print("[AHCI]   Unknown SIG with DET=3, treating as SATA\n");
                HbaPortType::Sata
            }
        }
    }

    /// Read PxSIG, retrying until it becomes non-zero or the timeout expires.
    /// Some controllers update SIG asynchronously after DET reaches 3.
    fn read_sig_with_retry(base: u64) -> u32 {
        // Retry for up to ~50 ms (5_000_000 × ~10 ns)
        let mut retries = 5_000_000u32;
        loop {
            let sig = unsafe {
                core::ptr::read_volatile((base + PORT_SIG as u64) as *const u32)
            };
            // A nonzero, non-0xFFFFFFFF value means the device has written its signature.
            if sig != 0 && sig != 0xFFFF_FFFF {
                return sig;
            }
            if retries == 0 {
                return sig; // Return whatever we have (may be 0)
            }
            retries -= 1;
            crate::cpu::pause();
        }
    }
}

// ── Print helpers for IDENTIFY strings ───────────────────────────────────────

/// Print an ATA IDENTIFY string field (words `start`..`end`-1, byte-swapped per word).
fn print_id_string(label: &'static str, id: &[u16], start: usize, end: usize) {
    serial::serial_print("[AHCI]   ");
    serial::serial_print(label);
    serial::serial_print(": ");
    for w in start..end {
        if w >= id.len() { break; }
        let hi = ((id[w] >> 8) & 0xFF) as u8;
        let lo = (id[w] & 0xFF) as u8;
        if hi >= b' ' && hi < 0x7F { serial::serial_print_byte(hi); }
        if lo >= b' ' && lo < 0x7F { serial::serial_print_byte(lo); }
    }
    serial::serial_print("\n");
}

// ── AhciDisk — implements storage::BlockDevice for one AHCI port ─────────────

struct AhciDisk {
    port:       Mutex<AhciPort>,
    capacity:   u64,  // in 4 KiB blocks
    port_type:  HbaPortType,
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

    fn name(&self) -> &'static str {
        match self.port_type {
            HbaPortType::Satapi => "AHCI-ATAPI",
            _                   => "AHCI",
        }
    }
}

// ── DMA helpers ───────────────────────────────────────────────────────────────

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

// ── HBA MMIO helpers (non-port) ───────────────────────────────────────────────

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
    let abar_raw = unsafe { pci::get_bar(dev, 5) };
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

    // Disable legacy INTx interrupts on the PCI level so the AHCI controller
    // never fires an IRQ line.  We always use polling mode, not interrupts.
    // Without this, the HBA asserts INTx after every DMA completion; since no
    // IDT handler is registered for the SATA IRQ vector on real hardware this
    // can cause IRQ storms or #NP exceptions that hang the kernel.
    unsafe {
        let cmd = pci::pci_config_read_u16(dev.bus, dev.device, dev.function, 0x04);
        pci::pci_config_write_u16(dev.bus, dev.device, dev.function, 0x04, cmd | 0x0400);
    }

    // ── 3. Map AHCI MMIO region into the kernel virtual address space ────────
    // AHCI register space: 0x100 (global) + 32 ports × 0x80 = 0x1100 bytes.
    // We map 8 KiB to be safe.
    let base = memory::map_mmio_range(abar, 0x2000);
    if base == 0 {
        serial::serial_print("[AHCI] MMIO mapping failed — skipping controller\n");
        return;
    }

    // ── 4. BIOS/OS Handoff (§10.6): take ownership from BIOS ────────────────
    // BOHC bits: [0]=BOS (BIOS owns), [1]=OOS (OS requests), [4]=BB (BIOS busy).
    // This is critical on real hardware: the BIOS may still own the HBA.
    let bohc = hreg(base, HBA_BOHC);
    serial::serial_print("[AHCI] BOHC=0x");
    serial::serial_print_hex(bohc as u64);
    serial::serial_print("\n");
    if bohc & 1 != 0 {
        // BIOS owns the HBA — request ownership by setting OOS (bit 1)
        serial::serial_print("[AHCI] BIOS owns HBA (BOS=1), requesting handoff...\n");
        hwreg(base, HBA_BOHC, bohc | (1 << 1));
        // Step 1: Wait for BIOS to clear BOS (max ~25 ms per spec §10.6)
        let mut t = 25_000_000u32;
        while hreg(base, HBA_BOHC) & 1 != 0 {
            if t == 0 { break; }
            t -= 1;
            crate::cpu::pause();
        }
        // Step 2: If BIOS Busy (BB, bit 4) is set, wait up to 2 seconds
        // for the BIOS to finish any in-flight operations before we reset.
        if hreg(base, HBA_BOHC) & (1 << 4) != 0 {
            serial::serial_print("[AHCI] BIOS busy (BB=1), waiting up to 2s...\n");
            let mut bb_wait = 200_000_000u32;
            while hreg(base, HBA_BOHC) & (1 << 4) != 0 {
                if bb_wait == 0 { break; }
                bb_wait -= 1;
                crate::cpu::pause();
            }
        }
        let bohc_after = hreg(base, HBA_BOHC);
        serial::serial_print("[AHCI] BOHC after handoff=0x");
        serial::serial_print_hex(bohc_after as u64);
        serial::serial_print("\n");
    }


    // ── 5. Global HBA Reset (AHCI spec §10.4.3) ──────────────────────────────
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
        crate::cpu::pause();
    }
    // Re-assert AHCI Enable. GHC.IE is intentionally NOT set: we use
    // polling mode and setting the global interrupt-enable bit would cause
    // the HBA to signal a PCI interrupt after every DMA completion.  Without
    // a registered handler that vector results in an IRQ storm or a #NP
    // exception on real hardware, hanging the kernel.
    hwreg(base, HBA_GHC, GHC_AE);

    // Let the HBA and PHY layers stabilise (~1 ms initial delay)
    for _ in 0..HBA_STABILIZE_ITERATIONS { crate::cpu::pause(); }

    // ── 6. Read capabilities and port bitmask ────────────────────────────────
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

    // On real hardware, SATA link negotiation after a global HBA reset can take
    // up to 2 seconds.  Wait until at least one implemented port shows DET≠0.
    let mut link_wait = HBA_LINK_WAIT_ITERATIONS;
    loop {
        let mut any_det = false;
        for p in 0u32..32 {
            if pi & (1 << p) == 0 { continue; }
            let pb = base + 0x100 + (p as u64 * 0x80);
            let det = unsafe {
                core::ptr::read_volatile((pb + PORT_SSTS as u64) as *const u32)
            } & 0x0F;
            if det != 0 {
                any_det = true;
                break;
            }
        }
        if any_det || link_wait == 0 { break; }
        link_wait -= 1;
        crate::cpu::pause();
    }
    if link_wait == 0 {
        serial::serial_print("[AHCI] Warning: no port showed DET≠0 within link-wait timeout\n");
    }

    // ── 7. Initialise every implemented port ─────────────────────────────────
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
        serial::serial_print(" SSTS=0x");
        serial::serial_print_hex(ssts as u64);
        serial::serial_print(" DET=");
        serial::serial_print_dec(det as u64);
        serial::serial_print("\n");

        // DET=4 → port explicitly disabled by the controller; skip.
        // DET=0 → no link seen yet, but link negotiation may still be in
        // progress on some ports that were slower than the one that triggered
        // the global link-wait exit.  Allow these ports to go through
        // COMRESET so they get a fair chance to bring up the link.
        if det == 4 {
            serial::serial_print("[AHCI] Port ");
            serial::serial_print_dec(port_idx as u64);
            serial::serial_print(": port disabled (DET=4) — skipping\n");
            continue;
        }

        // Allocate aligned DMA structures for this port
        // Command List: 32 × 32 bytes = 1 KiB
        let (cl_virt, cl_phys)   = alloc_dma(CMD_SLOTS * 32, 1024);
        // FIS receive buffer: 256 bytes
        let (fis_virt, fis_phys) = alloc_dma(256, 256);
        // CMD_SLOTS Command Tables, each CMD_TABLE_STRIDE bytes, aligned to 128
        let total_ct_size = CMD_SLOTS * CMD_TABLE_STRIDE;
        let (ct_virt, ct_phys)   = alloc_dma(total_ct_size, 128);

        // port_type placeholder — will be overwritten after init() succeeds
        // (SIG register is only valid after receiving the device's D2H FIS)
        let port = AhciPort {
            base:       port_base,
            dma: PortDma {
                cmd_list_virt:   cl_virt,
                cmd_list_phys:   cl_phys,
                fis_virt,
                fis_phys,
                cmd_tables_virt: ct_virt,
                cmd_tables_phys: ct_phys,
            },
            port_index: port_idx,
            port_type:  HbaPortType::Sata, // default; corrected below after init
        };

        serial::serial_print("[AHCI]   Port allocated. Initialising...\n");
        if !port.init() {
            serial::serial_print("[AHCI] Port ");
            serial::serial_print_dec(port_idx as u64);
            serial::serial_print(" init failed — skipping\n");
            continue;
        }

        // ── Re-probe signature NOW (after init / FIS receive enabled) ────────
        // The device has had time to send its D2H Register FIS.
        let port_type = AhciPort::probe(port_base);
        // Rebuild port with the correct type
        let port = AhciPort {
            base:       port_base,
            dma: PortDma {
                cmd_list_virt:   cl_virt,
                cmd_list_phys:   cl_phys,
                fis_virt,
                fis_phys,
                cmd_tables_virt: ct_virt,
                cmd_tables_phys: ct_phys,
            },
            port_index: port_idx,
            port_type,
        };

        serial::serial_print("[AHCI] Port ");
        serial::serial_print_dec(port_idx as u64);
        serial::serial_print(" Type=");
        match port_type {
            HbaPortType::Sata           => serial::serial_print("SATA"),
            HbaPortType::Satapi         => serial::serial_print("SATAPI"),
            HbaPortType::PortMultiplier => serial::serial_print("PortMux"),
            HbaPortType::Semb           => serial::serial_print("SEMB"),
            HbaPortType::None           => serial::serial_print("None"),
            HbaPortType::Unknown(_)     => serial::serial_print("Sata(fallback)"),
        }
        serial::serial_print("\n");

        // Skip non-block port types
        match port_type {
            HbaPortType::PortMultiplier | HbaPortType::Semb => {
                serial::serial_print("[AHCI] Port ");
                serial::serial_print_dec(port_idx as u64);
                serial::serial_print(": skipping unsupported port type\n");
                continue;
            }
            _ => {}
        }

        serial::serial_print("[AHCI]   Identifying device...\n");
        let capacity_sectors = port.identify().unwrap_or(0);
        let capacity_blocks = if capacity_sectors > 0 {
            capacity_sectors / SECTORS_PER_BLOCK
        } else {
            // IDENTIFY failed or returned zero; use sentinel max
            serial::serial_print("[AHCI] Port ");
            serial::serial_print_dec(port_idx as u64);
            serial::serial_print(": IDENTIFY returned 0, using sentinel capacity\n");
            u64::MAX / SECTORS_PER_BLOCK
        };

        serial::serial_print("[AHCI] Port ");
        serial::serial_print_dec(port_idx as u64);
        serial::serial_print(" ready — ");
        serial::serial_print_dec(capacity_sectors);
        serial::serial_print(" sectors (");
        serial::serial_print_dec(capacity_sectors / 2048); // MiB
        serial::serial_print(" MiB)\n");

        let pt = port.port_type;
        let disk: Arc<dyn crate::storage::BlockDevice> = Arc::new(AhciDisk {
            port:      Mutex::new(port),
            capacity:  capacity_blocks,
            port_type: pt,
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
