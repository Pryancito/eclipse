//! ATA/PATA PIO Mode Driver
//!
//! Provides support for reading and writing to ATA hard drives using
//! Programmed I/O (PIO) mode.
//!
//! ## Current Features
//! - LBA28 mode (supports drives up to 137GB / 2^28 sectors)
//! - LBA48 mode (supports drives up to 128PB / 2^48 sectors)
//! - Primary bus support (Master and Slave drives)
//! - PIO mode (polling-based, no DMA)
//!
//! ## Limitations
//! - No DMA support (would improve performance significantly)
//! - No interrupt-driven I/O (uses polling)
//! - No ATAPI/CD-ROM support
//! - No secondary bus support (would need ports 0x170-0x177)
//!
//! ## Future Enhancements
//! - DMA mode for better performance
//! - Interrupt-driven I/O instead of polling
//! - Secondary bus support
//! - SMART monitoring

use core::arch::asm;
use spin::Mutex;

/// ATA I/O Ports for Primary Bus
const ATA_DATA_PORT: u16 = 0x1F0;
const ATA_ERROR_PORT: u16 = 0x1F1;
const ATA_SECTOR_COUNT_PORT: u16 = 0x1F2;
const ATA_LBA_LOW_PORT: u16 = 0x1F3;
const ATA_LBA_MID_PORT: u16 = 0x1F4;
const ATA_LBA_HIGH_PORT: u16 = 0x1F5;
const ATA_DRIVE_HEAD_PORT: u16 = 0x1F6;
const ATA_COMMAND_PORT: u16 = 0x1F7; // Write
const ATA_STATUS_PORT: u16 = 0x1F7;  // Read

/// ATA Commands
const ATA_CMD_READ_SECTORS: u8 = 0x20;      // Read sectors (LBA28)
const ATA_CMD_READ_SECTORS_EXT: u8 = 0x24;  // Read sectors (LBA48)
const ATA_CMD_WRITE_SECTORS: u8 = 0x30;     // Write sectors (LBA28)
const ATA_CMD_WRITE_SECTORS_EXT: u8 = 0x34; // Write sectors (LBA48)
const ATA_CMD_FLUSH_CACHE: u8 = 0xE7;
const ATA_CMD_FLUSH_CACHE_EXT: u8 = 0xEA;   // Flush cache (LBA48)
const ATA_CMD_IDENTIFY: u8 = 0xEC;

/// Status Register Bits
const STATUS_BSY: u8 = 0x80; // Busy
const STATUS_DRDY: u8 = 0x40; // Drive Ready
const STATUS_DRQ: u8 = 0x08; // Data Request
const STATUS_ERR: u8 = 0x01; // Error

/// Global instance of the ATA Driver
pub static ATA_DRIVE: Mutex<Option<AtaDrive>> = Mutex::new(None);

/// ATA Drive Structure
pub struct AtaDrive {
    /// Is this the master drive?
    master: bool,
    /// Does the drive support LBA48?
    lba48_supported: bool,
    /// Maximum addressable LBA (for capacity detection)
    max_lba: u64,
}

impl AtaDrive {
    /// Create a new ATA driver instance
    /// 
    /// Note: This does not initialize the hardware. Call `init()` for that.
    pub const fn new(master: bool) -> Self {
        Self { 
            master,
            lba48_supported: false,
            max_lba: 0,
        }
    }

    /// Read data from a port
    unsafe fn inb(port: u16) -> u8 {
        let result: u8;
        asm!("in al, dx", out("al") result, in("dx") port);
        result
    }

    /// Write data to a port
    unsafe fn outb(port: u16, data: u8) {
        asm!("out dx, al", in("dx") port, in("al") data);
    }

    /// Read 16-bit word from a port
    unsafe fn inw(port: u16) -> u16 {
        let result: u16;
        asm!("in ax, dx", out("ax") result, in("dx") port);
        result
    }
    
    /// Write 16-bit word to a port
    unsafe fn outw(port: u16, data: u16) {
        asm!("out dx, ax", in("dx") port, in("ax") data);
    }
    
    /// Initialize the drive
    /// Returns true if successful, false otherwise
    pub fn init(&mut self) -> bool {
        unsafe {
            // Select drive (0xA0 for master, 0xB0 for slave)
            // Note: We are using the primary bus
            Self::outb(ATA_DRIVE_HEAD_PORT, if self.master { 0xA0 } else { 0xB0 });
            
            // Wait for 400ns logic or just a tiny delay
            for _ in 0..4 {
                Self::inb(ATA_STATUS_PORT);
            }
            
            // Check if drive exists by identifying it
            Self::outb(ATA_SECTOR_COUNT_PORT, 0);
            Self::outb(ATA_LBA_LOW_PORT, 0);
            Self::outb(ATA_LBA_MID_PORT, 0);
            Self::outb(ATA_LBA_HIGH_PORT, 0);
            
            Self::outb(ATA_COMMAND_PORT, ATA_CMD_IDENTIFY);
            
            let status = Self::inb(ATA_STATUS_PORT);
            if status == 0 {
                return false; // Drive does not exist
            }
            
            // Poll until BSY is clear
            while Self::inb(ATA_STATUS_PORT) & STATUS_BSY != 0 {}
            
            // Check LBA mid/high ports to see if it's ATAPI and not ATA
            let mid = Self::inb(ATA_LBA_MID_PORT);
            let high = Self::inb(ATA_LBA_HIGH_PORT);
            
            if mid != 0 || high != 0 {
                // Not a standard ATA drive (likely ATAPI/SATA)
                // For simplicity, we assume we want ATA
                return false;
            }
            
            // Poll until DRQ or ERR is set
            loop {
                let status = Self::inb(ATA_STATUS_PORT);
                if status & STATUS_ERR != 0 {
                    return false; // Error identifying
                }
                if status & STATUS_DRQ != 0 {
                    break;
                }
            }
            
            // Read identification data (256 words = 512 bytes)
            let mut identify_data = [0u16; 256];
            for i in 0..256 {
                identify_data[i] = Self::inw(ATA_DATA_PORT);
            }
            
            // Check for LBA48 support (word 83, bit 10)
            self.lba48_supported = (identify_data[83] & (1 << 10)) != 0;
            
            // Get maximum LBA
            if self.lba48_supported {
                // LBA48: words 100-103 (64-bit value)
                self.max_lba = identify_data[100] as u64
                    | ((identify_data[101] as u64) << 16)
                    | ((identify_data[102] as u64) << 32)
                    | ((identify_data[103] as u64) << 48);
            } else {
                // LBA28: words 60-61 (32-bit value)
                self.max_lba = identify_data[60] as u64
                    | ((identify_data[61] as u64) << 16);
            }
            
            crate::serial::serial_print("[ATA] Drive initialized successfully\n");
            crate::serial::serial_print("[ATA]   LBA48 support: ");
            crate::serial::serial_print(if self.lba48_supported { "Yes" } else { "No" });
            crate::serial::serial_print("\n[ATA]   Max LBA: ");
            crate::serial::serial_print_dec(self.max_lba);
            crate::serial::serial_print("\n[ATA]   Capacity: ~");
            crate::serial::serial_print_dec((self.max_lba * 512) / (1024 * 1024)); // MB
            crate::serial::serial_print(" MB\n");
            true
        }
    }
    
    /// Read sectors from the drive (supports both LBA28 and LBA48)
    /// 
    /// - `lba`: Logical Block Address (Sector index)
    /// - `buffer`: Buffer to store data. Must be a multiple of 512 bytes.
    /// 
    /// Automatically uses LBA48 mode for LBAs > 0x0FFFFFFF if supported.
    pub fn read_sectors(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if buffer.len() % 512 != 0 {
            return Err("Buffer length must be a multiple of 512 bytes");
        }
        
        let sectors = buffer.len() / 512;
        if sectors == 0 {
            return Ok(());
        }
        
        // Check if LBA is within drive capacity
        if lba >= self.max_lba {
            return Err("LBA out of range for this drive");
        }
        
        // Read sectors one at a time
        for i in 0..sectors {
            let offset = i * 512;
            let current_lba = lba + i as u64;
            
            // Choose LBA28 or LBA48 based on LBA value and drive support
            let result = if current_lba > 0x0FFFFFFF {
                if !self.lba48_supported {
                    return Err("LBA48 required but not supported by drive");
                }
                self.read_sector_lba48(current_lba, &mut buffer[offset..offset + 512])
            } else {
                self.read_sector_lba28(current_lba, &mut buffer[offset..offset + 512])
            };
            
            match result {
                Ok(_) => {},
                Err(e) => return Err(e),
            }
        }
        
        Ok(())
    }
    
    /// Read a single sector using LBA28
    fn read_sector_lba28(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if lba > 0x0FFFFFFF {
            return Err("LBA28 out of range");
        }
        
        unsafe {
            // Wait for drive to be ready
            while Self::inb(ATA_STATUS_PORT) & STATUS_BSY != 0 {}
            
            // Select drive and bits 24-27 of LBA
            // 0xE0 = LBA Mode (bit 6) + 0xA0 (Drive select)
            let drive_head = 0xE0 | ((if self.master { 0 } else { 1 }) << 4) | ((lba >> 24) & 0x0F) as u8;
            Self::outb(ATA_DRIVE_HEAD_PORT, drive_head);
            
            // Send NULL byte to Error port for delay
            Self::outb(ATA_ERROR_PORT, 0);
            
            // Set sector count to 1
            Self::outb(ATA_SECTOR_COUNT_PORT, 1);
            
            // Send LBA bits 0-7
            Self::outb(ATA_LBA_LOW_PORT, lba as u8);
            
            // Send LBA bits 8-15
            Self::outb(ATA_LBA_MID_PORT, (lba >> 8) as u8);
            
            // Send LBA bits 16-23
            Self::outb(ATA_LBA_HIGH_PORT, (lba >> 16) as u8);
            
            // Send Read Command
            Self::outb(ATA_COMMAND_PORT, ATA_CMD_READ_SECTORS);
            
            // Wait for data
            loop {
                let status = Self::inb(ATA_STATUS_PORT);
                if status & STATUS_ERR != 0 {
                    return Err("ATA Read Error");
                }
                if status & STATUS_DRQ != 0 {
                    break;
                }
            }
            
            // Read 256 words (512 bytes)
            for i in 0..256 {
                let data = Self::inw(ATA_DATA_PORT);
                buffer[i * 2] = data as u8;
                buffer[i * 2 + 1] = (data >> 8) as u8;
            }
        }
        
        Ok(())
    }
    
    /// Read a single sector using LBA48
    fn read_sector_lba48(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if lba > 0x0000FFFFFFFFFFFF {
            return Err("LBA48 out of range (max 48-bit)");
        }
        
        unsafe {
            // Wait for drive to be ready
            while Self::inb(ATA_STATUS_PORT) & STATUS_BSY != 0 {}
            
            // Select drive
            let drive_select = if self.master { 0x40 } else { 0x50 }; // LBA mode bit 6
            Self::outb(ATA_DRIVE_HEAD_PORT, drive_select);
            
            // LBA48 uses a special sequence: write high bytes, then low bytes
            
            // Sector count high byte (we're reading 1 sector, so 0)
            Self::outb(ATA_SECTOR_COUNT_PORT, 0);
            
            // LBA high bytes (bits 24-47)
            Self::outb(ATA_LBA_LOW_PORT, (lba >> 24) as u8);
            Self::outb(ATA_LBA_MID_PORT, (lba >> 32) as u8);
            Self::outb(ATA_LBA_HIGH_PORT, (lba >> 40) as u8);
            
            // Sector count low byte (1 sector)
            Self::outb(ATA_SECTOR_COUNT_PORT, 1);
            
            // LBA low bytes (bits 0-23)
            Self::outb(ATA_LBA_LOW_PORT, lba as u8);
            Self::outb(ATA_LBA_MID_PORT, (lba >> 8) as u8);
            Self::outb(ATA_LBA_HIGH_PORT, (lba >> 16) as u8);
            
            // Send Read Command (LBA48 extended)
            Self::outb(ATA_COMMAND_PORT, ATA_CMD_READ_SECTORS_EXT);
            
            // Wait for data
            loop {
                let status = Self::inb(ATA_STATUS_PORT);
                if status & STATUS_ERR != 0 {
                    return Err("ATA Read Error (LBA48)");
                }
                if status & STATUS_DRQ != 0 {
                    break;
                }
            }
            
            // Read 256 words (512 bytes)
            for i in 0..256 {
                let data = Self::inw(ATA_DATA_PORT);
                buffer[i * 2] = data as u8;
                buffer[i * 2 + 1] = (data >> 8) as u8;
            }
        }
        
        Ok(())
    }
}

/// Initialize the ATA system
/// Tries to initialize both master and slave drives on the primary bus
pub fn init() {
    crate::serial::serial_print("[ATA] Initializing ATA subsystem...\n");
    
    // Try master drive first
    let mut master = AtaDrive::new(true);
    if master.init() {
        *ATA_DRIVE.lock() = Some(master);
        return; // Master drive found and initialized
    }
    
    // Try slave drive if master failed
    crate::serial::serial_print("[ATA] Master drive not found, trying slave...\n");
    let mut slave = AtaDrive::new(false);
    if slave.init() {
        *ATA_DRIVE.lock() = Some(slave);
    } else {
        crate::serial::serial_print("[ATA] No ATA drives found on primary bus\n");
    }
}

/// Read block(s) from ATA drive
/// Note: EclipseFS uses 4096 byte blocks, ATA uses 512 byte sectors.
/// We need to read 8 sectors per block.
pub fn read_block(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    if buffer.len() < 4096 {
        return Err("Buffer too small for block read");
    }
    
    let mut drive_lock = ATA_DRIVE.lock();
    if let Some(ref mut drive) = *drive_lock {
        // Convert block (4096) to LBA (512)
        // 1 block = 8 sectors
        let lba_start = block_num * 8;
        
        crate::serial::serial_print("[ATA] reading block ");
        crate::serial::serial_print_dec(block_num);
        crate::serial::serial_print(" (LBA ");
        crate::serial::serial_print_dec(lba_start);
        crate::serial::serial_print(")...\n");
        
        // Read 8 sectors
        drive.read_sectors(lba_start, &mut buffer[..4096])
    } else {
        Err("No ATA drive available")
    }
}
