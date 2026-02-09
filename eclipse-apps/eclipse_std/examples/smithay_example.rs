//! Smithay App example using eclipse_std

use eclipse_std::prelude::*;
use eclipse_libc::{getpid, yield_cpu};

fn main() -> i32 {
    let pid = getpid();
    
    println!("╔════════════════════════════════════════════════╗");
    println!("║      SMITHAY COMPOSITOR (eclipse_std)          ║");
    println!("╚════════════════════════════════════════════════╝");
    println!("[SMITHAY] Starting (PID: {})", pid);
    
    // Can now use String and Vec!
    let compositor_name = String::from("Smithay Compositor");
    let mut client_list = Vec::new();
    
    println!("[SMITHAY] {}", compositor_name);
    println!("[SMITHAY] Clients connected: {}", client_list.len());
    
    // Simulate some work
    for i in 0..5 {
        println!("[SMITHAY] Initialization step {}/5", i + 1);
        for _ in 0..1000 {
            yield_cpu();
        }
    }
    
    println!("[SMITHAY] Ready and running!");
    
    // Main loop
    let mut counter = 0u64;
    loop {
        counter += 1;
        if counter % 1000000 == 0 {
            println!("[SMITHAY] [Status] Active - iterations: {}", counter);
        }
        yield_cpu();
    }
}

eclipse_main!(main);
