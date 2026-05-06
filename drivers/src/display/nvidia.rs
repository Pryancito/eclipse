use alloc::string::String;
use alloc::vec::Vec;

use crate::prelude::{ColorFormat, DisplayInfo, FrameBuffer};
use crate::scheme::{DisplayScheme, DrmScheme, Scheme};
use crate::DeviceResult;

pub struct NvidiaGpu {
    name: String,
    info: DisplayInfo,
    // BARs
    _bar0: usize, // Registers
    _bar1: usize, // Framebuffer
}

/// Try to read the active display resolution from NVIDIA GPU registers.
///
/// Probes HEAD 0 raster-size registers for both NV50+ and NV40 GPU families.
/// Returns `(width, height)` if a plausible value is found, `None` otherwise.
///
/// # Safety
/// `bar0` must be a valid virtual address for the mapped NVIDIA BAR0 region
/// (at least 4 MiB must be mapped).
unsafe fn probe_resolution_from_bar0(bar0: usize) -> Option<(u32, u32)> {
    // NV50+ (GeForce 8xxx / 2006 onwards): HEAD0 raster size at BAR0 + 0x610798
    // bits 31:16 = height, bits 15:0 = width
    let reg = core::ptr::read_volatile((bar0 + 0x61_0798) as *const u32);
    let (w, h) = (reg & 0xFFFF, reg >> 16);
    if w > 0 && h > 0 && w <= 16384 && h <= 16384 {
        return Some((w, h));
    }

    // NV40 (GeForce 6/7 series): PCRTC HEAD0 at BAR0 + 0x600000, offset 0x2C
    // bits 31:16 = height, bits 15:0 = width
    let reg = core::ptr::read_volatile((bar0 + 0x60_002C) as *const u32);
    let (w, h) = (reg & 0xFFFF, reg >> 16);
    if w > 0 && h > 0 && w <= 16384 && h <= 16384 {
        return Some((w, h));
    }

    None
}

impl NvidiaGpu {
    /// Create a new NvidiaGpu.
    ///
    /// * `name`    – unique device name (e.g. `"nvidia-gpu-0:2.0"`)
    /// * `bar0`    – virtual address of NVIDIA register space (BAR0, ≥4 MiB mapped)
    /// * `fb`      – virtual address of the linear framebuffer (BAR1/2/3)
    /// * `fb_size` – size of the framebuffer in bytes
    /// * `width`, `height` – fallback resolution (used when register probing fails)
    pub fn new(
        name: String,
        bar0: usize,
        fb: usize,
        fb_size: usize,
        width: u32,
        height: u32,
    ) -> DeviceResult<Self> {
        // Prefer the resolution currently programmed into the GPU.
        let (w, h) = unsafe { probe_resolution_from_bar0(bar0) }
            .unwrap_or((width, height));

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
            _bar0: bar0,
            _bar1: fb,
        })
    }
}

impl Scheme for NvidiaGpu {
    fn name(&self) -> &str {
        &self.name
    }

    fn handle_irq(&self, _irq_num: usize) {
        // Handle GSP interrupts
    }
}

impl DisplayScheme for NvidiaGpu {
    fn info(&self) -> DisplayInfo {
        self.info
    }

    fn fb(&self) -> FrameBuffer {
        unsafe {
            FrameBuffer::from_raw_parts_mut(self.info.fb_base_vaddr as *mut u8, self.info.fb_size)
        }
    }
}

use crate::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane, GemHandle};

impl DrmScheme for NvidiaGpu {
    fn get_caps(&self) -> DrmCaps {
        DrmCaps {
            has_3d: true,
            has_cursor: true,
            max_width: self.info.width,
            max_height: self.info.height,
        }
    }

    fn import_buffer(&self, _handle: GemHandle) -> bool {
        true
    }

    fn free_buffer(&self, _handle: GemHandle) {}

    fn create_fb(&self, handle_id: u32, _width: u32, _height: u32, _pitch: u32) -> Option<u32> {
        Some(handle_id)
    }

    fn page_flip(&self, _fb_id: u32) -> bool {
        true
    }

    fn set_cursor(&self, _crtc_id: u32, _x: i32, _y: i32, _handle: u32, _flags: u32) -> bool {
        true
    }

    fn wait_vblank(&self, _crtc_id: u32) -> bool {
        true
    }

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

    fn get_planes(&self) -> Vec<u32> {
        alloc::vec![3001]
    }

    fn set_plane(&self, _plane_id: u32, _crtc_id: u32, _fb_id: u32, _x: i32, _y: i32, _w: u32, _h: u32, _src_x: u32, _src_y: u32, _src_w: u32, _src_h: u32) -> bool {
        true
    }
}
