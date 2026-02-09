# Smithay App - Xwayland Compositor for Eclipse OS

## Overview

Smithay App is a Wayland compositor with Xwayland support designed specifically for Eclipse OS. It provides a graphical environment that supports both Wayland and X11 applications using the native Eclipse OS IPC system and direct framebuffer access via `/dev/fb0`.

> **📘 Technical Documentation**: For detailed information about the `no_std` and `no_main` configuration and how smithay_app is loaded by initd, see [TECHNICAL.md](TECHNICAL.md).

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

The application is built using Rust nightly with the `x86_64-unknown-none` target:

```bash
cd eclipse-apps/smithay_app
cargo +nightly build --release --target x86_64-unknown-none -Zbuild-std=core,alloc
```

The resulting binary is located at:
```
target/x86_64-unknown-none/release/smithay_app
```

## Running

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

## Dependencies

- `eclipse-libc`: Eclipse OS standard library providing syscall wrappers

## System Requirements

- Eclipse OS kernel with framebuffer support
- Display hardware with framebuffer device (`/dev/fb0`)
- IPC subsystem initialized

## Future Enhancements

- Full Wayland protocol implementation
- Window management (compositing, stacking, focus)
- Input event handling (keyboard, mouse)
- Multiple display support
- 3D acceleration via DRI
- Client connection management
- Damage tracking and efficient rendering

## License

Part of the Eclipse OS project.

## Author

Implemented for Eclipse OS by the Eclipse OS team.
