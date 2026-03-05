//! Embedded service binaries (all built with eclipse_std, target x86_64-unknown-eclipse)

/// Service binaries embedded in kernel (in init startup order)
pub static LOG_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/log_service/target/x86_64-unknown-eclipse/release/log_service");
pub static DEVFS_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/devfs_service/target/x86_64-unknown-eclipse/release/devfs_service");
pub static INPUT_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/input_service/target/x86_64-unknown-eclipse/release/input_service");
pub static DISPLAY_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/display_service/target/x86_64-unknown-eclipse/release/display_service");
pub static NETWORK_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/network_service/target/x86_64-unknown-eclipse/release/network_service");
pub static GUI_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/gui_service/target/x86_64-unknown-eclipse/release/gui_service");
pub static FILESYSTEM_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/filesystem_service/target/x86_64-unknown-eclipse/release/filesystem_service");
pub static AUDIO_SERVICE_BINARY: &[u8] = include_bytes!("../userspace/audio_service/target/x86_64-unknown-eclipse/release/audio_service");
