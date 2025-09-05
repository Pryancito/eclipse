#!/bin/bash

# Script de compilación para Eclipse OS que funciona en QEMU (versión simple)
# Este script crea una imagen que funciona correctamente en QEMU

echo "🌙 Compilando Eclipse OS para QEMU (versión simple)..."
echo "====================================================="

# Configurar variables de entorno
export RUSTFLAGS="-C target-cpu=native -C opt-level=2"
export CARGO_TARGET_DIR="target_qemu"

# Crear directorio de compilación
mkdir -p $CARGO_TARGET_DIR

# 1. Compilar el kernel Eclipse
echo ""
echo "🔧 Paso 1: Compilando kernel Eclipse..."
cargo build --release --target x86_64-unknown-none --manifest-path eclipse_kernel/Cargo.toml

if [ $? -ne 0 ]; then
    echo "❌ Error compilando el kernel Eclipse"
    exit 1
fi

echo "✅ Kernel Eclipse compilado exitosamente"

# 2. Verificar que el bootloader UEFI esté compilado
echo ""
echo "🔧 Paso 2: Verificando bootloader UEFI..."
if [ ! -f "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi" ]; then
    echo "📦 Compilando bootloader UEFI..."
    cd bootloader-uefi
    ./build.sh
    if [ $? -ne 0 ]; then
        echo "❌ Error compilando bootloader UEFI"
        exit 1
    fi
    cd ..
fi

echo "✅ Bootloader UEFI verificado"

# 3. Crear imagen de disco simple para QEMU
echo ""
echo "🔧 Paso 3: Creando imagen de disco simple para QEMU..."

# Crear imagen de disco de 32MB
dd if=/dev/zero of=eclipse-os-qemu-simple.img bs=1M count=32

# Crear loop device
LOOP_DEV=$(sudo losetup -f --show eclipse-os-qemu-simple.img)
echo "📁 Loop device: $LOOP_DEV"

# Crear tabla de particiones MBR simple
sudo fdisk $LOOP_DEV << EOF
n
p
1

+30M
t
c
w
EOF

# Crear particiones
sudo partprobe $LOOP_DEV

# Formatear partición como FAT32
sudo mkfs.fat -F32 ${LOOP_DEV}p1

# Montar partición
sudo mkdir -p /mnt/eclipse-efi
sudo mount ${LOOP_DEV}p1 /mnt/eclipse-efi

# Crear estructura de directorios UEFI
sudo mkdir -p /mnt/eclipse-efi/EFI/BOOT

# Copiar bootloader UEFI
sudo cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-efi/EFI/BOOT/BOOTX64.EFI

# Copiar kernel Eclipse
sudo cp $CARGO_TARGET_DIR/x86_64-unknown-none/release/eclipse_kernel /mnt/eclipse-efi/

# Crear archivo de información
sudo tee /mnt/eclipse-efi/README.txt > /dev/null << 'INFO_EOF'
🌙 Eclipse OS - Sistema Operativo en Rust
=========================================

Versión: 1.0 QEMU Simple
Arquitectura: x86_64
Tipo: Imagen de disco para QEMU

Características:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado
- Optimizado para QEMU
- Sistema de debug integrado

Para probar en QEMU:
qemu-system-x86_64 -bios /usr/share/qemu/OVMF.fd -drive file=eclipse-os-qemu-simple.img,format=raw -m 512M

Desarrollado con ❤️ en Rust
INFO_EOF

# Desmontar partición
sudo umount /mnt/eclipse-efi
sudo rmdir /mnt/eclipse-efi

# Desconectar loop device
sudo losetup -d $LOOP_DEV

# 4. Crear script de prueba
echo ""
echo "🔧 Paso 4: Creando script de prueba..."
cat > test_qemu_simple.sh << 'SCRIPT_EOF'
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
    -serial stdio \
    -monitor stdio
SCRIPT_EOF

chmod +x test_qemu_simple.sh

# 5. Mostrar resumen
echo ""
echo "🎉 ¡Compilación completada exitosamente!"
echo "========================================"
echo ""
echo "📋 Archivos generados:"
echo "  - eclipse-os-qemu-simple.img (Imagen de disco para QEMU)"
echo "  - test_qemu_simple.sh (Script de prueba)"
echo "  - target_qemu/ (Directorio de compilación del kernel)"
echo ""
echo "🚀 Para probar en QEMU:"
echo "  ./test_qemu_simple.sh"
echo ""
echo "🔍 Características de la imagen:"
echo "  - Bootloader UEFI personalizado"
echo "  - Kernel Eclipse optimizado"
echo "  - Estructura MBR con partición FAT32"
echo "  - Formato compatible con QEMU"
echo ""
echo "✨ ¡Listo para probar en QEMU!"
