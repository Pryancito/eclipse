//! Server module
//! 
//! High-level server interface that uses either libwayland or custom implementation

use crate::bindings::WaylandResult;

#[cfg(has_libwayland)]
use crate::bindings::WaylandError;

#[cfg(has_libwayland)]
use crate::bindings::libwayland;

/// Wayland server structure
pub struct Server {
    #[cfg(has_libwayland)]
    display: Option<libwayland::Display>,
    
    #[cfg(not(has_libwayland))]
    _custom: CustomServer,
}

#[cfg(not(has_libwayland))]
struct CustomServer {
    // Custom server state
}

impl Server {
    /// Create a new Wayland server
    pub fn new() -> WaylandResult<Self> {
        #[cfg(has_libwayland)]
        {
            let display = libwayland::Display::create()?;
            Ok(Server {
                display: Some(display),
            })
        }
        
        #[cfg(not(has_libwayland))]
        {
            Ok(Server {
                _custom: CustomServer {},
            })
        }
    }
    
    /// Add a socket to the server
    pub fn add_socket(&mut self) -> WaylandResult<&str> {
        #[cfg(has_libwayland)]
        {
            if let Some(ref mut display) = self.display {
                display.add_socket_auto()
            } else {
                Err(WaylandError::InitFailed)
            }
        }
        
        #[cfg(not(has_libwayland))]
        {
            // Custom server socket
            Ok("wayland-0")
        }
    }
    
    /// Run the server event loop
    pub fn run(&mut self) {
        #[cfg(has_libwayland)]
        {
            if let Some(ref mut display) = self.display {
                display.run();
            }
        }
        
        #[cfg(not(has_libwayland))]
        {
            // Custom server run loop
        }
    }
    
    /// Get the display pointer (for compositor integration)
    pub fn get_display_ptr(&mut self) -> *mut core::ffi::c_void {
        #[cfg(has_libwayland)]
        {
            if let Some(ref display) = self.display {
                display.as_ptr() as *mut core::ffi::c_void
            } else {
                core::ptr::null_mut()
            }
        }
        
        #[cfg(not(has_libwayland))]
        {
            core::ptr::null_mut()
        }
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new().expect("Failed to create server")
    }
}
