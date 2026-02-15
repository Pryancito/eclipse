#![no_std]
#![no_main]

extern crate eclipse_std as std;

use std::prelude::*;
use std::println;

std::eclipse_main!(test_main);

fn test_main() -> i32 {
    println!("--- Eclipse OS std Verification ---");
    
    // Test File I/O
    println!("Testing File I/O...");
    let test_file = "test.txt";
    let message = "Hello from eclipse_std!";
    
    match std::fs::write(test_file, message.as_bytes()) {
        Ok(_) => println!("Successfully wrote to {}", test_file),
        Err(_) => println!("Failed to write to {}", test_file),
    }
    
    match std::fs::read_to_string(test_file) {
        Ok(content) => println!("Read back: '{}'", content),
        Err(_) => println!("Failed to read from {}", test_file),
    }
    
    // Test Threading/Time
    println!("Testing Sleep...");
    std::thread::sleep(std::time::Duration::from_millis(100));
    println!("Sleep finished.");
    
    println!("Verification Complete!");
    0
}
