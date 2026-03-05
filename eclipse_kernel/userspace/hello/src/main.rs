//! Hello World - Primer programa en userspace de Eclipse OS
#![no_std]
#![no_main]

use eclipse_libc::{println, exit, getpid, sleep_ms};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Obtener PID
    let pid = unsafe { getpid() };
    
    // Imprimir mensaje
    println!("Hello from userspace!");
    println!("My PID is: {}", pid);
    println!("Eclipse OS Microkernel is awesome!");
    
    // Yield CPU some times
    for i in 0..5 {
        println!("Loop iteration: {}", i);
        sleep_ms(1);
    }
    
    println!("Goodbye from userspace!");
    unsafe { exit(0); }
}
