//! VirtIO device driver implementation
//! 
//! Implements minimal VirtIO support for block devices in QEMU/KVM environments.
//! Based on VirtIO 1.0 specification.

use core::ptr::{read_volatile, write_volatile};
use spin::Mutex;
use core::arch::asm;

/// Read from I/O port (8-bit)
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    value
}

/// Write to I/O port (8-bit)
#[inline]
unsafe fn outb(port: u16, value: u8) {
    asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
}

/// Read from I/O port (16-bit)
#[inline]
unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    asm!("in ax, dx", out("ax") value, in("dx") port, options(nomem, nostack, preserves_flags));
    value
}

/// Write to I/O port (16-bit)
#[inline]
unsafe fn outw(port: u16, value: u16) {
    asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
}

/// Read from I/O port (32-bit)
#[inline]
unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack, preserves_flags));
    value
}

/// Write to I/O port (32-bit)
#[inline]
unsafe fn outl(port: u16, value: u32) {
    asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
}

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

/// VirtIO Legacy PCI register offsets (I/O port based)
const VIRTIO_PCI_DEVICE_FEATURES: u16 = 0x00;  // 32-bit r/o
const VIRTIO_PCI_DRIVER_FEATURES: u16 = 0x04;  // 32-bit r/w
const VIRTIO_PCI_QUEUE_ADDR: u16 = 0x08;       // 32-bit r/w
const VIRTIO_PCI_QUEUE_SIZE: u16 = 0x0C;       // 16-bit r/o
const VIRTIO_PCI_QUEUE_SEL: u16 = 0x0E;        // 16-bit r/w
const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 0x10;     // 16-bit r/w
const VIRTIO_PCI_DEVICE_STATUS: u16 = 0x12;    // 8-bit r/w
const VIRTIO_PCI_ISR_STATUS: u16 = 0x13;       // 8-bit r/o

/// Delay cycles after setting DRIVER_OK status
/// Gives device time to process status change before first operation
/// 
/// This is a conservative delay (~1000-2000 CPU cycles) that works across different
/// QEMU/KVM configurations and CPU speeds. While VirtIO devices typically respond
/// quickly in virtualized environments, this ensures status change is processed
/// before we attempt first I/O operation.
const STATUS_CHANGE_DELAY_CYCLES: u32 = 1000;

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
    ring: [u16; 256], // Support up to 256 queue entries
    used_event: u16, // Used event (only if VIRTIO_F_EVENT_IDX)
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
    ring: [VirtQUsedElem; 256],
    avail_event: u16, // Available event (only if VIRTIO_F_EVENT_IDX)
}

/// VirtIO block request header
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtIOBlockReq {
    req_type: u32,
    reserved: u32,
    sector: u64,
}

/// VirtIO block request types
const VIRTIO_BLK_T_IN: u32 = 0;   // Read
const VIRTIO_BLK_T_OUT: u32 = 1;  // Write

/// VirtIO block status codes
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

/// Virtqueue structure
struct Virtqueue {
    queue_size: u16,
    descriptors: *mut VirtQDescriptor,
    avail: *mut VirtQAvail,
    used: *mut VirtQUsed,
    desc_phys: u64,
    avail_phys: u64,
    used_phys: u64,
    free_head: u16,
    num_used: u16,
    last_used_idx: u16,
}

// Safety: Virtqueue uses raw pointers but manages them correctly
unsafe impl Send for Virtqueue {}

impl Virtqueue {
    /// Create a new virtqueue with DMA-allocated memory
    unsafe fn new(queue_size: u16) -> Option<Self> {
        // Calculate sizes for each component
        let desc_size = core::mem::size_of::<VirtQDescriptor>() * queue_size as usize;
        let avail_size = 6 + 2 * queue_size as usize + 2; // flags + idx + ring + used_event
        let used_size = 6 + 8 * queue_size as usize + 2; // flags + idx + ring + avail_event
        
        // Allocate descriptors (16-byte aligned)
        let (desc_ptr, desc_phys) = crate::memory::alloc_dma_buffer(desc_size, 16)?;
        let descriptors = desc_ptr as *mut VirtQDescriptor;
        
        // Allocate available ring (2-byte aligned)
        let (avail_ptr, avail_phys) = crate::memory::alloc_dma_buffer(avail_size, 2)?;
        let avail = avail_ptr as *mut VirtQAvail;
        
        // Allocate used ring (4-byte aligned) 
        let (used_ptr, used_phys) = crate::memory::alloc_dma_buffer(used_size, 4)?;
        let used = used_ptr as *mut VirtQUsed;
        
        // Initialize descriptors as a free list
        for i in 0..queue_size {
            (*descriptors.add(i as usize)).next = if i + 1 < queue_size { i + 1 } else { 0 };
            (*descriptors.add(i as usize)).flags = 0;
        }
        
        // Initialize available ring
        (*avail).flags = 0;
        (*avail).idx = 0;
        
        // Initialize used ring
        (*used).flags = 0;
        (*used).idx = 0;
        
        Some(Virtqueue {
            queue_size,
            descriptors,
            avail,
            used,
            desc_phys,
            avail_phys,
            used_phys,
            free_head: 0,
            num_used: 0,
            last_used_idx: 0,
        })
    }
    
    /// Allocate a descriptor chain
    unsafe fn alloc_desc(&mut self) -> Option<u16> {
        if self.num_used >= self.queue_size {
            return None;
        }
        
        let desc = self.free_head;
        self.free_head = (*self.descriptors.add(desc as usize)).next;
        self.num_used += 1;
        
        Some(desc)
    }
    
    /// Free a descriptor chain
    unsafe fn free_desc(&mut self, desc_idx: u16) {
        let mut idx = desc_idx;
        loop {
            let desc = &mut *self.descriptors.add(idx as usize);
            let next = desc.next;
            let has_next = (desc.flags & VIRTQ_DESC_F_NEXT) != 0;
            
            // Add to free list
            desc.flags = 0;
            desc.next = self.free_head;
            self.free_head = idx;
            self.num_used -= 1;
            
            if !has_next {
                break;
            }
            idx = next;
        }
    }
    
    /// Add buffers to the queue
    unsafe fn add_buf(&mut self, buffers: &[(u64, u32, u16)]) -> Option<u16> {
        if buffers.is_empty() || buffers.len() > self.queue_size as usize {
            return None;
        }
        
        // Allocate descriptor chain
        let head = self.alloc_desc()?;
        let mut desc_idx = head;
        
        for (i, &(addr, len, flags)) in buffers.iter().enumerate() {
            let desc = &mut *self.descriptors.add(desc_idx as usize);
            desc.addr = addr;
            desc.len = len;
            desc.flags = flags;
            
            if i + 1 < buffers.len() {
                let next = self.alloc_desc()?;
                desc.flags |= VIRTQ_DESC_F_NEXT;
                desc.next = next;
                desc_idx = next;
            }
        }
        
        // Add to available ring
        let avail = &mut *self.avail;
        let idx = avail.idx as usize % self.queue_size as usize;
        avail.ring[idx] = head;
        
        // Memory barrier to ensure ring write is visible before updating idx
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        
        // Update index - this tells the device there's work to do
        avail.idx = avail.idx.wrapping_add(1);
        
        // Memory barrier to ensure idx write is visible to device
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        
        Some(head)
    }
    
    /// Check if there are used buffers
    unsafe fn has_used(&self) -> bool {
        // Memory barrier to ensure we see device updates
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let used = &*self.used;
        self.last_used_idx != used.idx
    }
    
    /// Get next used buffer
    unsafe fn get_used(&mut self) -> Option<(u16, u32)> {
        if !self.has_used() {
            return None;
        }
        
        let used = &*self.used;
        let idx = self.last_used_idx as usize % self.queue_size as usize;
        let elem = used.ring[idx];
        
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        
        Some((elem.id as u16, elem.len))
    }
}

/// VirtIO block device driver
pub struct VirtIOBlockDevice {
    mmio_base: u64,       // MMIO base address (0 if using I/O ports)
    io_base: u16,         // I/O port base (0 if using MMIO)
    queue_size: u16,
    queue: Option<Virtqueue>,
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
                io_base: 0,
                queue_size: 8,
                queue: None,
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
            io_base: 0,
            queue_size: 8,
            queue: None,
        })
    }
    
    /// Create a new VirtIO block device from PCI BAR address
    unsafe fn new_from_pci(bar_addr: u64) -> Option<Self> {
        // For PCI devices, the BAR points to VirtIO registers
        // Create device structure  
        Some(VirtIOBlockDevice {
            mmio_base: bar_addr,
            io_base: 0,
            queue_size: 8,
            queue: None,
        })
    }
    
    /// Create a new VirtIO block device from PCI I/O ports
    unsafe fn new_from_pci_io(io_base: u16) -> Option<Self> {
        Some(VirtIOBlockDevice {
            mmio_base: 0,
            io_base,
            queue_size: 8,
            queue: None,
        })
    }
    
    /// Initialize the VirtIO block device
    unsafe fn init(&mut self) -> bool {
        if self.mmio_base == 0 && self.io_base == 0 {
            // Simulated device - initialize test data
            self.init_simulated_disk();
            return true;
        }
        
        if self.io_base != 0 {
            // I/O port based (legacy PCI)
            return self.init_legacy_pci();
        }
        
        // MMIO based
        self.init_mmio()
    }
    
    /// Initialize legacy PCI VirtIO device (I/O ports)
    unsafe fn init_legacy_pci(&mut self) -> bool {
        use crate::serial;
        
        serial::serial_print("[VirtIO] Initializing legacy PCI device\n");
        
        // Reset device
        outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, 0);
        
        // Set ACKNOWLEDGE
        outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, VIRTIO_STATUS_ACKNOWLEDGE as u8);
        
        // Set DRIVER
        let status = inb(self.io_base + VIRTIO_PCI_DEVICE_STATUS);
        outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, status | (VIRTIO_STATUS_DRIVER as u8));
        
        // Read device features
        let features = inl(self.io_base + VIRTIO_PCI_DEVICE_FEATURES);
        serial::serial_print("[VirtIO] Device features: ");
        serial::serial_print_hex(features as u64);
        serial::serial_print("\n");
        
        // Write driver features (accept all for now)
        outl(self.io_base + VIRTIO_PCI_DRIVER_FEATURES, 0);
        
        // Select queue 0
        outw(self.io_base + VIRTIO_PCI_QUEUE_SEL, 0);
        
        // Get queue size
        let queue_size = inw(self.io_base + VIRTIO_PCI_QUEUE_SIZE);
        serial::serial_print("[VirtIO] Queue size: ");
        serial::serial_print_dec(queue_size as u64);
        serial::serial_print("\n");
        
        if queue_size == 0 || queue_size > 256 {
            serial::serial_print("[VirtIO] Invalid queue size\n");
            return false;
        }
        
        // Use a reasonable queue size
        let actual_queue_size = if queue_size > 128 { 128 } else { queue_size };
        serial::serial_print("[VirtIO] Using queue size: ");
        serial::serial_print_dec(actual_queue_size as u64);
        serial::serial_print("\n");
        
        // Create virtqueue
        match Virtqueue::new(actual_queue_size) {
            Some(queue) => {
                // Set queue address (physical address / 4096)
                let queue_pfn = (queue.desc_phys / 4096) as u32;
                serial::serial_print("[VirtIO] Queue PFN: ");
                serial::serial_print_hex(queue_pfn as u64);
                serial::serial_print("\n");
                
                outl(self.io_base + VIRTIO_PCI_QUEUE_ADDR, queue_pfn);
                
                self.queue = Some(queue);
                self.queue_size = actual_queue_size;
                
                // Set DRIVER_OK
                let status = inb(self.io_base + VIRTIO_PCI_DEVICE_STATUS);
                outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, status | (VIRTIO_STATUS_DRIVER_OK as u8));
                
                // Small delay to let device process the status change
                for _ in 0..STATUS_CHANGE_DELAY_CYCLES {
                    core::hint::spin_loop();
                }
                
                // Verify status was set correctly
                let final_status = inb(self.io_base + VIRTIO_PCI_DEVICE_STATUS);
                serial::serial_print("[VirtIO] Final device status: ");
                serial::serial_print_hex(final_status as u64);
                serial::serial_print("\n");
                
                serial::serial_print("[VirtIO] Legacy PCI device initialized successfully\n");
                true
            }
            None => {
                serial::serial_print("[VirtIO] Failed to allocate virtqueue\n");
                false
            }
        }
    }
    
    /// Initialize MMIO VirtIO device
    unsafe fn init_mmio(&mut self) -> bool {
        let regs = self.mmio_base as *mut VirtIOMMIORegs;
        
        // Debug: Check if this is actually MMIO or PCI
        let magic = read_volatile(&(*regs).magic_value);
        crate::serial::serial_print("[VirtIO] Magic value: ");
        crate::serial::serial_print_hex(magic as u64);
        crate::serial::serial_print("\n");
        
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
        
        // Setup virtqueue
        write_volatile(&mut (*regs).queue_sel, 0); // Select queue 0
        
        let queue_size = read_volatile(&(*regs).queue_num_max);
        crate::serial::serial_print("[VirtIO] Queue size read: ");
        crate::serial::serial_print_hex(queue_size as u64);
        crate::serial::serial_print("\n");
        
        // VirtIO spec allows queue sizes up to 32768, but we'll use a reasonable limit
        if queue_size == 0 || queue_size > 256 {
            crate::serial::serial_print("[VirtIO] Invalid queue size (must be 1-256)\n");
            return false;
        }
        
        // Use a smaller queue size that we can handle (power of 2, <=128)
        let actual_queue_size = if queue_size > 128 { 128 } else { queue_size };
        crate::serial::serial_print("[VirtIO] Using queue size: ");
        crate::serial::serial_print_dec(actual_queue_size as u64);
        crate::serial::serial_print("\n");
        
        // Create virtqueue
        match Virtqueue::new(actual_queue_size as u16) {
            Some(queue) => {
                // Set queue size
                write_volatile(&mut (*regs).queue_num, actual_queue_size as u32);
                
                // Set descriptor table address
                let desc_low = (queue.desc_phys & 0xFFFFFFFF) as u32;
                let desc_high = (queue.desc_phys >> 32) as u32;
                write_volatile(&mut (*regs).queue_desc_low, desc_low);
                write_volatile(&mut (*regs).queue_desc_high, desc_high);
                
                // Set available ring address
                let avail_low = (queue.avail_phys & 0xFFFFFFFF) as u32;
                let avail_high = (queue.avail_phys >> 32) as u32;
                write_volatile(&mut (*regs).queue_driver_low, avail_low);
                write_volatile(&mut (*regs).queue_driver_high, avail_high);
                
                // Set used ring address
                let used_low = (queue.used_phys & 0xFFFFFFFF) as u32;
                let used_high = (queue.used_phys >> 32) as u32;
                write_volatile(&mut (*regs).queue_device_low, used_low);
                write_volatile(&mut (*regs).queue_device_high, used_high);
                
                // Mark queue as ready
                write_volatile(&mut (*regs).queue_ready, 1);
                
                self.queue = Some(queue);
                
                crate::serial::serial_print("[VirtIO] Virtqueue initialized successfully\n");
            }
            None => {
                crate::serial::serial_print("[VirtIO] Failed to allocate virtqueue\n");
                return false;
            }
        }
        
        // Set DRIVER_OK status bit
        let status = read_volatile(&(*regs).status);
        write_volatile(&mut (*regs).status, status | VIRTIO_STATUS_DRIVER_OK);
        
        crate::serial::serial_print("[VirtIO] Device initialized with real virtqueue\n");
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
        
        if self.mmio_base == 0 && self.io_base == 0 {
            // Simulated read
            const PARTITION_OFFSET: u64 = 131328;
            
            unsafe {
                if block_num < PARTITION_OFFSET {
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
        
        // Real VirtIO block read
        unsafe {
            let queue = self.queue.as_mut().ok_or_else(|| {
                crate::serial::serial_print("[VirtIO] read_block failed: No virtqueue available\n");
                "No virtqueue available"
            })?;
            
            // Allocate DMA buffers for request
            let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(
                core::mem::size_of::<VirtIOBlockReq>(), 16
            ).ok_or_else(|| {
                crate::serial::serial_print("[VirtIO] read_block failed: Cannot allocate request buffer\n");
                "Failed to allocate request buffer"
            })?;
            
            let (status_ptr, status_phys) = crate::memory::alloc_dma_buffer(1, 1)
                .ok_or_else(|| {
                    crate::serial::serial_print("[VirtIO] read_block failed: Cannot allocate status buffer\n");
                    "Failed to allocate status buffer"
                })?;
            
            let buffer_phys = crate::memory::virt_to_phys(buffer.as_ptr() as u64);
            
            // Build request header
            let req = &mut *(req_ptr as *mut VirtIOBlockReq);
            req.req_type = VIRTIO_BLK_T_IN; // Read
            req.reserved = 0;
            req.sector = block_num * 8; // 4KB block = 8 * 512-byte sectors
            
            // Build descriptor chain: request -> data -> status
            let buffers = [
                (req_phys, core::mem::size_of::<VirtIOBlockReq>() as u32, 0),
                (buffer_phys, 4096, VIRTQ_DESC_F_WRITE),
                (status_phys, 1, VIRTQ_DESC_F_WRITE),
            ];
            
            let _desc_idx = queue.add_buf(&buffers).ok_or("Failed to add buffer to queue")?;
            
            // Memory barrier before notifying device to ensure all writes are visible
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            
            // Notify device
            if self.io_base != 0 && self.mmio_base == 0 {
                // Legacy PCI - use I/O port notification
                outw(self.io_base + VIRTIO_PCI_QUEUE_NOTIFY, 0);
            } else if self.mmio_base != 0 {
                // MMIO - use MMIO register notification
                let regs = self.mmio_base as *mut VirtIOMMIORegs;
                write_volatile(&mut (*regs).queue_notify, 0);
            } else {
                // This should never happen due to early return for simulated disk
                crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
                crate::memory::free_dma_buffer(status_ptr, 1, 1);
                return Err("Invalid device configuration");
            }
            
            // Wait for completion (polling for now)
            let mut timeout = 1000000;
            while !queue.has_used() && timeout > 0 {
                timeout -= 1;
                core::hint::spin_loop();
            }
            
            if timeout == 0 {
                crate::serial::serial_print("[VirtIO] read_block failed: Device timeout (block ");
                crate::serial::serial_print_dec(block_num);
                crate::serial::serial_print(")\n");
                // Cleanup
                crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
                crate::memory::free_dma_buffer(status_ptr, 1, 1);
                return Err("VirtIO read timeout");
            }
            
            // Get used buffer
            if let Some((used_idx, _len)) = queue.get_used() {
                // Check status
                let status = *status_ptr;
                
                // Free buffers
                queue.free_desc(used_idx);
                crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
                crate::memory::free_dma_buffer(status_ptr, 1, 1);
                
                if status != VIRTIO_BLK_S_OK {
                    crate::serial::serial_print("[VirtIO] read_block failed: Bad status ");
                    crate::serial::serial_print_dec(status as u64);
                    crate::serial::serial_print("\n");
                    return Err("VirtIO read failed");
                }
                
                return Ok(());
            }
            
            // Cleanup on failure
            crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
            crate::memory::free_dma_buffer(status_ptr, 1, 1);
            Err("No used buffer returned")
        }
    }
    
    /// Write a block to the device
    pub fn write_block(&mut self, block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if buffer.len() < 4096 {
            return Err("Buffer too small (need 4096 bytes)");
        }
        
        if self.mmio_base == 0 && self.io_base == 0 {
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
        
        // Real VirtIO block write
        unsafe {
            let queue = self.queue.as_mut().ok_or("No virtqueue available")?;
            
            // Allocate DMA buffers
            let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(
                core::mem::size_of::<VirtIOBlockReq>(), 16
            ).ok_or("Failed to allocate request buffer")?;
            
            let (status_ptr, status_phys) = crate::memory::alloc_dma_buffer(1, 1)
                .ok_or("Failed to allocate status buffer")?;
            
            let buffer_phys = crate::memory::virt_to_phys(buffer.as_ptr() as u64);
            
            // Build request header
            let req = &mut *(req_ptr as *mut VirtIOBlockReq);
            req.req_type = VIRTIO_BLK_T_OUT; // Write
            req.reserved = 0;
            req.sector = block_num * 8;
            
            // Build descriptor chain: request -> data -> status
            let buffers = [
                (req_phys, core::mem::size_of::<VirtIOBlockReq>() as u32, 0),
                (buffer_phys, 4096, 0), // Data is read by device
                (status_phys, 1, VIRTQ_DESC_F_WRITE),
            ];
            
            let _desc_idx = queue.add_buf(&buffers).ok_or("Failed to add buffer to queue")?;
            
            // Memory barrier before notifying device to ensure all writes are visible
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            
            // Notify device
            if self.io_base != 0 && self.mmio_base == 0 {
                // Legacy PCI - use I/O port notification
                outw(self.io_base + VIRTIO_PCI_QUEUE_NOTIFY, 0);
            } else if self.mmio_base != 0 {
                // MMIO - use MMIO register notification
                let regs = self.mmio_base as *mut VirtIOMMIORegs;
                write_volatile(&mut (*regs).queue_notify, 0);
            } else {
                // This should never happen due to early return for simulated disk
                crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
                crate::memory::free_dma_buffer(status_ptr, 1, 1);
                return Err("Invalid device configuration");
            }
            
            // Wait for completion
            let mut timeout = 1000000;
            while !queue.has_used() && timeout > 0 {
                timeout -= 1;
                core::hint::spin_loop();
            }
            
            if timeout == 0 {
                crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
                crate::memory::free_dma_buffer(status_ptr, 1, 1);
                return Err("VirtIO write timeout");
            }
            
            // Get used buffer
            if let Some((used_idx, _len)) = queue.get_used() {
                let status = *status_ptr;
                
                queue.free_desc(used_idx);
                crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
                crate::memory::free_dma_buffer(status_ptr, 1, 1);
                
                if status != VIRTIO_BLK_S_OK {
                    return Err("VirtIO write failed");
                }
                
                return Ok(());
            }
            
            crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
            crate::memory::free_dma_buffer(status_ptr, 1, 1);
            Err("No used buffer returned")
        }
    }
}

/// Initialize VirtIO devices
pub fn init() {
    use crate::serial;
    
    serial::serial_print("[VirtIO] Initializing VirtIO devices...\n");
    serial::serial_print("[VirtIO] Searching for VirtIO block devices on PCI bus...\n");
    
    // Try to find VirtIO block device on PCI bus first
    if let Some(pci_dev) = crate::pci::find_virtio_block_device() {
        serial::serial_print("[VirtIO] Found VirtIO block device on PCI!\n");
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
            serial::serial_print("[VirtIO]   BAR0 raw=");
            serial::serial_print_hex(bar0 as u64);
            serial::serial_print(" (bit0=");
            serial::serial_print_dec((bar0 & 1) as u64);
            serial::serial_print(")\n");
            
            let bar_addr = (bar0 & !0xF) as u64;
            
            serial::serial_print("[VirtIO]   BAR0 masked=");
            serial::serial_print_hex(bar_addr);
            serial::serial_print("\n");
            
            // Check if this is I/O port or MMIO BAR
            if (bar0 & 1) != 0 {
                // I/O port BAR - VirtIO legacy PCI
                let io_base = (bar0 & !0x3) as u16;
                serial::serial_print("[VirtIO]   I/O port BAR detected at base=");
                serial::serial_print_hex(io_base as u64);
                serial::serial_print("\n");
                
                // Try to initialize I/O port based device
                match VirtIOBlockDevice::new_from_pci_io(io_base) {
                    Some(mut device) => {
                        serial::serial_print("[VirtIO]   Attempting to initialize I/O port device...\n");
                        if device.init() {
                            serial::serial_print("[VirtIO] I/O port device initialized successfully\n");
                            *BLOCK_DEVICE.lock() = Some(device);
                            return;
                        } else {
                            serial::serial_print("[VirtIO]   I/O port device initialization failed\n");
                        }
                    }
                    None => {
                        serial::serial_print("[VirtIO] Failed to create I/O port device\n");
                    }
                }
            } else {
                // Memory BAR - Try MMIO
                serial::serial_print("[VirtIO]   Memory BAR - trying MMIO\n");
                
                // Try to create a real VirtIO device from PCI
                if bar_addr != 0 {
                    match VirtIOBlockDevice::new_from_pci(bar_addr) {
                        Some(mut device) => {
                            serial::serial_print("[VirtIO]   Attempting to initialize MMIO device...\n");
                            if device.init() {
                                serial::serial_print("[VirtIO] Real PCI device initialized successfully\n");
                                *BLOCK_DEVICE.lock() = Some(device);
                                return;
                            } else {
                                serial::serial_print("[VirtIO]   MMIO device initialization failed\n");
                            }
                        }
                        None => {
                            serial::serial_print("[VirtIO] Failed to create device from PCI BAR\n");
                        }
                    }
                } else {
                    serial::serial_print("[VirtIO]   BAR0 address is 0, cannot initialize\n");
                }
            }
        }
    } else {
        serial::serial_print("[VirtIO] No VirtIO block device found on PCI bus\n");
    }
    
    // Don't fall back to simulated device - let ATA driver handle real hardware
    serial::serial_print("[VirtIO] No VirtIO device available, ATA will be used if present\n");
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
