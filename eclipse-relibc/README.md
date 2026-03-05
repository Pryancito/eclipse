# eclipse-relibc

**C Library in Rust for Eclipse OS**

Implementación de la API C/POSIX en Rust para el sistema operativo Eclipse, al estilo de [relibc](https://gitlab.redox-os.org/redox-os/relibc/) de Redox. Proporciona los encajes (wrappers) de syscalls del microkernel y las cabeceras necesarias para ejecutar código C y Rust que use la libc.

## Características

- **Escrito en Rust**: código seguro y mantenible.
- **Plataforma Eclipse**: `src/platform/eclipse/` — syscalls y tipos específicos del microkernel Eclipse.
- **Cabeceras tipo POSIX**: `stdio`, `stdlib`, `string`, `unistd`, `pthread`, `fcntl`, `sys/*`, etc., implementadas sobre los syscalls de Eclipse.
- **no_std**: pensado para usuariospace del kernel; opcionalmente con `alloc` y heap para `malloc`/`FILE*`.

## Estructura del proyecto

```
eclipse-relibc/
├── src/
│   ├── header/          # Implementaciones de “cabeceras” C
│   │   ├── stdio/       # printf, FILE*, stdin/stdout/stderr
│   │   ├── stdlib/      # malloc, exit, abort
│   │   ├── unistd/      # read, write, exit, getpid, fork, exec
│   │   ├── pthread/     # Mutex, condvar (futex)
│   │   ├── sys_*/       # sys/stat, sys/mman, sys/socket, etc.
│   │   └── ...
│   ├── platform/
│   │   └── eclipse/     # Código específico de Eclipse OS
│   │       ├── mod.rs
│   │       └── syscall.rs  # Re-export eclipse_syscall
│   ├── types.rs         # c_int, size_t, pid_t, etc.
│   ├── c_str.rs
│   ├── internal_alloc.rs
│   └── lib.rs
├── Cargo.toml
└── README.md
```

## Requisitos

- Rust **nightly** (para `build-std` y target custom).
- Crate [eclipse-syscall](https://github.com/your-org/eclipse-syscall) en el mismo workspace (path `../eclipse-syscall`).

## Uso

En tu `Cargo.toml` (kernel userspace o apps):

```toml
[dependencies]
eclipse-libc = { path = "../eclipse-relibc", default-features = false, features = ["std", "allocator"] }
```

Features típicas:

- `std`: habilita APIs que usan alloc (p. ej. `FILE*`, `malloc`).
- `allocator`: activa el allocator global para `malloc`/`free`.
- `panic-handler`: integración con el panic handler de la aplicación.

## Compilación

Desde la raíz del repositorio Eclipse:

```bash
cargo +nightly build -p eclipse-libc --release
```

Para un target custom (p. ej. `x86_64-unknown-eclipse`):

```bash
cargo +nightly build -p eclipse-libc --target x86_64-unknown-eclipse.json -Z build-std=core,alloc --release
```

## Relación con el resto de Eclipse OS

- **eclipse-syscall**: interfaz en bruto de syscalls; eclipse-relibc la usa para implementar la API C/POSIX.
- **eclipse_std**: capa tipo “std” de Rust (heap, `println!`, `fn main()`) que usa eclipse-relibc como backend.
- **Servicios userspace** (init, filesystem, input, …): enlazan contra eclipse-relibc para syscalls y libc.

## Referencias

- [relibc (Redox)](https://gitlab.redox-os.org/redox-os/relibc/) — C library in Rust for Redox (y Linux WIP).
- [Eclipse OS](https://github.com/your-org/eclipse) — Microkernel y usuariospace.

## Licencia

MIT (o la que use el proyecto Eclipse OS).
