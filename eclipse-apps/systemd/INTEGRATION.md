# Eclipse-SystemD Integration Guide

## Overview

This document describes how to integrate the new `eclipse-systemd` init system with the Eclipse OS microkernel boot process.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│               Eclipse Microkernel                    │
│  (Handles Memory, IPC, Scheduling, Interrupts)       │
└─────────────────────────────────────────────────────┘
                        │
                        │ Launches PID 1
                        ▼
┌─────────────────────────────────────────────────────┐
│          Eclipse-SystemD (Init System)               │
│  - Service Management                                │
│  - Dependency Resolution                             │
│  - Health Monitoring                                 │
│  - Zombie Reaping                                    │
└─────────────────────────────────────────────────────┘
           │           │           │           │
           ▼           ▼           ▼           ▼
    ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐
    │   FS    │ │ Network │ │ Display │ │  Audio  │
    │ Server  │ │ Server  │ │ Server  │ │ Server  │
    └─────────┘ └─────────┘ └─────────┘ └─────────┘
```

## Boot Process

### 1. Kernel Boot
The Eclipse microkernel boots and initializes:
- Memory management
- Interrupt handling
- IPC system
- Basic scheduler

### 2. Init Launch
The kernel loads and executes the init process (eclipse-systemd):

**Option A: Embedded Binary**
```rust
// In kernel/src/main.rs
pub static INIT_BINARY: &[u8] = 
    include_bytes!("../../userland/systemd/target/x86_64-unknown-none/release/eclipse-systemd");
```

**Option B: From Filesystem** (preferred)
```rust
// Load from /sbin/eclipse-systemd on mounted filesystem
match filesystem::read_file("/sbin/eclipse-systemd", &mut buffer) {
    Ok(size) => load_elf(&buffer[..size]),
    Err(_) => fallback_to_embedded(),
}
```

### 3. SystemD Initialization
Eclipse-systemd performs these phases:

**Phase 1: Early Boot**
- Set up process environment
- Initialize signal handlers
- Prepare for service management

**Phase 2: System Init**
- Mount filesystems (/, /proc, /sys, /dev)
- Initialize logging
- Load configuration

**Phase 3: Service Startup**
- Start services with no dependencies (e.g., filesystem server)
- Wait for dependencies to be ready
- Start dependent services (network, display, audio, input)

**Phase 4: Main Loop**
- Monitor service health
- Restart failed services per policy
- Reap zombie processes
- Handle IPC messages

## Integration Steps

### Step 1: Build Eclipse-SystemD

```bash
cd userland/systemd
./build.sh
```

This produces: `target/x86_64-unknown-none/release/eclipse-systemd`

### Step 2: Option A - Embed in Kernel

Update `eclipse_kernel/src/main.rs`:

```rust
// Replace the current INIT_BINARY path
pub static INIT_BINARY: &[u8] = 
    include_bytes!("../../userland/systemd/target/x86_64-unknown-none/release/eclipse-systemd");
```

### Step 3: Option B - Install to Filesystem

```bash
# Create filesystem structure
mkdir -p /sbin
cp userland/systemd/target/x86_64-unknown-none/release/eclipse-systemd /sbin/

# Or use the filesystem creation tool
./mkfs-eclipsefs --add-file /sbin/eclipse-systemd=userland/systemd/target/...
```

### Step 4: Update Build System

Modify the main `build.sh` to include systemd:

```bash
# Build systemd
print_step "Building Eclipse-SystemD..."
cd userland/systemd
./build.sh
cd ../..
```

### Step 5: Rebuild and Test

```bash
# Full system rebuild
./build.sh

# Test in QEMU
./qemu.sh
```

## Expected Boot Output

When eclipse-systemd starts successfully, you should see:

```
╔════════════════════════════════════════════════════════════════╗
║           ECLIPSE-SYSTEMD v0.1.0 - Init System                ║
║              Modern Service Manager for Microkernel            ║
╚════════════════════════════════════════════════════════════════╝

Eclipse-SystemD starting with PID: 1

[INIT] Initializing service registry...
  [OK] Registered 5 services

[PHASE 1] Early boot initialization
  [EARLY] Setting up process environment
  [EARLY] Initializing signal handlers
  [EARLY] Early boot complete

[PHASE 2] System initialization
  [SYSTEM] Mounting filesystems
  [SYSTEM] Setting up /proc
  [SYSTEM] Setting up /sys
  [SYSTEM] Setting up /dev
  [SYSTEM] System initialization complete

[PHASE 3] Starting system services
  [START] Starting services with no dependencies...
  [START] filesystem.service - EclipseFS Filesystem Server
    [OK] filesystem.service started with PID 2

  [START] Starting dependent services...
  [START] network.service - Network Stack Service
    [OK] network.service started with PID 3
  ...

[PHASE 4] Entering main service manager loop
[READY] Eclipse-SystemD is ready

[HEARTBEAT #1] SystemD operational
```

## Troubleshooting

### SystemD doesn't start
- Verify the binary is correctly embedded or in /sbin/
- Check kernel logs for ELF loading errors
- Ensure x86_64-unknown-none target is installed

### Services fail to start
- Check that service binaries are available
- Verify dependencies are met
- Review restart policies and limits

### System hangs
- Enable verbose kernel logging
- Check for infinite loops in service startup
- Verify IPC communication is working

## Configuration

### Service Definition

Services are defined in `src/main.rs`:

```rust
add_service(Service::new(
    "service-name.service",
    "Service Description",
    ServiceType::Simple,
    RestartPolicy::OnFailure,
    10,  // Priority
    &[],  // Dependencies (indices)
));
```

### Restart Policies

- `RestartPolicy::No` - Never restart
- `RestartPolicy::OnFailure` - Restart on error exit only
- `RestartPolicy::Always` - Always restart
- `RestartPolicy::OnAbnormal` - Restart on abnormal termination

## Future Enhancements

1. **Unit File Support**
   - Parse systemd-compatible .service files
   - Load services from /etc/systemd/system/

2. **Socket Activation**
   - Start services on-demand when sockets are accessed
   - Reduce memory usage

3. **Resource Control**
   - CPU/memory limits per service
   - Cgroup-like functionality

4. **Logging Integration**
   - Unified logging service (journald-like)
   - Service output capture

5. **Runtime Control**
   - systemctl-like interface
   - Dynamic service management

## Related Files

- `userland/systemd/src/main.rs` - Main systemd implementation
- `userland/systemd/Cargo.toml` - Build configuration
- `userland/systemd/README.md` - User documentation
- `eclipse_kernel/src/main.rs` - Kernel init loading
- `eclipse_kernel/userspace/libc/` - Syscall interface

## References

- Eclipse OS Architecture: `MICROKERNEL_ARCHITECTURE.md`
- Init Implementation: `INIT_IMPLEMENTATION.md`
- Build Guide: `BUILD_GUIDE.md`
