//! Fork Test - Minimal test to verify fork() return values

use std::prelude::v1::*;

fn main() {
    println!("=== FORK TEST START ===");
    println!("Parent PID: {}", unsafe { std::libc::getpid() });
    
    println!("Calling fork()...");
    let pid = unsafe { std::libc::fork() };
    
    println!("After fork(), return value: {}", pid);
    
    if pid == 0 {
        // Child process
        println!("[CHILD] I am the child! fork() returned 0");
        println!("[CHILD] My PID is: {}", unsafe { std::libc::getpid() });
        println!("[CHILD] Exiting...");
        unsafe { std::libc::exit(0); }
    } else if pid > 0 {
        // Parent process
        println!("[PARENT] I am the parent! fork() returned {}", pid);
        println!("[PARENT] My PID is: {}", unsafe { std::libc::getpid() });
        println!("[PARENT] Child PID is: {}", pid);
        
        // Wait a bit for child to finish
        unsafe { std::libc::sleep_ms(100); }
        
        println!("[PARENT] Test complete!");
        unsafe { std::libc::exit(0); }
    } else {
        // Fork failed
        println!("[ERROR] fork() failed!");
        unsafe { std::libc::exit(1); }
    }
}
