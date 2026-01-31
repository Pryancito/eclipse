//! ATA/PIANO PIO Mode Driver
//!
//! Provides basic support for reading and writing to ATA hard drives using
//! Programmed I/O (PIO) mode. Only supports the Primary Bus, Master Drive for now.

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
const ATA_CMD_READ_SECTORS: u8 = 0x20;
const ATA_CMD_WRITE_SECTORS: u8 = 0x30;
const ATA_CMD_FLUSH_CACHE: u8 = 0xE7;
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
}

impl AtaDrive {
    /// Create a new ATA driver instance
    /// 
    /// Note: This does not initialize the hardware. Call `init()` for that.
    pub const fn new(master: bool) -> Self {
        Self { master }
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
            
            // Read identification data (256 words) to clear buffer
            for _ in 0..256 {
                Self::inw(ATA_DATA_PORT);
            }
            
            crate::serial::serial_print("[ATA] Drive initialized successfully\n");
            true
        }
    }
    
    /// Read sectors from the drive (LBA28)
    /// 
    /// - `lba`: Logical Block Address (Sector index)
    /// - `buffer`: Buffer to store data. Must be a multiple of 512 bytes.
    pub fn read_sectors(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if buffer.len() % 512 != 0 {
            return Err("Buffer length must be a multiple of 512 bytes");
        }
        
        let sectors = buffer.len() / 512;
        if sectors == 0 {
            return Ok(());
        }
        
        // Limited to 255 sectors per read in LBA28
        // For simplicity, we read 1 sector at a time loop if needed, 
        // effectively implementing a simple loop.
        for i in 0..sectors {
            let offset = i * 512;
            let current_lba = lba + i as u64;
            
            match self.read_sector_lba28(current_lba, &mut buffer[offset..offset + 512]) {
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
}

/// Initialize the ATA system
pub fn init() {
    let mut drive = AtaDrive::new(true); // Master
    
    if drive.init() {
        *ATA_DRIVE.lock() = Some(drive);
    } else {
        crate::serial::serial_print("[ATA] Failed to initialize primary master drive\n");
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
