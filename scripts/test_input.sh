#!/bin/bash
# Script de prueba del sistema de entrada de Eclipse OS

set -e

echo "================================================"
echo "Sistema de Pruebas de Entrada - Eclipse OS"
echo "================================================"
echo ""

# Colores para output
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Función para imprimir mensajes
print_success() {
    echo -e "${GREEN}✓${NC} $1"
}

print_error() {
    echo -e "${RED}✗${NC} $1"
}

print_info() {
    echo -e "${YELLOW}ℹ${NC} $1"
}

# Verificar que estamos en el directorio correcto
if [ ! -d "eclipse_kernel" ]; then
    print_error "Error: Debe ejecutar este script desde el directorio raíz del proyecto"
    exit 1
fi

print_info "Verificando dependencias..."

# Verificar Rust
if ! command -v rustc &> /dev/null; then
    print_error "Rust no está instalado"
    print_info "Instale Rust desde: https://rustup.rs/"
    exit 1
fi
print_success "Rust $(rustc --version)"

# Verificar nasm
if ! command -v nasm &> /dev/null; then
    print_error "NASM no está instalado"
    print_info "Instale NASM: sudo apt-get install nasm"
    exit 1
fi
print_success "NASM $(nasm --version | head -n1)"

# Verificar target x86_64-unknown-none
if ! rustup target list | grep "x86_64-unknown-none (installed)" &> /dev/null; then
    print_info "Instalando target x86_64-unknown-none..."
    rustup target add x86_64-unknown-none
    print_success "Target x86_64-unknown-none instalado"
else
    print_success "Target x86_64-unknown-none ya instalado"
fi

# Verificar QEMU (opcional)
if command -v qemu-system-x86_64 &> /dev/null; then
    print_success "QEMU $(qemu-system-x86_64 --version | head -n1)"
    QEMU_AVAILABLE=true
else
    print_info "QEMU no está instalado (opcional para testing)"
    QEMU_AVAILABLE=false
fi

echo ""
print_info "Compilando Eclipse Kernel..."
cd eclipse_kernel

# Compilar el kernel
if cargo build --release 2>&1 | tee /tmp/eclipse_build.log | grep -q "Finished"; then
    print_success "Kernel compilado exitosamente"
else
    print_error "Error al compilar el kernel"
    print_info "Revise /tmp/eclipse_build.log para más detalles"
    exit 1
fi

cd ..

echo ""
print_info "Verificando módulos del sistema de entrada..."

# Verificar archivos críticos
check_file() {
    if [ -f "$1" ]; then
        print_success "$1"
    else
        print_error "Falta: $1"
        return 1
    fi
}

FILES_OK=true
check_file "eclipse_kernel/src/drivers/keyboard.rs" || FILES_OK=false
check_file "eclipse_kernel/src/drivers/mouse.rs" || FILES_OK=false
check_file "eclipse_kernel/src/drivers/input_system.rs" || FILES_OK=false
check_file "eclipse_kernel/src/drivers/ps2_integration.rs" || FILES_OK=false
check_file "eclipse_kernel/src/idt.rs" || FILES_OK=false

if [ "$FILES_OK" = false ]; then
    print_error "Faltan archivos críticos del sistema de entrada"
    exit 1
fi

echo ""
print_info "Componentes del sistema de entrada:"
echo "  - Driver de Teclado PS/2"
echo "  - Driver de Ratón PS/2"
echo "  - Sistema de Entrada Unificado"
echo "  - Integración PS/2"
echo "  - Manejadores de Interrupciones (IRQ 1, IRQ 12)"

echo ""
if [ "$QEMU_AVAILABLE" = true ]; then
    print_info "Opciones de prueba disponibles:"
    echo ""
    echo "1. Ejecutar en QEMU (modo básico)"
    echo "   ./test_input.sh qemu"
    echo ""
    echo "2. Ejecutar en QEMU con opciones de debug"
    echo "   ./test_input.sh qemu-debug"
    echo ""
    echo "3. Solo compilar (ya completado)"
    echo ""
    
    if [ "$1" = "qemu" ]; then
        print_info "Ejecutando Eclipse OS en QEMU..."
        print_info "Pruebe el teclado y ratón en la ventana de QEMU"
        print_info "Use Ctrl+Alt+G para liberar el mouse"
        print_info "Use Ctrl+A, X para salir de QEMU"
        echo ""
        
        # Buscar el kernel compilado
        KERNEL_PATH="eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel"
        
        if [ ! -f "$KERNEL_PATH" ]; then
            print_error "No se encuentra el kernel compilado en: $KERNEL_PATH"
            exit 1
        fi
        
        # Ejecutar QEMU con soporte para teclado y ratón
        qemu-system-x86_64 \
            -kernel "$KERNEL_PATH" \
            -m 512M \
            -display gtk \
            -device usb-kbd \
            -device usb-mouse \
            -serial stdio \
            -no-reboot \
            -no-shutdown
            
    elif [ "$1" = "qemu-debug" ]; then
        print_info "Ejecutando Eclipse OS en QEMU (modo debug)..."
        print_info "Presione Ctrl+Alt+2 para ver el monitor de QEMU"
        print_info "Use 'info irq' en el monitor para ver estadísticas de interrupciones"
        echo ""
        
        KERNEL_PATH="eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel"
        
        if [ ! -f "$KERNEL_PATH" ]; then
            print_error "No se encuentra el kernel compilado en: $KERNEL_PATH"
            exit 1
        fi
        
        qemu-system-x86_64 \
            -kernel "$KERNEL_PATH" \
            -m 512M \
            -display gtk \
            -device usb-kbd \
            -device usb-mouse \
            -serial stdio \
            -monitor vc \
            -d int \
            -D /tmp/eclipse_qemu_debug.log \
            -no-reboot \
            -no-shutdown
            
        print_info "Log de debug guardado en: /tmp/eclipse_qemu_debug.log"
    fi
else
    print_info "Instale QEMU para ejecutar pruebas: sudo apt-get install qemu-system-x86"
fi

echo ""
print_success "Sistema de entrada compilado y listo"
echo ""
print_info "Para más información, consulte: INPUT_SYSTEM_DOCUMENTATION.md"
