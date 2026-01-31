# Eclipse-SystemD Kernel Integration Example

This example shows how to integrate eclipse-systemd as the init process for Eclipse OS.

## Method 1: Embed in Kernel (Current Approach)

### Step 1: Build eclipse-systemd

```bash
cd userland/systemd
./build.sh
```

### Step 2: Update kernel to use new systemd

Edit `eclipse_kernel/src/main.rs` and update the `INIT_BINARY` constant:

```rust
/// Init process binary embedded in kernel
/// Eclipse-SystemD - Modern init system for microkernel
pub static INIT_BINARY: &[u8] = include_bytes!("../../userland/systemd/target/x86_64-unknown-none/release/eclipse-systemd");
```

### Step 3: Rebuild kernel

```bash
cd eclipse_kernel
cargo +nightly build --target x86_64-unknown-none --release
```

### Step 4: Boot and verify

```bash
cd ..
./qemu.sh
```

Expected output:
```
╔════════════════════════════════════════════════════════════════╗
║           ECLIPSE-SYSTEMD v0.1.0 - Init System                ║
║              Modern Service Manager for Microkernel            ║
╚════════════════════════════════════════════════════════════════╝

Eclipse-SystemD starting with PID: 1
...
```

## Method 2: Load from Filesystem (Future)

When the filesystem is fully integrated:

1. Install systemd to filesystem:
```bash
mkdir -p /sbin
cp userland/systemd/target/x86_64-unknown-none/release/eclipse-systemd /sbin/
```

2. Kernel loads from `/sbin/eclipse-systemd` first, falls back to embedded binary if not found

## Differences from Current Init

### Current: eclipse_kernel/userspace/init
- Basic init process
- 11KB binary
- Simple service spawning
- TODO markers for future work

### New: userland/systemd/eclipse-systemd
- Full-featured init system
- 20KB binary
- Advanced service management:
  - Dependency tracking
  - Restart policies
  - Health monitoring
  - Zombie reaping
  - Priority-based startup
- Production-ready architecture
- Extensible for future features

## Migration Path

1. **Phase 1** (Current): Use embedded eclipse-systemd as PID 1
2. **Phase 2**: Load eclipse-systemd from `/sbin/eclipse-systemd` when filesystem available
3. **Phase 3**: Add systemd unit file support for configuration
4. **Phase 4**: Implement socket activation and advanced features

## Service Management

Eclipse-SystemD manages 5 core services:

1. **filesystem.service** - EclipseFS filesystem server (Priority 10)
2. **network.service** - Network stack (Priority 8, depends on FS)
3. **display.service** - Graphics/display server (Priority 9, depends on FS)
4. **audio.service** - Audio subsystem (Priority 7, depends on FS)
5. **input.service** - Input devices (Priority 9, depends on FS)

Services are started in dependency order with proper synchronization.

## Testing

### Verify Binary
```bash
cd userland/systemd
file target/x86_64-unknown-none/release/eclipse-systemd
```

Should output:
```
ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked, stripped
```

### Check Size
```bash
ls -lh target/x86_64-unknown-none/release/eclipse-systemd
```

Should be around 20KB.

### Verify Build
```bash
./build.sh
```

Should complete without errors.

## Next Steps

1. Integrate with main build.sh
2. Test boot with QEMU
3. Verify service spawning works with microkernel IPC
4. Add logging integration
5. Implement systemctl-like control interface

## Related Documentation

- `README.md` - SystemD user documentation
- `INTEGRATION.md` - Full integration guide
- `../../MICROKERNEL_ARCHITECTURE.md` - System architecture
- `../../INIT_IMPLEMENTATION.md` - Init system overview
