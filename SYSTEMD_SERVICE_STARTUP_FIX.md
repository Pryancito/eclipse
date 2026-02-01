# Eclipse-SystemD Service Startup Fix

## Problem

Eclipse-SystemD was only starting the log service and not proceeding to start dependent services. The system would show:

```
[PHASE 3] Starting system services
  [START] Starting services with no dependencies...
  [START] log.service - Log Server / Console - Central Logging Service
  ...
  [LOG-SERVICE] Ready to accept log messages from other services
```

After this, no additional services would start, even though 7 services were registered.

## Root Cause Analysis

### Investigation

1. **Execution Flow**: The log service (service 0, no dependencies) starts successfully
2. **Missing Output**: After starting log service, systemd should print "Starting dependent services..." but this message never appears
3. **Hanging Point**: Execution appears to stop somewhere between completing the first service startup and beginning the dependency resolution loop

### Likely Causes

1. **Too Short Initialization Delay**: `SERVICE_INIT_DELAY` was set to 100 yield iterations, which is very short
   - The working `userland/systemd` version uses 10000 (100x more)
   - With only 100 yields, race conditions or timing issues could occur
   - The parent process might not get enough time to properly handle the child process startup

2. **Scheduler Issues**: Possible scheduler bug where parent process doesn't get rescheduled
   - With frequent yields and multiple processes, scheduler might have edge case bugs
   - Parent might get stuck in ready queue

3. **Silent Crash**: Parent process might be panicking or crashing without visible error
   - Panic handler in eclipse-libc just calls `exit(1)` without printing
   - Crash would appear as silent hang

## Solution

### Changes Made

#### 1. Increase Service Initialization Delay

**File**: `eclipse-apps/systemd/src/main.rs`

```rust
// Changed from:
const SERVICE_INIT_DELAY: u32 = 100;

// To:
const SERVICE_INIT_DELAY: u32 = 10000;
```

**Rationale**: Match the working implementation and give processes more time to stabilize before starting dependent services.

#### 2. Add Comprehensive Debug Logging

Added debug output at key points to trace execution:

- Entry to `start_system_services()`
- Before and after yield loops
- For each service being checked in dependency resolution
- For each pass of the dependency resolution loop
- For dependency check results (met vs not met)

**Example Debug Output Expected**:
```
[DEBUG] Entering start_system_services
[DEBUG] Yielding 10000 times for service initialization
[DEBUG] Done yielding for service log.service
[DEBUG] Completed starting services with no dependencies

[START] Starting dependent services...
[PASS 1] Checking service dependencies...
[DEBUG] Checking service devfs.service state=Inactive has_deps=true
[DEBUG] Dependencies met for devfs.service, starting...
...
```

#### 3. Fix Build Scripts

**Files**: `build.sh`, `build_userspace_services.sh`

Changed from:
```bash
cargo build --release --target x86_64-unknown-none
```

To:
```bash
cargo +nightly build --release --target x86_64-unknown-none
```

**Rationale**: systemd uses nightly-only features (lang_items), so must be built with nightly Rust.

## Expected Service Startup Sequence

With the fix, services should start in this order:

### Wave 1 (No Dependencies)
- **log.service** (service 0) - Logging infrastructure

### Wave 2 (Depends on Log)
- **devfs.service** (service 1) - Device file system, depends on [0]
- **filesystem.service** (service 2) - VFS layer, depends on [0]

### Wave 3 (Depends on Log + DevFS + Filesystem)
- **input.service** (service 3) - Input handling, depends on [0,1,2]
- **display.service** (service 4) - Display system, depends on [0,1,2]
- **audio.service** (service 5) - Audio system, depends on [0,1,2]
- **network.service** (service 6) - Networking, depends on [0,1,2]

### Final State
- All 7 services running
- SystemD enters main monitoring loop
- Periodic heartbeats showing service status

## Testing Instructions

### 1. Build System

```bash
cd /home/runner/work/eclipse/eclipse
./build.sh
```

### 2. Run in QEMU

```bash
./qemu.sh
```

### 3. Expected Output

You should see:

```
[PHASE 3] Starting system services
  [DEBUG] Entering start_system_services
  [START] Starting services with no dependencies...
  [START] log.service - Log Server / Console - Central Logging Service
  [DEBUG] Yielding 10000 times for service initialization
    [OK] log.service started with PID X
  [DEBUG] Done yielding for service log.service
  [DEBUG] Completed starting services with no dependencies

  [START] Starting dependent services...
  [PASS 1] Checking service dependencies...
  [DEBUG] Checking service devfs.service state=Inactive has_deps=true
  [DEBUG] Dependencies met for devfs.service, starting...
  [START] devfs.service - Device Manager - Creates /dev nodes
    [OK] devfs.service started with PID Y
  ...
  [COMPLETE] No more services to start

[PHASE 4] Entering main service manager loop
[READY] Eclipse-SystemD is ready

[HEARTBEAT #1] SystemD operational
═══════════════════════════════════════════════════════════════
SERVICE STATUS:
───────────────────────────────────────────────────────────────
  log.service [active] PID:X Restarts:0
  devfs.service [active] PID:Y Restarts:0
  filesystem.service [active] PID:Z Restarts:0
  ...
═══════════════════════════════════════════════════════════════
```

### 4. Verify All Services Started

Check that all 7 services show `[active]` status in the heartbeat output.

## Cleanup After Verification

Once the fix is confirmed working, disable debug output for production:

1. Edit `eclipse-apps/systemd/src/main.rs`
2. Change `const DEBUG_SERVICE_STARTUP: bool = true;` to `const DEBUG_SERVICE_STARTUP: bool = false;`
3. Rebuild systemd: `cd eclipse-apps/systemd && cargo +nightly build --release --target x86_64-unknown-none`
4. Test again to ensure production build works without debug output

## Technical Details

### Service Dependency Graph

```
                    log.service (0)
                         |
            +------------+------------+
            |                         |
      devfs.service (1)      filesystem.service (2)
            |                         |
            +------------+------------+
                         |
         +-------+-------+-------+
         |       |       |       |
    input(3) display(4) audio(5) network(6)
```

### Dependency Resolution Algorithm

The multi-pass algorithm ensures cascading dependencies work:

1. **Pass 1**: Start services whose dependencies are Active
   - devfs, filesystem start (both depend only on log which is Active)
2. **Pass 2**: Check again for newly satisfied dependencies
   - input, display, audio, network start (all depend on log+devfs+filesystem which are now all Active)
3. **Continue**: Up to 10 passes maximum to handle deep dependency chains
4. **Terminate**: When no services started in a pass

### Scheduler Considerations

- Each `yield_cpu()` call triggers a context switch
- With 10000 yields, parent gives child ample time to initialize
- Scheduler uses round-robin with ready queue (64 process capacity)
- Both parent and child yield regularly, preventing starvation

## Files Modified

1. `eclipse-apps/systemd/src/main.rs`
   - Changed SERVICE_INIT_DELAY: 100 → 10000
   - Added debug logging throughout service startup flow

2. `build_userspace_services.sh`
   - Changed: `cargo build` → `cargo +nightly build`

3. `build.sh`
   - Changed: `cargo build` → `cargo +nightly build`

## References

- Original systemd implementation: `eclipse-apps/systemd/src/main.rs`
- Working reference: `userland/systemd/src/main.rs` (uses SERVICE_INIT_DELAY = 10000)
- Eclipse libc: `eclipse_kernel/userspace/libc/`
- Scheduler: `eclipse_kernel/src/scheduler.rs`
