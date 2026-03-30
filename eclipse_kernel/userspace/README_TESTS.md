# Tests en servicios (userspace)

Se han añadido **tests de unidad**, **benchmark** y **stress** a los servicios.

## Servicios con tests

| Servicio | Unit | Stress / benchmark | Cómo ejecutar |
|----------|------|---------------------|----------------|
| **input_service** | EventQueue (push/pop, lleno, FIFO) | 10k ciclos, 50k ops | `cargo run -p input_service --bin input_service_tests --features test` (requiere target host con std o panic=abort) |
| **log_service** | Buffer (append, capacidad) | 1000 appends | `cargo test -p log_service` (si el crate `test` está disponible) |
| **display_service** | enumerate_vesa_modes, select_best_vesa_mode | 10k selecciones | `cargo test -p display_service` |
| **filesystem_service** | ECLIPSEFS_MAGIC, EclipseFSHeader::from_bytes | — | `cargo test -p filesystem_service` |
| **network_service** | NetworkCard::new, InterfaceType | — | `cargo test -p network_service` |
| **audio_service** | AudioDeviceType | — | `cargo test -p audio_service` |
| **devfs_service** | DeviceType enum | — | `cargo test -p devfs_service` |
| **gui_service** | COMPOSITOR_PATH | — | `cargo test -p gui_service` |

## Nota sobre `eclipse_std`

Los servicios usan `eclipse_std` en lugar de la std de Rust. Por eso:

- **`cargo test`** en un servicio puede fallar con *"can't find crate for `test`"*, porque el crate `test` viene de la std estándar.
- **input_service** incluye una lib (`EventQueue`) y un binario de tests (`input_service_tests`) con feature `test`; al compilar para target host puede ser necesario `--target x86_64-unknown-linux-gnu` y un perfil con `panic = "abort"` si hay problemas de unwinding.

Para ejecutar tests en host de forma fiable, opciones posibles:

1. Añadir un workspace o crate de tests que use la std del host y dependa de las libs de los servicios (donde se haya extraído lógica a lib).
2. Ejecutar los tests en el target **x86_64-unknown-linux-musl** con un test runner propio que no dependa del crate `test`.
3. Usar los runners manuales (como `input_service_tests`) compilando con el target host y las flags necesarias.

## Estructura de input_service

- `src/lib.rs`: define `EventQueue` y el módulo `tests` (con feature `test`) con `run_all()`.
- `src/main.rs`: binario del servicio; usa `crate::EventQueue`.
- `src/test_runner.rs`: binario que llama a `input_service::tests::run_all()` (solo con `--features test`).
