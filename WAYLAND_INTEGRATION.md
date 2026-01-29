# Wayland Integration Guide for Eclipse OS

This document explains how to use libwayland and wlroots with Eclipse OS's Wayland infrastructure.

## Overview

Eclipse OS now supports three modes of Wayland operation:

1. **wlroots mode** (preferred) - Uses the wlroots compositor library
2. **libwayland mode** - Uses standard libwayland-server
3. **Custom mode** - Uses Eclipse OS's custom Wayland implementation

The integration layer automatically detects and uses the best available option.

## Installation

### Install Required Dependencies

```bash
# Install libwayland development files
sudo apt-get install libwayland-dev

# Install wlroots (if available for your distribution)
# For Ubuntu 22.04 and newer:
sudo apt-get install libwlroots-dev

# Or build wlroots from source:
git clone https://gitlab.freedesktop.org/wlroots/wlroots
cd wlroots
meson build
ninja -C build
sudo ninja -C build install
```

### Verify Installation

```bash
# Check if libraries are detected
pkg-config --modversion wayland-server
pkg-config --modversion wlroots
```

## Building

### Build Wayland Integration Library

```bash
cd userland/wayland_integration
cargo build --release
```

The build script will automatically detect available libraries and configure features accordingly.

### Build Wayland Compositor

The compositor Makefile now supports automatic library detection:

```bash
cd userland/wayland_compositor
make

# The build output will show which backend is being used:
# "Found wlroots - building with wlroots support"
# or
# "Found wayland-server - building with libwayland support"
# or
# "Building with custom implementation - no system libraries"
```

### Build Wayland Server

```bash
cd userland/wayland_server
cargo build --release --features use_wayland_integration
```

## Usage

### Using the Integration Library in Your Code

```rust
use wayland_integration::{Server, Compositor, get_info};

fn main() {
    // Get information about available backends
    let info = get_info();
    println!("Wayland backend: {}", info.backend);
    println!("Version: {}", info.version);
    
    // Create server and compositor
    let mut server = Server::new().expect("Failed to create server");
    let mut compositor = Compositor::new().expect("Failed to create compositor");
    
    // Add socket
    let socket = server.add_socket().expect("Failed to add socket");
    println!("Listening on: {}", socket);
    
    // Initialize compositor with server display
    let display_ptr = server.get_display_ptr();
    compositor.init(display_ptr).expect("Failed to init compositor");
    
    // Start compositor backend
    compositor.start().expect("Failed to start compositor");
    
    // Run server event loop
    server.run();
}
```

### Checking Which Backend is Active

```rust
use wayland_integration;

fn check_backend() {
    if wayland_integration::HAS_WLROOTS {
        println!("Running with wlroots backend");
    } else if wayland_integration::HAS_LIBWAYLAND {
        println!("Running with libwayland backend");
    } else {
        println!("Running with custom Eclipse OS backend");
    }
}
```

## Build Script Integration

The `build.sh` script in the root directory automatically detects and uses the appropriate Wayland libraries:

```bash
./build.sh
```

The script will:
1. Build the wayland_integration library
2. Detect available system libraries
3. Build the compositor with the best available backend
4. Build the server with appropriate features
5. Build Wayland applications

## Architecture

```
Eclipse OS Wayland Stack
┌─────────────────────────────────────┐
│    Wayland Applications             │
│  (calculator, terminal, editor)     │
├─────────────────────────────────────┤
│   COSMIC Desktop Client             │
│  (desktop environment)              │
├─────────────────────────────────────┤
│  Wayland Integration Layer          │
│  - Auto-detects libraries           │
│  - Provides unified API             │
│  ├── wlroots backend                │
│  ├── libwayland backend             │
│  └── custom backend                 │
├─────────────────────────────────────┤
│  System Libraries (if available)    │
│  - libwayland-server                │
│  - libwayland-client                │
│  - wlroots                          │
└─────────────────────────────────────┘
```

## Testing

### Test Library Detection

```bash
cd userland/wayland_integration
cargo build --verbose 2>&1 | grep -i "wayland\|wlroots"
```

### Test Compositor Build

```bash
cd userland/wayland_compositor
make clean
make
./wayland_compositor_wlroots  # if wlroots was detected
# or
./wayland_compositor_wayland   # if only libwayland was detected
# or
./wayland_compositor          # custom implementation
```

### Run Integration Tests

```bash
# Build all components
cd /path/to/eclipse
./build.sh

# The build script will show which libraries were detected
```

## Troubleshooting

### Libraries Not Detected

**Problem**: Build warnings show libraries not found.

**Solution**: Install the development packages:
```bash
sudo apt-get install libwayland-dev libwlroots-dev pkg-config
```

### Linking Errors with wlroots

**Problem**: Linker errors when building with wlroots.

**Solution**: Make sure wlroots is properly installed:
```bash
# Check if wlroots is in pkg-config path
pkg-config --libs wlroots

# If not found, you may need to update PKG_CONFIG_PATH
export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:$PKG_CONFIG_PATH
```

### Custom Implementation Works But Libraries Don't

**Problem**: Custom implementation builds fine but library-based builds fail.

**Solution**: This is expected behavior. The integration layer provides fallback to custom implementation when system libraries are not available. You can continue using the custom implementation or install the required libraries.

## Performance Comparison

| Backend | Performance | Features | Hardware Acceleration |
|---------|------------|----------|----------------------|
| wlroots | Best | Full | Yes |
| libwayland | Good | Core | Limited |
| Custom | Basic | Minimal | No |

## Feature Matrix

| Feature | wlroots | libwayland | Custom |
|---------|---------|------------|--------|
| Display server | ✓ | ✓ | ✓ |
| Compositor | ✓ | Partial | Basic |
| Multi-output | ✓ | ✓ | ✗ |
| Hardware rendering | ✓ | ✗ | ✗ |
| Input handling | ✓ | ✓ | Basic |
| XDG shell | ✓ | ✓ | ✗ |

## Migration Guide

### From Custom Implementation to libwayland

If you're currently using the custom Wayland implementation:

1. Install libwayland-dev:
   ```bash
   sudo apt-get install libwayland-dev
   ```

2. Rebuild:
   ```bash
   cd userland/wayland_integration
   cargo clean
   cargo build --release
   ```

3. The build script will automatically detect and use libwayland.

### From libwayland to wlroots

If you want to upgrade from libwayland to wlroots:

1. Install wlroots:
   ```bash
   sudo apt-get install libwlroots-dev
   # or build from source
   ```

2. Rebuild:
   ```bash
   cd userland/wayland_integration
   cargo clean
   cargo build --release
   ```

3. The build script will automatically prefer wlroots over libwayland.

## References

- [Wayland Documentation](https://wayland.freedesktop.org/)
- [libwayland API](https://wayland.freedesktop.org/docs/html/)
- [wlroots Documentation](https://gitlab.freedesktop.org/wlroots/wlroots)
- [Eclipse OS README](/README.md)

## Contributing

When adding new Wayland features:

1. Add FFI bindings to `wayland_integration/src/bindings/`
2. Create safe wrappers in appropriate modules
3. Provide fallback for custom implementation
4. Update documentation
5. Test with all three backends

## License

Part of Eclipse OS project.
