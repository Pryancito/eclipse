#!/bin/bash

echo "🔧 Creando imagen QEMU optimizada para Eclipse OS..."

# Crear imagen de disco QEMU
echo "💾 Creando imagen de disco QEMU..."
qemu-img create -f qcow2 eclipse-os.qcow2 2G

# Crear partición EFI en la imagen
echo "🔧 Configurando partición EFI..."
sudo losetup -fP eclipse-os.qcow2
LOOP_DEV=$(sudo losetup -j eclipse-os.qcow2 | cut -d: -f1)

# Crear tabla de particiones GPT
sudo /sbin/parted $LOOP_DEV mklabel gpt
sudo /sbin/parted $LOOP_DEV mkpart ESP fat32 1MiB 100MiB
sudo /sbin/parted $LOOP_DEV set 1 esp on
sudo /sbin/parted $LOOP_DEV mkpart primary ext4 100MiB 100%

# Formatear particiones
sudo /usr/sbin/mkfs.fat -F32 ${LOOP_DEV}p1
sudo mkfs.ext4 ${LOOP_DEV}p2

# Montar partición EFI
sudo mkdir -p /mnt/eclipse-efi
sudo mount ${LOOP_DEV}p1 /mnt/eclipse-efi

# Crear estructura de directorios EFI
sudo mkdir -p /mnt/eclipse-efi/EFI/BOOT

# Copiar bootloader UEFI
sudo cp efi/boot/bootx64.efi /mnt/eclipse-efi/EFI/BOOT/

# Crear configuración GRUB para QEMU
sudo mkdir -p /mnt/eclipse-efi/boot/grub
sudo tee /mnt/eclipse-efi/boot/grub/grub.cfg > /dev/null << 'GRUB_EOF'
set timeout=5
set default=0

menuentry "Eclipse OS - QEMU Optimized" {
    multiboot /boot/eclipse_kernel
    boot
}

menuentry "Eclipse OS - Debug Mode" {
    multiboot /boot/eclipse_kernel debug=1
    boot
}

menuentry "Eclipse OS - Safe Mode" {
    multiboot /boot/eclipse_kernel safe_mode=1
    boot
}
GRUB_EOF

# Copiar kernel
sudo cp boot/eclipse_kernel /mnt/eclipse-efi/boot/

# Desmontar
sudo umount /mnt/eclipse-efi
sudo losetup -d $LOOP_DEV

# Crear script de prueba QEMU
cat > test_qemu.sh << 'QEMU_EOF'
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
QEMU_EOF

chmod +x test_qemu.sh

echo "✅ Imagen QEMU creada exitosamente: eclipse-os.qcow2"
echo "📊 Tamaño del archivo:"
ls -lh eclipse-os.qcow2
echo ""
echo "🚀 Para probar en QEMU:"
echo "   ./test_qemu.sh"
echo ""
echo "🔧 Configuración QEMU optimizada:"
echo "   - CPU: host (máximo rendimiento)"
echo "   - RAM: 1GB"
echo "   - Red: NAT con port forwarding SSH (2222)"
echo "   - VGA: std (compatible)"
echo "   - Serial: mon:stdio (para debug)"
