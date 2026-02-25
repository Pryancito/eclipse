//! AHCI (Advanced Host Controller Interface) Driver
//!
//! Uses the simple-ahci crate for SATA disk access via DMA.
//! Integrates with the bcache for block I/O.

use spin::Mutex;
use alloc::sync::Arc;
use simple_ahci::{AhciDriver, Hal};
use crate::{memory, serial, pci, interrupts};

/// Eclipse OS implementation of the simple-ahci HAL trait.
pub struct EclipseAhciHal;

impl Hal for EclipseAhciHal {
    fn virt_to_phys(va: usize) -> usize {
        memory::virt_to_phys(va as u64) as usize
    }

    fn current_ms() -> u64 {
        interrupts::ticks()
    }

    fn flush_dcache() {
        // x86: memory barrier to ensure DMA visibility
        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)); }
    }
}

static AHCI_DRIVER: Mutex<Option<Arc<Mutex<AhciDriver<EclipseAhciHal>>>>> = Mutex::new(None);

/// Block size used by AHCI (512 bytes per sector)
pub const AHCI_SECTOR_SIZE: usize = 512;

/// Bcache uses 4096-byte blocks = 8 sectors
const SECTORS_PER_BLOCK: u64 = 4096 / AHCI_SECTOR_SIZE as u64;

/// Initialize AHCI driver for all detected SATA controllers.
pub fn init() {
    let controllers = pci::find_all_sata_ahci();
    if controllers.is_empty() {
        serial::serial_print("[AHCI] No SATA AHCI controllers found on PCI\n");
        return;
    }

    for (idx, dev) in controllers.iter().enumerate() {
        serial::serial_print("[AHCI] Initializing controller ");
        serial::serial_print_dec(idx as u64);
        serial::serial_print("\n");
        init_controller(dev);
    }
}

fn init_controller(dev: &pci::PciDevice) {
    // AHCI ABAR is typically at BAR5 (64-bit)
    let bar5 = unsafe { pci::get_bar(dev, 5) };
    let bar0 = unsafe { pci::get_bar(dev, 0) };
    let abar = if bar5 & 1 == 0 && bar5 != 0 { bar5 } else { bar0 };
    let abar = abar & !0xF; // Mask PCI BAR flags
    if abar == 0 {
        serial::serial_print("[AHCI] No valid MMIO BAR found\n");
        return;
    }

    unsafe { pci::enable_device(dev, true); }

    // Map the whole ABAR (at least 0x1100 for 32 ports)
    let virt_base = memory::map_mmio_range(abar, 0x2000); // 8KB to be safe
    if virt_base == 0 {
        serial::serial_print("[AHCI] Failed to map MMIO\n");
        return;
    }

    // --- Global Host Control (GHC) Setup ---
    let ghc_addr = (virt_base + 0x04) as *mut u32;
    unsafe {
        let mut ghc = core::ptr::read_volatile(ghc_addr);
        serial::serial_print("[AHCI] Initial GHC: ");
        serial::serial_print_hex(ghc as u64);
        serial::serial_print("\n");
        
        // Ensure AHCI Enable (AE) is set
        ghc |= 0x80000000;
        core::ptr::write_volatile(ghc_addr, ghc);
        
        // HBA Reset (HR)
        serial::serial_print("[AHCI] Triggering HBA Reset...\n");
        core::ptr::write_volatile(ghc_addr, ghc | 0x01);
        
        let mut timeout = 1_000_000;
        while (core::ptr::read_volatile(ghc_addr) & 0x01) != 0 && timeout > 0 {
            timeout -= 1;
            core::hint::spin_loop();
        }
        
        // Restore AE after reset (some controllers clear it)
        core::ptr::write_volatile(ghc_addr, 0x80000000);
        
        if timeout == 0 {
            serial::serial_print("[AHCI] WARNING: HBA Reset timed out!\n");
        } else {
            serial::serial_print("[AHCI] HBA Reset successful\n");
        }
        
        // Wait a bit for the controller to stabilize
        for _ in 0..100000 { core::hint::spin_loop(); }
    }

    // Manual Port Scanning
    // ABAR + 0x0C: PI (Ports Implemented) - bitmask of implemented ports
    let pi = unsafe { core::ptr::read_volatile((virt_base + 0x0C) as *const u32) };
    
    serial::serial_print("[AHCI] Ports Implemented Mask: ");
    serial::serial_print_hex(pi as u64);
    serial::serial_print("\n");

    // Initialize simple-ahci ONE time per controller
    serial::serial_print("[AHCI] Calling simple-ahci try_new...\n");
    let driver = unsafe { AhciDriver::<EclipseAhciHal>::try_new(virt_base as usize) };
    serial::serial_print("[AHCI] try_new returned\n");

    let Some(mut d) = driver else {
        serial::serial_print("[AHCI] simple-ahci failed to initialize controller\n");
        return;
    };

    let arc_driver = Arc::new(Mutex::new(d));

    // Initialize global driver for legacy/direct access
    {
        let mut global_driver = AHCI_DRIVER.lock();
        *global_driver = Some(arc_driver.clone());
        serial::serial_print("[AHCI] Global driver initialized\n");
    }

    for i in 0..32 {
        if (pi >> i) & 1 == 1 {
            // Port is implemented, check if a device is present
            let ssts = unsafe { core::ptr::read_volatile((virt_base + 0x100 + (i * 0x80) + 0x28) as *const u32) };
            let det = ssts & 0x0F; // Device Detection
            
            if det == 0x03 {
                serial::serial_print("[AHCI] Device found on port ");
                serial::serial_print_dec(i as u64);
                
                // Get more status
                let sact = unsafe { core::ptr::read_volatile((virt_base + 0x100 + (i * 0x80) + 0x34) as *const u32) };
                serial::serial_print(" SSTS=");
                serial::serial_print_hex(ssts as u64);
                serial::serial_print(" SACT=");
                serial::serial_print_hex(sact as u64);
                serial::serial_print("\n");
                
                // Currently, simple-ahci might only support a single port.
                // We'll wrap the driver in an AhciDisk. 
                // If the crate is limited to one port, it will always read from that one.
                // But at least we register it as a block device.
                let disk = Arc::new(AhciDisk {
                    driver: arc_driver.clone(),
                });
                crate::storage::register_device(disk);
            }
        }
    }
}

struct AhciDisk {
    driver: Arc<Mutex<AhciDriver<EclipseAhciHal>>>,
}

impl crate::storage::BlockDevice for AhciDisk {
    fn read(&self, block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        let mut driver = self.driver.lock();
        let lba = block * SECTORS_PER_BLOCK;
        if driver.read(lba, buffer) {
            Ok(())
        } else {
            Err("AHCI read failed")
        }
    }

    fn write(&self, block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        let mut driver = self.driver.lock();
        let lba = block * SECTORS_PER_BLOCK;
        if driver.write(lba, buffer) {
            Ok(())
        } else {
            Err("AHCI write failed")
        }
    }

    fn capacity(&self) -> u64 {
        let driver = self.driver.lock();
        driver.capacity() as u64 / SECTORS_PER_BLOCK
    }

    fn name(&self) -> &'static str {
        "AHCI"
    }
}

/// Read sectors from AHCI device. Returns true on success.
pub fn read_sectors(lba: u64, buf: &mut [u8]) -> bool {
    let guard = AHCI_DRIVER.lock();
    if let Some(arc_driver) = guard.as_ref() {
        let mut driver = arc_driver.lock();
        driver.read(lba, buf)
    } else {
        false
    }
}

/// Write sectors to AHCI device. Returns true on success.
pub fn write_sectors(lba: u64, buf: &[u8]) -> bool {
    let guard = AHCI_DRIVER.lock();
    if let Some(arc_driver) = guard.as_ref() {
        let mut driver = arc_driver.lock();
        driver.write(lba, buf)
    } else {
        false
    }
}

/// Read one 4096-byte block (used by bcache). Block 0 = sectors 0-7, block 1 = 8-15, etc.
pub fn read_block(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    if buffer.len() != 4096 {
        return Err("Buffer must be 4096 bytes");
    }
    let lba = block_num * SECTORS_PER_BLOCK;
    if read_sectors(lba, buffer) {
        Ok(())
    } else {
        Err("AHCI read failed")
    }
}

/// Write one 4096-byte block.
pub fn write_block(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    if buffer.len() != 4096 {
        return Err("Buffer must be 4096 bytes");
    }
    let lba = block_num * SECTORS_PER_BLOCK;
    if write_sectors(lba, buffer) {
        Ok(())
    } else {
        Err("AHCI write failed")
    }
}

/// Check if AHCI driver is available.
pub fn is_available() -> bool {
    AHCI_DRIVER.lock().is_some()
}
