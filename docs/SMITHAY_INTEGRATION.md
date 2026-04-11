# Integración Smithay en smithay_app

## Resumen

Smithay **no se puede usar** en smithay_app cuando se compila para Eclipse con `eclipse_std`, por conflicto de lang items. En cambio, sí es viable en **builds Linux** usando la feature `std_compat` de eclipse_std.

## Problema original

Al añadir Smithay como dependencia:

1. **`#[alloc_error_handler]`** en eclipse_std entra en conflicto con el de libstd.
2. **`panic_impl`** duplicado: eclipse_std y std definen el mismo lang item.

Causa: eclipse_std y libstd no pueden coexistir definiendo lang items en el mismo binario.

## Solución 1: eclipse_std compatible con std

eclipse_std imita la API de std de forma compatible. Con la feature **`std_compat`**:

- eclipse_std **no define** `#[alloc_error_handler]` ni `#[panic_handler]`.
- libstd aporta esos lang items cuando está en el grafo de dependencias.

### Uso en un binario Linux con Smithay

```toml
[target.'cfg(target_os = "linux")'.dependencies]
std = { package = "eclipse_std", path = "../eclipse_std", features = ["std_compat"] }
smithay = { version = "0.7", default-features = false, features = ["desktop", "renderer_pixman"] }
```

Así el código puede usar `std = eclipse_std` (rename) y Smithay trae libstd; eclipse_std en modo std_compat no define lang items y no hay conflicto.

### Nota sobre std y eclipse_std

smithay_app actualmente usa `std = { package = "eclipse_std", ... }` para poder escribir `use std::io` etc. En builds Eclipse (sin Smithay), eclipse_std aporta los lang items. En builds Linux con Smithay, usar `std_compat` en eclipse_std evita duplicados.

## Opciones para usar Smithay

### A. Binario Linux (recomendado con std_compat)

- `eclipse_std` con feature `std_compat`.
- Smithay + calloop + xkbcommon.
- Target: `x86_64-unknown-linux-gnu`.

Permite desarrollar/debuggear el compositor en Linux con Smithay.

### B. Patrones portados (actual para Eclipse)

Arquitectura inspirada en Smithay/cosmic-comp sin la dependencia:

- Módulos: compositor, damage, shell, etc.
- Damage tracking, Space, Output, etc.

### C. Eclipse con std real

Requeriría soporte completo de libc (epoll, XKB, etc.) en el microkernel.

## Referencias

- [Smithay](https://github.com/smithay/smithay)
- [cosmic-comp](https://github.com/pop-os/cosmic-comp)
- Smithay exige: calloop (event loop), xkbcommon (teclado), rustix (syscalls)
