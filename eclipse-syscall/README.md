# eclipse-syscall

Type-safe syscall interface for Eclipse OS, inspired by redox-syscall.

## Features

- Zero-cost abstractions over raw syscalls
- Type safety with Result types
- POSIX-compatible error codes
- Inline assembly for maximum performance
- no_std compatible

## Usage

```rust
use eclipse_syscall::call::*;

fn main() {
    // Write to stdout
    let msg = b"Hello, Eclipse OS!\n";
    write(1, msg).unwrap();
    
    // Open a file
    let fd = open("/path/to/file", O_RDONLY).unwrap();
    
    // Map memory
    let addr = mmap(
        0,
        4096,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0
    ).unwrap();
}
```

## Status

Foundation for full std support on Eclipse OS.

Current syscalls:
- Process: exit, fork, exec, getpid, getppid
- I/O: read, write, open, close
- Memory: mmap, munmap, brk
- IPC: send, receive
- More to come...

## Next Steps

This crate is the foundation for:
1. eclipse-libc (POSIX C library in Rust)
2. std backend for Eclipse OS
3. Full application ecosystem
