#!/bin/bash

# Script de instalación directa de Eclipse OS
# Este script instala Eclipse OS directamente en un disco sin necesidad de ISO

echo "🌙 Instalador de Eclipse OS v1.0"
echo "================================="
echo ""

# Verificar permisos de root
if [ "$EUID" -ne 0 ]; then
    echo "❌ Error: Este script debe ejecutarse como root"
    echo "   Usa: sudo ./install_eclipse_os.sh"
    exit 1
fi

# Función para mostrar ayuda
show_help() {
    echo "Uso: $0 [OPCIONES] DISCO"
    echo ""
    echo "OPCIONES:"
    echo "  -h, --help     Mostrar esta ayuda"
    echo "  -a, --auto     Instalación automática (sin confirmación)"
    echo "  -f, --force    Forzar instalación sin confirmación"
    echo ""
    echo "DISCO:"
    echo "  Disco donde instalar Eclipse OS (ej: /dev/sda)"
    echo ""
    echo "Ejemplos:"
    echo "  $0 /dev/sda"
    echo "  $0 --auto /dev/sda"
    echo "  $0 --force /dev/sda"
    echo ""
}

# Función para mostrar discos disponibles
show_disks() {
    echo "💾 Discos disponibles:"
    echo "====================="
    lsblk -d -o NAME,SIZE,MODEL,TYPE | grep disk | nl
    echo ""
}

# Función para verificar disco
verify_disk() {
    local disk=$1
    
    if [ ! -b "$disk" ]; then
        echo "❌ Error: $disk no es un dispositivo de bloque válido"
        return 1
    fi
    
    # Verificar que no esté montado
    if mount | grep -q "$disk"; then
        echo "❌ Error: $disk tiene particiones montadas"
        echo "   Desmonta las particiones antes de continuar"
        return 1
    fi
    
    return 0
}

# Función para crear particiones
create_partitions() {
    local disk=$1
    
    echo "🔧 Creando particiones en $disk..."
    
    # Limpiar tabla de particiones
    echo "   🗑️  Limpiando tabla de particiones..."
    wipefs -a "$disk" 2>/dev/null || true
    
    # Crear tabla GPT
    echo "   📋 Creando tabla de particiones GPT..."
    parted "$disk" mklabel gpt
    
    # Crear partición EFI (100MB)
    echo "   💾 Creando partición EFI (100MB)..."
    parted "$disk" mkpart EFI fat32 1MiB 101MiB
    parted "$disk" set 1 esp on
    
    # Crear partición root (resto del disco)
    echo "   🗂️  Creando partición root (resto del disco)..."
    parted "$disk" mkpart ROOT ext4 101MiB 100%
    
    # Sincronizar cambios
    sync
    partprobe "$disk"
    
    echo "✅ Particiones creadas exitosamente"
}

# Función para formatear particiones
format_partitions() {
    local disk=$1
    local efi_partition="${disk}1"
    local root_partition="${disk}2"
    
    echo "🔧 Formateando particiones..."
    
    # Formatear partición EFI
    echo "   💾 Formateando partición EFI como FAT32..."
    mkfs.fat -F32 -n "ECLIPSE_EFI" "$efi_partition"
    
    # Formatear partición root
    echo "   🗂️  Formateando partición root como EXT4..."
    mkfs.ext4 -F -L "ECLIPSE_ROOT" "$root_partition"
    
    echo "✅ Particiones formateadas exitosamente"
}

# Función para instalar bootloader
install_bootloader() {
    local disk=$1
    local efi_partition="${disk}1"
    
    echo "🔧 Instalando bootloader UEFI..."
    
    # Crear directorios de montaje
    mkdir -p /mnt/eclipse-efi
    
    # Montar partición EFI
    echo "   📁 Montando partición EFI..."
    mount "$efi_partition" /mnt/eclipse-efi
    
    # Crear estructura EFI
    echo "   📂 Creando estructura EFI..."
    mkdir -p /mnt/eclipse-efi/EFI/BOOT
    mkdir -p /mnt/eclipse-efi/EFI/eclipse
    
    # Copiar bootloader
    echo "   📦 Instalando bootloader..."
    if [ -f "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi" ]; then
        cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-efi/EFI/BOOT/BOOTX64.EFI
        cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-efi/EFI/eclipse/eclipse-bootloader.efi
    else
        echo "❌ Error: Bootloader no encontrado"
        echo "   Ejecuta: cd bootloader-uefi && ./build.sh"
        return 1
    fi
    
    # Copiar kernel
    echo "   🧠 Instalando kernel..."
    if [ -f "target_hardware/x86_64-unknown-none/release/eclipse_kernel" ]; then
        cp target_hardware/x86_64-unknown-none/release/eclipse_kernel /mnt/eclipse-efi/eclipse_kernel
    else
        echo "❌ Error: Kernel no encontrado"
        echo "   Ejecuta: cargo build --release"
        return 1
    fi
    
    # Crear archivos de configuración
    echo "   ⚙️  Creando archivos de configuración..."
    
    # Configuración del bootloader
    cat > /mnt/eclipse-efi/boot.conf << 'BOOT_CONF_EOF'
# Eclipse OS Boot Configuration
# =============================

TIMEOUT=5
DEFAULT_ENTRY=eclipse
SHOW_MENU=true

[entry:eclipse]
title=Eclipse OS
description=Sistema Operativo Eclipse v1.0
kernel=/eclipse_kernel
initrd=
args=quiet splash
BOOT_CONF_EOF
    
    # README
    cat > /mnt/eclipse-efi/README.txt << 'README_EOF'
🌙 Eclipse OS - Sistema Operativo en Rust
=========================================

Versión: 1.0
Arquitectura: x86_64
Tipo: Instalación en disco

Características:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado
- Sistema de archivos optimizado
- Interfaz gráfica moderna

Desarrollado con ❤️ en Rust
README_EOF
    
    # Desmontar partición EFI
    umount /mnt/eclipse-efi
    rmdir /mnt/eclipse-efi
    
    echo "✅ Bootloader instalado exitosamente"
}

# Función principal de instalación
install_eclipse_os() {
    local disk=$1
    local auto_install=$2
    
    echo "🚀 Iniciando instalación de Eclipse OS..."
    echo "========================================"
    echo ""
    
    # Verificar disco
    if ! verify_disk "$disk"; then
        exit 1
    fi
    
    # Mostrar información del disco
    echo "📀 Disco seleccionado: $disk"
    lsblk "$disk"
    echo ""
    
    # Confirmar instalación (si no es automática)
    if [ "$auto_install" != "true" ]; then
        echo "⚠️  ADVERTENCIA: Esto borrará TODOS los datos en $disk"
        read -p "¿Estás seguro de que quieres continuar? (escribe 'SI' para confirmar): " CONFIRM
        
        if [ "$CONFIRM" != "SI" ]; then
            echo "❌ Instalación cancelada"
            exit 1
        fi
    fi
    
    # Crear particiones
    create_partitions "$disk"
    
    # Formatear particiones
    format_partitions "$disk"
    
    # Instalar bootloader
    install_bootloader "$disk"
    
    echo ""
    echo "🎉 ¡Instalación completada exitosamente!"
    echo "========================================"
    echo ""
    echo "📋 Resumen de la instalación:"
    echo "  - Disco: $disk"
    echo "  - Partición EFI: ${disk}1 (FAT32)"
    echo "  - Partición root: ${disk}2 (EXT4)"
    echo "  - Bootloader: UEFI"
    echo "  - Kernel: Eclipse OS v1.0"
    echo ""
    echo "🔄 Reinicia el sistema para usar Eclipse OS"
    echo ""
    echo "💡 Consejos:"
    echo "  - Asegúrate de que UEFI esté habilitado en tu BIOS"
    echo "  - Selecciona el disco como dispositivo de arranque"
    echo "  - Si no arranca, verifica la configuración UEFI"
}

# Procesar argumentos
AUTO_INSTALL="false"
DISK=""

while [[ $# -gt 0 ]]; do
    case $1 in
        -h|--help)
            show_help
            exit 0
            ;;
        -a|--auto)
            AUTO_INSTALL="true"
            shift
            ;;
        -f|--force)
            AUTO_INSTALL="true"
            shift
            ;;
        -*)
            echo "❌ Opción desconocida: $1"
            show_help
            exit 1
            ;;
        *)
            DISK="$1"
            shift
            ;;
    esac
done

# Si no se especificó disco, mostrar discos disponibles
if [ -z "$DISK" ]; then
    show_disks
    echo "Uso: $0 [OPCIONES] DISCO"
    echo "Ejemplo: $0 /dev/sda"
    exit 1
fi

# Verificar que los archivos necesarios existen
if [ ! -f "target_hardware/x86_64-unknown-none/release/eclipse_kernel" ]; then
    echo "❌ Error: Kernel no encontrado"
    echo "   Ejecuta: cargo build --release"
    exit 1
fi

if [ ! -f "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi" ]; then
    echo "❌ Error: Bootloader no encontrado"
    echo "   Ejecuta: cd bootloader-uefi && ./build.sh"
    exit 1
fi

# Ejecutar instalación
install_eclipse_os "$DISK" "$AUTO_INSTALL"
