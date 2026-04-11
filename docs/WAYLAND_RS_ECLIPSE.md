# wayland-rs en Eclipse OS

Resumen de la opción **wayland-rs** (Pure Rust) frente a Smithay para el compositor Wayland de Eclipse OS, y cómo encaja con lo que ya existe en el kernel.

## Por qué wayland-rs puede encajar mejor

- **Pure Rust**: `wayland-server` con `default-features = false` evita depender de libwayland.so (C).
- **Menos capas**: No arrastras libudev, libinput, Mesa, etc.; solo protocolos Wayland y tu propio backend de display/input.
- **Control**: Entiendes cómo van los píxeles (cliente → wl_shm → compositor → framebuffer) sin ocultarlo un framework grande.
- **Portabilidad**: Más fácil llevarlo a `no_std` / target Eclipse si en el futuro quieres un compositor más “bare metal”.

## Lo que ya tienes en Eclipse OS

### 1. Sockets tipo Unix (transporte Wayland)

El kernel ya expone una capa compatible con “Unix sockets” para el transporte:

- **Syscalls**: `socket`, `bind`, `listen`, `accept`, `connect` (p. ej. en `eclipse_kernel/src/syscalls.rs`).
- **Scheme `socket:`**: `eclipse_kernel/src/servers.rs` — `SocketScheme` con `bind(path)`, `listen()`, `accept()`, `connect(path)`.
- **libc**: `eclipse-relibc` (y eclipse-libc) tienen `socket`, `bind`, `listen`, `accept`, `connect` sobre esos syscalls.

Para wayland-rs el “puente” es ese: **servidor escucha en una ruta (ej. `/var/run/wayland-0` o `wayland-0`), clientes hacen `connect(path)`**. Falta asegurar que la implementación actual de `accept` / lectura/escritura en el scheme permita realmente pasar bytes entre procesos (hoy `accept` devuelve EAGAIN; hay que completar el pairing y el flujo de datos).

### 2. Display / framebuffer

- Framebuffer vía syscalls (map, info, etc.) ya lo usas en el backend Eclipse de smithay_app.
- Esa misma capa sirve para un compositor basado en wayland-rs: tú dibujas en el framebuffer según los buffers Wayland (p. ej. wl_shm).

## Integración técnica sugerida

### 1. Dependencia wayland-server (sin C)

```toml
[dependencies]
wayland-server = { version = "0.31", default-features = false }
# default-features = false evita enlazar libwayland.so
```

### 2. Bucle de eventos e IPC

- **Problema principal**: Wayland asume sockets Unix; en Eclipse OS el transporte son tus syscalls/scheme `socket:`.
- **Pasos**:
  - Asegurar que `bind` + `listen` en una ruta (ej. `/var/run/wayland-0` o la convención que uses) funcione con el scheme actual.
  - Completar el scheme para que `accept()` devuelva un fd/stream usable y que `read`/`write` sobre ese fd muevan bytes entre el proceso servidor (compositor) y el cliente.
  - Crear un `Display` de wayland-rs y asociarlo a ese “listener” (por ejemplo un wrapper que use tus `accept`/read/write en lugar de `std::os::unix::net`).

### 3. Handlers (Globales)

- En wayland-rs defines **Globales** (Compositor, wl_shm, Seat, etc.) implementando `GlobalDispatch<Interface, Data>`.
- Es el equivalente a lo que Smithay hace por ti; aquí lo haces explícito (más código, más control).

### 4. wl_shm (memoria compartida)

- Opción más directa para Eclipse: cliente y compositor comparten un pool (archivo mapeado o equivalente en tu FS/shm).
- wayland-rs (y crates como wayland-shm) te dan la parte protocolo; tú implementas el backend de buffers (por ejemplo sobre un `file:` o un scheme `shm:` en Eclipse si lo añades).

### 5. Event loop

- En Linux/Rust se suele usar **calloop** para integrar el `Display` de wayland-rs con el bucle de eventos.
- En Eclipse OS puedes:
  - Usar calloop si compila con tu target y con tu capa de sockets (usando tus fds de socket), o
  - Implementar un bucle propio que: `accept` → insertar cliente en el `Display` → `dispatch_clients` / `flush_clients` y mezclar con input/framebuffer.

## Comparación rápida

| Aspecto              | Smithay (actual en Linux)     | wayland-rs (propuesto)        |
|----------------------|-------------------------------|--------------------------------|
| Dependencias C       | Indirectas (winit, etc.)     | Ninguna con default-features   |
| Complejidad          | Framework “con opiniones”     | Solo protocolo + tu backend   |
| Socket / transporte  | std Unix (Linux)              | Tu scheme `socket:` en Eclipse |
| Event loop           | winit / Smithay               | calloop o bucle propio         |
| wl_shm               | Incluido en Smithay           | wayland-shm + tu shm/FS        |

## Siguiente paso concreto para Eclipse OS

1. **Completar el scheme `socket:`** en el kernel para que `accept` y read/write permitan conexiones reales entre procesos (compositor y clientes Wayland).
2. **Crear un crate mínimo “eclipse-wayland-server”** (o módulo dentro de smithay_app):
   - `wayland-server` con `default-features = false`.
   - `Display::new()`, registrar Globales (Compositor, wl_shm, Seat básico).
   - Listener que use `socket`/`bind`/`listen`/`accept` de eclipse-relibc y conecte los streams al `Display`.
3. **Event loop**: integrar con calloop (si es viable en tu target) o con el mismo bucle que ya usas para input + framebuffer.
4. **wl_shm**: implementar el backend de buffers (archivo temporal o scheme `shm:`) y enlazarlo con wayland-shm / handlers de wayland-rs.

Con eso tendrías un servidor Wayland “pure Rust” que habla por tu propio IPC (sockets del kernel) y dibuja sobre tu framebuffer, sin Smithay ni libwayland.so.

## Estado actual: feature `wayland` opcional

En **smithay_app**, wayland-server y wayland-protocols están detrás de la feature **`wayland`** para el target Eclipse:

- **Sin** `--features wayland` (por defecto): el build para `x86_64-unknown-linux-musl` compila correctamente; no se incluyen wayland-server ni wayland-protocols.
- **Con** `--features wayland`: la cadena de dependencias (errno, wayland-sys, downcast-rs, etc.) espera el crate `std` del sysroot. Con `-Z build-std=core,alloc` no hay `std` en el sysroot; el patch `std = eclipse_std` solo aplica cuando un crate declara `std` en Cargo.toml. Hasta tener `std` disponible para este target, la feature `wayland` no compilará.

**Progreso:** En `eclipse-apps/vendor/` hay forks de **bitflags**, **errno**, **downcast-rs** y **wayland-sys** que usan `std = eclipse_std` (path). Con los patches activos, la build con `--features wayland` avanza hasta **rustix** (dependencia de wayland-backend).

**Intentos con backend libc de rustix:**  
Forzar el backend libc (`RUSTFLAGS="--cfg rustix_use_libc"`) hace que rustix use llamadas vía libc. Para que el patch aplique, el paquete en `eclipse-relibc` debe llamarse **`libc`** y los miembros del workspace deben depender de `libc = "0.2"` (no por path con otro nombre); así `[patch.crates-io] libc = { path = "../eclipse-relibc" }` sustituye la libc de crates.io. Añadir **`"target-family": "unix"`** en `x86_64-unknown-linux-musl.json` hace que `cfg(unix)` sea cierto y rustix compile `zero_msghdr`. **eclipse-relibc** incluye ya `msghdr`, `cmsghdr`, `MSG_*`, `CMSG_*`, `sendmsg`/`recvmsg`. Con los patches de wayland activos y lo anterior, la build llega a compilar nuestra libc y falla en **rustix** porque su backend libc **asume `std`** (Iterator, FusedIterator, OsStrExt, Option::Some del prelude, etc.) y nosotros usamos `-Z build-std=core,alloc`. **Build con `-Z build-std=core,alloc,std`:** Si se añade `std` al sysroot, rustix tendría el `std` real, pero Cargo **no aplica `[patch.crates-io]` a las dependencias del std** al usar build-std (véase [PR #9424](https://github.com/rust-lang/cargo/pull/9424), cerrado sin merge; se planteó un futuro `[patch]` específico para std). El std del sysroot sigue usando libc de crates.io (con módulo `unistd`), no eclipse-relibc, y el build falla. No se puede inyectar eclipse_std como "std" porque rustix usa el crate std del lenguaje.

Conclusión: para wayland en Eclipse haría falta (1) portar rustix a core/alloc, o (2) usar la opción 1 (crate Wayland no_std mínimo) del apartado siguiente, o (3) aplicar un parche local a Cargo (como en el PR #9424) para que build-std respete los patches del workspace.

## ¿Wayland-rs no_std?

**wayland-rs (wayland-server, wayland-backend) no es no_std** y no tiene feature para serlo:

- **wayland-backend** tiene **rustix** como dependencia obligatoria (no opcional): se usa en el transporte (sockets Unix, `send_msg`/`rcv_msg` con SCM_RIGHTS, epoll/kqueue), en `socket_peercred`, y en tipos como `RawPid`/`RawUid`/`RawGid`. Todo el backend “rs” (pure Rust) está atado a `std` + rustix.
- No existe en crates.io un sustituto wayland “no_std” ni un wayland-server que permita cambiar solo el transporte.

**Opciones si quieres Wayland en Eclipse sin std/rustix:**

1. **Crate propio no_std mínimo (“wayland-wire” o dentro de smithay_app)**  
   - Implementar solo lo necesario: **wire format** del protocolo (mensajes binarios), **wl_display** + **wl_registry** + **wl_compositor** (y luego wl_surface/wl_shm si hace falta).  
   - Definir un **trait de transporte** (p. ej. `Read + Write` o `fn send(&mut self, buf: &[u8], fds: &[RawFd])` / `fn recv(&mut self, buf: &mut [u8], fds: &mut [RawFd])`).  
   - En Eclipse: implementar ese trait con tus sockets del kernel (read/write sobre el fd devuelto por `accept`).  
   - Ventaja: control total, sin rustix ni std; encaja con `eclipse_std` y con tu scheme de sockets.  
   - Coste: más código propio (wire format + dispatch mínimo); se puede reutilizar la especificación XML del protocolo para generar estructuras/opcodes.

2. **Fork wayland-backend con feature “custom transport”**  
   - Añadir un backend alternativo que en lugar de `UnixStream` + rustix use un **trait** (p. ej. `WaylandTransport`) inyectado; en Eclipse implementarías ese trait con tus sockets.  
   - Sigue requiriendo que el resto del crate (y wayland-server) pueda compilar con `std = eclipse_std` o con un subset; el bloqueo actual es rustix, no solo el transporte.

3. **Proponer upstream (wayland-rs) un backend “pluggable transport”**  
   - Issue/PR para que wayland-backend permita un transporte abstracto (trait) y, en un futuro, un build sin rustix (o con rustix opcional).  
   - A largo plazo sería la solución más limpia si el proyecto lo acepta.

**Recomendación práctica para Eclipse:** la opción 1 (crate no_std mínimo con wire format + trait de transporte + sockets de Eclipse) es la única que evita por completo std y rustix y te deja un servidor Wayland mínimo que puedes ampliar (wl_shm, etc.) según necesites.
