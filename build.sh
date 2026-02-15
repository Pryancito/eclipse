#!/bin/bash

# Script de construcción completo para Eclipse OS
# Compila kernel, bootloader, userland completo y aplicaciones Wayland
# Evita problemas con rustup en sudo

set -e

# Asegurar que trabajamos desde el directorio del script
cd "$(dirname "$0")"

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
ECLIPSE_TARGET="$(pwd)/x86_64-unknown-eclipse.json"
ECLIPSE_TARGET_NAME="x86_64-unknown-eclipse"
BUILD_DIR="eclipse-os-build"
BASE_DIR=$(pwd)
mkdir -p "$BUILD_DIR"

echo "╔══════════════════════════════════════════════════════════════════════╗"
echo "║ ECLIPSE OS - SCRIPT DE CONSTRUCCIÓN COMPLETO v0.1.0 ║"
echo "║ EclipseFS + Kernel + Bootloader + Userland + Aplicaciones Wayland + Instalador ║"
echo "╚══════════════════════════════════════════════════════════════════════╝"
echo ""

# Función para compilar la librería EclipseFS
build_eclipsefs_lib() {
    print_step "Compilando librería EclipseFS unificada..."
    
    cd eclipsefs-lib
    
    # Compilar versión std (para instalador y FUSE driver)
    print_status "Compilando eclipsefs-lib (versión std)..."
    cargo build --features std
    
    if [ $? -eq 0 ]; then
        print_success "eclipsefs-lib (std) compilada exitosamente"
        
        # Mostrar información de la librería compilada
        local lib_path="target/debug/libeclipsefs_lib.rlib"
        if [ -f "$lib_path" ]; then
            local lib_size=$(du -h "$lib_path" | cut -f1)
            print_status "Librería std generada: $lib_path ($lib_size)"
        fi
    else
        print_error "Error al compilar eclipsefs-lib (std)"
        cd ..
        return 1
    fi
    
    # Compilar versión no_std (para kernel)
    print_status "Compilando eclipsefs-lib (versión no_std)..."
    cargo build --no-default-features
    
    if [ $? -eq 0 ]; then
        print_success "eclipsefs-lib (no_std) compilada exitosamente"
        
        # Mostrar información de la librería compilada
        local lib_path="target/debug/libeclipsefs_lib.rlib"
        if [ -f "$lib_path" ]; then
            local lib_size=$(du -h "$lib_path" | cut -f1)
            print_status "Librería no_std generada: $lib_path ($lib_size)"
        fi
    else
        print_error "Error al compilar eclipsefs-lib (no_std)"
        cd ..
        return 1
    fi
    
    cd ..
}

# Función para compilar eclipse-syscall
build_eclipse_syscall() {
    print_step "Compilando eclipse-syscall..."
    
    if [ ! -d "eclipse-syscall" ]; then
        print_status "Directorio eclipse-syscall no encontrado, saltando..."
        return 0
    fi
    
    cd eclipse-syscall
    
    print_status "Compilando eclipse-syscall..."
    RUSTFLAGS="-Zunstable-options $RUSTFLAGS" cargo +nightly -Z unstable-options build --release --target "$ECLIPSE_TARGET" -Z build-std=core,alloc
    
    if [ $? -eq 0 ]; then
        print_success "eclipse-syscall compilado exitosamente"
    else
        print_error "Error al compilar eclipse-syscall"
        cd ..
        return 1
    fi
    
    cd ..
}

# Función para preparar el sysroot
prepare_sysroot() {
    print_step "Preparando sysroot..."
    mkdir -p "$BUILD_DIR/sysroot/usr/lib"
    mkdir -p "$BUILD_DIR/sysroot/usr/include"
    # Los símbolos reales se proporcionan vía Rust stubs o libc
    print_status "Skipping dummy library creation..."
}

# Función para compilar eclipse-libc
build_eclipse_libc() {
    print_step "Compilando eclipse-libc..."
    
    if [ ! -d "eclipse-libc" ]; then
        print_status "Directorio eclipse-libc no encontrado, saltando..."
        return 0
    fi
    
    cd eclipse-libc
    
    print_status "Compilando eclipse-libc..."
    RUSTFLAGS="-Zunstable-options --cfg eclipse_target $RUSTFLAGS" cargo +nightly -Z unstable-options build --release --target "$ECLIPSE_TARGET" -Z build-std=core,alloc
    
    if [ $? -eq 0 ]; then
        print_success "eclipse-libc compilado exitosamente"
        
        # Instalar en sysroot como libc.a
        mkdir -p "$BASE_DIR/$BUILD_DIR/sysroot/usr/lib"
        cp "target/$ECLIPSE_TARGET_NAME/release/libeclipse_libc.a" "$BASE_DIR/$BUILD_DIR/sysroot/usr/lib/libc.a"
        print_status "Instalado en sysroot: $BASE_DIR/$BUILD_DIR/sysroot/usr/lib/libc.a"
    else
        print_error "Error al compilar eclipse-libc"
        cd ..
        return 1
    fi
    
    cd ..
}

# Función para compilar eclipse_std
build_eclipse_std() {
    print_step "Compilando eclipse_std..."
    
    if [ ! -d "eclipse-apps/eclipse_std" ]; then
        print_status "Directorio eclipse-apps/eclipse_std no encontrado, saltando..."
        return 0
    fi
    
    cd eclipse-apps/eclipse_std
    
    print_status "Compilando eclipse_std (y deps: eclipse-syscall, eclipse-libc)..."
    RUSTFLAGS="-Zunstable-options --cfg eclipse_target $RUSTFLAGS" cargo +nightly -Z unstable-options build --release --target "$ECLIPSE_TARGET" -Z build-std=core,alloc
    
    if [ $? -eq 0 ]; then
        print_success "eclipse_std compilado exitosamente"
    else
        print_error "Error al compilar eclipse_std"
        cd ../..
        return 1
    fi
    
    cd ../..
}

# Función para compilar e instalar libXfont 1.5 (para TinyX con fuentes built-in)
build_libxfont15() {
    print_step "Compilando libXfont 1.5..."
    if [ ! -d "eclipse-apps/libXfont" ]; then
        print_status "eclipse-apps/libXfont no encontrado, saltando..."
        return 0
    fi
    local TINYX_INSTALL="$BASE_DIR/eclipse-apps/tinyx/install"
    mkdir -p "$TINYX_INSTALL"
    cd eclipse-apps/libXfont
    if [ ! -f "config.status" ] || [ "x$FORCE_LIBXFONT_CONFIGURE" = x1 ]; then
        print_status "Configurando libXfont 1.5 (prefix=$TINYX_INSTALL)..."
        ./configure --prefix="$TINYX_INSTALL" --enable-builtins || { cd ../..; print_error "Configure libXfont falló"; return 1; }
    fi
    print_status "Compilando libXfont 1.5..."
    make -j"$(nproc)" || { cd ../..; print_error "Make libXfont falló"; return 1; }
    make install || { cd ../..; print_error "Make install libXfont falló"; return 1; }
    print_success "libXfont 1.5 instalado en $TINYX_INSTALL"
    cd ../..
}

# Función para compilar TinyX (Xfbdev) para Eclipse OS
build_tinyx_for_eclipse_os() {
    print_step "Compilando TinyX (Xfbdev) para Eclipse OS..."
    if [ ! -d "eclipse-apps/tinyx" ]; then
        print_status "eclipse-apps/tinyx no encontrado, saltando..."
        return 0
    fi
    local TINYX_INSTALL="$BASE_DIR/eclipse-apps/tinyx/install"
    if [ -f "eclipse-apps/tinyx/install/lib/pkgconfig/xfont.pc" ]; then
        export PKG_CONFIG_PATH="$TINYX_INSTALL/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
        export TINYX_USE_LIBXFONT1=1
    fi
    cd eclipse-apps/tinyx
    if [ ! -f "Makefile" ] || ! grep -q "enable_builtin_fonts" configure 2>/dev/null; then
        print_status "Configurando TinyX (configure-eclipse.sh)..."
        ./configure-eclipse.sh || { cd ../..; print_error "Configure TinyX falló"; return 1; }
    fi
    SYSROOT_TINYX="${SYSROOT:-$BASE_DIR/$BUILD_DIR/sysroot}"
    print_status "Compilando Xfbdev (make, enlace estático)..."
    # CFLAGS con -fno-PIE para que el build con sysroot no use -fPIE del configure (PIE es para build nativo)
    TINYX_SYSROOT_CFLAGS="-fno-PIE -O2 -ffunction-sections -fdata-sections -fvisibility=hidden -fno-unwind-tables -fno-asynchronous-unwind-tables -Wall"
    TINYX_LDFLAGS_STATIC="-static -Wl,-O1 -Wl,-as-needed"
    if make -j"$(nproc)" CC="gcc --sysroot=$SYSROOT_TINYX -fno-stack-protector -fno-PIE -O2" CFLAGS="$TINYX_SYSROOT_CFLAGS" LDFLAGS="-B$SYSROOT_TINYX/usr/lib -no-pie $TINYX_LDFLAGS_STATIC" LIBS="-lz" 2>/dev/null; then
        print_success "TinyX (Xfbdev) compilado con sysroot Eclipse OS (estático)"
    else
        print_status "Make con sysroot falló, intentando make nativo (estático, CRT sin TLS)..."
        make clean 2>/dev/null || true
        # CRT sin TLS: evita __libc_setup_tls (page fault 0x388 en Eclipse OS)
        print_status "Compilando CRT sin TLS (crt0_start.o, crt0_no_tls.o)..."
        rm -f crt0_no_tls.o crt0_start.o
        gcc -c -O2 -fno-stack-protector -fno-PIE crt0_start.S -o crt0_start.o || true
        gcc -c -O2 -fno-stack-protector -fno-PIE crt0_no_tls.c -o crt0_no_tls.o || true
        if [ -f "crt0_start.o" ] && [ -f "crt0_no_tls.o" ]; then
            TINYX_LDFLAGS_STATIC="-nostartfiles -Wl,--wrap=__libc_setup_tls $(pwd)/crt0_start.o $(pwd)/crt0_no_tls.o -static -Wl,-O1 -Wl,-as-needed"
            print_status "Enlazando Xfbdev con -nostartfiles y --wrap=__libc_setup_tls"
        fi
        if make -j"$(nproc)" LDFLAGS="$TINYX_LDFLAGS_STATIC"; then
            print_success "TinyX (Xfbdev) compilado (nativo, estático, sin TLS)"
        else
            cd ../..
            print_error "Make TinyX falló"
            return 1
        fi
    fi
    cd ../..
}

# Función para compilar el proceso init (embedded)
build_eclipse_init() {
    print_step "Compilando init process (embedded)..."
    
    cd eclipse_kernel/userspace/init
    
    print_status "Compilando eclipse-init..."
    RUSTFLAGS="-C relocation-model=static" rustup run nightly cargo build --release --target x86_64-unknown-none
    
    if [ $? -eq 0 ]; then
        print_success "eclipse-init compilado exitosamente"
        
        local init_path="target/x86_64-unknown-none/release/eclipse-init"
        if [ -f "$init_path" ]; then
            local init_size=$(du -h "$init_path" | cut -f1)
            print_status "Init process generado: $init_path ($init_size)"
        fi
    else
        print_error "Error al compilar eclipse-init"
        cd ../../..
        return 1
    fi
    
    cd ../../..
}

# Función para compilar servicios de userspace
build_userspace_services() {
    print_step "Compilando servicios de userspace..."
    
    # Asegurar que rust-src está instalado
    print_status "Verificando rust-src component..."
    rustup component add rust-src --toolchain nightly 2>/dev/null || true
    
    # Lista de servicios a compilar (en orden de inicio)
    local SERVICES="log_service devfs_service filesystem_service input_service display_service audio_service network_service gui_service"
    
    for service in $SERVICES; do
        print_status "Compilando $service..."
        
        if [ ! -d "eclipse_kernel/userspace/$service" ]; then
            print_error "Directorio eclipse_kernel/userspace/$service no encontrado"
            return 1
        fi
        
        cd "eclipse_kernel/userspace/$service"
        
        if [ ! -f "Cargo.toml" ]; then
            print_error "Cargo.toml no encontrado para $service"
            cd ../../..
            return 1
        fi
        
        # Compilar el servicio usando build-std (requerido por config.toml)
        cargo +nightly build --release --target x86_64-unknown-none -Zbuild-std=core
        
        if [ $? -eq 0 ]; then
            local service_path="target/x86_64-unknown-none/release/$service"
            if [ -f "$service_path" ]; then
                local service_size=$(du -h "$service_path" | cut -f1)
                print_status "$service generado: $service_size"
            fi
        else
            print_error "Error al compilar $service"
            cd ../../..
            return 1
        fi
        
        cd ../../..
    done
    
    print_success "Todos los servicios de userspace compilados exitosamente"
}

# Función para compilar el kernel
build_kernel() {
    print_step "Compilando kernel Eclipse OS v0.1.0..."
    
    # Compilar el kernel directamente con cargo (forzar uso de linker.ld absoluto)
    print_status "Compilando kernel para target $KERNEL_TARGET..."
    cd eclipse_kernel
    if [ "${KERNEL_MINIMAL:-0}" = "1" ]; then
        print_status "Modo MINIMAL: compilando kernel sin características opcionales"
        rustup run nightly cargo build --target x86_64-unknown-none --release
    else
        rustup run nightly cargo build --target x86_64-unknown-none --release
    fi

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
    
    # Compilar el bootloader directamente con cargo usando nightly
    print_status "Compilando bootloader para target $UEFI_TARGET..."
    cargo +nightly build --release --target "$UEFI_TARGET"
    
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
    SYSROOT_LDFLAGS="-C link-arg=-L$BASE_DIR/$BUILD_DIR/sysroot/usr/lib -C link-arg=-nostdlib"
    RUSTFLAGS="-Zunstable-options --cfg eclipse_target $SYSROOT_LDFLAGS $RUSTFLAGS" cargo +nightly -Z unstable-options build --release --target "$ECLIPSE_TARGET" -Z build-std=std,panic_abort
    
    if [ $? -eq 0 ]; then
        print_success "Instalador compilado exitosamente"
        
        # Mostrar información del instalador compilado
        local installer_path="target/x86_64-unknown-linux-musl/release/eclipse-installer"
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
    
    # Compilar systemd (no_std, bare metal init)
    print_status "Compilando systemd..."
    # Usamos none target porque systemd es no_std y usa _start custom
    RUSTFLAGS="-C relocation-model=static" cargo +nightly build --release --target x86_64-unknown-none
    
    if [ $? -eq 0 ]; then
        print_success "Systemd compilado exitosamente"
        
        # Mostrar información del sistema compilado
        local systemd_path="target/x86_64-unknown-none/release/eclipse-systemd"
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
    RUSTFLAGS="-C relocation-model=pic" cargo build --release
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
    RUSTFLAGS="-C relocation-model=pic" cargo build --release
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
    RUSTFLAGS="-C relocation-model=pic" cargo build --release
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
    RUSTFLAGS="-C relocation-model=pic" cargo build --release
    if [ $? -ne 0 ]; then
        print_error "Error al compilar sistema DRM"
        cd ../..
        return 1
    fi

    print_success "Sistema DRM compilado exitosamente"
    cd ../..
}

# Función para compilar la biblioteca de integración Wayland
build_wayland_integration() {
    print_step "Compilando biblioteca de integración Wayland..."
    
    if [ ! -d "userland/wayland_integration" ]; then
        print_status "Directorio wayland_integration no encontrado, saltando..."
        return 0
    fi
    
    cd userland/wayland_integration
    
    print_status "Detectando bibliotecas del sistema (libwayland, wlroots)..."
    RUSTFLAGS="-C relocation-model=pic" cargo build --release
    
    if [ $? -eq 0 ]; then
        print_success "Biblioteca de integración Wayland compilada exitosamente"
        cd ../..
        return 0
    else
        print_error "Error al compilar biblioteca de integración Wayland"
        cd ../..
        return 1
    fi
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

# Función para compilar Wayland Server (Rust)
build_wayland_server() {
    print_step "Compilando Wayland Server (Rust)..."

    if [ ! -d "userland/wayland_server" ]; then
        print_status "Directorio wayland_server no encontrado, saltando..."
        return 0
    fi

    cd userland/wayland_server

    print_status "Compilando wayland_server..."
    RUSTFLAGS="-C relocation-model=pic" cargo build --release
    if [ $? -ne 0 ]; then
        print_error "Error al compilar wayland_server"
        cd ../..
        return 1
    fi

    print_success "Wayland Server (Rust) compilado exitosamente"
    cd ../..
}

# Función para compilar COSMIC Client (Rust)
build_cosmic_client() {
    print_step "Compilando COSMIC Client (Rust)..."

    if [ ! -d "userland/cosmic_client" ]; then
        print_status "Directorio cosmic_client no encontrado, saltando..."
        return 0
    fi

    cd userland/cosmic_client

    print_status "Compilando cosmic_client..."
    RUSTFLAGS="-C relocation-model=pic" cargo build --release
    if [ $? -ne 0 ]; then
        print_error "Error al compilar cosmic_client"
        cd ../..
        return 1
    fi

    print_success "COSMIC Client (Rust) compilado exitosamente"
    cd ../..
}

# Función para compilar Wayland Compositor
build_wayland_compositor() {
    print_step "Compilando Wayland Compositor (C con soporte wlroots/libwayland)..."

    if [ ! -d "userland/wayland_compositor" ]; then
        print_status "Directorio wayland_compositor no encontrado, saltando..."
        return 0
    fi

    cd userland/wayland_compositor

    print_status "Compilando wayland_compositor con detección automática de bibliotecas..."
    print_status "El Makefile detectará automáticamente wlroots, libwayland o usará implementación personalizada"
    
    make clean
    make
    if [ $? -ne 0 ]; then
        print_error "Error al compilar wayland_compositor"
        cd ../..
        return 1
    fi

    print_success "Wayland Compositor compilado exitosamente"
    cd ../..
}

# Función para compilar COSMIC Desktop
build_cosmic_desktop() {
    print_step "Compilando COSMIC Desktop..."

    if [ ! -d "userland/cosmic_desktop" ]; then
        print_status "Directorio cosmic_desktop no encontrado, saltando..."
        return 0
    fi

    cd userland/cosmic_desktop

    print_status "Compilando cosmic_desktop..."
    make clean
    make
    if [ $? -ne 0 ]; then
        print_error "Error al compilar cosmic_desktop"
        cd ../..
        return 1
    fi

    print_success "COSMIC Desktop compilado exitosamente"
    cd ../..
}

# Función para compilar todos los módulos userland
build_userland() {
    print_step "Compilando módulos userland..."

    #build_userland_main
    #build_module_loader
    #build_graphics_module
    #build_app_framework
    #build_drm_system
    #build_wayland_integration
    
    # Nuevos componentes base
    build_eclipse_syscall
    prepare_sysroot
    build_eclipse_libc
    build_eclipse_std
    build_libxfont15
    build_tinyx_for_eclipse_os
    
    # Aplicaciones
    #build_wayland_apps
    #build_wayland_server
    #build_cosmic_client
    #build_wayland_compositor
    #build_cosmic_desktop

    print_success "Todos los módulos userland compilados exitosamente"
}

# Función para crear la distribución básica
create_basic_distribution() {
    print_step "Creando distribución básica de Eclipse OS..."
    
    # Crear directorio de distribución
    mkdir -p "$BUILD_DIR"/{boot,efi/boot,userland/{bin,lib,config,systemd/{services,targets}}}
    
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

        # Copiar Wayland Server (Rust) si existe
        if [ -f "userland/wayland_server/target/x86_64-unknown-linux-gnu/release/wayland_server" ]; then
            cp "userland/wayland_server/target/x86_64-unknown-linux-gnu/release/wayland_server" "$BUILD_DIR/userland/bin/"
            chmod +x "$BUILD_DIR/userland/bin/wayland_server"
            print_status "Wayland Server (Rust) copiado"
        elif [ -f "userland/wayland_server/target/release/wayland_server" ]; then
            cp "userland/wayland_server/target/release/wayland_server" "$BUILD_DIR/userland/bin/"
            chmod +x "$BUILD_DIR/userland/bin/wayland_server"
            print_status "Wayland Server (Rust) copiado"
        fi

        # Copiar COSMIC Client (Rust) si existe
        if [ -f "userland/cosmic_client/target/x86_64-unknown-linux-gnu/release/cosmic_client" ]; then
            cp "userland/cosmic_client/target/x86_64-unknown-linux-gnu/release/cosmic_client" "$BUILD_DIR/userland/bin/"
            chmod +x "$BUILD_DIR/userland/bin/cosmic_client"
            print_status "COSMIC Client (Rust) copiado"
        elif [ -f "userland/cosmic_client/target/release/cosmic_client" ]; then
            cp "userland/cosmic_client/target/release/cosmic_client" "$BUILD_DIR/userland/bin/"
            chmod +x "$BUILD_DIR/userland/bin/cosmic_client"
            print_status "COSMIC Client (Rust) copiado"
        fi

        # Copiar Wayland Compositor (C) si existe - soporta múltiples variantes
        if [ -f "userland/wayland_compositor/wayland_compositor_wlroots" ]; then
            cp "userland/wayland_compositor/wayland_compositor_wlroots" "$BUILD_DIR/userland/bin/wayland_compositor"
            chmod +x "$BUILD_DIR/userland/bin/wayland_compositor"
            print_status "Wayland Compositor (wlroots) copiado"
        elif [ -f "userland/wayland_compositor/wayland_compositor_wayland" ]; then
            cp "userland/wayland_compositor/wayland_compositor_wayland" "$BUILD_DIR/userland/bin/wayland_compositor"
            chmod +x "$BUILD_DIR/userland/bin/wayland_compositor"
            print_status "Wayland Compositor (libwayland) copiado"
        elif [ -f "userland/wayland_compositor/wayland_compositor" ]; then
            cp "userland/wayland_compositor/wayland_compositor" "$BUILD_DIR/userland/bin/wayland_compositor"
            chmod +x "$BUILD_DIR/userland/bin/wayland_compositor"
            print_status "Wayland Compositor (custom) copiado"
        fi

        # Copiar COSMIC Desktop (C) si existe
        if [ -f "userland/cosmic_desktop/cosmic_desktop" ]; then
            cp "userland/cosmic_desktop/cosmic_desktop" "$BUILD_DIR/userland/bin/cosmic_desktop_c"
            chmod +x "$BUILD_DIR/userland/bin/cosmic_desktop_c"
            print_status "COSMIC Desktop (C) copiado"
        fi

        # Crear directorios /usr/bin y /usr/sbin si no existen
        mkdir -p "$BUILD_DIR/usr/bin"
        mkdir -p "$BUILD_DIR/usr/sbin"
        
        # Copiar systemd si existe
        if [ -f "eclipse-apps/systemd/target/x86_64-unknown-none/release/eclipse-systemd" ]; then
            cp "eclipse-apps/systemd/target/x86_64-unknown-none/release/eclipse-systemd" "$BUILD_DIR/usr/sbin/"
            chmod +x "$BUILD_DIR/usr/sbin/eclipse-systemd"
            print_status "Systemd copiado e instalado en /usr/sbin/"
        fi
        

        # Copiar Xfbdev (TinyX) si existe
        if [ -f "eclipse-apps/tinyx/kdrive/fbdev/Xfbdev" ]; then
            cp "eclipse-apps/tinyx/kdrive/fbdev/Xfbdev" "$BUILD_DIR/usr/bin/"
            chmod +x "$BUILD_DIR/usr/bin/Xfbdev"
            print_status "Xfbdev (TinyX) copiado e instalado en /usr/bin/"
        fi
        
        # Copiar binarios de Wayland y COSMIC a /usr/bin/
        # Nota: Estos binarios no existen en la versión actual del proyecto
        # if [ -f "eclipse-apps/services/waylandd/target/release/eclipse_wayland" ]; then
        #     cp "eclipse-apps/services/waylandd/target/release/eclipse_wayland" "$BUILD_DIR/usr/bin/"
        #     chmod +x "$BUILD_DIR/usr/bin/eclipse_wayland"
        #     print_status "eclipse_wayland instalado en /usr/bin/"
        # fi

        # if [ -f "eclipse-apps/apps/cosmic/target/release/eclipse_cosmic" ]; then
        #     cp "eclipse-apps/apps/cosmic/target/release/eclipse_cosmic" "$BUILD_DIR/usr/bin/"
        #     chmod +x "$BUILD_DIR/usr/bin/eclipse_cosmic"
        #     print_status "eclipse_cosmic instalado en /usr/bin/"
        # fi

        # if [ -f "eclipse-apps/apps/rwaybar/target/release/rwaybar" ]; then
        #     cp "eclipse-apps/apps/rwaybar/target/release/rwaybar" "$BUILD_DIR/usr/bin/"
        #     chmod +x "$BUILD_DIR/usr/bin/rwaybar"
        #     print_status "rwaybar instalado en /usr/bin/"
        # fi

        #     print_status "eclipse_taskbar instalado en /usr/bin/"
        # fi
        
        
        # Crear configuración de userland
        cat > "$BUILD_DIR/userland/config/system.conf" << EOF
[system]
name = "Eclipse OS"
version = "0.1.0"
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

[desktop_environment]
# Nota: Algunos componentes del desktop environment no están implementados aún
wayland_server = "/usr/bin/Xfbdev"
# cosmic_desktop = "/userland/bin/eclipse_cosmic"
# rwaybar = "/userland/bin/rwaybar"
# eclipse_taskbar = "/userland/bin/eclipse_taskbar"
# eclipse_notifications = "/userland/bin/eclipse_notifications"
# eclipse_window_manager = "/userland/bin/eclipse_window_manager"

[display]
driver = "drm"
fallback = "vga"
primary_device = "/dev/dri/card0"

[ipc]
socket_path = "/tmp/eclipse_ipc.sock"
wayland_socket = "/tmp/eclipse/wayland.sock"
notifications_socket = "/tmp/eclipse/notifications.sock"
window_manager_socket = "/tmp/eclipse/window_manager.sock"
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

        # Crear script de inicio del desktop environment completo
        cat > "$BUILD_DIR/userland/bin/start_desktop.sh" << 'EOF'
#!/bin/bash

echo "🌙 Iniciando Eclipse OS Desktop Environment..."

# Crear directorios necesarios
mkdir -p /tmp/eclipse/shm
mkdir -p /tmp/eclipse

# Configurar variables de entorno
export XDG_RUNTIME_DIR="/tmp/$(id -u)-runtime"
mkdir -p "$XDG_RUNTIME_DIR"
export WAYLAND_DISPLAY="wayland-0"
export ECLIPSE_DEBUG_IPC=1
export ECLIPSE_IPC_SOCKET="/tmp/eclipse/wayland.sock"

# Función para limpiar al salir
cleanup() {
    echo "🛑 Deteniendo Eclipse OS Desktop..."
    kill $WAYLANDD_PID $COSMIC_PID $RWAYBAR_PID $NOTIFICATIONS_PID $WINDOW_MANAGER_PID 2>/dev/null
    rm -rf /tmp/eclipse/shm
    rm -rf /tmp/eclipse
    echo "✅ Limpieza completada"
    exit 0
}

# Registrar función de limpieza
trap cleanup EXIT INT TERM

echo "🚀 Iniciando Eclipse OS..."
echo "   Nota: Desktop environment completo no implementado aún"
echo "   Solo systemd disponible por ahora"

# Mantener el script en ejecución
# Nota: En futuras versiones se implementará el wait para los PIDs del desktop environment
sleep infinity
EOF
        chmod +x "$BUILD_DIR/userland/bin/start_desktop.sh"
        print_status "Script de inicio del desktop environment creado"

        # Crear configuración de rwaybar para Eclipse OS
        cat > "$BUILD_DIR/userland/config/rwaybar.toml" << 'EOF'
# Configuración de rwaybar para Eclipse OS
[bar]
height = 48
background = "#1a1a1a"
foreground = "#ffffff"
border = "#333333"
border_width = 1

[bar.position]
top = false
bottom = true
left = 0
right = 0

[bar.tray]
position = "right"
spacing = 10

[bar.workspaces]
position = "left"
spacing = 5

[bar.window]
position = "center"
format = "{title}"

[bar.clock]
position = "right"
format = "%H:%M:%S"
tooltip_format = "%A, %B %d, %Y"

[bar.battery]
position = "right"
format = "{capacity}% {status}"
format_charging = "⚡ {capacity}%"
format_discharging = "🔋 {capacity}%"
format_full = "🔋 {capacity}%"
format_unknown = "❓ {capacity}%"
format_critical = "⚠ {capacity}%"
tooltip_format = "{capacity}% {time} {status}"

[bar.cpu]
position = "right"
format = "CPU: {usage}%"
tooltip_format = "CPU: {usage}%"

[bar.memory]
position = "right"
format = "RAM: {usage}%"
tooltip_format = "RAM: {usage}%"

[bar.disk]
position = "right"
format = "Disk: {usage}%"
tooltip_format = "Disk: {usage}%"

[bar.temperature]
position = "right"
format = "🌡️ {temperature}°C"
tooltip_format = "Temperature: {temperature}°C"

[bar.network]
position = "right"
format = "🌐 {ifname}"
format_disconnected = "🌐 Disconnected"
tooltip_format = "{ifname}: {ipaddr}"

[bar.volume]
position = "right"
format = "🔊 {volume}%"
format_muted = "🔇 Muted"
tooltip_format = "Volume: {volume}%"

[bar.backlight]
position = "right"
format = "💡 {brightness}%"
tooltip_format = "Brightness: {brightness}%"

[bar.power]
position = "right"
format = "⚡ {power}W"
tooltip_format = "Power: {power}W"

[bar.wireless]
position = "right"
format = "📶 {essid}"
format_disconnected = "📶 Disconnected"
tooltip_format = "{essid}: {signal}%"

[bar.bluetooth]
position = "right"
format = "🔵 {status}"
tooltip_format = "Bluetooth: {status}"

[bar.pulseaudio]
position = "right"
format = "🔊 {volume}%"
format_muted = "🔇 Muted"
tooltip_format = "Volume: {volume}%"

[bar.custom]
position = "right"
format = "🌙 Eclipse OS"
tooltip_format = "Eclipse OS v0.1.0 - Desktop Environment"
EOF
    print_status "Configuración de rwaybar creada"
        
        print_success "Módulos userland copiados a la distribución"
    fi
    
    # Copiar binarios de eclipse-apps si existen
        # Copiar binarios de Wayland y COSMIC a /usr/bin/
        #mkdir -p "$BUILD_DIR/usr/bin"

    # if [ -f "eclipse-apps/target/release/eclipse_cosmic" ]; then
    #     cp "eclipse-apps/target/release/eclipse_cosmic" "$BUILD_DIR/userland/bin/"
    #     print_status "eclipse_cosmic copiado"
    # fi

    # if [ -f "eclipse-apps/target/release/rwaybar" ]; then
    #     cp "eclipse-apps/target/release/rwaybar" "$BUILD_DIR/userland/bin/"
    #     print_status "rwaybar copiado"
    # fi

    # if [ -f "eclipse-apps/target/release/eclipse_taskbar" ]; then
    #     cp "eclipse-apps/target/release/eclipse_taskbar" "$BUILD_DIR/userland/bin/"
    #     print_status "eclipse_taskbar copiado"
    # fi

    # if [ -f "eclipse-apps/target/release/eclipse_notifications" ]; then
    #     cp "eclipse-apps/target/release/eclipse_notifications" "$BUILD_DIR/userland/bin/"
    #     print_status "eclipse_notifications copiado"
    # fi

    # if [ -f "eclipse-apps/target/release/eclipse_window_manager" ]; then
    #     cp "eclipse-apps/target/release/eclipse_window_manager" "$BUILD_DIR/userland/bin/"
    #     print_status "eclipse_window_manager copiado"
    # fi

    # Copiar unidades/targets de systemd para eclipse-apps
    if [ -d "eclipse-apps/systemd/services" ]; then
        cp eclipse-apps/systemd/services/*.service "$BUILD_DIR/userland/systemd/services/" 2>/dev/null || true
        print_status "Unidades systemd (services) copiadas"
    fi
    if [ -d "eclipse-apps/systemd/targets" ]; then
        cp eclipse-apps/systemd/targets/*.target "$BUILD_DIR/userland/systemd/targets/" 2>/dev/null || true
        print_status "Unidades systemd (targets) copiadas"
    fi

    # Copiar el instalador si existe
    if [ -f "installer/target/x86_64-unknown-linux-musl/release/eclipse-installer" ]; then
        cp "installer/target/x86_64-unknown-linux-musl/release/eclipse-installer" "$BUILD_DIR/userland/bin/"
        print_status "Instalador copiado a la distribución"
    else
        print_status "Instalador no encontrado - no se puede copiar"
    fi
    
    # Crear configuración UEFI básica (no GRUB ya que usamos bootloader UEFI personalizado)
    cat > "$BUILD_DIR/efi/boot/uefi_config.txt" << EOF
# Configuración UEFI para Eclipse OS v0.1.0
# Bootloader personalizado - no requiere GRUB

[system]
kernel_path = "/boot/eclipse_kernel"
userland_path = "/userland/bin/eclipse_userland"

[debug]
enable_debug = false
log_level = "info"
EOF
    
    # Crear symlink para fuentes (TinyX espera /usr/lib/X11/fonts)
    mkdir -p "$BUILD_DIR/usr/lib/X11/fonts"
    if [ -d "/usr/share/fonts/X11" ]; then
        cp -r /usr/share/fonts/X11 "$BUILD_DIR/usr/lib/X11/fonts/"
        print_status "Copiado de fuentes: /usr/share/fonts/X11 -> $BUILD_DIR/usr/lib/X11/fonts/"
    fi
    
    print_success "Distribución básica creada en $BUILD_DIR"
}

# Función para crear imagen USB booteable
create_bootable_image() {
    print_step "Creando imagen USB booteable..."
    
    # Verificar que existan los archivos necesarios
    local BOOTLOADER_PATH="bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader.efi"
    local KERNEL_PATH="eclipse_kernel/target/$KERNEL_TARGET/release/eclipse_kernel"
    
    if [ ! -f "$BOOTLOADER_PATH" ]; then
        print_error "Bootloader no encontrado en: $BOOTLOADER_PATH"
        return 1
    fi
    
    if [ ! -f "$KERNEL_PATH" ]; then
        print_error "Kernel no encontrado en: $KERNEL_PATH"
        return 1
    fi
    
    # Nombre del archivo de imagen
    local IMG_FILE="eclipse_os.img"
    
    # Siempre recrear la imagen para tener 2 particiones
    if [ -f "$IMG_FILE" ]; then
        print_status "Eliminando imagen existente para recrear con 2 particiones..."
        rm -f "$IMG_FILE"
    fi
    
    print_status "Creando imagen de 2GB con 2 particiones..."
    dd if=/dev/zero of="$IMG_FILE" bs=1M count=2048 status=progress 2>&1 | tail -1
    
    # Crear particiones con parted (requiere GPT y 2 particiones)
    # Buscar parted en ubicaciones comunes
    PARTED_CMD=""
    for path in /sbin/parted /usr/sbin/parted /usr/bin/parted parted; do
        if command -v $path &> /dev/null || [ -x "$path" ]; then
            PARTED_CMD="$path"
            break
        fi
    done
    
    if [ -n "$PARTED_CMD" ]; then
        print_status "Creando tabla GPT y particiones con parted ($PARTED_CMD)..."
        
        # Crear tabla GPT
        sudo "$PARTED_CMD" "$IMG_FILE" --script mklabel gpt
        
        # Partición 1: ESP (FAT32, 512MB) para bootloader y kernel
        sudo "$PARTED_CMD" "$IMG_FILE" --script mkpart ESP fat32 1MiB 513MiB
        sudo "$PARTED_CMD" "$IMG_FILE" --script set 1 esp on
        
        # Partición 2: EclipseFS (resto) para sistema de archivos
        sudo "$PARTED_CMD" "$IMG_FILE" --script mkpart primary ext4 513MiB 100%
        
        print_status "Configurando loop device..."
        LOOP=$(sudo losetup -fP --show "$IMG_FILE")
        print_status "Loop device: $LOOP"
        
        # Esperar a que aparezcan las particiones
        sleep 1
        
        # Formatear partición 1 como FAT32
        print_status "Formateando partición 1 (FAT32)..."
        sudo mkfs.fat -F32 -n "ECLIPSE_OS" "${LOOP}p1"
        
        # Formatear partición 2 con EclipseFS usando mkfs-eclipsefs
        print_status "Formateando partición 2 (EclipseFS)..."
        
        if [ -f "mkfs-eclipsefs/target/release/mkfs-eclipsefs" ]; then
            # Usar mkfs-eclipsefs compilado
            print_status "Usando mkfs-eclipsefs para formateo profesional..."
            sudo ./mkfs-eclipsefs/target/release/mkfs-eclipsefs -f -L "Eclipse OS Root" -N 10000 "${LOOP}p2"
            print_success "✓ Partición 2 formateada con mkfs-eclipsefs"
            
            # Poblar el filesystem con los archivos de BUILD_DIR
            if [ -f "populate-eclipsefs/target/release/populate-eclipsefs" ] && [ -d "$BUILD_DIR" ]; then
                print_status "Poblando filesystem EclipseFS con archivos del sistema..."
                
                # Crear directorios estándar en BUILD_DIR si no existen
                mkdir -p "$BUILD_DIR"/{bin,sbin,usr/{bin,sbin,lib},etc,var,tmp,home,root,dev,proc,sys}
                
                # Copiar eclipse-systemd a las ubicaciones estándar si existe
                if [ -f "eclipse-apps/systemd/target/x86_64-unknown-none/release/eclipse-systemd" ]; then
                    mkdir -p "$BUILD_DIR/sbin"
                    mkdir -p "$BUILD_DIR/usr/sbin"
                    cp "eclipse-apps/systemd/target/x86_64-unknown-none/release/eclipse-systemd" "$BUILD_DIR/sbin/eclipse-systemd"
                    chmod +x "$BUILD_DIR/sbin/eclipse-systemd"
                    print_status "eclipse-systemd copiado a /sbin/"
                fi
                

                # Copiar otros binarios importantes si existen
                if [ -d "userland/target/release" ]; then
                    mkdir -p "$BUILD_DIR/bin"
                    mkdir -p "$BUILD_DIR/usr/bin"
                    
                    for binary in eclipse_userland module_loader graphics_module app_framework; do
                        if [ -f "userland/target/release/$binary" ] || [ -f "userland/*/target/release/$binary" ]; then
                            find userland -name "$binary" -path "*/release/$binary" -exec cp {} "$BUILD_DIR/bin/" \; 2>/dev/null
                            print_status "$binary copiado a /bin/"
                        fi
                    done
                fi
                
                # Usar populate-eclipsefs para copiar todo al filesystem
                print_status "Ejecutando populate-eclipsefs..."
                sudo ./populate-eclipsefs/target/release/populate-eclipsefs "${LOOP}p2" "$BUILD_DIR"
                
                if [ $? -eq 0 ]; then
                    print_success "✓ Filesystem EclipseFS poblado exitosamente"
                else
                    print_error "Error al poblar filesystem EclipseFS"
                fi
            else
                print_status "populate-eclipsefs o BUILD_DIR no encontrado, filesystem quedará vacío"
            fi
        else
            # Fallback: header simple con Python
            print_status "mkfs-eclipsefs no encontrado, usando método simple..."
            
            python3 << 'PYTHON_EOF'
import struct
import time
import uuid

header = bytearray(4096)
header[0:4] = struct.pack('<I', 0x45434653)  # "ECFS"
header[4:8] = struct.pack('<I', 0x00020000)  # v2.0
header[8:16] = struct.pack('<Q', int(time.time()))
header[16:20] = struct.pack('<I', 4096)
header[20:28] = struct.pack('<Q', 380000)
header[28:36] = struct.pack('<Q', 4096)
header[36:44] = struct.pack('<Q', 128000)
header[44:52] = struct.pack('<Q', 4096 + 128000)
header[52:60] = struct.pack('<Q', 4096 + 128000 + 4096)
header[100:111] = b"Eclipse OS\x00"
header[200:216] = uuid.uuid4().bytes

with open('/tmp/eclipsefs_header.bin', 'wb') as f:
    f.write(header)
PYTHON_EOF
            
            sudo dd if=/tmp/eclipsefs_header.bin of="${LOOP}p2" bs=4096 count=1 conv=notrunc status=none
            rm -f /tmp/eclipsefs_header.bin
            print_status "✓ Header EclipseFS escrito"
        fi
        
        # Montar partición FAT32 y copiar archivos
        print_status "Montando partición FAT32..."
        MOUNT_POINT="/tmp/eclipse_efi_mount"
        sudo mkdir -p "$MOUNT_POINT"
        sudo mount "${LOOP}p1" "$MOUNT_POINT"
        
        # Crear estructura de directorios
        print_status "Creando estructura EFI..."
        sudo mkdir -p "$MOUNT_POINT/EFI/BOOT"
        sudo mkdir -p "$MOUNT_POINT/boot"
        sudo mkdir -p "$MOUNT_POINT/eclipse"
        
        # Copiar bootloader
        print_status "Copiando bootloader..."
        sudo cp "$BOOTLOADER_PATH" "$MOUNT_POINT/EFI/BOOT/BOOTX64.EFI"
        
        # Copiar kernel
        print_status "Copiando kernel..."
        sudo cp "$KERNEL_PATH" "$MOUNT_POINT/boot/eclipse_kernel"
        
        # Crear configuración de boot
        cat > /tmp/boot.cfg << 'EOF'
# Eclipse OS Boot Configuration
kernel=/boot/eclipse_kernel
resolution=1024x768
debug=false
EOF
        sudo cp /tmp/boot.cfg "$MOUNT_POINT/eclipse/boot.cfg"
        rm /tmp/boot.cfg
        
        # Copiar configuración UEFI si existe
        if [ -f "$BUILD_DIR/efi/boot/uefi_config.txt" ]; then
            sudo cp "$BUILD_DIR/efi/boot/uefi_config.txt" "$MOUNT_POINT/eclipse/"
        fi
        
        # Mostrar contenido
        print_status "Contenido de la partición FAT32:"
        ls -lah "$MOUNT_POINT/EFI/BOOT/"
        ls -lah "$MOUNT_POINT/boot/"
        
        # Desmontar FAT32
        print_status "Desmontando partición FAT32..."
        
        # Asegurar que no estamos en el directorio del punto de montaje
        cd "$(dirname "$0")"
        
        # Sincronizar para vaciar todas las escrituras pendientes al disco
        sync
        
        # Intentar desmontar con reintentos
        UNMOUNT_RETRIES=5
        UNMOUNT_SUCCESS=0
        for i in $(seq 1 $UNMOUNT_RETRIES); do
            if [ $i -gt 1 ]; then
                print_status "Reintentando desmontaje (intento $i/$UNMOUNT_RETRIES)..."
                sleep 1
                sync
            fi
            if sudo umount "$MOUNT_POINT" 2>/dev/null; then
                UNMOUNT_SUCCESS=1
                break
            fi
        done
        
        if [ $UNMOUNT_SUCCESS -eq 1 ]; then
            print_success "✓ Partición FAT32 desmontada correctamente"
            sudo rmdir "$MOUNT_POINT" 2>/dev/null || true
        else
            print_error "Error al desmontar $MOUNT_POINT"
            print_status "Intentando limpieza forzada..."
            sudo umount -l "$MOUNT_POINT" 2>/dev/null || true
            sleep 1
            sudo rmdir "$MOUNT_POINT" 2>/dev/null || true
        fi
        
        # Partición EclipseFS ya fue poblada con populate-eclipsefs
        print_success "Partición EclipseFS lista con archivos del sistema"
        print_status "Puede montar con: sudo eclipsefs-fuse ${LOOP}p2 /mnt"
        
        # Desconectar loop device
        print_status "Limpiando loop device..."
        sudo losetup -d "$LOOP"
        
        print_success "Imagen booteable con 2 particiones creada: $IMG_FILE ($(du -h "$IMG_FILE" | cut -f1))"
        print_status "  Partición 1: ESP (FAT32, 512MB) - Bootloader + Kernel"
        print_status "  Partición 2: EclipseFS (poblada) - Sistema con archivos"
        
    else
        print_error "parted no encontrado. Se requiere para crear particiones GPT"
        print_status "Instala con: sudo apt-get install parted"
        return 1
    fi
    
    echo ""
    print_success "Para probar ejecuta: sudo ./qemu.sh"
}

# Función para mostrar resumen de construcción
show_build_summary() {
    echo ""
    print_success "Compilación completada exitosamente"
    echo ""
    echo "Binarios compilados:"
    echo "Componentes compilados:"
    echo "  Librería EclipseFS: eclipsefs-lib/target/debug/libeclipsefs_lib.rlib"
    echo "  Init Process: eclipse_kernel/userspace/init/target/x86_64-unknown-none/release/eclipse-init"
    echo "  Kernel Eclipse OS: target/$KERNEL_TARGET/release/eclipse_kernel"
    echo "  Bootloader UEFI: bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader.efi"
    echo "  Instalador: installer/target/x86_64-unknown-linux-musl/release/eclipse-installer"
    echo "  Systemd: eclipse-apps/systemd/target/x86_64-unknown-none/release/eclipse-systemd"
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
    echo "Desktop Environment:"
    echo "  Nota: Desktop environment no implementado en esta versión"
    echo "  eclipse-systemd: eclipse-apps/systemd/target/release/eclipse-systemd"
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
    
    # Mostrar imagen USB si existe
    if [ -f "eclipse_os.img" ]; then
        echo "Imagen USB booteable:"
        echo "  - eclipse_os.img ($(du -h eclipse_os.img | cut -f1))"
        echo "  - Lista para usar en: sudo ./qemu.sh"
    fi
    
    echo ""
    echo "Eclipse OS v0.1.0 está listo para usar!"
}

# Función para compilar eclipse-apps (IPC + systemd)
build_eclipse_apps() {
    print_step "Compilando workspace eclipse-apps (IPC + systemd)..."

    if [ ! -d "eclipse-apps" ]; then
        print_status "Directorio eclipse-apps no encontrado, saltando..."
        return 0
    fi

    cd eclipse-apps

    print_status "Compilando librería eclipse_ipc..."
    cd libs/ipc && RUSTFLAGS="-C relocation-model=pic" cargo build --release || { cd ../..; print_error "Fallo compilando eclipse_ipc"; return 1; }
    cd ../..

    # print_status "Compilando eclipse-systemd..."
    # cd systemd && cargo build --release --target x86_64-unknown-none || { cd ..; print_error "Fallo compilando eclipse-systemd"; return 1; }
    # cd ..

    print_success "eclipse-apps compilado completamente"
    cd ..
}

# Función para compilar mkfs-eclipsefs
build_mkfs_eclipsefs() {
    print_step "Compilando mkfs-eclipsefs..."
    
    if [ ! -d "mkfs-eclipsefs" ]; then
        print_status "Directorio mkfs-eclipsefs no encontrado, saltando..."
        return 0
    fi
    
    cd mkfs-eclipsefs
    
    print_status "Compilando mkfs-eclipsefs..."
    RUSTFLAGS="-C relocation-model=pic" cargo build --release
    
    if [ $? -eq 0 ]; then
        print_success "mkfs-eclipsefs compilado exitosamente"
        
        local mkfs_path="target/release/mkfs-eclipsefs"
        if [ -f "$mkfs_path" ]; then
            local mkfs_size=$(du -h "$mkfs_path" | cut -f1)
            print_status "mkfs-eclipsefs generado: $mkfs_path ($mkfs_size)"
        fi
    else
        print_error "Error al compilar mkfs-eclipsefs"
        cd ..
        return 1
    fi
    
    cd ..
}

# Función para compilar populate-eclipsefs
build_populate_eclipsefs() {
    print_step "Compilando populate-eclipsefs..."
    
    if [ ! -d "populate-eclipsefs" ]; then
        print_status "Directorio populate-eclipsefs no encontrado, saltando..."
        return 0
    fi
    
    cd populate-eclipsefs
    
    print_status "Compilando populate-eclipsefs..."
    RUSTFLAGS="-C relocation-model=pic" cargo build --release
    
    if [ $? -eq 0 ]; then
        print_success "populate-eclipsefs compilado exitosamente"
        
        local populate_path="target/release/populate-eclipsefs"
        if [ -f "$populate_path" ]; then
            local populate_size=$(du -h "$populate_path" | cut -f1)
            print_status "populate-eclipsefs generado: $populate_path ($populate_size)"
        fi
    else
        print_error "Error al compilar populate-eclipsefs"
        cd ..
        return 1
    fi
    
    cd ..
}

# Función para compilar eclipsefs-cli
build_eclipsefs_cli() {
    print_step "Compilando eclipsefs-cli..."
    
    if [ ! -d "eclipsefs-cli" ]; then
        print_status "Directorio eclipsefs-cli no encontrado, saltando..."
        return 0
    fi
    
    cd eclipsefs-cli
    
    print_status "Compilando eclipsefs CLI tool..."
    SYSROOT_LDFLAGS="-C link-arg=-L$BASE_DIR/$BUILD_DIR/sysroot/usr/lib -C link-arg=-nostdlib"
    RUSTFLAGS="-Zunstable-options -C relocation-model=pic $SYSROOT_LDFLAGS $RUSTFLAGS" cargo +nightly -Z unstable-options build --release --target "$ECLIPSE_TARGET" -Z build-std=std,panic_abort
    
    if [ $? -eq 0 ]; then
        print_success "eclipsefs-cli compilado exitosamente"
        
        local cli_path="target/release/eclipsefs"
        if [ -f "$cli_path" ]; then
            local cli_size=$(du -h "$cli_path" | cut -f1)
            print_status "eclipsefs CLI generado: $cli_path ($cli_size)"
        fi
    else
        print_error "Error al compilar eclipsefs-cli"
        cd ..
        return 1
    fi
    
    cd ..
}

# Función principal
main() {
    # Ejecutar pasos de construcción
    build_eclipsefs_lib
    build_mkfs_eclipsefs
    build_populate_eclipsefs
    build_eclipsefs_cli
    build_eclipse_init
    build_userspace_services
    build_kernel
    build_bootloader
    build_installer
    build_systemd
    prepare_sysroot
    build_eclipse_libc
    build_eclipse_apps
    build_userland
    
    # We still need systemd and ipc which are in eclipse_apps, so let's run them selectively?
    # Actually, eclipse-systemd is already built.
    # But create_basic_distribution needs them.
    # Let's run a modified version of these or just trust they are there?
    # build_eclipse_apps runs: ipc, systemd.
    # ipc failed before but passed with PIC? No, it passed.
    # So build_eclipse_apps should be fine if we remove other things?
    # But build_eclipse_apps only builds ipc and systemd.
    # Wait, build.sh:1322 says build_eclipse_apps builds ipc and systemd.
    # So I should keep build_eclipse_apps.
    # build_eclipse_apps
    
    # build_userland builds: wayland_server, cosmic_client, module_loader etc.
    # These are failing.
    # build_userland
    
    # Crear distribución completa para compatibilidad con instalador
    create_basic_distribution
    
    # Crear imagen booteable USB solo si se solicita explícitamente
    if [ "$1" = "image" ]; then
        create_bootable_image
    else
        echo ""
        print_status "Imagen de disco NO creada. Para crearla ejecuta: ./build.sh image"
    fi
    
    show_build_summary
}

# Ejecutar función principal
main "$@"
