//! ATA/PATA PIO Mode Driver
//!
//! Provides support for reading and writing to ATA hard drives using
//! Programmed I/O (PIO) mode.
//!
//! ## Current Features
//! - LBA28 mode (supports drives up to 137 GB)
//! - LBA48 mode (supports drives up to 128 PB)
//! - Primary bus   (I/O base 0x1F0, master and slave)
//! - Secondary bus (I/O base 0x170, master and slave)
//! - PIO read and write with FLUSH CACHE after every write
//!
//! ## Limitations
//! - No DMA support (would improve performance significantly)
//! - No interrupt-driven I/O (polling based)
//! - No ATAPI/CD-ROM support

use core::arch::asm;
use spin::Mutex;
use alloc::sync::Arc;
use alloc::vec::Vec;

// ── I/O port offsets from bus base address ────────────────────────────────────
// Primary bus base:   0x1F0   control port: base + 0x206 = 0x3F6
// Secondary bus base: 0x170   control port: base + 0x206 = 0x376

const OFF_DATA:         u16 = 0; // 16-bit data register
const OFF_FEATURES:     u16 = 1; // write: features; read: error
const OFF_SECTOR_COUNT: u16 = 2;
const OFF_LBA_LOW:      u16 = 3;
const OFF_LBA_MID:      u16 = 4;
const OFF_LBA_HIGH:     u16 = 5;
const OFF_DRIVE_HEAD:   u16 = 6;
const OFF_CMD_STATUS:   u16 = 7; // write: command; read: status
const OFF_CONTROL:      u16 = 0x206; // alt-status / control (does not clear IRQ pending)

// ── ATA Commands ─────────────────────────────────────────────────────────────
const ATA_CMD_READ_SECTORS:     u8 = 0x20; // PIO read  (LBA28)
const ATA_CMD_READ_SECTORS_EXT: u8 = 0x24; // PIO read  (LBA48)
const ATA_CMD_WRITE_SECTORS:    u8 = 0x30; // PIO write (LBA28)
const ATA_CMD_WRITE_SECTORS_EXT: u8 = 0x34; // PIO write (LBA48)
const ATA_CMD_FLUSH_CACHE:      u8 = 0xE7; // Flush volatile write cache (LBA28)
const ATA_CMD_FLUSH_CACHE_EXT:  u8 = 0xEA; // Flush volatile write cache (LBA48)
const ATA_CMD_IDENTIFY:         u8 = 0xEC;

// -- Status Register Bits -----------------------------------------------------
const STATUS_BSY:  u8 = 0x80; // Busy - do not access other registers when set
const STATUS_DRDY: u8 = 0x40; // Drive Ready - must be 1 before issuing a command
const STATUS_DRQ:  u8 = 0x08; // Data Request - data can be transferred
const STATUS_ERR:  u8 = 0x01; // Error - see the Error register for details

// -- Poll budget --------------------------------------------------------------
/// Maximum spin iterations for BSY / DRDY / DRQ polls (approx 1-5 seconds on real HW).
const ATA_POLL_LIMIT: usize = 5_000_000;

// ── AtaDrive ─────────────────────────────────────────────────────────────────

/// One ATA drive (master or slave on primary or secondary bus).
pub struct AtaDrive {
    /// Base I/O port of the bus (0x1F0 = primary, 0x170 = secondary).
    base_port: u16,
    /// Master (true) or Slave (false).
    master: bool,
    /// Supports LBA48 (drives > 137 GB).
    lba48_supported: bool,
    /// Total addressable sectors (used for range checks).
    max_lba: u64,
    /// Human-readable label printed during init.
    label: &'static str,
}

impl AtaDrive {
    pub const fn new(base_port: u16, master: bool, label: &'static str) -> Self {
        Self { base_port, master, lba48_supported: false, max_lba: 0, label }
    }

    // ── Port accessors ────────────────────────────────────────────────────────

    #[inline] fn data_port(&self)         -> u16 { self.base_port + OFF_DATA }
    #[inline] fn features_port(&self)     -> u16 { self.base_port + OFF_FEATURES }
    #[inline] fn sector_count_port(&self) -> u16 { self.base_port + OFF_SECTOR_COUNT }
    #[inline] fn lba_low_port(&self)      -> u16 { self.base_port + OFF_LBA_LOW }
    #[inline] fn lba_mid_port(&self)      -> u16 { self.base_port + OFF_LBA_MID }
    #[inline] fn lba_high_port(&self)     -> u16 { self.base_port + OFF_LBA_HIGH }
    #[inline] fn drive_head_port(&self)   -> u16 { self.base_port + OFF_DRIVE_HEAD }
    #[inline] fn command_port(&self)      -> u16 { self.base_port + OFF_CMD_STATUS }
    #[inline] fn status_port(&self)       -> u16 { self.base_port + OFF_CMD_STATUS }
    #[inline] fn alt_status_port(&self)   -> u16 { self.base_port + OFF_CONTROL }

    // ── Raw I/O helpers ───────────────────────────────────────────────────────

    #[inline]
    unsafe fn inb(port: u16) -> u8 {
        let v: u8;
        asm!("in al, dx", out("al") v, in("dx") port, options(nostack, nomem, preserves_flags));
        v
    }

    #[inline]
    unsafe fn outb(port: u16, val: u8) {
        asm!("out dx, al", in("dx") port, in("al") val, options(nostack, nomem, preserves_flags));
    }

    #[inline]
    unsafe fn inw(port: u16) -> u16 {
        let v: u16;
        asm!("in ax, dx", out("ax") v, in("dx") port, options(nostack, nomem, preserves_flags));
        v
    }

    #[inline]
    unsafe fn outw(port: u16, val: u16) {
        asm!("out dx, ax", in("dx") port, in("ax") val, options(nostack, nomem, preserves_flags));
    }

    // ── Poll helpers ──────────────────────────────────────────────────────────

    /// 400 ns delay: read alt-status 4× (each ISA I/O takes ≈ 100 ns).
    unsafe fn delay_400ns(&self) {
        for _ in 0..4 { Self::inb(self.alt_status_port()); }
    }

    /// Wait until BSY=0 **and** DRDY=1.  Must be satisfied before issuing
    /// any ATA command other than DEVICE RESET.
    fn wait_for_ready(&self) -> Result<(), &'static str> {
        let mut i = ATA_POLL_LIMIT;
        loop {
            let s = unsafe { Self::inb(self.status_port()) };
            if s == 0xFF { return Err("ATA floating bus"); }
            if s & STATUS_BSY == 0 && s & STATUS_DRDY != 0 { return Ok(()); }
            if i == 0 { return Err("ATA timeout waiting for DRDY"); }
            i -= 1;
            core::hint::spin_loop();
        }
    }

    /// Wait until BSY=0.  Used after IDENTIFY (DRDY may not be set yet).
    fn wait_for_bsy_clear(&self) -> Result<(), &'static str> {
        let mut i = ATA_POLL_LIMIT;
        loop {
            let s = unsafe { Self::inb(self.status_port()) };
            if s == 0xFF { return Err("ATA floating bus"); }
            if s & STATUS_BSY == 0 { return Ok(()); }
            if i == 0 { return Err("ATA timeout waiting for BSY clear"); }
            i -= 1;
            core::hint::spin_loop();
        }
    }

    /// Wait until DRQ=1 or ERR=1.
    fn wait_for_drq(&self) -> Result<(), &'static str> {
        let mut i = ATA_POLL_LIMIT;
        loop {
            let s = unsafe { Self::inb(self.status_port()) };
            if s == 0xFF { return Err("ATA floating bus"); }
            if s & STATUS_ERR != 0 { return Err("ATA error bit set"); }
            if s & STATUS_DRQ != 0 { return Ok(()); }
            if i == 0 { return Err("ATA timeout waiting for DRQ"); }
            i -= 1;
            core::hint::spin_loop();
        }
    }

    // ── Drive init / IDENTIFY ─────────────────────────────────────────────────

    /// Probe the drive via IDENTIFY DEVICE.  Returns `true` if a drive is found.
    pub fn init(&mut self) -> bool {
        unsafe {
            // Select drive (0xA0 master, 0xB0 slave) with LBA mode.
            Self::outb(self.drive_head_port(), if self.master { 0xA0 } else { 0xB0 });
            self.delay_400ns();

            // Zero address registers, then send IDENTIFY.
            Self::outb(self.sector_count_port(), 0);
            Self::outb(self.lba_low_port(), 0);
            Self::outb(self.lba_mid_port(), 0);
            Self::outb(self.lba_high_port(), 0);
            Self::outb(self.command_port(), ATA_CMD_IDENTIFY);

            // If status is 0 immediately, no drive is present.
            let status = Self::inb(self.status_port());
            if status == 0 || status == 0xFF { return false; }

            // Wait for BSY to clear (DRDY may not be set during IDENTIFY).
            if self.wait_for_bsy_clear().is_err() { return false; }

            // LBA_MID / LBA_HIGH ≠ 0 → ATAPI or packet device; skip.
            let mid  = Self::inb(self.lba_mid_port());
            let high = Self::inb(self.lba_high_port());
            if mid != 0 || high != 0 { return false; }

            // Wait for DRQ (data ready to read).
            if self.wait_for_drq().is_err() { return false; }

            // Read 256 words of IDENTIFY data.
            let mut id = [0u16; 256];
            for w in id.iter_mut() { *w = Self::inw(self.data_port()); }

            // Word 83 bit 10 → LBA48 command set supported.
            self.lba48_supported = (id[83] & (1 << 10)) != 0;

            // Capacity: words 100–103 (LBA48) or words 60–61 (LBA28).
            self.max_lba = if self.lba48_supported {
                id[100] as u64
                    | ((id[101] as u64) << 16)
                    | ((id[102] as u64) << 32)
                    | ((id[103] as u64) << 48)
            } else {
                id[60] as u64 | ((id[61] as u64) << 16)
            };

            if self.max_lba == 0 { return false; }

            crate::serial::serial_print("[ATA] ");
            crate::serial::serial_print(self.label);
            crate::serial::serial_print(" ready — LBA48=");
            crate::serial::serial_print(if self.lba48_supported { "yes" } else { "no" });
            crate::serial::serial_print(" max_lba=");
            crate::serial::serial_print_dec(self.max_lba);
            crate::serial::serial_print(" (~");
            crate::serial::serial_print_dec((self.max_lba * 512) / (1024 * 1024));
            crate::serial::serial_print(" MiB)\n");
        }
        true
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    /// Read `buffer.len() / 512` consecutive sectors starting at `lba`.
    pub fn read_sectors(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if buffer.len() % 512 != 0 { return Err("ATA read: buffer not sector-aligned"); }
        let sectors = buffer.len() / 512;
        for i in 0..sectors {
            let cur = lba + i as u64;
            if cur >= self.max_lba { return Err("ATA LBA out of range"); }
            let buf = &mut buffer[i * 512..(i + 1) * 512];
            if cur > 0x0FFF_FFFF && self.lba48_supported {
                self.read_sector_lba48(cur, buf)?;
            } else {
                self.read_sector_lba28(cur, buf)?;
            }
        }
        Ok(())
    }

    fn read_sector_lba28(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        unsafe {
            self.wait_for_ready()?;
            let dh = 0xE0
                | (if self.master { 0u8 } else { 0x10 })
                | ((lba >> 24) as u8 & 0x0F);
            Self::outb(self.drive_head_port(), dh);
            Self::outb(self.features_port(), 0);
            Self::outb(self.sector_count_port(), 1);
            Self::outb(self.lba_low_port(),  lba as u8);
            Self::outb(self.lba_mid_port(),  (lba >> 8) as u8);
            Self::outb(self.lba_high_port(), (lba >> 16) as u8);
            Self::outb(self.command_port(), ATA_CMD_READ_SECTORS);
            self.delay_400ns();
            self.wait_for_drq()?;
            for i in 0..256 {
                let w = Self::inw(self.data_port());
                buf[i * 2]     = w as u8;
                buf[i * 2 + 1] = (w >> 8) as u8;
            }
        }
        Ok(())
    }

    fn read_sector_lba48(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        unsafe {
            self.wait_for_ready()?;
            // LBA48: write HIGH bytes first, then LOW bytes (each register is
            // a 2-deep FIFO — the controller latches the previous write as the
            // high byte when the low byte is written).
            let dh = if self.master { 0x40 } else { 0x50 }; // LBA mode, no bits 24-27
            Self::outb(self.drive_head_port(), dh);
            // High bytes (previous):
            Self::outb(self.sector_count_port(), 0);               // count[15:8] = 0
            Self::outb(self.lba_low_port(),  (lba >> 24) as u8);  // LBA[31:24]
            Self::outb(self.lba_mid_port(),  (lba >> 32) as u8);  // LBA[39:32]
            Self::outb(self.lba_high_port(), (lba >> 40) as u8);  // LBA[47:40]
            // Low bytes (current):
            Self::outb(self.sector_count_port(), 1);               // count[7:0]  = 1
            Self::outb(self.lba_low_port(),   lba        as u8);  // LBA[7:0]
            Self::outb(self.lba_mid_port(),  (lba >>  8) as u8);  // LBA[15:8]
            Self::outb(self.lba_high_port(), (lba >> 16) as u8);  // LBA[23:16]
            Self::outb(self.command_port(), ATA_CMD_READ_SECTORS_EXT);
            self.delay_400ns();
            self.wait_for_drq()?;
            for i in 0..256 {
                let w = Self::inw(self.data_port());
                buf[i * 2]     = w as u8;
                buf[i * 2 + 1] = (w >> 8) as u8;
            }
        }
        Ok(())
    }

    // ── Write ─────────────────────────────────────────────────────────────────

    /// Write `buffer.len() / 512` consecutive sectors starting at `lba`,
    /// then issue FLUSH CACHE to commit the write to persistent storage.
    pub fn write_sectors(&mut self, lba: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if buffer.len() % 512 != 0 { return Err("ATA write: buffer not sector-aligned"); }
        let sectors = buffer.len() / 512;
        for i in 0..sectors {
            let cur = lba + i as u64;
            if cur >= self.max_lba { return Err("ATA LBA out of range"); }
            let buf = &buffer[i * 512..(i + 1) * 512];
            if cur > 0x0FFF_FFFF && self.lba48_supported {
                self.write_sector_lba48(cur, buf)?;
            } else {
                self.write_sector_lba28(cur, buf)?;
            }
        }
        // Flush the drive's volatile write cache.
        self.flush_cache()
    }

    fn write_sector_lba28(&mut self, lba: u64, data: &[u8]) -> Result<(), &'static str> {
        unsafe {
            self.wait_for_ready()?;
            let dh = 0xE0
                | (if self.master { 0u8 } else { 0x10 })
                | ((lba >> 24) as u8 & 0x0F);
            Self::outb(self.drive_head_port(), dh);
            Self::outb(self.features_port(), 0);
            Self::outb(self.sector_count_port(), 1);
            Self::outb(self.lba_low_port(),  lba as u8);
            Self::outb(self.lba_mid_port(),  (lba >> 8) as u8);
            Self::outb(self.lba_high_port(), (lba >> 16) as u8);
            Self::outb(self.command_port(), ATA_CMD_WRITE_SECTORS);
            self.delay_400ns();
            self.wait_for_drq()?;
            for i in 0..256 {
                let w = data[i * 2] as u16 | ((data[i * 2 + 1] as u16) << 8);
                Self::outw(self.data_port(), w);
            }
            // Wait for the drive to commit the sector to its cache.
            self.wait_for_bsy_clear()?;
            let s = Self::inb(self.status_port());
            if s & STATUS_ERR != 0 { return Err("ATA write error (LBA28)"); }
        }
        Ok(())
    }

    fn write_sector_lba48(&mut self, lba: u64, data: &[u8]) -> Result<(), &'static str> {
        unsafe {
            self.wait_for_ready()?;
            let dh = if self.master { 0x40 } else { 0x50 };
            Self::outb(self.drive_head_port(), dh);
            // High bytes first:
            Self::outb(self.sector_count_port(), 0);
            Self::outb(self.lba_low_port(),  (lba >> 24) as u8);
            Self::outb(self.lba_mid_port(),  (lba >> 32) as u8);
            Self::outb(self.lba_high_port(), (lba >> 40) as u8);
            // Low bytes:
            Self::outb(self.sector_count_port(), 1);
            Self::outb(self.lba_low_port(),   lba        as u8);
            Self::outb(self.lba_mid_port(),  (lba >>  8) as u8);
            Self::outb(self.lba_high_port(), (lba >> 16) as u8);
            // Use WRITE SECTORS EXT (0x34) for LBA48 writes.
            Self::outb(self.command_port(), ATA_CMD_WRITE_SECTORS_EXT);
            self.delay_400ns();
            self.wait_for_drq()?;
            for i in 0..256 {
                let w = data[i * 2] as u16 | ((data[i * 2 + 1] as u16) << 8);
                Self::outw(self.data_port(), w);
            }
            self.wait_for_bsy_clear()?;
            let s = Self::inb(self.status_port());
            if s & STATUS_ERR != 0 { return Err("ATA write error (LBA48)"); }
        }
        Ok(())
    }

    /// Issue FLUSH CACHE / FLUSH CACHE EXT to commit buffered writes.
    fn flush_cache(&mut self) -> Result<(), &'static str> {
        unsafe {
            self.wait_for_ready()?;
            let cmd = if self.lba48_supported {
                ATA_CMD_FLUSH_CACHE_EXT
            } else {
                ATA_CMD_FLUSH_CACHE
            };
            Self::outb(self.command_port(), cmd);
            self.wait_for_bsy_clear()?;
            let s = Self::inb(self.status_port());
            if s & STATUS_ERR != 0 { return Err("ATA flush cache error"); }
        }
        Ok(())
    }
}

// ── Global drive registry ────────────────────────────────────────────────────

/// All successfully initialised ATA drives, in probe order.
static ATA_DRIVES: Mutex<Vec<AtaDrive>> = Mutex::new(Vec::new());

// ── AtaDisk — implements storage::BlockDevice for one drive ──────────────────

struct AtaDisk {
    /// Index into `ATA_DRIVES`.
    drive_idx: usize,
}

impl crate::storage::BlockDevice for AtaDisk {
    fn read(&self, block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if buffer.len() != 4096 { return Err("ATA read: buffer must be 4096 bytes"); }
        let lba = block * 8; // 4096-byte block = 8 × 512-byte sectors
        let mut drives = ATA_DRIVES.lock();
        let drive = drives.get_mut(self.drive_idx).ok_or("ATA: drive index invalid")?;
        drive.read_sectors(lba, buffer)
    }

    fn write(&self, block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if buffer.len() != 4096 { return Err("ATA write: buffer must be 4096 bytes"); }
        let lba = block * 8;
        let mut drives = ATA_DRIVES.lock();
        let drive = drives.get_mut(self.drive_idx).ok_or("ATA: drive index invalid")?;
        drive.write_sectors(lba, buffer)
    }

    fn capacity(&self) -> u64 {
        ATA_DRIVES.lock()
            .get(self.drive_idx)
            .map(|d| d.max_lba / 8) // 4096-byte blocks
            .unwrap_or(0)
    }

    fn name(&self) -> &'static str { "ATA" }
}

// ── Public init ──────────────────────────────────────────────────────────────

/// Probe all ATA positions and register each found drive as a block device.
///
/// Four positions are tried in order:
///   1. Primary bus, master   (base 0x1F0)
///   2. Primary bus, slave    (base 0x1F0)
///   3. Secondary bus, master (base 0x170)
///   4. Secondary bus, slave  (base 0x170)
pub fn init() {
    crate::serial::serial_print("[ATA] Probing ATA buses...\n");

    let candidates: [(&'static str, u16, bool); 4] = [
        ("primary-master",   0x1F0, true),
        ("primary-slave",    0x1F0, false),
        ("secondary-master", 0x170, true),
        ("secondary-slave",  0x170, false),
    ];

    for (label, base, master) in &candidates {
        let mut drive = AtaDrive::new(*base, *master, label);
        if drive.init() {
            let mut drives = ATA_DRIVES.lock();
            let idx = drives.len();
            drives.push(drive);
            drop(drives); // release lock before calling into storage
            crate::storage::register_device(Arc::new(AtaDisk { drive_idx: idx }));
        }
    }

    if ATA_DRIVES.lock().is_empty() {
        crate::serial::serial_print("[ATA] No ATA drives found\n");
    }
}

// ── Legacy helpers ───────────────────────────────────────────────────────────

/// Returns `true` if at least one ATA drive has been registered.
pub fn is_available() -> bool {
    !ATA_DRIVES.lock().is_empty()
}

/// Read one 4096-byte block from the first registered ATA drive.
pub fn read_block(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    if buffer.len() != 4096 { return Err("Buffer must be 4096 bytes"); }
    let lba = block_num * 8;
    let mut drives = ATA_DRIVES.lock();
    let drive = drives.first_mut().ok_or("No ATA drive available")?;
    drive.read_sectors(lba, buffer)
}
