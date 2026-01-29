# SystemD Reset Fix

## Problem
The system was resetting (triple fault) when attempting to transfer control to systemd in userland.

## Root Cause
The systemd stub created when no systemd binary is found contained a `HLT` instruction:
- `HLT` is a **privileged instruction** (requires CPL 0 / ring 0)
- When executed in userland (CPL 3 / ring 3), it causes a General Protection Fault (#GP)
- Without an exception handler, this leads to a triple fault and system reset

## Solution
Replaced `HLT` with `PAUSE` instruction in the stub:
- `PAUSE` is **not privileged** - safe in any privilege level
- CPU-friendly spin-wait instruction
- Prevents the system reset while maintaining a minimal stub

## Technical Details

### Original Code (BUGGY)
```asm
hlt         ; 0xF4 - Privileged, causes #GP in ring 3
jmp -3      ; 0xEB 0xFD
```

### Fixed Code
```asm
pause       ; 0xF3 0x90 - Safe in userland
jmp -4      ; 0xEB 0xFC
```

## Files Modified
- `eclipse_kernel/src/vfs_global.rs` - Fixed stub generation
- `eclipse_kernel/src/process_transfer.rs` - Re-enabled userland transfer

## Result
✅ System no longer resets when transferring to systemd
✅ Systemd stub runs safely in userland
✅ System remains operational
