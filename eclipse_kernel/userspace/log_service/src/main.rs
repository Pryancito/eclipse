//! Log Service / Console - Central logging for all system services
//! 
//! This service provides centralized logging via:
//! - Serial port output (for real-time debugging)
//! - File logging to /var/log/system.log (when filesystem is available)
//! 
//! It must start first so other services have a place to send their logs.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, yield_cpu};

/// Log buffer for storing messages before filesystem is ready
/// 
/// # Safety
/// These static mutable variables are safe because:
/// - Log service runs as a single-threaded process (PID 1 or 2)
/// - No concurrent access within the service
/// - Future: When IPC log messages are added, wrap in Mutex for thread safety
const LOG_BUFFER_SIZE: usize = 4096;
static mut LOG_BUFFER: [u8; LOG_BUFFER_SIZE] = [0; LOG_BUFFER_SIZE];
static mut LOG_BUFFER_POS: usize = 0;

/// Add a log message to the buffer
fn log_to_buffer(msg: &str) {
    unsafe {
        let bytes = msg.as_bytes();
        let available = LOG_BUFFER_SIZE - LOG_BUFFER_POS;
        let to_copy = bytes.len().min(available);
        
        if to_copy > 0 {
            LOG_BUFFER[LOG_BUFFER_POS..LOG_BUFFER_POS + to_copy]
                .copy_from_slice(&bytes[..to_copy]);
            LOG_BUFFER_POS += to_copy;
        }
    }
}

/// Write a log message to both serial and file
fn log_message(msg: &str) {
    // 1. Write to serial port (immediate output for debugging)
    println!("{}", msg);
    
    // 2. Buffer the message for later file write
    log_to_buffer(msg);
    log_to_buffer("\n");
    
    // 3. TODO: When filesystem syscalls (open, write, close) are available,
    //    write buffered logs to /var/log/system.log
    //    Example:
    //    let fd = open("/var/log/system.log", O_WRONLY | O_CREAT | O_APPEND);
    //    if fd >= 0 {
    //        write(fd, LOG_BUFFER.as_ptr(), LOG_BUFFER_POS);
    //        close(fd);
    //    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // TODO: Use PID in log messages when string formatting is available
    let _pid = getpid();
    
    log_message("╔══════════════════════════════════════════════════════════════╗");
    log_message("║              LOG SERVER / CONSOLE SERVICE                    ║");
    log_message("║         Serial Output + File Logging (/var/log/)             ║");
    log_message("╚══════════════════════════════════════════════════════════════╝");
    
    // Use a simple string concatenation approach for PID
    log_message("[LOG-SERVICE] Starting");
    log_message("[LOG-SERVICE] Initializing logging subsystem...");
    log_message("[LOG-SERVICE] Serial port configured for output");
    log_message("[LOG-SERVICE] Log buffer allocated (4KB)");
    log_message("[LOG-SERVICE] Target log file: /var/log/system.log");
    log_message("[LOG-SERVICE] Ready to accept log messages from other services");
    
    // Main loop - handle log messages
    let mut heartbeat_counter = 0u64;
    let mut log_stats_counter = 0u64;
    
    loop {
        heartbeat_counter += 1;
        
        // Process log messages (simulated IPC reception)
        if heartbeat_counter % 500000 == 0 {
            log_stats_counter += 1;
            
            // Report buffer usage every 10 heartbeats
            if log_stats_counter % 10 == 0 {
                log_message("[LOG-SERVICE] Operational - Monitoring buffer");
            } else {
                log_message("[LOG-SERVICE] Operational - Processing log messages");
            }
        }
        
        yield_cpu();
    }
}
