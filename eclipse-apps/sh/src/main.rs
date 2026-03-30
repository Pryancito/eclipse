#![cfg_attr(target_vendor = "eclipse", no_std)]
#![cfg_attr(not(target_vendor = "eclipse"), no_main)]

#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
use std::prelude::v1::*;

use std::process::Command;

#[cfg(target_vendor = "eclipse")]
fn main() {
    println!("Eclipse OS v3 Shell (sh)");
    loop {
        print!("moebius@eclipse:~$ ");
        
        // Read line
        let mut input = String::new();
        let mut buffer = [0u8; 1];
        
        loop {
            // Read 1 byte from stdin (fd 0)
            if let Ok(1) = eclipse_syscall::call::read(0, &mut buffer) {
                let c = buffer[0];
                if c == b'\n' {
                    let _ = eclipse_syscall::call::write(1, b"\n");
                    break;
                } else if c == b'\r' {
                    let _ = eclipse_syscall::call::write(1, b"\r\n");
                    break;
                } else if c == 8 || c == 127 { // Backspace
                    if !input.is_empty() {
                        let _ = input.pop();
                        // Erase character on screen
                        let _ = eclipse_syscall::call::write(1, b"\x08 \x08");
                    }
                } else {
                    input.push(c as char);
                    // Echo character to stdout (fd 1)
                    let _ = eclipse_syscall::call::write(1, &[c]);
                }
            } else {
                // Yield CPU if no data
                let _ = eclipse_syscall::call::sched_yield();
            }
        }
        
        let cmd = input.trim();
        if cmd.is_empty() {
            continue;
        }

        if cmd == "exit" {
            break;
        }
        
        // Split args (very basic)
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        let program = parts[0];
        
        // Tries to execute from /bin/ first
        let bin_path = format!("/bin/{}", program);
        
        let mut command = Command::new(&bin_path);
        
        // Our new spawn_with_stdio (fds 0, 1, 2)
        match command.spawn_with_stdio(0, 1, 2) {
            Ok(mut child) => {
                match child.wait() {
                    Ok(status) => println!("Process exited with code {}", status.code().unwrap_or(-1)),
                    Err(e) => println!("Failed to wait: {:?}", e),
                }
            }
            Err(e) => {
                println!("Failed to execute {}: {:?}", program, e);
            }
        }
    }
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    println!("Only supported on Eclipse OS");
}
