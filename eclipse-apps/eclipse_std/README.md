# Eclipse STD - Standard Library Compatibility for Eclipse OS

## Overview

`eclipse_std` is a compatibility layer that allows Eclipse OS applications to use a `std`-like interface with `main()` functions, while maintaining compatibility with the microkernel's `no_std` architecture.

## Quick Start

### Using eclipse_std in your application

```rust
use eclipse_std::prelude::*;

fn main() -> i32 {
    println!("Hello from Eclipse OS!");
    
    // You can use:
    // - String and Vec (via alloc)
    // - println!/eprintln! macros
    // - Box, format!, etc.
    
    let name = String::from("Eclipse OS");
    let mut numbers = Vec::new();
    numbers.push(1);
    numbers.push(2);
    numbers.push(3);
    
    println!("Running on: {}", name);
    println!("Numbers: {:?}", numbers);
    
    0  // Return exit code
}

eclipse_main!(main);
```

### Cargo.toml

```toml
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[dependencies]
eclipse_std = { path = "../eclipse_std" }

[profile.release]
panic = "abort"
lto = true
opt-level = "z"
```

## Features

- ✅ Familiar `main()` function (instead of `_start`)
- ✅ Heap allocation (String, Vec, Box, etc.)
- ✅ `println!`/`eprintln!` macros
- ✅ Compatible with existing `exec()` syscall
- ✅ Automatic runtime initialization
- ✅ Panic handler with location information
- ✅ 2MB heap (configurable)

## Architecture

```
User Application (with main)
         ↓
    eclipse_std
         ↓
  eclipse_libc (syscalls)
         ↓
   Eclipse Kernel
```

### How it Works

1. You write a normal `main() -> i32` function
2. The `eclipse_main!` macro generates the required `_start` entry point
3. `_start` calls `main_wrapper` which:
   - Initializes the heap
   - Sets up panic handler
   - Calls your `main()` function
   - Exits with the return code

## Example: Converting smithay_app

### Before (no_std):
```rust
#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, yield_cpu};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    println!("[SMITHAY] Starting (PID: {})", pid);
    
    loop {
        yield_cpu();
    }
}
```

### After (eclipse_std):
```rust
use eclipse_std::prelude::*;
use eclipse_libc::{getpid, yield_cpu};

fn main() -> i32 {
    let pid = getpid();
    println!("[SMITHAY] Starting (PID: {})", pid);
    
    // Can now use Vec, String, etc.
    let mut compositor_state = String::from("initializing");
    
    loop {
        yield_cpu();
    }
}

eclipse_main!(main);
```

## Current Limitations

1. **Heap Size**: Fixed 2MB heap (no dynamic growth yet)
2. **Allocator**: Simple bump allocator (no deallocation)
3. **Threading**: Not yet implemented
4. **File I/O**: Basic - only stdin/stdout/stderr
5. **Networking**: Not yet implemented

## Future Enhancements

- [ ] Syscall-based dynamic heap allocation
- [ ] Better allocator (with deallocation)
- [ ] Thread support via Eclipse IPC
- [ ] File I/O wrappers
- [ ] Networking support
- [ ] Mutex/RwLock implementations
- [ ] Environment variables
- [ ] Command-line arguments

## Building

```bash
cd eclipse-apps/eclipse_std
cargo +nightly build --release --target x86_64-unknown-none -Zbuild-std=core,alloc
```

## Testing

Create a test application:

```bash
cd eclipse-apps
cargo new --lib my-test-app
cd my-test-app
# Add eclipse_std to dependencies
# Write your main() function
cargo build --release --target x86_64-unknown-none -Zbuild-std=core,alloc
```

## License

Part of the Eclipse OS project.
