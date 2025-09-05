#!/bin/bash
echo "🚀 Iniciando Eclipse OS en QEMU (versión simple)..."
echo "=================================================="
echo ""
echo "Comandos disponibles:"
echo "  - Ctrl+Alt+G: Liberar mouse"
echo "  - Ctrl+Alt+F: Pantalla completa"
echo "  - Ctrl+Alt+Q: Salir"
echo ""
echo "Presiona Enter para continuar..."
read
qemu-system-x86_64 \
    -bios /usr/share/qemu/OVMF.fd \
    -drive file=eclipse-os-qemu-simple.img,format=raw \
    -m 512M \
    -serial stdio
