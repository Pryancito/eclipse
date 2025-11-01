#!/bin/bash

# Script para ejecutar Eclipse OS en QEMU usando el disco físico nvme0n1
# ADVERTENCIA: Este script requiere permisos de root para acceder al disco físico

set -e

# Colores para output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Función para imprimir mensajes
print_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

print_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

# Verificar que QEMU está instalado
if ! command -v qemu-system-x86_64 &> /dev/null; then
    print_error "QEMU no está instalado. Instálalo con:"
    echo "  Ubuntu/Debian: sudo apt install qemu-system-x86"
    echo "  Fedora: sudo dnf install qemu-system-x86"
    echo "  Arch: sudo pacman -S qemu"
    exit 1
fi

# Variables de configuración
DISK="${DISK:-eclipse_os.img}"  # Usar eclipse.img por defecto, /dev/nvme0n1 si se especifica DISK=/dev/nvme0n1
MEMORY="4G"
CPUS="2"
USE_XHCI="${USE_XHCI:-1}"  # 1=XHCI (USB 3.0), 0=UHCI/EHCI (legacy)
USB_PORTS_2="${USB_PORTS_2:-4}"  # Número de puertos USB 2.0
USB_PORTS_3="${USB_PORTS_3:-4}"  # Número de puertos USB 3.0
CREATE_USB_DISK="${CREATE_USB_DISK:-1}"  # Crear disco USB de prueba

# Verificar que el disco existe (archivo o dispositivo de bloque)
if [ ! -f "$DISK" ] && [ ! -b "$DISK" ]; then
    print_error "El disco $DISK no existe o no es accesible"
    print_info "Opciones:"
    print_info "  1. Crear imagen: ./create_disk_image.sh"
    print_info "  2. Usar disco físico: DISK=/dev/nvme0n1 sudo ./qemu.sh"
    exit 1
fi

if [ -f "$DISK" ]; then
    print_info "Usando imagen de disco: $DISK"
elif [ -b "$DISK" ]; then
    print_info "Usando disco físico: $DISK"
fi

print_info "=== Iniciando Eclipse OS en QEMU ==="
print_info "Disco principal: $DISK"
print_info "Memoria: $MEMORY"
print_info "CPUs: $CPUS"
print_info "Controlador USB: $([ "$USE_XHCI" = "1" ] && echo "XHCI (USB 3.0)" || echo "Legacy (UHCI/EHCI)")"
print_info ""
print_info "Controles:"
print_info "  - Ctrl+A, X: Salir de QEMU"
print_info "  - Ctrl+A, C: Consola de monitor QEMU"
print_info ""
print_warning "Nota: Para cambiar a USB legacy, ejecuta: USE_XHCI=0 sudo ./qemu.sh"
echo ""

# OVMF (UEFI firmware) - intentar diferentes ubicaciones comunes
OVMF_VARS=""
OVMF_CODE="/usr/share/OVMF/OVMF_CODE_4M.fd"

# Buscar OVMF en ubicaciones comunes
for path in \
    "/usr/share/OVMF/OVMF_VARS.fd" \
    "/usr/share/edk2/ovmf/OVMF_VARS.fd" \
    "/usr/share/qemu/OVMF_VARS.fd" \
    "/usr/share/ovmf/OVMF_VARS.fd"; do
    if [ -f "$path" ]; then
        OVMF_VARS="$path"
        break
    fi
done

for path in \
    "/usr/share/OVMF/OVMF_CODE_4M.fd" \
    "/usr/share/edk2/ovmf/OVMF_CODE.fd" \
    "/usr/share/qemu/OVMF_CODE.fd" \
    "/usr/share/ovmf/OVMF_CODE.fd"; do
    if [ -f "$path" ]; then
        OVMF_CODE="$path"
        break
    fi
done

# Si no encontramos OVMF, intentar usar el que viene con el instalador
if [ -z "$OVMF_VARS" ] || [ -z "$OVMF_CODE" ]; then
    if [ -f "installer/OVMF_VARS.fd" ]; then
        OVMF_VARS="/usr/share/OVMF/OVMF_VARS_4M.fd"
        OVMF_CODE="/usr/share/OVMF/OVMF_CODE_4M.fd"  # Asumimos que existe si existe OVMF_VARS
        print_info "Usando OVMF del directorio del instalador"
    fi
fi

# Crear variables OVMF temporales para esta sesión
TEMP_OVMF_VARS="./eclipse_ovmf_vars.fd"
if [ -n "$OVMF_VARS" ]; then
    cp "$OVMF_VARS" "$TEMP_OVMF_VARS"
    print_info "Usando firmware UEFI: $OVMF_CODE"
else
    print_warning "OVMF no encontrado. Ejecutando en modo legacy BIOS"
    print_info "Para UEFI completo, instala OVMF:"
    echo "  Ubuntu/Debian: sudo apt install ovmf"
    echo "  Fedora: sudo dnf install edk2-ovmf"
    echo "  Arch: sudo pacman -S edk2-ovmf"
fi

# Comando QEMU base
QEMU_CMD="qemu-system-x86_64"

# Agregar UEFI si está disponible
if [ -n "$OVMF_CODE" ] && [ -n "$OVMF_VARS" ]; then
    QEMU_CMD="$QEMU_CMD -drive if=pflash,format=raw,readonly=on,file=$OVMF_CODE"
    QEMU_CMD="$QEMU_CMD -drive if=pflash,format=raw,file=$TEMP_OVMF_VARS"
fi

# Configuración básica
# Usar IDE en lugar de VirtIO porque el driver VirtIO está simulando lecturas
QEMU_CMD="$QEMU_CMD -drive file=$DISK,format=raw,if=ide"
QEMU_CMD="$QEMU_CMD -m $MEMORY"
QEMU_CMD="$QEMU_CMD -smp $CPUS"
QEMU_CMD="$QEMU_CMD -enable-kvm"

# Configuración de display
QEMU_CMD="$QEMU_CMD -vga virtio"

# Configuración de dispositivos USB
if [ "$USE_XHCI" = "1" ]; then
    print_info "Usando controlador XHCI (USB 3.0)"
    print_info "Puertos USB 2.0: $USB_PORTS_2, Puertos USB 3.0: $USB_PORTS_3"
    
    # Agregar controlador XHCI
    QEMU_CMD="$QEMU_CMD -device qemu-xhci,id=xhci,p2=$USB_PORTS_2,p3=$USB_PORTS_3"
    
    # Agregar dispositivos USB al controlador XHCI
    QEMU_CMD="$QEMU_CMD -device usb-kbd,bus=xhci.0,port=1"
    QEMU_CMD="$QEMU_CMD -device usb-mouse,bus=xhci.0,port=2"
    QEMU_CMD="$QEMU_CMD -device usb-tablet,bus=xhci.0,port=3"
else
    print_warning "Usando controlador USB legacy (UHCI/EHCI)"
    QEMU_CMD="$QEMU_CMD -usb"
    QEMU_CMD="$QEMU_CMD -device usb-tablet"
    QEMU_CMD="$QEMU_CMD -device usb-kbd"
fi

# Puerto de monitor QEMU (para debugging)
QEMU_CMD="$QEMU_CMD -serial stdio"

# Ejecutar QEMU
print_info "Comando QEMU:"
echo "$QEMU_CMD"
echo ""

exec $QEMU_CMD

# Cleanup (se ejecuta cuando QEMU termina)
cleanup() {
    if [ -f "$TEMP_OVMF_VARS" ]; then
        rm -f "$TEMP_OVMF_VARS"
        print_info "Variables OVMF temporales eliminadas"
    fi
}

trap cleanup EXIT

print_success "QEMU finalizado correctamente"
