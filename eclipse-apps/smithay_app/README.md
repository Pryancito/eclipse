# Smithay App - Compositor para Eclipse OS

## Overview

Smithay App es **un solo binario** (como xfwl4) con dos backends según el target de compilación:

- **Target Linux (host)**: compositor Wayland con la librería [Smithay](https://github.com/Smithay/smithay) + winit (OpenGL). Útil para desarrollar y probar clientes Wayland.
- **Target Eclipse**: compositor propio (embedded-graphics, SideWind, DRM/KMS, IPC) para Eclipse OS.

Un mismo crate, un mismo nombre de ejecutable; el backend se elige por `--target`.

> **📘 Technical Documentation**: For detailed information about the `no_std` and `no_main` configuration and how smithay_app is loaded by initd, see [TECHNICAL.md](TECHNICAL.md).
>
> **📐 Design Reference**: smithay_app se basa en [xfwl4](../xfwl4) como referencia de diseño. Ver [XFWL4_REFERENCE.md](XFWL4_REFERENCE.md) para patrones portables (focus, ciclado, keybindings).

## Features

- **Direct Framebuffer Access**: Uses framebuffer syscalls for direct memory-mapped graphics rendering (conceptually /dev/fb0)
- **Eclipse OS IPC Integration**: Native IPC communication for inter-process messaging
- **Xwayland Support**: Provides X11 compatibility layer for legacy X applications
- **X11 Socket Management**: Creates and manages X11 Unix domain socket at `/tmp/.X11-unix/X0`
- **Framebuffer Operations**: Supports clearing, drawing, and rendering to the framebuffer

## Architecture

### Framebuffer Backend

The compositor accesses the framebuffer through Eclipse OS syscalls (rather than directly opening /dev/fb0):

1. **SYS_GET_FRAMEBUFFER_INFO (15)**: Retrieves framebuffer dimensions, pitch, and pixel format
2. **SYS_MAP_FRAMEBUFFER (16)**: Maps the framebuffer into the process's virtual address space

The framebuffer is accessed as a memory-mapped region, allowing efficient direct pixel manipulation.

### IPC Communication

The compositor uses Eclipse OS's native IPC system for communication:

- **MSG_TYPE_GRAPHICS (0x10)**: Graphics and rendering messages
- **MSG_TYPE_INPUT (0x40)**: Input device events (keyboard, mouse)
- **MSG_TYPE_SIGNAL (0x400)**: Signal and control messages

Messages are sent and received using the `send()` and `receive()` syscalls.

### Xwayland Integration

The compositor provides X11 compatibility through:

1. **X11 Socket Creation**: Creates `/tmp/.X11-unix/X0` for X client connections
2. **X Window Manager (XWM)**: Manages X11 windows within the Wayland compositor
3. **Protocol Translation**: Translates between X11 and Wayland protocols

## Building

Un mismo binario, dos formas de compilar:

**Compositor Wayland (Smithay) en Linux (host):**
```bash
cd eclipse-apps
cargo build -p smithay_app --target x86_64-unknown-linux-gnu --release
# Ejecutable: target/x86_64-unknown-linux-gnu/release/smithay_app
# Ejecutar con: ./smithay_app/run_linux.sh  (evita ENOMEM en sigaltstack)
# Socket Wayland: wayland-5 (ej. WAYLAND_DISPLAY=wayland-5 weston-terminal)
```

**Compositor Eclipse (DRM, SideWind) para Eclipse OS:**
```bash
cd eclipse-apps
./smithay_app/build_eclipse.sh
# Binario: target/x86_64-unknown-linux-musl/release/smithay_app
```

O manualmente (usa **core,alloc** — NUNCA std — porque x86_64-unknown-linux-musl tiene os="none"):
```bash
# 1. eclipse-relibc primero
cd eclipse-relibc
cargo +nightly build --release --target ../x86_64-unknown-linux-musl.json -Z unstable-options -Z build-std=core,alloc
cp target/x86_64-unknown-linux-musl/release/libeclipse_libc.rlib target/x86_64-unknown-linux-musl/release/libc.a

# 2. smithay_app
cd ../eclipse-apps
cargo +nightly build -p smithay_app --target ../x86_64-unknown-linux-musl.json -Z unstable-options -Z build-std=core,alloc --release
```
El script `build.sh` en la raíz del repo automatiza estos pasos para la build completa.

## Running

### Linux (compositor Wayland / Smithay)

Si aparece `failed to set up alternative stack guard page: Cannot allocate memory (os error 12)`, es un límite de memoria del sistema. Usa el script:

```bash
cd eclipse-apps
cargo build -p smithay_app --target x86_64-unknown-linux-gnu --release
./smithay_app/run_linux.sh target/x86_64-unknown-linux-gnu/release/smithay_app
```

O antes de ejecutar manualmente:
```bash
ulimit -v unlimited
ulimit -s 65536
./target/x86_64-unknown-linux-gnu/release/smithay_app
```

### Eclipse OS

The compositor is typically launched by the `gui_service` during system initialization. It can also be started manually:

```bash
/usr/bin/smithay_app
```

Upon startup, the compositor will:

1. Initialize framebuffer access
2. Clear the screen to a dark gray background
3. Draw a test gradient pattern
4. Create the X11 socket
5. Enter the main event loop

## Output

When running, the compositor displays status information:

```
╔══════════════════════════════════════════════════════════════╗
║         SMITHAY XWAYLAND COMPOSITOR v0.2.0                   ║
║         Using Eclipse OS IPC and /dev/fb0                    ║
╚══════════════════════════════════════════════════════════════╝
[SMITHAY] Starting (PID: X)
[SMITHAY] Initializing graphics backend...
[SMITHAY]   - Framebuffer: WIDTHxHEIGHT @ BPP bpp
[SMITHAY]   - Framebuffer mapped at address: 0xXXXXXXXX
[SMITHAY]   - Framebuffer backend ready
[SMITHAY]   - Clearing framebuffer to color: 0xFF1A1A1A
[SMITHAY]   - Drawing test pattern...
[SMITHAY] Initializing Xwayland integration...
[SMITHAY]   - Socket path: /tmp/.X11-unix/X0
[SMITHAY]   - X11 socket created successfully
[SMITHAY]   - X Window Manager (XWM) started
[SMITHAY]   - Xwayland ready for X11 clients
[SMITHAY] Initializing IPC communication...
[SMITHAY]   - IPC handler ready
[SMITHAY] Compositor ready and running
[SMITHAY] Display: WIDTHxHEIGHT @ BPP bpp
[SMITHAY] Waiting for Wayland and X11 clients...
[SMITHAY] [Status] Active | Messages: X | Wayland: 0 | X11: 0
```

## Troubleshooting

**Error: `none of the predicates in this cfg_select evaluated to true` (en std/src/sys/alloc/mod.rs):**  
Estás usando `-Z build-std=std` o similar. Para Eclipse OS **no** debes compilar el `std` real de Rust; `x86_64-unknown-linux-musl` tiene `os: "none"` y la std no soporta bare-metal. Usa solo `-Z build-std=core,alloc` y deja que `eclipse_std` reemplace a `std` (ya está configurado en Cargo.toml).

## Dependencies

- `eclipse_std` / `eclipse-libc`: standard library y syscalls de Eclipse OS
- `eclipse_ipc`, `sidewind`, `embedded-graphics`: compositor actual
- `smithay` (opcional, feature `smithay`): reservado para una futura integración del compositor Wayland real

## System Requirements

- Eclipse OS kernel with framebuffer support
- Display hardware with framebuffer device (`/dev/fb0`)
- IPC subsystem initialized

## Future Enhancements

- **Integrar la librería Smithay** como backend Wayland real (hoy es compositor propio).
- Full Wayland protocol implementation
- Xwayland / X11 compatibility layer
- Multiple display support
- 3D acceleration via DRI
- Client connection management
- Damage tracking and efficient rendering

## License

Part of the Eclipse OS project.

## Author

Implemented for Eclipse OS by the Eclipse OS team.
