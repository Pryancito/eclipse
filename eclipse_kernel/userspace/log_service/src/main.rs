//! Log Service / Console - Central logging for all system services
//! 
//! This service provides centralized logging via:
//! - Serial port output (for real-time debugging)
//! - File logging to /tmp/system.log (in-memory virtual filesystem, always writable)
//! 
//! It must start first so other services have a place to send their logs.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, getppid, send, sleep_ms, open, write, close, O_WRONLY, O_CREAT, O_APPEND, Spinlock};

/// Log buffer for storing messages before filesystem is ready
const LOG_BUFFER_SIZE: usize = 4096;

struct LogBuffer {
    buf: [u8; LOG_BUFFER_SIZE],
    pos: usize,
}

/// Global log buffer protected by a spinlock for SMP thread safety.
static LOG_STATE: Spinlock<LogBuffer> = Spinlock::new(LogBuffer {
    buf: [0; LOG_BUFFER_SIZE],
    pos: 0,
});

/// Append `msg` into the provided (already-locked) log buffer.
fn log_to_buffer(state: &mut LogBuffer, msg: &str) {
    let bytes = msg.as_bytes();
    let available = LOG_BUFFER_SIZE - state.pos;
    let to_copy = bytes.len().min(available);
    if to_copy > 0 {
        state.buf[state.pos..state.pos + to_copy].copy_from_slice(&bytes[..to_copy]);
        state.pos += to_copy;
    }
}

/// Write a log message to both serial and file
fn log_message(msg: &str) {
    // 1. Write to serial port (immediate output for debugging)
    println!("{}", msg);

    // 2. Buffer the message and flush if 75% full – all under the spinlock.
    let mut state = LOG_STATE.lock();
    log_to_buffer(&mut state, msg);
    log_to_buffer(&mut state, "\n");
    if state.pos > 3072 {
        flush_log_buffer(&mut state);
    }
}

/// Flush the log buffer to /tmp/system.log (caller must hold the lock).
fn flush_log_buffer(state: &mut LogBuffer) {
    if state.pos == 0 {
        return;
    }
    let fd = open("file:/tmp/system.log", O_WRONLY | O_CREAT | O_APPEND, 0o644);
    if fd >= 0 {
        let written = write(fd as u32, &state.buf[..state.pos]);
        if written > 0 {
            state.pos = 0;
        }
        close(fd);
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    log_message("+--------------------------------------------------------------+");
    log_message("|              LOG SERVER / CONSOLE SERVICE                    |");
    log_message("|         Serial Output + File Logging (/tmp/system.log)       |");
    log_message("+--------------------------------------------------------------+");
    
    log_message("[LOG-SERVICE] Starting...");
    log_message("[LOG-SERVICE] Initializing logging subsystem...");
    
    // Explicitly log the PID
    println!("[LOG-SERVICE] Running with PID: {}", pid);
    
    log_message("[LOG-SERVICE] Serial port configured for output");
    log_message("[LOG-SERVICE] Log buffer allocated (4KB)");
    log_message("[LOG-SERVICE] Target log file: /tmp/system.log");
    log_message("[LOG-SERVICE] Ready to accept log messages from other services");
    
    // Notify init (parent) that we are ready
    let ppid = getppid();
    if ppid > 0 {
        // Send READY message (Type 255 = Signal/Process)
        let ready_msg = b"READY";
        send(ppid as u32, 255, ready_msg);
        
        println!("[LOG-SERVICE] Sent READY signal to parent PID: {}", ppid);
        log_message("[LOG-SERVICE] Handshake with init complete");
    } else {
        log_message("[LOG-SERVICE] WARNING: No parent PID (ppid=0), cannot signal READY");
    }
    
    // Main loop - handle log messages
    let mut heartbeat_counter = 0u64;
    let mut log_stats_counter = 0u64;
    let mut flush_counter = 0u64;
    
    loop {
        heartbeat_counter += 1;
        flush_counter += 1;
        
        // Flush log buffer to file every ~10 s (1000 iterations * 10 ms)
        if flush_counter % 1000 == 0 {
            let mut state = LOG_STATE.lock();
            flush_log_buffer(&mut state);
        }
        
        // Status report every ~5 s (500 iterations * 10 ms)
        if heartbeat_counter % 500 == 0 {
            log_stats_counter += 1;
            
            if log_stats_counter % 10 == 0 {
                log_message("[LOG-SERVICE] Operational - Monitoring buffer");
            } else {
                log_message("[LOG-SERVICE] Operational - Processing log messages");
            }
        }
        
        sleep_ms(10);
    }
}
