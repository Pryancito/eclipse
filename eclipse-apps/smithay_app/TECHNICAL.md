# Technical Documentation - smithay_app Binary Configuration

## Question: Does smithay_app need to be no_std and no_main?

**Answer: YES** - smithay_app MUST be configured with `#![no_std]` and `#![no_main]` because it will be loaded and executed by initd (specifically gui_service) in the Eclipse OS bare-metal environment.

## Current Configuration (CORRECT)

```rust
#![no_std]
#![no_main]

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // ... smithay_app implementation
}
```

## Why This Configuration is Required

### 1. Bare Metal Environment

Eclipse OS is a microkernel operating system where userspace applications run in a bare-metal environment without the standard library infrastructure:

- **No standard library**: No heap allocator, file I/O, threads, or other std features
- **No standard entry point**: Cannot use the standard `main()` function
- **Custom runtime**: Must define own `_start` entry point

### 2. Loading Mechanism

smithay_app is loaded by `gui_service` (which is started by init) using the following process:

```rust
// In gui_service/src/main.rs
let app_path = "/usr/bin/smithay_app";
let fd = open(app_path, O_RDONLY, 0);
let bytes_read = read(fd, &mut APP_BUFFER);
close(fd);

// Replace gui_service process with smithay_app
exec(binary_slice);  // Never returns on success
```

The `exec()` syscall:
1. Loads the ELF64 binary into memory
2. Validates the ELF header and program segments
3. Maps segments into process address space
4. Jumps to the entry point (`_start`)

### 3. ELF Binary Format

The built binary is a valid ELF64 executable:

```
$ file smithay_app
ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked

$ readelf -h smithay_app
  Entry point address:               0x1c22
  
$ nm smithay_app | grep _start
0000000000001c22 T _start
```

### 4. Comparison with Other Eclipse Apps

All applications in `eclipse-apps/` follow the same pattern:

#### systemd (eclipse-apps/systemd/src/main.rs)
```rust
#![no_std]
#![no_main]

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // systemd implementation
}
```

#### smithay_app (eclipse-apps/smithay_app/src/main.rs)
```rust
#![no_std]
#![no_main]

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // smithay compositor implementation
}
```

Both are loaded the same way via `exec()` syscall.

## Dependencies

smithay_app uses `eclipse-libc` which provides syscall wrappers:

```rust
use eclipse_libc::{
    println,          // Output via sys_write
    getpid,           // Get process ID
    yield_cpu,        // Yield to scheduler
    get_framebuffer_info,  // SYS_GET_FRAMEBUFFER_INFO (15)
    map_framebuffer,       // SYS_MAP_FRAMEBUFFER (16)
    send, receive,    // IPC syscalls
    FramebufferInfo,  // Framebuffer structure
};
```

All these functions are implemented as direct syscall wrappers without requiring std.

## Build Configuration

### Cargo.toml
```toml
[package]
name = "smithay_app"
version = "0.1.0"
edition = "2021"

[dependencies]
eclipse-libc = { path = "../../eclipse_kernel/userspace/libc" }

[profile.release]
panic = "abort"    # No unwinding in bare metal
lto = true         # Link-time optimization
opt-level = "z"    # Optimize for size
codegen-units = 1  # Single codegen unit for better optimization
```

### Build Command
```bash
cargo +nightly build --release \
    --target x86_64-unknown-none \
    -Zbuild-std=core,alloc
```

This produces a standalone ELF64 binary with no dependencies on the standard library.

## Execution Flow

```
┌──────────────────────────────────────────────────────────┐
│ 1. Kernel boots and starts init (PID 1)                 │
└────────────────────┬─────────────────────────────────────┘
                     │
                     ▼
┌──────────────────────────────────────────────────────────┐
│ 2. Init starts services in order, including gui_service │
└────────────────────┬─────────────────────────────────────┘
                     │
                     ▼
┌──────────────────────────────────────────────────────────┐
│ 3. gui_service opens /usr/bin/smithay_app               │
│    - open() syscall                                      │
│    - read() binary into buffer                           │
│    - exec() to replace process                           │
└────────────────────┬─────────────────────────────────────┘
                     │
                     ▼
┌──────────────────────────────────────────────────────────┐
│ 4. Kernel exec syscall handler:                         │
│    - Validates ELF header                                │
│    - Maps program segments                               │
│    - Sets up stack at 0x20040000                         │
│    - Jumps to _start entry point                         │
└────────────────────┬─────────────────────────────────────┘
                     │
                     ▼
┌──────────────────────────────────────────────────────────┐
│ 5. smithay_app::_start() begins execution               │
│    - Initializes framebuffer                             │
│    - Creates X11 socket                                  │
│    - Enters compositor event loop                        │
└──────────────────────────────────────────────────────────┘
```

## Common Mistakes to Avoid

❌ **WRONG**: Using standard library
```rust
use std::vec::Vec;  // ERROR: std not available
fn main() { }       // ERROR: no standard main
```

✅ **CORRECT**: Using no_std with custom entry point
```rust
#![no_std]
#![no_main]

use eclipse_libc::{println};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("Hello from smithay_app!");
    loop { }
}
```

## Summary

**YES, smithay_app MUST be `no_std` and `no_main`** because:

1. ✅ It runs in a bare-metal Eclipse OS environment
2. ✅ It's loaded via `exec()` syscall which expects an ELF64 binary
3. ✅ It needs a custom `_start` entry point (not `main`)
4. ✅ It uses only `eclipse-libc` for syscalls (no std library)
5. ✅ This matches the pattern of all other eclipse-apps binaries

The current configuration is **correct and complete**.
