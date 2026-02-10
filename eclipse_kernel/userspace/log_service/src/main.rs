//! Log Service / Console - Central logging for all system services
//! 
//! This service provides centralized logging via:
//! - Serial port output (for real-time debugging)
//! - File logging to /var/log/system.log (when filesystem is available)
//! 
//! It must start first so other services have a place to send their logs.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, getppid, send, yield_cpu, open, write, close, O_WRONLY, O_CREAT, O_APPEND};

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
    
    // 2. Buffer the message for batched file writes
    log_to_buffer(msg);
    log_to_buffer("\n");
    
    // 3. Write buffered logs to /var/log/system.log
    // Flush buffer when it reaches a threshold or periodically
    unsafe {
        if LOG_BUFFER_POS > 3072 {  // Flush when 75% full
            flush_log_buffer();
        }
    }
}

/// Flush the log buffer to /var/log/system.log
fn flush_log_buffer() {
    unsafe {
        if LOG_BUFFER_POS == 0 {
            return; // Nothing to flush
        }
        
        // Open the log file using the file: scheme explicitly
        let fd = open("file:/var/log/system.log", O_WRONLY | O_CREAT | O_APPEND, 0o644);
        if fd >= 0 {
            // Write buffered data to file
            let written = write(fd as u32, &LOG_BUFFER[..LOG_BUFFER_POS]);
            if written > 0 {
                // Successfully written
                LOG_BUFFER_POS = 0; // Reset buffer
            }
            // Close the file
            close(fd);
        }
    }
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
    
    // Notify init (parent) that we are ready
    let ppid = getppid();
    if ppid > 0 {
        // Send READY message (Type 255 = Signal/Process)
        // This avoids collision with Network Server (ID 3)
        let ready_msg = b"READY";
        send(ppid as u32, 255, ready_msg);
        log_message("[LOG-SERVICE] Sent READY signal to init");
    }
    
    // Main loop - handle log messages
    let mut heartbeat_counter = 0u64;
    let mut log_stats_counter = 0u64;
    let mut flush_counter = 0u64;
    
    loop {
        heartbeat_counter += 1;
        flush_counter += 1;
        
        // Periodically flush log buffer to file (every 1 million iterations)
        if flush_counter % 1000000 == 0 {
            flush_log_buffer();
        }
        
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
