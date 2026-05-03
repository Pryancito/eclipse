use alloc::vec::Vec;

use crate::prelude::{ColorFormat, DisplayInfo, FrameBuffer};
use crate::scheme::{DisplayScheme, DrmScheme, Scheme};
use crate::DeviceResult;

pub struct NvidiaGpu {
    info: DisplayInfo,
    // BARs
    _bar0: usize, // Registers
    _bar1: usize, // Framebuffer
}

impl NvidiaGpu {
    pub fn new(bar0: usize, bar1: usize, width: u32, height: u32) -> DeviceResult<Self> {
        let info = DisplayInfo {
            width,
            height,
            format: ColorFormat::ARGB8888,
            fb_base_vaddr: bar1,
            fb_size: (width * height * 4) as usize,
        };
        Ok(Self {
            info,
            _bar0: bar0,
            _bar1: bar1,
        })
    }
}

impl Scheme for NvidiaGpu {
    fn name(&self) -> &str {
        "nvidia-gpu"
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
