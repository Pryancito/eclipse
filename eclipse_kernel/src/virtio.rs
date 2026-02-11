//! VirtIO device driver implementation
//! 
//! Implements minimal VirtIO support for block devices in QEMU/KVM environments.
//! Based on VirtIO 1.0 specification.

use core::ptr::{read_volatile, write_volatile};
use spin::Mutex;
use core::arch::asm;

use crate::serial;

/// Read time stamp counter
#[inline]
fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

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

/// Flush cache line
#[inline]
unsafe fn clflush(addr: u64) {
    core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack, preserves_flags));
}

unsafe fn sfence() {
    core::arch::asm!("sfence", options(nostack, preserves_flags));
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

/// Delay cycles after notifying device
/// Gives device time to process notification before we start polling
const DEVICE_NOTIFY_DELAY_CYCLES: u32 = 1000;

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
    next_avail: u16, // Index of the next available descriptor to allocate
    num_used: u16,   // Number of descriptors currently allocated/in-use
    last_used_idx: u16,
}

// Safety: Virtqueue uses raw pointers but manages them correctly
unsafe impl Send for Virtqueue {}

impl Virtqueue {
    /// Create a new virtqueue with DMA-allocated memory
    unsafe fn new(queue_size: u16) -> Option<Self> {
        crate::serial::serial_print("[VirtIO-VQ] Creating new virtqueue with size=");
        crate::serial::serial_print_dec(queue_size as u64);
        crate::serial::serial_print("\n");
        
        // Calculate sizes according to VirtIO Legacy spec
        // The Used Ring must be aligned to 4096 bytes boundary
        
        let desc_size = core::mem::size_of::<VirtQDescriptor>() * queue_size as usize;
        let avail_size = 6 + 2 * queue_size as usize + 2; // flags + idx + ring + used_event
        let used_size = 6 + 8 * queue_size as usize + 2; // flags + idx + ring + avail_event
        
        // Calculate offsets
        let avail_offset = desc_size;
        
        // Used ring must be 4096-byte aligned
        let mut used_offset = avail_offset + avail_size;
        if used_offset % 4096 != 0 {
            used_offset = (used_offset + 4095) & !4095;
        }
        
        let total_size = used_offset + used_size;
        
        crate::serial::serial_print("[VirtIO] Allocating contiguous queue memory: ");
        crate::serial::serial_print_dec(total_size as u64);
        crate::serial::serial_print(" bytes (aligned to 4096)\n");
        
        // Allocate single contiguous buffer (4096-byte aligned)
        let (mem_ptr, mem_phys) = crate::memory::alloc_dma_buffer(total_size, 4096)?;
        
        // Zero out memory
        core::ptr::write_bytes(mem_ptr, 0, total_size);
        
        // Calculate pointers and physical addresses
        let descriptors = mem_ptr as *mut VirtQDescriptor;
        let desc_phys = mem_phys;
        
        let avail = mem_ptr.add(avail_offset) as *mut VirtQAvail;
        let avail_phys = mem_phys + avail_offset as u64;
        
        let used = mem_ptr.add(used_offset) as *mut VirtQUsed;
        let used_phys = mem_phys + used_offset as u64;
        
        crate::serial::serial_print("[VirtIO]   Desc phys: ");
        crate::serial::serial_print_hex(desc_phys);
        crate::serial::serial_print("\n");
        crate::serial::serial_print("[VirtIO]   Avail phys: ");
        crate::serial::serial_print_hex(avail_phys);
        crate::serial::serial_print("\n");
        crate::serial::serial_print("[VirtIO]   Used phys: ");
        crate::serial::serial_print_hex(used_phys);
        crate::serial::serial_print("\n");
        
        // Initialize descriptors
        // We do NOT use a linked free list anymore. Descriptors are allocated sequentially.
        // Just zero them out initially.
        for i in 0..queue_size {
            let desc = &mut *descriptors.add(i as usize);
            write_volatile(&mut desc.addr, 0);
            write_volatile(&mut desc.len, 0);
            write_volatile(&mut desc.flags, 0);
            write_volatile(&mut desc.next, 0);
        }
        sfence();

        crate::serial::serial_print("[VirtIO-VQ] Initialized descriptors (counter-based allocation)\n");
        
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
            next_avail: 0,
            num_used: 0,
            last_used_idx: 0,
        })
    }
    
    /// Allocate a descriptor using a simple ring counter
    unsafe fn alloc_desc(&mut self) -> Option<u16> {
        // Check if we have descriptors available
        if self.num_used >= self.queue_size {
            crate::serial::serial_print("[VirtIO-VQ] alloc_desc: queue full\n");
            return None;
        }
        
        let desc_idx = self.next_avail;
        
        // Increment counter modulo queue_size
        self.next_avail = (self.next_avail + 1) % self.queue_size;
        self.num_used += 1;
        
        // Optional: clear the descriptor to be safe (though add_buf will overwrite it)
        let desc = &mut *self.descriptors.add(desc_idx as usize);
        write_volatile(&mut desc.flags, 0);
        write_volatile(&mut desc.next, 0);
        
        Some(desc_idx)
    }
    
    /// Free a descriptor chain
    unsafe fn free_desc(&mut self, desc_idx: u16) {
        let mut idx = desc_idx;
        let mut count = 0;
        
        // Walk the chain to count how many descriptors we are freeing
        loop {
            count += 1;
            
            let desc = &*self.descriptors.add(idx as usize);
            let flags = read_volatile(&raw const desc.flags);
            let next = read_volatile(&raw const desc.next);
            
            if (flags & VIRTQ_DESC_F_NEXT) == 0 {
                break;
            }
            idx = next;
        }
        
        if count > self.num_used {
             crate::serial::serial_print("[VirtIO-VQ] ERROR: Freeing more descriptors than allocated! count=");
             crate::serial::serial_print_dec(count as u64);
             crate::serial::serial_print(" num_used=");
             crate::serial::serial_print_dec(self.num_used as u64);
             crate::serial::serial_print("\n");
             self.num_used = 0;
        } else {
            self.num_used -= count;
        }
    }

    
    /// Add buffers to the queue
    unsafe fn add_buf(&mut self, buffers: &[(u64, u32, u16)]) -> Option<u16> {
        if buffers.is_empty() || buffers.len() > self.queue_size as usize {
            return None;
        }
        
        // Allocate descriptors and build chain
        let head = self.alloc_desc()?;
        let mut curr_idx = head;
        
        for (i, &(addr, len, flags)) in buffers.iter().enumerate() {
            let desc = &mut *self.descriptors.add(curr_idx as usize);
            
            // Write base fields
            write_volatile(&mut desc.addr, addr);
            write_volatile(&mut desc.len, len);
            
            if i + 1 < buffers.len() {
                // Not the last buffer, link to next
                let next_idx = self.alloc_desc()?;
                write_volatile(&mut desc.flags, flags | VIRTQ_DESC_F_NEXT);
                write_volatile(&mut desc.next, next_idx);
                
                // Debug log (using volatile reads for accuracy)
                crate::serial::serial_print("[VirtIO-VQ] Desc ");
                crate::serial::serial_print_dec(i as u64);
                crate::serial::serial_print(" (idx=");
                crate::serial::serial_print_dec(curr_idx as u64);
                crate::serial::serial_print("): addr=");
                crate::serial::serial_print_hex(unsafe { core::ptr::read_volatile(&raw const desc.addr) });
                crate::serial::serial_print(" len=");
                crate::serial::serial_print_dec(unsafe { core::ptr::read_volatile(&raw const desc.len) } as u64);
                crate::serial::serial_print(" flags=");
                crate::serial::serial_print_hex(unsafe { core::ptr::read_volatile(&raw const desc.flags) } as u64);
                crate::serial::serial_print(" next=");
                crate::serial::serial_print_dec(unsafe { core::ptr::read_volatile(&raw const desc.next) } as u64);
                crate::serial::serial_print("\n");
                
                clflush(desc as *const _ as u64);
                curr_idx = next_idx;
            } else {
                // Last buffer in chain
                write_volatile(&mut desc.flags, flags);
                write_volatile(&mut desc.next, 0);
                
                // Debug log
                crate::serial::serial_print("[VirtIO-VQ] Desc ");
                crate::serial::serial_print_dec(i as u64);
                crate::serial::serial_print(" (idx=");
                crate::serial::serial_print_dec(curr_idx as u64);
                crate::serial::serial_print("): addr=");
                crate::serial::serial_print_hex(unsafe { core::ptr::read_volatile(&raw const desc.addr) });
                crate::serial::serial_print(" len=");
                crate::serial::serial_print_dec(unsafe { core::ptr::read_volatile(&raw const desc.len) } as u64);
                crate::serial::serial_print(" flags=");
                crate::serial::serial_print_hex(unsafe { core::ptr::read_volatile(&raw const desc.flags) } as u64);
                crate::serial::serial_print(" (last)\n");
                
                clflush(desc as *const _ as u64);
            }
        }
        
        // Add to available ring
        let avail = &mut *self.avail;
        let ring_idx = read_volatile(&avail.idx) as usize % self.queue_size as usize;
        write_volatile(&mut avail.ring[ring_idx], head);
        
        // FLUSH ring entry
        clflush(&avail.ring[ring_idx] as *const _ as u64);

        // Update index - this tells the device there's work to do
        let new_idx = read_volatile(&avail.idx).wrapping_add(1);
        
        // Memory barriers to ensure all descriptor writes are visible before idx update
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        sfence(); 
        
        write_volatile(&mut avail.idx, new_idx);
        clflush(&avail.idx as *const _ as u64);
        sfence();

        // Ensure update is visible before notification
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        sfence();
        
        Some(head)
    }
    
    /// Check if there are used buffers
    unsafe fn has_used(&self) -> bool {
        // INVALIDATE cache for used ring index
        clflush(&((*self.used).idx) as *const _ as u64);
        
        // Memory barrier to ensure we see device updates
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let used = &*self.used;
        // MUST use volatile read as device updates this asynchronously
        let idx = read_volatile(&used.idx);
        self.last_used_idx != idx
    }
    
    /// Get next used buffer
    unsafe fn get_used(&mut self) -> Option<(u16, u32)> {
        // INVALIDATE cache for used ring entry
        let idx = self.last_used_idx as usize % self.queue_size as usize;
        clflush(&((*self.used).ring[idx]) as *const _ as u64);

        // We can just call has_used() to check, but efficiency matters
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let used = &*self.used;
        let current_idx = read_volatile(&used.idx);
        
        if self.last_used_idx == current_idx {
            return None;
        }
        
        let idx = self.last_used_idx as usize % self.queue_size as usize;
        // Volatile read of ring element
        let elem_id = read_volatile(&used.ring[idx].id);
        let elem_len = read_volatile(&used.ring[idx].len);
        
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        
        Some((elem_id as u16, elem_len))
    }
}

/// VirtIO block device driver
pub struct VirtIOBlockDevice {
    mmio_base: u64,       // MMIO base address (0 if using I/O ports)
    io_base: u16,         // I/O port base (0 if using MMIO)
    queue_size: u16,
    queue: Option<Virtqueue>,
}

static BLOCK_DEVICES: Mutex<alloc::vec::Vec<VirtIOBlockDevice>> = Mutex::new(alloc::vec::Vec::new());

impl VirtIOBlockDevice {
    /// Create a new VirtIO block device from MMIO base
    unsafe fn new(mmio_base: u64) -> Option<Self> {
        let regs = mmio_base as *mut VirtIOMMIORegs;
        
        // Check magic value
        let magic = read_volatile(&(*regs).magic_value);
        if magic != VIRTIO_MAGIC {
            // No VirtIO device found
            return None;
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
            serial::serial_print("[VirtIO] Invalid queue size (must be 1-256): ");
            serial::serial_print_dec(queue_size as u64);
            serial::serial_print("\n");
            return false;
        }
        
        let actual_queue_size = queue_size;
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
                
                // Verify queue address was set correctly
                let readback_pfn = inl(self.io_base + VIRTIO_PCI_QUEUE_ADDR);
                serial::serial_print("[VirtIO] Queue PFN readback: ");
                serial::serial_print_hex(readback_pfn as u64);
                serial::serial_print("\n");
                
                if readback_pfn != queue_pfn {
                    serial::serial_print("[VirtIO] ERROR: Queue PFN readback mismatch!\n");
                    return false;
                }
                
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
    

    /// Read a block from the device
    pub fn read_block(&mut self, block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if buffer.len() < 4096 {
            return Err("Buffer too small (need 4096 bytes)");
        }
        
        // VirtIO block read
        unsafe {
            let queue = self.queue.as_mut().ok_or_else(|| {
                crate::serial::serial_print("[VirtIO] read_block failed: No virtqueue available\n");
                "No virtqueue available"
            })?;
            
            // Allocate DMA buffers for request
            // Align to 64 bytes to avoid false sharing with other cache lines
            let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(
                core::mem::size_of::<VirtIOBlockReq>(), 64
            ).ok_or_else(|| {
                crate::serial::serial_print("[VirtIO] read_block failed: Cannot allocate request buffer\n");
                "Failed to allocate request buffer"
            })?;
            
            crate::serial::serial_print("[VirtIO] READ block=");
            crate::serial::serial_print_dec(block_num);
            crate::serial::serial_print(" on device ");
            crate::serial::serial_print_hex(self.mmio_base | self.io_base as u64);
            crate::serial::serial_print("\n");

            crate::serial::serial_print("[VirtIO] Request: v=");
            crate::serial::serial_print_hex(req_ptr as u64);
            crate::serial::serial_print(" p=");
            crate::serial::serial_print_hex(req_phys);
            crate::serial::serial_print("\n");
            
            // Allocate status buffer
            // Align to 64 bytes to avoid false sharing
            let (status_ptr, status_phys) = crate::memory::alloc_dma_buffer(1, 64)
                .ok_or_else(|| {
                    crate::serial::serial_print("[VirtIO] read_block failed: Cannot allocate status buffer\n");
                    crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 64);
                    "Failed to allocate status buffer"
                })?;
            
            // Allocate BOUNCE BUFFER for data
            // Accessing heap directly (Vec) via virt_to_phys is risky if the heap is large
            // or if the Vec is not physically contiguous (which it isn't guaranteed to be).
            // A dedicated DMA buffer guarantees we give the device a valid, contiguous physical region.
            let (bounce_ptr, bounce_phys) = crate::memory::alloc_dma_buffer(4096, 4096)
                .ok_or_else(|| {
                    crate::serial::serial_print("[VirtIO] read_block failed: Cannot allocate bounce buffer\n");
                    crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 64);
                    crate::memory::free_dma_buffer(status_ptr, 1, 64);
                    "Failed to allocate bounce buffer"
                })?;

            crate::serial::serial_print("[VirtIO] Bounce Buffer: v=");
            crate::serial::serial_print_hex(bounce_ptr as u64);
            crate::serial::serial_print(" p=");
            crate::serial::serial_print_hex(bounce_phys);
            crate::serial::serial_print("\n");

            // Zero out bounce buffer to detect if device actually writes to it
            core::ptr::write_bytes(bounce_ptr, 0, 4096);

            // Initialize status to 0x55 to detect if device touches it
            *status_ptr = 0x55;
            
            // Build request header
            let req = &mut *(req_ptr as *mut VirtIOBlockReq);
            req.req_type = VIRTIO_BLK_T_IN; // Read
            req.reserved = 0;
            req.sector = block_num * 8; // 4KB block = 8 * 512-byte sectors
            
            // Build descriptor chain: request -> bounce buffer -> status
            let buffers = [
                (req_phys, core::mem::size_of::<VirtIOBlockReq>() as u32, 0),
                (bounce_phys, 4096, VIRTQ_DESC_F_WRITE),
                (status_phys, 1, VIRTQ_DESC_F_WRITE),
            ];

            // Debug log request content AFTER build
            crate::serial::serial_print("[VirtIO] Header (p=");
            crate::serial::serial_print_hex(req_phys);
            crate::serial::serial_print("): type=");
            crate::serial::serial_print_dec(req.req_type as u64);
            crate::serial::serial_print(" sector=");
            crate::serial::serial_print_dec(req.sector);
            crate::serial::serial_print(" (raw: ");
            for i in 0..16 {
                let b = unsafe { core::ptr::read_volatile((req_ptr as *const u8).add(i)) };
                crate::serial::serial_print_hex(b as u64);
                crate::serial::serial_print(" ");
            }
            crate::serial::serial_print(")\n");
            
            // FLUSH CACHE (Ensure data reaches RAM before device reads it)
            clflush(req_ptr as u64);
            clflush(status_ptr as u64);
            // Flush bounce buffer (4KB)
            for i in (0..4096).step_by(64) {
                clflush((bounce_ptr as u64) + i);
            }
            sfence();
            
            let result: Result<(), &'static str> = (|| {
                let desc_idx = queue.add_buf(&buffers).ok_or("Failed to add buffer to queue")?;
                
                // FORCE MEMORY BARRIER before notification
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                
                // Notify device
                if self.io_base != 0 && self.mmio_base == 0 {
                    const VIRTIO_QUEUE_INDEX: u16 = 0;
                    outw(self.io_base + VIRTIO_PCI_QUEUE_NOTIFY, VIRTIO_QUEUE_INDEX);
                } else if self.mmio_base != 0 {
                    let regs = self.mmio_base as *mut VirtIOMMIORegs;
                    write_volatile(&mut (*regs).queue_notify, 0);
                } else {
                    return Err("Invalid device configuration");
                }
                
                // Check initial status
                let initial_status = read_volatile(status_ptr);
                crate::serial::serial_print("[VirtIO] Pre-wait Status: 0x");
                crate::serial::serial_print_hex(initial_status as u64);
                crate::serial::serial_print("\n");
                
                // Wait for completion (Timeout using RDTSC)
                // 2 GHz = 2 * 10^9 cycles/sec. 1ms = 2 * 10^6 cycles.
                // Wait up to 1 second (approx 2*10^9 cycles) or more conservatively 3*10^9.
                let start_time = rdtsc();
                let timeout_cycles = 3_000_000_000; // ~1-2 seconds depending on CPU freq
                
                while !queue.has_used() {
                    if rdtsc().wrapping_sub(start_time) > timeout_cycles {
                        // Final status check before failing
                        let final_timeout_status = read_volatile(status_ptr);
                        crate::serial::serial_print("[VirtIO] Timeout Status: 0x");
                        crate::serial::serial_print_hex(final_timeout_status as u64);
                        crate::serial::serial_print("\n");
                        
                        crate::serial::serial_print("[VirtIO] read_block failed: Device timeout (RDTSC)\n");
                        crate::serial::serial_print("[VirtIO] WARNING: Leaking DMA buffers to prevent memory corruption\n");
                        return Err("VirtIO read timeout (buffers leaked)");
                    }
                     
                    if self.io_base != 0 {
                         // Ack interrupt if PCI (though we shouldn't need to in poll mode ideally, but for legacy cleanup)
                         let _isr = inb(self.io_base + VIRTIO_PCI_ISR_STATUS);
                    }
                    core::hint::spin_loop();
                }
                
                // Get used buffer
                if let Some((used_idx, len)) = queue.get_used() {
                    // Memory fence to ensure device writes are visible
                    core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
                    
                    crate::serial::serial_print("[VirtIO] Read completed: len=");
                    crate::serial::serial_print_dec(len as u64);
                    crate::serial::serial_print("\n");
                    
                    // Check status - MUST use volatile read as device writes this asynchronously
                    let status = read_volatile(status_ptr);

                    // INVALIDATE CACHE for bounce buffer after read
                    // This ensures we see what the device wrote to RAM
                    for i in (0..4096).step_by(64) {
                        clflush((bounce_ptr as u64) + i);
                    }
                    // Memory barrier to ensure clflush is finished before we copy
                    core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

                    // RELEASE DESCRIPTOR IMMEDIATELY (Safety: Freeing before return)
                    // We must free the descriptor so it goes back to the free pool.
                    queue.free_desc(used_idx);
                    
                    if status == 0x55 {
                        crate::serial::serial_print("[VirtIO] read_block failed: Status not updated (still 0x55)\n");
                        return Err("VirtIO status not updated (IRQ lost?)");
                    }
                    
                    if status != VIRTIO_BLK_S_OK {
                        crate::serial::serial_print("[VirtIO] read_block failed: Bad status 0x");
                        crate::serial::serial_print_hex(status as u64);
                        crate::serial::serial_print("\n");
                        return Err("VirtIO read failed");
                    }
                    
                    // Copy data from bounce buffer to user buffer
                    core::ptr::copy_nonoverlapping(bounce_ptr, buffer.as_mut_ptr(), 4096);
                    Ok(())
                } else {
                    // Should be unreachable if has_used() returned true
                    Err("Spurious wakeup")
                }
            })();

            // Clean up DMA buffers (RAII-style)
            // Note: If result is "Buffers Leaked" timeout, we do NOT free them.
            if let Err(e) = result {
                if e == "VirtIO read timeout (buffers leaked)" {
                    return Err(e);
                }
            }
            
            // Standard cleanup
            crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 64);
            crate::memory::free_dma_buffer(status_ptr, 1, 64);
            crate::memory::free_dma_buffer(bounce_ptr, 4096, 4096);
            
            result
        }
    }
    
    /// Write a block to the device
    pub fn write_block(&mut self, block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if buffer.len() < 4096 {
            return Err("Buffer too small (need 4096 bytes)");
        }
        
        // VirtIO block write
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
            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            
            // Notify device
            if self.io_base != 0 && self.mmio_base == 0 {
                // Legacy PCI - use I/O port notification
                outw(self.io_base + VIRTIO_PCI_QUEUE_NOTIFY, 0);
            } else if self.mmio_base != 0 {
                // MMIO - use MMIO register notification
                let regs = self.mmio_base as *mut VirtIOMMIORegs;
                write_volatile(&mut (*regs).queue_notify, 0);
            } else {
                // Invalid device configuration
                crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
                crate::memory::free_dma_buffer(status_ptr, 1, 1);
                return Err("Invalid device configuration");
            }
            
            // Wait for completion
            let mut timeout = 100000000;
            while !queue.has_used() && timeout > 0 {
                timeout -= 1;
                core::hint::spin_loop();
            }
            
            if timeout == 0 {
                crate::serial::serial_print("[VirtIO] write_block failed: Device timeout (block ");
                crate::serial::serial_print_dec(block_num);
                crate::serial::serial_print(")\n");
                
                crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
                crate::memory::free_dma_buffer(status_ptr, 1, 1);
                return Err("VirtIO write timeout");
            }
            
            // Get used buffer
            if let Some((used_idx, _len)) = queue.get_used() {
                // Memory fence to ensure device writes are visible
                core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
                
                // Check status - MUST use volatile read as device writes this asynchronously
                let status = unsafe { read_volatile(status_ptr) };
                
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
    // Search for ALL VirtIO block devices on PCI
    let devices = crate::pci::get_all_devices();
    for dev in devices {
        if dev.is_virtio() && (dev.device_id == 0x1001 || dev.device_id == 0x1042) {
            serial::serial_print("[VirtIO] Found block device on PCI! Bus=");
            serial::serial_print_dec(dev.bus as u64);
            serial::serial_print(" Dev=");
            serial::serial_print_dec(dev.device as u64);
            serial::serial_print("\n");
            
            unsafe {
                crate::pci::enable_device(&dev, true);
                let bar0 = crate::pci::get_bar(&dev, 0);
                
                if (bar0 & 1) != 0 {
                    let io_base = (bar0 & !0x3) as u16;
                    if let Some(mut virt_dev) = VirtIOBlockDevice::new_from_pci_io(io_base) {
                        if virt_dev.init() {
                            serial::serial_print("[VirtIO] Initialized device at ");
                            serial::serial_print_hex(io_base as u64);
                            serial::serial_print("\n");
                            BLOCK_DEVICES.lock().push(virt_dev);
                        }
                    }
                }
            }
        }
    }
    
    serial::serial_print("[VirtIO] Total devices initialized: ");
    serial::serial_print_dec(BLOCK_DEVICES.lock().len() as u64);
    serial::serial_print("\n");

    // Register as disk: scheme
    crate::scheme::register_scheme("disk", alloc::sync::Arc::new(DiskScheme));
    serial::serial_print("[VirtIO] Registered 'disk:' scheme\n");
}

/// Global wrapper to read a block from the first available VirtIO device
pub fn read_block(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    let mut devices = BLOCK_DEVICES.lock();
    if let Some(dev) = devices.get_mut(0) {
        dev.read_block(block_num, buffer)
    } else {
        Err("No VirtIO block device found")
    }
}

/// Global wrapper to write a block to the first available VirtIO device
pub fn write_block(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    let mut devices = BLOCK_DEVICES.lock();
    if let Some(dev) = devices.get_mut(0) {
        dev.write_block(block_num, buffer)
    } else {
        Err("No VirtIO block device found")
    }
}


// --- Redox-style Scheme Implementation ---

use crate::scheme::{Scheme, Stat, error as scheme_error};

struct OpenDisk {
    disk_idx: usize,
    offset: u64, // offset in bytes
}

static OPEN_DISKS: Mutex<alloc::vec::Vec<Option<OpenDisk>>> = Mutex::new(alloc::vec::Vec::new());

pub struct DiskScheme;

impl Scheme for DiskScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let disk_idx = path.parse::<usize>().map_err(|_| scheme_error::EINVAL)?;
        
        let devices = BLOCK_DEVICES.lock();
        if disk_idx >= devices.len() {
            return Err(scheme_error::ENOENT);
        }

        let mut open_disks = OPEN_DISKS.lock();
        for (i, slot) in open_disks.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(OpenDisk { disk_idx, offset: 0 });
                return Ok(i);
            }
        }
        
        let id = open_disks.len();
        open_disks.push(Some(OpenDisk { disk_idx, offset: 0 }));
        Ok(id)
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let mut devices = BLOCK_DEVICES.lock();
        let mut open_disks = OPEN_DISKS.lock();
        let open_disk = open_disks.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        // Convert byte offset to block number
        let block_num = open_disk.offset / 4096;
        let offset_in_block = (open_disk.offset % 4096) as usize;
        
        let mut temp_block = alloc::vec![0u8; 4096];
        let device = devices.get_mut(open_disk.disk_idx).ok_or(scheme_error::EIO)?;
        
        if let Err(e) = device.read_block(block_num, &mut temp_block) {
            serial::serial_print("[DISK-SCHEME] read_block failed for disk ");
            serial::serial_print_dec(open_disk.disk_idx as u64);
            serial::serial_print(" block ");
            serial::serial_print_dec(block_num);
            serial::serial_print("\n");
            return Err(scheme_error::EIO);
        }
        
        let available = 4096 - offset_in_block;
        let to_copy = core::cmp::min(buffer.len(), available);
        
        buffer[..to_copy].copy_from_slice(&temp_block[offset_in_block..offset_in_block + to_copy]);
        
        open_disk.offset += to_copy as u64;
        Ok(to_copy)
    }

    fn write(&self, _id: usize, _buffer: &[u8]) -> Result<usize, usize> {
        Err(scheme_error::EIO) // Read-only for now
    }

    fn lseek(&self, id: usize, offset: isize, whence: usize) -> Result<usize, usize> {
        let mut open_disks = OPEN_DISKS.lock();
        let open_disk = open_disks.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        let new_offset = match whence {
            0 => offset as u64, // SEEK_SET
            1 => (open_disk.offset as isize + offset) as u64, // SEEK_CUR
            _ => return Err(scheme_error::EINVAL),
        };
        
        open_disk.offset = new_offset;
        Ok(new_offset as usize)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut open_disks = OPEN_DISKS.lock();
        if let Some(slot) = open_disks.get_mut(id) {
            *slot = None;
            Ok(0)
        } else {
            Err(scheme_error::EBADF)
        }
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        Ok(0)
    }
}
