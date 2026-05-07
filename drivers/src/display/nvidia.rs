use alloc::string::String;
use alloc::vec::Vec;

use crate::prelude::{ColorFormat, DisplayInfo, FrameBuffer};
use crate::scheme::{DisplayScheme, DrmScheme, Scheme};
use crate::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane, GemHandle};
use crate::DeviceResult;
use lock::Mutex;

// --- Registers and Constants (aligned with Nova / open-gpu-kernel-modules) ---
mod regs {
    pub const NV_PMC_BOOT_0: u32 = 0x0000_0000;
    pub const PMC_BOOT0_CHIP_ID_SHIFT: u32 = 20;
    pub const PMC_BOOT0_CHIP_ID_MASK: u32 = 0xFFF;
    
    pub const PMC_BOOT0_CHIPID_TURING_MIN: u32 = 0x160;
    pub const PMC_BOOT0_CHIPID_TURING_MAX: u32 = 0x16F;
    pub const PMC_BOOT0_CHIPID_AMPERE_MIN: u32 = 0x170;
    pub const PMC_BOOT0_CHIPID_AMPERE_MAX: u32 = 0x17F;
    pub const PMC_BOOT0_CHIPID_ADA_MIN: u32 = 0x190;
    pub const PMC_BOOT0_CHIPID_ADA_MAX: u32 = 0x19F;
    pub const PMC_BOOT0_CHIPID_HOPPER_MIN: u32 = 0x1B0;
    pub const PMC_BOOT0_CHIPID_HOPPER_MAX: u32 = 0x1BF;
    pub const PMC_BOOT0_CHIPID_BLACKWELL_MIN: u32 = 0x200;

    pub const NV_PFB_CSTATUS: u32 = 0x0010_020C;
    pub const NV_PFB_CSTATUS_MEM_SIZE_MASK: u32 = 0x7FFF;

    pub const NV_THERM_TEMP: u32 = 0x0002_0400;
    pub const NV_THERM_TEMP_VALUE_MASK: u32 = 0x1FF;
    pub const NV_THERM_TEMP_VALUE_SIGN_BIT: u32 = 0x100;

    // Display resolution registers (legacy/fallback)
    pub const NV50_HEAD0_RASTER_SIZE: u32 = 0x610798;
    pub const NV40_PCRTC_HEAD0_SIZE: u32 = 0x60002C;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvidiaArchitecture {
    Unknown,
    Turing,      // RTX 20 series
    Ampere,      // RTX 30 series
    AdaLovelace, // RTX 40 series
    Hopper,      // H100/H200
    Blackwell,   // RTX 50 series
}

pub struct NvidiaGpu {
    name: String,
    info: DisplayInfo,
    architecture: NvidiaArchitecture,
    gpu_model: &'static str,
    vram_size_mb: u32,
    _bar0: usize,
    _bar1: usize,
    vram_allocator: Mutex<Option<NvidiaVramAllocator>>,
}

/// Simple bitmap-based VRAM allocator for BAR1 aperture (4KB page granularity)
struct NvidiaVramAllocator {
    base_phys: u64,
    total_size: u64,
    bitmap: Vec<u64>,
}

impl NvidiaVramAllocator {
    fn new(base_phys: u64, total_size: u64) -> Self {
        let num_pages = (total_size / 4096) as usize;
        let num_u64s = (num_pages + 63) / 64;
        Self {
            base_phys,
            total_size,
            bitmap: alloc::vec![0; num_u64s],
        }
    }

    fn _alloc(&mut self, size: usize, align: usize) -> Option<u64> {
        let num_pages = (size + 4095) / 4096;
        let align_pages = (align.max(4096) / 4096).max(1);
        let total_bits = (self.total_size / 4096) as usize;
        
        let mut count = 0;
        let mut start_bit = 0;
        
        for bit in 0..total_bits {
            let uidx = bit / 64;
            let ubit = bit % 64;
            let is_free = (self.bitmap[uidx] & (1 << ubit)) == 0;
            
            if is_free {
                if count == 0 {
                    if bit % align_pages != 0 { continue; }
                    start_bit = bit;
                }
                count += 1;
                if count >= num_pages {
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
}

impl NvidiaGpu {
    fn pitch_pixels(&self) -> usize {
        let width = self.info.width as usize;
        let height = self.info.height as usize;
        if width == 0 || height == 0 {
            return width;
        }

        // Accept moderately padded scanlines (for example 2048-wide alignment on
        // a 1920-wide mode) while rejecting BAR apertures that are far larger
        // than the visible framebuffer and would produce a bogus inferred pitch.
        const MAX_PITCH_PADDING_PIXELS: usize = 4096;
        let bytes_per_pixel = self.info.format.bytes() as usize;
        let visible_size = width
            .saturating_mul(height)
            .saturating_mul(bytes_per_pixel);

        if self.info.fb_size >= visible_size {
            let inferred = self.info.fb_size / height / bytes_per_pixel;
            if inferred >= width && inferred <= width + MAX_PITCH_PADDING_PIXELS {
                return inferred;
            }
        }

        width
    }

    pub fn new(
        name: String,
        bar0: usize,
        fb: usize,
        fb_size: usize,
        width: u32,
        height: u32,
    ) -> DeviceResult<Self> {
        // 1. Identify Architecture
        let boot0 = unsafe { core::ptr::read_volatile((bar0 + regs::NV_PMC_BOOT_0 as usize) as *const u32) };
        let arch = arch_from_pmc_boot0(boot0);
        
        // 2. Identify Model (simplified matching)
        let gpu_model = match arch {
            NvidiaArchitecture::Turing => "NVIDIA Turing GPU",
            NvidiaArchitecture::Ampere => "NVIDIA Ampere GPU",
            NvidiaArchitecture::AdaLovelace => "NVIDIA Ada Lovelace GPU",
            NvidiaArchitecture::Hopper => "NVIDIA Hopper GPU",
            NvidiaArchitecture::Blackwell => "NVIDIA Blackwell GPU",
            _ => "Unknown NVIDIA GPU",
        };

        // 3. Read VRAM Size
        let vram_size_mb = unsafe {
            core::ptr::read_volatile((bar0 + regs::NV_PFB_CSTATUS as usize) as *const u32)
        } & regs::NV_PFB_CSTATUS_MEM_SIZE_MASK;

        // 4. Read Temperature
        let temperature = read_temperature(bar0);

        // 5. Resolution probing (prefer programmed resolution)
        let (w, h) = unsafe { probe_resolution_from_bar0(bar0) }.unwrap_or((width, height));

        log::warn!("[NVIDIA] Detected {} ({:?}), VRAM: {} MB, Temp: {:?}°C", 
            gpu_model, arch, vram_size_mb, temperature);

        let info = DisplayInfo {
            width: w,
            height: h,
            format: ColorFormat::ARGB8888,
            fb_base_vaddr: fb,
            fb_size,
        };

        Ok(Self {
            name,
            info,
            architecture: arch,
            gpu_model,
            vram_size_mb,
            _bar0: bar0,
            _bar1: fb,
            vram_allocator: Mutex::new(Some(NvidiaVramAllocator::new(fb as u64, fb_size as u64))),
        })
    }

    pub fn architecture(&self) -> NvidiaArchitecture { self.architecture }
    pub fn model(&self) -> &'static str { self.gpu_model }
    pub fn vram_size_mb(&self) -> u32 { self.vram_size_mb }
    pub fn temperature(&self) -> Option<i32> {
        read_temperature(self._bar0)
    }

    pub fn fill_rect(&self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        let width = self.info.width;
        let height = self.info.height;
        let x = x.min(width);
        let y = y.min(height);
        let w = w.min(width.saturating_sub(x));
        let h = h.min(height.saturating_sub(y));
        if w == 0 || h == 0 { return; }

        let ptr = self.info.fb_base_vaddr as *mut u32;
        let pitch_u32 = self.pitch_pixels();

        for py in 0..h {
            let row_start = (y + py) as usize * pitch_u32 + (x as usize);
            for px in 0..w {
                unsafe {
                    core::ptr::write_volatile(ptr.add(row_start + px as usize), color);
                }
            }
        }
    }

    pub fn blit_rect(&self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, w: u32, h: u32) {
        let width = self.info.width;
        let height = self.info.height;
        let w = w.min(width.saturating_sub(src_x)).min(width.saturating_sub(dst_x));
        let h = h.min(height.saturating_sub(src_y)).min(height.saturating_sub(dst_y));
        if w == 0 || h == 0 { return; }

        let ptr = self.info.fb_base_vaddr as *mut u32;
        let pitch_u32 = self.pitch_pixels();

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
    }
}

fn arch_from_pmc_boot0(boot0: u32) -> NvidiaArchitecture {
    let chip_id = (boot0 >> regs::PMC_BOOT0_CHIP_ID_SHIFT) & regs::PMC_BOOT0_CHIP_ID_MASK;
    if chip_id >= regs::PMC_BOOT0_CHIPID_BLACKWELL_MIN {
        NvidiaArchitecture::Blackwell
    } else if chip_id >= regs::PMC_BOOT0_CHIPID_HOPPER_MIN && chip_id <= regs::PMC_BOOT0_CHIPID_HOPPER_MAX {
        NvidiaArchitecture::Hopper
    } else if chip_id >= regs::PMC_BOOT0_CHIPID_ADA_MIN && chip_id <= regs::PMC_BOOT0_CHIPID_ADA_MAX {
        NvidiaArchitecture::AdaLovelace
    } else if chip_id >= regs::PMC_BOOT0_CHIPID_AMPERE_MIN && chip_id <= regs::PMC_BOOT0_CHIPID_AMPERE_MAX {
        NvidiaArchitecture::Ampere
    } else if chip_id >= regs::PMC_BOOT0_CHIPID_TURING_MIN && chip_id <= regs::PMC_BOOT0_CHIPID_TURING_MAX {
        NvidiaArchitecture::Turing
    } else {
        NvidiaArchitecture::Unknown
    }
}

fn read_temperature(bar0: usize) -> Option<i32> {
    let raw = unsafe { core::ptr::read_volatile((bar0 + regs::NV_THERM_TEMP as usize) as *const u32) };
    if raw == 0 || raw == 0xFFFF_FFFF { return None; }
    let raw9 = raw & regs::NV_THERM_TEMP_VALUE_MASK;
    if (raw9 & regs::NV_THERM_TEMP_VALUE_SIGN_BIT) != 0 {
        Some((raw9 as i32) - 512)
    } else {
        Some(raw9 as i32)
    }
}

unsafe fn probe_resolution_from_bar0(bar0: usize) -> Option<(u32, u32)> {
    let reg = core::ptr::read_volatile((bar0 + regs::NV50_HEAD0_RASTER_SIZE as usize) as *const u32);
    let (w, h) = (reg & 0xFFFF, reg >> 16);
    if w > 0 && h > 0 && w <= 16384 && h <= 16384 { return Some((w, h)); }

    let reg = core::ptr::read_volatile((bar0 + regs::NV40_PCRTC_HEAD0_SIZE as usize) as *const u32);
    let (w, h) = (reg & 0xFFFF, reg >> 16);
    if w > 0 && h > 0 && w <= 16384 && h <= 16384 { return Some((w, h)); }
    None
}

impl Scheme for NvidiaGpu {
    fn name(&self) -> &str { &self.name }
    fn handle_irq(&self, _irq_num: usize) {}
}

impl DisplayScheme for NvidiaGpu {
    fn info(&self) -> DisplayInfo { self.info }
    fn fb(&self) -> FrameBuffer {
        unsafe { FrameBuffer::from_raw_parts_mut(self.info.fb_base_vaddr as *mut u8, self.info.fb_size) }
    }
}

impl DrmScheme for NvidiaGpu {
    fn get_caps(&self) -> DrmCaps {
        DrmCaps {
            has_3d: true,
            has_cursor: true,
            max_width: self.info.width,
            max_height: self.info.height,
        }
    }

    fn import_buffer(&self, _handle: GemHandle) -> bool { true }

    fn free_buffer(&self, handle: GemHandle) {
        if let Some(ref mut a) = *self.vram_allocator.lock() {
            a.free(handle.phys_addr, handle.size);
        }
    }

    fn create_fb(&self, handle_id: u32, _width: u32, _height: u32, _pitch: u32) -> Option<u32> {
        Some(handle_id)
    }

    fn page_flip(&self, _fb_id: u32) -> bool { true }

    fn set_cursor(&self, _crtc_id: u32, _x: i32, _y: i32, _handle: u32, flags: u32) -> bool {
        const DRM_CURSOR_MOVE: u32 = 0x02;
        if (flags & DRM_CURSOR_MOVE) != 0 {
            // Potential software cursor update here if supported
            return true;
        }
        false
    }

    fn wait_vblank(&self, _crtc_id: u32) -> bool { true }

    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        (Vec::new(), alloc::vec![2001], alloc::vec![1001])
    }

    fn get_connector(&self, id: u32) -> Option<DrmConnector> {
        if id == 1001 {
            Some(DrmConnector { id, connected: true, mm_width: 0, mm_height: 0 })
        } else { None }
    }

    fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
        if id == 2001 {
            Some(DrmCrtc { id, fb_id: 0, x: 0, y: 0 })
        } else { None }
    }

    fn get_plane(&self, id: u32) -> Option<DrmPlane> {
        if id == 3001 {
            Some(DrmPlane { id, crtc_id: 2001, fb_id: 0, possible_crtcs: 1, plane_type: 1 })
        } else { None }
    }

    fn get_planes(&self) -> Vec<u32> { alloc::vec![3001] }

    fn set_plane(&self, _plane_id: u32, _crtc_id: u32, _fb_id: u32, _x: i32, _y: i32, _w: u32, _h: u32, _src_x: u32, _src_y: u32, _src_w: u32, _src_h: u32) -> bool {
        true
    }
    
    fn ioctl(&self, request: u32, arg: usize) -> Result<usize, i32> {
        match request {
            0x10DE0001 => { // Get Temperature
                if let Some(t) = self.temperature() {
                    Ok(t as usize)
                } else {
                    Err(22) // EINVAL
                }
            },
            0x10DE0002 => { // Get VRAM size MB
                Ok(self.vram_size_mb as usize)
            },
            0x10DE0010 => { // Fill Rect (arg is pointer to [u32; 5]: x, y, w, h, color)
                let p = arg as *const u32;
                unsafe {
                    self.fill_rect(*p, *p.add(1), *p.add(2), *p.add(3), *p.add(4));
                }
                Ok(0)
            },
            0x10DE0011 => { // Blit Rect (arg is pointer to [u32; 6]: sx, sy, dx, dy, w, h)
                let p = arg as *const u32;
                unsafe {
                    self.blit_rect(*p, *p.add(1), *p.add(2), *p.add(3), *p.add(4), *p.add(5));
                }
                Ok(0)
            },
            _ => Err(38), // ENOSYS
        }
    }
}
