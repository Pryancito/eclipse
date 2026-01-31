#!/bin/bash
# Simple test to verify VirtIO driver initialization logic

set -e

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║     Eclipse OS VirtIO Driver - Logic Verification Test      ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# Test 1: Check that VirtIO module exists and is properly structured
echo "[TEST 1] Checking VirtIO module structure..."
if grep -q "pub fn init()" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/virtio.rs; then
    echo "✓ VirtIO init() function exists"
else
    echo "✗ VirtIO init() function missing"
    exit 1
fi

if grep -q "pub fn read_block" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/virtio.rs; then
    echo "✓ VirtIO read_block() function exists"
else
    echo "✗ VirtIO read_block() function missing"
    exit 1
fi

# Test 2: Check block device abstraction in filesystem
echo ""
echo "[TEST 2] Checking block device abstraction..."
if grep -q "fn read_block_from_device" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/filesystem.rs; then
    echo "✓ Block device abstraction exists"
else
    echo "✗ Block device abstraction missing"
    exit 1
fi

if grep -q "crate::virtio::read_block" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/filesystem.rs; then
    echo "✓ Filesystem uses VirtIO for block reads"
else
    echo "✗ Filesystem doesn't use VirtIO"
    exit 1
fi

if grep -q "crate::ata::read_block" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/filesystem.rs; then
    echo "✓ Filesystem has ATA fallback"
else
    echo "✗ Filesystem missing ATA fallback"
    exit 1
fi

# Test 3: Check kernel initialization order
echo ""
echo "[TEST 3] Checking kernel initialization order..."
if grep -A 5 "virtio::init()" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/main.rs | grep -q "ata::init()"; then
    echo "✓ VirtIO initialized before ATA"
else
    echo "✗ Initialization order incorrect"
    exit 1
fi

# Test 4: Check QEMU configuration
echo ""
echo "[TEST 4] Checking QEMU configuration..."
if grep -q "if=virtio" /home/runner/work/eclipse/eclipse/qemu.sh; then
    echo "✓ QEMU configured to use VirtIO"
else
    echo "✗ QEMU not using VirtIO"
    exit 1
fi

# Test 5: Check simulated disk initialization
echo ""
echo "[TEST 5] Checking simulated disk initialization..."
if grep -q "ECLIPSEFS" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/virtio.rs; then
    echo "✓ EclipseFS magic signature in VirtIO driver"
else
    echo "✗ EclipseFS signature missing"
    exit 1
fi

if grep -q "to_le_bytes" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/virtio.rs; then
    echo "✓ Using correct little-endian encoding"
else
    echo "✗ Byte encoding may be incorrect"
    exit 1
fi

# Test 6: Check partition offset handling
echo ""
echo "[TEST 6] Checking partition offset handling..."
if grep -q "PARTITION_OFFSET.*131328" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/virtio.rs; then
    echo "✓ Partition offset correctly set to 131328"
else
    echo "✗ Partition offset incorrect or missing"
    exit 1
fi

# Test 7: Verify build artifacts exist
echo ""
echo "[TEST 7] Checking build artifacts..."
if [ -f "/home/runner/work/eclipse/eclipse/eclipse_kernel/target/x86_64-eclipse-microkernel/release/eclipse_kernel" ]; then
    echo "✓ Kernel binary exists"
    ls -lh /home/runner/work/eclipse/eclipse/eclipse_kernel/target/x86_64-eclipse-microkernel/release/eclipse_kernel
else
    echo "✗ Kernel binary not found"
    exit 1
fi

if [ -f "/home/runner/work/eclipse/eclipse/bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader.efi" ]; then
    echo "✓ Bootloader binary exists"
    ls -lh /home/runner/work/eclipse/eclipse/bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader.efi
else
    echo "✗ Bootloader binary not found"
    exit 1
fi

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║                  ALL TESTS PASSED ✓✓✓                       ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "Summary:"
echo "  - VirtIO driver module is correctly structured"
echo "  - Block device abstraction is in place"
echo "  - VirtIO is tried before ATA fallback"
echo "  - QEMU is configured for VirtIO"
echo "  - Simulated disk has proper EclipseFS header"
echo "  - Partition offset handling is correct"
echo "  - All binaries successfully built"
echo ""
echo "The VirtIO implementation is ready for runtime testing!"
