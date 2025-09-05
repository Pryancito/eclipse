#!/bin/bash

echo "🚀 Iniciando Eclipse OS en QEMU..."

# Configuración optimizada para QEMU
QEMU_OPTS=(
    -machine q35
    -cpu host
    -smp 2
    -m 1G
    -drive file=eclipse-os.qcow2,format=qcow2
    -netdev user,id=net0,hostfwd=tcp::2222-:22
    -device e1000,netdev=net0
    -vga std
    -serial mon:stdio
    -no-reboot
    -no-shutdown
)

# Ejecutar QEMU
qemu-system-x86_64 "${QEMU_OPTS[@]}"
