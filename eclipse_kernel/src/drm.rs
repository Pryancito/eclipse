//! DRM (Direct Rendering Manager) Subsystem for Eclipse OS
//!
//! Provides a unified interface for graphics drivers (NVIDIA, VirtIO, etc.)
//! and handles buffer management (GEM-lite) and mode setting (KMS).

use spin::Mutex;
use alloc::vec::Vec;
use alloc::sync::Arc;

/// DRM Device capabilities
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmCaps {
    pub has_3d: bool,
    pub has_cursor: bool,
    pub max_width: u32,
    pub max_height: u32,
}

/// A DRM Framebuffer object
#[derive(Debug, Clone, Copy)]
pub struct DrmFramebuffer {
    pub id: u32,
    /// GEM handle that backs this framebuffer (equals the hardware resource_id used by drivers)
    pub gem_handle_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub phys_addr: u64,
    pub size: usize,
}

/// GEM (Graphics Execution Manager) handle
#[derive(Debug, Clone, Copy)]
pub struct GemHandle {
    pub id: u32,
    pub size: usize,
    pub phys_addr: u64,
}

/// Límite por buffer GEM: alineado con [`crate::memory::MAX_KERNEL_DMA_HEAP_ALLOC`] (`alloc_dma_buffer`).
pub const MAX_GEM_BUFFER_SIZE: usize = crate::memory::MAX_KERNEL_DMA_HEAP_ALLOC;

/// DRM Connector (output)
#[derive(Debug, Clone, Copy)]
pub struct DrmConnector {
    pub id: u32,
    pub connected: bool,
    pub mm_width: u32,
    pub mm_height: u32,
}

/// DRM CRTC (display controller)
#[derive(Debug, Clone, Copy)]
pub struct DrmCrtc {
    pub id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
}

/// DRM Plane (Overlay, Primary, or Cursor)
#[derive(Debug, Clone, Copy)]
pub struct DrmPlane {
    pub id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub plane_type: u32, // 1=Primary, 2=Cursor, 0=Overlay
}

/// Abstract trait for DRM Driver implementations
pub trait DrmDriver: Send + Sync {
    fn name(&self) -> &'static str;
    fn get_caps(&self) -> DrmCaps;
    
    /// Allocate a buffer (GEM object)
    fn alloc_buffer(&self, size: usize) -> Option<GemHandle>;
    
    /// Free a buffer (GEM object)
    fn free_buffer(&self, handle: GemHandle);
    
    /// Create a framebuffer from a GEM handle
    fn create_fb(&self, handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32>;
    
    /// Page flip: atomically switch to a new framebuffer
    fn page_flip(&self, fb_id: u32) -> bool;
    
    /// Set hardware cursor position and/or image
    fn set_cursor(&self, crtc_id: u32, x: i32, y: i32, handle: u32, flags: u32) -> bool;

    /// Wait for vertical blank on a CRTC
    fn wait_vblank(&self, crtc_id: u32) -> bool;

    /// Get driver resources (fb_ids, crtc_ids, connector_ids)
    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>);

    /// Get connector info
    fn get_connector(&self, id: u32) -> Option<DrmConnector>;

    /// Get crtc info
    fn get_crtc(&self, id: u32) -> Option<DrmCrtc>;

    /// Get plane info
    fn get_plane(&self, id: u32) -> Option<DrmPlane>;

    /// Get all planes supported by the driver
    fn get_planes(&self) -> Vec<u32>;

    /// Set plane properties (mapping a FB to a CRTC at specific coordinates)
    fn set_plane(&self, plane_id: u32, crtc_id: u32, fb_id: u32, x: i32, y: i32, w: u32, h: u32, src_x: u32, src_y: u32, src_w: u32, src_h: u32) -> bool;
}

struct DrmState {
    drivers: Vec<Arc<dyn DrmDriver>>,
    next_handle_id: u32,
    next_fb_id: u32,
    handles: Vec<GemHandle>,
    framebuffers: Vec<DrmFramebuffer>,
}

static DRM_STATE: Mutex<DrmState> = Mutex::new(DrmState {
    drivers: Vec::new(),
    next_handle_id: 1,
    next_fb_id: 1,
    handles: Vec::new(),
    framebuffers: Vec::new(),
});

pub fn init() {
    crate::serial::serial_print("[DRM] Subsystem initialized\n");
    
    // Register fallback driver if GOP is present
    if let Some((phys, width, height, pitch, size, _source)) = crate::boot::get_fb_info() {
        if phys != 0 && phys != 0xDEADBEEF {
            register_driver(Arc::new(SimpleFbDrmDriver {
                phys_addr: phys,
                width,
                height,
                pitch,
                size: size as usize,
            }));
            crate::serial::serial_print("[DRM] Registered fallback SimpleFB driver for GOP\n");
        }
    }
}

/// Simple Framebuffer DRM Driver (Fallback for GOP/EFI)
pub struct SimpleFbDrmDriver {
    pub phys_addr: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub size: usize,
}

impl DrmDriver for SimpleFbDrmDriver {
    fn name(&self) -> &'static str { "simplefb" }
    fn get_caps(&self) -> DrmCaps {
        DrmCaps {
            has_3d: false,
            has_cursor: false,
            max_width: self.width,
            max_height: self.height,
        }
    }
    
    fn alloc_buffer(&self, size: usize) -> Option<GemHandle> {
        unsafe {
            // Use DMA allocator so it is physically contiguous
            let (_ptr, phys) = crate::memory::alloc_dma_buffer(size, 4096)?;
            Some(GemHandle { id: 0, size, phys_addr: phys })
        }
    }
    
    fn free_buffer(&self, handle: GemHandle) {
        unsafe {
            let ptr = crate::memory::phys_to_virt(handle.phys_addr) as *mut u8;
            crate::memory::free_dma_buffer(ptr, handle.size, 4096);
        }
    }
    
    fn create_fb(&self, _handle_id: u32, _width: u32, _height: u32, _pitch: u32) -> Option<u32> {
        Some(1) // Always succeed, metadata handled by drm.rs
    }
    
    fn page_flip(&self, fb_id: u32) -> bool {
        // SimpleFB page flip: software copy from GemHandle to real Framebuffer
        let fb = if let Some(f) = get_fb(fb_id) { f } else { return false; };
        
        // Safety: ensure we don't copy out of bounds
        let copy_size = core::cmp::min(fb.size, self.size);
        let src = crate::memory::phys_to_virt(fb.phys_addr);
        let dst = crate::memory::phys_to_virt(self.phys_addr);
        
        unsafe {
            core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, copy_size);
        }
        true
    }
    
    fn set_cursor(&self, _crtc_id: u32, _x: i32, _y: i32, _handle: u32, _flags: u32) -> bool { false }
    fn wait_vblank(&self, _crtc_id: u32) -> bool { 
        crate::scheduler::yield_cpu();
        true 
    }

    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        (alloc::vec![], alloc::vec![200], alloc::vec![100])
    }

    fn get_connector(&self, id: u32) -> Option<DrmConnector> {
        if id == 100 {
            Some(DrmConnector { id, connected: true, mm_width: 0, mm_height: 0 })
        } else { None }
    }

    fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
        if id == 200 {
            Some(DrmCrtc { id, fb_id: 0, x: 0, y: 0 })
        } else { None }
    }

    fn get_plane(&self, id: u32) -> Option<DrmPlane> {
        if id == 300 {
            Some(DrmPlane {
                id,
                crtc_id: 200,
                fb_id: 0,
                possible_crtcs: 1,
                plane_type: 1, // Primary
            })
        } else { None }
    }

    fn get_planes(&self) -> Vec<u32> { alloc::vec![300] }

    fn set_plane(&self, plane_id: u32, _crtc_id: u32, fb_id: u32, _x: i32, _y: i32, _w: u32, _h: u32, _src_x: u32, _src_y: u32, _src_w: u32, _src_h: u32) -> bool {
        if plane_id == 300 {
            return self.page_flip(fb_id);
        }
        false
    }
}

/// Register a new DRM driver
pub fn register_driver(driver: Arc<dyn DrmDriver>) {
    let mut state = DRM_STATE.lock();
    crate::serial::serial_print("[DRM] Registering driver: ");
    crate::serial::serial_print(driver.name());
    crate::serial::serial_print("\n");
    
    // Prioritize specialized drivers over fallback simplefb
    if driver.name() == "simplefb" {
        state.drivers.push(driver);
    } else {
        state.drivers.insert(0, driver);
    }
}

/// Get the primary DRM driver (usually first one registered)
pub fn get_primary_driver() -> Option<Arc<dyn DrmDriver>> {
    DRM_STATE.lock().drivers.first().cloned()
}

/// Allocate a buffer (GEM object) via the primary driver
pub fn alloc_buffer(size: usize) -> Option<GemHandle> {
    if size == 0 || size > MAX_GEM_BUFFER_SIZE {
        return None;
    }
    let mut state = DRM_STATE.lock();
    let driver = state.drivers.first()?.clone();
    let id = state.next_handle_id;
    state.next_handle_id += 1;
    drop(state);

    if let Some(mut handle) = driver.alloc_buffer(size) {
        handle.id = id;
        DRM_STATE.lock().handles.push(handle);
        return Some(GemHandle { id: handle.id, size: handle.size, phys_addr: handle.phys_addr });
    }
    None
}

pub fn get_handle(handle_id: u32) -> Option<GemHandle> {
    DRM_STATE.lock().handles.iter().find(|h| h.id == handle_id).map(|h| GemHandle { id: h.id, size: h.size, phys_addr: h.phys_addr })
}

/// Create a framebuffer from a GEM handle
pub fn create_fb(handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32> {
    let handle = get_handle(handle_id)?;
    let driver = get_primary_driver()?;
    
    // El driver necesita saber que resource_id usar (sequential handle_id) y la phys_addr
    let _hardware_fb_id = driver.create_fb(handle_id, width, height, pitch)?;
    
    let mut state = DRM_STATE.lock();
    let fb_id = state.next_fb_id;
    state.next_fb_id += 1;

    // Store metadata for the syscalls
    let fb = DrmFramebuffer {
        id: fb_id,
        gem_handle_id: handle_id,
        width,
        height,
        pitch,
        phys_addr: handle.phys_addr,
        size: (pitch as usize) * (height as usize),
    };
    
    state.framebuffers.push(fb);
    Some(fb_id)
}

/// Get framebuffer info
pub fn get_fb(fb_id: u32) -> Option<DrmFramebuffer> {
    DRM_STATE.lock().framebuffers.iter().find(|fb| fb.id == fb_id).cloned()
}

/// Get framebuffer info by the backing GEM handle id (VirtIO resource_id)
pub fn get_fb_by_gem_handle(gem_handle_id: u32) -> Option<DrmFramebuffer> {
    DRM_STATE.lock().framebuffers.iter().find(|fb| fb.gem_handle_id == gem_handle_id).cloned()
}

/// Page flip implementation via DRM subsystem
pub fn page_flip(fb_id: u32) -> bool {
    if let Some(driver) = get_primary_driver() {
        driver.page_flip(fb_id)
    } else {
        false
    }
}

/// Get primary driver caps
pub fn get_caps() -> Option<DrmCaps> {
    get_primary_driver().map(|d| d.get_caps())
}

/// GEM Close: free a buffer handle
pub fn gem_close(handle_id: u32) -> bool {
    let mut state = DRM_STATE.lock();
    if let Some(pos) = state.handles.iter().position(|h| h.id == handle_id) {
        let handle = state.handles[pos];
        let driver = state.drivers.first().cloned();
        state.handles.remove(pos);
        drop(state); // Unlock before calling driver
        
        if let Some(d) = driver {
            d.free_buffer(handle);
        }
        true
    } else {
        false
    }
}

/// Destroy a framebuffer
pub fn destroy_fb(fb_id: u32) -> bool {
    let mut state = DRM_STATE.lock();
    if let Some(pos) = state.framebuffers.iter().position(|fb| fb.id == fb_id) {
        state.framebuffers.remove(pos);
        true
    } else {
        false
    }
}

/// Get DRM resources
pub fn get_resources() -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let state = DRM_STATE.lock();
    let fbs: Vec<u32> = state.framebuffers.iter().map(|fb| fb.id).collect();
    let mut crtcs = Vec::new();
    let mut connectors = Vec::new();
    
    for driver in &state.drivers {
        let (_, d_crtcs, d_conns) = driver.get_resources();
        crtcs.extend(d_crtcs);
        connectors.extend(d_conns);
    }
    
    (fbs, crtcs, connectors)
}

pub fn get_connector(id: u32) -> Option<DrmConnector> {
    let state = DRM_STATE.lock();
    for driver in &state.drivers {
        if let Some(conn) = driver.get_connector(id) {
            return Some(conn);
        }
    }
    None
}

pub fn get_crtc(id: u32) -> Option<DrmCrtc> {
    let state = DRM_STATE.lock();
    for driver in &state.drivers {
        if let Some(crtc) = driver.get_crtc(id) {
            return Some(crtc);
        }
    }
    None
}

/// Wait for vblank on a CRTC
pub fn wait_vblank(crtc_id: u32) -> bool {
    if let Some(driver) = get_primary_driver() {
        driver.wait_vblank(crtc_id)
    } else {
        false
    }
}

/// Set hardware cursor
pub fn set_cursor(crtc_id: u32, x: i32, y: i32, handle: u32, flags: u32) -> bool {
    if let Some(driver) = get_primary_driver() {
        driver.set_cursor(crtc_id, x, y, handle, flags)
    } else {
        false
    }
}

/// Get all available planes across all drivers
pub fn get_planes() -> Vec<u32> {
    let state = DRM_STATE.lock();
    let mut planes = Vec::new();
    for driver in &state.drivers {
        planes.extend(driver.get_planes());
    }
    planes
}

/// Get info for a specific plane
pub fn get_plane(id: u32) -> Option<DrmPlane> {
    let state = DRM_STATE.lock();
    for driver in &state.drivers {
        if let Some(plane) = driver.get_plane(id) {
            return Some(plane);
        }
    }
    None
}

/// Set plane configuration (atomic-lite)
pub fn set_plane(plane_id: u32, crtc_id: u32, fb_id: u32, x: i32, y: i32, w: u32, h: u32, src_x: u32, src_y: u32, src_w: u32, src_h: u32) -> bool {
    if let Some(driver) = get_primary_driver() {
        driver.set_plane(plane_id, crtc_id, fb_id, x, y, w, h, src_x, src_y, src_w, src_h)
    } else {
        false
    }
}
