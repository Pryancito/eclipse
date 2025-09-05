#!/bin/bash

# Script para reinstalar Eclipse OS con bootloader estable
# Este script soluciona el problema del reseteo automático

echo "🌙 Reinstalación de Eclipse OS v0.4.0 - Bootloader Estable"
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

# Función para verificar y compilar archivos necesarios
check_and_build_files() {
    local missing_files=()
    
    # Verificar kernel
    if [ ! -f "eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel" ]; then
        missing_files+=("kernel")
    fi
    
    # Verificar bootloader
    if [ ! -f "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader.efi" ]; then
        missing_files+=("bootloader")
    fi
    
    # Verificar userland (opcional)
    if [ -d "userland" ]; then
        if [ ! -f "userland/module_loader/target/release/module_loader" ] || 
           [ ! -f "userland/graphics_module/target/release/graphics_module" ] || 
           [ ! -f "userland/app_framework/target/release/app_framework" ]; then
            missing_files+=("userland")
        fi
    fi
    
    # Si faltan archivos, informar al usuario
    if [ ${#missing_files[@]} -gt 0 ]; then
        echo "❌ Error: Archivos faltantes detectados: ${missing_files[*]}"
        echo ""
        echo "🔧 Solución:"
        echo "   1. Ejecuta: ./build.sh"
        echo "   2. Luego ejecuta: sudo ./reinstall_stable.sh"
        echo ""
        exit 1
    fi
}

# Verificar y compilar archivos necesarios
check_and_build_files

# 3. Crear particiones
echo "🔧 Paso 3: Creando particiones..."
if ! wipefs -a "$DISK" 2>/dev/null; then
    echo "   ⚠️  Advertencia: No se pudo limpiar completamente la tabla de particiones"
fi

if ! parted "$DISK" mklabel gpt; then
    echo "❌ Error: No se pudo crear tabla GPT en $DISK"
    exit 1
fi

if ! parted "$DISK" mkpart EFI fat32 1MiB 101MiB; then
    echo "❌ Error: No se pudo crear partición EFI"
    exit 1
fi

if ! parted "$DISK" set 1 esp on; then
    echo "❌ Error: No se pudo marcar partición EFI como ESP"
    exit 1
fi

if ! parted "$DISK" mkpart ROOT ext4 101MiB 100%; then
    echo "❌ Error: No se pudo crear partición root"
    exit 1
fi

sync
if ! partprobe "$DISK"; then
    echo "   ⚠️  Advertencia: partprobe falló, pero las particiones deberían estar disponibles"
fi

# Verificar que las particiones existen
sleep 2
if [ ! -b "${DISK}1" ] || [ ! -b "${DISK}2" ]; then
    echo "❌ Error: Las particiones no se crearon correctamente"
    exit 1
fi

echo "✅ Particiones creadas exitosamente"
echo ""

# 4. Formatear particiones
echo "🔧 Paso 4: Formateando particiones..."
if ! mkfs.fat -F32 -n "ECLIPSE_EFI" "${DISK}1"; then
    echo "❌ Error: No se pudo formatear partición EFI"
    exit 1
fi

if ! mkfs.ext4 -F -L "ECLIPSE_ROOT" "${DISK}2"; then
    echo "❌ Error: No se pudo formatear partición root"
    exit 1
fi

echo "✅ Particiones formateadas exitosamente"
echo ""

# 5. Montar particiones
echo "🔧 Paso 5: Montando particiones..."
mkdir -p /mnt/eclipse-efi
mkdir -p /mnt/eclipse-root

if ! mount "${DISK}1" /mnt/eclipse-efi; then
    echo "❌ Error: No se pudo montar partición EFI"
    exit 1
fi

if ! mount "${DISK}2" /mnt/eclipse-root; then
    echo "❌ Error: No se pudo montar partición root"
    umount /mnt/eclipse-efi
    exit 1
fi

echo "✅ Particiones montadas exitosamente"
echo ""

# 6. Instalar bootloader estable
echo "🔧 Paso 6: Instalando bootloader estable..."
mkdir -p /mnt/eclipse-efi/EFI/BOOT
mkdir -p /mnt/eclipse-efi/EFI/eclipse

# Copiar bootloader estable
if ! cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-efi/EFI/BOOT/BOOTX64.EFI; then
    echo "❌ Error: No se pudo copiar bootloader a EFI/BOOT/"
    exit 1
fi

if ! cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-efi/EFI/eclipse/eclipse-bootloader.efi; then
    echo "❌ Error: No se pudo copiar bootloader a EFI/eclipse/"
    exit 1
fi

echo "✅ Bootloader estable instalado"
echo ""

# 7. Instalar kernel
echo "🔧 Paso 7: Instalando kernel..."
if ! cp eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel /mnt/eclipse-efi/eclipse_kernel; then
    echo "❌ Error: No se pudo copiar kernel"
    exit 1
fi

echo "✅ Kernel instalado"
echo ""

# 7.5. Instalar módulos userland
echo "🔧 Paso 7.5: Instalando módulos userland..."
if [ -d "userland" ]; then
    # Crear directorio para userland
    mkdir -p /mnt/eclipse-efi/userland/{bin,lib,config}
    
    # Copiar binarios userland
    if [ -f "userland/module_loader/target/release/module_loader" ]; then
        cp userland/module_loader/target/release/module_loader /mnt/eclipse-efi/userland/bin/
        echo "   ✓ Module Loader instalado"
    fi
    
    if [ -f "userland/graphics_module/target/release/graphics_module" ]; then
        cp userland/graphics_module/target/release/graphics_module /mnt/eclipse-efi/userland/bin/
        echo "   ✓ Graphics Module instalado"
    fi
    
    if [ -f "userland/app_framework/target/release/app_framework" ]; then
        cp userland/app_framework/target/release/app_framework /mnt/eclipse-efi/userland/bin/
        echo "   ✓ App Framework instalado"
    fi
    
    # Crear configuración de userland
    cat > /mnt/eclipse-efi/userland/config/system.conf << 'EOF'
# Eclipse OS Userland Configuration
# =================================

# Module settings
modules=graphics_module,audio_module,network_module,storage_module

# Applications
apps=terminal,filemanager,editor,monitor

# Graphics settings
graphics_mode=1920x1080x32
vga_fallback=true

# Memory settings
kernel_memory=64M
userland_memory=256M
EOF
    
    echo "   ✓ Configuración de userland creada"
    echo "✅ Módulos userland instalados"
else
    echo "⚠️  Advertencia: Directorio userland no encontrado"
fi
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
description=Sistema Operativo Eclipse v0.4.0 - Sin reseteo automático
kernel=/eclipse_kernel
initrd=
args=quiet splash
BOOT_CONF_EOF

# README actualizado
cat > /mnt/eclipse-efi/README.txt << 'README_EOF'
🌙 Eclipse OS - Sistema Operativo en Rust
=========================================

Versión: 0.4.0 (Estable)
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
echo "  - Kernel: Eclipse OS v0.4.0"
echo ""
echo "🔧 Cambios realizados:"
echo "  ✅ Bootloader estable instalado"
echo "  ✅ Sin reseteo automático"
echo "  ✅ Bucle infinito para mantener el sistema activo"
echo "  ✅ Mensajes de estado periódicos"
echo "  ✅ Módulos userland instalados (si disponibles)"
echo ""
echo "🔄 Reinicia el sistema para usar Eclipse OS estable"
echo ""
echo "💡 El sistema ahora debería funcionar sin reseteos automáticos"
