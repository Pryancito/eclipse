//! VirtIO device driver implementation
//! 
//! Implements minimal VirtIO support for block devices in QEMU/KVM environments.
//! Based on VirtIO 1.0 specification.

use core::ptr::{read_volatile, write_volatile};
use spin::Mutex;

/// VirtIO MMIO base address (typical for QEMU)
const VIRTIO_MMIO_BASE: u64 = 0x0A000000;

/// VirtIO device magic value
const VIRTIO_MAGIC: u32 = 0x74726976;

/// VirtIO device IDs
const VIRTIO_ID_BLOCK: u32 = 2;

/// VirtIO device status flags
const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
const VIRTIO_STATUS_DRIVER: u32 = 2;
const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
const VIRTIO_STATUS_FAILED: u32 = 128;

/// VirtIO MMIO register offsets
#[repr(C)]
struct VirtIOMMIORegs {
    magic_value: u32,           // 0x000
    version: u32,               // 0x004
    device_id: u32,             // 0x008
    vendor_id: u32,             // 0x00c
    device_features: u32,       // 0x010
    device_features_sel: u32,   // 0x014
    _reserved1: [u32; 2],
    driver_features: u32,       // 0x020
    driver_features_sel: u32,   // 0x024
    _reserved2: [u32; 2],
    queue_sel: u32,             // 0x030
    queue_num_max: u32,         // 0x034
    queue_num: u32,             // 0x038
    _reserved3: [u32; 2],
    queue_ready: u32,           // 0x044
    _reserved4: [u32; 2],
    queue_notify: u32,          // 0x050
    _reserved5: [u32; 3],
    interrupt_status: u32,      // 0x060
    interrupt_ack: u32,         // 0x064
    _reserved6: [u32; 2],
    status: u32,                // 0x070
    _reserved7: [u32; 3],
    queue_desc_low: u32,        // 0x080
    queue_desc_high: u32,       // 0x084
    _reserved8: [u32; 2],
    queue_driver_low: u32,      // 0x090
    queue_driver_high: u32,     // 0x094
    _reserved9: [u32; 2],
    queue_device_low: u32,      // 0x0a0
    queue_device_high: u32,     // 0x0a4
}

/// VirtIO queue descriptor
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct VirtQDescriptor {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// Descriptor flags
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

/// VirtIO available ring
#[repr(C, align(2))]
struct VirtQAvail {
    flags: u16,
    idx: u16,
    ring: [u16; 8], // Small queue for now
}

/// VirtIO used ring element
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtQUsedElem {
    id: u32,
    len: u32,
}

/// VirtIO used ring
#[repr(C, align(4))]
struct VirtQUsed {
    flags: u16,
    idx: u16,
    ring: [VirtQUsedElem; 8],
}

/// VirtIO block device driver
pub struct VirtIOBlockDevice {
    mmio_base: u64,
    queue_size: u16,
    // In a full implementation, these would be allocated:
    // descriptors: *mut VirtQDescriptor,
    // avail_ring: *mut VirtQAvail,
    // used_ring: *mut VirtQUsed,
}

static BLOCK_DEVICE: Mutex<Option<VirtIOBlockDevice>> = Mutex::new(None);

// Simulated block storage for testing (512 KB = 128 blocks of 4KB each)
static mut SIMULATED_DISK: [u8; 512 * 1024] = [0; 512 * 1024];

impl VirtIOBlockDevice {
    /// Create a new VirtIO block device from MMIO base
    unsafe fn new(mmio_base: u64) -> Option<Self> {
        let regs = mmio_base as *mut VirtIOMMIORegs;
        
        // Check magic value
        let magic = read_volatile(&(*regs).magic_value);
        if magic != VIRTIO_MAGIC {
            // No real VirtIO device, return simulated one
            crate::serial::serial_print("[VirtIO] No real device found, using simulated disk\n");
            return Some(VirtIOBlockDevice {
                mmio_base: 0,
                queue_size: 8,
            });
        }
        
        // Check version (should be 2 for VirtIO 1.0)
        let version = read_volatile(&(*regs).version);
        if version != 2 {
            return None;
        }
        
        // Check device ID
        let device_id = read_volatile(&(*regs).device_id);
        if device_id != VIRTIO_ID_BLOCK {
            return None;
        }
        
        Some(VirtIOBlockDevice {
            mmio_base,
            queue_size: 8,
        })
    }
    
    /// Create a new VirtIO block device from PCI BAR address
    unsafe fn new_from_pci(bar_addr: u64) -> Option<Self> {
        // For PCI devices, the BAR points to VirtIO registers
        // Try to detect if this is a valid VirtIO device
        // Note: PCI VirtIO devices use I/O or Memory-mapped I/O
        
        // For now, create a device with the PCI BAR as the base
        // The actual implementation would need to:
        // 1. Parse PCI capabilities to find VirtIO structures
        // 2. Setup virtqueues
        // 3. Enable DMA
        
        // Create device structure  
        Some(VirtIOBlockDevice {
            mmio_base: bar_addr,
            queue_size: 8,
        })
    }
    
    /// Initialize the VirtIO block device
    unsafe fn init(&mut self) -> bool {
        if self.mmio_base == 0 {
            // Simulated device - initialize test data
            self.init_simulated_disk();
            return true;
        }
        
        let regs = self.mmio_base as *mut VirtIOMMIORegs;
        
        // Reset device
        write_volatile(&mut (*regs).status, 0);
        
        // Set ACKNOWLEDGE status bit
        write_volatile(&mut (*regs).status, VIRTIO_STATUS_ACKNOWLEDGE);
        
        // Set DRIVER status bit
        let status = read_volatile(&(*regs).status);
        write_volatile(&mut (*regs).status, status | VIRTIO_STATUS_DRIVER);
        
        // Read and acknowledge features (for now, accept default features)
        write_volatile(&mut (*regs).device_features_sel, 0);
        let _features = read_volatile(&(*regs).device_features);
        
        write_volatile(&mut (*regs).driver_features_sel, 0);
        write_volatile(&mut (*regs).driver_features, 0);
        
        // Set FEATURES_OK
        let status = read_volatile(&(*regs).status);
        write_volatile(&mut (*regs).status, status | VIRTIO_STATUS_FEATURES_OK);
        
        // Check that device accepted our features
        let status = read_volatile(&(*regs).status);
        if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
            return false;
        }
        
        // TODO: Setup real virtqueue
        // This would involve:
        // 1. Allocate descriptor table, avail ring, used ring
        // 2. Write physical addresses to MMIO registers
        // 3. Set queue size and mark ready
        
        // Set DRIVER_OK status bit
        let status = read_volatile(&(*regs).status);
        write_volatile(&mut (*regs).status, status | VIRTIO_STATUS_DRIVER_OK);
        
        true
    }
    
    /// Initialize simulated disk with test data
    unsafe fn init_simulated_disk(&mut self) {
        use crate::serial;
        
        // Create a minimal EclipseFS header at block 0 (which maps to partition offset)
        // EclipseFS header structure (from eclipsefs-lib format.rs):
        // Magic: "ECLIPSEFS" (9 bytes)
        // Version: u32 (4 bytes) - little endian
        // Inode table offset: u64 (8 bytes) - little endian
        // Inode table size: u64 (8 bytes) - little endian
        // Total inodes: u32 (4 bytes) - little endian
        // And more fields...
        
        let mut offset = 0;
        
        // Magic number: "ECLIPSEFS"
        SIMULATED_DISK[offset..offset+9].copy_from_slice(b"ECLIPSEFS");
        offset += 9;
        
        // Version: 1.0 (0x00010000) - little endian
        let version: u32 = 0x00010000; // Major 1, Minor 0
        SIMULATED_DISK[offset..offset+4].copy_from_slice(&version.to_le_bytes());
        offset += 4;
        
        // Inode table offset: 4096 (after header) - little endian
        let inode_table_offset: u64 = 4096;
        SIMULATED_DISK[offset..offset+8].copy_from_slice(&inode_table_offset.to_le_bytes());
        offset += 8;
        
        // Inode table size: 4096 (minimal) - little endian
        let inode_table_size: u64 = 4096;
        SIMULATED_DISK[offset..offset+8].copy_from_slice(&inode_table_size.to_le_bytes());
        offset += 8;
        
        // Total inodes: 1 (just root) - little endian
        let total_inodes: u32 = 1;
        SIMULATED_DISK[offset..offset+4].copy_from_slice(&total_inodes.to_le_bytes());
        offset += 4;
        
        // Header checksum: 0 (skip for now)
        let header_checksum: u32 = 0;
        SIMULATED_DISK[offset..offset+4].copy_from_slice(&header_checksum.to_le_bytes());
        offset += 4;
        
        // Metadata checksum: 0
        let metadata_checksum: u32 = 0;
        SIMULATED_DISK[offset..offset+4].copy_from_slice(&metadata_checksum.to_le_bytes());
        offset += 4;
        
        // Data checksum: 0
        let data_checksum: u32 = 0;
        SIMULATED_DISK[offset..offset+4].copy_from_slice(&data_checksum.to_le_bytes());
        offset += 4;
        
        // Creation time: 0
        let creation_time: u64 = 0;
        SIMULATED_DISK[offset..offset+8].copy_from_slice(&creation_time.to_le_bytes());
        offset += 8;
        
        // Last check: 0
        let last_check: u64 = 0;
        SIMULATED_DISK[offset..offset+8].copy_from_slice(&last_check.to_le_bytes());
        offset += 8;
        
        // Flags: 0
        let flags: u32 = 0;
        SIMULATED_DISK[offset..offset+4].copy_from_slice(&flags.to_le_bytes());
        
        serial::serial_print("[VirtIO] Simulated disk initialized with EclipseFS header\n");
    }
    
    /// Read a block from the device
    pub fn read_block(&mut self, block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if buffer.len() < 4096 {
            return Err("Buffer too small (need 4096 bytes)");
        }
        
        if self.mmio_base == 0 {
            // Simulated read
            // The filesystem expects blocks starting at 131328 (partition offset)
            // So we map that to the start of our simulated disk
            const PARTITION_OFFSET: u64 = 131328;
            
            unsafe {
                if block_num < PARTITION_OFFSET {
                    // Block is before the partition, return zeros
                    buffer[..4096].fill(0);
                    return Ok(());
                }
                
                let relative_block = block_num - PARTITION_OFFSET;
                let offset = (relative_block as usize) * 4096;
                
                if offset + 4096 > SIMULATED_DISK.len() {
                    return Err("Block number out of range");
                }
                buffer[..4096].copy_from_slice(&SIMULATED_DISK[offset..offset + 4096]);
            }
            return Ok(());
        }
        
        // TODO: Real VirtIO block read would:
        // 1. Allocate descriptors for request header, data buffer, status
        // 2. Chain them together
        // 3. Add to available ring
        // 4. Notify device via MMIO write
        // 5. Poll used ring for completion
        // 6. Check status byte
        
        Err("Real VirtIO read not yet implemented")
    }
    
    /// Write a block to the device
    pub fn write_block(&mut self, block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if buffer.len() < 4096 {
            return Err("Buffer too small (need 4096 bytes)");
        }
        
        if self.mmio_base == 0 {
            // Simulated write
            const PARTITION_OFFSET: u64 = 131328;
            
            unsafe {
                if block_num < PARTITION_OFFSET {
                    // Block is before the partition, ignore write
                    return Ok(());
                }
                
                let relative_block = block_num - PARTITION_OFFSET;
                let offset = (relative_block as usize) * 4096;
                
                if offset + 4096 > SIMULATED_DISK.len() {
                    return Err("Block number out of range");
                }
                SIMULATED_DISK[offset..offset + 4096].copy_from_slice(&buffer[..4096]);
            }
            return Ok(());
        }
        
        // TODO: Real VirtIO block write
        Err("Real VirtIO write not yet implemented")
    }
}

/// Initialize VirtIO devices
pub fn init() {
    use crate::serial;
    
    serial::serial_print("[VirtIO] Initializing VirtIO devices...\n");
    
    // Try to find VirtIO block device on PCI bus first
    if let Some(pci_dev) = crate::pci::find_virtio_block_device() {
        serial::serial_print("[VirtIO] Found VirtIO block device on PCI\n");
        serial::serial_print("[VirtIO]   Bus=");
        serial::serial_print_dec(pci_dev.bus as u64);
        serial::serial_print(" Device=");
        serial::serial_print_dec(pci_dev.device as u64);
        serial::serial_print(" Function=");
        serial::serial_print_dec(pci_dev.function as u64);
        serial::serial_print("\n");
        
        unsafe {
            // Enable the PCI device for DMA and I/O
            crate::pci::enable_device(&pci_dev, true);
            
            // Get BAR0 for VirtIO registers
            let bar0 = crate::pci::get_bar(&pci_dev, 0);
            let bar_addr = (bar0 & !0xF) as u64;
            
            serial::serial_print("[VirtIO]   BAR0=0x");
            serial::serial_print_hex(bar_addr);
            serial::serial_print("\n");
            
            // Try to create a real VirtIO device from PCI
            if bar_addr != 0 {
                match VirtIOBlockDevice::new_from_pci(bar_addr) {
                    Some(mut device) => {
                        if device.init() {
                            serial::serial_print("[VirtIO] Real PCI device initialized successfully\n");
                            *BLOCK_DEVICE.lock() = Some(device);
                            return;
                        }
                    }
                    None => {
                        serial::serial_print("[VirtIO] Failed to create device from PCI BAR\n");
                    }
                }
            }
        }
    } else {
        serial::serial_print("[VirtIO] No VirtIO block device found on PCI bus\n");
    }
    
    // Fall back to simulated device
    serial::serial_print("[VirtIO] Falling back to simulated block device\n");
    unsafe {
        let mut device = VirtIOBlockDevice {
            mmio_base: 0,
            queue_size: 8,
        };
        
        if device.init() {
            serial::serial_print("[VirtIO] Simulated device initialized successfully\n");
            *BLOCK_DEVICE.lock() = Some(device);
        } else {
            serial::serial_print("[VirtIO] Failed to initialize simulated device\n");
        }
    }
}

/// Get a reference to the block device
pub fn get_block_device() -> Option<&'static Mutex<Option<VirtIOBlockDevice>>> {
    Some(&BLOCK_DEVICE)
}

/// Read a block from the block device
pub fn read_block(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    let mut device_lock = BLOCK_DEVICE.lock();
    if let Some(ref mut device) = *device_lock {
        device.read_block(block_num, buffer)
    } else {
        Err("No block device available")
    }
}

/// Write a block to the block device
pub fn write_block(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    let mut device_lock = BLOCK_DEVICE.lock();
    if let Some(ref mut device) = *device_lock {
        device.write_block(block_num, buffer)
    } else {
        Err("No block device available")
    }
}
