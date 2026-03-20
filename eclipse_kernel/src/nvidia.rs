//! NVIDIA GPU Driver Support (Nova-aligned)
//!
//! This module provides integration with NVIDIA GPUs for Eclipse OS, aligned with
//! the **Nova** open-source driver project (Linux kernel 6.15+) and NVIDIA's
//! open-gpu-kernel-modules (https://github.com/NVIDIA/open-gpu-kernel-modules).
//!
//! ## Nova (upstream reference)
//! Nova is the new open-source, Rust-written NVIDIA driver in the mainline Linux
//! kernel, intended to supersede Nouveau for GSP-based GPUs:
//! - **nova-core**: Core driver, abstraction around GPU hardware and firmware (GSP, Falcon, FWSEC, devinit, VBIOS)
//! - **nova-drm**: Second-level DRM driver for display/compute
//!
//! Eclipse follows the same architecture: core (this module + sidewind_nvidia) and
//! display via VirtIO/GOP or userspace display service.
//!
//! ## Supported GPUs
//! GSP-based NVIDIA GPUs only (Turing and newer):
//! - Turing (RTX 20 series)
//! - Ampere (RTX 30 series)
//! - Ada Lovelace (RTX 40 series)
//! - Hopper (H100, H200, etc.)
//! - Blackwell (RTX 50 series, B100, B200)
//!
//! ## Features
//! - PCI device detection and enumeration
//! - GPU identification via PCI device ID + PMC_BOOT_0 hardware cross-check
//! - BAR (Base Address Register) mapping (32 MB for Turing+)
//! - VRAM size detection from NV_PFB_CSTATUS register
//! - GPU temperature reading from THERM registers
//! - PMC engine enable before GSP boot
//! - GSP firmware loading and Falcon CPUCTL boot sequence
//! - GSP RPC infrastructure
//! - Multi-GPU support
//!
//! ## References
//! - Nova: https://docs.kernel.org/next/gpu/nova/index.html
//! - open-gpu-kernel-modules: https://github.com/NVIDIA/open-gpu-kernel-modules

use crate::pci::{PciDevice, find_nvidia_gpus, get_bar, get_bar_size};
use crate::memory::{map_mmio_range, map_framebuffer_kernel, PHYS_MEM_OFFSET, GPU_FW_PHYS_BASE, GPU_FW_MAX_SIZE, GPU_RPC_PHYS_BASE, GPU_RPC_MAX_SIZE};
use crate::serial;
use crate::filesystem;
use alloc::vec::Vec;
use alloc::vec;
use spin::Mutex;

// Use our shared NVIDIA abstraction crate
use sidewind_nvidia::registers::*;
use sidewind_nvidia::gsp::*;

use core::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// Stored NVIDIA framebuffer info for display fallback (BAR1 / linear VRAM aperture).
/// Populated during nvidia::init() and used by sys_get_framebuffer_info /
/// sys_map_framebuffer when neither EFI GOP nor VirtIO is available.
#[derive(Clone, Copy)]
struct NvidiaFbInfo {
    phys: u64,      // Actual screen scanout address (may be at an offset)
    bar1_phys: u64, // Base address of the PCI BAR1 aperture
    bar1_size: u64,
    width: u32,
    height: u32,
    pitch: u32,
}

static NVIDIA_FB_INFO: Mutex<Option<NvidiaFbInfo>> = Mutex::new(None);

/// Kernel virtual address of the BAR1 mapping created by map_framebuffer_kernel().
/// All kernel-side VRAM access (fill_rect, blit_rect, page_flip shadow-blit) uses
/// this base instead of PHYS_MEM_OFFSET + phys, which only works when the HHDM
/// covers the physical address (it does NOT for high-address BARs ≥ top-of-RAM).
static NVIDIA_BAR1_KERNEL_VADDR: AtomicU64 = AtomicU64::new(0);

/// Simple chunk-based VRAM allocator for BAR1 aperture
/// Bitmap-based VRAM allocator for BAR1 aperture (4KB page granularity)
struct NvidiaVramAllocator {
    base_phys: u64,
    total_size: u64,
    bitmap: Vec<u64>, // Use u64 for faster bit scanning
}

impl NvidiaVramAllocator {
    fn new(base_phys: u64, total_size: u64) -> Self {
        let num_pages = (total_size / 4096) as usize;
        let num_u64s = (num_pages + 63) / 64;
        Self {
            base_phys,
            total_size,
            bitmap: vec![0; num_u64s],
        }
    }

    fn alloc(&mut self, size: usize, align: usize) -> Option<u64> {
        let num_pages = (size + 4095) / 4096;
        let align_pages = (align.max(4096) / 4096).max(1);
        
        let total_bits = (self.total_size / 4096) as usize;
        
        // Find a contiguous range of free bits
        let mut count = 0;
        let mut start_bit = 0;
        
        for bit in 0..total_bits {
            let uidx = bit / 64;
            let ubit = bit % 64;
            
            let is_free = (self.bitmap[uidx] & (1 << ubit)) == 0;
            
            if is_free {
                if count == 0 {
                    // Check alignment
                    if bit % align_pages != 0 {
                        continue;
                    }
                    start_bit = bit;
                }
                count += 1;
                if count >= num_pages {
                    // Mark as used
                    for i in 0..num_pages {
                        let b = start_bit + i;
                        self.bitmap[b / 64] |= 1 << (b % 64);
                    }
                    return Some(self.base_phys + (start_bit as u64 * 4096));
                }
            } else {
                count = 0;
            }
        }
        None
    }

    fn free(&mut self, phys_addr: u64, size: usize) {
        let offset = phys_addr.saturating_sub(self.base_phys);
        if offset >= self.total_size { return; }
        
        let start_bit = (offset / 4096) as usize;
        let num_pages = (size + 4095) / 4096;
        
        for i in 0..num_pages {
            let b = start_bit + i;
            if b / 64 < self.bitmap.len() {
                self.bitmap[b / 64] &= !(1 << (b % 64));
            }
        }
    }

    fn used_pages(&self) -> u64 {
        // Bitmap: 0 = free, 1 = used (1 bit per 4KB page).
        let mut used: u64 = 0;
        for &w in self.bitmap.iter() {
            used += w.count_ones() as u64;
        }
        used
    }

    fn used_bytes(&self) -> u64 {
        self.used_pages().saturating_mul(4096)
    }
}

static VRAM_ALLOCATOR: Mutex<Option<NvidiaVramAllocator>> = Mutex::new(None);

/// Kernel mapping of BAR1 framebuffer (size mapped) so we only map once.
static NVIDIA_BAR1_MAPPED_SIZE: Mutex<Option<usize>> = Mutex::new(None);

pub struct NvidiaDrmDriver;

/// Dynamic interrupt handler for NVIDIA GPUs. 
/// Called by the assembly stub in interrupts.rs.
pub fn handle_interrupt() {
    // Current simple implementation: just log that we got an interrupt.
    // In a full driver, this would signal a condition variable or process RPC responses.
    // crate::serial::serial_print("[NVIDIA] ⚡ Interrupt received!\n");
}

impl crate::drm::DrmDriver for NvidiaDrmDriver {
    fn name(&self) -> &'static str { "nvidia-nova" }
    fn get_caps(&self) -> crate::drm::DrmCaps {
        let fb = NVIDIA_FB_INFO.lock();
        if let Some(f) = fb.as_ref() {
            crate::drm::DrmCaps {
                has_3d: true,
                has_cursor: true,
                max_width: f.width,
                max_height: f.height,
            }
        } else {
            crate::drm::DrmCaps { has_3d: false, has_cursor: false, max_width: 0, max_height: 0 }
        }
    }
    fn alloc_buffer(&self, size: usize) -> Option<crate::drm::GemHandle> {
        // Prefer system DMA memory for fast CPU access (Write-Back).
        // Reading from VRAM (Write-Combining) is extremely slow (UC-like),
        // which kills performance in software-assisted rendering/compositing.
        unsafe {
            if let Some((_ptr, phys)) = crate::memory::alloc_dma_buffer(size, 4096) {
                return Some(crate::drm::GemHandle { id: 0, size, phys_addr: phys });
            }
        }

        // Fallback to VRAM (BAR1 aperture) only if System RAM is exhausted
        {
            let mut allocator = VRAM_ALLOCATOR.lock();
            if let Some(ref mut a) = *allocator {
                if let Some(phys) = a.alloc(size, 4096) {
                    return Some(crate::drm::GemHandle { id: 0, size, phys_addr: phys });
                }
            }
        }

        None
    }
    fn free_buffer(&self, handle: crate::drm::GemHandle) {
        let mut allocator = VRAM_ALLOCATOR.lock();
        if let Some(ref mut a) = *allocator {
            a.free(handle.phys_addr, handle.size);
        }
    }
    fn create_fb(&self, handle_id: u32, _width: u32, _height: u32, _pitch: u32) -> Option<u32> {
        // Simple case: just treat the handle as the FB ID.
        Some(handle_id)
    }
    fn page_flip(&self, fb_id: u32) -> bool {
        let fb = match crate::drm::get_fb(fb_id) {
            Some(fb) => fb,
            None => return false,
        };
        let fb_phys = fb.phys_addr;

        // Shadow blit: copy rendered GEM buffer → scanout via the kernel
        // BAR1 mapping.
        let (scanout_phys, bar1_phys, bar1_size, width, height, pitch) = {
            let info = NVIDIA_FB_INFO.lock();
            if let Some(i) = info.as_ref() {
                (i.phys, i.bar1_phys, i.bar1_size, i.width, i.height, i.pitch)
            } else {
                return false;
            }
        };

        // If the GEM buffer IS the scanout base, nothing to blit.
        if fb_phys == scanout_phys {
            return true;
        }

        let src_vaddr = if fb_phys >= bar1_phys && fb_phys < bar1_phys + bar1_size {
            // Source is in VRAM (BAR1)
            let vbase = NVIDIA_BAR1_KERNEL_VADDR.load(AtomicOrdering::Relaxed);
            if vbase == 0 { return false; }
            (vbase + (fb_phys - bar1_phys)) as *const u8
        } else {
            // Source is in System RAM
            crate::memory::phys_to_virt(fb_phys) as *const u8
        };

        let dst_vaddr = {
            let vbase = NVIDIA_BAR1_KERNEL_VADDR.load(AtomicOrdering::Relaxed);
            if vbase == 0 { return false; }
            // CRITICAL FIX: Add offset from BAR1 base to scanout actual phys
            let offset = scanout_phys.saturating_sub(bar1_phys);
            (vbase + offset) as *mut u8
        };

        let src_pitch = fb.pitch as usize;
        let dst_pitch = pitch as usize;
        let copy_width = (width as usize * 4).min(src_pitch).min(dst_pitch);

        for py in 0..height as usize {
            unsafe {
                let src_row = src_vaddr.add(py * src_pitch);
                let dst_row = dst_vaddr.add(py * dst_pitch);
                core::ptr::copy_nonoverlapping(src_row, dst_row, copy_width);
            }
        }
        true
    }
    fn set_cursor(&self, _crtc_id: u32, x: i32, y: i32, _handle: u32, flags: u32) -> bool {
        const DRM_CURSOR_MOVE: u32 = 0x02;
        if (flags & DRM_CURSOR_MOVE) != 0 {
            crate::sw_cursor::update(x as u32, y as u32);
            return true;
        }
        false
    }
    fn wait_vblank(&self, _crtc_id: u32) -> bool {
        // Simplified: wait_vblank currently just yields if the driver doesn't have 
        // a real hardware vblank interrupt implemented yet.
        crate::scheduler::yield_cpu();
        true
    }
    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        if NVIDIA_FB_INFO.lock().is_some() {
            (Vec::new(), alloc::vec![7000], alloc::vec![6000])
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        }
    }
    fn get_connector(&self, id: u32) -> Option<crate::drm::DrmConnector> {
        if id == 6000 {
            Some(crate::drm::DrmConnector { id, connected: true, mm_width: 0, mm_height: 0 })
        } else { None }
    }
    fn get_crtc(&self, id: u32) -> Option<crate::drm::DrmCrtc> {
        if id == 7000 {
            Some(crate::drm::DrmCrtc { id, fb_id: 0, x: 0, y: 0 })
        } else { None }
    }
    fn get_plane(&self, id: u32) -> Option<crate::drm::DrmPlane> {
        if id == 8000 {
            Some(crate::drm::DrmPlane {
                id,
                crtc_id: 7000,
                fb_id: 0,
                possible_crtcs: 1, // Bitmask for CRTC 7000 (first CRTC)
                plane_type: 1, // Primary
            })
        } else {
            None
        }
    }
    fn get_planes(&self) -> Vec<u32> {
        if NVIDIA_FB_INFO.lock().is_some() {
            alloc::vec![8000]
        } else {
            Vec::new()
        }
    }
    fn set_plane(&self, plane_id: u32, crtc_id: u32, fb_id: u32, _x: i32, _y: i32, _w: u32, _h: u32, _src_x: u32, _src_y: u32, _src_w: u32, _src_h: u32) -> bool {
        // For now, only handle the primary plane by performing a page flip
        if plane_id == 8000 && crtc_id == 7000 {
            return self.page_flip(fb_id);
        }
        false
    }
}

/// Fill a rectangle on the NVIDIA BAR1 framebuffer (used by sys_gpu_command(1, 0, ...)).
/// Payload: x (u32), y (u32), w (u32), h (u32), color (u32) = 20 bytes, little-endian.
/// Maps BAR1 in kernel on first use and writes pixels (32bpp ARGB).
pub fn fill_rect(payload: &[u8]) -> bool {
    if payload.len() < 20 {
        return false;
    }
    let (fb_phys, bar1_phys, width, height, pitch) = match get_nvidia_fb_info() {
        Some(t) => t,
        None => return false,
    };
    let x = u32::from_le_bytes(payload[0..4].try_into().unwrap_or([0; 4]));
    let y = u32::from_le_bytes(payload[4..8].try_into().unwrap_or([0; 4]));
    let w = u32::from_le_bytes(payload[8..12].try_into().unwrap_or([0; 4]));
    let h = u32::from_le_bytes(payload[12..16].try_into().unwrap_or([0; 4]));
    let color = u32::from_le_bytes(payload[16..20].try_into().unwrap_or([0; 4]));

    let x = x.min(width);
    let y = y.min(height);
    let w = w.saturating_sub(0).min(width.saturating_sub(x));
    let h = h.saturating_sub(0).min(height.saturating_sub(y));
    if w == 0 || h == 0 {
        return true;
    }

    // Use the kernel BAR1 mapping created during nvidia::init().
    let vbase = NVIDIA_BAR1_KERNEL_VADDR.load(AtomicOrdering::Relaxed);
    if vbase == 0 {
        return false;
    }

    // CRITICAL FIX: Apply offset from BAR1 base to the actual framebuffer phys
    let (fb_phys, bar1_p) = {
        let info = NVIDIA_FB_INFO.lock();
        if let Some(i) = info.as_ref() {
            (i.phys, i.bar1_phys)
        } else {
            return false;
        }
    };
    let offset = fb_phys.saturating_sub(bar1_p);
    let ptr = (vbase + offset) as *mut u32;

    let pitch_u32 = (pitch as usize / 4).max(1);
    for py in 0..h {
        let row_start = (y + py) as usize * pitch_u32 + (x as usize);
        for px in 0..w {
            unsafe {
                core::ptr::write_volatile(ptr.add(row_start + px as usize), color);
            }
        }
    }
    true
}

/// Blit (copy) a rectangle within the NVIDIA BAR1 framebuffer (sys_gpu_command(1, 1, ...)).
/// Payload: src_x, src_y, dst_x, dst_y, w, h (6×u32 = 24 bytes), little-endian.
/// Overlapping regions are handled by copying in reverse row order when dst_y > src_y.
pub fn blit_rect(payload: &[u8]) -> bool {
    if payload.len() < 24 {
        return false;
    }
    let (fb_phys, bar1_phys, width, height, pitch) = match get_nvidia_fb_info() {
        Some(t) => t,
        None => return false,
    };
    let src_x = u32::from_le_bytes(payload[0..4].try_into().unwrap_or([0; 4]));
    let src_y = u32::from_le_bytes(payload[4..8].try_into().unwrap_or([0; 4]));
    let dst_x = u32::from_le_bytes(payload[8..12].try_into().unwrap_or([0; 4]));
    let dst_y = u32::from_le_bytes(payload[12..16].try_into().unwrap_or([0; 4]));
    let w = u32::from_le_bytes(payload[16..20].try_into().unwrap_or([0; 4]));
    let h = u32::from_le_bytes(payload[20..24].try_into().unwrap_or([0; 4]));

    let w = w.min(width.saturating_sub(src_x)).min(width.saturating_sub(dst_x));
    let h = h.min(height.saturating_sub(src_y)).min(height.saturating_sub(dst_y));
    if w == 0 || h == 0 {
        return true;
    }

    // Use the kernel BAR1 mapping created during nvidia::init().
    let vbase = NVIDIA_BAR1_KERNEL_VADDR.load(AtomicOrdering::Relaxed);
    if vbase == 0 {
        return false;
    }

    // CRITICAL FIX: Apply offset from BAR1 base to the actual framebuffer phys
    let offset = fb_phys.saturating_sub(bar1_phys);
    let ptr = (vbase + offset) as *mut u32;

    let pitch_u32 = (pitch as usize / 4).max(1);
    let same_row_overlap = dst_y == src_y && dst_x > src_x && dst_x < src_x + w;
    let overlap_down = dst_y > src_y && dst_y < src_y + h;

    if same_row_overlap {
        for py in 0..h {
            let src_row = (src_y + py) as usize * pitch_u32 + (src_x as usize);
            let dst_row = (dst_y + py) as usize * pitch_u32 + (dst_x as usize);
            unsafe {
                for i in (0..w as usize).rev() {
                    core::ptr::write(ptr.add(dst_row + i), core::ptr::read(ptr.add(src_row + i)));
                }
            }
        }
    } else if overlap_down {
        for py in (0..h).rev() {
            let src_row = (src_y + py) as usize * pitch_u32 + (src_x as usize);
            let dst_row = (dst_y + py) as usize * pitch_u32 + (dst_x as usize);
            unsafe {
                core::ptr::copy(ptr.add(src_row), ptr.add(dst_row), w as usize);
            }
        }
    } else {
        for py in 0..h {
            let src_row = (src_y + py) as usize * pitch_u32 + (src_x as usize);
            let dst_row = (dst_y + py) as usize * pitch_u32 + (dst_x as usize);
            unsafe {
                core::ptr::copy(ptr.add(src_row), ptr.add(dst_row), w as usize);
            }
        }
    }
    true
}

/// Blit from a specific GEM handle to the primary framebuffer (sys_gpu_command(1, 2, ...)).
/// Payload: src_handle:u32, src_x:u32, src_y:u32, dst_x:u32, dst_y:u32, w:u32, h:u32 (7×u32 = 28 bytes).
pub fn blit_from_handle(payload: &[u8]) -> bool {
    if payload.len() < 28 {
        return false;
    }
    
    let src_handle_id = u32::from_le_bytes(payload[0..4].try_into().unwrap_or([0; 4]));
    let src_x = u32::from_le_bytes(payload[4..8].try_into().unwrap_or([0; 4]));
    let src_y = u32::from_le_bytes(payload[8..12].try_into().unwrap_or([0; 4]));
    let dst_x = u32::from_le_bytes(payload[12..16].try_into().unwrap_or([0; 4]));
    let dst_y = u32::from_le_bytes(payload[16..20].try_into().unwrap_or([0; 4]));
    let w = u32::from_le_bytes(payload[20..24].try_into().unwrap_or([0; 4]));
    let h = u32::from_le_bytes(payload[24..28].try_into().unwrap_or([0; 4]));

    let (fb_phys, bar1_phys, width, height, pitch) = match get_nvidia_fb_info() {
        Some(t) => t,
        None => return false,
    };

    let src_handle = match crate::drm::get_handle(src_handle_id) {
        Some(h) => h,
        None => return false,
    };

    let w = w.min(width.saturating_sub(src_x)).min(width.saturating_sub(dst_x));
    let h = h.min(height.saturating_sub(src_y)).min(height.saturating_sub(dst_y));
    if w == 0 || h == 0 {
        return true;
    }

    // We already have all info from get_nvidia_fb_info() at the start of the function
    let bar1_size = {
        let info = NVIDIA_FB_INFO.lock();
        info.as_ref().map(|i| i.bar1_size).unwrap_or(0)
    };

    // We need the source pitch. Try to find the framebuffer for this handle.
    let src_pitch = if let Some(fb) = crate::drm::get_fb_by_gem_handle(src_handle_id) {
        fb.pitch as usize
    } else {
        width as usize * 4 // Fallback
    };
    let src_ptr = if src_handle.phys_addr >= bar1_phys && src_handle.phys_addr < bar1_phys + bar1_size {
        let vbase = NVIDIA_BAR1_KERNEL_VADDR.load(AtomicOrdering::Relaxed);
        if vbase == 0 { return false; }
        (vbase + (src_handle.phys_addr - bar1_phys)) as *const u32
    } else {
        crate::memory::phys_to_virt(src_handle.phys_addr) as *const u32
    };

    let dst_ptr = {
        let vbase = NVIDIA_BAR1_KERNEL_VADDR.load(AtomicOrdering::Relaxed);
        if vbase == 0 { return false; }
        // CRITICAL FIX: Offset from BAR1 to actual scanout
        let offset = fb_phys.saturating_sub(bar1_phys);
        (vbase + offset) as *mut u32
    };

    // Boundary checks to avoid kernel panic on out-of-bounds blit
    let src_max_pixels = src_handle.size / 4;
    let dst_max_pixels = ((pitch as usize) * (height as usize)) / 4;

    let dst_pitch = pitch as usize;

    for py in 0..h {
        let src_row_off = (src_y + py) as usize * (src_pitch / 4) + (src_x as usize);
        let dst_row_off = (dst_y + py) as usize * (dst_pitch / 4) + (dst_x as usize);
        
        if src_row_off + (w as usize) > src_max_pixels || dst_row_off + (w as usize) > dst_max_pixels {
            continue;
        }

        unsafe {
            core::ptr::copy_nonoverlapping(src_ptr.add(src_row_off), dst_ptr.add(dst_row_off), w as usize);
        }
    }
    
    true
}

/// Return (phys, bar1_phys, width, height, pitch) of the NVIDIA BAR1 linear aperture,
/// or None if no NVIDIA GPU was detected / BAR1 is not accessible.
pub fn get_nvidia_fb_info() -> Option<(u64, u64, u32, u32, u32)> {
    let guard = NVIDIA_FB_INFO.lock();
    guard.as_ref().map(|i| (i.phys, i.bar1_phys, i.width, i.height, i.pitch))
}

/// NVIDIA GPU Architecture Types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvidiaArchitecture {
    Unknown,
    Turing,      // RTX 20 series (2018) — TU1xx, chip_id 0x160..0x16F
    Ampere,      // RTX 30 series (2020) — GA1xx, chip_id 0x170..0x17F
    AdaLovelace, // RTX 40 series (2022) — AD1xx, chip_id 0x190..0x19F
    Hopper,      // H100/H200 (2022)     — GH1xx, chip_id 0x1B0..0x1BF
    Blackwell,   // RTX 50 series (2024) — GB2xx, chip_id >= 0x200
}

/// NVIDIA GPU Information
#[derive(Debug, Clone)]
pub struct NvidiaGpuInfo {
    pub pci_device: PciDevice,
    pub architecture: NvidiaArchitecture,
    pub name: &'static str,
    pub memory_size_mb: u32,
    pub cuda_cores: u32,
    pub sm_count: u32,  // Streaming Multiprocessor count
    pub rt_cores: u32,  // Ray Tracing cores
    pub tensor_cores: u32,  // Tensor cores for AI
}

/// Active NVIDIA GPU with mapped registers
pub struct NvidiaGpu {
    pub info: NvidiaGpuInfo,
    pub bar0_virt: u64,
    pub bar0_size: usize,
}

impl NvidiaGpuInfo {
    /// Create GPU info from PCI device
    pub fn from_pci_device(pci_device: PciDevice) -> Self {
        let (architecture, name, memory_size_mb, cuda_cores, sm_count) = 
            identify_gpu(pci_device.device_id);
        
        // Calculate RT cores and Tensor cores based on SM count
        let (rt_cores, tensor_cores) = match architecture {
            NvidiaArchitecture::Turing => (sm_count, sm_count * 8),
            NvidiaArchitecture::Ampere => (sm_count, sm_count * 4),
            NvidiaArchitecture::AdaLovelace => (sm_count, sm_count * 4),
            NvidiaArchitecture::Hopper => (sm_count, sm_count * 4),
            NvidiaArchitecture::Blackwell => (sm_count, sm_count * 4),
            _ => (0, 0),
        };
        
        Self {
            pci_device,
            architecture,
            name,
            memory_size_mb,
            cuda_cores,
            sm_count,
            rt_cores,
            tensor_cores,
        }
    }
    
    /// Check if this GPU is supported by open-gpu-kernel-modules
    pub fn is_open_source_supported(&self) -> bool {
        // Open-gpu-kernel-modules supports Turing and newer
        matches!(self.architecture, 
            NvidiaArchitecture::Turing | 
            NvidiaArchitecture::Ampere | 
            NvidiaArchitecture::AdaLovelace |
            NvidiaArchitecture::Hopper |
            NvidiaArchitecture::Blackwell
        )
    }
}

/// Derive NvidiaArchitecture from the PMC_BOOT_0 hardware register value.
/// Uses chip_id = PMC_BOOT_0[31:20] (12-bit field) as discriminant.
/// This matches the open-gpu-kernel-modules chip-detection logic.
pub fn arch_from_pmc_boot0(boot0: u32) -> NvidiaArchitecture {
    let chip_id = (boot0 >> PMC_BOOT0_CHIP_ID_SHIFT) & PMC_BOOT0_CHIP_ID_MASK;
    if chip_id >= PMC_BOOT0_CHIPID_BLACKWELL_MIN {
        NvidiaArchitecture::Blackwell
    } else if chip_id >= PMC_BOOT0_CHIPID_HOPPER_MIN && chip_id <= PMC_BOOT0_CHIPID_HOPPER_MAX {
        NvidiaArchitecture::Hopper
    } else if chip_id >= PMC_BOOT0_CHIPID_ADA_MIN && chip_id <= PMC_BOOT0_CHIPID_ADA_MAX {
        NvidiaArchitecture::AdaLovelace
    } else if chip_id >= PMC_BOOT0_CHIPID_AMPERE_MIN && chip_id <= PMC_BOOT0_CHIPID_AMPERE_MAX {
        NvidiaArchitecture::Ampere
    } else if chip_id >= PMC_BOOT0_CHIPID_TURING_MIN && chip_id <= PMC_BOOT0_CHIPID_TURING_MAX {
        NvidiaArchitecture::Turing
    } else {
        NvidiaArchitecture::Unknown
    }
}

/// Read VRAM size in MB from the NV_PFB_CSTATUS register (BAR0 + 0x10020C).
/// Returns 0 if the register is inaccessible or not yet programmed by the GPU.
/// From open-gpu-kernel-modules: dev_fb.h / NV_PFB_CSTATUS bits [14:0].
pub fn read_vram_size_mb(bar0_virt: u64) -> u32 {
    let raw = unsafe {
        core::ptr::read_volatile((bar0_virt + NV_PFB_CSTATUS as u64) as *const u32)
    };
    raw & NV_PFB_CSTATUS_MEM_SIZE_MASK
}

/// Read GPU core temperature in Celsius from the THERM engine (BAR0 + 0x20400).
/// Bits [8:0] are a signed 9-bit value.  Returns None if the register reads 0
/// or 0xFFFFFFFF (GPU not yet initialized / THERM not powered).
/// From open-gpu-kernel-modules: dev_therm.h / NV_THERM_TEMP.
pub fn read_temperature(bar0_virt: u64) -> Option<i32> {
    let raw = unsafe {
        core::ptr::read_volatile((bar0_virt + NV_THERM_TEMP as u64) as *const u32)
    };
    if raw == 0 || raw == 0xFFFF_FFFF {
        return None;
    }
    let raw9 = raw & NV_THERM_TEMP_VALUE_MASK;
    // Sign-extend 9-bit value
    let temp = if (raw9 & NV_THERM_TEMP_VALUE_SIGN_BIT) != 0 {
        (raw9 as i32) - 512
    } else {
        raw9 as i32
    };
    Some(temp)
}

/// Identify GPU based on PCI device ID.
/// Returns (architecture, name, memory_mb, cuda_cores, sm_count).
/// Device IDs sourced from open-gpu-kernel-modules / NVIDIA PCI ID database.
fn identify_gpu(device_id: u16) -> (NvidiaArchitecture, &'static str, u32, u32, u32) {
    match device_id {
        // ---------------------------------------------------------------
        // Blackwell — RTX 50 series (2024–2025), GB202/GB203/GB205/GB206
        // ---------------------------------------------------------------
        0x2B85 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5090", 32768, 21760, 170),
        0x2B89 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5080", 16384, 10752, 84),
        0x2C00 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5070 Ti", 16384,  8960, 70),
        0x2C20 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5070",   12288,  6144, 48),
        0x2C30 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5060 Ti", 8192,  4608, 36),
        // ---------------------------------------------------------------
        // Ada Lovelace — RTX 40 series (2022–2024), AD102/AD103/AD104/AD106/AD107
        // ---------------------------------------------------------------
        // Desktop (all variants)
        0x2684 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4090",           24576, 16384, 128),
        0x2685 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4090 D",         24576, 16384, 128),
        0x2704 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4080",           16384,  9728,  76),
        0x2702 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4080 Super",     16384, 10240,  80),
        0x2782 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Ti",        12288,  7680,  60),
        0x2783 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Ti Super",  16384,  8448,  66),
        0x2786 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070",           12288,  5888,  46),
        0x2788 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Super",     12288,  7168,  56),
        0x2803 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060 Ti",         8192,  4352,  34),
        0x2805 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060 Ti 16GB",   16384,  4352,  34),
        0x2882 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060",            8192,  3072,  24),
        0x2860 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4050",            6144,  2560,  20),
        // Ada Lovelace — mobile
        0x27A0 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4090 Laptop",    16384, 9728, 76),
        0x27B0 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4080 Laptop",    12288, 7424, 58),
        0x27B8 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Laptop",    8192,  4608, 36),
        0x27BA => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070 Ti Laptop", 12288, 5888, 46),
        0x27E0 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060 Laptop",    8192,  3072, 24),
        0x27E8 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4050 Laptop",    6144,  2560, 20),
        // Ada Lovelace — professional / data-centre
        0x26B1 => (NvidiaArchitecture::AdaLovelace, "RTX 6000 Ada Generation",   49152, 18176, 142),
        0x26B3 => (NvidiaArchitecture::AdaLovelace, "RTX 5000 Ada Generation",   32768, 12800, 100),
        0x26B9 => (NvidiaArchitecture::AdaLovelace, "RTX 4500 Ada Generation",   24576,  7680,  60),
        0x26BA => (NvidiaArchitecture::AdaLovelace, "RTX 4000 Ada Generation",   20480,  6144,  48),
        0x26BB => (NvidiaArchitecture::AdaLovelace, "RTX 4000 SFF Ada Generation", 20480, 6144, 48),
        0x26BD => (NvidiaArchitecture::AdaLovelace, "RTX 2000 Ada Generation",   16384,  3072,  24),
        0x2230 => (NvidiaArchitecture::AdaLovelace, "NVIDIA L40",                48128, 18176, 142),
        0x26B5 => (NvidiaArchitecture::AdaLovelace, "NVIDIA L40S",               49152, 18176, 142),
        // ---------------------------------------------------------------
        // Hopper — H100/H200 (2022–2023), GH100
        // ---------------------------------------------------------------
        0x2330 => (NvidiaArchitecture::Hopper, "NVIDIA H100 SXM5 80GB",  81920, 0, 132),
        0x2331 => (NvidiaArchitecture::Hopper, "NVIDIA H100 PCIe 80GB",  81920, 0, 114),
        0x2335 => (NvidiaArchitecture::Hopper, "NVIDIA H200 SXM5 141GB", 144384, 0, 132),
        0x2339 => (NvidiaArchitecture::Hopper, "NVIDIA H100 NVL",        94208, 0, 132),
        // ---------------------------------------------------------------
        // Ampere — RTX 30 series (2020–2022), GA102/GA103/GA104/GA106/GA107
        // ---------------------------------------------------------------
        // Desktop
        0x2204 => (NvidiaArchitecture::Ampere, "GeForce RTX 3090",      24576, 10496, 82),
        0x2208 => (NvidiaArchitecture::Ampere, "GeForce RTX 3090 Ti",   24576, 10752, 84),
        0x2206 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080",      10240,  8704, 68),
        0x220A => (NvidiaArchitecture::Ampere, "GeForce RTX 3080 12GB", 12288,  8960, 70),
        0x2216 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080 Ti",   12288, 10240, 80),
        0x2484 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070",       8192,  5888, 46),
        0x2488 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070 Ti",    8192,  6144, 48),
        0x2489 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 Ti",    8192,  4864, 38),
        0x2503 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060",       12288,  3584, 28),
        0x2504 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 8GB",    8192,  3584, 28),
        0x2544 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 12GB",  12288,  3584, 28),
        0x2571 => (NvidiaArchitecture::Ampere, "GeForce RTX 3050",        8192,  2560, 20),
        0x2582 => (NvidiaArchitecture::Ampere, "GeForce RTX 3050 6GB",    6144,  2048, 16),
        // Ampere — mobile
        0x2420 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080 Ti Laptop", 16384, 7424, 58),
        0x2460 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080 Laptop",   16384, 6144, 48),
        0x24A0 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070 Ti Laptop", 8192, 5888, 46),
        0x24B0 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070 Laptop",    8192, 5120, 40),
        0x24DC => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 Laptop",    6144, 3840, 30),
        0x25A0 => (NvidiaArchitecture::Ampere, "GeForce RTX 3050 Laptop",    4096, 2048, 16),
        // Ampere — professional / data-centre
        0x2235 => (NvidiaArchitecture::Ampere, "NVIDIA A100 80GB PCIe", 81920, 0, 108),
        0x20B5 => (NvidiaArchitecture::Ampere, "NVIDIA A100 80GB SXM4", 81920, 0, 108),
        0x20B2 => (NvidiaArchitecture::Ampere, "NVIDIA A100 40GB PCIe", 40960, 0, 108),
        0x20F5 => (NvidiaArchitecture::Ampere, "NVIDIA A10",             24576, 9216, 72),
        0x2236 => (NvidiaArchitecture::Ampere, "NVIDIA A10G",            24576, 9216, 72),
        0x2231 => (NvidiaArchitecture::Ampere, "NVIDIA A40",             49152, 10752, 84),
        0x2233 => (NvidiaArchitecture::Ampere, "NVIDIA A30",             24576, 0, 56),
        0x25B6 => (NvidiaArchitecture::Ampere, "NVIDIA A16",             16384, 0, 28),
        0x1EB8 => (NvidiaArchitecture::Ampere, "NVIDIA T4",              16384, 2560, 40),
        // ---------------------------------------------------------------
        // Turing — RTX 20 series / GTX 16 series (2018–2020), TU102..TU117
        // ---------------------------------------------------------------
        // RTX 20 series desktop
        0x1E02 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Ti",     11264, 4352, 68),
        0x1E04 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Super",   8192, 3072, 48),
        0x1E07 => (NvidiaArchitecture::Turing, "GeForce RTX 2080",         8192, 2944, 46),
        0x1E82 => (NvidiaArchitecture::Turing, "GeForce RTX 2070 Super",   8192, 2560, 40),
        0x1E84 => (NvidiaArchitecture::Turing, "GeForce RTX 2070",         8192, 2304, 36),
        0x1F02 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Super",   8192, 2176, 34),
        0x1F06 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Super",   8192, 2176, 34),
        0x1F07 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Super 8G", 8192, 2176, 34),
        0x1F03 => (NvidiaArchitecture::Turing, "GeForce RTX 2060",         6144, 1920, 30),
        0x1F08 => (NvidiaArchitecture::Turing, "GeForce RTX 2060",         6144, 1920, 30),
        0x1F0A => (NvidiaArchitecture::Turing, "GeForce RTX 2060",         6144, 1920, 30),
        0x1F0B => (NvidiaArchitecture::Turing, "GeForce RTX 2060 6GB",     6144, 1920, 30),
        // GTX 16 series desktop (Turing architecture, no RT cores)
        0x1F36 => (NvidiaArchitecture::Turing, "GeForce GTX 1660 Super",   6144, 1408, 22),
        0x1F44 => (NvidiaArchitecture::Turing, "GeForce GTX 1660 Ti",      6144, 1536, 24),
        0x1F82 => (NvidiaArchitecture::Turing, "GeForce GTX 1660",         6144, 1408, 22),
        0x1F91 => (NvidiaArchitecture::Turing, "GeForce GTX 1650 Super",   4096, 1280, 20),
        0x1F99 => (NvidiaArchitecture::Turing, "GeForce GTX 1650",         4096, 896,  14),
        // Turing — mobile
        0x1E90 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Laptop",  8192, 2944, 46),
        0x1E91 => (NvidiaArchitecture::Turing, "GeForce RTX 2070 Laptop",  8192, 2304, 36),
        0x1E93 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Laptop",  6144, 1920, 30),
        // Turing — professional
        0x1E30 => (NvidiaArchitecture::Turing, "Quadro RTX 6000",  24576, 4608, 72),
        0x1E78 => (NvidiaArchitecture::Turing, "Quadro RTX 5000",  16384, 3072, 48),
        0x1E36 => (NvidiaArchitecture::Turing, "Quadro RTX 4000",   8192, 2304, 36),
        // Default/Unknown
        _ => (NvidiaArchitecture::Unknown, "Unknown NVIDIA GPU", 0, 0, 0),
    }
}

/// NVIDIA GSP Firmware Loader
pub struct GspLoader;

impl GspLoader {
    /// Load GSP firmware from filesystem into a dedicated physical region
    pub fn load_firmware(path: &str) -> Result<NvidiaFirmware, &'static str> {
        serial::serial_print("[NVIDIA] Loading GSP firmware from ");
        serial::serial_print(path);
        serial::serial_print("...\n");

        let inode = filesystem::Filesystem::lookup_path(path).map_err(|_| "Firmware file not found")?;
        let size = filesystem::Filesystem::get_file_size(inode).map_err(|_| "Failed to get firmware size")?;
        
        serial::serial_print("[NVIDIA]   Firmware size: ");
        serial::serial_print_dec(size);
        serial::serial_print(" bytes\n");

        if size > GPU_FW_MAX_SIZE {
            return Err("Firmware too large (exceeds GPU_FW_MAX_SIZE)");
        }

        // Use the centralized GPU hardware region defined in memory.rs
        let phys_base = GPU_FW_PHYS_BASE;
        let virt_base = PHYS_MEM_OFFSET + phys_base;

        serial::serial_print("[NVIDIA]   Allocating firmware memory at Phys: 0x");
        serial::serial_print_hex(phys_base);
        serial::serial_print("\n");

        // Read the file in 4KB chunks
        let mut offset: u64 = 0;
        let mut chunk = [0u8; 4096];
        
        while offset < size {
            let to_read = core::cmp::min(4096, (size - offset) as usize);
            let bytes_read = filesystem::Filesystem::read_file_by_inode_at(inode, &mut chunk[..to_read], offset)?;
            
            if bytes_read == 0 { break; }

            // Copy chunk to target physical memory via Higher Half mapping
            unsafe {
                core::ptr::copy_nonoverlapping(
                    chunk.as_ptr(),
                    (virt_base + offset) as *mut u8,
                    bytes_read
                );
            }

            offset += bytes_read as u64;
            
            // Progress indicator every 1MB
            if offset % (1024 * 1024) == 0 {
                serial::serial_print(".");
            }
        }
        serial::serial_print(" Done\n");

        Ok(NvidiaFirmware {
            phys_base,
            virt_base,
            size: size as usize,
        })
    }
}

/// Represents loaded NVIDIA firmware in memory
pub struct NvidiaFirmware {
    pub phys_base: u64,
    pub virt_base: u64,
    pub size: usize,
}

/// GSP RPC Client
pub struct RpcClient {
    pub queue_virt: *mut GspRpcQueue,
    pub next_seq: u32,
}

impl RpcClient {
    pub fn new(phys_base: u64) -> Self {
        let virt = (PHYS_MEM_OFFSET + phys_base) as *mut GspRpcQueue;
        unsafe {
            GspRpcQueue::init_at(virt);
        }
        Self { 
            queue_virt: virt,
            next_seq: 1,
        }
    }

    pub fn send_command(&mut self, opcode: GspOpcode, payload: &[u8]) -> Result<u32, GspStatus> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        
        let header = GspHeader {
            opcode: opcode as u32,
            seq_num: seq,
            status: 0,
            payload_len: payload.len() as u32,
        };
        
        unsafe {
            (*self.queue_virt).push(header, payload)?;
        }
        Ok(seq)
    }

    pub fn poll_response(&mut self) -> Option<GspMessage<GSP_RPC_PAYLOAD_SIZE>> {
        unsafe {
            (*self.queue_virt).pop()
        }
    }
}

/// Initialize NVIDIA GPU subsystem
pub fn init() {
    serial::serial_print("[NVIDIA] Initializing NVIDIA GPU subsystem (Nova-aligned)...\n");
    serial::serial_print("[NVIDIA] Reference: Nova (Linux kernel 6.15+), open-gpu-kernel-modules\n");
    
    let gpus = find_nvidia_gpus();
    
    if gpus.is_empty() {
        serial::serial_print("[NVIDIA] No NVIDIA GPUs detected\n");
        return;
    }
    
    serial::serial_print("[NVIDIA] Found ");
    serial::serial_print_dec(gpus.len() as u64);
    serial::serial_print(" NVIDIA GPU(s)\n");
    
    // Pre-identify the primary GPU by matching BAR1 with UEFI GOP address.
    // If no match is found, default to the first GPU (index 0).
    let gop_phys = crate::boot::get_fb_info().and_then(|(phys, _, _, _, source)| {
        if source == crate::boot::FbSource::Uefi { Some(phys) } else { None }
    });

    let mut primary_index = 0;
    if let Some(target_phys) = gop_phys {
        for (i, gpu) in gpus.iter().enumerate() {
            let bar1 = unsafe { get_bar(gpu, 1) };
            if bar1 != 0 && bar1 == target_phys {
                primary_index = i;
                serial::serial_print("[NVIDIA]   Primary display identified on GPU ");
                serial::serial_print_dec(i as u64);
                serial::serial_print("\n");
                break;
            }
        }
    }

    for (index, gpu) in gpus.iter().enumerate() {
        let gpu_info = NvidiaGpuInfo::from_pci_device(*gpu);
        
        serial::serial_print("[NVIDIA] GPU ");
        serial::serial_print_dec(index as u64);
        serial::serial_print(": ");
        serial::serial_print(gpu_info.name);
        serial::serial_print("\n");
        
        serial::serial_print("[NVIDIA]   Device ID: 0x");
        serial::serial_print_hex(gpu.device_id as u64);
        serial::serial_print("\n");
        
        serial::serial_print("[NVIDIA]   Architecture (PCI ID): ");
        match gpu_info.architecture {
            NvidiaArchitecture::Blackwell   => serial::serial_print("Blackwell"),
            NvidiaArchitecture::AdaLovelace => serial::serial_print("Ada Lovelace"),
            NvidiaArchitecture::Ampere      => serial::serial_print("Ampere"),
            NvidiaArchitecture::Turing      => serial::serial_print("Turing"),
            NvidiaArchitecture::Hopper      => serial::serial_print("Hopper"),
            NvidiaArchitecture::Unknown     => serial::serial_print("Unknown"),
        }
        serial::serial_print("\n");
        
        if gpu_info.memory_size_mb > 0 {
            serial::serial_print("[NVIDIA]   Memory (PCI ID table): ");
            serial::serial_print_dec(gpu_info.memory_size_mb as u64);
            serial::serial_print(" MB\n");
        }
        
        if gpu_info.cuda_cores > 0 {
            serial::serial_print("[NVIDIA]   CUDA Cores: ");
            serial::serial_print_dec(gpu_info.cuda_cores as u64);
            serial::serial_print("\n");
            
            serial::serial_print("[NVIDIA]   SM Count: ");
            serial::serial_print_dec(gpu_info.sm_count as u64);
            serial::serial_print("\n");
            
            if gpu_info.rt_cores > 0 {
                serial::serial_print("[NVIDIA]   RT Cores: ");
                serial::serial_print_dec(gpu_info.rt_cores as u64);
                serial::serial_print("\n");
            }
            
            if gpu_info.tensor_cores > 0 {
                serial::serial_print("[NVIDIA]   Tensor Cores: ");
                serial::serial_print_dec(gpu_info.tensor_cores as u64);
                serial::serial_print("\n");
            }
        }
        
        serial::serial_print("[NVIDIA]   BAR0 (PCI): 0x");
        serial::serial_print_hex(gpu.bar0 as u64);
        serial::serial_print("\n");

        // Report advanced capabilities
        serial::serial_print("[NVIDIA]   Advanced Features:\n");
        serial::serial_print("[NVIDIA]     ✓ CUDA Runtime\n");
        if gpu_info.rt_cores > 0 {
            serial::serial_print("[NVIDIA]     ✓ Ray Tracing (RT Cores)\n");
        }
        serial::serial_print("[NVIDIA]     ✓ DisplayPort/HDMI Output\n");
        serial::serial_print("[NVIDIA]     ✓ Power Management\n");
        
        let arch_is_turing = matches!(gpu_info.architecture, NvidiaArchitecture::Turing);
        let encoder_caps = video::EncoderCapabilities::detect(arch_is_turing, gpu_info.sm_count);
        let decoder_caps = video::DecoderCapabilities::detect(arch_is_turing, gpu_info.sm_count);
        
        serial::serial_print("[NVIDIA]     ✓ Video Encode (NVENC): ");
        serial::serial_print_dec(encoder_caps.supported_codecs.len() as u64);
        serial::serial_print(" codecs\n");
        
        serial::serial_print("[NVIDIA]     ✓ Video Decode (NVDEC): ");
        serial::serial_print_dec(decoder_caps.supported_codecs.len() as u64);
        serial::serial_print(" codecs\n");
        
        if gpu_info.is_open_source_supported() {
            serial::serial_print("[NVIDIA]   ✓ Supported by open-gpu-kernel-modules\n");
        } else {
            serial::serial_print("[NVIDIA]   ⚠ Not supported by open-gpu-kernel-modules\n");
        }
        
        // --- Phase 1: BAR0 mapping ---
        // Turing and newer use 32 MB BAR0 (from open-gpu-kernel-modules default).
        // Legacy GPUs (pre-Turing, not supported here) use 16 MB.
        let bar0_phys = unsafe { get_bar(gpu, 0) };
        
        if bar0_phys == 0 {
            serial::serial_print("[NVIDIA]   ⚠ BAR0 is unassigned (0). Skipping GPU to prevent triple-fault.\n");
            continue;
        }

        // Enable the PCI device (I/O + Memory + Bus Master)
        unsafe {
            crate::pci::enable_device(&gpu, true);
            // Disable legacy INTx interrupts to prevent IRQ storms
            let mut command = crate::pci::pci_config_read_u16(gpu.bus, gpu.device, gpu.function, 0x04);
            command |= 0x0400; // PCI_COMMAND_INTERRUPT_DISABLE
            crate::pci::pci_config_write_u16(gpu.bus, gpu.device, gpu.function, 0x04, command);
        }
        serial::serial_print("[NVIDIA]   Device enabled (I/O, Memory, Bus Master, INTx Disabled)\n");

        let bar0_size = 32 * 1024 * 1024; // 32 MB for Turing+ (open-gpu-kernel-modules standard)
        
        serial::serial_print("[NVIDIA]   Mapping BAR0 (Phys: 0x");
        serial::serial_print_hex(bar0_phys);
        serial::serial_print(", 32 MB)...\n");

        let bar0_virt = map_mmio_range(bar0_phys, bar0_size);
        
        serial::serial_print("[NVIDIA]   Mapped BAR0 to Virt: 0x");
        serial::serial_print_hex(bar0_virt);
        serial::serial_print("\n");

        // --- Phase 2: Hardware identity check via PMC_BOOT_0 ---
        // PMC_BOOT_0 contains the chip ID embedded in bits [31:20].
        // We cross-check the PCI-ID-derived architecture against the register value.
        let boot_0 = unsafe {
            core::ptr::read_volatile((bar0_virt + NV_PMC_BOOT_0 as u64) as *const u32)
        };
        serial::serial_print("[NVIDIA]   PMC_BOOT_0: 0x");
        serial::serial_print_hex(boot_0 as u64);
        serial::serial_print("\n");

        if boot_0 == 0 || boot_0 == 0xFFFF_FFFF {
            serial::serial_print("[NVIDIA]   ⚠ BAR0 not accessible (PMC_BOOT_0=0x");
            serial::serial_print_hex(boot_0 as u64);
            serial::serial_print("). Skipping this GPU.\n");
            continue;
        }

        serial::serial_print("[NVIDIA]   ✓ BAR0 accessible (GPU ID: 0x");
        serial::serial_print_hex(boot_0 as u64);
        serial::serial_print(")\n");

        // Cross-validate architecture from hardware register
        let hw_arch = arch_from_pmc_boot0(boot_0);
        serial::serial_print("[NVIDIA]   Architecture (PMC_BOOT_0): ");
        match hw_arch {
            NvidiaArchitecture::Blackwell   => serial::serial_print("Blackwell"),
            NvidiaArchitecture::AdaLovelace => serial::serial_print("Ada Lovelace"),
            NvidiaArchitecture::Ampere      => serial::serial_print("Ampere"),
            NvidiaArchitecture::Turing      => serial::serial_print("Turing"),
            NvidiaArchitecture::Hopper      => serial::serial_print("Hopper"),
            NvidiaArchitecture::Unknown     => serial::serial_print("Unknown"),
        }
        serial::serial_print("\n");

        if hw_arch != gpu_info.architecture && hw_arch != NvidiaArchitecture::Unknown {
            serial::serial_print("[NVIDIA]   ⚠ Architecture mismatch: PCI ID says one arch, ");
            serial::serial_print("PMC_BOOT_0 chip_id says another. Using PMC_BOOT_0.\n");
        }

        // --- Phase 2b: BAR1 (linear VRAM aperture) as display fallback ---
        // BAR1 is the CPU-visible linear aperture over VRAM on Turing+ GPUs.
        {
            let bar1_phys = unsafe { get_bar(gpu, 1) };
            serial::serial_print("[NVIDIA]   BAR1 (VRAM aperture) phys: 0x");
            serial::serial_print_hex(bar1_phys);
            serial::serial_print("\n");
            
            if bar1_phys != 0 && index == primary_index {
                let mut width = 1920u32;
                let mut height = 1080u32;
                let mut pitch = 1920u32 * 4;

                let mut fb_phys = bar1_phys;
                
                // Inherit from GOP if possible
                if let Some((phys, w, h, p, source)) = crate::boot::get_fb_info() {
                    if source == crate::boot::FbSource::Uefi {
                        width = w;
                        height = h;
                        pitch = p;
                        fb_phys = phys;
                        serial::serial_print("[NVIDIA]   Inheriting native resolution (GOP): ");
                        serial::serial_print_dec(width as u64);
                        serial::serial_print("x");
                        serial::serial_print_dec(height as u64);
                        serial::serial_print(" @ 0x");
                        serial::serial_print_hex(fb_phys);
                        serial::serial_print(" (Pitch: ");
                        serial::serial_print_dec(pitch as u64);
                        serial::serial_print(")\n");
                    }
                } else {
                    // Only align pitch if we are NOT inheriting a valid hardware state
                    let mut safe_pitch = pitch.max(width * 4);
                    safe_pitch = (safe_pitch + 255) & !255;
                    if safe_pitch != pitch {
                        serial::serial_print("[NVIDIA]   Adjusted pitch for alignment: ");
                        serial::serial_print_dec(pitch as u64);
                        serial::serial_print(" -> ");
                        serial::serial_print_dec(safe_pitch as u64);
                        serial::serial_print("\n");
                        pitch = safe_pitch;
                    }
                }

                let bar1_size = unsafe { crate::pci::get_bar_size(&gpu, 1) };
                
                let mut guard = NVIDIA_FB_INFO.lock();
                *guard = Some(NvidiaFbInfo { 
                    phys: fb_phys, 
                    bar1_phys, 
                    bar1_size, 
                    width, 
                    height, 
                    pitch 
                });
                
                // Initialize VRAM allocator for BAR1
                let alloc_size = if bar1_size == 0 { 32 * 1024 * 1024 } else { bar1_size.min(256 * 1024 * 1024) };
                
                let mut vram_guard = VRAM_ALLOCATOR.lock();
                *vram_guard = Some(NvidiaVramAllocator::new(bar1_phys, alloc_size));
                
                serial::serial_print("[NVIDIA]   ✓ Primary Display initialized (GPU ");
                serial::serial_print_dec(index as u64);
                serial::serial_print(")\n");

                // Map the full BAR1 range in kernel virtual space
                let mut map_guard = NVIDIA_BAR1_MAPPED_SIZE.lock();
                if map_guard.is_none() {
                    let vaddr = map_framebuffer_kernel(bar1_phys, alloc_size as usize);
                    if vaddr != 0 {
                        NVIDIA_BAR1_KERNEL_VADDR.store(vaddr, AtomicOrdering::Relaxed);
                    }
                    *map_guard = Some(alloc_size as usize);
                }
            }
        }

        // Enable MSI (Message Signaled Interrupts)
        let pci_dev_copy = *gpu; 
        if unsafe { crate::pci::pci_enable_msi(&pci_dev_copy, crate::interrupts::GPU_INTERRUPT_VECTOR, 0) } {
            serial::serial_print("[NVIDIA]   MSI enabled (Vector 0x40, CPU 0)\n");
        } else {
            serial::serial_print("[NVIDIA]   ⚠ MSI not supported or failed to enable (polling mode active)\n");
        }

        // Thermal Monitoring
        if let Some(temp) = read_temperature(bar0_virt) {
            serial::serial_print("[NVIDIA]   GPU Temperature: ");
            serial::serial_print_dec(temp as u64);
            serial::serial_print("°C\n");
            if temp > 85 {
                serial::serial_print("[NVIDIA]   ⚠ WARNING: High temperature detected!\n");
            }
        }

        // --- Phase 3: VRAM size from hardware register ---
        // NV_PFB_CSTATUS bits [14:0] = VRAM size in MB (only valid after GPU init,
        // but may reflect VBIOS pre-programmed value on warm boot).
        let hw_vram_mb = read_vram_size_mb(bar0_virt);
        if hw_vram_mb > 0 {
            serial::serial_print("[NVIDIA]   VRAM (NV_PFB_CSTATUS): ");
            serial::serial_print_dec(hw_vram_mb as u64);
            serial::serial_print(" MB\n");
        } else {
            serial::serial_print("[NVIDIA]   VRAM: not yet readable (NV_PFB_CSTATUS=0)\n");
        }

        // --- Phase 4: Temperature reading ---
        // Only attempt if THERM is powered (register not 0 / 0xFFFF_FFFF).
        match read_temperature(bar0_virt) {
            Some(temp) => {
                serial::serial_print("[NVIDIA]   Temperature: ");
                serial::serial_print_dec(temp as u64);
                serial::serial_print(" deg C\n");
            }
            None => {
                serial::serial_print("[NVIDIA]   Temperature: THERM not initialized\n");
            }
        }

        // --- Phase 5: PMC engine enable ---
        // Before GSP boot, enable all standard GPU engine subsystems.
        // This follows the open-gpu-kernel-modules _pmc_enable sequence.
        unsafe {
            let current = core::ptr::read_volatile(
                (bar0_virt + NV_PMC_ENABLE as u64) as *const u32
            );
            serial::serial_print("[NVIDIA]   PMC_ENABLE (before): 0x");
            serial::serial_print_hex(current as u64);
            serial::serial_print("\n");
            core::ptr::write_volatile(
                (bar0_virt + NV_PMC_ENABLE as u64) as *mut u32,
                current | NV_PMC_ENABLE_DEFAULT,
            );
            // Readback confirms write was accepted
            let confirmed = core::ptr::read_volatile(
                (bar0_virt + NV_PMC_ENABLE as u64) as *const u32
            );
            serial::serial_print("[NVIDIA]   PMC_ENABLE (after):  0x");
            serial::serial_print_hex(confirmed as u64);
            serial::serial_print("\n");
        }

        // --- Phase 6: OpenGL context initialization ---
        // PGRAPH (bit 13) is already active via NV_PMC_ENABLE_DEFAULT.
        // Init the kernel GL context and reserve a primary render surface.
        let vram_for_gl = if hw_vram_mb > 0 { hw_vram_mb } else { gpu_info.memory_size_mb };
        let current_bar1 = unsafe { get_bar(gpu, 1) };
        
        let (w, h) = {
            let guard = NVIDIA_FB_INFO.lock();
            if let Some(info) = *guard {
                (info.width, info.height)
            } else {
                (1920, 1080)
            }
        };
        opengl::init_all_gpus(bar0_virt, current_bar1, vram_for_gl, w, h);

        // --- Phase 7: GSP firmware load and Falcon boot sequence ---
        let fw_path = "/lib/firmware/gsp.bin";
        match GspLoader::load_firmware(fw_path) {
            Ok(fw) => {
                serial::serial_print("[NVIDIA]   ✓ GSP Firmware loaded (");
                serial::serial_print_dec(fw.size as u64);
                serial::serial_print(" bytes at phys 0x");
                serial::serial_print_hex(fw.phys_base);
                serial::serial_print(")\n");
                
                serial::serial_print("[NVIDIA]   Booting GSP Falcon (Nova/open-gpu-kernel-modules protocol)...\n");
                
                unsafe {
                    // Step 6a: Configure DMA transfer base register (DMATRFBASE)
                    // Set to firmware physical address >> 8 as per Falcon DMA spec.
                    let fw_base_shifted = (fw.phys_base >> 8) as u32;
                    core::ptr::write_volatile(
                        (bar0_virt + NV_GSP_DMATRFBASE as u64) as *mut u32,
                        fw_base_shifted,
                    );

                    // Step 6b: Clear both mailboxes for clean handshake
                    core::ptr::write_volatile(
                        (bar0_virt + NV_GSP_MAILBOX0 as u64) as *mut u32, 0,
                    );
                    core::ptr::write_volatile(
                        (bar0_virt + NV_GSP_MAILBOX1 as u64) as *mut u32, 0,
                    );

                    // Step 6c: Release GSP Falcon from reset via CPUCTL (STARTCPU bit).
                    // This is the canonical boot kick from open-gpu-kernel-modules
                    // kgspBootstrapRiscvOSDma_TU102 (src/nvidia/kernel/gpu/gsp/kernel_gsp.c).
                    core::ptr::write_volatile(
                        (bar0_virt + NV_GSP_CPUCTL as u64) as *mut u32,
                        NV_PFALCON_FALCON_CPUCTL_STARTCPU,
                    );
                    serial::serial_print("[NVIDIA]   GSP Falcon STARTCPU issued. Awaiting MAILBOX0 handshake");

                    // Step 6d: Initialize RPC Client
                    let mut rpc = RpcClient::new(GPU_RPC_PHYS_BASE);

                    // Step 6e: Poll MAILBOX0 for GSP-RM ready signature.
                    // From open-gpu-kernel-modules: GSP writes a magic value when ready.
                    // Timeout: 1 second using kernel ticks.
                    let mut success = false;
                    let timeout_tick = crate::interrupts::ticks() + 1000;

                    while crate::interrupts::ticks() < timeout_tick {
                        let mb0 = core::ptr::read_volatile(
                            (bar0_virt + NV_GSP_MAILBOX0 as u64) as *const u32,
                        );
                        if mb0 == GSP_MAILBOX0_READY_MAGIC_1
                            || mb0 == GSP_MAILBOX0_READY_MAGIC_2
                            || mb0 == GSP_MAILBOX0_READY_MAGIC_3
                        {
                            success = true;
                            break;
                        }
                        if crate::interrupts::ticks() % 200 == 0 {
                            serial::serial_print(".");
                        }
                        crate::cpu::pause();
                    }

                    if success {
                        serial::serial_print(" ✓ GSP READY\n");

                        // Step 6f: GSP Capability Discovery via RPC
                        serial::serial_print("[NVIDIA]   Sending GSP RPC: ControlGetCaps\n");
                        match rpc.send_command(GspOpcode::ControlGetCaps, &[]) {
                            Ok(seq) => {
                                serial::serial_print("[NVIDIA]     RPC sent (Seq: ");
                                serial::serial_print_dec(seq as u64);
                                serial::serial_print("). Waiting for response...");
                                
                                let mut found = false;
                                for _ in 0..1000 {
                                    if let Some(msg) = rpc.poll_response() {
                                        if msg.header.seq_num == seq {
                                            serial::serial_print(" ✓ Response (Status: ");
                                            serial::serial_print_dec(msg.header.status as u64);
                                            serial::serial_print(")\n");
                                            found = true;
                                            break;
                                        }
                                    }
                                    for _ in 0..100_000 { crate::cpu::pause(); }
                                }
                                if !found {
                                    serial::serial_print(" ⚠ RPC response timeout\n");
                                }
                            }
                            Err(e) => {
                                serial::serial_print("[NVIDIA]   ⚠ RPC Failed: ");
                                serial::serial_print_dec(e as u64);
                                serial::serial_print("\n");
                            }
                        }

                        /* 
                        // Step 6g: Disable DisplaySetup temporarily as the set_mode helper is a stub.
                        // Calling GSP DisplaySetup without a proper set_mode following it can 
                        // reset the VBIOS/GOP state and cause screen corruption or "double vision".
                        if let Ok(seq) = rpc.send_command(GspOpcode::DisplaySetup, &[]) {
                            // ... (deshabilitado hasta v0.2.2)
                        }
                        */
                        serial::serial_print("[NVIDIA]   GSP initialization complete (Preserving VBIOS Display State)\n");
                    } else {
                        let mb0 = core::ptr::read_volatile(
                            (bar0_virt + NV_GSP_MAILBOX0 as u64) as *const u32,
                        );
                        let mb1 = core::ptr::read_volatile(
                            (bar0_virt + NV_GSP_MAILBOX1 as u64) as *const u32,
                        );
                        serial::serial_print(" ⚠ GSP Timeout (MAILBOX0=0x");
                        serial::serial_print_hex(mb0 as u64);
                        serial::serial_print(", MAILBOX1=0x");
                        serial::serial_print_hex(mb1 as u64);
                        serial::serial_print(")\n");
                        serial::serial_print("[NVIDIA]   ℹ GSP timeout is expected when gsp.bin is\n");
                        serial::serial_print("[NVIDIA]     not found or invalid for this GPU model.\n");
                    }
                }
            }
            Err(e) => {
                serial::serial_print("[NVIDIA]   ⚠ Firmware load failed: ");
                serial::serial_print(e);
                serial::serial_print("\n");
                serial::serial_print("[NVIDIA]   ℹ Place NVIDIA GSP firmware at /lib/firmware/gsp.bin\n");
                serial::serial_print("[NVIDIA]   ℹ (from open-gpu-kernel-modules or linux-firmware package)\n");
            }
        }
    }
    
    // Register with DRM subsystem exactly once if any GPU was detected
    if find_nvidia_gpus().len() > 0 {
        crate::drm::register_driver(alloc::sync::Arc::new(NvidiaDrmDriver));
    }
    
    serial::serial_print("[NVIDIA] Initialization complete\n");
}

/// Get list of detected NVIDIA GPUs
pub fn get_nvidia_gpus() -> Vec<NvidiaGpuInfo> {
    find_nvidia_gpus()
        .iter()
        .map(|pci_dev| NvidiaGpuInfo::from_pci_device(*pci_dev))
        .collect()
}

/// Refresh metrics for all active NVIDIA GPUs and update the AI core.
pub fn update_all_gpu_vitals() {
    // Update VRAM stats for dashboard (sum of all detected GPUs + used from our BAR1 allocator).
    let gpu_infos = get_nvidia_gpus();
    let total_vram_bytes_all: u64 = gpu_infos
        .iter()
        .map(|g| (g.memory_size_mb as u64).saturating_mul(1024 * 1024))
        .sum();

    let used_vram_bytes_primary = {
        let mut allocator = VRAM_ALLOCATOR.lock();
        allocator
            .as_ref()
            .map(|a| a.used_bytes())
            .unwrap_or(0)
    };

    crate::ai_core::set_gpu_vram_stats(total_vram_bytes_all, used_vram_bytes_primary);

    let gpus = find_nvidia_gpus();
    for (i, pci_dev) in gpus.iter().enumerate().take(4) {
        // We need BAR0 to read registers (temperature/engine heuristic).
        // Here we rely on the HHDM phys->virt mapping.
        let mut bar0 = 0u64;
        let pci_bar0 = pci_dev.bar0;
        if pci_bar0 != 0 {
            bar0 = crate::memory::phys_to_virt(pci_bar0 & !0xF);
        }

        if bar0 != 0 {
            if let Some(temp) = read_temperature(bar0) {
                // For load, we don't have a simple register yet, but we can 
                // check if the engine is busy via PMC ENABLE bits as a heuristic.
                let raw_pmc = unsafe {
                    core::ptr::read_volatile((bar0 + 0x200) as *const u32)
                };
                let load = if raw_pmc & 0x1 != 0 { 45 } else { 2 }; // Heuristic/Mock for now

                // Report temperature in Tenths of Celsius (450 = 45.0 C)
                let mem_used = if i == 0 { used_vram_bytes_primary } else { 0 };
                crate::ai_core::update_gpu_metrics_by_bus(pci_dev.bus, load, mem_used, (temp as u32) * 10);
            }
        }
    }
}


pub use sidewind_nvidia::features::*;