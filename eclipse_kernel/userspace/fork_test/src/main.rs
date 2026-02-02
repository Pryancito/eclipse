//! Fork Test - Minimal test to verify fork() return values
#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, fork, exit, yield_cpu};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("=== FORK TEST START ===");
    println!("Parent PID: {}", getpid());
    
    println!("Calling fork()...");
    let pid = fork();
    
    println!("After fork(), return value: {}", pid);
    
    if pid == 0 {
        // Child process
        println!("[CHILD] I am the child! fork() returned 0");
        println!("[CHILD] My PID is: {}", getpid());
        println!("[CHILD] Exiting...");
        exit(0);
    } else if pid > 0 {
        // Parent process
        println!("[PARENT] I am the parent! fork() returned {}", pid);
        println!("[PARENT] My PID is: {}", getpid());
        println!("[PARENT] Child PID is: {}", pid);
        
        // Wait a bit for child to finish
        for _ in 0..10000 {
            yield_cpu();
        }
        
        println!("[PARENT] Test complete!");
        exit(0);
    } else {
        // Fork failed
        println!("[ERROR] fork() failed!");
        exit(1);
    }
}
