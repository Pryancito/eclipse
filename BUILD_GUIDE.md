# 🔨 Eclipse OS - Complete Build Guide

This guide provides comprehensive instructions for building Eclipse OS from source.

## Table of Contents

1. [System Requirements](#system-requirements)
2. [Installing Dependencies](#installing-dependencies)
3. [Setting Up Rust](#setting-up-rust)
4. [Building Components](#building-components)
5. [Running Tests](#running-tests)
6. [Creating Bootable Images](#creating-bootable-images)
7. [Troubleshooting](#troubleshooting)

---

## System Requirements

### Minimum Requirements
- **OS**: Linux (Ubuntu 20.04+, Debian 11+, Fedora 35+, Arch Linux)
- **CPU**: x86_64 processor
- **RAM**: 4GB (2GB minimum)
- **Disk**: 5GB free space
- **Internet**: Required for downloading dependencies

### Recommended Requirements
- **OS**: Ubuntu 22.04 LTS or newer
- **RAM**: 8GB or more
- **Disk**: 10GB+ free space
- **CPU**: Multi-core processor for faster builds

---

## Installing Dependencies

### Ubuntu/Debian

```bash
# Update package list
sudo apt-get update

# Install build essentials
sudo apt-get install -y \
    build-essential \
    git \
    curl \
    wget

# Install QEMU for testing
sudo apt-get install -y \
    qemu-system-x86 \
    ovmf

# Install additional tools
sudo apt-get install -y \
    nasm \
    mtools \
    xorriso
```

### Fedora

```bash
sudo dnf install -y \
    gcc \
    git \
    curl \
    qemu-system-x86 \
    edk2-ovmf \
    nasm \
    mtools \
    xorriso
```

### Arch Linux

```bash
sudo pacman -S --needed \
    base-devel \
    git \
    curl \
    qemu \
    edk2-ovmf \
    nasm \
    mtools \
    libisoburn
```

---

## Setting Up Rust

### Install Rust

```bash
# Install rustup (Rust installer)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Follow the prompts and choose default installation

# Add Rust to PATH (or restart terminal)
source $HOME/.cargo/env
```

### Configure Rust Toolchain

```bash
# Install nightly toolchain (required for Eclipse OS)
rustup toolchain install nightly

# Set nightly as default (optional)
rustup default nightly

# Or keep stable as default and use +nightly flag

# Add required targets
rustup target add x86_64-unknown-none --toolchain nightly
rustup target add x86_64-unknown-uefi --toolchain nightly

# Add required components
rustup component add rust-src --toolchain nightly
rustup component add llvm-tools-preview --toolchain nightly
```

### Verify Installation

```bash
# Check Rust version
rustc --version
# Should show: rustc 1.70+ 

# Check cargo version
cargo --version
# Should show: cargo 1.70+

# Check nightly toolchain
rustup toolchain list
# Should include: nightly-x86_64-unknown-linux-gnu

# Check targets
rustup target list --installed --toolchain nightly
# Should include:
# - x86_64-unknown-none
# - x86_64-unknown-uefi
```

---

## Building Components

### Clone the Repository

```bash
# Clone from GitHub
git clone https://github.com/Pryancito/eclipse.git
cd eclipse
```

### Build Order

Eclipse OS must be built in the following order:

1. **Userspace Programs** (init and services)
2. **Kernel** (embeds userspace binaries)
3. **Bootloader** (optional)

### 1. Build Init System

```bash
cd eclipse_kernel/userspace/init

# Build init
cargo +nightly build --release

# Verify binary
ls -lh target/x86_64-unknown-none/release/eclipse-init
# Should show ~15 KB binary

cd ../../..
```

### 2. Build Services

```bash
# Build all 5 services
cd eclipse_kernel/userspace

for service in filesystem_service network_service display_service audio_service input_service; do
    echo "Building $service..."
    cd $service
    cargo +nightly build --release
    cd ..
done

# Verify all binaries
ls -lh */target/x86_64-unknown-none/release/*_service
# Should show 5 binaries, each ~11 KB

cd ../..
```

### 3. Build Kernel

```bash
cd eclipse_kernel

# Build kernel (embeds init and services)
cargo +nightly build --release --target x86_64-unknown-none

# Verify kernel binary
ls -lh target/x86_64-unknown-none/release/eclipse_kernel
# Should show ~926 KB binary

cd ..
```

### 4. Build Bootloader (Optional)

```bash
cd bootloader-uefi

# Build UEFI bootloader
cargo +nightly build --release --target x86_64-unknown-uefi

# Verify bootloader
ls -lh target/x86_64-unknown-uefi/release/bootloader-uefi.efi
# Should show ~994 KB binary

cd ..
```

---

## Running Tests

### Automated Test Suite

```bash
# Run comprehensive test suite
./test_kernel.sh

# Expected output:
# ╔══════════════════════════════════════════════════════════════╗
# ║         Eclipse OS Kernel Test Suite v1.0                   ║
# ╚══════════════════════════════════════════════════════════════╝
# 
# Tests Passed:  11 / 13  (84.6%)
# Critical Tests: 11/11 (100%) ✅
```

### Manual Testing

```bash
# Test kernel build
cd eclipse_kernel
cargo +nightly test --target x86_64-unknown-none

# Test individual services
cd userspace/filesystem_service
cargo +nightly test

# Test init
cd ../init
cargo +nightly test
```

---

## Creating Bootable Images

### QEMU Testing

```bash
# Run kernel in QEMU (if you have full build system)
./qemu.sh

# Or manually:
qemu-system-x86_64 \
    -kernel eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel \
    -serial stdio \
    -display none
```

### Create ISO Image (if build system supports)

```bash
# Run build script
./build.sh

# This will create:
# - Kernel binary
# - Bootable ISO
# - USB image
```

---

## Build Optimization

### Fast Debug Build

```bash
# Build without optimizations (faster compilation)
cargo +nightly build --target x86_64-unknown-none
```

### Release Build (Optimized)

```bash
# Build with full optimizations (slower compilation, faster runtime)
cargo +nightly build --release --target x86_64-unknown-none
```

### Parallel Build

```bash
# Use all CPU cores
cargo +nightly build -j$(nproc) --release
```

### Clean Build

```bash
# Clean all build artifacts
cargo clean

# Then rebuild
cargo +nightly build --release
```

---

## Troubleshooting

### Build Fails: "linker not found"

```bash
# Install GCC/linker
sudo apt-get install build-essential

# Or on Fedora
sudo dnf install gcc
```

### Build Fails: "target not found"

```bash
# Re-add targets
rustup target add x86_64-unknown-none --toolchain nightly
rustup target add x86_64-unknown-uefi --toolchain nightly
```

### Build Fails: "nightly not installed"

```bash
# Install nightly toolchain
rustup toolchain install nightly

# Verify
rustup toolchain list
```

### Build Fails: "rust-src not found"

```bash
# Install rust-src component
rustup component add rust-src --toolchain nightly
```

### Out of Memory During Build

```bash
# Reduce parallel jobs
cargo build -j2 --release

# Or build components separately
cd userspace/init
cargo build --release
cd ../..
# Then kernel
cd eclipse_kernel
cargo build --release
```

### Slow Build Times

```bash
# Use sccache for caching
cargo install sccache
export RUSTC_WRAPPER=sccache

# Then build normally
cargo build --release
```

### Permission Denied

```bash
# Ensure proper permissions
chmod +x build.sh test_kernel.sh

# Run with proper permissions
./build.sh
```

---

## Build Artifacts

After a successful build, you'll have:

```
eclipse/
├── eclipse_kernel/
│   ├── target/x86_64-unknown-none/release/
│   │   └── eclipse_kernel          (926 KB - Main kernel)
│   └── userspace/
│       ├── init/target/.../eclipse-init          (15 KB)
│       ├── filesystem_service/target/...         (11 KB)
│       ├── network_service/target/...            (11 KB)
│       ├── display_service/target/...            (11 KB)
│       ├── audio_service/target/...              (11 KB)
│       └── input_service/target/...              (11 KB)
└── bootloader-uefi/
    └── target/x86_64-unknown-uefi/release/
        └── bootloader-uefi.efi     (994 KB - UEFI bootloader)

Total: ~1 MB for complete system
```

---

## Build Time Estimates

| Component | Debug Build | Release Build |
|-----------|-------------|---------------|
| Init | 10 seconds | 30 seconds |
| Service (each) | 10 seconds | 30 seconds |
| All Services | 50 seconds | 2.5 minutes |
| Kernel | 1 minute | 3 minutes |
| Bootloader | 30 seconds | 1 minute |
| **Total** | **~3 minutes** | **~7 minutes** |

*Times are approximate on a modern 4-core CPU with 8GB RAM*

---

## Advanced Topics

### Cross-Compilation

Eclipse OS is designed for x86_64, but the build system supports:

```bash
# Build for specific target
cargo +nightly build --target x86_64-unknown-none

# Custom target JSON (advanced)
cargo +nightly build --target custom-target.json
```

### Custom Linker Scripts

```bash
# Modify linker script
vim eclipse_kernel/linker.ld

# Then rebuild
cargo +nightly build --release
```

### Build Configuration

```bash
# Set custom features
cargo +nightly build --features "custom_feature"

# Disable default features
cargo +nightly build --no-default-features
```

---

## Continuous Integration

For automated builds in CI/CD:

```bash
# Install dependencies (CI environment)
sudo apt-get update && sudo apt-get install -y build-essential

# Install Rust (CI)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source $HOME/.cargo/env

# Setup toolchain
rustup toolchain install nightly
rustup target add x86_64-unknown-none --toolchain nightly
rustup component add rust-src --toolchain nightly

# Build
./test_kernel.sh
```

---

## Next Steps

After successful build:

1. **Test the system**: Run `./test_kernel.sh`
2. **Explore architecture**: See [ARCHITECTURE.md](ARCHITECTURE.md)
3. **Contribute**: See [CONTRIBUTING.md](CONTRIBUTING.md)
4. **Report issues**: Use GitHub Issues

---

## Getting Help

- **GitHub Issues**: https://github.com/Pryancito/eclipse/issues
- **Discussions**: https://github.com/Pryancito/eclipse/discussions
- **Quick Start**: See [QUICKSTART.md](QUICKSTART.md)

---

**Happy Building!** 🔨

Eclipse OS - A Modern Microkernel Operating System in Rust
