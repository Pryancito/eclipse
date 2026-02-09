# eclipse-libc

POSIX-compatible C library for Eclipse OS, written in Rust. Inspired by Redox OS's relibc.

## Overview

eclipse-libc provides a C/POSIX interface for Eclipse OS, enabling full std library support and compatibility with C/C++ applications.

## Architecture

```
Applications (C/C++ or Rust with std)
    ↓
eclipse-libc (POSIX C API)
    ↓
eclipse-syscall (type-safe syscalls)
    ↓
Eclipse Kernel
```

## Current Implementation Status

### ✅ Implemented

**Memory Management** (stdlib.h):
- `malloc()` - allocate memory
- `free()` - free memory  
- `calloc()` - allocate and zero
- `realloc()` - resize allocation
- `abort()` - abort program

**String Operations** (string.h):
- `memcpy()` - copy memory
- `memset()` - set memory
- `strlen()` - string length

**I/O** (stdio.h, unistd.h):
- `putchar()` - write character
- `puts()` - write string
- `write()` - write to fd
- `read()` - read from fd
- `close()` - close fd

### ⏳ TODO

**stdio.h**:
- [ ] FILE streams
- [ ] fopen/fclose/fread/fwrite
- [ ] printf/scanf family
- [ ] stdin/stdout/stderr globals

**stdlib.h**:
- [ ] atoi/atof/strtol conversions
- [ ] getenv/setenv
- [ ] system()

**string.h**:
- [ ] strcmp/strncmp
- [ ] strcpy/strncpy
- [ ] strcat/strncat

**pthread.h**:
- [ ] pthread_create/join
- [ ] pthread_mutex_t
- [ ] pthread_cond_t

**unistd.h**:
- [ ] fork/exec
- [ ] pipe/dup
- [ ] chdir/getcwd

## Usage

### From Rust

```rust
use eclipse_libc::*;

unsafe {
    // Allocate memory
    let ptr = malloc(1024);
    
    // Use string functions
    let s = b"Hello\0";
    let len = strlen(s.as_ptr() as *const c_char);
    
    // I/O
    puts(s.as_ptr() as *const c_char);
    
    // Clean up
    free(ptr);
}
```

### From C (future)

```c
#include <stdio.h>
#include <stdlib.h>

int main() {
    puts("Hello from Eclipse OS!");
    
    void *ptr = malloc(1024);
    free(ptr);
    
    return 0;
}
```

## Building

```bash
cargo build --release
```

Produces:
- `libeclipse_libc.a` - static library
- `libeclipse_libc.rlib` - Rust library

## Implementation Details

### Memory Allocator

Currently uses mmap-based allocator:
- All allocations use `SYS_MMAP` syscall
- TODO: Implement free list or dlmalloc integration
- TODO: Implement `SYS_MUNMAP` for deallocation

### Threading

Threading support pending:
- Requires `SYS_CLONE` implementation in kernel
- Will use futex for synchronization

## Next Steps

1. **Week 3-4**: Complete stdio (FILE streams, printf/scanf)
2. **Week 5-6**: Complete stdlib and string operations
3. **Week 7-8**: Implement pthread support
4. **Future**: Networking (socket API)

## References

- Redox OS relibc: https://gitlab.redox-os.org/redox-os/relibc
- POSIX specification: https://pubs.opengroup.org/onlinepubs/9699919799/
