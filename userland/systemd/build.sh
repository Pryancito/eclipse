#!/bin/bash
# Build script for eclipse-systemd

set -e

echo "Building Eclipse-SystemD..."
echo ""

# Check if rust nightly is installed
if ! command -v cargo +nightly &> /dev/null; then
    echo "Installing nightly Rust toolchain..."
    rustup toolchain install nightly
fi

# Check if rust-src component is installed
if ! rustup component list --toolchain nightly | grep -q "rust-src (installed)"; then
    echo "Adding rust-src component..."
    rustup component add rust-src --toolchain nightly
fi

# Build the systemd binary
echo "Compiling eclipse-systemd (release build)..."
cargo +nightly build --release

# Check if build succeeded
if [ $? -eq 0 ]; then
    echo ""
    echo "✓ Build successful!"
    echo ""
    echo "Binary location: target/x86_64-unknown-none/release/eclipse-systemd"
    ls -lh target/x86_64-unknown-none/release/eclipse-systemd
    echo ""
    file target/x86_64-unknown-none/release/eclipse-systemd
else
    echo ""
    echo "✗ Build failed!"
    exit 1
fi
