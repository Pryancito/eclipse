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

/// Abstract trait for DRM Driver implementations
pub trait DrmDriver: Send + Sync {
    fn name(&self) -> &'static str;
    fn get_caps(&self) -> DrmCaps;
    
    /// Allocate a buffer (GEM object)
    fn alloc_buffer(&self, size: usize) -> Option<GemHandle>;
    
    /// Create a framebuffer from a GEM handle
    fn create_fb(&self, handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32>;
    
    /// Page flip: atomically switch to a new framebuffer
    fn page_flip(&self, fb_id: u32) -> bool;
    
    /// Set hardware cursor position
    fn set_cursor(&self, x: u32, y: u32) -> bool;
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
}

/// Register a new DRM driver
pub fn register_driver(driver: Arc<dyn DrmDriver>) {
    let mut state = DRM_STATE.lock();
    crate::serial::serial_print("[DRM] Registering driver: ");
    crate::serial::serial_print(driver.name());
    crate::serial::serial_print("\n");
    state.drivers.push(driver);
}

/// Get the primary DRM driver (usually first one registered)
pub fn get_primary_driver() -> Option<Arc<dyn DrmDriver>> {
    DRM_STATE.lock().drivers.first().cloned()
}

/// Allocate a buffer (GEM object) via the primary driver
pub fn alloc_buffer(size: usize) -> Option<GemHandle> {
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
