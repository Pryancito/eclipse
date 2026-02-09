# eclipse-libc

POSIX-compatible C library for Eclipse OS, written in Rust. Inspired by Redox OS's relibc.

## Current Implementation Status (Phase 2 - Week 3-4)

### ✅ Implemented

**Memory Management** (stdlib.h):
- `malloc()` - allocate memory via mmap
- `free()` - free memory
- `calloc()` - allocate and zero
- `realloc()` - resize allocation
- `abort()` - abort program

**String Operations** (string.h):
- `memcpy()` - copy memory
- `memset()` - set memory
- `strlen()` - string length

**File I/O** (stdio.h):
- `FILE` - file stream structure
- `stdin`, `stdout`, `stderr` - standard streams
- `fopen()` - open file
- `fclose()` - close file
- `fread()` - read from file
- `fwrite()` - write to file
- `fflush()` - flush buffer
- `fputc()` - write character to stream
- `putchar()` - write character to stdout
- `puts()` - write string to stdout
- `fputs()` - write string to stream

**POSIX** (unistd.h):
- `write()` - write to file descriptor
- `read()` - read from file descriptor
- `close()` - close file descriptor

### ⏳ TODO

**stdio.h**:
- [ ] printf/scanf family (variadic functions - complex in no_std)
- [ ] getc/fgetc
- [ ] ungetc
- [ ] ferror/feof/clearerr

**stdlib.h**:
- [ ] atoi/atof/strtol conversions
- [ ] getenv/setenv
- [ ] system()

**string.h**:
- [ ] strcmp/strncmp
- [ ] strcpy/strncpy
- [ ] strcat/strncat
- [ ] strstr/strchr

**pthread.h**:
- [ ] pthread_create/join
- [ ] pthread_mutex_t
- [ ] pthread_cond_t

**unistd.h**:
- [ ] fork/exec
- [ ] pipe/dup
- [ ] chdir/getcwd

## Usage

### File I/O Example

```rust
use eclipse_libc::*;

unsafe {
    // Open file
    let file = fopen(b"/path/to/file\0".as_ptr() as *const c_char, 
                     b"w\0".as_ptr() as *const c_char);
    
    if !file.is_null() {
        // Write to file
        let data = b"Hello, Eclipse OS!";
        fwrite(data.as_ptr() as *const c_void, 1, data.len(), file);
        
        // Close file
        fclose(file);
    }
    
    // Use stdout
    puts(b"Written to file!\0".as_ptr() as *const c_char);
}
```

### Memory Management Example

```rust
use eclipse_libc::*;

unsafe {
    let ptr = malloc(1024);
    
    // Use memory
    memset(ptr, 0, 1024);
    
    free(ptr);
}
```

## Building

```bash
cargo build --release
```

Produces:
- `libeclipse_libc.a` - static library (7.3 MB)
- `libeclipse_libc.rlib` - Rust library (35 KB)

## Architecture

```
Applications
    ↓
eclipse-libc (POSIX C API)
    ↓
eclipse-syscall (type-safe syscalls)
    ↓
Eclipse Kernel
```

## Progress

Phase 1 (eclipse-syscall): ✅ 100%
Phase 2 (eclipse-libc): 🔄 40% (basic functions + FILE I/O)
Phase 3 (kernel syscalls): ⏳ 0%
Phase 4 (std backend): ⏳ 0%

**Overall Progress: ~45%**

## Next Steps

1. **Week 5-6**: Complete stdlib and string operations
2. **Week 7-8**: Implement pthread support (requires kernel SYS_CLONE)
3. **Phase 3**: Expand kernel syscalls (mmap, munmap, clone, futex)
4. **Phase 4**: Implement std/sys/eclipse backend

## References

- Redox OS relibc: https://gitlab.redox-os.org/redox-os/relibc
- POSIX specification: https://pubs.opengroup.org/onlinepubs/9699919799/
