# Arquitectura de eclipse-relibc

## Capas

1. **eclipse-syscall** (dependencia)  
   Llamadas en bruto al kernel (syscall0/1/2/3, `call::read`, `call::write`, etc.).

2. **platform/eclipse**  
   Reexporta `eclipse_syscall` y agrupa lo específico de Eclipse (tipos, constantes, helpers si los hay).

3. **header/**  
   Cada módulo bajo `header/` implementa una “cabecera” C clásica:
   - **stdio**: `printf`, `FILE*`, `stdin`/`stdout`/`stderr` (sobre `write`/`read` y buffer interno).
   - **stdlib**: `malloc`/`free`/`exit`/`abort` (malloc usa `internal_alloc` o syscalls de memoria según features).
   - **unistd**: `read`, `write`, `exit`, `getpid`, `fork`, `exec`, etc. → syscalls directos.
   - **pthread**: mutex/condvar vía futex del kernel.
   - **sys_***: `stat`, `mman`, `socket`, etc. según lo que exponga el kernel.

4. **types.rs / c_str.rs**  
   Tipos C (c_int, size_t, pid_t, …) y utilidades para strings C (CStr, nul-terminated).

5. **internal_alloc**  
   Allocator para `malloc`/`calloc`/`realloc` cuando está activa la feature `allocator` (p. ej. bump o lista libre sobre memoria obtenida por syscall).

## Flujo típico

- Una app o servicio usa `eclipse-libc` (este crate).
- Las llamadas “C” (p. ej. `read`, `write`, `exit`) se resuelven en `header/unistd`, `header/stdio`, etc.
- Esos módulos llaman a `eclipse_syscall::call::*` o a `eclipse_syscall::syscallN`.
- El kernel Eclipse ejecuta el syscall y devuelve; el resultado se traduce a retornos/errno al estilo POSIX.

## Añadir una nueva “cabecera”

1. Crear `src/header/nombre_cabecera/mod.rs`.
2. Implementar las funciones `extern "C"` que necesites, usando `eclipse_syscall` o otros módulos de `header/`.
3. En `lib.rs`, añadir `pub mod nombre_cabecera` bajo `header` y reexportar en `pub use header::nombre_cabecera::*` si procede.

## Añadir soporte para otra plataforma

1. Añadir `src/platform/otra_plataforma/` (mod.rs, syscall.rs o equivalente).
2. En `src/platform/mod.rs`, `pub mod otra_plataforma` y usar `#[cfg(...)]` para elegir entre `eclipse` y `otra_plataforma`.
3. En los `header/` que dependan de la plataforma, usar `cfg` para llamar a la implementación correcta (como en relibc para Redox vs Linux).
