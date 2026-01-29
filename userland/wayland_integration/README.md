# Wayland Integration Layer

A Rust library providing integration with `libwayland` and `wlroots` for Eclipse OS, with automatic fallback to custom implementation.

## Overview

This library provides a unified API for Wayland functionality that can use:
1. **libwayland** - The standard Wayland client and server libraries
2. **wlroots** - A modular Wayland compositor library
3. **Custom implementation** - Fallback to Eclipse OS's custom Wayland implementation

## Features

- ✅ Automatic library detection via `pkg-config`
- ✅ Compile-time feature detection
- ✅ FFI bindings for `libwayland-server` and `wlroots`
- ✅ Safe Rust wrappers around C APIs
- ✅ Seamless fallback to custom implementation
- ✅ No runtime dependencies if libraries are not available
- ✅ Support for both `std` and `no_std` environments

## Dependencies

### System Libraries (Optional)

The following system libraries are optional. If they're not found, the library will use Eclipse OS's custom Wayland implementation:

```bash
# Install libwayland development files
sudo apt-get install libwayland-dev

# Install wlroots development files (if available for your distribution)
sudo apt-get install libwlroots-dev

# Or build from source
git clone https://gitlab.freedesktop.org/wlroots/wlroots
cd wlroots
meson build
ninja -C build
sudo ninja -C build install
```

### Rust Dependencies

These are automatically managed by Cargo:
- `pkg-config` (build dependency) - For detecting system libraries
- `heapless` - For no_std collections
- `linked_list_allocator` - For memory allocation

## Building

```bash
cd userland/wayland_integration
cargo build --release
```

The build script will automatically detect available libraries and configure features:

- If `libwayland-server` is found → enables `has_libwayland` cfg flag
- If `wlroots` is found → enables `has_wlroots` cfg flag
- If neither is found → uses custom implementation

## Features

### Default Features
- `libwayland` - Enable libwayland integration
- `wlroots` - Enable wlroots integration

### Optional Features
- `std` - Enable standard library support (for userland)

## Usage

### Basic Server Example

```rust
use wayland_integration::{Server, Compositor, get_info};

fn main() {
    // Get information about available backends
    let info = get_info();
    println!("Wayland backend: {}", info.backend);
    println!("Has libwayland: {}", info.has_libwayland);
    println!("Has wlroots: {}", info.has_wlroots);
    
    // Create a server
    let mut server = Server::new().expect("Failed to create server");
    
    // Add socket
    let socket = server.add_socket().expect("Failed to add socket");
    println!("Listening on socket: {}", socket);
    
    // Create compositor
    let mut compositor = Compositor::new().expect("Failed to create compositor");
    
    // Initialize compositor with server display
    compositor.init(server.get_display_ptr()).expect("Failed to init compositor");
    
    // Start compositor backend
    compositor.start().expect("Failed to start compositor");
    
    // Run server event loop
    server.run();
}
```

### Checking Available Features

```rust
use wayland_integration;

fn main() {
    if wayland_integration::HAS_LIBWAYLAND {
        println!("Using libwayland");
    } else if wayland_integration::HAS_WLROOTS {
        println!("Using wlroots");
    } else {
        println!("Using custom implementation");
    }
}
```

## Architecture

```
wayland_integration/
├── src/
│   ├── lib.rs              # Main library entry point
│   ├── bindings/           # FFI bindings
│   │   ├── mod.rs         # Common types and errors
│   │   ├── libwayland.rs  # libwayland bindings
│   │   └── wlroots.rs     # wlroots bindings
│   ├── compositor.rs       # High-level compositor API
│   ├── server.rs          # High-level server API
│   └── protocol.rs        # Wayland protocol definitions
├── build.rs               # Build script for library detection
└── Cargo.toml            # Package configuration
```

## Integration with Eclipse OS

This library is designed to integrate with:

- **wayland_server** (`userland/wayland_server`) - Rust Wayland server
- **wayland_compositor** (`userland/wayland_compositor`) - C Wayland compositor
- **wayland_apps** - Wayland client applications
- **cosmic_client** - COSMIC desktop client

## Build Script Details

The `build.rs` script uses `pkg-config` to detect system libraries:

1. Checks for `wayland-server` >= 1.18.0
2. Checks for `wayland-client`
3. Checks for `wlroots` >= 0.16.0
4. Sets appropriate `cfg` flags for conditional compilation
5. Prints warnings if libraries are not found

## Platform Support

- **Linux**: Full support with system libraries
- **Other Unix-like**: Partial support (custom implementation)
- **Eclipse OS**: Full support with custom implementation

## Troubleshooting

### libwayland not found

```
warning: libwayland-server not found. Using fallback implementation.
Install with: sudo apt-get install libwayland-dev
```

**Solution**: Install the development package:
```bash
sudo apt-get install libwayland-dev
```

### wlroots not found

```
warning: wlroots not found. Using fallback implementation.
Install with: sudo apt-get install libwlroots-dev
```

**Solution**: wlroots may not be packaged for all distributions. Build from source:
```bash
git clone https://gitlab.freedesktop.org/wlroots/wlroots
cd wlroots
meson build
ninja -C build
sudo ninja -C build install
```

### pkg-config not found

**Solution**: Install pkg-config:
```bash
sudo apt-get install pkg-config
```

## Testing

```bash
# Run tests
cargo test

# Run with verbose output to see library detection
cargo build --verbose

# Test without libwayland
cargo build --no-default-features --features wlroots

# Test without wlroots
cargo build --no-default-features --features libwayland

# Test with custom implementation only
cargo build --no-default-features
```

## Contributing

When adding new Wayland functionality:

1. Add FFI declarations to `bindings/` modules
2. Create safe wrappers in `compositor.rs` or `server.rs`
3. Provide fallback implementation for custom backend
4. Update documentation

## License

Part of Eclipse OS project.

## See Also

- [libwayland documentation](https://wayland.freedesktop.org/docs/html/)
- [wlroots documentation](https://gitlab.freedesktop.org/wlroots/wlroots)
- [Wayland protocol specification](https://wayland.freedesktop.org/docs/html/apa.html)
