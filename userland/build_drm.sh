#!/bin/bash

# Script de compilación para el sistema DRM de Eclipse OS

echo "Compilando sistema DRM de Eclipse OS..."

# Compilar el módulo DRM
cd drm_display
cargo build --release
if [ $? -ne 0 ]; then
    echo "Error compilando módulo DRM"
    exit 1
fi

# Compilar el userland con DRM
cd ..
cargo build --release
if [ $? -ne 0 ]; then
    echo "Error compilando userland con DRM"
    exit 1
fi

# Compilar ejemplo DRM
cargo build --example drm_demo --release
if [ $? -ne 0 ]; then
    echo "Error compilando ejemplo DRM"
    exit 1
fi

echo "Compilación DRM completada exitosamente"
echo "Ejecutar con: cargo run --example drm_demo --release"
