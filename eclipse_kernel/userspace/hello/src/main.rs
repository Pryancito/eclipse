//! Hello World - Primer programa en userspace de Eclipse OS

use std::prelude::v1::*;

fn main() {
    // Obtener PID
    let pid = unsafe { std::libc::getpid() };
    
    // Imprimir mensaje
    println!("Hello from userspace!");
    println!("My PID is: {}", pid);
    println!("Eclipse OS Microkernel is awesome!");
    
    // Yield CPU some times
    for i in 0..5 {
        println!("Loop iteration: {}", i);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    
    println!("Goodbye from userspace!");
}
