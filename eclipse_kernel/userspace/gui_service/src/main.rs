//! GUI Service - Launches Graphical Environment
//! 
//! This service:
//! 1. Starts after network_service.
//! 2. Initializes Xwayland (simulated).
//! 3. Launches the XFwl4 Compositor (xfwl4) from disk.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, exit, yield_cpu, open, read, close, exec, O_RDONLY};

/// Buffer for loading the application binary
/// 2MB should be enough for our simulated app
static mut APP_BUFFER: [u8; 32 * 1024 * 1024] = [0; 32 * 1024 * 1024];

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
    println!("[GUI-SERVICE]   - Xwayland ready");
    println!("[GUI-SERVICE]   - DISPLAY=:0 configured");

    // Launch XFwl4 App
    println!("[GUI-SERVICE] Launching XFwl4 Compositor (xfwl4)...");
    
    let app_path = "file:/usr/bin/xfwl4";
    
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
        // Read file into buffer in chunks
        let mut total_bytes_read = 0;
        loop {
             let chunk_size = read(fd as u32, &mut APP_BUFFER[total_bytes_read..]);
             if chunk_size < 0 {
                 println!("[GUI-SERVICE] ERROR: Failed to read (ret={})", chunk_size);
                 break;
             }
             if chunk_size == 0 {
                 break; // EOF
             }
             total_bytes_read += chunk_size as usize;
             // print progress every ~100KB to avoid spam
             if total_bytes_read % (100 * 1024) == 0 {
                  println!("[GUI-SERVICE] Read {} bytes...", total_bytes_read);
             }
        }
        
        let bytes_read = total_bytes_read as isize;
        
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
