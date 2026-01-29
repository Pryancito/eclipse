# Eclipse-SystemD Kernel Integration

## Overview

The Eclipse OS kernel includes infrastructure to support **eclipse-systemd** as the init system (PID 1). This document describes the current state of the integration, what works, what's simulated, and what's needed for full functionality.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                   Eclipse OS Boot Flow                   │
├─────────────────────────────────────────────────────────┤
│                                                           │
│  1. Bootloader (UEFI)                                    │
│       ↓                                                   │
│  2. Kernel (_start in main.rs)                          │
│       ↓                                                   │
│  3. kernel_main() in main_simple.rs                      │
│       ↓                                                   │
│  4. System Initialization                                │
│     - Memory management                                  │
│     - Interrupts                                         │
│     - Drivers                                            │
│       ↓                                                   │
│  5. SystemD Integration Point ← YOU ARE HERE             │
│     - init_and_execute_systemd()                         │
│     - InitSystem::initialize()                           │
│     - InitSystem::execute_init()                         │
│       ↓                                                   │
│  6a. [SUCCESS] eclipse-systemd takes over (PID 1)        │
│       └→ Service management                              │
│       └→ Target activation                               │
│       └→ Process supervision                             │
│                                                           │
│  6b. [FALLBACK] Kernel main loop                         │
│       └→ Event processing                                │
│       └→ Driver management                               │
│                                                           │
└─────────────────────────────────────────────────────────┘
```

## Integration Components

### 1. init_system.rs
**Location**: `eclipse_kernel/src/init_system.rs`

**Purpose**: Core module managing the transition from kernel to systemd.

**Key Structures**:
- `InitSystem`: Main manager for systemd initialization
- `InitProcess`: Configuration for PID 1 process

**Key Functions**:
- `initialize()`: Sets up the init system configuration
- `execute_init()`: Attempts to transfer control to systemd

**Status**: ✅ Implemented with documented limitations

### 2. process_memory.rs
**Location**: `eclipse_kernel/src/process_memory.rs`

**Purpose**: Manages memory allocation for userland processes.

**Key Features**:
- Process memory layout (text, data, heap, stack)
- Memory mapping flags (read, write, execute, user)
- Stack and heap management

**Status**: ✅ Complete API, ⚠️ Simulated page table setup

### 3. process_transfer.rs
**Location**: `eclipse_kernel/src/process_transfer.rs`

**Purpose**: Handles the actual control transfer from kernel to userland.

**Key Components**:
- `ProcessContext`: CPU register state for userland
- `ProcessTransfer`: Manager for userland transitions
- `transfer_to_userland_with_iretq()`: Control transfer mechanism

**Status**: ✅ Documented, ⚠️ Requires complete paging to execute

### 4. elf_loader.rs
**Location**: `eclipse_kernel/src/elf_loader.rs`

**Purpose**: Loads ELF64 executables for execution.

**Features**:
- ELF header parsing
- Program segment loading
- Entry point detection

**Status**: ✅ Parser complete, ⚠️ Uses fake data without VFS

### 5. Kernel Main Integration
**Location**: `eclipse_kernel/src/main_simple.rs`

**Integration Point**: Added before main loop in `kernel_main()`

```rust
// Check if systemd is enabled
if systemd_enabled {
    match init_and_execute_systemd(fb) {
        Ok(_) => { /* systemd took over */ }
        Err(e) => { /* fallback to kernel loop */ }
    }
}
```

**Status**: ✅ Implemented and functional

## Current Implementation Status

### ✅ What Works (Implemented)

1. **SystemD Module Detection**
   - Kernel can detect if systemd should be enabled
   - Configurable via kernel parameters
   - Graceful fallback if disabled

2. **Init Process Configuration**
   - PID 1 setup with correct environment variables
   - Standard paths (PATH, HOME, etc.)
   - Systemd-specific environment (DISPLAY, XDG_*)

3. **Integration Hooks**
   - Kernel calls systemd initialization at the right time
   - Error handling and fallback logic
   - Status messages on framebuffer

4. **Component Communication**
   - init_system.rs → elf_loader.rs
   - init_system.rs → process_memory.rs
   - init_system.rs → process_transfer.rs
   - All modules connect properly

### ⚠️ What's Simulated (Needs Real Implementation)

1. **Filesystem Access**
   - **Current**: Uses hardcoded fake ELF data
   - **Needed**: Read `/sbin/init` from actual VFS
   - **Blocker**: No Virtual Filesystem (VFS) implementation

2. **Virtual Memory**
   - **Current**: Simulates page table setup
   - **Needed**: Real CR3 loading and page table configuration
   - **Blocker**: Incomplete paging subsystem

3. **Control Transfer**
   - **Current**: Documents what `iretq` should do, returns error
   - **Needed**: Actual ring 3 transition via `iretq`
   - **Blocker**: Requires functioning page tables

4. **File Permissions**
   - **Current**: Simulated checks always pass
   - **Needed**: Real filesystem permission verification
   - **Blocker**: No filesystem implementation

### ❌ What's Missing (Not Started)

1. **Critical Syscalls**
   ```
   - fork/clone: Create new processes
   - execve: Execute programs
   - wait/waitpid: Wait for child processes
   - signal: SIGTERM, SIGKILL, etc.
   - getpid/getppid: Process identification
   ```

2. **Procfs (/proc)**
   ```
   - /proc/<pid>/stat: Process information
   - /proc/cpuinfo: CPU information
   - /proc/meminfo: Memory information
   ```

3. **Service File Loading**
   ```
   - Read .service files from /etc/eclipse/systemd/system/
   - Parse configuration
   - Validate service dependencies
   ```

## Testing the Integration

### Build the Kernel

```bash
cd eclipse_kernel
cargo build --release --target x86_64-unknown-none
```

### Boot Sequence with SystemD Enabled

When you boot the kernel, you should see:

```
═══════════════════════════════════════════════════════
  ✅ ECLIPSE OS - Sistema completamente inicializado
═══════════════════════════════════════════════════════

🔄 Transferencia de control a eclipse-systemd...

=== ECLIPSE-SYSTEMD TOMANDO CONTROL ===

PID 1: eclipse-systemd iniciando...
Sistema de logging activado
Cargando servicios del sistema...

⚠ Error systemd: Transferencia al userland no soportada sin memoria virtual completa - usando kernel loop

🚀 Iniciando loop principal mejorado...
   Procesando eventos del sistema...
```

### Expected Behavior

1. ✅ Kernel boots normally
2. ✅ SystemD initialization is attempted
3. ✅ Framebuffer shows systemd messages
4. ⚠️ Transfer fails with documented error
5. ✅ Kernel falls back to main loop
6. ✅ System continues running normally

## Enabling/Disabling SystemD

### Via Code (Current Method)

Edit `eclipse_kernel/src/main_simple.rs`:

```rust
// Set to false to disable systemd
let systemd_enabled = true;  // or false
```

### Via Kernel Parameters (Planned)

When bootloader parameter parsing is implemented:

```bash
# Enable systemd
eclipse.init=/sbin/init

# Disable systemd (use kernel loop)
eclipse.init=kernel
```

## Roadmap to Full Functionality

### Phase 1: Foundation (COMPLETE) ✅
- [x] Create init_system.rs module
- [x] Integrate with kernel_main
- [x] Document current state
- [x] Implement proper error handling

### Phase 2: Virtual Memory (IN PROGRESS) 🚧
- [ ] Complete page table implementation in paging.rs
- [ ] Implement real CR3 switching
- [ ] Setup kernel and userland page tables
- [ ] Enable process_transfer.rs iretq

### Phase 3: Filesystem (REQUIRED) 📋
- [ ] Implement VFS layer
- [ ] Add EclipseFS driver
- [ ] Implement file reading
- [ ] Mount root filesystem
- [ ] Enable elf_loader.rs real file reading

### Phase 4: Process Management (REQUIRED) 📋
- [ ] Implement fork syscall
- [ ] Implement execve syscall
- [ ] Implement wait/waitpid syscalls
- [ ] Add process table
- [ ] Add scheduler integration

### Phase 5: Signal Handling (REQUIRED) 📋
- [ ] Implement signal delivery
- [ ] Add SIGTERM handler
- [ ] Add SIGKILL handler
- [ ] Integrate with process manager

### Phase 6: Procfs (OPTIONAL) 🎯
- [ ] Implement /proc filesystem
- [ ] Add /proc/<pid>/stat
- [ ] Add /proc/cpuinfo
- [ ] Add /proc/meminfo

### Phase 7: Full Integration (GOAL) 🎉
- [ ] SystemD successfully starts as PID 1
- [ ] Services can be loaded
- [ ] Process supervision works
- [ ] System boots to target

## How to Contribute

### For Kernel Developers

Focus areas:
1. **Virtual Memory**: Complete `paging.rs` implementation
2. **VFS**: Implement basic virtual filesystem
3. **Syscalls**: Add fork, exec, wait, signal handlers
4. **Procfs**: Create /proc filesystem

### For SystemD Developers

Current work:
1. **Service Parser**: Already complete ✅
2. **Process Monitor**: Works with /proc (when available)
3. **Dependency Resolution**: Already complete ✅
4. **Journal**: Works (needs filesystem for persistence)

### Testing Checklist

- [ ] Kernel compiles without errors
- [ ] SystemD integration code is reachable
- [ ] Error messages are clear and helpful
- [ ] Fallback to kernel loop works
- [ ] No crashes during init attempt
- [ ] Documentation is up to date

## Troubleshooting

### "eclipse-systemd no encontrado"
**Cause**: Simulated file check failed  
**Solution**: Normal in current implementation, fallback works

### "Transferencia al userland no soportada"
**Cause**: Page tables not set up for userland  
**Solution**: Expected until paging.rs is complete

### Kernel hangs at init
**Cause**: Possible infinite loop in init code  
**Solution**: Check serial output, verify error handling

### No systemd messages shown
**Cause**: SystemD might be disabled or code not reached  
**Solution**: Check systemd_enabled flag in kernel_main

## References

- **SystemD Documentation**: `eclipse-apps/systemd/README.md`
- **Init System Module**: `eclipse_kernel/src/init_system.rs`
- **Process Transfer**: `eclipse_kernel/src/process_transfer.rs`
- **ELF Loader**: `eclipse_kernel/src/elf_loader.rs`

## Questions?

For questions about the systemd integration:
- Check this document first
- Review source code comments
- Check the systemd README
- Open an issue on GitHub

---

**Last Updated**: 2026-01-29  
**Status**: Integration framework complete, awaiting VFS and paging  
**Next Steps**: Complete virtual memory implementation
