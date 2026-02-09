//! Simple Hello World using eclipse_std

use eclipse_std::prelude::*;

fn main() -> i32 {
    println!("Hello from Eclipse OS!");
    println!("This app uses eclipse_std for familiar syntax");
    
    // Can use String
    let os_name = String::from("Eclipse OS");
    println!("Running on: {}", os_name);
    
    // Can use Vec
    let mut numbers = Vec::new();
    for i in 1..=5 {
        numbers.push(i);
    }
    println!("Numbers: {:?}", numbers);
    
    0
}

eclipse_main!(main);
