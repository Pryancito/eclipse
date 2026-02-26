#!/bin/bash
# Script para ejecutar input_test en el host

set -e

# Mover temporalmente la config del workspace que fuerza build-std
if [ -f ../.cargo/config.toml ]; then
    mv ../.cargo/config.toml ../.cargo/config.toml.bak
    trap "mv ../.cargo/config.toml.bak ../.cargo/config.toml" EXIT
fi

# Ejecutar en el host
cargo run --target x86_64-unknown-linux-gnu "$@"
