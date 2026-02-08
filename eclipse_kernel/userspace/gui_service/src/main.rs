//! GUI Service - Launches Graphical Environment
//! 
//! This service:
//! 1. Starts after network_service.
//! 2. Initializes Xwayland (simulated).
//! 3. Launches the Smithay Compositor (smithay_app) from disk.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, exit, yield_cpu, open, read, close, exec, O_RDONLY};

/// Buffer for loading the application binary
/// 2MB should be enough for our simulated app
static mut APP_BUFFER: [u8; 2 * 1024 * 1024] = [0; 2 * 1024 * 1024];

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                    GUI SERVICE                               ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!("[GUI-SERVICE] Starting (PID: {})", pid);
    
    // Simulate Xwayland startup
    println!("[GUI-SERVICE] Initializing Xwayland support...");
    println!("[GUI-SERVICE]   - Setting up X socket /tmp/.X11-unix/X0");
    println!("[GUI-SERVICE]   - Starting Xwayland server on :0");
    
    // Simulate some startup time
    for _ in 0..5 {
        yield_cpu();
    }
    
    println!("[GUI-SERVICE]   - Xwayland ready");
    println!("[GUI-SERVICE]   - DISPLAY=:0 configured");

    // Launch Smithay App
    println!("[GUI-SERVICE] Launching Smithay Compositor (smithay_app)...");
    
    let app_path = "/bin/smithay_app";
    
    unsafe {
        // Open application file
        // 0 for O_RDONLY (assuming standard flags)
        let fd = open(app_path, O_RDONLY, 0);
        
        if fd < 0 {
            println!("[GUI-SERVICE] ERROR: Failed to open {}", app_path);
            println!("[GUI-SERVICE] Entering sleep mode...");
            loop { yield_cpu(); }
        }
        
        println!("[GUI-SERVICE] Reading {}...", app_path);
        
        // Read file into buffer
        // Note: In a real implementation we'd read in chunks or use mmap
        // Here we assume it fits in our static buffer
        let bytes_read = read(fd as u32, &mut APP_BUFFER);
        
        close(fd);
        
        if bytes_read <= 0 {
            println!("[GUI-SERVICE] ERROR: Failed to read {} (bytes_read={})", app_path, bytes_read);
            loop { yield_cpu(); }
        }
        
        println!("[GUI-SERVICE] Loaded {} bytes. Executing...", bytes_read);
        
        // Create slice for exec
        let binary_slice = core::slice::from_raw_parts(APP_BUFFER.as_ptr(), bytes_read as usize);
        
        // Replace current process with synthesis_app
        let ret = exec(binary_slice);
        
        // Exec should not return on success
        println!("[GUI-SERVICE] ERROR: exec() failed with code {}", ret);
    }

    // Fallback loop
    loop {
        yield_cpu();
    }
}
