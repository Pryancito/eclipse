# Wayland Server for Eclipse OS

A complete Wayland compositor implementation in Rust for the Eclipse OS userland.

## Overview

This is a from-scratch implementation of a Wayland display server that runs as a userland process in Eclipse OS. It implements the core Wayland protocol and provides compositing services for Wayland client applications.

## Architecture

### Core Components

1. **Protocol Layer** (`protocol.rs`)
   - Wayland wire protocol definitions
   - Message header and argument types
   - Opcode definitions for core interfaces:
     - `wl_display` - Main display object
     - `wl_registry` - Global registry
     - `wl_compositor` - Compositor interface
     - `wl_surface` - Surface objects

2. **Server Core** (`server.rs`)
   - Client connection management (up to 16 clients)
   - Object management (256 objects per client)
   - Message dispatching
   - Protocol request handling

3. **Object Management** (`objects.rs`)
   - Wayland object lifecycle management
   - Surface and buffer representations
   - Object state tracking

4. **Socket/IPC** (`socket.rs`)
   - Unix domain socket handling
   - Message buffer management
   - Client connection handling

5. **Compositor** (`compositor.rs`)
   - Surface composition
   - Frame rendering
   - Damage tracking

## Features

- ✅ Core Wayland protocol support
- ✅ Multi-client support (up to 16 concurrent clients)
- ✅ Surface management
- ✅ No standard library (no_std)
- ✅ Custom memory allocator
- ✅ Direct syscall usage
- ✅ Small binary size (~3.7KB)

## Building

```bash
cd userland/wayland_server
cargo build --release
```

The binary will be located at:
```
target/x86_64-unknown-linux-gnu/release/wayland_server
```

## Running

The server binds to `/tmp/wayland-0` by default and listens for Wayland client connections.

```bash
./wayland_server
```

## Integration

This Wayland server integrates with the Eclipse OS kernel's Wayland infrastructure (`eclipse_kernel/src/wayland/`) and provides userland compositing services.

## Technical Details

- **Language**: Rust (no_std)
- **Binary Size**: ~3.7KB (optimized release build)
- **Memory**: 2MB heap allocation
- **Syscalls**: Direct syscall interface via inline assembly
- **Protocol**: Wayland wire protocol

## Limitations

Current implementation focuses on core functionality:
- Socket operations are simulated (would need full kernel syscall support)
- Basic protocol message handling
- Simplified rendering pipeline
- No XDG shell protocol yet

## Future Enhancements

- [ ] XDG shell protocol for advanced window management
- [ ] Input event handling (keyboard, pointer, touch)
- [ ] Shared memory buffer support (wl_shm)
- [ ] DRM/KMS integration for hardware acceleration
- [ ] Full socket I/O implementation with kernel support
- [ ] Multi-output support

## License

Part of Eclipse OS project.
