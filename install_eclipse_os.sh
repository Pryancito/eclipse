#!/bin/bash

# Script de instalación directa de Eclipse OS
# Este script instala Eclipse OS directamente en un disco sin necesidad de ISO

echo "🌙 Instalador de Eclipse OS v0.4.0"
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
    if ! wipefs -a "$disk" 2>/dev/null; then
        echo "   ⚠️  Advertencia: No se pudo limpiar completamente la tabla de particiones"
    fi
    
    # Crear tabla GPT
    echo "   📋 Creando tabla de particiones GPT..."
    if ! parted "$disk" mklabel gpt; then
        echo "❌ Error: No se pudo crear tabla GPT en $disk"
        return 1
    fi
    
    # Crear partición EFI (100MB)
    echo "   💾 Creando partición EFI (100MB)..."
    if ! parted "$disk" mkpart EFI fat32 1MiB 101MiB; then
        echo "❌ Error: No se pudo crear partición EFI"
        return 1
    fi
    
    if ! parted "$disk" set 1 esp on; then
        echo "❌ Error: No se pudo marcar partición EFI como ESP"
        return 1
    fi
    
    # Crear partición root (resto del disco)
    echo "   🗂️  Creando partición root (resto del disco)..."
    if ! parted "$disk" mkpart ROOT ext4 101MiB 100%; then
        echo "❌ Error: No se pudo crear partición root"
        return 1
    fi
    
    # Sincronizar cambios
    echo "   🔄 Sincronizando cambios..."
    sync
    if ! partprobe "$disk"; then
        echo "   ⚠️  Advertencia: partprobe falló, pero las particiones deberían estar disponibles"
    fi
    
    # Verificar que las particiones existen
    sleep 2
    local part1="${disk}p1"
    local part2="${disk}p2"
    
    # Si no existen con 'p', probar sin 'p' (para discos SATA)
    if [ ! -b "$part1" ] && [ ! -b "${disk}1" ]; then
        echo "❌ Error: Las particiones no se crearon correctamente"
        return 1
    fi
    
    # Ajustar nombres de particiones según el tipo de disco
    if [ -b "$part1" ]; then
        # Disco loop o NVMe
        part1="${disk}p1"
        part2="${disk}p2"
    else
        # Disco SATA
        part1="${disk}1"
        part2="${disk}2"
    fi
    
    echo "✅ Particiones creadas exitosamente"
}

# Función para formatear particiones
format_partitions() {
    local disk=$1
    local efi_partition="${disk}p1"
    local root_partition="${disk}p2"
    
    # Ajustar nombres de particiones según el tipo de disco
    if [ ! -b "$efi_partition" ]; then
        efi_partition="${disk}1"
        root_partition="${disk}2"
    fi
    
    echo "🔧 Formateando particiones..."
    
    # Formatear partición EFI
    echo "   💾 Formateando partición EFI como FAT32..."
    if ! mkfs.fat -F32 -n "ECLIPSE_EFI" "$efi_partition"; then
        echo "❌ Error: No se pudo formatear partición EFI"
        return 1
    fi
    
    # Formatear partición root
    echo "   🗂️  Formateando partición root como EXT4..."
    if ! mkfs.ext4 -F -L "ECLIPSE_ROOT" "$root_partition"; then
        echo "❌ Error: No se pudo formatear partición root"
        return 1
    fi
    
    echo "✅ Particiones formateadas exitosamente"
}

# Función para instalar bootloader
install_bootloader() {
    local disk=$1
    local efi_partition="${disk}p1"
    
    # Ajustar nombres de particiones según el tipo de disco
    if [ ! -b "$efi_partition" ]; then
        efi_partition="${disk}1"
    fi
    
    echo "🔧 Instalando bootloader UEFI..."
    
    # Crear directorios de montaje
    mkdir -p /mnt/eclipse-efi
    
    # Montar partición EFI
    echo "   📁 Montando partición EFI..."
    if ! mount "$efi_partition" /mnt/eclipse-efi; then
        echo "❌ Error: No se pudo montar partición EFI"
        return 1
    fi
    
    # Crear estructura EFI
    echo "   📂 Creando estructura EFI..."
    mkdir -p /mnt/eclipse-efi/EFI/BOOT
    mkdir -p /mnt/eclipse-efi/EFI/eclipse
    
    # Copiar bootloader
    echo "   📦 Instalando bootloader..."
    if [ -f "bootloader-uefi/target_hardware/x86_64-unknown-uefi/release/eclipse-bootloader.efi" ]; then
        if ! cp bootloader-uefi/target_hardware/x86_64-unknown-uefi/release/eclipse-bootloader.efi /mnt/eclipse-efi/EFI/BOOT/BOOTX64.EFI; then
            echo "❌ Error: No se pudo copiar bootloader a EFI/BOOT/"
            return 1
        fi
        if ! cp bootloader-uefi/target_hardware/x86_64-unknown-uefi/release/eclipse-bootloader.efi /mnt/eclipse-efi/EFI/eclipse/eclipse-bootloader.efi; then
            echo "❌ Error: No se pudo copiar bootloader a EFI/eclipse/"
            return 1
        fi
    else
        echo "❌ Error: Bootloader no encontrado"
        echo "   Ejecuta: ./build.sh"
        return 1
    fi
    
    # Copiar kernel
    echo "   🧠 Instalando kernel..."
    if [ -f "target_hardware/x86_64-unknown-none/release/eclipse_kernel" ]; then
        if ! cp target_hardware/x86_64-unknown-none/release/eclipse_kernel /mnt/eclipse-efi/eclipse_kernel; then
            echo "❌ Error: No se pudo copiar kernel"
            return 1
        fi
    else
        echo "❌ Error: Kernel no encontrado"
        echo "   Ejecuta: ./build.sh"
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
description=Sistema Operativo Eclipse v0.4.0
kernel=/eclipse_kernel
initrd=
args=quiet splash
BOOT_CONF_EOF
    
    # README
    cat > /mnt/eclipse-efi/README.txt << 'README_EOF'
🌙 Eclipse OS - Sistema Operativo en Rust
=========================================

Versión: 0.4.0
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
    if ! create_partitions "$disk"; then
        echo "❌ Error: Falló la creación de particiones"
        exit 1
    fi
    
    # Formatear particiones
    if ! format_partitions "$disk"; then
        echo "❌ Error: Falló el formateo de particiones"
        exit 1
    fi
    
    # Instalar bootloader
    if ! install_bootloader "$disk"; then
        echo "❌ Error: Falló la instalación del bootloader"
        exit 1
    fi
    
    echo ""
    echo "🎉 ¡Instalación completada exitosamente!"
    echo "========================================"
    echo ""
    echo "📋 Resumen de la instalación:"
    echo "  - Disco: $disk"
    echo "  - Partición EFI: ${disk}1 (FAT32)"
    echo "  - Partición root: ${disk}2 (EXT4)"
    echo "  - Bootloader: UEFI"
    echo "  - Kernel: Eclipse OS v0.4.0"
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

# Función para verificar y compilar archivos necesarios
check_and_build_files() {
    local missing_files=()
    
    # Verificar kernel
    if [ ! -f "target_hardware/x86_64-unknown-none/release/eclipse_kernel" ]; then
        missing_files+=("kernel")
    fi
    
    # Verificar bootloader
    if [ ! -f "bootloader-uefi/target_hardware/x86_64-unknown-uefi/release/eclipse-bootloader.efi" ]; then
        missing_files+=("bootloader")
    fi
    
    # Si faltan archivos, compilar
    if [ ${#missing_files[@]} -gt 0 ]; then
        echo "🔧 Archivos faltantes detectados: ${missing_files[*]}"
        echo "   Compilando con build.sh..."
        
        if [ ! -f "build.sh" ]; then
            echo "❌ Error: build.sh no encontrado"
            echo "   Asegúrate de estar en el directorio raíz del proyecto"
            exit 1
        fi
        
        if ! ./build.sh; then
            echo "❌ Error: Falló la compilación con build.sh"
            exit 1
        fi
        
        echo "✅ Compilación completada exitosamente"
        echo ""
    fi
}

# Verificar y compilar archivos necesarios
check_and_build_files

# Ejecutar instalación
install_eclipse_os "$DISK" "$AUTO_INSTALL"
