#!/bin/bash
# Script de construcción para el microkernel Eclipse OS

set -e

echo "=== Building Eclipse Microkernel ==="

# Compilar con nightly Rust
echo "Compiling kernel..."
cargo +nightly build --target x86_64-eclipse-microkernel.json --release

echo "=== Build complete ==="
echo "Kernel binary: target/x86_64-unknown-none/release/eclipse_kernel"
