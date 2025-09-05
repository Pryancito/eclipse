#!/bin/bash

# Script para reinstalar Eclipse OS con bootloader estable
# Este script soluciona el problema del reseteo automático

echo "🌙 Reinstalación de Eclipse OS - Bootloader Estable"
echo "=================================================="
echo ""

# Verificar permisos de root
if [ "$EUID" -ne 0 ]; then
    echo "❌ Error: Este script debe ejecutarse como root"
    echo "   Usa: sudo ./reinstall_stable.sh"
    exit 1
fi

# Mostrar discos disponibles
echo "💾 Discos disponibles:"
lsblk -d -o NAME,SIZE,MODEL,TYPE | grep disk | nl
echo ""

# Solicitar disco de destino
read -p "Ingresa el nombre del disco donde reinstalar (ej: /dev/sda): " DISK

if [ ! -b "$DISK" ]; then
    echo "❌ Error: $DISK no es un dispositivo de bloque válido"
    exit 1
fi

# Confirmar reinstalación
echo ""
echo "⚠️  ADVERTENCIA: Esto borrará TODOS los datos en $DISK"
read -p "¿Estás seguro de que quieres continuar? (escribe 'SI' para confirmar): " CONFIRM

if [ "$CONFIRM" != "SI" ]; then
    echo "❌ Reinstalación cancelada"
    exit 1
fi

echo ""
echo "🚀 Iniciando reinstalación con bootloader estable..."
echo ""

# Verificar que los archivos necesarios existen
if [ ! -f "target_hardware/x86_64-unknown-none/release/eclipse_kernel" ]; then
    echo "🔧 Compilando kernel Eclipse..."
    cargo build --release --target x86_64-unknown-none --manifest-path eclipse_kernel/Cargo.toml
    
    if [ $? -ne 0 ]; then
        echo "❌ Error compilando kernel"
        exit 1
    fi
    
    echo "✅ Kernel compilado exitosamente"
    echo ""
fi

if [ ! -f "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi" ]; then
    echo "🔧 Compilando bootloader estable..."
    cd bootloader-uefi
    ./build.sh
    if [ $? -ne 0 ]; then
        echo "❌ Error compilando bootloader"
        exit 1
    fi
    cd ..
    
    echo "✅ Bootloader estable compilado exitosamente"
    echo ""
fi

# 3. Crear particiones
echo "🔧 Paso 3: Creando particiones..."
wipefs -a "$DISK" 2>/dev/null || true
parted "$DISK" mklabel gpt
parted "$DISK" mkpart EFI fat32 1MiB 101MiB
parted "$DISK" set 1 esp on
parted "$DISK" mkpart ROOT ext4 101MiB 100%
sync
partprobe "$DISK"

echo "✅ Particiones creadas exitosamente"
echo ""

# 4. Formatear particiones
echo "🔧 Paso 4: Formateando particiones..."
mkfs.fat -F32 -n "ECLIPSE_EFI" "${DISK}1"
mkfs.ext4 -F -L "ECLIPSE_ROOT" "${DISK}2"

echo "✅ Particiones formateadas exitosamente"
echo ""

# 5. Montar particiones
echo "🔧 Paso 5: Montando particiones..."
mkdir -p /mnt/eclipse-efi
mkdir -p /mnt/eclipse-root
mount "${DISK}1" /mnt/eclipse-efi
mount "${DISK}2" /mnt/eclipse-root

echo "✅ Particiones montadas exitosamente"
echo ""

# 6. Instalar bootloader estable
echo "🔧 Paso 6: Instalando bootloader estable..."
mkdir -p /mnt/eclipse-efi/EFI/BOOT
mkdir -p /mnt/eclipse-efi/EFI/eclipse

# Copiar bootloader estable
cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-efi/EFI/BOOT/BOOTX64.EFI
cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-efi/EFI/eclipse/eclipse-bootloader.efi

echo "✅ Bootloader estable instalado"
echo ""

# 7. Instalar kernel
echo "🔧 Paso 7: Instalando kernel..."
cp target_hardware/x86_64-unknown-none/release/eclipse_kernel /mnt/eclipse-efi/eclipse_kernel

echo "✅ Kernel instalado"
echo ""

# 8. Crear archivos de configuración
echo "🔧 Paso 8: Creando archivos de configuración..."

# Configuración del bootloader
cat > /mnt/eclipse-efi/boot.conf << 'BOOT_CONF_EOF'
# Eclipse OS Boot Configuration - Versión Estable
# ===============================================

TIMEOUT=5
DEFAULT_ENTRY=eclipse
SHOW_MENU=true

[entry:eclipse]
title=Eclipse OS (Estable)
description=Sistema Operativo Eclipse v1.0 - Sin reseteo automático
kernel=/eclipse_kernel
initrd=
args=quiet splash
BOOT_CONF_EOF

# README actualizado
cat > /mnt/eclipse-efi/README.txt << 'README_EOF'
🌙 Eclipse OS - Sistema Operativo en Rust
=========================================

Versión: 1.0 (Estable)
Arquitectura: x86_64
Tipo: Instalación en disco
Estado: Sin reseteo automático

Características:
- Kernel microkernel en Rust
- Bootloader UEFI estable
- Sistema de archivos optimizado
- Sin reseteo automático

Desarrollado con ❤️ en Rust
README_EOF

echo "✅ Archivos de configuración creados"
echo ""

# 9. Desmontar particiones
echo "🔧 Paso 9: Desmontando particiones..."
umount /mnt/eclipse-efi
umount /mnt/eclipse-root
rmdir /mnt/eclipse-efi
rmdir /mnt/eclipse-root

echo "✅ Particiones desmontadas exitosamente"
echo ""

# 10. Mostrar resumen
echo "🎉 ¡Reinstalación completada exitosamente!"
echo "=========================================="
echo ""
echo "📋 Resumen de la reinstalación:"
echo "  - Disco: $DISK"
echo "  - Partición EFI: ${DISK}1 (FAT32)"
echo "  - Partición root: ${DISK}2 (EXT4)"
echo "  - Bootloader: UEFI Estable (sin reseteo automático)"
echo "  - Kernel: Eclipse OS v1.0"
echo ""
echo "🔧 Cambios realizados:"
echo "  ✅ Bootloader estable instalado"
echo "  ✅ Sin reseteo automático"
echo "  ✅ Bucle infinito para mantener el sistema activo"
echo "  ✅ Mensajes de estado periódicos"
echo ""
echo "🔄 Reinicia el sistema para usar Eclipse OS estable"
echo ""
echo "💡 El sistema ahora debería funcionar sin reseteos automáticos"
