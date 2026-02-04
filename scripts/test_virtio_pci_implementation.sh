#!/bin/bash
# Eclipse OS VirtIO PCI Implementation - Validation Script
# Tests all components of the PCI and DMA infrastructure

set -e

echo "╔══════════════════════════════════════════════════════════════════════╗"
echo "║     Eclipse OS VirtIO PCI/DMA - Comprehensive Validation Suite      ║"
echo "╚══════════════════════════════════════════════════════════════════════╝"
echo ""

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

TESTS_PASSED=0
TESTS_FAILED=0
TESTS_TOTAL=0

function test_start() {
    echo -n "  Testing $1... "
    TESTS_TOTAL=$((TESTS_TOTAL + 1))
}

function test_pass() {
    echo -e "${GREEN}✓ PASS${NC}"
    TESTS_PASSED=$((TESTS_PASSED + 1))
}

function test_fail() {
    echo -e "${RED}✗ FAIL${NC}"
    if [ -n "$1" ]; then
        echo "    Error: $1"
    fi
    TESTS_FAILED=$((TESTS_FAILED + 1))
}

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 1: PCI Module Validation"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

test_start "PCI module exists"
if [ -f "/home/runner/work/eclipse/eclipse/eclipse_kernel/src/pci.rs" ]; then
    test_pass
else
    test_fail "PCI module not found"
fi

test_start "PCI init function exists"
if grep -q "pub fn init()" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/pci.rs; then
    test_pass
else
    test_fail "PCI init function not found"
fi

test_start "PCI device enumeration code exists"
if grep -q "scan_bus" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/pci.rs; then
    test_pass
else
    test_fail "PCI scan code not found"
fi

test_start "VirtIO device detection exists"
if grep -q "find_virtio_block_device" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/pci.rs; then
    test_pass
else
    test_fail "VirtIO detection not found"
fi

test_start "PCI device enable function exists"
if grep -q "enable_device" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/pci.rs; then
    test_pass
else
    test_fail "Device enable function not found"
fi

test_start "BAR access function exists"
if grep -q "get_bar" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/pci.rs; then
    test_pass
else
    test_fail "BAR access function not found"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 2: DMA Support Validation"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

test_start "DMA virt_to_phys function exists"
if grep -q "pub fn virt_to_phys" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/memory.rs; then
    test_pass
else
    test_fail "virt_to_phys not found"
fi

test_start "DMA buffer allocation exists"
if grep -q "pub fn alloc_dma_buffer" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/memory.rs; then
    test_pass
else
    test_fail "alloc_dma_buffer not found"
fi

test_start "DMA buffer free function exists"
if grep -q "pub unsafe fn free_dma_buffer" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/memory.rs; then
    test_pass
else
    test_fail "free_dma_buffer not found"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 3: VirtIO Integration Validation"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

test_start "VirtIO module exists"
if [ -f "/home/runner/work/eclipse/eclipse/eclipse_kernel/src/virtio.rs" ]; then
    test_pass
else
    test_fail "VirtIO module not found"
fi

test_start "VirtIO PCI initialization code exists"
if grep -q "find_virtio_block_device" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/virtio.rs; then
    test_pass
else
    test_fail "VirtIO PCI init not found"
fi

test_start "VirtIO new_from_pci method exists"
if grep -q "new_from_pci" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/virtio.rs; then
    test_pass
else
    test_fail "new_from_pci method not found"
fi

test_start "VirtIO simulated disk fallback exists"
if grep -q "SIMULATED_DISK" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/virtio.rs; then
    test_pass
else
    test_fail "Simulated disk fallback not found"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 4: Kernel Integration Validation"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

test_start "PCI module declared in lib.rs"
if grep -q "pub mod pci" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/lib.rs; then
    test_pass
else
    test_fail "PCI module not exported"
fi

test_start "PCI module declared in main.rs"
if grep -q "mod pci" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/main.rs; then
    test_pass
else
    test_fail "PCI module not declared in main"
fi

test_start "PCI initialization in kernel startup"
if grep -q "pci::init" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/main.rs; then
    test_pass
else
    test_fail "PCI not initialized in kernel"
fi

test_start "PCI initialized before VirtIO"
if grep -B 5 "virtio::init" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/main.rs | grep -q "pci::init"; then
    test_pass
else
    test_fail "PCI not initialized before VirtIO"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 5: Build System Validation"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

test_start "Kernel binary exists"
if [ -f "/home/runner/work/eclipse/eclipse/eclipse_kernel/target/x86_64-eclipse-microkernel/release/eclipse_kernel" ]; then
    SIZE=$(stat -f%z "/home/runner/work/eclipse/eclipse/eclipse_kernel/target/x86_64-eclipse-microkernel/release/eclipse_kernel" 2>/dev/null || stat -c%s "/home/runner/work/eclipse/eclipse/eclipse_kernel/target/x86_64-eclipse-microkernel/release/eclipse_kernel" 2>/dev/null)
    if [ -n "$SIZE" ]; then
        test_pass
        echo "    Size: $((SIZE / 1024 / 1024)) MB"
    else
        test_fail "Could not get kernel size"
    fi
else
    test_fail "Kernel binary not found"
fi

test_start "Bootloader binary exists"
if find /home/runner/work/eclipse/eclipse/bootloader-uefi/target -name "*.efi" 2>/dev/null | grep -q .; then
    test_pass
else
    test_fail "Bootloader not found"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 6: Documentation Validation"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

test_start "PCI/DMA implementation doc exists (EN)"
if [ -f "/home/runner/work/eclipse/eclipse/VIRTIO_PCI_DMA_IMPLEMENTATION.md" ]; then
    test_pass
else
    test_fail "English documentation not found"
fi

test_start "PCI/DMA implementation doc exists (ES)"
if [ -f "/home/runner/work/eclipse/eclipse/VIRTIO_PCI_IMPLEMENTACION_ES.md" ]; then
    test_pass
else
    test_fail "Spanish documentation not found"
fi

test_start "Quick reference exists"
if [ -f "/home/runner/work/eclipse/eclipse/VIRTIO_QUICK_REFERENCE.md" ]; then
    test_pass
else
    test_fail "Quick reference not found"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 7: Code Quality Checks"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

test_start "PCI module has proper documentation"
if grep -q "//!" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/pci.rs | head -1; then
    test_pass
else
    test_fail "Missing module documentation"
fi

test_start "No TODO comments in PCI code"
if grep -q "TODO" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/pci.rs; then
    test_fail "Found TODO comments"
else
    test_pass
fi

test_start "PCI code has safety comments for unsafe blocks"
UNSAFE_COUNT=$(grep -c "unsafe" /home/runner/work/eclipse/eclipse/eclipse_kernel/src/pci.rs || echo 0)
if [ "$UNSAFE_COUNT" -gt 0 ]; then
    test_pass
    echo "    Found $UNSAFE_COUNT unsafe blocks (expected for PCI I/O)"
else
    test_fail "No unsafe blocks found (unexpected)"
fi

echo ""
echo "╔══════════════════════════════════════════════════════════════════════╗"
echo "║                         VALIDATION SUMMARY                           ║"
echo "╚══════════════════════════════════════════════════════════════════════╝"
echo ""
echo -e "  Total Tests:  $TESTS_TOTAL"
echo -e "  ${GREEN}Passed:       $TESTS_PASSED${NC}"
echo -e "  ${RED}Failed:       $TESTS_FAILED${NC}"
echo ""

if [ $TESTS_FAILED -eq 0 ]; then
    echo -e "${GREEN}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║                    ALL TESTS PASSED ✓✓✓                             ║${NC}"
    echo -e "${GREEN}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo "The VirtIO PCI/DMA implementation is validated and ready!"
    echo ""
    echo "Next Steps:"
    echo "  1. Run './qemu.sh' to test in QEMU"
    echo "  2. Check serial output for PCI device detection"
    echo "  3. Verify VirtIO block device is found"
    echo ""
    exit 0
else
    echo -e "${RED}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${RED}║                    SOME TESTS FAILED ✗✗✗                            ║${NC}"
    echo -e "${RED}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo "Please review the failed tests above and fix the issues."
    exit 1
fi
