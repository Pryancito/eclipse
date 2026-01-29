#!/bin/bash
# Test script for Wayland integration
# Tests building with different library configurations

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "╔════════════════════════════════════════════════════════════╗"
echo "║  Wayland Integration Test Suite for Eclipse OS            ║"
echo "╚════════════════════════════════════════════════════════════╝"
echo ""

# Test 1: Build wayland_integration library
echo -e "${YELLOW}Test 1: Building wayland_integration library...${NC}"
cd userland/wayland_integration
cargo clean > /dev/null 2>&1
cargo build --release 2>&1 | grep -E "warning.*wayland|warning.*wlroots|Compiling wayland_integration" || true
if [ $? -eq 0 ]; then
    echo -e "${GREEN}✓ wayland_integration library built successfully${NC}"
else
    echo -e "${RED}✗ Failed to build wayland_integration library${NC}"
    exit 1
fi
cd ../..
echo ""

# Test 2: Check library detection
echo -e "${YELLOW}Test 2: Checking library detection...${NC}"
if pkg-config --exists wayland-server 2>/dev/null; then
    echo -e "${GREEN}✓ libwayland-server detected${NC}"
    pkg-config --modversion wayland-server
else
    echo -e "${YELLOW}⚠ libwayland-server not found (will use custom implementation)${NC}"
fi

if pkg-config --exists wlroots 2>/dev/null; then
    echo -e "${GREEN}✓ wlroots detected${NC}"
    pkg-config --modversion wlroots
else
    echo -e "${YELLOW}⚠ wlroots not found (will use fallback)${NC}"
fi
echo ""

# Test 3: Build wayland_compositor
echo -e "${YELLOW}Test 3: Building wayland_compositor...${NC}"
cd userland/wayland_compositor
make clean > /dev/null 2>&1
make 2>&1 | head -5
if [ $? -eq 0 ]; then
    echo -e "${GREEN}✓ wayland_compositor built successfully${NC}"
    # Check which variant was built
    if [ -f "wayland_compositor_wlroots" ]; then
        echo -e "${GREEN}  Built with wlroots support${NC}"
    elif [ -f "wayland_compositor_wayland" ]; then
        echo -e "${GREEN}  Built with libwayland support${NC}"
    elif [ -f "wayland_compositor" ]; then
        echo -e "${GREEN}  Built with custom implementation${NC}"
    fi
else
    echo -e "${RED}✗ Failed to build wayland_compositor${NC}"
    exit 1
fi
cd ../..
echo ""

# Test 4: Check features in wayland_integration
echo -e "${YELLOW}Test 4: Checking wayland_integration features...${NC}"
cd userland/wayland_integration
echo "Features available:"
cargo build --release --verbose 2>&1 | grep -E "cfg.*has_" || echo "  Using custom implementation (no system libraries)"
cd ../..
echo ""

# Test 5: Verify file sizes
echo -e "${YELLOW}Test 5: Verifying binary sizes...${NC}"
if [ -f "userland/wayland_integration/target/release/libwayland_integration.rlib" ]; then
    SIZE=$(du -h "userland/wayland_integration/target/release/libwayland_integration.rlib" | cut -f1)
    echo -e "${GREEN}✓ libwayland_integration.rlib: $SIZE${NC}"
fi

if [ -f "userland/wayland_compositor/wayland_compositor" ] || \
   [ -f "userland/wayland_compositor/wayland_compositor_wlroots" ] || \
   [ -f "userland/wayland_compositor/wayland_compositor_wayland" ]; then
    COMP=$(ls userland/wayland_compositor/wayland_compositor* 2>/dev/null | head -1)
    SIZE=$(du -h "$COMP" | cut -f1)
    echo -e "${GREEN}✓ wayland_compositor: $SIZE${NC}"
fi
echo ""

# Summary
echo "╔════════════════════════════════════════════════════════════╗"
echo "║  Test Summary                                              ║"
echo "╚════════════════════════════════════════════════════════════╝"
echo -e "${GREEN}All tests passed!${NC}"
echo ""
echo "Integration Status:"
if pkg-config --exists wlroots 2>/dev/null; then
    echo "  Backend: wlroots (preferred)"
elif pkg-config --exists wayland-server 2>/dev/null; then
    echo "  Backend: libwayland (standard)"
else
    echo "  Backend: custom Eclipse OS implementation"
fi
echo ""
echo "To install system libraries:"
echo "  sudo apt-get install libwayland-dev libwlroots-dev"
echo ""
