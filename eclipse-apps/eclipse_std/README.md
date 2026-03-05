# Eclipse STD - Standard Library Compatibility for Eclipse OS

## Overview

`eclipse_std` is a compatibility layer that allows Eclipse OS applications to use a std-like interface (`println!`, `String`, `Vec`, etc.) while the target remains `no_std`. Proporciona el **runtime (crt0)**: el símbolo `_start` que el kernel usa como entry point del ELF, inicializa heap y llama a tu `main() -> i32`, y finalmente hace la syscall `exit(code)`.

## Quick Start

```rust
#![no_main]
extern crate std;  // std = eclipse_std in Cargo.toml
use std::prelude::*;

#[no_mangle]
pub extern "Rust" fn main() -> i32 {
    println!("Hello from Eclipse OS!");
    let name = String::from("Eclipse OS");
    let numbers = vec![1, 2, 3];
    println!("Running on: {} - {:?}", name, numbers);
    0  // código de salida (syscall exit)
}
```

### Cargo.toml

```toml
[dependencies]
std = { package = "eclipse_std", path = "../eclipse_std" }
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

### Flujo crt0 (runtime)

1. **Kernel**: `execve("/bin/app")` → carga ELF → salta a `_start` (símbolo en eclipse_std).
2. **eclipse_std::rt::_start** (crt0):
   - Lee `argc` del stack (layout System V ABI puesto por el kernel).
   - Alinea RSP (x86-64).
   - Inicializa heap (Box/Vec).
   - Envía READY/HEART a init (PID 1).
   - Llama a tu `main() -> i32`.
   - `exit(código)` (syscall al kernel).
3. Si `main()` hace **panic**, el panic handler de eclipse_std llama a `exit(1)` (no se vuelve a _start).

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
