#!/bin/bash

echo "🔧 Creando imagen QEMU simplificada para Eclipse OS..."

# Crear imagen de disco QEMU más grande
echo "💾 Creando imagen de disco QEMU..."
qemu-img create -f qcow2 eclipse-os-simple.qcow2 4G

# Crear script de prueba QEMU simplificado
cat > test_simple_qemu.sh << 'QEMU_EOF'
#!/bin/bash

echo "🚀 Iniciando Eclipse OS en QEMU (modo simplificado)..."

# Configuración optimizada para QEMU
QEMU_OPTS=(
    -machine q35
    -cpu host
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
QEMU_EOF

chmod +x test_simple_qemu.sh

echo "✅ Imagen QEMU simplificada creada: eclipse-os-simple.qcow2"
echo "📊 Tamaño del archivo:"
ls -lh eclipse-os-simple.qcow2
echo ""
echo "🚀 Para probar en QEMU:"
echo "   ./test_simple_qemu.sh"
echo ""
echo "🔧 Configuración QEMU optimizada:"
echo "   - CPU: host (máximo rendimiento)"
echo "   - RAM: 1GB"
echo "   - Red: NAT con port forwarding SSH (2222)"
echo "   - VGA: std (compatible)"
echo "   - Serial: mon:stdio (para debug)"
echo "   - Kernel: carga directa (sin bootloader)"
