#!/bin/bash
# Eclipse OS Kernel Test Suite
# Tests all major kernel functionality

set -e

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║         Eclipse OS Kernel Test Suite v1.0                   ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

TESTS_PASSED=0
TESTS_FAILED=0
TESTS_TOTAL=0

function test_start() {
    echo -n "Testing $1... "
    TESTS_TOTAL=$((TESTS_TOTAL + 1))
}

function test_pass() {
    echo -e "${GREEN}✓ PASS${NC}"
    TESTS_PASSED=$((TESTS_PASSED + 1))
}

function test_fail() {
    echo -e "${RED}✗ FAIL${NC}"
    if [ -n "$1" ]; then
        echo "  Error: $1"
    fi
    TESTS_FAILED=$((TESTS_FAILED + 1))
}

function test_skip() {
    echo -e "${YELLOW}⊘ SKIP${NC} ($1)"
}

# ==============================================================================
# Build Tests
# ==============================================================================

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 1: Build Tests"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

test_start "Service binaries build"
if cd eclipse_kernel/userspace && \
   for service in filesystem_service network_service display_service audio_service input_service; do
       cd $service && cargo +nightly build --release &> /dev/null && cd ..
   done; then
    test_pass
else
    test_fail "Service build failed"
fi
cd ../../

test_start "Init binary builds"
if cd eclipse_kernel/userspace/init && \
   cargo +nightly build --release &> /dev/null; then
    test_pass
else
    test_fail "Init build failed"
fi
cd ../../../

test_start "Kernel builds"
if cd eclipse_kernel && \
   cargo +nightly build --release &> /dev/null; then
    test_pass
else
    test_fail "Kernel build failed"
fi
cd ..

test_start "Bootloader builds"
if cd bootloader-uefi && \
   cargo +nightly build --release --target x86_64-unknown-uefi &> /dev/null; then
    test_pass
else
    test_fail "Bootloader build failed"
fi
cd ..

# ==============================================================================
# Binary Verification Tests
# ==============================================================================

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 2: Binary Verification Tests"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

test_start "Service binaries exist"
SERVICES_OK=true
for service in filesystem_service network_service display_service audio_service input_service; do
    if [ ! -f "eclipse_kernel/userspace/$service/target/x86_64-unknown-none/release/$service" ]; then
        SERVICES_OK=false
        break
    fi
done
if [ "$SERVICES_OK" = true ]; then
    test_pass
else
    test_fail "Missing service binaries"
fi

test_start "Init binary exists"
if [ -f "eclipse_kernel/userspace/init/target/x86_64-unknown-none/release/eclipse-init" ]; then
    test_pass
else
    test_fail "Init binary not found"
fi

test_start "Kernel binary exists"
if [ -f "eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel" ]; then
    test_pass
else
    test_fail "Kernel binary not found"
fi

test_start "Bootloader binary exists"
if [ -f "bootloader-uefi/target/x86_64-unknown-uefi/release/bootloader-uefi.efi" ]; then
    test_pass
else
    test_fail "Bootloader binary not found"
fi

# ==============================================================================
# Binary Size Tests
# ==============================================================================

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 3: Binary Size Verification"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

test_start "Service binaries reasonable size"
SERVICES_SIZE_OK=true
for service in filesystem_service network_service display_service audio_service input_service; do
    SIZE=$(stat -f%z "eclipse_kernel/userspace/$service/target/x86_64-unknown-none/release/$service" 2>/dev/null || stat -c%s "eclipse_kernel/userspace/$service/target/x86_64-unknown-none/release/$service" 2>/dev/null || echo "0")
    if [ "$SIZE" -lt 1000 ] || [ "$SIZE" -gt 50000 ]; then
        SERVICES_SIZE_OK=false
        break
    fi
done
if [ "$SERVICES_SIZE_OK" = true ]; then
    test_pass
else
    test_fail "Service binary sizes outside expected range (1KB-50KB)"
fi

test_start "Init binary reasonable size"
INIT_SIZE=$(stat -f%z "eclipse_kernel/userspace/init/target/x86_64-unknown-none/release/eclipse-init" 2>/dev/null || stat -c%s "eclipse_kernel/userspace/init/target/x86_64-unknown-none/release/eclipse-init" 2>/dev/null || echo "0")
if [ "$INIT_SIZE" -gt 5000 ] && [ "$INIT_SIZE" -lt 50000 ]; then
    test_pass
else
    test_fail "Init binary size $INIT_SIZE outside expected range (5KB-50KB)"
fi

test_start "Kernel binary reasonable size"
KERNEL_SIZE=$(stat -f%z "eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel" 2>/dev/null || stat -c%s "eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel" 2>/dev/null || echo "0")
if [ "$KERNEL_SIZE" -gt 500000 ] && [ "$KERNEL_SIZE" -lt 2000000 ]; then
    test_pass
else
    test_fail "Kernel binary size $KERNEL_SIZE outside expected range (500KB-2MB)"
fi

# ==============================================================================
# Code Quality Tests
# ==============================================================================

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Phase 4: Code Quality Tests"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

test_start "Kernel has no compilation errors"
if cd eclipse_kernel && cargo +nightly build --release 2>&1 | grep -q "^error"; then
    test_fail "Kernel has compilation errors"
else
    test_pass
fi
cd ..

test_start "Services have no compilation errors"
SERVICES_ERRORS=false
cd eclipse_kernel/userspace
for service in filesystem_service network_service display_service audio_service input_service init; do
    if cd $service && cargo +nightly build --release 2>&1 | grep -q "^error"; then
        SERVICES_ERRORS=true
        break
    fi
    cd ..
done
cd ../../..
if [ "$SERVICES_ERRORS" = false ]; then
    test_pass
else
    test_fail "Services have compilation errors"
fi

# ==============================================================================
# Summary
# ==============================================================================

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Test Summary"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Tests Passed:  ${GREEN}$TESTS_PASSED${NC} / $TESTS_TOTAL"
echo "Tests Failed:  ${RED}$TESTS_FAILED${NC} / $TESTS_TOTAL"
echo ""

if [ $TESTS_FAILED -eq 0 ]; then
    echo -e "${GREEN}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║                 ALL TESTS PASSED! ✓                          ║${NC}"
    echo -e "${GREEN}╚══════════════════════════════════════════════════════════════╝${NC}"
    exit 0
else
    echo -e "${RED}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${RED}║                 SOME TESTS FAILED ✗                          ║${NC}"
    echo -e "${RED}╚══════════════════════════════════════════════════════════════╝${NC}"
    exit 1
fi
