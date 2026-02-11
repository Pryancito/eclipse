#!/bin/bash
set -e

echo "Building userspace services..."

# Build libc first if needed (assuming it's a dependency)
# cd userspace/libc && cargo build --release && cd ../..

# List of services to build
SERVICES="log_service devfs_service filesystem_service network_service display_service audio_service input_service gui_service init"

for service in $SERVICES; do
    echo "Building $service..."
    if [ -d "eclipse_kernel/userspace/$service" ]; then
        cd "eclipse_kernel/userspace/$service"
        # Ensure Cargo.toml exists
        if [ ! -f "Cargo.toml" ]; then
            echo "Error: Cargo.toml not found for $service"
            exit 1
        fi
        
        # Build for the same target as kernel or custom userspace target?
        # The include path is `target/x86_64-unknown-none/release/service_name`
        # So we use x86_64-unknown-none target
        RUSTFLAGS="-C link-arg=-Tlinker.ld -C relocation-model=static" cargo +nightly build --release --target x86_64-unknown-none
        
        if [ $? -ne 0 ]; then
            echo "Failed to build $service"
            exit 1
        fi
        cd ../../..
    else
        echo "Directory eclipse_kernel/userspace/$service not found!"
        exit 1
    fi
done

echo "Building eclipse-systemd..."
if [ -d "eclipse-apps/systemd" ]; then
    cd eclipse-apps/systemd
    cargo clean
    RUSTFLAGS="-C no-redzone" cargo +nightly build --release --target x86_64-unknown-none
    if [ $? -ne 0 ]; then
        echo "Failed to build eclipse-systemd"
        exit 1
    fi
    cd ../..
else
    echo "Directory eclipse-apps/systemd not found!"
    exit 1
fi

echo "All userspace services built successfully."
