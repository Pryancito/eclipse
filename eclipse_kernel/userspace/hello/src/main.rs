//! Hello World - Primer programa en userspace de Eclipse OS
#![no_std]
#![no_main]

use eclipse_libc::{println, exit, getpid, yield_cpu};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Obtener PID
    let pid = getpid();
    
    // Imprimir mensaje
    println!("Hello from userspace!");
    println!("My PID is: {}", pid);
    println!("Eclipse OS Microkernel is awesome!");
    
    // Yield CPU algunas veces
    for i in 0..5 {
        println!("Loop iteration: {}", i);
        yield_cpu();
    }
    
    println!("Goodbye from userspace!");
    exit(0);
}
