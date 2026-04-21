#![no_std]

/// Service IDs for `sys_spawn_service` (arg1).
/// These must match the match arms in `eclipse_kernel/src/syscalls.rs` `get_service_slice`.
pub mod spawn_service {
    /// Log server
    pub const LOG: u32 = 0;
    /// Device-filesystem server
    pub const DEVFS: u32 = 1;
    /// Filesystem / VFS server
    pub const FILESYSTEM: u32 = 2;
    /// Input server
    pub const INPUT: u32 = 3;
    /// Display server
    pub const DISPLAY: u32 = 4;
    /// Audio server
    pub const AUDIO: u32 = 5;
    /// Network server
    pub const NETWORK: u32 = 6;
    /// GUI / compositor launcher
    pub const GUI: u32 = 7;
    /// Seat management server
    pub const SEATD: u32 = 8;

    /// Filesystem paths where the kernel loads each service binary.
    pub const PATH_LOG: &str = "/sbin/log_service";
    pub const PATH_DEVFS: &str = "/sbin/devfs_service";
    pub const PATH_FILESYSTEM: &str = "/sbin/filesystem_service";
    pub const PATH_INPUT: &str = "/sbin/input_service";
    pub const PATH_DISPLAY: &str = "/sbin/display_service";
    pub const PATH_AUDIO: &str = "/sbin/audio_service";
    pub const PATH_NETWORK: &str = "/sbin/network_service";
    pub const PATH_GUI: &str = "/sbin/gui_service";
    pub const PATH_SEATD: &str = "/sbin/seatd";
}

/// Numeric service-ID type used by the spawn_service syscall.
pub type SpawnServiceId = u32;

/// Returns the short name (used as process name) for a service ID.
pub fn spawn_service_short_name(service_id: u32) -> &'static str {
    match service_id {
        spawn_service::LOG => "log",
        spawn_service::DEVFS => "devfs",
        spawn_service::FILESYSTEM => "filesystem",
        spawn_service::INPUT => "input",
        spawn_service::DISPLAY => "display",
        spawn_service::AUDIO => "audio",
        spawn_service::NETWORK => "network",
        spawn_service::GUI => "gui",
        spawn_service::SEATD => "seatd",
        _ => "unknown",
    }
}

/// Returns the descriptive name for a service ID.
pub fn spawn_service_name(service_id: u32) -> &'static str {
    match service_id {
        spawn_service::LOG => "Log Server",
        spawn_service::DEVFS => "Device Filesystem Server",
        spawn_service::FILESYSTEM => "Filesystem Server",
        spawn_service::INPUT => "Input Server",
        spawn_service::DISPLAY => "Display Server",
        spawn_service::AUDIO => "Audio Server",
        spawn_service::NETWORK => "Network Server",
        spawn_service::GUI => "GUI Service",
        spawn_service::SEATD => "Seat Management Server",
        _ => "Unknown Service",
    }
}

/// Maps a `sys_spawn_service` service ID to the index in the init `SERVICES` array.
///
/// The init process keeps a 10-element `SERVICES` array:
/// `[kernel(0), init(1), log(2), devfs(3), filesystem(4), input(5), display(6), audio(7), network(8), gui(9)]`
///
/// Service IDs 0-7 map to indices 2-9 (offset of 2).
#[inline]
pub fn spawn_id_to_init_services_index(service_id: u32) -> usize {
    match service_id {
        7 => 9, // gui
        _ => service_id as usize + 2,
    }
}

/// Macro that resolves a service name token to its numeric service ID at compile time.
///
/// # Example
/// ```
/// use eclipse_program_codes::spawn_service_id;
/// let id: u32 = spawn_service_id!(log);   // → 0
/// let id: u32 = spawn_service_id!(devfs); // → 1
/// ```
#[macro_export]
macro_rules! spawn_service_id {
    (log)        => { $crate::spawn_service::LOG        };
    (devfs)      => { $crate::spawn_service::DEVFS      };
    (filesystem) => { $crate::spawn_service::FILESYSTEM };
    (input)      => { $crate::spawn_service::INPUT      };
    (display)    => { $crate::spawn_service::DISPLAY    };
    (audio)      => { $crate::spawn_service::AUDIO      };
    (network)    => { $crate::spawn_service::NETWORK    };
    (gui)        => { $crate::spawn_service::GUI        };
    (seatd)      => { $crate::spawn_service::SEATD      };
}
