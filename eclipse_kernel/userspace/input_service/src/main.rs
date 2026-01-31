//! Input Service - Manages keyboard and mouse input
#![no_std]
#![no_main]

use eclipse_libc::{println, exit, getpid, yield_cpu};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    println!("[INPUT-SERVICE] Input service starting (PID: {})", pid);
    println!("[INPUT-SERVICE] Initializing input devices...");
    
    // Simulate input handling
    for i in 0..50 {
        if i % 10 == 0 {
            println!("[INPUT-SERVICE] Heartbeat {} - Processing input events", i);
        }
        yield_cpu();
    }
    
    println!("[INPUT-SERVICE] Input service shutting down cleanly");
    exit(0);
}
