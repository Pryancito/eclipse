# Eclipse SystemD Orchestrator Implementation

## Overview
This document describes the implementation of the Eclipse SystemD orchestrator (init system) that manages the boot sequence and service dependencies for Eclipse OS.

## Service Launch Order

According to the requirements, the orchestrator launches services in the following order:

### 1. Log Server / Console (Priority 10)
- **Service**: `log_service`
- **Location**: `eclipse_kernel/userspace/log_service/`
- **Purpose**: Central logging service for debugging
- **Dependencies**: None (must be first)
- **Why First**: All other services need a place to send error messages and logs for debugging

### 2. Device Manager - devfs (Priority 9)
- **Service**: `devfs_service`
- **Location**: `eclipse_kernel/userspace/devfs_service/`
- **Purpose**: Creates and manages device nodes in /dev
- **Dependencies**: Log service
- **Devices Created**:
  - /dev/null - Null device
  - /dev/zero - Zero device
  - /dev/random - Random number generator
  - /dev/console - System console
  - /dev/tty - Terminal devices
  - /dev/fb0 - Framebuffer device
  - /dev/input/* - Input devices

### 3. Input Server (Priority 8)
- **Service**: `input_service`
- **Location**: `eclipse_kernel/userspace/input_service/`
- **Purpose**: Manages keyboard and mouse interrupts
- **Dependencies**: Log service + devfs
- **Why Third**: Needs device nodes from devfs to access hardware

### 4. Graphics Server / Display Server (Priority 7)
- **Service**: `display_service`
- **Location**: `eclipse_kernel/userspace/display_service/`
- **Purpose**: Initializes video buffer and manages graphics
- **Dependencies**: Log service + devfs + Input server
- **Why Fourth**: Depends on Input Server being ready to capture events

### 5. Network Server (Priority 6)
- **Service**: `network_service`
- **Location**: `eclipse_kernel/userspace/network_service/`
- **Purpose**: Network stack service
- **Dependencies**: Log service + devfs + Input server
- **Why Last**: Most complex and error-prone service

## Implementation Details

### Orchestrator Location
- **Main Binary**: `eclipse-apps/systemd/`
- **Target**: `/sbin/eclipse-systemd`
- **Build**: `cargo +nightly build --release --target x86_64-unknown-none`

### Service Registry
Services are registered in the `init_services()` function with:
- Name and description
- Service type (Simple, Forking, OneShot, Notify)
- Restart policy (No, OnFailure, Always, OnAbnormal)
- Priority level (higher number = starts earlier)
- Dependencies (array of service indices)

### Dependency Management
The orchestrator:
1. Starts services with no dependencies first
2. Waits for initialization delay
3. Checks dependencies before starting dependent services
4. Monitors service health and handles failures
5. Implements automatic restart policies

### Service States
- **Inactive**: Service not started
- **Activating**: Service starting up
- **Active**: Service running normally
- **Deactivating**: Service shutting down
- **Failed**: Service failed
- **Restarting**: Service being restarted

## Build Configuration

### New Services
Both `log_service` and `devfs_service` include:
- `Cargo.toml` - Package configuration
- `.cargo/config.toml` - Build target configuration
- `linker.ld` - Linker script
- `src/main.rs` - Service implementation

### Build Command
```bash
cd eclipse_kernel/userspace/<service_name>
cargo +nightly build --release
```

Binary output: `target/x86_64-unknown-none/release/<service_name>`

## Integration with Eclipse OS

### Boot Sequence
1. Kernel boots
2. Kernel starts init process (PID 1)
3. Init process is eclipse-systemd
4. SystemD orchestrates service startup in dependency order
5. SystemD enters main loop for service monitoring

### Service Spawning
Services are spawned using:
1. `fork()` - Create new process
2. `get_service_binary(id)` - Get service binary from kernel
3. `exec()` - Execute service binary

### Service Monitoring
The main loop:
- Reaps zombie processes
- Monitors service health
- Restarts failed services (up to max attempts)
- Prints heartbeat and status updates

## Files Modified

### New Services Created
- `eclipse_kernel/userspace/log_service/`
- `eclipse_kernel/userspace/devfs_service/`

### SystemD Orchestrator Updated
- `eclipse-apps/systemd/src/main.rs` - Service order and dependencies
- `eclipse-apps/systemd/.cargo/config.toml` - Build configuration

### Init Process Updated
- `eclipse_kernel/userspace/init/src/main.rs` - Service order alignment

### Configuration Files
- `userland/systemd/src/main.rs` - Kept in sync (not used in builds)

## Testing

All services build successfully:
- ✅ log_service: 11KB binary
- ✅ devfs_service: 11KB binary
- ✅ eclipse-systemd: 20KB binary

## Security

- No unsafe code blocks in new services
- All services use safe Rust syscall wrappers
- No unwrap() or panic!() calls
- Proper error handling throughout

## Future Enhancements

Potential improvements:
- Socket activation support
- Full microkernel IPC integration
- Service dependency visualization
- Service configuration files
- Dynamic service loading
- Service sandboxing and isolation
