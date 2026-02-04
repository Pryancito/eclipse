# 🚀 Eclipse OS - Quick Start Guide

Get Eclipse OS running in **5 minutes**!

## Prerequisites

- Linux system (Ubuntu 20.04+ recommended)
- 2GB free disk space
- Internet connection

## Step 1: Install Dependencies (2 minutes)

```bash
# Update system
sudo apt-get update

# Install build tools
sudo apt-get install -y build-essential qemu-system-x86 ovmf

# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

## Step 2: Setup Rust Toolchain (1 minute)

```bash
# Install nightly Rust
rustup toolchain install nightly

# Add required targets
rustup target add x86_64-unknown-none --toolchain nightly
rustup target add x86_64-unknown-uefi --toolchain nightly
rustup component add rust-src --toolchain nightly
```

## Step 3: Clone and Build (2 minutes)

```bash
# Clone the repository
git clone https://github.com/Pryancito/eclipse.git
cd eclipse

# Build everything
cd eclipse_kernel
cargo +nightly build --release --target x86_64-unknown-none

# Build init and services
cd userspace/init
cargo +nightly build --release
cd ../..

# Services
for service in filesystem_service network_service display_service audio_service input_service; do
    cd userspace/$service
    cargo +nightly build --release
    cd ../..
done
```

## Step 4: Run! (30 seconds)

### Option A: Quick Test
```bash
# Run automated test suite
./test_kernel.sh
```

### Option B: Full System (requires full build)
```bash
# If you have the full build system
./build.sh
./qemu.sh
```

## What You Should See

```
╔══════════════════════════════════════════════════════════════╗
║              ECLIPSE OS INIT SYSTEM v0.2.0                   ║
╚══════════════════════════════════════════════════════════════╝

Init process started with PID: 1

[INIT] Phase 1: Mounting filesystems...
[INIT] Phase 2: Starting essential services...
  [SERVICE] Starting filesystem... started
[INIT] Phase 3: Starting system services...
  [SERVICE] Starting network... started
  [SERVICE] Starting display... started
  [SERVICE] Starting audio... started
  [SERVICE] Starting input... started
[INIT] Phase 4: Entering main loop...

[INIT] Heartbeat #1 - System operational
```

## Next Steps

- **Explore the system**: See [README.md](README.md) for full features
- **Build guide**: See [BUILD_GUIDE.md](BUILD_GUIDE.md) for detailed instructions
- **Architecture**: See [ARCHITECTURE.md](ARCHITECTURE.md) for system design
- **Development**: See [CONTRIBUTING.md](CONTRIBUTING.md) to contribute

## Troubleshooting

### Build Fails
```bash
# Clean and retry
cargo clean
cargo +nightly build --release
```

### Missing Tools
```bash
# Verify Rust installation
rustc --version
cargo --version
rustup --version

# Should show 1.70+ and nightly available
```

### QEMU Not Found
```bash
# Install QEMU
sudo apt-get install qemu-system-x86
```

## Quick Commands Reference

```bash
# Build kernel
cd eclipse_kernel && cargo +nightly build --release

# Run tests
./test_kernel.sh

# Clean everything
cargo clean
```

## Success! 🎉

You now have Eclipse OS running! The system is:
- ✅ Multi-process microkernel
- ✅ 5 independent services
- ✅ Complete process management
- ✅ Professional quality

**Enjoy exploring Eclipse OS!**

---

For detailed documentation, see:
- [README.md](README.md) - Full project overview
- [BUILD_GUIDE.md](BUILD_GUIDE.md) - Complete build instructions
- [ARCHITECTURE.md](ARCHITECTURE.md) - System architecture
- [ECLIPSE_OS_100_PERCENT_COMPLETE.md](ECLIPSE_OS_100_PERCENT_COMPLETE.md) - Achievement summary
