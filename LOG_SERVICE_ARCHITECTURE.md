# Log Service Architecture

## Overview
The Log Service provides centralized logging for all Eclipse OS services through multiple output channels.

## Logging Channels

### 1. Serial Port Output (Real-time)
- **Purpose**: Immediate debugging output
- **Implementation**: Uses `write(1, ...)` syscall to output to stdout/serial
- **Availability**: Always active from service start
- **Use Case**: Real-time debugging, system monitoring during development

### 2. File Logging (/var/log/system.log)
- **Purpose**: Persistent log storage
- **Target Location**: `/var/log/system.log`
- **Current Status**: Buffered in memory (4KB buffer)
- **Future**: Will write to filesystem when file syscalls (open, write, close) are implemented

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│              Other Services                             │
│  (devfs, input, display, network)                       │
└───────────────────┬─────────────────────────────────────┘
                    │ (Future: IPC log messages)
                    ▼
┌─────────────────────────────────────────────────────────┐
│              LOG SERVICE (PID ~2)                       │
│                                                         │
│  ┌───────────────────────────────────────────────────┐ │
│  │         log_message(msg)                          │ │
│  │  1. Write to serial (immediate)                   │ │
│  │  2. Buffer in memory                              │ │
│  │  3. TODO: Flush to /var/log/system.log           │ │
│  └───────────────────────────────────────────────────┘ │
│         │                    │                          │
│         ▼                    ▼                          │
│   ┌──────────┐        ┌────────────┐                   │
│   │  Serial  │        │ Log Buffer │                   │
│   │   Port   │        │   (4KB)    │                   │
│   └──────────┘        └────────────┘                   │
└─────────────────────────────────────────────────────────┘
                              │
                              │ (Future: When FS available)
                              ▼
                    ┌───────────────────┐
                    │  /var/log/        │
                    │   system.log      │
                    └───────────────────┘
```

## Log Buffer

### Specifications
- **Size**: 4096 bytes (4KB)
- **Type**: Static in-memory array
- **Purpose**: Store logs until filesystem is ready
- **Overflow**: Stops accepting when full (to prevent data corruption)

### Buffer State
```rust
static mut LOG_BUFFER: [u8; 4096] = [0; 4096];
static mut LOG_BUFFER_POS: usize = 0;
```

## Service Lifecycle

### Startup Sequence
1. Log service starts (Priority 10 - first service)
2. Initialize serial output
3. Allocate log buffer
4. Display banner
5. Report readiness
6. Enter main loop

### Main Loop Operations
- Monitor buffer usage
- Report operational status
- (Future) Process IPC log messages from other services
- (Future) Flush buffer to /var/log/system.log periodically

## Future Enhancements

### Required for File Logging
1. **Kernel Syscalls**:
   - `SYS_OPEN` - Open file with flags
   - `SYS_CLOSE` - Close file descriptor
   - Enhanced `SYS_WRITE` - Support for file descriptors beyond 1/2
   
2. **Filesystem Service**:
   - Must be operational before log file writes
   - Must support `/var/log/` directory creation
   - Must handle append operations

3. **Integration Code**:
```rust
// When filesystem syscalls are available:
// Note: These constants would need to be defined in eclipse_libc
// const O_WRONLY: i32 = 0x0001;
// const O_CREAT: i32 = 0x0100;
// const O_APPEND: i32 = 0x0400;

let fd = open("/var/log/system.log", O_WRONLY | O_CREAT | O_APPEND, 0644);
if fd >= 0 {
    write(fd as u32, &LOG_BUFFER[..LOG_BUFFER_POS]);
    close(fd);
    LOG_BUFFER_POS = 0; // Reset buffer
}
```

### Log Rotation
- Implement log file rotation when size exceeds threshold
- Keep multiple log files (system.log, system.log.1, system.log.2, etc.)
- Compress old log files

### Structured Logging
- Add log levels (DEBUG, INFO, WARN, ERROR, CRITICAL)
- Add timestamps
- Add service/component identifiers
- Add thread/process IDs

### Performance Optimizations
- Increase buffer size if needed
- Implement ring buffer for continuous logging
- Batch writes to reduce syscall overhead
- Async I/O for file writes

## Testing

### Current Tests
- Service builds successfully
- Binary size: 16KB (increased from 11KB with buffer)
- No compilation warnings

### Manual Testing
```bash
# Build the service
cd eclipse_kernel/userspace/log_service
cargo +nightly build --release

# Check binary
ls -lh target/x86_64-unknown-none/release/log_service
```

### Integration Testing (Future)
- Verify serial output appears correctly
- Verify log file is created in /var/log/
- Verify log file contains all buffered messages
- Test buffer overflow handling
- Test concurrent log messages from multiple services

## Security Considerations

- **Buffer Overflow Protection**: Fixed-size buffer prevents unlimited memory growth
- **Unsafe Code**: Uses unsafe blocks for static buffer access (documented as safe for single-threaded service)
- **Single-threaded**: Service runs in single thread, no concurrent access to buffer
- **Path Validation**: Future file operations should validate `/var/log/` path
- **Permissions**: Log file should have appropriate permissions (0644)
- **Sanitization**: Log messages should be sanitized to prevent log injection

## Dependencies

- `eclipse_libc`: Syscall wrappers (getpid, yield_cpu, write)
- Kernel: Serial port driver for output
- (Future) Kernel: File operation syscalls
- (Future) Filesystem Service: For persistent storage
