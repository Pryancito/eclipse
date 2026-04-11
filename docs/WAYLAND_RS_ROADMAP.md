# Roadmap: wayland-rs en Eclipse OS

Plan por fases para tener un servidor Wayland "pure Rust" que funcione sobre Eclipse OS.

---

## Fase 1: Sockets Unix que funcionen de verdad (kernel)

**Objetivo:** Que un proceso pueda `listen()` en una ruta y otro `connect()`; `accept()` devuelva un fd nuevo y `read`/`write` en ambos extremos muevan bytes.

**Estado actual (resumen):**
- ~~`accept()` siempre devuelve `EAGAIN`~~ **Hecho:** hay cola de conexiones; `connect()` encola y `accept()` devuelve un fd nuevo.
- ~~`connect()` no empareja con el servidor~~ **Hecho:** se crea una `Connection` con buffers y se encola en el listener.
- ~~`read`/`write` stubs~~ **Hecho:** leen/escriben en los buffers de la conexión (hasta 256 KiB por dirección); sin datos → `EAGAIN`, conexión cerrada → `Ok(0)`.
- **close:** marca el extremo como cerrado y elimina la conexión cuando ambos lados cerraron.

**Qué hay que hacer:**

1. **Modelo de “conexión” en el scheme**
   - Una **conexión** es un par (servidor, cliente) con buffers (o una cola por dirección).
   - Al hacer `connect(path)`:
     - Buscar un listener con ese `path`.
     - Crear una `Connection` (id, buffer A→B, buffer B→A, o una sola cola bidireccional).
     - Apuntar el socket del **cliente** a esa conexión (guardar `connection_id` en el socket).
     - Encolar en el listener “pending connection” (connection_id + lado cliente ya asignado).
   - Al hacer `accept()` en el listener:
     - Si hay pending connection: sacarla, crear un **nuevo fd** (nuevo resource_id en el scheme) que sea el “lado servidor” de esa misma conexión.
     - Devolver ese fd como resultado de `accept()`.

2. **read/write por conexión**
   - Cada fd que sea “extremo de una conexión” (sea del servidor o del cliente) debe tener asociado un `connection_id`.
   - `write(fd, buf)`: meter los bytes en el buffer “hacia el otro extremo” de esa conexión.
   - `read(fd, buf)`: sacar bytes del buffer “desde el otro extremo”.
   - Si no hay datos: devolver EAGAIN (o bloqueo, según quieras).

**Dónde tocar:** `eclipse_kernel/src/servers.rs` (SocketScheme, Socket, y nuevas estructuras Connection + cola de pendientes). Posiblemente un ioctl o extensión del scheme para “obtener fd del lado servidor de la conexión” si no quieres mezclar “socket” y “connection” en el mismo tipo de recurso.

**Criterio de éxito:** Un test en userspace: proceso A hace socket/bind/listen en "wayland-0", proceso B hace socket/connect("wayland-0"); A hace accept() y obtiene un fd; A escribe en ese fd, B lee en el suyo y ve los mismos bytes (y viceversa).

---

## Fase 2: Crate mínimo wayland-rs (userspace)

**Objetivo:** Servidor que crea un `Display`, registra un global (p. ej. `wl_compositor`), escucha en una ruta usando los sockets de Eclipse (socket/bind/listen/accept vía eclipse-relibc) y acepta una conexión.

**Pasos:**

1. Crear un crate (ej. `eclipse-apps/eclipse_wayland` o módulo en `smithay_app`):
   ```toml
   [dependencies]
   wayland-server = { version = "0.31", default-features = false }
   ```
2. Inicializar `Display`, registrar un global (compositor), sin lógica aún.
3. En lugar de `ListeningSocket::bind()` de Smithay, usar:
   - `socket(AF_UNIX, SOCK_STREAM, 0)` → fd
   - `bind(fd, path)` (path = "wayland-0" o "/var/run/wayland-0" según lo que el kernel acepte)
   - `listen(fd, backlog)`
   - En el bucle: `accept(fd)` → cuando haya conexión, envolver el fd en algo que wayland-server pueda usar como `Stream` (si wayland-rs acepta un tipo que implemente Read/Write, usar ese wrapper).
4. wayland-rs suele esperar un tipo que implemente `AsFd`/`AsRawFd` o un stream; habrá que ver la API exacta de 0.31 para “insertar” un cliente desde un fd. Si hace falta, un pequeño shim que convierta tu fd de Eclipse en el tipo que pida wayland-server.

**Criterio de éxito:** El servidor arranca, hace listen en una ruta, y cuando un cliente (p. ej. `wayland-scanner` + cliente mínimo o weston-terminal si lo portas) hace connect(), el servidor hace accept() y wayland-server recibe la conexión (sin fallar al hacer dispatch).

---

## Fase 3: Event loop y wl_shm

- Integrar el `Display` en un bucle de eventos (calloop si compila con tu target, o bucle propio que llame a `display.dispatch_clients()` / `flush_clients()` y mezcle con input/framebuffer).
- Implementar wl_shm (pools de memoria compartida). En Eclipse puede ser un archivo en un tmpfs o un scheme `shm:` que devuelva un fd mapeable; wayland-rs/wayland-shm te dan la parte protocolo.

---

## Por dónde empezar (recomendación)

1. **Empezar por Fase 1** (sockets en el kernel). Sin conexiones y read/write reales, wayland-rs no puede recibir clientes.
2. **Primer hito en Fase 1:**  
   - Añadir en `servers.rs` la noción de **Connection** (dos buffers o una cola) y una cola de **pending connections** por listener.  
   - En `connect()`: crear Connection, encolar en el listener, asociar el socket del cliente a esa conexión.  
   - En `accept()`: si hay pending, sacar, crear fd “servidor” para esa conexión, devolverlo.  
   - En `read`/`write` del scheme: si el resource es un extremo de conexión, leer/escribir en los buffers de esa conexión.

Cuando tengas un test userspace que pase bytes entre dos procesos por ese socket, Fase 2 (wayland-rs mínimo) es “solo” usar ese mismo fd desde el compositor y conectarlo al `Display`.
