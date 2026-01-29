//! Embedded systemd binary
//! 
//! This module contains the mini-systemd binary embedded in the kernel

/// Get the embedded mini-systemd binary
/// Returns the binary data as a static byte slice
pub fn get_embedded_systemd() -> &'static [u8] {
    // Try to include the mini-systemd binary if it was copied during build
    // If the file doesn't exist at build time, this will use an empty slice
    const MINI_SYSTEMD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mini-systemd.bin"));
    
    if MINI_SYSTEMD.is_empty() {
        crate::debug::serial_write_str("EMBEDDED_SYSTEMD: Binary is empty, using fallback\n");
        &[]
    } else {
        crate::debug::serial_write_str(&alloc::format!(
            "EMBEDDED_SYSTEMD: Loaded {} bytes\n",
            MINI_SYSTEMD.len()
        ));
        MINI_SYSTEMD
    }
}

/// Check if embedded systemd is available
pub fn has_embedded_systemd() -> bool {
    const MINI_SYSTEMD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mini-systemd.bin"));
    !MINI_SYSTEMD.is_empty()
}

