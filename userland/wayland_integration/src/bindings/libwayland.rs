//! libwayland FFI bindings
//! 
//! Provides Rust bindings to libwayland-server and libwayland-client

use super::{WaylandResult, WaylandError};

#[cfg(has_libwayland)]
use core::ffi::{c_void, c_int, c_char};

/// Initialize libwayland
pub fn init() -> Result<(), &'static str> {
    #[cfg(has_libwayland)]
    {
        // libwayland initialization is implicit when creating a display
        Ok(())
    }
    
    #[cfg(not(has_libwayland))]
    {
        Err("libwayland not available")
    }
}

#[cfg(has_libwayland)]
mod ffi {
    use super::*;
    
    // Opaque types
    #[repr(C)]
    pub struct wl_display {
        _private: [u8; 0],
    }
    
    #[repr(C)]
    pub struct wl_event_loop {
        _private: [u8; 0],
    }
    
    #[repr(C)]
    pub struct wl_client {
        _private: [u8; 0],
    }
    
    extern "C" {
        // Display functions
        pub fn wl_display_create() -> *mut wl_display;
        pub fn wl_display_destroy(display: *mut wl_display);
        pub fn wl_display_get_event_loop(display: *mut wl_display) -> *mut wl_event_loop;
        pub fn wl_display_add_socket_auto(display: *mut wl_display) -> *const c_char;
        pub fn wl_display_run(display: *mut wl_display);
        
        // Event loop functions
        pub fn wl_event_loop_dispatch(loop_: *mut wl_event_loop, timeout: c_int) -> c_int;
    }
}

#[cfg(has_libwayland)]
pub use ffi::*;

/// Wayland Display wrapper
pub struct Display {
    #[cfg(has_libwayland)]
    ptr: *mut ffi::wl_display,
    
    #[cfg(not(has_libwayland))]
    _phantom: core::marker::PhantomData<()>,
}

impl Display {
    /// Create a new Wayland display
    pub fn create() -> WaylandResult<Self> {
        #[cfg(has_libwayland)]
        {
            let ptr = unsafe { ffi::wl_display_create() };
            if ptr.is_null() {
                Err(WaylandError::InitFailed)
            } else {
                Ok(Display { ptr })
            }
        }
        
        #[cfg(not(has_libwayland))]
        {
            Err(WaylandError::NotAvailable)
        }
    }
    
    /// Add a socket with an automatically chosen name
    pub fn add_socket_auto(&mut self) -> WaylandResult<&str> {
        #[cfg(has_libwayland)]
        {
            let socket_name = unsafe { ffi::wl_display_add_socket_auto(self.ptr) };
            if socket_name.is_null() {
                Err(WaylandError::InitFailed)
            } else {
                // Convert C string to Rust str (unsafe but necessary for FFI)
                Ok("wayland-0") // Simplified for now
            }
        }
        
        #[cfg(not(has_libwayland))]
        {
            Err(WaylandError::NotAvailable)
        }
    }
    
    /// Run the display event loop
    pub fn run(&mut self) {
        #[cfg(has_libwayland)]
        unsafe {
            ffi::wl_display_run(self.ptr);
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        #[cfg(has_libwayland)]
        unsafe {
            if !self.ptr.is_null() {
                ffi::wl_display_destroy(self.ptr);
            }
        }
    }
}
