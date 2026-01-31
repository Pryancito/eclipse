# Eclipse-SystemD

Modern init system (PID 1) for Eclipse OS Microkernel.

## Overview

Eclipse-SystemD is a lightweight, efficient init system designed specifically for the Eclipse OS microkernel architecture. It provides service management, dependency tracking, and integration with the microkernel's IPC system.

## Features

- **Service Management**: Start, stop, and monitor system services
- **Dependency Tracking**: Automatically resolve and respect service dependencies
- **Restart Policies**: Configurable restart behavior for failed services
- **Parallel Startup**: Start independent services concurrently for faster boot
- **Health Monitoring**: Continuous monitoring of service health
- **Zombie Reaping**: Automatic cleanup of terminated child processes
- **Priority-Based Scheduling**: Services can be prioritized for startup order

## Service Types

Eclipse-SystemD supports multiple service types:

- **Simple**: Long-running service that doesn't fork
- **Forking**: Service that forks into background
- **OneShot**: Service that runs once and exits
- **Notify**: Service that signals when ready

## Restart Policies

Services can have different restart policies:

- **No**: Never restart the service
- **OnFailure**: Restart only if service exits with error
- **Always**: Always restart the service
- **OnAbnormal**: Restart on abnormal termination

## Built-in Services

The following services are registered by default:

1. **filesystem.service** - EclipseFS filesystem server
2. **network.service** - Network stack service
3. **display.service** - Display and graphics server
4. **audio.service** - Audio playback and capture
5. **input.service** - Input device management

## Boot Phases

Eclipse-SystemD follows a structured boot process:

### Phase 1: Early Boot Initialization
- Process environment setup
- Signal handler initialization

### Phase 2: System Initialization
- Filesystem mounting
- Virtual filesystem setup (/proc, /sys, /dev)

### Phase 3: Service Startup
- Start services with no dependencies
- Start dependent services in priority order

### Phase 4: Main Loop
- Service monitoring
- Zombie process reaping
- Service restart handling
- Health checks

## Building

```bash
cd userland/systemd
cargo build --release
```

The binary will be built for the `x86_64-unknown-none` target using the custom target specification.

## Integration with Microkernel

Eclipse-SystemD integrates with the Eclipse microkernel through:

- **Syscalls**: Uses the Eclipse syscall interface for process management
- **IPC**: Will communicate with services via microkernel IPC
- **Process Management**: Uses fork/exec for service spawning

## Dependencies

- `eclipse_libc`: Eclipse OS userspace C library providing syscall wrappers

## Architecture

Eclipse-SystemD follows a modular architecture:

```
┌─────────────────────────────────────┐
│      Eclipse-SystemD (PID 1)        │
├─────────────────────────────────────┤
│  Service Registry                   │
│  Dependency Tracker                 │
│  Process Monitor                    │
│  Restart Manager                    │
└─────────────────────────────────────┘
         │         │         │
         ▼         ▼         ▼
    ┌────────┐ ┌────────┐ ┌────────┐
    │FS Srv  │ │Net Srv │ │Disp Srv│
    └────────┘ └────────┘ └────────┘
```

## Future Enhancements

- Socket activation for on-demand service startup
- Service unit file parsing (systemd-compatible)
- Cgroup-like resource management
- Logging service integration
- Boot time optimization
- Service templates
- Timer-based service activation

## License

Part of Eclipse OS project.
