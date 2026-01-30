#!/bin/bash
# Installation script for Eclipse S6 init system

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}╔══════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║    Eclipse S6 Init System - Installation Script         ║${NC}"
echo -e "${BLUE}║    Perfect Modular Systems Engineering                   ║${NC}"
echo -e "${BLUE}╚══════════════════════════════════════════════════════════╝${NC}"
echo ""

# Check if running as root
if [ "$EUID" -ne 0 ]; then 
    echo -e "${RED}Error: This script must be run as root${NC}"
    exit 1
fi

# Build the S6 init binary
echo -e "${YELLOW}[1/6]${NC} Building Eclipse S6..."
cargo build --release
if [ $? -ne 0 ]; then
    echo -e "${RED}Error: Failed to build Eclipse S6${NC}"
    exit 1
fi
echo -e "${GREEN}✓ Build complete${NC}"

# Install binary
echo -e "${YELLOW}[2/6]${NC} Installing binary..."
install -m 755 target/release/eclipse-s6 /sbin/eclipse-s6
echo -e "${GREEN}✓ Binary installed to /sbin/eclipse-s6${NC}"

# Create symbolic link for init
echo -e "${YELLOW}[3/6]${NC} Creating /sbin/init symlink..."
ln -sf /sbin/eclipse-s6 /sbin/init
echo -e "${GREEN}✓ Symlink created${NC}"

# Create directories
echo -e "${YELLOW}[4/6]${NC} Creating S6 directories..."
mkdir -p /run/service
mkdir -p /etc/s6/rc
mkdir -p /var/log/s6
echo -e "${GREEN}✓ Directories created${NC}"

# Install service definitions
echo -e "${YELLOW}[5/6]${NC} Installing service definitions..."
cp -r services/* /run/service/
chmod +x /run/service/*/run
chmod +x /run/service/*/log/run 2>/dev/null || true
echo -e "${GREEN}✓ Services installed${NC}"

# Set permissions
echo -e "${YELLOW}[6/6]${NC} Setting permissions..."
chown -R root:root /run/service
chown -R root:root /etc/s6
chown -R root:root /var/log/s6
echo -e "${GREEN}✓ Permissions set${NC}"

echo ""
echo -e "${GREEN}╔══════════════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║           Eclipse S6 Installation Complete!             ║${NC}"
echo -e "${GREEN}╚══════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "To start S6 as init, reboot the system or run:"
echo -e "  ${BLUE}/sbin/eclipse-s6${NC}"
echo ""
echo -e "To control services, use:"
echo -e "  ${BLUE}eclipse-s6 start <service>${NC}"
echo -e "  ${BLUE}eclipse-s6 stop <service>${NC}"
echo -e "  ${BLUE}eclipse-s6 restart <service>${NC}"
echo -e "  ${BLUE}eclipse-s6 status <service>${NC}"
echo ""
