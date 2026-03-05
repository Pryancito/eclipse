//! Log Service / Console - Central logging for all system services

#![no_main]
extern crate std;
extern crate alloc;

use std::prelude::*;

/// Log buffer for storing messages before filesystem is ready
const LOG_BUFFER_SIZE: usize = 4096;

struct LogBuffer {
    buf: [u8; LOG_BUFFER_SIZE],
    pos: usize,
}

static LOG_STATE: std::libc::Spinlock<LogBuffer> = std::libc::Spinlock::new(LogBuffer {
    buf: [0; LOG_BUFFER_SIZE],
    pos: 0,
});

fn log_to_buffer(state: &mut LogBuffer, msg: &str) {
    let bytes = msg.as_bytes();
    let available = LOG_BUFFER_SIZE - state.pos;
    let to_copy = bytes.len().min(available);
    if to_copy > 0 {
        state.buf[state.pos..state.pos + to_copy].copy_from_slice(&bytes[..to_copy]);
        state.pos += to_copy;
    }
}

fn flush_log_buffer(state: &mut LogBuffer) {
    if state.pos == 0 {
        return;
    }
    let fd = std::libc::eclipse_open("file:/tmp/system.log", std::libc::O_WRONLY | std::libc::O_CREAT | std::libc::O_APPEND, 0o644);
    if fd >= 0 {
        let written = std::libc::eclipse_write(fd as u32, &state.buf[..state.pos]);
        if written > 0 {
            state.pos = 0;
        }
        unsafe { std::libc::eclipse_close(fd); }
    }
}

fn log_message(msg: &str) {
    println!("{}", msg);
    let mut state = LOG_STATE.lock();
    log_to_buffer(&mut state, msg);
    log_to_buffer(&mut state, "\n");
    if state.pos > 3072 {
        flush_log_buffer(&mut state);
    }
}

#[no_mangle]
pub extern "Rust" fn main() -> i32 {
    let pid = unsafe { std::libc::getpid() };

    log_message("+--------------------------------------------------------------+");
    log_message("|              LOG SERVER / CONSOLE SERVICE                    |");
    log_message("|         Serial Output + File Logging (/tmp/system.log)       |");
    log_message("+--------------------------------------------------------------+");

    log_message("[LOG-SERVICE] Starting...");
    log_message("[LOG-SERVICE] Initializing logging subsystem...");
    println!("[LOG-SERVICE] Running with PID: {}", pid);
    log_message("[LOG-SERVICE] Serial port configured for output");
    log_message("[LOG-SERVICE] Log buffer allocated (4KB)");
    log_message("[LOG-SERVICE] Target log file: /tmp/system.log");
    log_message("[LOG-SERVICE] Ready to accept log messages from other services");

    let ppid = unsafe { std::libc::getppid() };
    if ppid > 0 {
        let _ = std::libc::send_ipc(ppid as u32, 255, b"READY");
        println!("[LOG-SERVICE] Sent READY signal to parent PID: {}", ppid);
        log_message("[LOG-SERVICE] Handshake with init complete");
    } else {
        log_message("[LOG-SERVICE] WARNING: No parent PID (ppid=0), cannot signal READY");
    }

    let mut heartbeat_counter = 0u64;
    let mut flush_counter = 0u64;
    let mut ipc_buffer = [0u8; 256];

    loop {
        heartbeat_counter += 1;
        flush_counter += 1;

        loop {
            let (len, _sender) = std::libc::receive_ipc(&mut ipc_buffer);
            if len == 0 {
                break;
            }
            if let Ok(msg) = core::str::from_utf8(&ipc_buffer[..len]) {
                log_message(msg);
            }
        }

        if flush_counter % 10000 == 0 {
            let mut state = LOG_STATE.lock();
            flush_log_buffer(&mut state);
        }

        if heartbeat_counter % 60000 == 0 && heartbeat_counter > 0 {
            log_message("[LOG-SERVICE] Operational - Processing log messages");
        }

        std::libc::sleep_ms(1);
    }
}
