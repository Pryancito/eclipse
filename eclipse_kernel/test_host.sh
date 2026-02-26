#!/bin/bash
# test_host.sh - Run kernel unit tests on the host

# Navigate to kernel directory if not already there
cd "$(dirname "$0")"

CONFIG=".cargo/config.toml"
BAK=".cargo/config.toml.bak"

if [ ! -f "$CONFIG" ] && [ ! -f "$BAK" ]; then
    echo "Error: .cargo/config.toml not found."
    exit 1
fi

# Function to restore on exit
cleanup() {
    if [ -f "$BAK" ]; then
        mv "$BAK" "$CONFIG"
    fi
}
trap cleanup EXIT

# Disable the kernel config
if [ -f "$CONFIG" ]; then
    mv "$CONFIG" "$BAK"
fi

echo "Running kernel unit tests on host (x86_64-unknown-linux-gnu)..."
# We exclude the kernel binary itself as it's #![no_main] and causes link errors on host
# We test the library parts, specifically usb_hid for now.
# --test-threads=1 is CRITICAL because tests share global mock statics (KEYS/MOUSE)
cargo test --lib usb_hid --target x86_64-unknown-linux-gnu -- --test-threads=1 "$@"
