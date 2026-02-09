# Implementación de std Completo para Eclipse OS (Modelo Redox)

## Visión General

Implementar soporte completo de la biblioteca estándar de Rust para Eclipse OS, siguiendo el modelo de **Redox OS** y su **relibc**.

## Arquitectura de Redox OS (Modelo a Seguir)

### Componentes Principales de Redox

1. **relibc** (https://gitlab.redox-os.org/redox-os/relibc)
   - C library escrita en Rust
   - Compatible con POSIX
   - Backend para std de Rust
   - ~40,000+ líneas de código

2. **redox-syscall** 
   - Capa de syscalls segura
   - Interfaz tipada para todas las syscalls
   - Usado por relibc

3. **std personalizado**
   - Target: `x86_64-unknown-redox`
   - Usa relibc como backend
   - Compilado desde fuente con -Zbuild-std

4. **Scheme-based VFS**
   - URLs como paths: `file:/path`, `tcp:`, `udp:`
   - Cada "scheme" es un servidor userspace

## Plan de Implementación para Eclipse OS

### Fase 1: Eclipse Syscall Crate (1-2 semanas)

Crear `eclipse-syscall` - interfaz tipada para syscalls.

```rust
// eclipse-syscall/src/lib.rs
#![no_std]

/// Syscall numbers
pub mod number {
    pub const SYS_EXIT: usize = 0;
    pub const SYS_WRITE: usize = 1;
    pub const SYS_READ: usize = 2;
    // ... todos los syscalls
}

/// Syscall wrappers tipados
pub mod call {
    use super::number::*;
    
    #[inline(always)]
    pub unsafe fn syscall1(n: usize, arg1: usize) -> usize {
        let ret: usize;
        core::arch::asm!(
            "int 0x80",
            in("rax") n,
            in("rdi") arg1,
            lateout("rax") ret,
        );
        ret
    }
    
    pub fn exit(status: i32) -> ! {
        unsafe {
            syscall1(SYS_EXIT, status as usize);
        }
        unreachable!()
    }
    
    pub fn write(fd: usize, buf: &[u8]) -> Result<usize, Error> {
        let ret = unsafe {
            syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
        };
        if ret == usize::MAX {
            Err(Error::new(EINVAL))
        } else {
            Ok(ret)
        }
    }
}

/// Error handling (similar a relibc)
pub mod error {
    pub const EINVAL: i32 = 22;
    pub const ENOMEM: i32 = 12;
    // ... todos los errno
    
    #[derive(Debug)]
    pub struct Error {
        pub errno: i32,
    }
    
    impl Error {
        pub fn new(errno: i32) -> Self {
            Self { errno }
        }
    }
}
```

### Fase 2: Eclipse Libc (relibc-style) (6-8 semanas)

Crear `eclipse-libc` como biblioteca C/POSIX en Rust.

#### Estructura:
```
eclipse-libc/
├── Cargo.toml
├── src/
│   ├── lib.rs              # Entry point
│   ├── header/             # C headers (tipos)
│   │   ├── stdio/
│   │   ├── stdlib/
│   │   ├── string/
│   │   ├── unistd/
│   │   ├── sys/
│   │   └── ...
│   ├── platform/           # OS-specific
│   │   ├── eclipse/        # Eclipse OS backend
│   │   │   ├── mod.rs
│   │   │   ├── syscall.rs
│   │   │   ├── signal.rs
│   │   │   └── ...
│   │   └── mod.rs
│   └── c_str.rs            # Utilidades C
└── tests/
```

#### Funcionalidad Básica:

**stdio.h equivalente:**
```rust
// eclipse-libc/src/header/stdio/mod.rs
use crate::platform::types::*;
use eclipse_syscall::call::*;

#[repr(C)]
pub struct FILE {
    fd: c_int,
    flags: c_int,
    buffer: *mut c_void,
    // ...
}

pub static mut stdin: *mut FILE = core::ptr::null_mut();
pub static mut stdout: *mut FILE = core::ptr::null_mut();
pub static mut stderr: *mut FILE = core::ptr::null_mut();

#[no_mangle]
pub unsafe extern "C" fn fopen(
    path: *const c_char,
    mode: *const c_char
) -> *mut FILE {
    // Implementación usando syscalls de Eclipse
}

#[no_mangle]
pub unsafe extern "C" fn fwrite(
    ptr: *const c_void,
    size: size_t,
    nmemb: size_t,
    stream: *mut FILE,
) -> size_t {
    // Implementación
}

#[no_mangle]
pub unsafe extern "C" fn printf(
    format: *const c_char,
    ...
) -> c_int {
    // Implementación con varargs
}
```

**stdlib.h equivalente:**
```rust
// eclipse-libc/src/header/stdlib/mod.rs

#[no_mangle]
pub unsafe extern "C" fn malloc(size: size_t) -> *mut c_void {
    // Usar syscall de allocación o mmap
    // Implementar dlmalloc o similar
}

#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    // Implementación
}

#[no_mangle]
pub unsafe extern "C" fn exit(status: c_int) -> ! {
    eclipse_syscall::call::exit(status);
}
```

**pthread (threading):**
```rust
// eclipse-libc/src/header/pthread/mod.rs

#[repr(C)]
pub struct pthread_t {
    // Implementación específica de Eclipse
}

#[no_mangle]
pub unsafe extern "C" fn pthread_create(
    thread: *mut pthread_t,
    attr: *const pthread_attr_t,
    start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> c_int {
    // Usar syscall fork + exec o thread_create de Eclipse
}
```

### Fase 3: Syscalls del Kernel (2-3 semanas)

Expandir syscalls de Eclipse para soportar POSIX:

```rust
// eclipse_kernel/src/syscalls.rs

// Nuevos syscalls necesarios:
pub enum SyscallNumber {
    // Existentes
    Exit = 0,
    Write = 1,
    Read = 2,
    // ... existentes
    
    // Nuevos para std/POSIX
    Mmap = 20,          // Memory mapping
    Munmap = 21,        // Unmap memory
    Mprotect = 22,      // Change memory protection
    Brk = 23,           // Change data segment size
    Clone = 24,         // Create thread/process
    Futex = 25,         // Fast userspace mutex
    Nanosleep = 26,     // High-resolution sleep
    Gettime = 27,       // Get current time
    Pipe = 28,          // Create pipe
    Dup = 29,           // Duplicate fd
    Fcntl = 30,         // File control
    Ioctl = 31,         // Device control
    Poll = 32,          // Wait for events
    Select = 33,        // I/O multiplexing
    Socket = 34,        // Create socket
    Bind = 35,          // Bind socket
    Connect = 36,       // Connect socket
    Accept = 37,        // Accept connection
    Sendto = 38,        // Send data
    Recvfrom = 39,      // Receive data
    Shutdown = 40,      // Shutdown socket
    // ... más según necesidad
}
```

**Implementar mmap (crítico para allocadores):**
```rust
fn sys_mmap(
    addr: u64,
    length: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64
) -> u64 {
    // Validar parámetros
    if length == 0 || length > MAX_MMAP_SIZE {
        return u64::MAX; // Error
    }
    
    // Allocar memoria física
    let phys_addr = memory::allocate_pages((length + 0xFFF) / 0x1000);
    
    // Mapear en el espacio de proceso
    let virt_addr = if addr == 0 {
        // Kernel elige dirección
        find_free_virtual_region(length)
    } else {
        addr
    };
    
    // Mapear con permisos
    memory::map_user_memory(
        current_process_id(),
        virt_addr,
        phys_addr,
        length,
        prot_to_flags(prot)
    );
    
    virt_addr
}
```

### Fase 4: std Backend (4-6 semanas)

Crear backend de std para Eclipse OS.

#### Estructura:
```
rust/library/std/src/sys/eclipse/
├── mod.rs
├── alloc.rs          # Allocator (usa mmap/brk)
├── args.rs           # Command-line args
├── env.rs            # Environment variables
├── fs.rs             # File system
├── io.rs             # I/O traits
├── net.rs            # Networking
├── os.rs             # OS-specific extensions
├── path.rs           # Path handling
├── pipe.rs           # Pipes
├── process.rs        # Process management
├── stdio.rs          # stdin/stdout/stderr
├── thread.rs         # Threading
├── time.rs           # Time handling
└── ...
```

**Ejemplo - alloc.rs:**
```rust
// std/src/sys/eclipse/alloc.rs
use crate::alloc::{GlobalAlloc, Layout, System};
use eclipse_syscall::call::{mmap, munmap};

#[stable(feature = "alloc_system_type", since = "1.28.0")]
unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();
        
        // Usar mmap para allocaciones grandes
        if size >= 4096 {
            mmap(
                0,
                size,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0
            ) as *mut u8
        } else {
            // Usar allocador interno para small allocations
            dlmalloc::dlmalloc(size, align)
        }
    }
    
    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size();
        if size >= 4096 {
            munmap(ptr as usize, size);
        } else {
            dlmalloc::dlfree(ptr);
        }
    }
}
```

**Ejemplo - fs.rs:**
```rust
// std/src/sys/eclipse/fs.rs
use eclipse_syscall::call::{open, read, write, close};

pub struct File {
    fd: i32,
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let flags = opts.get_access_mode()
            | opts.get_creation_mode();
        
        let fd = open(
            path.as_os_str().as_bytes(),
            flags,
            0o666
        )?;
        
        Ok(File { fd })
    }
    
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        read(self.fd as usize, buf)
            .map_err(|e| io::Error::from_raw_os_error(e.errno))
    }
}
```

**Ejemplo - thread.rs:**
```rust
// std/src/sys/eclipse/thread.rs
use eclipse_syscall::call::clone;

pub struct Thread {
    id: ThreadId,
}

impl Thread {
    pub fn new(stack_size: usize, p: Box<dyn FnOnce()>) -> io::Result<Thread> {
        // Allocar stack
        let stack = mmap(
            0,
            stack_size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0
        )?;
        
        // Crear thread usando clone syscall
        let tid = clone(
            CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD,
            stack + stack_size,
            thread_start_wrapper,
            Box::into_raw(p) as *mut c_void
        )?;
        
        Ok(Thread { id: ThreadId(tid) })
    }
}
```

### Fase 5: Target Specification (1 semana)

Crear target para rustc:

```json
// x86_64-unknown-eclipse.json
{
  "llvm-target": "x86_64-unknown-none",
  "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-f80:128-n8:16:32:64-S128",
  "arch": "x86_64",
  "target-endian": "little",
  "target-pointer-width": "64",
  "target-c-int-width": "32",
  "os": "eclipse",
  "executables": true,
  "linker-flavor": "ld.lld",
  "linker": "rust-lld",
  "panic-strategy": "unwind",
  "disable-redzone": true,
  "features": "-mmx,-sse,+soft-float",
  "relocation-model": "static",
  "code-model": "kernel",
  "position-independent-executables": true
}
```

### Fase 6: Build System (1-2 semanas)

Integrar todo en el sistema de build:

```bash
# build_std.sh

# 1. Compilar eclipse-syscall
cd eclipse-syscall
cargo build --target x86_64-unknown-eclipse

# 2. Compilar eclipse-libc
cd eclipse-libc
cargo build --target x86_64-unknown-eclipse

# 3. Compilar std personalizado
cd rust
./configure --target=x86_64-unknown-eclipse
./x.py build --stage 1 library/std

# 4. Compilar aplicaciones
cd smithay_app
cargo build --target x86_64-unknown-eclipse
```

## Cronograma Estimado

### Mes 1-2: Fundamentos
- ✅ eclipse-syscall crate
- ✅ Syscalls básicos del kernel (mmap, brk, etc.)
- ✅ eclipse-libc básico (stdio, stdlib)

### Mes 3-4: Core Functionality
- ⏳ Allocador completo (dlmalloc integration)
- ⏳ Threading (pthread_create, mutexes)
- ⏳ File I/O completo

### Mes 5-6: std Backend
- ⏳ std/sys/eclipse implementation
- ⏳ Networking support
- ⏳ Time/Date support

### Mes 7-8: Integration & Testing
- ⏳ Build system integration
- ⏳ Testing suite
- ⏳ Performance optimization

## Comparación con Redox

| Característica | Redox | Eclipse (Propuesto) |
|----------------|-------|---------------------|
| Libc | relibc (~40k LOC) | eclipse-libc (similar) |
| Syscalls | ~80 syscalls | ~50 syscalls (expandir) |
| Threading | Si (fork + exec) | Si (clone syscall) |
| Networking | Si (scheme-based) | Si (syscalls directos) |
| VFS | Scheme-based URLs | Tradicional paths |
| std Support | Completo | Completo (objetivo) |

## Ventajas del Enfoque Redox

1. **Todo en Rust**: Seguridad de memoria
2. **POSIX Compatible**: Apps existentes funcionan
3. **Modular**: Cada componente independiente
4. **Testeable**: Cada capa se puede testear
5. **Mantenible**: Código más claro que C

## Primeros Pasos

### Semana 1-2: Crear eclipse-syscall

```bash
cd eclipse-kernel
cargo new --lib eclipse-syscall
```

```rust
// eclipse-syscall/src/lib.rs
#![no_std]

pub mod number;
pub mod call;
pub mod error;

// Implementación básica
```

### Semana 3-4: Expandir Kernel Syscalls

```rust
// eclipse_kernel/src/syscalls.rs

// Agregar:
fn sys_mmap(...) -> u64 { }
fn sys_munmap(...) -> u64 { }
fn sys_clone(...) -> u64 { }
fn sys_futex(...) -> u64 { }
```

### Semana 5-8: Eclipse Libc Básico

```rust
// eclipse-libc/src/lib.rs

// Implementar:
// - malloc/free
// - printf/scanf
// - fopen/fread/fwrite
// - pthread_create
```

## Recursos

- Redox OS relibc: https://gitlab.redox-os.org/redox-os/relibc
- Rust std source: https://github.com/rust-lang/rust/tree/master/library/std
- POSIX specification: https://pubs.opengroup.org/onlinepubs/9699919799/

## Conclusión

Implementar std completo para Eclipse OS siguiendo el modelo Redox es:
- ✅ **Factible**: Redox lo ha hecho
- ✅ **Beneficioso**: Apps pueden usar std completo
- ⚠️ **Largo**: 6-8 meses de trabajo
- ⚠️ **Complejo**: Requiere conocimiento profundo

¿Quieres que comience con la implementación de eclipse-syscall como primer paso?
