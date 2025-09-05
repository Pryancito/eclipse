#!/bin/bash

# Script de instalación avanzada de Eclipse OS v0.4.0
# Integrado con build.sh y con mejor detección de discos

set -e

echo "🌙 Instalador Avanzado de Eclipse OS v0.4.0"
echo "==========================================="
echo ""

# Colores para output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Función para mostrar mensajes con colores
log_info() {
    echo -e "${BLUE}ℹ️  $1${NC}"
}

log_success() {
    echo -e "${GREEN}✅ $1${NC}"
}

log_warning() {
    echo -e "${YELLOW}⚠️  $1${NC}"
}

log_error() {
    echo -e "${RED}❌ $1${NC}"
}

# Función para mostrar ayuda
show_help() {
    echo "Uso: $0 [OPCIONES] [DISCO]"
    echo ""
    echo "OPCIONES:"
    echo "  -h, --help           Mostrar esta ayuda"
    echo "  -a, --auto           Instalación automática (sin confirmación)"
    echo "  -f, --force          Forzar instalación sin confirmación"
    echo "  -b, --build          Compilar antes de instalar"
    echo "  -c, --check          Solo verificar dependencias y archivos"
    echo "  -l, --list           Listar discos disponibles y salir"
    echo "  -v, --verbose        Modo verboso"
    echo ""
    echo "DISCO:"
    echo "  Disco donde instalar Eclipse OS (ej: /dev/sda)"
    echo "  Si no se especifica, se mostrará un menú interactivo"
    echo ""
    echo "Ejemplos:"
    echo "  $0 /dev/sda"
    echo "  $0 --build --auto /dev/sda"
    echo "  $0 --check"
    echo "  $0 --list"
    echo ""
}

# Función para verificar dependencias
check_dependencies() {
    log_info "Verificando dependencias..."
    
    local missing_deps=()
    
    # Verificar comandos necesarios
    for cmd in parted wipefs mkfs.fat mkfs.ext4 lsblk mount umount sync partprobe; do
        if ! command -v "$cmd" &> /dev/null; then
            missing_deps+=("$cmd")
        fi
    done
    
    if [ ${#missing_deps[@]} -gt 0 ]; then
        log_error "Faltan dependencias: ${missing_deps[*]}"
        echo ""
        echo "Instala las dependencias con:"
        echo "  Ubuntu/Debian: sudo apt install parted dosfstools e2fsprogs util-linux"
        echo "  Arch Linux: sudo pacman -S parted dosfstools e2fsprogs util-linux"
        echo "  Fedora: sudo dnf install parted dosfstools e2fsprogs util-linux"
        return 1
    fi
    
    log_success "Todas las dependencias están instaladas"
    return 0
}

# Función para verificar archivos necesarios
check_files() {
    log_info "Verificando archivos necesarios..."
    
    local missing_files=()
    
    # Verificar kernel
    if [ ! -f "eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel" ]; then
        missing_files+=("eclipse_kernel")
    fi
    
    # Verificar bootloader
    if [ ! -f "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi" ]; then
        missing_files+=("bootloader-uefi")
    fi
    
    if [ ${#missing_files[@]} -gt 0 ]; then
        log_error "Faltan archivos: ${missing_files[*]}"
        echo ""
        echo "Compila los componentes faltantes con:"
        echo "  ./build.sh"
        return 1
    fi
    
    log_success "Todos los archivos necesarios están disponibles"
    return 0
}

# Función para listar discos disponibles
list_disks() {
    log_info "Discos disponibles:"
    echo ""
    
    # Usar lsblk con formato mejorado
    lsblk -d -o NAME,SIZE,MODEL,TYPE,MOUNTPOINT | while IFS= read -r line; do
        if [[ $line == *"disk"* ]]; then
            echo "  $line"
        fi
    done
    
    echo ""
    log_info "Discos virtuales (si existen):"
    
    # Buscar discos virtuales comunes
    for disk in /dev/loop* /dev/vd* /dev/nvme*; do
        if [ -b "$disk" ]; then
            local size=$(lsblk -d -o SIZE "$disk" | tail -n1)
            local model=$(lsblk -d -o MODEL "$disk" | tail -n1)
            echo "  $disk $size $model"
        fi
    done
}

# Función para seleccionar disco interactivamente
select_disk() {
    log_info "Seleccionando disco..."
    
    # Obtener lista de discos
    local disks=()
    while IFS= read -r line; do
        if [[ $line == *"disk"* ]]; then
            local disk_name=$(echo "$line" | awk '{print $1}')
            disks+=("/dev/$disk_name")
        fi
    done < <(lsblk -d -o NAME,TYPE | grep disk)
    
    if [ ${#disks[@]} -eq 0 ]; then
        log_error "No se encontraron discos disponibles"
        return 1
    fi
    
    echo ""
    echo "Discos disponibles:"
    for i in "${!disks[@]}"; do
        local disk="${disks[$i]}"
        local info=$(lsblk -d -o SIZE,MODEL "$disk" | tail -n1)
        echo "  $((i+1)). $disk $info"
    done
    
    echo ""
    while true; do
        read -p "Selecciona un disco (1-${#disks[@]}): " choice
        if [[ "$choice" =~ ^[0-9]+$ ]] && [ "$choice" -ge 1 ] && [ "$choice" -le "${#disks[@]}" ]; then
            selected_disk="${disks[$((choice-1))]}"
            break
        else
            log_error "Selección inválida. Intenta de nuevo."
        fi
    done
    
    log_success "Disco seleccionado: $selected_disk"
}

# Función para verificar disco
verify_disk() {
    local disk=$1
    
    log_info "Verificando disco: $disk"
    
    if [ ! -b "$disk" ]; then
        log_error "$disk no es un dispositivo de bloque válido"
        return 1
    fi
    
    # Verificar que no esté montado
    if mount | grep -q "$disk"; then
        log_error "$disk tiene particiones montadas"
        echo "   Desmonta las particiones antes de continuar:"
        mount | grep "$disk" | awk '{print "   umount " $3}'
        return 1
    fi
    
    # Verificar tamaño mínimo (100MB)
    local size_bytes=$(lsblk -b -d -o SIZE "$disk" | tail -n1)
    local size_mb=$((size_bytes / 1024 / 1024))
    
    if [ $size_mb -lt 100 ]; then
        log_error "El disco es demasiado pequeño (mínimo 100MB, actual: ${size_mb}MB)"
        return 1
    fi
    
    log_success "Disco verificado correctamente (${size_mb}MB)"
    return 0
}

# Función para crear particiones
create_partitions() {
    local disk=$1
    
    log_info "Creando particiones en $disk..."
    
    # Limpiar tabla de particiones
    log_info "Limpiando tabla de particiones..."
    wipefs -a "$disk" 2>/dev/null || true
    
    # Crear tabla GPT
    log_info "Creando tabla de particiones GPT..."
    parted "$disk" mklabel gpt
    
    # Crear partición EFI (100MB)
    log_info "Creando partición EFI (100MB)..."
    parted "$disk" mkpart EFI fat32 1MiB 101MiB
    parted "$disk" set 1 esp on
    
    # Crear partición root (resto del disco)
    log_info "Creando partición root (resto del disco)..."
    parted "$disk" mkpart ROOT ext4 101MiB 100%
    
    # Sincronizar cambios
    sync
    partprobe "$disk"
    
    log_success "Particiones creadas exitosamente"
}

# Función para formatear particiones
format_partitions() {
    local disk=$1
    local efi_partition="${disk}1"
    local root_partition="${disk}2"
    
    log_info "Formateando particiones..."
    
    # Formatear partición EFI
    log_info "Formateando partición EFI como FAT32..."
    mkfs.fat -F32 -n "ECLIPSE_EFI" "$efi_partition"
    
    # Formatear partición root
    log_info "Formateando partición root como EXT4..."
    mkfs.ext4 -F -L "ECLIPSE_ROOT" "$root_partition"
    
    log_success "Particiones formateadas exitosamente"
}

# Función para instalar bootloader
install_bootloader() {
    local disk=$1
    local efi_partition="${disk}1"
    
    log_info "Instalando bootloader UEFI..."
    
    # Crear directorios de montaje
    mkdir -p /mnt/eclipse-efi
    
    # Montar partición EFI
    log_info "Montando partición EFI..."
    mount "$efi_partition" /mnt/eclipse-efi
    
    # Crear estructura EFI
    log_info "Creando estructura EFI..."
    mkdir -p /mnt/eclipse-efi/EFI/BOOT
    mkdir -p /mnt/eclipse-efi/EFI/eclipse
    
    # Copiar bootloader
    log_info "Instalando bootloader..."
    cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-efi/EFI/BOOT/BOOTX64.EFI
    cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-efi/EFI/eclipse/eclipse-bootloader.efi
    
    # Copiar kernel
    log_info "Instalando kernel..."
    cp eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel /mnt/eclipse-efi/eclipse_kernel
    
    # Crear archivos de configuración
    log_info "Creando archivos de configuración..."
    
    # Configuración del bootloader
    cat > /mnt/eclipse-efi/boot.conf << 'BOOT_CONF_EOF'
# Eclipse OS Boot Configuration v0.4.0
# ====================================

TIMEOUT=5
DEFAULT_ENTRY=eclipse
SHOW_MENU=true

[entry:eclipse]
title=Eclipse OS v0.4.0
description=Sistema Operativo Eclipse v0.4.0 - Kernel híbrido en Rust
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
Fecha: $(date)

Características:
- Kernel híbrido en Rust
- Bootloader UEFI personalizado
- Shell interactiva avanzada
- Sistema de archivos optimizado
- Gestión de energía y térmica

Desarrollado con ❤️ en Rust
README_EOF
    
    # Desmontar partición EFI
    umount /mnt/eclipse-efi
    rmdir /mnt/eclipse-efi
    
    log_success "Bootloader instalado exitosamente"
}

# Función principal de instalación
install_eclipse_os() {
    local disk=$1
    local auto_install=$2
    
    echo "🚀 Iniciando instalación de Eclipse OS v0.4.0..."
    echo "================================================"
    echo ""
    
    # Verificar disco
    if ! verify_disk "$disk"; then
        exit 1
    fi
    
    # Mostrar información del disco
    log_info "Disco seleccionado: $disk"
    lsblk "$disk"
    echo ""
    
    # Confirmar instalación (si no es automática)
    if [ "$auto_install" != "true" ]; then
        log_warning "ADVERTENCIA: Esto borrará TODOS los datos en $disk"
        read -p "¿Estás seguro de que quieres continuar? (escribe 'SI' para confirmar): " CONFIRM
        
        if [ "$CONFIRM" != "SI" ]; then
            log_error "Instalación cancelada"
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
    log_success "¡Instalación completada exitosamente!"
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
BUILD_FIRST="false"
CHECK_ONLY="false"
LIST_DISKS="false"
VERBOSE="false"
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
        -b|--build)
            BUILD_FIRST="true"
            shift
            ;;
        -c|--check)
            CHECK_ONLY="true"
            shift
            ;;
        -l|--list)
            LIST_DISKS="true"
            shift
            ;;
        -v|--verbose)
            VERBOSE="true"
            shift
            ;;
        -*)
            log_error "Opción desconocida: $1"
            show_help
            exit 1
            ;;
        *)
            DISK="$1"
            shift
            ;;
    esac
done

# Modo verboso
if [ "$VERBOSE" = "true" ]; then
    set -x
fi

# Verificar permisos de root
if [ "$EUID" -ne 0 ]; then
    log_error "Este script debe ejecutarse como root"
    echo "   Usa: sudo $0"
    exit 1
fi

# Verificar dependencias
if ! check_dependencies; then
    exit 1
fi

# Solo verificar archivos
if [ "$CHECK_ONLY" = "true" ]; then
    check_files
    exit $?
fi

# Solo listar discos
if [ "$LIST_DISKS" = "true" ]; then
    list_disks
    exit 0
fi

# Compilar si es necesario
if [ "$BUILD_FIRST" = "true" ]; then
    log_info "Compilando Eclipse OS..."
    if [ -f "build.sh" ]; then
        ./build.sh
    else
        log_error "build.sh no encontrado"
        exit 1
    fi
fi

# Verificar archivos
if ! check_files; then
    exit 1
fi

# Seleccionar disco si no se especificó
if [ -z "$DISK" ]; then
    select_disk
    DISK="$selected_disk"
fi

# Ejecutar instalación
install_eclipse_os "$DISK" "$AUTO_INSTALL"
