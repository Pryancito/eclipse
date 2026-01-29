//! wlroots FFI bindings
//! 
//! Provides Rust bindings to the wlroots compositor library

use super::{WaylandResult, WaylandError};

#[cfg(has_wlroots)]
use core::ffi::{c_void, c_int, c_char};

/// Initialize wlroots
pub fn init() -> Result<(), &'static str> {
    #[cfg(has_wlroots)]
    {
        // wlroots initialization happens when creating backend
        Ok(())
    }
    
    #[cfg(not(has_wlroots))]
    {
        Err("wlroots not available")
    }
}

#[cfg(has_wlroots)]
mod ffi {
    use super::*;
    
    // Opaque types
    #[repr(C)]
    pub struct wlr_backend {
        _private: [u8; 0],
    }
    
    #[repr(C)]
    pub struct wlr_renderer {
        _private: [u8; 0],
    }
    
    #[repr(C)]
    pub struct wlr_allocator {
        _private: [u8; 0],
    }
    
    #[repr(C)]
    pub struct wlr_compositor {
        _private: [u8; 0],
    }
    
    #[repr(C)]
    pub struct wlr_output_layout {
        _private: [u8; 0],
    }
    
    extern "C" {
        // Backend functions
        pub fn wlr_backend_autocreate(display: *mut c_void) -> *mut wlr_backend;
        pub fn wlr_backend_start(backend: *mut wlr_backend) -> bool;
        pub fn wlr_backend_destroy(backend: *mut wlr_backend);
        
        // Renderer functions
        pub fn wlr_renderer_autocreate(backend: *mut wlr_backend) -> *mut wlr_renderer;
        pub fn wlr_renderer_destroy(renderer: *mut wlr_renderer);
        
        // Allocator functions
        pub fn wlr_allocator_autocreate(
            backend: *mut wlr_backend,
            renderer: *mut wlr_renderer
        ) -> *mut wlr_allocator;
        pub fn wlr_allocator_destroy(allocator: *mut wlr_allocator);
        
        // Compositor functions
        pub fn wlr_compositor_create(
            display: *mut c_void,
            renderer: *mut wlr_renderer
        ) -> *mut wlr_compositor;
        
        // Output layout
        pub fn wlr_output_layout_create() -> *mut wlr_output_layout;
        pub fn wlr_output_layout_destroy(layout: *mut wlr_output_layout);
    }
}

#[cfg(has_wlroots)]
pub use ffi::*;

/// wlroots Backend wrapper
pub struct Backend {
    #[cfg(has_wlroots)]
    ptr: *mut ffi::wlr_backend,
    
    #[cfg(not(has_wlroots))]
    _phantom: core::marker::PhantomData<()>,
}

impl Backend {
    /// Create a new wlroots backend with auto-detection
    pub fn autocreate(display: *mut core::ffi::c_void) -> WaylandResult<Self> {
        #[cfg(has_wlroots)]
        {
            let ptr = unsafe { ffi::wlr_backend_autocreate(display) };
            if ptr.is_null() {
                Err(WaylandError::InitFailed)
            } else {
                Ok(Backend { ptr })
            }
        }
        
        #[cfg(not(has_wlroots))]
        {
            let _ = display;
            Err(WaylandError::NotAvailable)
        }
    }
    
    /// Start the backend
    pub fn start(&mut self) -> WaylandResult<()> {
        #[cfg(has_wlroots)]
        {
            let success = unsafe { ffi::wlr_backend_start(self.ptr) };
            if success {
                Ok(())
            } else {
                Err(WaylandError::InitFailed)
            }
        }
        
        #[cfg(not(has_wlroots))]
        {
            Err(WaylandError::NotAvailable)
        }
    }
    
    #[cfg(has_wlroots)]
    pub fn as_ptr(&self) -> *mut ffi::wlr_backend {
        self.ptr
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        #[cfg(has_wlroots)]
        unsafe {
            if !self.ptr.is_null() {
                ffi::wlr_backend_destroy(self.ptr);
            }
        }
    }
}

/// wlroots Renderer wrapper
pub struct Renderer {
    #[cfg(has_wlroots)]
    ptr: *mut ffi::wlr_renderer,
    
    #[cfg(not(has_wlroots))]
    _phantom: core::marker::PhantomData<()>,
}

impl Renderer {
    /// Create a renderer with auto-detection
    pub fn autocreate(backend: &Backend) -> WaylandResult<Self> {
        #[cfg(has_wlroots)]
        {
            let ptr = unsafe { ffi::wlr_renderer_autocreate(backend.ptr) };
            if ptr.is_null() {
                Err(WaylandError::InitFailed)
            } else {
                Ok(Renderer { ptr })
            }
        }
        
        #[cfg(not(has_wlroots))]
        {
            let _ = backend;
            Err(WaylandError::NotAvailable)
        }
    }
    
    #[cfg(has_wlroots)]
    pub fn as_ptr(&self) -> *mut ffi::wlr_renderer {
        self.ptr
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        #[cfg(has_wlroots)]
        unsafe {
            if !self.ptr.is_null() {
                ffi::wlr_renderer_destroy(self.ptr);
            }
        }
    }
}
