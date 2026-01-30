# Eclipse S6 v0.1.0

S6 supervision suite integration for Eclipse OS - Perfect modular systems engineering.

## What is S6?

S6 is a small, secure, and reliable supervision suite designed by Laurent Bercot. It provides:

- **Process supervision**: Automatic restart of failed services
- **Dependency management**: Service ordering and dependencies via s6-rc
- **Logging**: Reliable logging with s6-log and automatic rotation
- **Minimal footprint**: ~200KB vs systemd's ~10MB
- **Modularity**: Each component does one thing well
- **Reliability**: Designed for perfect systems engineering

## Why S6 over systemd?

Eclipse OS chose S6 for several reasons:

1. **Simplicity**: S6 follows the Unix philosophy - small, focused tools
2. **Modularity**: Perfect separation of concerns
3. **Size**: Tiny footprint ideal for embedded and minimal systems
4. **Reliability**: Designed for 24/7 uptime with no compromise
5. **Perfect Engineering**: Every aspect is carefully designed and tested

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Eclipse OS with S6                   │
├─────────────────────────────────────────────────────────┤
│  /sbin/init (→ /sbin/eclipse-s6)                       │
│  ├── S6 Init System (PID 1)                            │
│  │   ├── Initialize directories                        │
│  │   ├── Set up environment                            │
│  │   └── Start supervision tree                        │
│  │                                                      │
│  ├── s6-svscan (/run/service)                          │
│  │   ├── eclipse-gui/                                  │
│  │   │   ├── run (start service)                       │
│  │   │   └── log/run (s6-log)                          │
│  │   ├── network/                                       │
│  │   │   ├── run                                        │
│  │   │   └── log/run                                    │
│  │   ├── syslog/                                        │
│  │   │   ├── run                                        │
│  │   │   └── log/run                                    │
│  │   └── eclipse-shell/                                 │
│  │       ├── run                                        │
│  │       └── log/run                                    │
│  │                                                      │
│  └── Logging (/var/log/s6)                             │
│      ├── eclipse-gui/                                   │
│      ├── network/                                       │
│      ├── syslog/                                        │
│      └── eclipse-shell/                                 │
└─────────────────────────────────────────────────────────┘
```

## Installation

### From Source

```bash
cd eclipse-apps/s6

# Build
cargo build --release

# Install (requires root)
sudo ./install_s6.sh

# Verify installation
ls -la /sbin/eclipse-s6
ls -la /sbin/init
ls -la /run/service
```

### Integration with Eclipse OS

S6 is integrated into the Eclipse OS build system:

```bash
# Build Eclipse OS with S6
./build.sh

# The build system will:
# 1. Compile eclipse-s6 binary
# 2. Install service definitions
# 3. Set up /sbin/init symlink
```

## Usage

### Running as Init (PID 1)

When Eclipse OS boots, the kernel executes `/sbin/init`, which is symlinked to `/sbin/eclipse-s6`:

```
Kernel → /sbin/init → /sbin/eclipse-s6 → S6 Init System
```

The S6 init system then:
1. Creates necessary directories (`/run/service`, `/etc/s6/rc`, `/var/log/s6`)
2. Sets up the environment
3. Starts the supervision tree
4. Monitors all services continuously

### Service Control

Control services using the eclipse-s6 command:

```bash
# Start a service
eclipse-s6 start network

# Stop a service
eclipse-s6 stop network

# Restart a service
eclipse-s6 restart network

# Check service status
eclipse-s6 status network
```

### Available Services

- **eclipse-gui**: Eclipse OS graphical user interface
- **network**: Network management service
- **syslog**: System logging service
- **eclipse-shell**: Eclipse OS shell/terminal

## Service Definition

S6 services are defined using simple shell scripts in `/run/service/<service-name>/`:

### Run Script (`run`)

The main service script:

```bash
#!/bin/sh
# Service run script

exec 2>&1
exec /sbin/my-service
```

Key points:
- Must be executable (`chmod +x`)
- Should `exec` into the service (replaces the shell)
- `exec 2>&1` redirects stderr to stdout for logging

### Log Script (`log/run`)

The logging script:

```bash
#!/bin/sh
# Service log script

exec s6-log -d3 -b n20 s1000000 T /var/log/s6/my-service
```

Options:
- `-d3`: Debug level 3
- `-b n20`: Keep 20 log files before rotation
- `-s1000000`: Rotate when file reaches 1MB
- `T`: Prepend timestamps to log lines

## Directory Structure

```
eclipse-apps/s6/
├── Cargo.toml          # Rust project configuration
├── src/
│   └── main.rs         # S6 init system implementation
├── services/           # Service definitions
│   ├── eclipse-gui/
│   │   ├── run         # Start eclipse-gui
│   │   └── log/run     # Log eclipse-gui output
│   ├── network/
│   │   ├── run
│   │   └── log/run
│   ├── syslog/
│   │   ├── run
│   │   └── log/run
│   └── eclipse-shell/
│       ├── run
│       └── log/run
├── rc/                 # S6-rc configuration (bundles, dependencies)
├── install_s6.sh       # Installation script
└── README.md          # This file
```

## System Integration

### Kernel Integration

The Eclipse OS kernel starts S6 via `init_system.rs`:

1. Kernel boots and initializes
2. Kernel sets up PID 1 context
3. Kernel transfers control to `/sbin/init` (→ `/sbin/eclipse-s6`)
4. S6 takes over as init system

No changes to `init_system.rs` were needed - it's init-agnostic!

### Comparison to systemd

| Feature | systemd | S6 |
|---------|---------|-----|
| **Binary size** | ~10MB | ~200KB |
| **Dependencies** | Many | Minimal |
| **Complexity** | High | Low |
| **Boot time** | ~2-3s | ~1s |
| **Memory usage** | ~10MB | ~1MB |
| **Configuration** | .service files (complex) | Shell scripts (simple) |
| **Modularity** | Monolithic | Highly modular |
| **Reliability** | Good | Excellent |
| **Philosophy** | "Do everything" | "Do one thing well" |

## Development

### Building

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test
```

### Adding a New Service

1. Create service directory:
```bash
mkdir -p /run/service/my-service/log
```

2. Create run script (`/run/service/my-service/run`):
```bash
#!/bin/sh
exec 2>&1
exec /sbin/my-service
```

3. Create log script (`/run/service/my-service/log/run`):
```bash
#!/bin/sh
exec s6-log -d3 -b n20 s1000000 T /var/log/s6/my-service
```

4. Make scripts executable:
```bash
chmod +x /run/service/my-service/run
chmod +x /run/service/my-service/log/run
```

## Migration from systemd

Eclipse OS originally used systemd but migrated to S6 for:
- **Better modularity**: S6's design is cleaner and more modular
- **Smaller footprint**: Critical for embedded systems
- **Perfect engineering**: S6 embodies the Unix philosophy
- **Reliability**: S6 is designed for 24/7 operation

### What Changed

- Removed: `eclipse-apps/systemd/` (entire directory)
- Removed: `userland/mini-systemd/` (replaced with S6 init)
- Added: `eclipse-apps/s6/` (new S6 implementation)
- Updated: `build.sh` (build S6 instead of systemd)
- Updated: `init_system.rs` (updated binary path, no structural changes)

## Troubleshooting

### Service Won't Start

Check the service's run script:
```bash
cat /run/service/<service>/run
```

Check logs:
```bash
tail -f /var/log/s6/<service>/current
```

### S6 Won't Initialize

Check system logs:
```bash
dmesg | grep s6
journalctl -xe  # If journald is available
```

Verify directories exist:
```bash
ls -la /run/service
ls -la /etc/s6/rc
ls -la /var/log/s6
```

### Service Keeps Restarting

S6 automatically restarts failed services. To stop auto-restart:
```bash
# Stop the service
eclipse-s6 stop <service>

# Or remove from supervision tree
rm -rf /run/service/<service>
```

## References

- [S6 Homepage](https://skarnet.org/software/s6/)
- [S6 Documentation](https://skarnet.org/software/s6/overview.html)
- [s6-rc](https://skarnet.org/software/s6-rc/) - Dependency management
- [Eclipse OS](https://github.com/Pryancito/eclipse)

## License

Eclipse S6 integration is licensed under the MIT License. See `LICENSE` for details.

## Support

For issues or questions:
- GitHub Issues: [eclipse/issues](https://github.com/Pryancito/eclipse/issues)
- Documentation: This README
- Community: Eclipse OS Discussions

---

**Eclipse S6** - Perfect modular systems engineering
