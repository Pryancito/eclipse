#!/bin/bash

echo "🔧 Creando imagen de disco UEFI simple para Eclipse OS"
echo "====================================================="

# Crear imagen de disco de 64MB
echo "💾 Creando imagen de disco..."
dd if=/dev/zero of=eclipse-os-simple-uefi.img bs=1M count=64 status=progress

# Crear tabla de particiones GPT usando fdisk
echo "🔧 Configurando partición GPT..."
echo -e "g\nn\n\n\n+50M\nt\n1\nn\n\n\n\nw" | fdisk eclipse-os-simple-uefi.img

# Crear loop device
echo "🔗 Configurando loop device..."
sudo losetup -fP eclipse-os-simple-uefi.img
LOOP_DEV=$(sudo losetup -j eclipse-os-simple-uefi.img | cut -d: -f1)

# Formatear partición EFI
echo "📁 Formateando partición EFI..."
sudo /usr/sbin/mkfs.fat -F32 ${LOOP_DEV}p1

# Montar partición EFI
echo "📂 Montando partición EFI..."
sudo mkdir -p /mnt/eclipse-efi
sudo mount ${LOOP_DEV}p1 /mnt/eclipse-efi

# Crear estructura de directorios EFI
echo "📋 Creando estructura de directorios..."
sudo mkdir -p /mnt/eclipse-efi/EFI/BOOT
sudo mkdir -p /mnt/eclipse-efi/boot

# Copiar bootloader UEFI
echo "📦 Copiando bootloader UEFI..."
sudo cp efi/boot/bootx64.efi /mnt/eclipse-efi/EFI/BOOT/

# Copiar kernel
echo "🔧 Copiando kernel..."
sudo cp boot/eclipse_kernel /mnt/eclipse-efi/boot/

# Crear archivo de información
echo "📄 Creando archivo de información..."
sudo tee /mnt/eclipse-efi/README.txt > /dev/null << 'INFO_EOF'
Eclipse OS - Sistema Operativo en Rust
=====================================

Versión: 1.0
Arquitectura: x86_64
Tipo: Imagen de disco UEFI booteable

Características:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado
- Sistema de memoria avanzado
- Gestión de procesos multitarea
- Sistema de archivos virtual
- Drivers de hardware
- Stack de red completo
- Sistema de seguridad robusto
- Interfaz gráfica con soporte NVIDIA
- Sistema de AI avanzado
- Aplicaciones de usuario integradas

Aplicaciones incluidas:
- Shell interactivo
- Calculadora científica
- Gestor de archivos
- Información del sistema
- Editor de texto
- Gestor de tareas

Para instalar en USB:
sudo dd if=eclipse-os-simple-uefi.img of=/dev/sdX bs=4M status=progress

Para probar en QEMU:
qemu-system-x86_64 -drive file=eclipse-os-simple-uefi.img,format=raw -m 512M

Desarrollado con ❤️ en Rust
INFO_EOF

# Desmontar y limpiar
echo "🧹 Limpiando..."
sudo umount /mnt/eclipse-efi
sudo losetup -d $LOOP_DEV

# Crear script de prueba QEMU
cat > test_simple_uefi_disk.sh << 'QEMU_EOF'
#!/bin/bash

echo "🚀 Iniciando Eclipse OS desde imagen de disco UEFI simple..."

# Configuración QEMU para UEFI
QEMU_OPTS=(
    -machine q35
    -cpu host
    -smp 2
    -m 1G
    -drive file=eclipse-os-simple-uefi.img,format=raw
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

chmod +x test_simple_uefi_disk.sh

echo "✅ Imagen de disco UEFI simple creada: eclipse-os-simple-uefi.img"
echo "📊 Tamaño del archivo:"
ls -lh eclipse-os-simple-uefi.img
echo ""
echo "🚀 Para probar en QEMU:"
echo "   ./test_simple_uefi_disk.sh"
echo ""
echo "💾 Para copiar a USB:"
echo "   sudo dd if=eclipse-os-simple-uefi.img of=/dev/sdX bs=4M status=progress"
echo ""
echo "🔍 Verificar particiones:"
echo "   fdisk -l eclipse-os-simple-uefi.img"
