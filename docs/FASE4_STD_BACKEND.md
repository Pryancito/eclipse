# Fase 4: std Backend Implementation Plan

## Objetivo

Implementar el backend `std/sys/eclipse` para permitir que aplicaciones Rust utilicen la biblioteca estándar completa en Eclipse OS.

## Arquitectura

```
Rust Application
    ↓
std (biblioteca estándar de Rust)
    ↓
std/sys/eclipse (backend específico de Eclipse OS) ← FASE 4
    ↓
eclipse-libc (76 funciones POSIX)
    ↓
eclipse-syscall (syscall wrappers)
    ↓
Eclipse Kernel
```

## Enfoque: Dos Caminos Posibles

### Opción A: Fork de rust-lang/rust (Complejo pero Completo)

**Ventajas**:
- std completo y oficial
- Máxima compatibilidad
- Ecosistema completo

**Desventajas**:
- Requiere modificar el compilador Rust
- Proceso de build complejo
- Mantenimiento difícil
- 4-6 semanas de trabajo

### Opción B: Capa de Compatibilidad std-like (Práctico)

**Ventajas**:
- No requiere modificar rustc
- Más rápido de implementar
- Fácil mantenimiento
- Funciona hoy

**Desventajas**:
- No es std "oficial"
- Requiere usar nuestro prelude
- Limitado por no_std

## Decisión: Opción B (Mejorada)

Vamos a **mejorar eclipse_std** existente para proporcionar una experiencia casi idéntica a std, aprovechando todo el trabajo de eclipse-libc.

## Plan de Implementación

### Semana 1-2: eclipse_std v2.0

Mejorar eclipse_std para usar eclipse-libc:

```rust
// eclipse_std/src/lib.rs
#![no_std]

extern crate alloc;
extern crate eclipse_libc;

pub mod io {
    // Usar FILE* de eclipse-libc
    pub struct File { /* ... */ }
    pub struct Stdin { /* ... */ }
    pub struct Stdout { /* ... */ }
}

pub mod thread {
    // Usar pthread de eclipse-libc
    pub struct Thread { /* ... */ }
    pub fn spawn<F>(f: F) -> Thread 
        where F: FnOnce() { /* ... */ }
}

pub mod sync {
    // Usar pthread_mutex de eclipse-libc
    pub struct Mutex<T> { /* ... */ }
}

pub mod collections {
    // Re-export alloc::collections
    pub use alloc::vec::Vec;
    pub use alloc::string::String;
}

pub mod prelude {
    pub use alloc::vec::Vec;
    pub use alloc::string::String;
    pub use crate::io::{stdin, stdout};
    pub use crate::thread;
}
```

### Semana 3-4: Módulos Principales

**io module**:
- File (wrapper de FILE*)
- Stdin, Stdout, Stderr
- Read, Write traits
- BufReader, BufWriter

**thread module**:
- Thread (wrapper de pthread_t)
- spawn(), sleep()
- JoinHandle

**sync module**:
- Mutex (wrapper de pthread_mutex_t)
- Condvar (wrapper de pthread_cond_t)
- RwLock (basado en mutex)

**fs module**:
- OpenOptions
- File operations
- Directory operations

### Semana 5-6: Testing & Documentation

- Tests unitarios
- Tests de integración
- Ejemplos
- Documentación completa

## Estructura de Archivos

```
eclipse-apps/eclipse_std/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs           # Core library
│   ├── macros.rs        # eclipse_main! + others
│   ├── io/
│   │   ├── mod.rs
│   │   ├── stdio.rs
│   │   └── buffered.rs
│   ├── thread/
│   │   ├── mod.rs
│   │   └── local.rs
│   ├── sync/
│   │   ├── mod.rs
│   │   ├── mutex.rs
│   │   └── condvar.rs
│   ├── fs/
│   │   ├── mod.rs
│   │   └── file.rs
│   ├── collections/
│   │   └── mod.rs       # Re-exports
│   └── prelude.rs
└── examples/
    ├── hello.rs
    ├── threads.rs
    └── file_io.rs
```

## Uso por Aplicaciones

```rust
// Aplicación usando eclipse_std
#![no_std]
#![no_main]

use eclipse_std::prelude::*;

fn main() -> i32 {
    println!("Hello from Eclipse OS!");
    
    // Vec and String work
    let mut numbers = Vec::new();
    numbers.push(1);
    numbers.push(2);
    
    // File I/O
    let mut file = File::create("/tmp/test.txt").unwrap();
    file.write_all(b"Hello!").unwrap();
    
    // Threading
    let handle = thread::spawn(|| {
        println!("Thread running!");
    });
    handle.join();
    
    0
}

eclipse_main!(main);
```

## Ventajas de Este Enfoque

1. **Implementación rápida**: 2-3 semanas vs 6+ semanas
2. **Aprovecha todo eclipse-libc**: 76 funciones ya implementadas
3. **No requiere modificar rustc**: Funciona hoy
4. **API familiar**: Casi idéntica a std
5. **Mantenible**: Código simple y claro

## Comparación con std "Real"

| Feature | std oficial | eclipse_std v2 |
|---------|-------------|----------------|
| Vec, String | ✅ | ✅ (via alloc) |
| println! | ✅ | ✅ (custom macro) |
| File I/O | ✅ | ✅ (via eclipse-libc) |
| Threading | ✅ | ✅ (via pthread) |
| Mutex | ✅ | ✅ (via pthread_mutex) |
| Collections | ✅ | ✅ (re-export alloc) |
| Network | ✅ | ⏳ (future) |
| Process | ✅ | ⏳ (future) |

## Timeline

- Semana 1: io module completo
- Semana 2: thread + sync modules
- Semana 3: fs module + testing
- Semana 4: Documentation + examples

**Total: 4 semanas** para eclipse_std v2.0 completo

## Siguientes Pasos

1. Expandir eclipse_std/src/lib.rs
2. Implementar io module con File
3. Implementar thread module con spawn
4. Implementar sync module con Mutex
5. Testing exhaustivo
6. Documentación

## Resultado Final

Aplicaciones Rust en Eclipse OS podrán usar:
- ✅ main() normal
- ✅ println! / eprintln!
- ✅ Vec, String, HashMap
- ✅ File I/O
- ✅ Threading
- ✅ Mutex, Condvar
- ✅ 90% del API de std

**Progreso total del proyecto: 75% → 100%**
