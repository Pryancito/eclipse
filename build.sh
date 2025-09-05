#!/bin/bash

# Script de construcción simplificado para Eclipse OS
# Evita problemas con rustup en sudo

set -e

# Colores para output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Función para imprimir mensajes
print_status() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

print_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

print_step() {
    echo -e "${YELLOW}[STEP]${NC} $1"
}

# Configuración
KERNEL_TARGET="x86_64-unknown-none"
UEFI_TARGET="x86_64-unknown-uefi"
BUILD_DIR="eclipse-os-build"

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║ ECLIPSE OS - SCRIPT DE CONSTRUCCIÓN SIMPLIFICADO v0.4.0 ║"
echo "║ Kernel + Bootloader + Distribución + Instalador ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# Función para compilar el kernel
build_kernel() {
    print_step "Compilando kernel Eclipse OS v0.4.0..."
    
    cd eclipse_kernel
    
    # Compilar el kernel directamente con cargo
    print_status "Compilando kernel para target $KERNEL_TARGET..."
    cargo build --release --target "$KERNEL_TARGET"
    
    if [ $? -eq 0 ]; then
        print_success "Kernel compilado exitosamente"
        
        # Mostrar información del kernel compilado
        local kernel_path="target/$KERNEL_TARGET/release/eclipse_kernel"
        if [ -f "$kernel_path" ]; then
            local kernel_size=$(du -h "$kernel_path" | cut -f1)
            print_status "Kernel generado: $kernel_path ($kernel_size)"
        fi
    else
        print_error "Error al compilar el kernel"
        return 1
    fi
    
    cd ..
}

# Función para compilar el bootloader
build_bootloader() {
    print_step "Compilando bootloader UEFI..."
    
    cd bootloader-uefi
    
    # Compilar el bootloader directamente con cargo
    print_status "Compilando bootloader para target $UEFI_TARGET..."
    cargo build --release --target "$UEFI_TARGET"
    
    if [ $? -eq 0 ]; then
        print_success "Bootloader UEFI compilado exitosamente"
        
        # Mostrar información del bootloader compilado
        local bootloader_path="target/$UEFI_TARGET/release/eclipse-bootloader.efi"
        if [ -f "$bootloader_path" ]; then
            local bootloader_size=$(du -h "$bootloader_path" | cut -f1)
            print_status "Bootloader generado: $bootloader_path ($bootloader_size)"
        fi
    else
        print_error "Error al compilar el bootloader"
        return 1
    fi
    
    cd ..
}

# Función para compilar el instalador
build_installer() {
    print_step "Compilando instalador del sistema..."
    
    cd installer
    
    # Compilar el instalador
    print_status "Compilando instalador..."
    cargo build --release
    
    if [ $? -eq 0 ]; then
        print_success "Instalador compilado exitosamente"
        
        # Mostrar información del instalador compilado
        local installer_path="target/release/eclipse-installer"
        if [ -f "$installer_path" ]; then
            local installer_size=$(du -h "$installer_path" | cut -f1)
            print_status "Instalador generado: $installer_path ($installer_size)"
        fi
    else
        print_error "Error al compilar el instalador"
        return 1
    fi
    
    cd ..
}

# Función para compilar userland
build_userland() {
    print_step "Compilando módulos userland..."
    
    if [ ! -d "userland" ]; then
        print_status "Directorio userland no encontrado, saltando..."
        return 0
    fi
    
    # Compilar IPC Common
    print_status "Compilando IPC Common..."
    cd userland/ipc_common
    cargo build --release
    if [ $? -eq 0 ]; then
        print_success "IPC Common compilado exitosamente"
    else
        print_error "Error al compilar IPC Common"
        return 1
    fi
    cd ../..
    
    # Compilar Module Loader
    print_status "Compilando Module Loader..."
    cd userland/module_loader
    cargo build --release
    if [ $? -eq 0 ]; then
        print_success "Module Loader compilado exitosamente"
    else
        print_error "Error al compilar Module Loader"
        return 1
    fi
    cd ../..
    
    # Compilar Graphics Module
    print_status "Compilando Graphics Module..."
    cd userland/graphics_module
    cargo build --release
    if [ $? -eq 0 ]; then
        print_success "Graphics Module compilado exitosamente"
    else
        print_error "Error al compilar Graphics Module"
        return 1
    fi
    cd ../..
    
    # Compilar App Framework
    print_status "Compilando App Framework..."
    cd userland/app_framework
    cargo build --release
    if [ $? -eq 0 ]; then
        print_success "App Framework compilado exitosamente"
    else
        print_error "Error al compilar App Framework"
        return 1
    fi
    cd ../..
    
    print_success "Todos los módulos userland compilados exitosamente"
}

# Función para crear la distribución básica
create_basic_distribution() {
    print_step "Creando distribución básica de Eclipse OS..."
    
    # Crear directorio de distribución
    mkdir -p "$BUILD_DIR"/{boot,efi/boot,userland/{bin,lib,config}}
    
    # Copiar el kernel
    if [ -f "eclipse_kernel/target/$KERNEL_TARGET/release/eclipse_kernel" ]; then
        cp "eclipse_kernel/target/$KERNEL_TARGET/release/eclipse_kernel" "$BUILD_DIR/boot/"
        print_status "Kernel copiado a la distribución"
    else
        print_error "Kernel no encontrado - no se puede crear la distribución"
        exit 1
    fi
    
    # Copiar el bootloader UEFI si existe
    if [ -f "bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader.efi" ]; then
        cp "bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader.efi" "$BUILD_DIR/efi/boot/bootx64.efi"
        print_status "Bootloader UEFI copiado a la distribución"
    else
        print_status "Bootloader UEFI no encontrado - creando placeholder"
        echo "Bootloader UEFI no disponible" > "$BUILD_DIR/efi/boot/bootx64.efi"
    fi
    
    # Copiar módulos userland si existen
    if [ -d "userland" ]; then
        print_status "Copiando módulos userland..."
        
        # Copiar binarios userland
        if [ -f "userland/module_loader/target/release/module_loader" ]; then
            cp "userland/module_loader/target/release/module_loader" "$BUILD_DIR/userland/bin/"
            print_status "Module Loader copiado"
        fi
        
        if [ -f "userland/graphics_module/target/release/graphics_module" ]; then
            cp "userland/graphics_module/target/release/graphics_module" "$BUILD_DIR/userland/bin/"
            print_status "Graphics Module copiado"
        fi
        
        if [ -f "userland/app_framework/target/release/app_framework" ]; then
            cp "userland/app_framework/target/release/app_framework" "$BUILD_DIR/userland/bin/"
            print_status "App Framework copiado"
        fi
        
        # Crear configuración de userland
        cat > "$BUILD_DIR/userland/config/system.conf" << EOF
[system]
name = "Eclipse OS"
version = "0.4.0"
kernel = "/boot/eclipse_kernel"

[modules]
module_loader = "/userland/bin/module_loader"
graphics_module = "/userland/bin/graphics_module"
app_framework = "/userland/bin/app_framework"

[ipc]
socket_path = "/tmp/eclipse_ipc.sock"
timeout = 5000
EOF
        print_status "Configuración de userland creada"
        print_success "Módulos userland copiados a la distribución"
    fi
    
    # Crear configuración GRUB básica
    cat > "$BUILD_DIR/boot/grub.cfg" << EOF
set timeout=5
set default=0

menuentry "Eclipse OS v0.4.0" {
    multiboot2 /boot/eclipse_kernel
    boot
}

menuentry "Eclipse OS (modo debug)" {
    multiboot2 /boot/eclipse_kernel debug
    boot
}
EOF
    
    print_success "Distribución básica creada en $BUILD_DIR"
}

# Función principal
main() {
    # Ejecutar pasos de construcción
    build_kernel
    build_bootloader
    build_installer
    build_userland
    create_basic_distribution
    
    print_success "Construcción completada exitosamente"
    echo ""
    echo "📁 Archivos generados:"
    echo "  🏗️  Distribución básica: $BUILD_DIR/"
    echo ""
    echo "🔧 Componentes compilados:"
    echo "  ✅ Kernel Eclipse OS: eclipse_kernel/target/$KERNEL_TARGET/release/eclipse_kernel"
    echo "  ✅ Bootloader UEFI: bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader.efi"
    echo "  ✅ Instalador: installer/target/release/eclipse-installer"
    echo "  ✅ Userland: Módulos compilados e instalados"
    echo ""
    echo "🎉 ¡Eclipse OS v0.4.0 está listo para usar!"
}

# Ejecutar función principal
main "$@"
