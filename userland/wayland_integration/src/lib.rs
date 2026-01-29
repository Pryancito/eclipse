//! Wayland Integration Layer for Eclipse OS
//! 
//! This library provides integration with libwayland and wlroots,
//! falling back to the custom Eclipse OS Wayland implementation
//! when the system libraries are not available.
//! 
//! # Features
//! 
//! - `libwayland`: Enable libwayland integration (default)
//! - `wlroots`: Enable wlroots integration (default)
//! - `std`: Enable standard library support
//! 
//! # Architecture
//! 
//! The integration layer provides:
//! - Automatic detection of system libraries via pkg-config
//! - Seamless fallback to custom implementation
//! - Common API surface for both modes
//! - FFI bindings for libwayland and wlroots

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
extern crate std;

pub mod bindings;
pub mod compositor;
pub mod server;
pub mod protocol;

/// Wayland integration version
pub const VERSION: &str = "0.1.0";

/// Check if libwayland is available at compile time
pub const HAS_LIBWAYLAND: bool = cfg!(has_libwayland);

/// Check if wlroots is available at compile time
pub const HAS_WLROOTS: bool = cfg!(has_wlroots);

/// Initialize the Wayland integration layer
/// 
/// This function initializes the appropriate backend based on
/// what libraries are available.
pub fn init() -> Result<(), &'static str> {
    #[cfg(has_libwayland)]
    {
        bindings::libwayland::init()?;
    }
    
    #[cfg(has_wlroots)]
    {
        bindings::wlroots::init()?;
    }
    
    Ok(())
}

/// Get information about the Wayland integration
pub fn get_info() -> WaylandInfo {
    WaylandInfo {
        has_libwayland: HAS_LIBWAYLAND,
        has_wlroots: HAS_WLROOTS,
        version: VERSION,
        backend: if HAS_WLROOTS {
            "wlroots"
        } else if HAS_LIBWAYLAND {
            "libwayland"
        } else {
            "custom"
        },
    }
}

/// Information about the Wayland integration
#[derive(Debug, Clone, Copy)]
pub struct WaylandInfo {
    pub has_libwayland: bool,
    pub has_wlroots: bool,
    pub version: &'static str,
    pub backend: &'static str,
}
