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
const VIRTIO_ID_GPU: u32 = 16;

/// VirtIO 1.0 feature bit (must be negotiated for modern devices)
/// In common config: feature_select 0 = bits 0-31, 1 = bits 32-63. VERSION_1 = bit 32.
const VIRTIO_F_VERSION_1_LO: u32 = 0;   // high half
const VIRTIO_F_VERSION_1_HI: u32 = 1;   // bit 0 of high half = bit 32

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

/// VirtIO GPU commands (virtio_gpu.h)
const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;

/// VirtIO GPU feature bits (VIRTIO_GPU_F_*)
const VIRTIO_GPU_F_VIRGL: u32 = 1;   // 3D acceleration via virgl
const VIRTIO_GPU_F_EDID: u32 = 2;    // EDID support (optional)

/// Virgl 3D commands (when VIRTIO_GPU_F_VIRGL negotiated) - match Linux virtio_gpu.h
const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x0200;
const VIRTIO_GPU_CMD_CTX_DESTROY: u32 = 0x0201;
const VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
const VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
const VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x0207;

const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;
const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
const CURSOR_RESOURCE_ID: u32 = 1;
const DISPLAY_BUFFER_RESOURCE_ID: u32 = 2;
const CURSOR_WIDTH: u32 = 64;
const CURSOR_HEIGHT: u32 = 64;

/// VirtIO GPU control header (24 bytes)
#[repr(C, packed)]
struct VirtioGpuCtrlHdr {
    ctrl_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    ring_idx: u8,
    padding: [u8; 3],
}

/// VirtIO GPU display info response (per scanout)
#[repr(C, packed)]
struct VirtioGpuDisplayOne {
    r_x: u32,
    r_y: u32,
    r_width: u32,
    r_height: u32,
    enabled: u32,
    flags: u32,
}

/// VirtIO GPU full display info response
#[repr(C, packed)]
struct VirtioGpuRespDisplayInfo {
    hdr: VirtioGpuCtrlHdr,
    pmodes: [VirtioGpuDisplayOne; VIRTIO_GPU_MAX_SCANOUTS],
}

/// VirtIO GPU MOVE_CURSOR/UPDATE_CURSOR request
#[repr(C, packed)]
struct VirtioGpuUpdateCursorReq {
    hdr: VirtioGpuCtrlHdr,
    scanout_id: u32,
    x: u32,
    y: u32,
    pos_padding: u32,
    resource_id: u32,
    hot_x: u32,
    hot_y: u32,
    padding2: u32,
}

/// virtio_gpu_rect
#[repr(C, packed)]
struct VirtioGpuRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// RESOURCE_CREATE_2D request
#[repr(C, packed)]
struct VirtioGpuResourceCreate2d {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

/// virtio_gpu_mem_entry
#[repr(C, packed)]
struct VirtioGpuMemEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

/// RESOURCE_ATTACH_BACKING request (mem_entries follow in same buffer)
#[repr(C, packed)]
struct VirtioGpuResourceAttachBacking {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    nr_entries: u32,
}

/// SET_SCANOUT request
#[repr(C, packed)]
struct VirtioGpuSetScanout {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    scanout_id: u32,
    resource_id: u32,
}

/// TRANSFER_TO_HOST_2D request
#[repr(C, packed)]
struct VirtioGpuTransferToHost2d {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

/// RESOURCE_FLUSH request
#[repr(C, packed)]
struct VirtioGpuResourceFlush {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    resource_id: u32,
    padding: u32,
}

/// CTX_CREATE request (Virgl) - create 3D context
#[repr(C, packed)]
struct VirtioGpuCtxCreate {
    hdr: VirtioGpuCtrlHdr,
    nlen: u32,
    padding: u32,
    debug_name: [u8; 64],
}

/// virtio_gpu_box - 3D region (for TRANSFER_*_3D)
#[repr(C, packed)]
struct VirtioGpuBox {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}

/// TRANSFER_TO_HOST_3D / TRANSFER_FROM_HOST_3D request
#[repr(C, packed)]
struct VirtioGpuTransferHost3d {
    hdr: VirtioGpuCtrlHdr,
    r#box: VirtioGpuBox,
    offset: u64,
    resource_id: u32,
    level: u32,
    stride: u32,
    layer_stride: u32,
}

/// RESOURCE_CREATE_3D request
#[repr(C, packed)]
struct VirtioGpuResourceCreate3d {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
    padding: u32,
}

/// SUBMIT_3D request header (command buffer follows)
#[repr(C, packed)]
struct VirtioGpuCmdSubmit {
    hdr: VirtioGpuCtrlHdr,
    size: u32,
    padding: u32,
}

/// CTX_ATTACH_RESOURCE / CTX_DETACH_RESOURCE
#[repr(C, packed)]
struct VirtioGpuCtxResource {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    padding: u32,
}

const VIRGL_RESOURCE_FLAG_Y_0_TOP: u32 = 1 << 0;

const VIRGL_MAX_CTX: u32 = 16;
const VIRGL_CTX_ID_MIN: u32 = 1;

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
        // crate::serial::serial_print("[VirtIO-VQ] Creating new virtqueue with size=");
        // crate::serial::serial_print_dec(queue_size as u64);
        // crate::serial::serial_print("\n");
        
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
        
        // crate::serial::serial_print("[VirtIO] Allocating contiguous queue memory: ");
        // crate::serial::serial_print_dec(total_size as u64);
        // crate::serial::serial_print(" bytes (aligned to 4096)\n");
        
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
        
        // crate::serial::serial_print("[VirtIO]   Desc phys: ");
        // crate::serial::serial_print_hex(desc_phys);
        // crate::serial::serial_print("\n");
        // crate::serial::serial_print("[VirtIO]   Avail phys: ");
        // crate::serial::serial_print_hex(avail_phys);
        // crate::serial::serial_print("\n");
        // crate::serial::serial_print("[VirtIO]   Used phys: ");
        // crate::serial::serial_print_hex(used_phys);
        // crate::serial::serial_print("\n");
        
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

                clflush(desc as *const _ as u64);
                curr_idx = next_idx;
            } else {
                // Last buffer in chain
                write_volatile(&mut desc.flags, flags);
                write_volatile(&mut desc.next, 0);

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
        
        // serial::serial_print("[VirtIO] Initializing legacy PCI device\n");
        
        // Reset device
        outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, 0);
        
        // Set ACKNOWLEDGE
        outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, VIRTIO_STATUS_ACKNOWLEDGE as u8);
        
        // Set DRIVER
        let status = inb(self.io_base + VIRTIO_PCI_DEVICE_STATUS);
        outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, status | (VIRTIO_STATUS_DRIVER as u8));
        
        // Read device features
        let features = inl(self.io_base + VIRTIO_PCI_DEVICE_FEATURES);
        // serial::serial_print("[VirtIO] Device features: ");
        // serial::serial_print_hex(features as u64);
        // serial::serial_print("\n");
        
        // Write driver features (accept all for now)
        outl(self.io_base + VIRTIO_PCI_DRIVER_FEATURES, 0);
        
        // Select queue 0
        outw(self.io_base + VIRTIO_PCI_QUEUE_SEL, 0);
        
        // Get queue size
        let queue_size = inw(self.io_base + VIRTIO_PCI_QUEUE_SIZE);
        // serial::serial_print("[VirtIO] Queue size: ");
        // serial::serial_print_dec(queue_size as u64);
        // serial::serial_print("\n");
        
        if queue_size == 0 || queue_size > 256 {
            serial::serial_print("[VirtIO] Invalid queue size (must be 1-256): ");
            serial::serial_print_dec(queue_size as u64);
            serial::serial_print("\n");
            return false;
        }
        
        let actual_queue_size = queue_size;
        // serial::serial_print("[VirtIO] Using queue size: ");
        // serial::serial_print_dec(actual_queue_size as u64);
        // serial::serial_print("\n");
        
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
                // serial::serial_print("[VirtIO] Final device status: ");
                // serial::serial_print_hex(final_status as u64);
                // serial::serial_print("\n");
                
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

// ============== VirtIO GPU Driver ==============

/// VirtIO GPU device driver
/// VirtIO 1.0 PCI modern: common config layout (virtio_pci_common_cfg)
const VIRTIO_PCI_COMMON_DFSELECT: usize = 0;
const VIRTIO_PCI_COMMON_DF: usize = 4;
const VIRTIO_PCI_COMMON_GFSELECT: usize = 8;
const VIRTIO_PCI_COMMON_GF: usize = 12;
const VIRTIO_PCI_COMMON_MSIX: usize = 16;
const VIRTIO_PCI_COMMON_NUMQ: usize = 18;
const VIRTIO_PCI_COMMON_STATUS: usize = 20;
const VIRTIO_PCI_COMMON_CFGGEN: usize = 21;
const VIRTIO_PCI_COMMON_Q_SELECT: usize = 22;
const VIRTIO_PCI_COMMON_Q_SIZE: usize = 24;
const VIRTIO_PCI_COMMON_Q_MSIX: usize = 26;
const VIRTIO_PCI_COMMON_Q_ENABLE: usize = 28;
const VIRTIO_PCI_COMMON_Q_NOFF: usize = 30;
const VIRTIO_PCI_COMMON_Q_DESCLO: usize = 32;
const VIRTIO_PCI_COMMON_Q_DESCHI: usize = 36;
const VIRTIO_PCI_COMMON_Q_AVAILLO: usize = 40;
const VIRTIO_PCI_COMMON_Q_AVAILHI: usize = 44;
const VIRTIO_PCI_COMMON_Q_USEDLO: usize = 48;
const VIRTIO_PCI_COMMON_Q_USEDHI: usize = 52;

pub struct VirtIOGpuDevice {
    mmio_base: u64,
    io_base: u16,
    /// VirtIO 1.0 modern: common config base (0 = legacy)
    modern_common: u64,
    /// VirtIO 1.0 modern: notify region virt addr to write queue index (0 = legacy)
    modern_notify_addr: u64,
    control_queue: Option<Virtqueue>,
    cursor_resource_created: bool,
    /// Cursor bitmap DMA allocation (kept for device to read)
    cursor_bitmap: Option<(*mut u8, u64)>,
    /// True if VIRTIO_GPU_F_VIRGL was negotiated (3D acceleration available)
    pub virgl_supported: bool,
    /// Bitmask of allocated Virgl context IDs (bit N = ctx_id N+1 in use)
    virgl_ctx_mask: u32,
}

// Safety: GPU device uses raw pointers but is only accessed through Mutex
unsafe impl Send for VirtIOGpuDevice {}

static GPU_DEVICES: Mutex<alloc::vec::Vec<VirtIOGpuDevice>> = Mutex::new(alloc::vec::Vec::new());

impl VirtIOGpuDevice {
    /// Create from PCI I/O ports (legacy)
    unsafe fn new_from_pci_io(io_base: u16) -> Self {
        VirtIOGpuDevice {
            mmio_base: 0,
            io_base,
            modern_common: 0,
            modern_notify_addr: 0,
            control_queue: None,
            cursor_resource_created: false,
            cursor_bitmap: None,
            virgl_supported: false,
            virgl_ctx_mask: 0,
        }
    }

    /// Notify device of queue 0 (control queue)
    #[inline]
    unsafe fn gpu_notify_queue(io_base: u16, mmio_base: u64, modern_notify_addr: u64) {
        for _ in 0..DEVICE_NOTIFY_DELAY_CYCLES {
            core::hint::spin_loop();
        }
        if io_base != 0 {
            outw(io_base + VIRTIO_PCI_QUEUE_NOTIFY, 0);
        } else if mmio_base != 0 {
            let regs = mmio_base as *mut VirtIOMMIORegs;
            write_volatile(&mut (*regs).queue_notify, 0);
        } else if modern_notify_addr != 0 {
            core::ptr::write_volatile(modern_notify_addr as *mut u16, 0);
        }
    }

    /// Create from PCI memory BAR
    unsafe fn new_from_pci_mmio(mmio_base: u64) -> Self {
        VirtIOGpuDevice {
            mmio_base,
            modern_common: 0,
            modern_notify_addr: 0,
            io_base: 0,
            control_queue: None,
            cursor_resource_created: false,
            cursor_bitmap: None,
            virgl_supported: false,
            virgl_ctx_mask: 0,
        }
    }

    /// Send control command, expect OK_NODATA response. Caller owns req allocation.
    fn send_ctrl_cmd_nodata(&mut self, req_phys: u64, req_size: usize) -> Result<(), &'static str> {
        let (io, mmio, modern) = (self.io_base, self.mmio_base, self.modern_notify_addr);
        let queue = self.control_queue.as_mut().ok_or("No control queue")?;
        let resp_size = core::mem::size_of::<VirtioGpuCtrlHdr>();
        let (resp_ptr, resp_phys) = crate::memory::alloc_dma_buffer(resp_size, 64)
            .ok_or("alloc resp failed")?;
        unsafe { core::ptr::write_bytes(resp_ptr, 0, resp_size); }
        let buffers = [
            (req_phys, req_size as u32, 0u16),
            (resp_phys, resp_size as u32, VIRTQ_DESC_F_WRITE),
        ];
        let head = unsafe {
            match queue.add_buf(&buffers) {
                Some(h) => h,
                None => {
                    crate::memory::free_dma_buffer(resp_ptr, resp_size, 64);
                    return Err("add_buf failed");
                }
            }
        };
        unsafe { Self::gpu_notify_queue(io, mmio, modern); }
        let mut timeout = 100_000;
        loop {
            unsafe { if queue.has_used() { break; } }
            if timeout == 0 {
                unsafe { queue.free_desc(head); crate::memory::free_dma_buffer(resp_ptr, resp_size, 64); }
                return Err("timeout");
            }
            timeout -= 1;
            core::hint::spin_loop();
        }
        let (used_head, _) = unsafe { queue.get_used().unwrap_or((0, 0)) };
        unsafe { queue.free_desc(used_head); }
        let ctrl_type = unsafe {
            core::ptr::read_unaligned((resp_ptr as *const u8).add(core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *const u32)
        };
        unsafe { crate::memory::free_dma_buffer(resp_ptr, resp_size, 64); }
        if ctrl_type != VIRTIO_GPU_RESP_OK_NODATA {
            return Err("unexpected response");
        }
        Ok(())
    }

    /// Initialize legacy PCI (I/O ports)
    unsafe fn init_legacy_pci(&mut self) -> bool {
        // Reset
        outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, 0);
        outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, VIRTIO_STATUS_ACKNOWLEDGE as u8);
        let status = inb(self.io_base + VIRTIO_PCI_DEVICE_STATUS);
        outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, status | (VIRTIO_STATUS_DRIVER as u8));

        // Features: negotiate Virgl if device supports it
        let dev_features = inl(self.io_base + VIRTIO_PCI_DEVICE_FEATURES);
        let driver_features = if (dev_features & VIRTIO_GPU_F_VIRGL) != 0 {
            VIRTIO_GPU_F_VIRGL
        } else {
            0
        };
        outl(self.io_base + VIRTIO_PCI_DRIVER_FEATURES, driver_features);
        self.virgl_supported = driver_features != 0;

        // Control queue (queue 0)
        outw(self.io_base + VIRTIO_PCI_QUEUE_SEL, 0);
        let queue_size = inw(self.io_base + VIRTIO_PCI_QUEUE_SIZE);
        crate::serial::serial_print("[VirtIO-GPU] Legacy queue_size=");
        crate::serial::serial_print_dec(queue_size as u64);
        crate::serial::serial_print("\n");
        if queue_size == 0 || queue_size > 256 {
            return false;
        }
        let actual_size = if queue_size > 64 { 64 } else { queue_size };

        match Virtqueue::new(actual_size) {
            Some(queue) => {
                let queue_pfn = (queue.desc_phys / 4096) as u32;
                outl(self.io_base + VIRTIO_PCI_QUEUE_ADDR, queue_pfn);
                self.control_queue = Some(queue);

                let status = inb(self.io_base + VIRTIO_PCI_DEVICE_STATUS);
                outb(self.io_base + VIRTIO_PCI_DEVICE_STATUS, status | (VIRTIO_STATUS_DRIVER_OK as u8));
                for _ in 0..STATUS_CHANGE_DELAY_CYCLES {
                    core::hint::spin_loop();
                }
                true
            }
            None => false,
        }
    }

    /// Initialize MMIO
    unsafe fn init_mmio(&mut self) -> bool {
        let regs = self.mmio_base as *mut VirtIOMMIORegs;
        let magic = read_volatile(&(*regs).magic_value);
        crate::serial::serial_print("[VirtIO-GPU] MMIO magic=");
        crate::serial::serial_print_hex(magic as u64);
        if magic != VIRTIO_MAGIC {
            crate::serial::serial_print(" (expected 0x74726976)\n");
            return false;
        }
        let device_id = read_volatile(&(*regs).device_id);
        crate::serial::serial_print(" device_id=");
        crate::serial::serial_print_hex(device_id as u64);
        if device_id != VIRTIO_ID_GPU {
            crate::serial::serial_print(" (expected 0x10 GPU)\n");
            return false;
        }
        crate::serial::serial_print("\n");

        write_volatile(&mut (*regs).status, 0);
        write_volatile(&mut (*regs).status, VIRTIO_STATUS_ACKNOWLEDGE);
        let status = read_volatile(&(*regs).status);
        write_volatile(&mut (*regs).status, status | VIRTIO_STATUS_DRIVER);

        write_volatile(&mut (*regs).device_features_sel, 0);
        let dev_features = read_volatile(&(*regs).device_features);
        let driver_features = if (dev_features & VIRTIO_GPU_F_VIRGL) != 0 {
            VIRTIO_GPU_F_VIRGL
        } else {
            0
        };
        write_volatile(&mut (*regs).driver_features_sel, 0);
        write_volatile(&mut (*regs).driver_features, driver_features);
        self.virgl_supported = driver_features != 0;

        let status = read_volatile(&(*regs).status);
        write_volatile(&mut (*regs).status, status | VIRTIO_STATUS_FEATURES_OK);
        let status = read_volatile(&(*regs).status);
        if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
            crate::serial::serial_print("[VirtIO-GPU] FEATURES_OK rejected, status=");
            crate::serial::serial_print_hex(status as u64);
            crate::serial::serial_print("\n");
            return false;
        }

        write_volatile(&mut (*regs).queue_sel, 0);
        let queue_size = read_volatile(&(*regs).queue_num_max);
        crate::serial::serial_print("[VirtIO-GPU] MMIO queue_num_max=");
        crate::serial::serial_print_dec(queue_size as u64);
        crate::serial::serial_print("\n");
        if queue_size == 0 || queue_size > 256 {
            return false;
        }
        let actual_size = if queue_size > 64 { 64 } else { queue_size } as u32;

        match Virtqueue::new(actual_size as u16) {
            Some(queue) => {
                write_volatile(&mut (*regs).queue_num, actual_size);
                write_volatile(&mut (*regs).queue_desc_low, (queue.desc_phys & 0xFFFFFFFF) as u32);
                write_volatile(&mut (*regs).queue_desc_high, (queue.desc_phys >> 32) as u32);
                write_volatile(&mut (*regs).queue_driver_low, (queue.avail_phys & 0xFFFFFFFF) as u32);
                write_volatile(&mut (*regs).queue_driver_high, (queue.avail_phys >> 32) as u32);
                write_volatile(&mut (*regs).queue_device_low, (queue.used_phys & 0xFFFFFFFF) as u32);
                write_volatile(&mut (*regs).queue_device_high, (queue.used_phys >> 32) as u32);
                write_volatile(&mut (*regs).queue_ready, 1);
                self.control_queue = Some(queue);

                let status = read_volatile(&(*regs).status);
                write_volatile(&mut (*regs).status, status | VIRTIO_STATUS_DRIVER_OK);
                true
            }
                None => false,
        }
    }

    /// Initialize VirtIO 1.0 PCI modern (virtio-gpu-pci, virtio-vga)
    unsafe fn init_mmio_modern(dev: &crate::pci::PciDevice) -> Option<Self> {
        use crate::pci::{pci_find_virtio_cap, pci_find_virtio_notify_cap, VIRTIO_PCI_CAP_COMMON_CFG, VIRTIO_PCI_CAP_ISR_CFG};
        let (common_bar, common_off, common_len) = pci_find_virtio_cap(dev, VIRTIO_PCI_CAP_COMMON_CFG)?;
        let (isr_bar, isr_off, _) = pci_find_virtio_cap(dev, VIRTIO_PCI_CAP_ISR_CFG)?;
        let (notify_bar, notify_off, _notify_len, notify_mult) = pci_find_virtio_notify_cap(dev)?;

        let bar_common = crate::pci::get_bar(dev, common_bar) & 0xFFFFFFFFFFFFFFF0u64;
        let bar_isr = crate::pci::get_bar(dev, isr_bar) & 0xFFFFFFFFFFFFFFF0u64;
        let bar_notify = crate::pci::get_bar(dev, notify_bar) & 0xFFFFFFFFFFFFFFF0u64;
        if bar_common == 0 || bar_isr == 0 || bar_notify == 0 {
            return None;
        }

        let common_virt = crate::memory::map_mmio_range(bar_common + common_off as u64, common_len as usize);
        let _isr_virt = crate::memory::map_mmio_range(bar_isr + isr_off as u64, 1);
        // Notify region: map enough for one notify (2 bytes) at our queue's offset
        // We'll get queue_notify_off from common config, then map notify_base + off*mult
        let notify_base_virt = crate::memory::map_mmio_range(bar_notify + notify_off as u64, 0x100);

        // Helper: write common config
        let common_w8 = |off: usize, v: u8| {
            core::ptr::write_volatile((common_virt + off as u64) as *mut u8, v);
        };
        let common_w16 = |off: usize, v: u16| {
            core::ptr::write_volatile((common_virt + off as u64) as *mut u16, v.to_le());
        };
        let common_w32 = |off: usize, v: u32| {
            core::ptr::write_volatile((common_virt + off as u64) as *mut u32, v.to_le());
        };
        let common_r8 = |off: usize| -> u8 {
            core::ptr::read_volatile((common_virt + off as u64) as *const u8)
        };
        let common_r16 = |off: usize| -> u16 {
            u16::from_le(core::ptr::read_volatile((common_virt + off as u64) as *const u16))
        };
        let common_r32 = |off: usize| -> u32 {
            u32::from_le(core::ptr::read_volatile((common_virt + off as u64) as *const u32))
        };

        // Reset
        common_w8(VIRTIO_PCI_COMMON_STATUS, 0);
        for _ in 0..100 { core::hint::spin_loop(); }
        common_w8(VIRTIO_PCI_COMMON_STATUS, VIRTIO_STATUS_ACKNOWLEDGE as u8);
        let st = common_r8(VIRTIO_PCI_COMMON_STATUS);
        common_w8(VIRTIO_PCI_COMMON_STATUS, st | VIRTIO_STATUS_DRIVER as u8);

        // Features: must set VIRTIO_F_VERSION_1 (bit 32). Select 0 = low 32, 1 = high 32.
        common_w32(VIRTIO_PCI_COMMON_GFSELECT, 0);
        let dev_lo = common_r32(VIRTIO_PCI_COMMON_DF);
        common_w32(VIRTIO_PCI_COMMON_GFSELECT, 1);
        let dev_hi = common_r32(VIRTIO_PCI_COMMON_DF);
        let guest_lo = if (dev_lo & VIRTIO_GPU_F_VIRGL) != 0 { VIRTIO_GPU_F_VIRGL } else { 0 };
        let guest_hi = if (dev_hi & VIRTIO_F_VERSION_1_HI) != 0 { VIRTIO_F_VERSION_1_HI } else { 0 };
        common_w32(VIRTIO_PCI_COMMON_GFSELECT, 0);
        common_w32(VIRTIO_PCI_COMMON_GF, guest_lo);
        common_w32(VIRTIO_PCI_COMMON_GFSELECT, 1);
        common_w32(VIRTIO_PCI_COMMON_GF, guest_hi);

        common_w8(VIRTIO_PCI_COMMON_STATUS, common_r8(VIRTIO_PCI_COMMON_STATUS) | VIRTIO_STATUS_FEATURES_OK as u8);
        if (common_r8(VIRTIO_PCI_COMMON_STATUS) & (VIRTIO_STATUS_FEATURES_OK as u8)) == 0 {
            serial::serial_print("[VirtIO-GPU] Modern: FEATURES_OK rejected\n");
            return None;
        }

        // Queue 0 (control)
        common_w16(VIRTIO_PCI_COMMON_Q_SELECT, 0);
        let queue_size = common_r16(VIRTIO_PCI_COMMON_Q_SIZE);
        serial::serial_print("[VirtIO-GPU] Modern queue_size=");
        serial::serial_print_dec(queue_size as u64);
        serial::serial_print("\n");
        if queue_size == 0 || queue_size > 256 {
            return None;
        }
        let actual_size = if queue_size > 64 { 64 } else { queue_size };

        let queue = Virtqueue::new(actual_size)?;
        common_w16(VIRTIO_PCI_COMMON_Q_SIZE, actual_size);
        common_w32(VIRTIO_PCI_COMMON_Q_DESCLO, (queue.desc_phys & 0xFFFFFFFF) as u32);
        common_w32(VIRTIO_PCI_COMMON_Q_DESCHI, (queue.desc_phys >> 32) as u32);
        common_w32(VIRTIO_PCI_COMMON_Q_AVAILLO, (queue.avail_phys & 0xFFFFFFFF) as u32);
        common_w32(VIRTIO_PCI_COMMON_Q_AVAILHI, (queue.avail_phys >> 32) as u32);
        common_w32(VIRTIO_PCI_COMMON_Q_USEDLO, (queue.used_phys & 0xFFFFFFFF) as u32);
        common_w32(VIRTIO_PCI_COMMON_Q_USEDHI, (queue.used_phys >> 32) as u32);

        let queue_notify_off = common_r16(VIRTIO_PCI_COMMON_Q_NOFF);
        common_w16(VIRTIO_PCI_COMMON_Q_ENABLE, 1);

        let notify_offset_bytes = (queue_notify_off as u32 * notify_mult) as u64;
        let notify_addr_virt = notify_base_virt + notify_offset_bytes;

        common_w8(VIRTIO_PCI_COMMON_STATUS, common_r8(VIRTIO_PCI_COMMON_STATUS) | VIRTIO_STATUS_DRIVER_OK as u8);

        serial::serial_print("[VirtIO-GPU] Initialized (VirtIO 1.0 PCI modern)\n");

        Some(VirtIOGpuDevice {
            mmio_base: 0,
            io_base: 0,
            modern_common: common_virt,
            modern_notify_addr: notify_addr_virt,
            control_queue: Some(queue),
            cursor_resource_created: false,
            cursor_bitmap: None,
            virgl_supported: guest_lo != 0,
            virgl_ctx_mask: 0,
        })
    }

    /// Get display info via GET_DISPLAY_INFO command
    pub fn get_display_info(&mut self) -> Result<(u32, u32), &'static str> {
        let queue = self.control_queue.as_mut().ok_or("No control queue")?;

        let req_size = core::mem::size_of::<VirtioGpuCtrlHdr>();
        let resp_size = core::mem::size_of::<VirtioGpuRespDisplayInfo>();

        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc req failed")?;
        let (resp_ptr, resp_phys) = crate::memory::alloc_dma_buffer(resp_size, 64)
            .ok_or_else(|| {
                unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64) };
                "alloc resp failed"
            })?;

        // Zero buffers
        unsafe {
            core::ptr::write_bytes(req_ptr, 0, req_size);
            core::ptr::write_bytes(resp_ptr, 0, resp_size);
        }

        // Set request type = GET_DISPLAY_INFO (use unaligned write for packed struct)
        unsafe {
            core::ptr::write_unaligned(
                req_ptr.add(core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32,
                VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            );
        }

        // Descriptor chain: [request out] -> [response in]
        let buffers = [
            (req_phys, req_size as u32, 0u16),           // device-readable
            (resp_phys, resp_size as u32, VIRTQ_DESC_F_WRITE), // device-writable
        ];

        let (io, mmio, modern) = (self.io_base, self.mmio_base, self.modern_notify_addr);
        let head = unsafe {
            match queue.add_buf(&buffers) {
                Some(h) => h,
                None => {
                    crate::memory::free_dma_buffer(req_ptr, req_size, 64);
                    crate::memory::free_dma_buffer(resp_ptr, resp_size, 64);
                    return Err("add_buf failed");
                }
            }
        };

        unsafe { Self::gpu_notify_queue(io, mmio, modern); }

        // Poll for completion
        let mut timeout = 1_000_000;
        loop {
            unsafe {
                if queue.has_used() {
                    break;
                }
            }
            if timeout == 0 {
                unsafe {
                    queue.free_desc(head);
                    crate::memory::free_dma_buffer(req_ptr, req_size, 64);
                    crate::memory::free_dma_buffer(resp_ptr, resp_size, 64);
                }
                return Err("get_display_info timeout");
            }
            timeout -= 1;
            core::hint::spin_loop();
        }

        let (used_head, _len) = unsafe { queue.get_used().unwrap_or((0, 0)) };
        unsafe { queue.free_desc(used_head); }

        let resp_base = resp_ptr as *const u8;
        let ctrl_type_offset = core::mem::offset_of!(VirtioGpuRespDisplayInfo, hdr)
            + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type);
        let ctrl_type = unsafe {
            core::ptr::read_unaligned(resp_base.add(ctrl_type_offset) as *const u32)
        };
        if ctrl_type != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            unsafe {
                crate::memory::free_dma_buffer(req_ptr, req_size, 64);
                crate::memory::free_dma_buffer(resp_ptr, resp_size, 64);
            }
            return Err("unexpected response type");
        }

        // First enabled scanout - use unaligned reads for packed struct
        let pmodes_offset = core::mem::offset_of!(VirtioGpuRespDisplayInfo, pmodes);
        let mode_size = core::mem::size_of::<VirtioGpuDisplayOne>();
        let enabled_offset = core::mem::offset_of!(VirtioGpuDisplayOne, enabled);
        let r_width_offset = core::mem::offset_of!(VirtioGpuDisplayOne, r_width);
        let r_height_offset = core::mem::offset_of!(VirtioGpuDisplayOne, r_height);

        let mut width = 0u32;
        let mut height = 0u32;
        for i in 0..VIRTIO_GPU_MAX_SCANOUTS {
            let mode_base = pmodes_offset + i * mode_size;
            let enabled = unsafe {
                core::ptr::read_unaligned(
                    resp_base.add(mode_base + enabled_offset) as *const u32
                )
            };
            if enabled != 0 {
                width = unsafe {
                    core::ptr::read_unaligned(
                        resp_base.add(mode_base + r_width_offset) as *const u32
                    )
                };
                height = unsafe {
                    core::ptr::read_unaligned(
                        resp_base.add(mode_base + r_height_offset) as *const u32
                    )
                };
                break;
            }
        }

        unsafe {
            crate::memory::free_dma_buffer(req_ptr, req_size, 64);
            crate::memory::free_dma_buffer(resp_ptr, resp_size, 64);
        }
        Ok((width, height))
    }

    /// Initialize cursor with image: RESOURCE_CREATE_2D, ATTACH_BACKING, UPDATE_CURSOR
    pub fn init_cursor(&mut self) -> Result<(), &'static str> {
        if self.cursor_resource_created {
            return Ok(());
        }
        let cursor_size = (CURSOR_WIDTH * CURSOR_HEIGHT * 4) as usize;
        let (bitmap_ptr, bitmap_phys) = crate::memory::alloc_dma_buffer(cursor_size, 4096)
            .ok_or("alloc cursor bitmap failed")?;
        unsafe {
            core::ptr::write_bytes(bitmap_ptr, 0, cursor_size);
            // Simple 64x64 arrow: white center cross, black outline, transparent elsewhere
            let ptr = bitmap_ptr as *mut u8;
            for y in 0..CURSOR_HEIGHT {
                for x in 0..CURSOR_WIDTH {
                    let i = (y * CURSOR_WIDTH + x) as usize * 4;
                    let (b, g, r, a) = if (x >= 28 && x < 36) || (y >= 28 && y < 36) {
                        if (x >= 30 && x < 34) || (y >= 30 && y < 34) {
                            (255, 255, 255, 255) // white center
                        } else {
                            (0, 0, 0, 255) // black outline
                        }
                    } else {
                        (0, 0, 0, 0)
                    };
                    *ptr.add(i) = b;
                    *ptr.add(i + 1) = g;
                    *ptr.add(i + 2) = r;
                    *ptr.add(i + 3) = a;
                }
            }
        }
        self.cursor_bitmap = Some((bitmap_ptr, bitmap_phys));

        self.resource_create_2d(CURSOR_RESOURCE_ID, VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM, CURSOR_WIDTH, CURSOR_HEIGHT)?;
        self.resource_attach_backing(CURSOR_RESOURCE_ID, &[(bitmap_phys, cursor_size as u32)])?;

        // UPDATE_CURSOR: set cursor image at (0,0), hotspot center (32,32)
        self.update_cursor(0, 0, 0, 32, 32)?;
        self.cursor_resource_created = true;
        Ok(())
    }

    /// UPDATE_CURSOR: set cursor image and position
    fn update_cursor(&mut self, scanout_id: u32, x: u32, y: u32, hot_x: u32, hot_y: u32) -> Result<(), &'static str> {
        let req_size = core::mem::size_of::<VirtioGpuUpdateCursorReq>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc update_cursor failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }
        let req_base = req_ptr as *mut u8;
        let base = core::mem::offset_of!(VirtioGpuUpdateCursorReq, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type);
        unsafe {
            core::ptr::write_unaligned(req_base.add(base) as *mut u32, VIRTIO_GPU_CMD_UPDATE_CURSOR);
            core::ptr::write_unaligned(req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, scanout_id)) as *mut u32, scanout_id);
            core::ptr::write_unaligned(req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, x)) as *mut u32, x);
            core::ptr::write_unaligned(req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, y)) as *mut u32, y);
            core::ptr::write_unaligned(req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, resource_id)) as *mut u32, CURSOR_RESOURCE_ID);
            core::ptr::write_unaligned(req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, hot_x)) as *mut u32, hot_x);
            core::ptr::write_unaligned(req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, hot_y)) as *mut u32, hot_y);
        }
        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// RESOURCE_CREATE_2D: create a 2D resource (for scanout or cursor)
    pub fn resource_create_2d(&mut self, resource_id: u32, format: u32, width: u32, height: u32) -> Result<(), &'static str> {
        let req_size = core::mem::size_of::<VirtioGpuResourceCreate2d>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64).ok_or("alloc create2d failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }
        let r = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate2d, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_RESOURCE_CREATE_2D);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate2d, resource_id)) as *mut u32, resource_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate2d, format)) as *mut u32, format);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate2d, width)) as *mut u32, width);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate2d, height)) as *mut u32, height);
        }
        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// RESOURCE_ATTACH_BACKING: attach guest memory to resource (addr_phys, length) per entry
    pub fn resource_attach_backing(&mut self, resource_id: u32, entries: &[(u64, u32)]) -> Result<(), &'static str> {
        let attach_size = core::mem::size_of::<VirtioGpuResourceAttachBacking>()
            + entries.len() * core::mem::size_of::<VirtioGpuMemEntry>();
        let (attach_ptr, attach_phys) = crate::memory::alloc_dma_buffer(attach_size, 64).ok_or("alloc attach failed")?;
        unsafe { core::ptr::write_bytes(attach_ptr, 0, attach_size); }
        let ab = attach_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(ab.add(core::mem::offset_of!(VirtioGpuResourceAttachBacking, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING);
            core::ptr::write_unaligned(ab.add(core::mem::offset_of!(VirtioGpuResourceAttachBacking, resource_id)) as *mut u32, resource_id);
            core::ptr::write_unaligned(ab.add(core::mem::offset_of!(VirtioGpuResourceAttachBacking, nr_entries)) as *mut u32, entries.len() as u32);
        }
        let ent_off = core::mem::size_of::<VirtioGpuResourceAttachBacking>();
        for (i, &(addr, len)) in entries.iter().enumerate() {
            let e = ent_off + i * core::mem::size_of::<VirtioGpuMemEntry>();
            unsafe {
                core::ptr::write_unaligned(ab.add(e + core::mem::offset_of!(VirtioGpuMemEntry, addr)) as *mut u64, addr);
                core::ptr::write_unaligned(ab.add(e + core::mem::offset_of!(VirtioGpuMemEntry, length)) as *mut u32, len);
            }
        }
        self.send_ctrl_cmd_nodata(attach_phys, attach_size)?;
        unsafe { crate::memory::free_dma_buffer(attach_ptr, attach_size, 64); }
        Ok(())
    }

    /// SET_SCANOUT: assign resource to display output
    pub fn set_scanout(&mut self, scanout_id: u32, resource_id: u32, x: u32, y: u32, w: u32, h: u32) -> Result<(), &'static str> {
        let req_size = core::mem::size_of::<VirtioGpuSetScanout>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64).ok_or("alloc set_scanout failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }
        let r = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuSetScanout, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_SET_SCANOUT);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuSetScanout, r) + core::mem::offset_of!(VirtioGpuRect, x)) as *mut u32, x);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuSetScanout, r) + core::mem::offset_of!(VirtioGpuRect, y)) as *mut u32, y);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuSetScanout, r) + core::mem::offset_of!(VirtioGpuRect, width)) as *mut u32, w);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuSetScanout, r) + core::mem::offset_of!(VirtioGpuRect, height)) as *mut u32, h);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuSetScanout, scanout_id)) as *mut u32, scanout_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuSetScanout, resource_id)) as *mut u32, resource_id);
        }
        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// TRANSFER_TO_HOST_2D: copy guest memory to resource
    pub fn transfer_to_host_2d(&mut self, resource_id: u32, x: u32, y: u32, w: u32, h: u32, offset: u64) -> Result<(), &'static str> {
        let req_size = core::mem::size_of::<VirtioGpuTransferToHost2d>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64).ok_or("alloc transfer failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }
        let r = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferToHost2d, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferToHost2d, r) + core::mem::offset_of!(VirtioGpuRect, x)) as *mut u32, x);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferToHost2d, r) + core::mem::offset_of!(VirtioGpuRect, y)) as *mut u32, y);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferToHost2d, r) + core::mem::offset_of!(VirtioGpuRect, width)) as *mut u32, w);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferToHost2d, r) + core::mem::offset_of!(VirtioGpuRect, height)) as *mut u32, h);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferToHost2d, offset)) as *mut u64, offset);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferToHost2d, resource_id)) as *mut u32, resource_id);
        }
        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// RESOURCE_FLUSH: flush resource to display
    pub fn resource_flush(&mut self, resource_id: u32, x: u32, y: u32, w: u32, h: u32) -> Result<(), &'static str> {
        let req_size = core::mem::size_of::<VirtioGpuResourceFlush>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64).ok_or("alloc flush failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }
        let r = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceFlush, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_RESOURCE_FLUSH);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceFlush, r) + core::mem::offset_of!(VirtioGpuRect, x)) as *mut u32, x);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceFlush, r) + core::mem::offset_of!(VirtioGpuRect, y)) as *mut u32, y);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceFlush, r) + core::mem::offset_of!(VirtioGpuRect, width)) as *mut u32, w);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceFlush, r) + core::mem::offset_of!(VirtioGpuRect, height)) as *mut u32, h);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceFlush, resource_id)) as *mut u32, resource_id);
        }
        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// Move hardware cursor (VIRTIO_GPU_CMD_MOVE_CURSOR)
    pub fn move_cursor(&mut self, scanout_id: u32, x: u32, y: u32) -> Result<(), &'static str> {
        let queue = self.control_queue.as_mut().ok_or("No control queue")?;

        let req_size = core::mem::size_of::<VirtioGpuUpdateCursorReq>();
        let resp_size = core::mem::size_of::<VirtioGpuCtrlHdr>();

        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc req failed")?;
        let (resp_ptr, resp_phys) = crate::memory::alloc_dma_buffer(resp_size, 64)
            .ok_or_else(|| {
                unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64) };
                "alloc resp failed"
            })?;

        unsafe {
            core::ptr::write_bytes(req_ptr, 0, req_size);
            core::ptr::write_bytes(resp_ptr, 0, resp_size);
        }

        let req_base = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(
                req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, hdr)
                    + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32,
                VIRTIO_GPU_CMD_MOVE_CURSOR,
            );
            core::ptr::write_unaligned(
                req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, scanout_id)) as *mut u32,
                scanout_id,
            );
            core::ptr::write_unaligned(
                req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, x)) as *mut u32,
                x,
            );
            core::ptr::write_unaligned(
                req_base.add(core::mem::offset_of!(VirtioGpuUpdateCursorReq, y)) as *mut u32,
                y,
            );
        }

        let buffers = [
            (req_phys, req_size as u32, 0u16),
            (resp_phys, resp_size as u32, VIRTQ_DESC_F_WRITE),
        ];

        let (io, mmio, modern) = (self.io_base, self.mmio_base, self.modern_notify_addr);
        let head = unsafe {
            match queue.add_buf(&buffers) {
                Some(h) => h,
                None => {
                    crate::memory::free_dma_buffer(req_ptr, req_size, 64);
                    crate::memory::free_dma_buffer(resp_ptr, resp_size, 64);
                    return Err("add_buf failed");
                }
            }
        };

        unsafe { Self::gpu_notify_queue(io, mmio, modern); }

        let mut timeout = 100_000;
        loop {
            unsafe {
                if queue.has_used() { break; }
            }
            if timeout == 0 {
                unsafe {
                    queue.free_desc(head);
                    crate::memory::free_dma_buffer(req_ptr, req_size, 64);
                    crate::memory::free_dma_buffer(resp_ptr, resp_size, 64);
                }
                return Err("move_cursor timeout");
            }
            timeout -= 1;
            core::hint::spin_loop();
        }

        let (used_head, _len) = unsafe { queue.get_used().unwrap_or((0, 0)) };
        unsafe { queue.free_desc(used_head); }

        let resp_base = resp_ptr as *const u8;
        let ctrl_type = unsafe {
            core::ptr::read_unaligned(
                resp_base.add(core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *const u32
            )
        };
        if ctrl_type != VIRTIO_GPU_RESP_OK_NODATA {
            unsafe {
                crate::memory::free_dma_buffer(req_ptr, req_size, 64);
                crate::memory::free_dma_buffer(resp_ptr, resp_size, 64);
            }
            return Err("unexpected response");
        }

        unsafe {
            crate::memory::free_dma_buffer(req_ptr, req_size, 64);
            crate::memory::free_dma_buffer(resp_ptr, resp_size, 64);
        }
        Ok(())
    }

    /// Create a Virgl 3D context (requires virgl_supported).
    /// Returns the context ID (1..=16) on success.
    pub fn virgl_ctx_create(&mut self, debug_name: &[u8]) -> Result<u32, &'static str> {
        if !self.virgl_supported {
            return Err("Virgl not supported");
        }
        let ctx_id = (VIRGL_CTX_ID_MIN..=VIRGL_MAX_CTX)
            .find(|&id| (self.virgl_ctx_mask & (1 << (id - 1))) == 0)
            .ok_or("No free Virgl context slot")?;

        let req_size = core::mem::size_of::<VirtioGpuCtxCreate>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc ctx_create failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }

        let req_base = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(
                req_base.add(core::mem::offset_of!(VirtioGpuCtxCreate, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32,
                VIRTIO_GPU_CMD_CTX_CREATE,
            );
            core::ptr::write_unaligned(
                req_base.add(core::mem::offset_of!(VirtioGpuCtxCreate, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctx_id)) as *mut u32,
                ctx_id,
            );
            let nlen = debug_name.len().min(64) as u32;
            core::ptr::write_unaligned(
                req_base.add(core::mem::offset_of!(VirtioGpuCtxCreate, nlen)) as *mut u32,
                nlen,
            );
            if nlen > 0 {
                let dst = req_base.add(core::mem::offset_of!(VirtioGpuCtxCreate, debug_name));
                core::ptr::copy_nonoverlapping(debug_name.as_ptr(), dst, nlen as usize);
            }
        }

        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        self.virgl_ctx_mask |= 1 << (ctx_id - 1);
        Ok(ctx_id)
    }

    /// Destroy a Virgl 3D context.
    pub fn virgl_ctx_destroy(&mut self, ctx_id: u32) -> Result<(), &'static str> {
        if !self.virgl_supported {
            return Err("Virgl not supported");
        }
        if ctx_id < VIRGL_CTX_ID_MIN || ctx_id > VIRGL_MAX_CTX {
            return Err("Invalid ctx_id");
        }
        if (self.virgl_ctx_mask & (1 << (ctx_id - 1))) == 0 {
            return Err("Context not allocated");
        }

        let req_size = core::mem::size_of::<VirtioGpuCtrlHdr>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc ctx_destroy failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }

        let req_base = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(
                req_base.add(core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32,
                VIRTIO_GPU_CMD_CTX_DESTROY,
            );
            core::ptr::write_unaligned(
                req_base.add(core::mem::offset_of!(VirtioGpuCtrlHdr, ctx_id)) as *mut u32,
                ctx_id,
            );
        }

        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        self.virgl_ctx_mask &= !(1 << (ctx_id - 1));
        Ok(())
    }

    /// Attach resource to context (Virgl).
    pub fn virgl_ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32) -> Result<(), &'static str> {
        if !self.virgl_supported {
            return Err("Virgl not supported");
        }
        let req_size = core::mem::size_of::<VirtioGpuCtxResource>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc ctx_attach_resource failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }
        let r = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuCtxResource, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuCtxResource, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctx_id)) as *mut u32, ctx_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuCtxResource, resource_id)) as *mut u32, resource_id);
        }
        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// Detach resource from context (Virgl).
    pub fn virgl_ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32) -> Result<(), &'static str> {
        if !self.virgl_supported {
            return Err("Virgl not supported");
        }
        let req_size = core::mem::size_of::<VirtioGpuCtxResource>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc ctx_detach_resource failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }
        let r = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuCtxResource, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuCtxResource, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctx_id)) as *mut u32, ctx_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuCtxResource, resource_id)) as *mut u32, resource_id);
        }
        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// Create a 3D resource (Virgl). resource_id must be unique, not 0.
    pub fn virgl_resource_create_3d(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        target: u32,
        format: u32,
        bind: u32,
        width: u32,
        height: u32,
        depth: u32,
        array_size: u32,
        last_level: u32,
        nr_samples: u32,
        flags: u32,
    ) -> Result<(), &'static str> {
        if !self.virgl_supported {
            return Err("Virgl not supported");
        }
        if resource_id == 0 {
            return Err("resource_id 0 reserved");
        }

        let req_size = core::mem::size_of::<VirtioGpuResourceCreate3d>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc resource_create_3d failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }

        let r = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_RESOURCE_CREATE_3D);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctx_id)) as *mut u32, ctx_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, resource_id)) as *mut u32, resource_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, target)) as *mut u32, target);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, format)) as *mut u32, format);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, bind)) as *mut u32, bind);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, width)) as *mut u32, width);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, height)) as *mut u32, height);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, depth)) as *mut u32, depth);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, array_size)) as *mut u32, array_size);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, last_level)) as *mut u32, last_level);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, nr_samples)) as *mut u32, nr_samples);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuResourceCreate3d, flags)) as *mut u32, flags);
        }

        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// Transfer guest memory to 3D resource (Virgl).
    pub fn virgl_transfer_to_host_3d(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        box_x: u32,
        box_y: u32,
        box_z: u32,
        box_w: u32,
        box_h: u32,
        box_d: u32,
        offset: u64,
        level: u32,
        stride: u32,
        layer_stride: u32,
    ) -> Result<(), &'static str> {
        if !self.virgl_supported {
            return Err("Virgl not supported");
        }

        let req_size = core::mem::size_of::<VirtioGpuTransferHost3d>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc transfer_to_host_3d failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }

        let r = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctx_id)) as *mut u32, ctx_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, x)) as *mut u32, box_x);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, y)) as *mut u32, box_y);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, z)) as *mut u32, box_z);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, w)) as *mut u32, box_w);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, h)) as *mut u32, box_h);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, d)) as *mut u32, box_d);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, offset)) as *mut u64, offset);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, resource_id)) as *mut u32, resource_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, level)) as *mut u32, level);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, stride)) as *mut u32, stride);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, layer_stride)) as *mut u32, layer_stride);
        }

        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// Transfer 3D resource to guest memory (Virgl).
    pub fn virgl_transfer_from_host_3d(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        box_x: u32,
        box_y: u32,
        box_z: u32,
        box_w: u32,
        box_h: u32,
        box_d: u32,
        offset: u64,
        level: u32,
        stride: u32,
        layer_stride: u32,
    ) -> Result<(), &'static str> {
        if !self.virgl_supported {
            return Err("Virgl not supported");
        }

        let req_size = core::mem::size_of::<VirtioGpuTransferHost3d>();
        let (req_ptr, req_phys) = crate::memory::alloc_dma_buffer(req_size, 64)
            .ok_or("alloc transfer_from_host_3d failed")?;
        unsafe { core::ptr::write_bytes(req_ptr, 0, req_size); }

        let r = req_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctx_id)) as *mut u32, ctx_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, x)) as *mut u32, box_x);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, y)) as *mut u32, box_y);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, z)) as *mut u32, box_z);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, w)) as *mut u32, box_w);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, h)) as *mut u32, box_h);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, r#box) + core::mem::offset_of!(VirtioGpuBox, d)) as *mut u32, box_d);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, offset)) as *mut u64, offset);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, resource_id)) as *mut u32, resource_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, level)) as *mut u32, level);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, stride)) as *mut u32, stride);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuTransferHost3d, layer_stride)) as *mut u32, layer_stride);
        }

        self.send_ctrl_cmd_nodata(req_phys, req_size)?;
        unsafe { crate::memory::free_dma_buffer(req_ptr, req_size, 64); }
        Ok(())
    }

    /// Submit 3D command buffer to context (Virgl).
    /// cmd_data: virgl command stream from Mesa. Must be in DMA-accessible memory.
    /// The caller must provide (cmd_ptr, cmd_phys) for the command buffer.
    pub fn virgl_submit_3d(&mut self, ctx_id: u32, cmd_data: &[u8]) -> Result<(), &'static str> {
        if !self.virgl_supported {
            return Err("Virgl not supported");
        }
        let size = cmd_data.len() as u32;
        if size == 0 {
            return Ok(());
        }

        let hdr_size = core::mem::size_of::<VirtioGpuCmdSubmit>();
        let total_size = hdr_size + cmd_data.len();
        let (buf_ptr, buf_phys) = crate::memory::alloc_dma_buffer(total_size, 64)
            .ok_or("alloc submit_3d failed")?;
        unsafe { core::ptr::write_bytes(buf_ptr, 0, total_size); }

        let r = buf_ptr as *mut u8;
        unsafe {
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuCmdSubmit, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctrl_type)) as *mut u32, VIRTIO_GPU_CMD_SUBMIT_3D);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuCmdSubmit, hdr) + core::mem::offset_of!(VirtioGpuCtrlHdr, ctx_id)) as *mut u32, ctx_id);
            core::ptr::write_unaligned(r.add(core::mem::offset_of!(VirtioGpuCmdSubmit, size)) as *mut u32, size);
            core::ptr::copy_nonoverlapping(cmd_data.as_ptr(), r.add(hdr_size), cmd_data.len());
        }

        self.send_ctrl_cmd_nodata(buf_phys, total_size)?;
        unsafe { crate::memory::free_dma_buffer(buf_ptr, total_size, 64); }
        Ok(())
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

    // Search for VirtIO GPU devices (PCI 0x1050 = virtio-gpu, virtio-vga, virtio-vga-gl)
    for dev in crate::pci::get_all_devices() {
        if dev.is_virtio() && dev.device_id == 0x1050 {
            serial::serial_print("[VirtIO-GPU] Found virtio-gpu on PCI Bus=");
            serial::serial_print_dec(dev.bus as u64);
            serial::serial_print(" Dev=");
            serial::serial_print_dec(dev.device as u64);
            serial::serial_print("\n");

            let mut gpu_initialized = false;
            unsafe {
                crate::pci::enable_device(&dev, true);

                // virtio-vga / virtio-vga-gl may have VGA at BAR0, virtio config at BAR2
                for bar_idx in [0u8, 2u8] {
                    let bar = crate::pci::get_bar(&dev, bar_idx);
                    serial::serial_print("[VirtIO-GPU] BAR");
                    serial::serial_print_dec(bar_idx as u64);
                    serial::serial_print("=");
                    serial::serial_print_hex(bar);
                    serial::serial_print(if (bar & 1) != 0 { " (I/O)" } else { " (MMIO)" });
                    serial::serial_print("\n");

                    if (bar & 1) != 0 {
                        let io_base = (bar & !0x3) as u16;
                        if io_base == 0 { continue; }
                        let mut gpu = VirtIOGpuDevice::new_from_pci_io(io_base);
                        if gpu.init_legacy_pci() {
                            serial::serial_print("[VirtIO-GPU] Initialized (I/O BAR");
                            serial::serial_print_dec(bar_idx as u64);
                            serial::serial_print(") at port ");
                            serial::serial_print_hex(io_base as u64);
                            if gpu.virgl_supported { serial::serial_print(" [Virgl 3D]"); }
                            serial::serial_print("\n");
                            if let Ok((w, h)) = gpu.get_display_info() {
                                serial::serial_print("[VirtIO-GPU] Display: ");
                                serial::serial_print_dec(w as u64);
                                serial::serial_print("x");
                                serial::serial_print_dec(h as u64);
                                serial::serial_print("\n");
                            }
                            GPU_DEVICES.lock().push(gpu);
                            gpu_initialized = true;
                            break;
                        }
                    } else {
                        let bar_phys = bar & 0xFFFFFFFFFFFFFFF0u64;
                        if bar_phys == 0 || bar_phys == 0xFFFFFFF0 || bar_phys == 0xFFFFFFFFFFFFFFF0 {
                            continue;
                        }
                        // Map MMIO region (VirtIO needs ~0x200 bytes) before accessing
                        const VIRTIO_MMIO_SIZE: usize = 0x200;
                        let mmio_base = crate::memory::map_mmio_range(bar_phys, VIRTIO_MMIO_SIZE);
                        let mut gpu = VirtIOGpuDevice::new_from_pci_mmio(mmio_base);
                        if gpu.init_mmio() {
                            serial::serial_print("[VirtIO-GPU] Initialized (MMIO BAR");
                            serial::serial_print_dec(bar_idx as u64);
                            serial::serial_print(") at ");
                            serial::serial_print_hex(mmio_base);
                            if gpu.virgl_supported { serial::serial_print(" [Virgl 3D]"); }
                            serial::serial_print("\n");
                            if let Ok((w, h)) = gpu.get_display_info() {
                                serial::serial_print("[VirtIO-GPU] Display: ");
                                serial::serial_print_dec(w as u64);
                                serial::serial_print("x");
                                serial::serial_print_dec(h as u64);
                                serial::serial_print("\n");
                            }
                            GPU_DEVICES.lock().push(gpu);
                            gpu_initialized = true;
                            break;
                        }
                    }
                }
                if !gpu_initialized {
                    // Try VirtIO 1.0 PCI modern (virtio-gpu-pci, virtio-vga)
                    if let Some(mut gpu) = VirtIOGpuDevice::init_mmio_modern(&dev) {
                        if let Ok((w, h)) = gpu.get_display_info() {
                            serial::serial_print("[VirtIO-GPU] Display: ");
                            serial::serial_print_dec(w as u64);
                            serial::serial_print("x");
                            serial::serial_print_dec(h as u64);
                            serial::serial_print("\n");
                        }
                        GPU_DEVICES.lock().push(gpu);
                        gpu_initialized = true;
                    }
                }
                if !gpu_initialized {
                    serial::serial_print("[VirtIO-GPU] All BARs and modern failed to init\n");
                }
            }
        }
    }

    serial::serial_print("[VirtIO] Total block devices: ");
    serial::serial_print_dec(BLOCK_DEVICES.lock().len() as u64);
    serial::serial_print(", GPU devices: ");
    serial::serial_print_dec(GPU_DEVICES.lock().len() as u64);
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

/// Check if a VirtIO GPU was initialized
pub fn has_virtio_gpu() -> bool {
    !GPU_DEVICES.lock().is_empty()
}

/// Check if the first VirtIO GPU has Virgl (3D) support
pub fn has_virtio_gpu_virgl() -> bool {
    let devices = GPU_DEVICES.lock();
    devices.first().map_or(false, |g| g.virgl_supported)
}

/// Create a Virgl 3D context on the first GPU. Returns ctx_id (1..=16) or None on failure.
pub fn virgl_ctx_create(debug_name: &[u8]) -> Option<u32> {
    let mut devices = GPU_DEVICES.lock();
    let dev = devices.first_mut()?;
    dev.virgl_ctx_create(debug_name).ok()
}

/// Destroy a Virgl 3D context. Returns true on success.
pub fn virgl_ctx_destroy(ctx_id: u32) -> bool {
    let mut devices = GPU_DEVICES.lock();
    let dev = match devices.first_mut() {
        Some(d) => d,
        None => return false,
    };
    dev.virgl_ctx_destroy(ctx_id).is_ok()
}

/// Attach resource to Virgl context. Returns true on success.
pub fn virgl_ctx_attach_resource(ctx_id: u32, resource_id: u32) -> bool {
    let mut devices = GPU_DEVICES.lock();
    let dev = match devices.first_mut() {
        Some(d) => d,
        None => return false,
    };
    dev.virgl_ctx_attach_resource(ctx_id, resource_id).is_ok()
}

/// Detach resource from Virgl context. Returns true on success.
pub fn virgl_ctx_detach_resource(ctx_id: u32, resource_id: u32) -> bool {
    let mut devices = GPU_DEVICES.lock();
    let dev = match devices.first_mut() {
        Some(d) => d,
        None => return false,
    };
    dev.virgl_ctx_detach_resource(ctx_id, resource_id).is_ok()
}

/// Create 3D resource. Returns true on success.
pub fn virgl_resource_create_3d(
    ctx_id: u32,
    resource_id: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
) -> bool {
    let mut devices = GPU_DEVICES.lock();
    let dev = match devices.first_mut() {
        Some(d) => d,
        None => return false,
    };
    dev.virgl_resource_create_3d(
        ctx_id, resource_id, target, format, bind,
        width, height, depth, array_size,
        last_level, nr_samples, flags,
    ).is_ok()
}

/// Transfer to host 3D. Returns true on success.
pub fn virgl_transfer_to_host_3d(
    ctx_id: u32,
    resource_id: u32,
    box_x: u32, box_y: u32, box_z: u32,
    box_w: u32, box_h: u32, box_d: u32,
    offset: u64, level: u32, stride: u32, layer_stride: u32,
) -> bool {
    let mut devices = GPU_DEVICES.lock();
    let dev = match devices.first_mut() {
        Some(d) => d,
        None => return false,
    };
    dev.virgl_transfer_to_host_3d(
        ctx_id, resource_id,
        box_x, box_y, box_z, box_w, box_h, box_d,
        offset, level, stride, layer_stride,
    ).is_ok()
}

/// Transfer from host 3D. Returns true on success.
pub fn virgl_transfer_from_host_3d(
    ctx_id: u32,
    resource_id: u32,
    box_x: u32, box_y: u32, box_z: u32,
    box_w: u32, box_h: u32, box_d: u32,
    offset: u64, level: u32, stride: u32, layer_stride: u32,
) -> bool {
    let mut devices = GPU_DEVICES.lock();
    let dev = match devices.first_mut() {
        Some(d) => d,
        None => return false,
    };
    dev.virgl_transfer_from_host_3d(
        ctx_id, resource_id,
        box_x, box_y, box_z, box_w, box_h, box_d,
        offset, level, stride, layer_stride,
    ).is_ok()
}

/// Submit 3D command buffer. Returns true on success.
pub fn virgl_submit_3d(ctx_id: u32, cmd_data: &[u8]) -> bool {
    let mut devices = GPU_DEVICES.lock();
    let dev = match devices.first_mut() {
        Some(d) => d,
        None => return false,
    };
    dev.virgl_submit_3d(ctx_id, cmd_data).is_ok()
}

/// Allocate backing memory for Virgl 3D resource. Returns (phys_addr, size) for kernel to map.
/// Caller maps at identity (vaddr = phys) and returns vaddr to userspace.
pub fn virgl_alloc_backing(size: usize) -> Option<(u64, usize)> {
    if size == 0 || size > 16 * 1024 * 1024 {
        return None; // max 16MB per allocation
    }
    let (ptr, phys) = crate::memory::alloc_dma_buffer(size, 4096)?;
    unsafe { core::ptr::write_bytes(ptr, 0, size); }
    Some((phys, size))
}

/// Attach backing memory to Virgl resource. phys_addr must be from virgl_alloc_backing (identity-mapped as vaddr).
pub fn virgl_resource_attach_backing(resource_id: u32, phys_addr: u64, size: usize) -> bool {
    if size == 0 {
        return false;
    }
    let mut devices = GPU_DEVICES.lock();
    let dev = match devices.first_mut() {
        Some(d) => d,
        None => return false,
    };
    dev.resource_attach_backing(resource_id, &[(phys_addr, size as u32)]).is_ok()
}

/// Get display dimensions from first VirtIO GPU (if available)
pub fn get_gpu_display_info() -> Option<(u32, u32)> {
    let mut devices = GPU_DEVICES.lock();
    if let Some(dev) = devices.get_mut(0) {
        dev.get_display_info().ok()
    } else {
        None
    }
}

/// Set hardware cursor position on first VirtIO GPU (scanout 0)
pub fn set_cursor_position(x: u32, y: u32) -> bool {
    let mut devices = GPU_DEVICES.lock();
    if let Some(dev) = devices.get_mut(0) {
        if !dev.cursor_resource_created {
            let _ = dev.init_cursor();
        }
        dev.move_cursor(0, x, y).is_ok()
    } else {
        false
    }
}

/// Allocate a VirtIO GPU display buffer (2D resource with DMA backing).
/// Returns Some((phys_addr, resource_id, pitch, size)) on success.
/// The caller must map phys_addr to userspace and call gpu_present to flip.
pub fn gpu_alloc_display_buffer(width: u32, height: u32) -> Option<(u64, u32, u32, usize)> {
    let pitch = width.wrapping_mul(4);
    let size = (pitch as usize).wrapping_mul(height as usize);
    if size == 0 || width == 0 || height == 0 {
        return None;
    }
    let (buf_ptr, buf_phys) = crate::memory::alloc_dma_buffer(size, 4096)?;
    unsafe { core::ptr::write_bytes(buf_ptr, 0, size); }

    let mut devices = GPU_DEVICES.lock();
    let dev = devices.get_mut(0)?;
    if dev.resource_create_2d(DISPLAY_BUFFER_RESOURCE_ID, VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM, width, height).is_err() {
        unsafe { crate::memory::free_dma_buffer(buf_ptr, size, 4096); }
        return None;
    }
    if dev.resource_attach_backing(DISPLAY_BUFFER_RESOURCE_ID, &[(buf_phys, size as u32)]).is_err() {
        unsafe { crate::memory::free_dma_buffer(buf_ptr, size, 4096); }
        return None;
    }
    if dev.set_scanout(0, DISPLAY_BUFFER_RESOURCE_ID, 0, 0, width, height).is_err() {
        unsafe { crate::memory::free_dma_buffer(buf_ptr, size, 4096); }
        return None;
    }
    Some((buf_phys, DISPLAY_BUFFER_RESOURCE_ID, pitch, size))
}

/// Present display buffer to screen: transfer guest memory to GPU and flush.
pub fn gpu_present(resource_id: u32, x: u32, y: u32, w: u32, h: u32) -> bool {
    let mut devices = GPU_DEVICES.lock();
    let dev = match devices.get_mut(0) {
        Some(d) => d,
        None => return false,
    };
    dev.transfer_to_host_2d(resource_id, x, y, w, h, 0).is_ok()
        && dev.resource_flush(resource_id, x, y, w, h).is_ok()
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
            2 => {
                 // SEEK_END - for raw disks, we need the disk size. 
                 // For now, let's assume a dummy large size or just return EINVAL if unknown.
                 // Actually, many block drivers don't support SEEK_END easily without size info.
                 // However, FileSystemScheme DOES support it because it knows file size.
                 // Let's just return current offset if size unknown, or EINVAL.
                 return Err(scheme_error::EINVAL)
            },
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
