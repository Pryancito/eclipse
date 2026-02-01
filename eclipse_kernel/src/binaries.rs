//! Embedded service binaries
//! 
//! This module contains the embedded service binaries that can be accessed
//! from userspace via the get_service_binary syscall.

/// Service binaries embedded in kernel (in init startup order)
pub static LOG_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/log_service/target/x86_64-unknown-none/release/log_service");
pub static DEVFS_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/devfs_service/target/x86_64-unknown-none/release/devfs_service");
pub static INPUT_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/input_service/target/x86_64-unknown-none/release/input_service");
pub static DISPLAY_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/display_service/target/x86_64-unknown-none/release/display_service");
pub static NETWORK_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/network_service/target/x86_64-unknown-none/release/network_service");

// Legacy services (kept for compatibility)
pub static FILESYSTEM_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/filesystem_service/target/x86_64-unknown-none/release/filesystem_service");
pub static AUDIO_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/audio_service/target/x86_64-unknown-none/release/audio_service");
