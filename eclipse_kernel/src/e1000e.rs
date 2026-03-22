//! Intel e1000e Ethernet Driver
//!
//! Supports Intel GbE controllers of the e1000e family, including:
//! - I217-LM / I217-V   (Haswell, 4th gen Core)
//! - I218-LM / I218-V   (Broadwell, 5th gen Core)
//! - I219-LM / I219-V   (Skylake and later, 6th+ gen Core)
//!
//! The I219-V (device ID 0x15B8) is a common card found in Intel 100-series
//! (Skylake) desktop/workstation platforms and is the card reported in the
//! issue.  This driver implements the basic e1000e register layout using
//! legacy Tx/Rx descriptors, which are supported by all variants.

use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;
use core::ptr::{read_volatile, write_volatile};

use crate::serial;
use crate::memory;

// ───────────────────────────────────────────────────────────────────────────
// PCI device IDs for the e1000e / I21x family (vendor = 0x8086 / Intel)
// ───────────────────────────────────────────────────────────────────────────
const INTEL_VENDOR_ID: u16 = 0x8086;

/// All PCI device IDs belonging to the I217/I218/I219 (e1000e PCH-based) family.
const E1000E_DEVICE_IDS: &[u16] = &[
    // Legacy e1000
    0x100E, // 82540EM (QEMU e1000)
    0x100F, // 82545EM
    0x1015, // 82540EM Qemu
    // PCIe e1000e
    0x10D3, // 82574L (QEMU e1000e)
    0x10EA, // 82577LM
    0x10F5, // 82567LM
    // I217
    0x153A, // I217-LM
    0x153B, // I217-V
    // I218
    0x155A, // I218-LM
    0x1559, // I218-V
    0x15A0, // I218-LM (2)
    0x15A1, // I218-V  (2)
    0x15A2, // I218-LM (3)
    0x15A3, // I218-V  (3)
    // I219  — the family relevant to the bug report
    0x156F, // I219-LM
    0x1570, // I219-V
    0x15B7, // I219-LM (2)
    0x15B8, // I219-V  (2)  ← the exact card from the issue
    0x15BB, // I219-LM (3)
    0x15BC, // I219-V  (3)
    0x15D7, // I219-LM (3) rev.2
    0x15D8, // I219-V  (3) rev.2
    0x15E3, // I219-LM (4)
    0x15D6, // I219-V  (4)
    0x0DC5, // I219-LM (17)
    0x0DC6, // I219-V  (17)
    0x0DC7, // I219-LM (18)
    0x0DC8, // I219-V  (18)
];

// ───────────────────────────────────────────────────────────────────────────
// Register offsets (relative to BAR0 virtual base)
// ───────────────────────────────────────────────────────────────────────────
const REG_CTRL:     u32 = 0x0000_0;
const REG_STATUS:   u32 = 0x0000_8; // Device status
const REG_EERD:     u32 = 0x0001_4;
const REG_CTRL_EXT: u32 = 0x0001_8; // Extended device control
const REG_MDIC:     u32 = 0x0002_0; // MDI (PHY) control
const REG_IMC:      u32 = 0x000D_8;
const REG_RCTL:     u32 = 0x0010_0;
const REG_TCTL:     u32 = 0x0040_0;
const REG_TIPG:     u32 = 0x0041_0;
const REG_RDBAL:    u32 = 0x0280_0;
const REG_RDBAH:    u32 = 0x0280_4;
const REG_RDLEN:    u32 = 0x0280_8;
const REG_RDH:      u32 = 0x0281_0;
const REG_RDT:      u32 = 0x0281_8;
const REG_TDBAL:    u32 = 0x0380_0;
const REG_TDBAH:    u32 = 0x0380_4;
const REG_TDLEN:    u32 = 0x0380_8;
const REG_TDH:      u32 = 0x0381_0;
const REG_TDT:      u32 = 0x0381_8;
const REG_MTA:      u32 = 0x0520_0; // Multicast Table Array (128 × u32)
const REG_RAL0:     u32 = 0x0540_0; // Receive Address Low  (filter 0)
const REG_RAH0:     u32 = 0x0540_4; // Receive Address High (filter 0)
const REG_RXCSUM:   u32 = 0x5000;   // Receive Checksum Offload Control

// ───────────────────────────────────────────────────────────────────────────
// CTRL register bits
// ───────────────────────────────────────────────────────────────────────────
const CTRL_FD:   u32 = 1 << 0;  // Full-duplex
const CTRL_ASDE: u32 = 1 << 5;  // Auto-speed detection enable
const CTRL_SLU:  u32 = 1 << 6;  // Set link up
const CTRL_RST:  u32 = 1 << 26; // Device reset

// ───────────────────────────────────────────────────────────────────────────
// STATUS register bits
// ───────────────────────────────────────────────────────────────────────────
const STATUS_LU: u32 = 1 << 1; // Link Up

// ───────────────────────────────────────────────────────────────────────────
// CTRL_EXT register bits
// ───────────────────────────────────────────────────────────────────────────
/// PHY Power-Down Enable — when set, the MAC holds the PHY in power-down.
/// Must be CLEARED to allow the I219-V's internal PHY to power up.
const CTRL_EXT_PHYPDEN: u32 = 1 << 30;
/// Driver Loaded — signals to the PCH Management Engine (Intel ME) that the
/// OS Ethernet driver has taken control of the device.  Without this, on
/// I217/I218/I219 (PCH-based) controllers the ME may still intercept or
/// redirect certain receive traffic, causing DHCP to silently fail.
const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;

// ───────────────────────────────────────────────────────────────────────────
// MDIC (MDI Control) register fields — used to access the PHY via MII
// ───────────────────────────────────────────────────────────────────────────
const MDIC_PHY1:     u32 = 1 << 21; // PHY address = 1 (I217/I218/I219 internal PHY)
const MDIC_OP_WRITE: u32 = 1 << 26; // Opcode 01 = write
const MDIC_OP_READ:  u32 = 1 << 27; // Opcode 10 = read
const MDIC_READY:    u32 = 1 << 28; // Transaction complete
const MDIC_ERROR:    u32 = 1 << 30; // Error flag

// MII/MDIO register index 0 = Basic Mode Control Register (BMCR)
const PHY_REG_BMCR:    u32 = 0;
const BMCR_POWER_DOWN: u16 = 1 << 11; // PHY power-down bit in BMCR

// ───────────────────────────────────────────────────────────────────────────
// RCTL register bits
// ───────────────────────────────────────────────────────────────────────────
const RCTL_EN:    u32 = 1 << 1;  // RX enable
const RCTL_UPE:   u32 = 1 << 3;  // Unicast promiscuous (accept all unicast)
const RCTL_MPE:   u32 = 1 << 4;  // Multicast promiscuous
const RCTL_BAM:   u32 = 1 << 15; // Broadcast accept
const RCTL_SECRC: u32 = 1 << 26; // Strip Ethernet CRC

// ───────────────────────────────────────────────────────────────────────────
// TCTL register bits
// ───────────────────────────────────────────────────────────────────────────
const TCTL_EN:  u32 = 1 << 1; // TX enable
const TCTL_PSP: u32 = 1 << 3; // Pad short packets

// ───────────────────────────────────────────────────────────────────────────
// TX/RX descriptor command/status bits
// ───────────────────────────────────────────────────────────────────────────
const TXD_CMD_EOP:  u8 = 1 << 0; // End of packet
const TXD_CMD_IFCS: u8 = 1 << 1; // Insert FCS
const TXD_CMD_RS:   u8 = 1 << 3; // Report status
const TXD_STA_DD:   u8 = 1 << 0; // Descriptor done
const RXD_STA_DD:   u8 = 1 << 0; // Descriptor done
const RXD_STA_EOP:  u8 = 1 << 1; // End of Packet (last descriptor for this frame)

/// Error bits in the legacy RX descriptor errors byte that indicate the
/// received frame data is corrupt and must be discarded.
///
/// * Bit 0 (CE)  — CRC Error or Alignment Error: frame CRC does not match.
/// * Bit 1 (SE)  — Symbol Error: 8B/10B code violation on the link.
/// * Bit 7 (RXE) — Rx Data Error: PCI bus or DMA error during write.
///
/// Bits 5 (TCPE) and 6 (IPE) are TCP/UDP and IP checksum offload results;
/// they are only valid when checksum offload is enabled in RXCSUM. We always
/// write RXCSUM = 0, so those bits will never be set by the hardware.
/// Bits 2 (SEQ), 3 (reserved), and 4 (Carrier Extension) are either transient
/// or not fatal to the frame data and are handled by the upper-layer stack.
const RXD_ERR_FATAL: u8 = (1 << 0) | (1 << 1) | (1 << 7); // CE | SE | RXE

// ───────────────────────────────────────────────────────────────────────────
// Descriptor ring sizes — powers of two so modulo is cheap
// ───────────────────────────────────────────────────────────────────────────
const RX_RING_SIZE: usize = 32;
const TX_RING_SIZE: usize = 32;
const PACKET_BUF_SIZE: usize = 2048;

// ───────────────────────────────────────────────────────────────────────────
// Legacy RX descriptor (16 bytes, little-endian)
// ───────────────────────────────────────────────────────────────────────────
#[repr(C, packed)]
struct RxDesc {
    buffer_addr: u64,
    length:      u16,
    checksum:    u16,
    status:      u8,
    errors:      u8,
    special:     u16,
}

// ───────────────────────────────────────────────────────────────────────────
// Legacy TX descriptor (16 bytes, little-endian)
// ───────────────────────────────────────────────────────────────────────────
#[repr(C, packed)]
struct TxDesc {
    buffer_addr: u64,
    length:      u16,
    cso:         u8,
    cmd:         u8,
    status:      u8,
    css:         u8,
    special:     u16,
}

// ───────────────────────────────────────────────────────────────────────────
// Device state (behind a Mutex so the outer type is Sync)
// ───────────────────────────────────────────────────────────────────────────
struct E1000EInner {
    /// Virtual base address of the MMIO BAR
    mmio_base: u64,
    mac: [u8; 6],

    // RX ring
    rx_descs_virt: u64,             // virtual address of descriptor ring
    rx_descs_phys: u64,             // physical address of descriptor ring
    rx_bufs: [(u64, u64); RX_RING_SIZE], // (virt, phys) per slot
    rx_tail: usize,                 // software's read cursor

    // TX ring
    tx_descs_virt: u64,
    tx_descs_phys: u64,
    tx_bufs: [(u64, u64); TX_RING_SIZE],
    tx_tail: usize,
}

/// Public handle to an Intel e1000e Ethernet device.
pub struct E1000EDevice {
    inner: Mutex<E1000EInner>,
}

// ───────────────────────────────────────────────────────────────────────────
// Module-level device registry (populated during init)
// ───────────────────────────────────────────────────────────────────────────
static E1000E_DEVICES: Mutex<Vec<Arc<E1000EDevice>>> = Mutex::new(Vec::new());

// ───────────────────────────────────────────────────────────────────────────
// Register read/write helpers
// ───────────────────────────────────────────────────────────────────────────
impl E1000EInner {
    #[inline]
    fn read32(&self, reg: u32) -> u32 {
        unsafe { read_volatile((self.mmio_base + reg as u64) as *const u32) }
    }

    #[inline]
    fn write32(&self, reg: u32, val: u32) {
        unsafe { write_volatile((self.mmio_base + reg as u64) as *mut u32, val) }
    }

    /// Read the MAC address stored in the Receive Address register 0.
    fn mac_from_rar(&self) -> [u8; 6] {
        let ral = self.read32(REG_RAL0);
        let rah = self.read32(REG_RAH0);
        [
            (ral         & 0xFF) as u8,
            ((ral >>  8) & 0xFF) as u8,
            ((ral >> 16) & 0xFF) as u8,
            ((ral >> 24) & 0xFF) as u8,
            (rah         & 0xFF) as u8,
            ((rah >>  8) & 0xFF) as u8,
        ]
    }

    /// Attempt to read a 16-bit word from the NVM via the EERD register.
    /// Returns `None` if the NVM read times out.
    fn nvm_read(&self, word_addr: u16) -> Option<u16> {
        // Start the read: address << 2 | START
        self.write32(REG_EERD, ((word_addr as u32) << 2) | 0x1);

        // Poll for completion; the DONE bit is bit 1 for most e1000e variants
        // and bit 4 for a few older ones — check both.
        for _ in 0..20_000 {
            let v = self.read32(REG_EERD);
            if (v & (1 << 1)) != 0 || (v & (1 << 4)) != 0 {
                return Some((v >> 16) as u16);
            }
            for _ in 0..200 { core::hint::spin_loop(); }
        }
        None
    }

    /// Read a PHY register via the MDI Control (MDIC) interface.
    ///
    /// The I217/I218/I219 family exposes its integrated PHY at address 1 via
    /// the standard IEEE 802.3 Clause-22 MII management frame format.
    /// Returns `None` on timeout or MDIC error.
    fn mdic_read(&self, phy_reg: u32) -> Option<u16> {
        self.write32(REG_MDIC,
            MDIC_PHY1 | ((phy_reg & 0x1F) << 16) | MDIC_OP_READ);
        for _ in 0..100_000 {
            let v = self.read32(REG_MDIC);
            if v & MDIC_READY != 0 {
                return if v & MDIC_ERROR != 0 { None } else { Some((v & 0xFFFF) as u16) };
            }
            for _ in 0..100 { core::hint::spin_loop(); }
        }
        None // timeout
    }

    /// Write a PHY register via the MDI Control (MDIC) interface.
    /// Returns `true` on success, `false` on timeout or error.
    fn mdic_write(&self, phy_reg: u32, data: u16) -> bool {
        self.write32(REG_MDIC,
            MDIC_PHY1 | ((phy_reg & 0x1F) << 16) | MDIC_OP_WRITE | (data as u32));
        for _ in 0..100_000 {
            let v = self.read32(REG_MDIC);
            if v & MDIC_READY != 0 {
                return v & MDIC_ERROR == 0;
            }
            for _ in 0..100 { core::hint::spin_loop(); }
        }
        false // timeout
    }

    /// Full hardware initialisation.  Returns `true` on success.
    unsafe fn init(&mut self) -> bool {
        // 1. Disable all interrupt sources
        self.write32(REG_IMC, 0xFFFF_FFFF);

        // 2. Issue a device reset and wait for it to clear
        let ctrl = self.read32(REG_CTRL);
        self.write32(REG_CTRL, ctrl | CTRL_RST);
        for _ in 0..200_000 { core::hint::spin_loop(); }
        // Spin until RST self-clears (hardware clears it when done)
        let mut waited = 0u32;
        loop {
            if self.read32(REG_CTRL) & CTRL_RST == 0 { break; }
            core::hint::spin_loop();
            waited += 1;
            if waited > 500_000 {
                serial::serial_print("[e1000e] WARN: RST bit did not clear, continuing anyway\n");
                break;
            }
        }

        // 3. Disable interrupts again (reset re-enables them)
        self.write32(REG_IMC, 0xFFFF_FFFF);

        // 3a. PCH / I219-V PHY power-up sequence.
        //     UEFI firmware may leave CTRL_EXT.PHYPDEN set (bit 30), which
        //     forces the integrated PHY into power-down and prevents link.
        //     Clear it unconditionally so the PHY can auto-negotiate.
        let ctrl_ext = self.read32(REG_CTRL_EXT);
        if ctrl_ext & CTRL_EXT_PHYPDEN != 0 {
            serial::serial_print("[e1000e] CTRL_EXT.PHYPDEN was set — powering up PHY\n");
            self.write32(REG_CTRL_EXT, ctrl_ext & !CTRL_EXT_PHYPDEN);
            // Allow the PHY a moment to wake from power-down (~500 µs on a
            // 3 GHz CPU where PAUSE ≈ 10 ns → 50 000 × 10 ns = 500 µs).
            for _ in 0..50_000 { core::hint::spin_loop(); }
        } else {
            // Always clear the bit on PCH-family (harmless if already clear)
            self.write32(REG_CTRL_EXT, ctrl_ext & !CTRL_EXT_PHYPDEN);
        }

        // 3b. Signal to the Intel PCH Management Engine (ME) that the OS
        //     driver has loaded and is taking ownership of the device.
        //     Setting CTRL_EXT.DRV_LOAD (bit 28) prevents the ME from
        //     intercepting or re-directing received traffic, which on
        //     I217/I218/I219 controllers can silently block DHCP replies.
        {
            let ctrl_ext2 = self.read32(REG_CTRL_EXT);
            self.write32(REG_CTRL_EXT, ctrl_ext2 | CTRL_EXT_DRV_LOAD);
        }

        // 3c. Check and clear the Power-Down bit in the PHY's BMCR register
        //     (MII register 0) via the MDIC interface.  Some firmware leaves
        //     the I219-V PHY in power-down mode after the OS hand-off.
        if let Some(bmcr) = self.mdic_read(PHY_REG_BMCR) {
            if bmcr & BMCR_POWER_DOWN != 0 {
                serial::serial_print("[e1000e] PHY BMCR power-down bit set — clearing\n");
                self.mdic_write(PHY_REG_BMCR, bmcr & !BMCR_POWER_DOWN);
                // Give the PHY ~500 µs to exit power-down before configuring
                // the MAC rings (50 000 × PAUSE ≈ 500 µs @ 3 GHz).
                for _ in 0..50_000 { core::hint::spin_loop(); }
            }
        } else {
            serial::serial_print("[e1000e] WARN: MDIC read of PHY BMCR timed out\n");
        }

        // 4. Read MAC address.
        //    BIOS/UEFI firmware typically programs RAR[0]; if it looks valid
        //    we use it directly.  Otherwise fall back to the NVM.
        let rar_mac = self.mac_from_rar();
        let rar_valid = rar_mac.iter().any(|&b| b != 0)
            && rar_mac != [0xFF; 6];

        if rar_valid {
            self.mac = rar_mac;
        } else {
            if let (Some(w0), Some(w1), Some(w2)) = (
                self.nvm_read(0),
                self.nvm_read(1),
                self.nvm_read(2),
            ) {
                self.mac = [
                    (w0 & 0xFF) as u8, ((w0 >> 8) & 0xFF) as u8,
                    (w1 & 0xFF) as u8, ((w1 >> 8) & 0xFF) as u8,
                    (w2 & 0xFF) as u8, ((w2 >> 8) & 0xFF) as u8,
                ];
            } else {
                serial::serial_print("[e1000e] WARN: Could not read MAC from NVM\n");
                // Leave as all-zeros; DHCP will still work as long as the
                // smoltcp stack is given a valid EthernetAddress.
            }
        }

        // 5. General device configuration: auto-speed, full-duplex, link up
        self.write32(REG_CTRL, CTRL_SLU | CTRL_ASDE | CTRL_FD);

        // 6. Zero the Multicast Table Array (MTA) — 128 × 32-bit entries
        for i in 0..128u32 {
            self.write32(REG_MTA + i * 4, 0);
        }

        // 7. Initialise the RX descriptor ring
        let rx_ring_bytes = RX_RING_SIZE * core::mem::size_of::<RxDesc>();
        let (rx_desc_ptr, rx_desc_phys) = match memory::alloc_dma_buffer(rx_ring_bytes, 16) {
            Some(p) => p,
            None => {
                serial::serial_print("[e1000e] ERROR: RX descriptor ring alloc failed\n");
                return false;
            }
        };
        core::ptr::write_bytes(rx_desc_ptr, 0, rx_ring_bytes);
        self.rx_descs_virt = rx_desc_ptr as u64;
        self.rx_descs_phys = rx_desc_phys;

        for i in 0..RX_RING_SIZE {
            let (buf_ptr, buf_phys) = match memory::alloc_dma_buffer(PACKET_BUF_SIZE, 16) {
                Some(p) => p,
                None => {
                    serial::serial_print("[e1000e] ERROR: RX buffer alloc failed\n");
                    return false;
                }
            };
            self.rx_bufs[i] = (buf_ptr as u64, buf_phys);

            let desc = (self.rx_descs_virt as *mut RxDesc).add(i);
            write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
            write_volatile(core::ptr::addr_of_mut!((*desc).status), 0);
        }

        // Programme the ring registers
        self.write32(REG_RDBAL, rx_desc_phys as u32);
        self.write32(REG_RDBAH, (rx_desc_phys >> 32) as u32);
        self.write32(REG_RDLEN, rx_ring_bytes as u32);
        self.write32(REG_RDH, 0);
        // Give all descriptors to hardware except the last slot (ring-full sentinel)
        self.write32(REG_RDT, (RX_RING_SIZE - 1) as u32);
        self.rx_tail = 0;

        // Disable IP/TCP/UDP checksum offload.  On I219-V the NVM may leave
        // RXCSUM non-zero, which causes the hardware to set TCPE (bit 5) and
        // IPE (bit 6) in the descriptor errors byte for certain frames.  Our
        // driver treats any non-zero errors byte as a discard signal, so
        // DHCP OFFER packets can be silently dropped.  Clearing RXCSUM
        // prevents the hardware from reporting those checksum results.
        self.write32(REG_RXCSUM, 0);

        // Enable RX: unicast+broadcast+multicast accept, strip CRC, 2 KiB buffers.
        // RCTL_UPE (Unicast Promiscuous) is included to ensure that unicast DHCP
        // OFFERs from DHCP servers that do not honor the BROADCAST flag are
        // accepted even if there is any transient issue with the RAR[0] filter.
        self.write32(REG_RCTL, RCTL_EN | RCTL_UPE | RCTL_BAM | RCTL_MPE | RCTL_SECRC);

        // 8. Initialise the TX descriptor ring
        let tx_ring_bytes = TX_RING_SIZE * core::mem::size_of::<TxDesc>();
        let (tx_desc_ptr, tx_desc_phys) = match memory::alloc_dma_buffer(tx_ring_bytes, 16) {
            Some(p) => p,
            None => {
                serial::serial_print("[e1000e] ERROR: TX descriptor ring alloc failed\n");
                return false;
            }
        };
        core::ptr::write_bytes(tx_desc_ptr, 0, tx_ring_bytes);
        self.tx_descs_virt = tx_desc_ptr as u64;
        self.tx_descs_phys = tx_desc_phys;

        for i in 0..TX_RING_SIZE {
            let (buf_ptr, buf_phys) = match memory::alloc_dma_buffer(PACKET_BUF_SIZE, 16) {
                Some(p) => p,
                None => {
                    serial::serial_print("[e1000e] ERROR: TX buffer alloc failed\n");
                    return false;
                }
            };
            self.tx_bufs[i] = (buf_ptr as u64, buf_phys);

            let desc = (self.tx_descs_virt as *mut TxDesc).add(i);
            write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
            // Mark slot as done so software can use it immediately
            write_volatile(core::ptr::addr_of_mut!((*desc).status), TXD_STA_DD);
        }

        self.write32(REG_TDBAL, tx_desc_phys as u32);
        self.write32(REG_TDBAH, (tx_desc_phys >> 32) as u32);
        self.write32(REG_TDLEN, tx_ring_bytes as u32);
        self.write32(REG_TDH, 0);
        self.write32(REG_TDT, 0);
        self.tx_tail = 0;

        // Enable TX: pad short frames, standard collision settings
        const CT:   u32 = 0x0F << 4;  // Collision threshold
        const COLD: u32 = 0x40 << 12; // Collision distance (full-duplex)
        self.write32(REG_TCTL, TCTL_EN | TCTL_PSP | CT | COLD);

        // Standard inter-packet gap for 802.3 GbE
        self.write32(REG_TIPG, 0x0060_200A);

        // 9. Programme RAR[0] with our MAC and set the Address Valid bit
        let ral = (self.mac[0] as u32)
            | ((self.mac[1] as u32) << 8)
            | ((self.mac[2] as u32) << 16)
            | ((self.mac[3] as u32) << 24);
        let rah = (self.mac[4] as u32)
            | ((self.mac[5] as u32) << 8)
            | (1u32 << 31); // AV (Address Valid) bit
        self.write32(REG_RAL0, ral);
        self.write32(REG_RAH0, rah);

        // 10. Wait for the PHY to finish auto-negotiation (link-up), up to ~2 s.
        //     On real hardware the I217/I218/I219 PHY can take 1–3 seconds to
        //     establish a GbE link after powering up.  Without this wait, DHCP
        //     DISCOVERs sent before link-up fill the 32-slot TX ring with frames
        //     the hardware cannot yet transmit; by the time the link does come up
        //     smoltcp's retry state may be confused.  A short active poll here
        //     ensures the network service can start DHCP on a live link.
        //
        //     NOTE: This is a spin-poll because the scheduler is not yet running
        //     at driver-init time (sleep_ms is unavailable).  The same pattern is
        //     used by the MDIC and PHY power-up waits earlier in this function.
        //     Actual elapsed time varies with CPU speed and SMT state; the inner
        //     loop provides "enough" delay between STATUS reads without burning
        //     through all 32 TX slots as link comes up.
        serial::serial_print("[e1000e] Waiting for link (up to ~2s)...\n");
        let mut link_up = false;
        // Poll STATUS_LU every ~50 000 PAUSE iterations (varies by CPU speed).
        // 4 000 outer iterations gives ample time for GbE auto-negotiation.
        for _ in 0..4_000u32 {
            if self.read32(REG_STATUS) & STATUS_LU != 0 {
                link_up = true;
                break;
            }
            for _ in 0..50_000u32 { core::hint::spin_loop(); }
        }
        if link_up {
            serial::serial_print("[e1000e] Link UP\n");
            // PHY settling delay (~2 ms at 3 GHz): after STATUS_LU asserts, the
            // I219-V PHY can still be completing its auto-negotiation state
            // transitions.  The 50 000 PAUSE iterations below correspond to
            // roughly 1.7–5 ms depending on CPU speed (each PAUSE is ~35–100 ns
            // on modern Intel cores).  This prevents the very first received
            // frames from arriving with transient error bits that would cause
            // early DHCP OFFERs to be silently discarded.  A timer-based sleep
            // is not available here because the scheduler is not yet running.
            for _ in 0..50_000u32 { core::hint::spin_loop(); }
        } else {
            serial::serial_print("[e1000e] Link not yet up after timeout (proceeding anyway)\n");
        }

        true
    }

    /// Transmit a single Ethernet frame.
    unsafe fn send_packet(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.len() > 1514 {
            return Err("e1000e: packet exceeds MTU");
        }

        let slot = self.tx_tail;
        let desc = (self.tx_descs_virt as *mut TxDesc).add(slot);

        // Descriptor must be free (DD bit set by hardware after transmission)
        let sta = read_volatile(core::ptr::addr_of!((*desc).status));
        if sta & TXD_STA_DD == 0 {
            return Err("e1000e: TX ring full");
        }

        // Copy frame into the pre-allocated DMA buffer for this slot
        let buf_virt = self.tx_bufs[slot].0 as *mut u8;
        let buf_phys = self.tx_bufs[slot].1;
        core::ptr::copy_nonoverlapping(data.as_ptr(), buf_virt, data.len());

        // Fill the descriptor
        write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
        write_volatile(core::ptr::addr_of_mut!((*desc).length), data.len() as u16);
        write_volatile(core::ptr::addr_of_mut!((*desc).cso), 0);
        write_volatile(
            core::ptr::addr_of_mut!((*desc).cmd),
            TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS,
        );
        write_volatile(core::ptr::addr_of_mut!((*desc).status), 0); // Clear DD
        write_volatile(core::ptr::addr_of_mut!((*desc).css), 0);
        write_volatile(core::ptr::addr_of_mut!((*desc).special), 0);

        // Advance the tail and ring the doorbell
        self.tx_tail = (slot + 1) % TX_RING_SIZE;
        self.write32(REG_TDT, self.tx_tail as u32);

        Ok(())
    }

    /// Receive one Ethernet frame into `buffer`.  Returns the frame length or
    /// `None` if no frame is currently available.
    unsafe fn receive_packet(&mut self, buffer: &mut [u8]) -> Option<usize> {
        let slot = self.rx_tail;
        let desc = (self.rx_descs_virt as *mut RxDesc).add(slot);

        let status = read_volatile(core::ptr::addr_of!((*desc).status));
        if status & RXD_STA_DD == 0 {
            return None; // Hardware has not written a frame here yet
        }

        let buf_phys = self.rx_bufs[slot].1;

        // Discard partial (multi-descriptor) frames.  With PACKET_BUF_SIZE = 2 KiB
        // and MTU = 1514 bytes every frame must fit in exactly one descriptor, so
        // EOP must always be set.  If EOP is missing the hardware is in an
        // unexpected state; return the slot to hardware and skip it.
        if status & RXD_STA_EOP == 0 {
            write_volatile(core::ptr::addr_of_mut!((*desc).status), 0);
            write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
            self.write32(REG_RDT, slot as u32);
            self.rx_tail = (slot + 1) % RX_RING_SIZE;
            return None;
        }

        // Only discard frames with fatal receive errors (CRC, symbol, or DMA
        // errors — bits CE, SE, RXE in the errors byte).  Sequence errors (SEQ),
        // carrier extension errors, and checksum offload results (TCPE/IPE, which
        // are suppressed by writing RXCSUM = 0 during init) are transient or
        // informational and must not cause valid DHCP frames to be dropped.
        let errors = read_volatile(core::ptr::addr_of!((*desc).errors));
        if errors & RXD_ERR_FATAL != 0 {
            write_volatile(core::ptr::addr_of_mut!((*desc).status), 0);
            write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
            self.write32(REG_RDT, slot as u32);
            self.rx_tail = (slot + 1) % RX_RING_SIZE;
            return None; // Discard frame with fatal hardware error
        }

        let frame_len = read_volatile(core::ptr::addr_of!((*desc).length)) as usize;
        let copy_len  = core::cmp::min(frame_len, buffer.len());

        let src = self.rx_bufs[slot].0 as *const u8;
        core::ptr::copy_nonoverlapping(src, buffer.as_mut_ptr(), copy_len);

        // Return the descriptor to hardware: clear status, restore buffer addr,
        // and advance RDT so hardware knows it can reuse this slot.
        write_volatile(core::ptr::addr_of_mut!((*desc).status), 0);
        write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
        self.write32(REG_RDT, slot as u32);

        self.rx_tail = (slot + 1) % RX_RING_SIZE;
        Some(copy_len)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Public device methods (called via the NetworkDevice trait in eth.rs)
// ───────────────────────────────────────────────────────────────────────────
impl E1000EDevice {
    pub fn get_mac_address(&self) -> [u8; 6] {
        self.inner.lock().mac
    }

    pub fn send_packet(&self, data: &[u8]) -> Result<(), &'static str> {
        let mut inner = self.inner.lock();
        unsafe { inner.send_packet(data) }
    }

    pub fn receive_packet(&self, buffer: &mut [u8]) -> Option<usize> {
        let mut inner = self.inner.lock();
        unsafe { inner.receive_packet(buffer) }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// PCI initialisation — scans all devices and registers found NICs
// ───────────────────────────────────────────────────────────────────────────

/// Returns `true` if `device_id` belongs to the e1000e PCH Ethernet family.
fn is_e1000e_device(vendor_id: u16, device_id: u16) -> bool {
    vendor_id == INTEL_VENDOR_ID && E1000E_DEVICE_IDS.contains(&device_id)
}

/// Initialise all Intel e1000e-family Ethernet controllers found on the PCI bus.
/// Each successfully initialised device is registered with the global
/// `eth::NET_DEVICE_REGISTRY` so it is visible as `eth:0`, `eth:1`, …
pub fn init() {
    serial::serial_print("[e1000e] Scanning PCI for Intel Ethernet controllers...\n");

    for dev in crate::pci::get_all_devices() {
        if !is_e1000e_device(dev.vendor_id, dev.device_id) {
            continue;
        }

        serial::serial_print("[e1000e] Found Intel Ethernet: device_id=0x");
        serial::serial_print_hex(dev.device_id as u64);
        serial::serial_print(" Bus=");
        serial::serial_print_dec(dev.bus as u64);
        serial::serial_print(" Dev=");
        serial::serial_print_dec(dev.device as u64);
        serial::serial_print("\n");

        unsafe {
            // Enable memory-space decoding and bus-mastering DMA
            crate::pci::enable_device(&dev, true);

            // BAR0 is the 128 KiB memory-mapped register space
            let bar0_phys = crate::pci::get_bar(&dev, 0);
            if bar0_phys == 0 {
                serial::serial_print("[e1000e] ERROR: BAR0 is zero, skipping device\n");
                continue;
            }

            // Map the MMIO region (128 KiB = 0x20000 bytes).
            // Per the memory module note: extend length by the page-offset of
            // the physical address to avoid under-mapping across a page boundary.
            let page_offset = (bar0_phys & 0xFFF) as usize;
            let mmio_virt = crate::memory::map_mmio_range(bar0_phys, 0x2_0000 + page_offset);
            if mmio_virt == 0 {
                serial::serial_print("[e1000e] ERROR: MMIO mapping failed, skipping device\n");
                continue;
            }

            let inner = E1000EInner {
                mmio_base: mmio_virt,
                mac: [0u8; 6],
                rx_descs_virt: 0,
                rx_descs_phys: 0,
                rx_bufs: [(0, 0); RX_RING_SIZE],
                rx_tail: 0,
                tx_descs_virt: 0,
                tx_descs_phys: 0,
                tx_bufs: [(0, 0); TX_RING_SIZE],
                tx_tail: 0,
            };

            let device = E1000EDevice { inner: Mutex::new(inner) };

            // Run hardware initialisation
            if !device.inner.lock().init() {
                serial::serial_print("[e1000e] Hardware init failed, skipping device\n");
                continue;
            }

            let mac = device.get_mac_address();
            serial::serial_print("[e1000e] Initialized. MAC: ");
            for i in 0..6 {
                serial::serial_print_hex(mac[i] as u64);
                if i < 5 { serial::serial_print(":"); }
            }
            serial::serial_print("\n");

            let arc_dev: Arc<E1000EDevice> = Arc::new(device);
            E1000E_DEVICES.lock().push(arc_dev.clone());

            // Register with the eth scheme's unified device registry so the
            // network service can open it as eth:N
            crate::eth::eth_register_device(arc_dev);
        }
    }
}
