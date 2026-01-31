# VirtIO Filesystem Mount Fix

## Problem Description

The Eclipse OS kernel was failing to mount the eclipsefs filesystem when using VirtIO block devices with the following error:

```
[FS] Attempting to mount eclipsefs...
[FS] Allocating superblock buffer...
[FS] Buffer allocated at: 0x0x0000000000383DD0
[FS] Reading superblock from block device...
[FS] Superblock read successfully
[FS] Invalid EclipseFS header
[KERNEL] Failed to mount filesystem: Invalid EclipseFS header
```

## Root Cause

The issue was in `eclipse_kernel/src/virtio.rs` in the `read_block()` and `write_block()` methods. The code used `if self.mmio_base == 0` to determine whether to use simulated disk or real VirtIO operations.

However, VirtIO devices can be configured in three ways:
1. **Simulated disk**: `mmio_base == 0 && io_base == 0`
2. **Legacy PCI (I/O port based)**: `mmio_base == 0 && io_base != 0`
3. **MMIO device**: `mmio_base != 0`

When using legacy PCI VirtIO devices (common in QEMU), the device has:
- `mmio_base = 0`
- `io_base = <non-zero I/O port address>`

The original code incorrectly treated this as a simulated disk instead of a real VirtIO device, causing filesystem reads to return incorrect data and resulting in "Invalid EclipseFS header" errors.

## Solution

### 1. Fixed Device Type Detection

**Before:**
```rust
if self.mmio_base == 0 {
    // Simulated read
    ...
}
```

**After:**
```rust
if self.mmio_base == 0 && self.io_base == 0 {
    // Simulated read
    ...
}
```

This correctly identifies simulated disks (both bases are 0) vs. real VirtIO devices (at least one base is non-zero).

### 2. Fixed Queue Notification

**Before:**
```rust
// Notify device
let regs = self.mmio_base as *mut VirtIOMMIORegs;
write_volatile(&mut (*regs).queue_notify, 0);
```

**After:**
```rust
// Notify device
if self.io_base != 0 && self.mmio_base == 0 {
    // Legacy PCI - use I/O port notification
    outw(self.io_base + VIRTIO_PCI_QUEUE_NOTIFY, 0);
} else if self.mmio_base != 0 {
    // MMIO - use MMIO register notification
    let regs = self.mmio_base as *mut VirtIOMMIORegs;
    write_volatile(&mut (*regs).queue_notify, 0);
} else {
    // Invalid configuration - cleanup and return error
    crate::memory::free_dma_buffer(req_ptr, core::mem::size_of::<VirtIOBlockReq>(), 16);
    crate::memory::free_dma_buffer(status_ptr, 1, 1);
    return Err("Invalid device configuration");
}
```

This ensures:
- Legacy PCI devices use I/O port operations (`outw`)
- MMIO devices use MMIO operations (`write_volatile`)
- Invalid configurations are caught with proper error handling

## Files Changed

- `eclipse_kernel/src/virtio.rs`:
  - Modified `VirtIOBlockDevice::read_block()` method (lines 672, 723-735)
  - Modified `VirtIOBlockDevice::write_block()` method (lines 776, 832-844)

## Testing

To verify the fix:

1. Build the kernel:
   ```bash
   ./build.sh
   ```

2. Run with QEMU using VirtIO block device:
   ```bash
   ./qemu.sh
   ```

3. Verify that the kernel output shows successful filesystem mount:
   ```
   [FS] Attempting to mount eclipsefs...
   [FS] EclipseFS signature found
   [FS] Version: 2.0
   [FS] Filesystem mounted successfully
   ```

## Impact

This fix enables Eclipse OS to correctly mount the eclipsefs filesystem when running in virtual environments (QEMU/KVM) with VirtIO block devices, which is the recommended configuration for development and testing.

## Related Documentation

- [VirtIO Specification](https://docs.oasis-open.org/virtio/virtio/v1.1/virtio-v1.1.html)
- [VirtIO Block Device](https://docs.oasis-open.org/virtio/virtio/v1.1/cs01/virtio-v1.1-cs01.html#x1-2390002)
- Eclipse OS VirtIO Implementation: `VIRTIO_DRIVER_IMPLEMENTATION.md`
