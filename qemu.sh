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
MEMORY="16G"
CPUS="4"
USE_KVM="${USE_KVM:-1}"         # 1=activar KVM si está disponible, 0=forzar TCG (sin -enable-kvm)
USE_XHCI="${USE_XHCI:-1}"  # 1=XHCI (USB 3.0), 0=UHCI/EHCI (legacy)
# Por defecto usamos dispositivos de entrada SOLO USB (teclado + ratón HID)
# PS2_MOUSE=1 activa el modo PS/2 legacy (sin USB HID)
PS2_MOUSE="${PS2_MOUSE:-0}"  # 1=PS/2 legado, 0=USB HID (teclado+ratón, recomendado)
USB_PORTS_2="${USB_PORTS_2:-4}"  # Número de puertos USB 2.0
USB_PORTS_3="${USB_PORTS_3:-4}"  # Número de puertos USB 3.0
CREATE_USB_DISK="${CREATE_USB_DISK:-1}"  # Crear disco USB de prueba
# Disco: virtio (por defecto) o ahci (para probar driver AHCI)
USE_AHCI="${USE_AHCI:-0}"  # 1=AHCI SATA, 0=VirtIO
# VirtIO Net: 1=activado (por defecto), 0=desactivado
USE_NET="${USE_NET:-1}"
# VirtIO GPU: 1=activado (por defecto), 0=VGA estándar
VIRTIO_GPU="${VIRTIO_GPU:-1}"
# VirtIO tipo: vga (virtio-vga, GOP compatible) o gpu (virtio-gpu PCI-only)
# vga recomendado: OVMF encuentra GOP. gpu puede colgar en "buscando GOP" según la versión de OVMF.
VIRTIO_GPU_TYPE="${VIRTIO_GPU_TYPE:-vga}"
RESOLUTION="${RESOLUTION:-1280x1024}"
RES_X="${RESOLUTION%%x*}"
RES_Y="${RESOLUTION##*x}"
[ -z "$RES_X" ] && RES_X=1280
[ -z "$RES_Y" ] && RES_Y=1024

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
print_warning ">>> RATON: Pulsa Ctrl+Alt+G para capturar el raton en la ventana QEMU <<<"
print_info "Disco principal: $DISK"
print_info "Memoria: $MEMORY"
print_info "CPUs: $CPUS"
print_info "Controlador USB: $([ "$USE_XHCI" = "1" ] && echo "XHCI (USB 3.0)" || echo "Legacy (UHCI/EHCI)")"
print_info "Ratón: $([ "$PS2_MOUSE" = "1" ] && echo "PS/2 (modo legacy)" || echo "USB HID (recomendado)")"
print_info "Disco: $([ "$USE_AHCI" = "1" ] && echo "AHCI (SATA)" || echo "VirtIO")"
print_info "VirtIO GPU: $([ "$VIRTIO_GPU" = "1" ] && echo "Activado" || echo "Desactivado (VGA std)")"
print_info "VirtIO Net: $([ "$USE_NET" = "1" ] && echo "Activado" || echo "Desactivado")"
print_info "Resolución: $RESOLUTION (export RESOLUTION=1920x1080 para cambiar)"
print_info ""
print_info "Controles:"
print_info "  - Ctrl+Alt+G: IMPORTANTE - Capturar/soltar ratón en la ventana QEMU"
print_info "                (sin capturar, el ratón NO se mueve en Eclipse)"
print_info "  - Ctrl+A, X: Salir de QEMU"
print_info "  - Ctrl+A, C: Consola de monitor QEMU"
print_info ""
print_warning "IMPORTANTE: Para que el ratón funcione, pulsa Ctrl+Alt+G tras iniciar QEMU"
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

# Aceleración KVM opcional (solo si el host la soporta y no estamos en WSL)
if [ "$USE_KVM" = "1" ]; then
    if [ -e /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        # Detectar WSL (KVM suele no estar disponible o dar problemas)
        if ! grep -qi "microsoft" /proc/version 2>/dev/null; then
            print_info "KVM disponible: usando -enable-kvm y -cpu host"
            QEMU_CMD="$QEMU_CMD -enable-kvm"
        else
            print_warning "Entorno similar a WSL detectado; KVM desactivado (USE_KVM=0 para silenciar)"
        fi
    else
        print_warning "KVM no disponible (sin /dev/kvm RW); ejecutando en modo TCG (sin -enable-kvm)"
    fi
else
    print_info "KVM desactivado explícitamente (USE_KVM=0); ejecutando en modo TCG puro"
fi

# Agregar UEFI si está disponible
if [ -n "$OVMF_CODE" ] && [ -n "$OVMF_VARS" ]; then
    QEMU_CMD="$QEMU_CMD -drive if=pflash,format=raw,readonly=on,file=$OVMF_CODE"
    QEMU_CMD="$QEMU_CMD -drive if=pflash,format=raw,file=$TEMP_OVMF_VARS"
fi

# Configuración del disco: VirtIO (por defecto) o AHCI
if [ "$USE_AHCI" = "1" ]; then
    # AHCI: emula controlador SATA (para probar driver AHCI de Eclipse)
    QEMU_CMD="$QEMU_CMD -device ahci,id=ahci"
    QEMU_CMD="$QEMU_CMD -drive if=none,file=$DISK,format=raw,id=disk0"
    QEMU_CMD="$QEMU_CMD -device ide-hd,bus=ahci.0,drive=disk0"
    print_info "Disco configurado con AHCI (SATA)"
else
    # VirtIO: recomendado para rendimiento
    QEMU_CMD="$QEMU_CMD -drive file=$DISK,format=raw,if=virtio"
fi

# Configuración de red
if [ "$USE_NET" = "1" ]; then
    QEMU_CMD="$QEMU_CMD -netdev user,id=net0 -device e1000e,netdev=net0"
    print_info "Red configurada: SLIRP (User mode) con port forwarding (8080->80, 2222->22), dispositivo e1000e"
fi
QEMU_CMD="$QEMU_CMD -m $MEMORY"
QEMU_CMD="$QEMU_CMD -smp $CPUS"
QEMU_CMD="$QEMU_CMD -no-reboot"

# Configuración de display
# -vga none: nuestra VirtIO es la única pantalla (Smithay visible)
# virtio-vga: compatible GOP, OVMF lo encuentra. virtio-gpu: PCI-only, GOP puede fallar
if [ "$VIRTIO_GPU" = "1" ]; then
    if [ "$VIRTIO_GPU_TYPE" = "gpu" ]; then
        QEMU_CMD="$QEMU_CMD -device virtio-gpu,xres=$RES_X,yres=$RES_Y"
        print_info "VirtIO GPU: ${RES_X}x${RES_Y}"
    else
        QEMU_CMD="$QEMU_CMD -device virtio-vga,xres=$RES_X,yres=$RES_Y"
        print_info "VirtIO VGA: ${RES_X}x${RES_Y}"
    fi
else
    print_info "VGA estándar"
fi

# Configuración de dispositivos de entrada
if [ "$PS2_MOUSE" = "1" ]; then
    # Modo PS/2 legacy: teclado + ratón a través del i8042.
    print_info "Entrada: PS/2 legacy (i8042). Ctrl+Alt+G para capturar el ratón."
    if [ "$USE_XHCI" = "1" ]; then
        QEMU_CMD="$QEMU_CMD -device qemu-xhci,id=xhci,p2=$USB_PORTS_2,p3=$USB_PORTS_3"
    else
        QEMU_CMD="$QEMU_CMD -usb"
    fi
else
    # Modo USB puro: teclado y ratón/tablet 100% USB HID.
    # Se deshabilita el i8042 para que no haya fallback PS/2.
    print_info "Entrada: USB HID puro (teclado + tablet XHCI). PS/2 desactivado."
    if [ "$USE_XHCI" = "1" ]; then
        QEMU_CMD="$QEMU_CMD -device qemu-xhci,id=xhci"
        # Puerto 1: teclado USB HID
        QEMU_CMD="$QEMU_CMD -device usb-kbd,bus=xhci.0,port=1"
        # Puerto 2: tablet USB (puntero absoluto; no requiere Ctrl+Alt+G)
        QEMU_CMD="$QEMU_CMD -device usb-tablet,bus=xhci.0,port=2"
    else
        QEMU_CMD="$QEMU_CMD -usb"
        QEMU_CMD="$QEMU_CMD -device usb-kbd"
        QEMU_CMD="$QEMU_CMD -device usb-tablet"
    fi
fi
QEMU_CMD="$QEMU_CMD -no-shutdown"

# Puerto de monitor QEMU (para debugging)
QEMU_CMD="$QEMU_CMD -serial stdio"
#QEMU_CMD="$QEMU_CMD -monitor telnet:127.0.0.1:5555,server,nowait"

# Flags de debugging opcionales
#   DEBUG_QEMU=1 ./qemu.sh     -> activa -d int (log de interrupciones, muy lento sin KVM)
if [ "${DEBUG_QEMU:-0}" = "1" ]; then
    print_warning "DEBUG_QEMU=1: activando '-d int' (esto puede ser MUY lento sin KVM)"
    QEMU_CMD="$QEMU_CMD -d int"
fi

# Ejecutar QEMU
print_info "Comando QEMU:"
echo "$QEMU_CMD"
echo ""

exec $QEMU_CMD "$@"

# Cleanup (se ejecuta cuando QEMU termina)
cleanup() {
    if [ -f "$TEMP_OVMF_VARS" ]; then
        rm -f "$TEMP_OVMF_VARS"
        print_info "Variables OVMF temporales eliminadas"
    fi
}

trap cleanup EXIT

print_success "QEMU finalizado correctamente"
