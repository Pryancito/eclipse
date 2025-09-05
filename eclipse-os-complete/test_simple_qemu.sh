#!/bin/bash

echo "🚀 Iniciando Eclipse OS en QEMU (modo simplificado)..."

# Configuración optimizada para QEMU
QEMU_OPTS=(
    -machine q35
    -cpu qemu64
    -smp 2
    -m 1G
    -drive file=eclipse-os-simple.qcow2,format=qcow2
    -kernel boot/eclipse_kernel
    -netdev user,id=net0,hostfwd=tcp::2222-:22
    -device e1000,netdev=net0
    -vga std
    -serial mon:stdio
    -no-reboot
    -no-shutdown
)

# Ejecutar QEMU
qemu-system-x86_64 "${QEMU_OPTS[@]}"
