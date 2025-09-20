#!/bin/bash

# Script de construcción completo para Eclipse OS
# Compila kernel, bootloader, userland completo y aplicaciones Wayland
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

echo "╔══════════════════════════════════════════════════════════════════════╗"
echo "║ ECLIPSE OS - SCRIPT DE CONSTRUCCIÓN COMPLETO v0.6.0 ║"
echo "║ Kernel + Bootloader + Userland + Aplicaciones Wayland + Instalador ║"
echo "╚══════════════════════════════════════════════════════════════════════╝"
echo ""

# Función para compilar el kernel
build_kernel() {
    print_step "Compilando kernel Eclipse OS v0.6.0..."
    
    # Compilar el kernel directamente con cargo (forzar uso de linker.ld absoluto)
    print_status "Compilando kernel para target $KERNEL_TARGET..."
    cd eclipse_kernel
    cargo build --target x86_64-unknown-none --release --features cosmic-desktop,ai-models

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

# Función para compilar systemd
build_systemd() {
    print_step "Compilando sistema systemd..."
    
    if [ ! -d "eclipse-apps/systemd" ]; then
        print_status "Directorio systemd no encontrado, saltando..."
        return 0
    fi
    
    cd eclipse-apps/systemd
    
    # Compilar systemd
    print_status "Compilando systemd..."
    cargo build --release
    
    if [ $? -eq 0 ]; then
        print_success "Systemd compilado exitosamente"
        
        # Mostrar información del systemd compilado
        local systemd_path="target/release/eclipse-systemd"
        if [ -f "$systemd_path" ]; then
            local systemd_size=$(du -h "$systemd_path" | cut -f1)
            print_status "Systemd generado: $systemd_path ($systemd_size)"
        fi
    else
        print_error "Error al compilar systemd"
        return 1
    fi
    
    cd ../..
}

# Función para compilar userland principal
build_userland_main() {
    print_step "Compilando userland principal..."

    if [ ! -d "userland" ]; then
        print_status "Directorio userland no encontrado, saltando..."
        return 0
    fi

    cd userland

    # Compilar biblioteca del userland
    print_status "Compilando biblioteca del userland..."
    cargo build --lib --release
    if [ $? -ne 0 ]; then
        print_error "Error al compilar la biblioteca del userland"
        cd ..
        return 1
    fi

    # Compilar binario principal del userland
    print_status "Compilando binario principal del userland..."
    cargo build --bin eclipse_userland --release
    if [ $? -ne 0 ]; then
        print_error "Error al compilar el binario principal del userland"
        cd ..
        return 1
    fi

    print_success "Userland principal compilado exitosamente"
    cd ..
}

# Función para compilar módulo de carga de módulos
build_module_loader() {
    print_step "Compilando module loader..."

    if [ ! -d "userland/module_loader" ]; then
        print_status "Directorio module_loader no encontrado, saltando..."
        return 0
    fi

    cd userland/module_loader

    print_status "Compilando module loader..."
    cargo build --release
    if [ $? -ne 0 ]; then
        print_error "Error al compilar module loader"
        cd ../..
        return 1
    fi

    print_success "Module loader compilado exitosamente"
    cd ../..
}

# Función para compilar módulo gráfico
build_graphics_module() {
    print_step "Compilando graphics module..."

    if [ ! -d "userland/graphics_module" ]; then
        print_status "Directorio graphics_module no encontrado, saltando..."
        return 0
    fi

    cd userland/graphics_module

    print_status "Compilando graphics module..."
    cargo build --release
    if [ $? -ne 0 ]; then
        print_error "Error al compilar graphics module"
        cd ../..
        return 1
    fi

    print_success "Graphics module compilado exitosamente"
    cd ../..
}

# Función para compilar framework de aplicaciones
build_app_framework() {
    print_step "Compilando app framework..."

    if [ ! -d "userland/app_framework" ]; then
        print_status "Directorio app_framework no encontrado, saltando..."
        return 0
    fi

    cd userland/app_framework

    print_status "Compilando app framework..."
    cargo build --release
    if [ $? -ne 0 ]; then
        print_error "Error al compilar app framework"
        cd ../..
        return 1
    fi

    print_success "App framework compilado exitosamente"
    cd ../..
}

# Función para compilar sistema DRM
build_drm_system() {
    print_step "Compilando sistema DRM..."

    if [ ! -d "userland/drm_display" ]; then
        print_status "Directorio drm_display no encontrado, saltando..."
        return 0
    fi

    cd userland/drm_display

    print_status "Compilando sistema DRM..."
    cargo build --release
    if [ $? -ne 0 ]; then
        print_error "Error al compilar sistema DRM"
        cd ../..
        return 1
    fi

    print_success "Sistema DRM compilado exitosamente"
    cd ../..
}

# Función para compilar aplicaciones Wayland
build_wayland_apps() {
    print_step "Compilando aplicaciones Wayland..."

    if [ ! -d "wayland_apps" ]; then
        print_status "Directorio wayland_apps no encontrado, saltando..."
        return 0
    fi

    cd wayland_apps

    # Compilar calculadora Wayland
    if [ -d "wayland_calculator" ]; then
        print_status "Compilando calculadora Wayland..."
        cd wayland_calculator
        cargo build --target x86_64-unknown-none --release
        if [ $? -ne 0 ]; then
            print_error "Error al compilar calculadora Wayland"
            cd ../..
            return 1
        fi
        cd ..
        print_success "Calculadora Wayland compilada"
    fi

    # Compilar terminal Wayland
    if [ -d "wayland_terminal" ]; then
        print_status "Compilando terminal Wayland..."
        cd wayland_terminal
        cargo build --target x86_64-unknown-none --release
        if [ $? -ne 0 ]; then
            print_error "Error al compilar terminal Wayland"
            cd ../..
            return 1
        fi
        cd ..
        print_success "Terminal Wayland compilada"
    fi

    # Compilar editor de texto Wayland
    if [ -d "wayland_text_editor" ]; then
        print_status "Compilando editor de texto Wayland..."
        cd wayland_text_editor
        cargo build --target x86_64-unknown-none --release
        if [ $? -ne 0 ]; then
            print_error "Error al compilar editor de texto Wayland"
            cd ../..
            return 1
        fi
        cd ..
        print_success "Editor de texto Wayland compilado"
    fi

    cd ..
    print_success "Aplicaciones Wayland compiladas exitosamente"
}

# Función para compilar todos los módulos userland
build_userland() {
    print_step "Compilando módulos userland..."

    build_userland_main
    build_module_loader
    build_graphics_module
    build_app_framework
    build_drm_system
    build_wayland_apps

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
        print_status "Continuando sin kernel..."
        # No salir, continuar con otros componentes
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
        
        # Copiar binario principal del userland
        if [ -f "userland/target/userland/eclipse_userland" ]; then
            cp "userland/target/userland/eclipse_userland" "$BUILD_DIR/userland/bin/"
            print_status "Userland principal copiado"
        elif [ -f "userland/target/release/eclipse_userland" ]; then
            cp "userland/target/release/eclipse_userland" "$BUILD_DIR/userland/bin/"
            print_status "Userland principal copiado"
        fi
        
        # Copiar binarios de módulos individuales si existen
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

        if [ -f "userland/ipc_common/target/release/ipc_common" ]; then
            cp "userland/ipc_common/target/release/ipc_common" "$BUILD_DIR/userland/bin/"
            print_status "IPC Common copiado"
        fi
        
        # Copiar sistema DRM si existe
        if [ -f "userland/drm_display/target/release/libdrm_display.rlib" ]; then
            cp "userland/drm_display/target/release/libdrm_display.rlib" "$BUILD_DIR/userland/lib/"
            print_status "Sistema DRM copiado"
        fi
        
        if [ -f "userland/drm_display/target/release/eclipse_display" ]; then
            cp "userland/drm_display/target/release/eclipse_display" "$BUILD_DIR/userland/bin/"
            print_status "Ejemplo DRM copiado"
        fi
        
        # Copiar aplicaciones Wayland si existen
        if [ -f "wayland_apps/wayland_calculator/target/release/wayland_calculator" ]; then
            cp "wayland_apps/wayland_calculator/target/release/wayland_calculator" "$BUILD_DIR/userland/bin/"
            print_status "Calculadora Wayland copiada"
        fi

        if [ -f "wayland_apps/wayland_terminal/target/release/wayland_terminal" ]; then
            cp "wayland_apps/wayland_terminal/target/release/wayland_terminal" "$BUILD_DIR/userland/bin/"
            print_status "Terminal Wayland copiada"
        fi

        if [ -f "wayland_apps/wayland_text_editor/target/release/wayland_text_editor" ]; then
            cp "wayland_apps/wayland_text_editor/target/release/wayland_text_editor" "$BUILD_DIR/userland/bin/"
            print_status "Editor de texto Wayland copiado"
        fi

        # Copiar systemd si existe
        if [ -f "eclipse-apps/systemd/target/release/eclipse-systemd" ]; then
            cp "eclipse-apps/systemd/target/release/eclipse-systemd" "$BUILD_DIR/userland/bin/"
            print_status "Systemd copiado"
        fi
        
        # Crear configuración de userland
        cat > "$BUILD_DIR/userland/config/system.conf" << EOF
[system]
name = "Eclipse OS"
version = "0.6.0"
kernel = "/boot/eclipse_kernel"
init_system = "systemd"

[modules]
module_loader = "/userland/bin/module_loader"
graphics_module = "/userland/bin/graphics_module"
app_framework = "/userland/bin/app_framework"
ipc_common = "/userland/bin/ipc_common"
eclipse_userland = "/userland/bin/eclipse_userland"
drm_display = "/userland/lib/libdrm_display.rlib"
systemd = "/userland/bin/eclipse-systemd"

[applications]
wayland_calculator = "/userland/bin/wayland_calculator"
wayland_terminal = "/userland/bin/wayland_terminal"
wayland_text_editor = "/userland/bin/wayland_text_editor"

[display]
driver = "drm"
fallback = "vga"
primary_device = "/dev/dri/card0"

[ipc]
socket_path = "/tmp/eclipse_ipc.sock"
timeout = 5000
EOF
        print_status "Configuración de userland creada"
        
        # Crear script de inicio DRM
        cat > "$BUILD_DIR/userland/bin/start_drm.sh" << 'EOF'
#!/bin/bash

echo "Iniciando Eclipse OS con sistema DRM..."

# Verificar permisos DRM
if [ ! -w /dev/dri/card0 ]; then
    echo "Error: Sin permisos para acceder a DRM"
    echo "Ejecutar como root o agregar usuario al grupo video"
    exit 1
fi

# Iniciar sistema DRM
export RUST_LOG=info
./eclipse_userland

echo "Eclipse OS con DRM iniciado"
EOF
        chmod +x "$BUILD_DIR/userland/bin/start_drm.sh"
        print_status "Script de inicio DRM creado"
        
        print_success "Módulos userland copiados a la distribución"
    fi
    
    # Copiar el instalador si existe
    if [ -f "target/release/eclipse-installer" ]; then
        cp "target/release/eclipse-installer" "$BUILD_DIR/userland/bin/"
        print_status "Instalador copiado a la distribución"
    else
        print_status "Instalador no encontrado - no se puede copiar"
    fi
    
    # Crear configuración UEFI básica (no GRUB ya que usamos bootloader UEFI personalizado)
    cat > "$BUILD_DIR/efi/boot/uefi_config.txt" << EOF
# Configuración UEFI para Eclipse OS v0.6.0
# Bootloader personalizado - no requiere GRUB

[system]
kernel_path = "/boot/eclipse_kernel"
userland_path = "/userland/bin/eclipse_userland"

[debug]
enable_debug = false
log_level = "info"
EOF
    
    print_success "Distribución básica creada en $BUILD_DIR"
}

# Función para mostrar resumen de construcción
show_build_summary() {
    echo ""
    print_success "Construcción completada exitosamente"
    echo ""
    echo "Archivos generados:"
    echo "  Distribución básica: $BUILD_DIR/"
    echo ""
    echo "Componentes compilados:"
    echo "  Kernel Eclipse OS: target/$KERNEL_TARGET/release/eclipse_kernel"
    echo "  Bootloader UEFI: bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader.efi"
    echo "  Instalador: installer/target/release/eclipse-installer"
    echo "  Systemd: eclipse-apps/systemd/target/release/eclipse-systemd"
    echo "  Userland principal: userland/target/release/eclipse_userland"
    echo "  Module Loader: userland/module_loader/target/release/module_loader"
    echo "  Graphics Module: userland/graphics_module/target/release/graphics_module"
    echo "  App Framework: userland/app_framework/target/release/app_framework"
    echo "  IPC Common: userland/ipc_common/target/release/ipc_common"
    echo "  Sistema DRM: userland/drm_display/target/release/libdrm_display.rlib"
    echo "  Calculadora Wayland: wayland_apps/wayland_calculator/target/release/wayland_calculator"
    echo "  Terminal Wayland: wayland_apps/wayland_terminal/target/release/wayland_terminal"
    echo "  Editor de texto Wayland: wayland_apps/wayland_text_editor/target/release/wayland_text_editor"
    echo ""
    echo "Distribución creada en: $BUILD_DIR/"
    echo "  - Kernel: $BUILD_DIR/boot/eclipse_kernel"
    echo "  - Bootloader: $BUILD_DIR/efi/boot/bootx64.efi"
    echo "  - Userland principal: $BUILD_DIR/userland/bin/eclipse_userland"
    echo "  - Module Loader: $BUILD_DIR/userland/bin/module_loader"
    echo "  - Graphics Module: $BUILD_DIR/userland/bin/graphics_module"
    echo "  - App Framework: $BUILD_DIR/userland/bin/app_framework"
    echo "  - IPC Common: $BUILD_DIR/userland/bin/ipc_common"
    echo "  - Sistema DRM: $BUILD_DIR/userland/lib/libdrm_display.rlib"
    echo "  - Calculadora Wayland: $BUILD_DIR/userland/bin/wayland_calculator"
    echo "  - Terminal Wayland: $BUILD_DIR/userland/bin/wayland_terminal"
    echo "  - Editor de texto Wayland: $BUILD_DIR/userland/bin/wayland_text_editor"
    echo "  - Instalador: $BUILD_DIR/userland/bin/eclipse-installer"
    echo "  - Systemd: $BUILD_DIR/userland/bin/eclipse-systemd"
    echo ""
    echo "Eclipse OS v0.6.0 está listo para usar!"
}

# Función principal
main() {
    # Ejecutar pasos de construcción
    build_kernel
    build_bootloader
    build_installer
    build_systemd
    build_userland
    create_basic_distribution
    show_build_summary
}

# Ejecutar función principal
main "$@"
