//! Fork Test - Minimal test to verify fork() return values
#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, fork, exit, sleep_ms};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("=== FORK TEST START ===");
    println!("Parent PID: {}", unsafe { getpid() });
    
    println!("Calling fork()...");
    let pid = unsafe { fork() };
    
    println!("After fork(), return value: {}", pid);
    
    if pid == 0 {
        // Child process
        println!("[CHILD] I am the child! fork() returned 0");
        println!("[CHILD] My PID is: {}", unsafe { getpid() });
        println!("[CHILD] Exiting...");
        unsafe { exit(0); }
    } else if pid > 0 {
        // Parent process
        println!("[PARENT] I am the parent! fork() returned {}", pid);
        println!("[PARENT] My PID is: {}", unsafe { getpid() });
        println!("[PARENT] Child PID is: {}", pid);
        
        // Wait a bit for child to finish
        sleep_ms(100);
        
        println!("[PARENT] Test complete!");
        unsafe { exit(0); }
    } else {
        // Fork failed
        println!("[ERROR] fork() failed!");
        unsafe { exit(1); }
    }
}
