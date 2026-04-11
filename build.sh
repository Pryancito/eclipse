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

print_warning() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

# Copia al rootfs staging las .so que `ldd` resuelve para un binario (labwc, etc.).
# Sin esto, en Eclipse solo están musl/libc y faltan libinput, EGL, cairo, pango…
eclipse_stage_ldd_libs() {
    local dest="$1"
    local bin="$2"
    [ -z "${ECLIPSE_SKIP_LDD_STAGING:-}" ] || return 0
    [ -f "$bin" ] || return 0
    if ! command -v ldd >/dev/null 2>&1; then
        print_warning "ldd no está en PATH; omito copia de dependencias de $bin. Instala libc-bin o define ECLIPSE_LABWC_LIB_PREFIX."
        return 0
    fi
    mkdir -p "$dest/lib"
    local line p n
    n=0
    while IFS= read -r line; do
        case "$line" in
            *"=> /"*)
                p="${line#*=> }"
                p="${p%% (*}"
                case "$p" in
                    */ld-linux*.so*)
                        continue
                        ;;
                esac
                [ -f "$p" ] || [ -L "$p" ] || continue
                cp -a "$p" "$dest/lib/"
                n=$((n + 1))
                ;;
        esac
    done < <(ldd "$bin" 2>/dev/null || true)
    if [ "$n" -gt 0 ]; then
        print_status "Staged $n bibliotecas dinámicas (ldd) desde $(basename "$bin") -> lib/"
    fi
}

# Si ldd falla (p. ej. musl intentó cargar /lib/.../libc.so script de glibc), copia DT_NEEDED resolviendo rutas.
eclipse_stage_readelf_needed_libs() {
    local dest="$1"
    local bin="$2"
    [ -z "${ECLIPSE_SKIP_LDD_STAGING:-}" ] || return 0
    [ -f "$bin" ] || return 0
    command -v readelf >/dev/null 2>&1 || return 0
    mkdir -p "$dest/lib"
    local soname n cand pf d
    n=0
    while IFS= read -r soname; do
        [ -z "$soname" ] && continue
        case "$soname" in
            ld-linux*.so*) continue ;;
        esac
        cand=""
        if command -v musl-gcc >/dev/null 2>&1; then
            pf="$(musl-gcc -print-file-name="$soname" 2>/dev/null || true)"
            case "$pf" in
                /*)
                    if [ -f "$pf" ] && readelf -h "$pf" >/dev/null 2>&1; then
                        cand="$pf"
                    fi
                    ;;
            esac
        fi
        if [ -z "$cand" ]; then
            for d in /usr/lib/x86_64-linux-musl /usr/lib/x86_64-linux-gnu /lib/x86_64-linux-gnu; do
                [ -e "$d/$soname" ] || continue
                readelf -h "$d/$soname" >/dev/null 2>&1 || continue
                cand="$d/$soname"
                break
            done
        fi
        [ -z "$cand" ] && continue
        cp -a "$cand" "$dest/lib/"
        n=$((n + 1))
    done < <(readelf -d "$bin" 2>/dev/null | sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p')
    if [ "$n" -gt 0 ]; then
        print_status "Staged $n bibliotecas (readelf NEEDED, sin ldd) desde $(basename "$bin") -> lib/"
    fi
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
echo "║ ECLIPSE OS - SCRIPT DE CONSTRUCCIÓN COMPLETO v0.2.0 ║"
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
    RUSTFLAGS="-Zunstable-options $RUSTFLAGS" cargo +nightly -Z unstable-options -Z json-target-spec build --release --target "$ECLIPSE_TARGET" -Z build-std=core,alloc
    
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

# Función para compilar eclipse-relibc (C library in Rust for Eclipse OS)
build_eclipse_libc() {
    print_step "Compilando eclipse-relibc..."
    
    if [ ! -d "eclipse-relibc" ]; then
        print_status "Directorio eclipse-relibc no encontrado, saltando..."
        return 0
    fi
    
    cd eclipse-relibc
    
    print_status "Compilando eclipse-relibc..."
    RUSTFLAGS="-Zunstable-options --cfg eclipse_target $RUSTFLAGS" cargo +nightly -Z unstable-options -Z json-target-spec build --release --target "$ECLIPSE_TARGET" -Z build-std=core,alloc
    
    if [ $? -eq 0 ]; then
        print_success "eclipse-relibc compilado exitosamente"
        
        # Instalar en sysroot como libc.a
        local SYSROOT_LIB="$BASE_DIR/$BUILD_DIR/sysroot/usr/lib"
        mkdir -p "$SYSROOT_LIB"
        cp "target/x86_64-unknown-eclipse/release/liblibc.rlib" "$SYSROOT_LIB/libc.a"
        print_status "Instalado en sysroot: $SYSROOT_LIB/libc.a"

        # Debilitar todos los símbolos globales en libc.a para evitar
        # conflictos de símbolos duplicados cuando se enlaza junto con
        # libstd (que también define __rust_alloc, etc.)
        if command -v objcopy &>/dev/null; then
            objcopy --weaken "$SYSROOT_LIB/libc.a"
            print_status "Símbolos de libc.a debilitados con objcopy --weaken"
        elif command -v llvm-objcopy &>/dev/null; then
            llvm-objcopy --weaken-all "$SYSROOT_LIB/libc.a"
            print_status "Símbolos de libc.a debilitados con llvm-objcopy"
        else
            print_error "objcopy no encontrado; el enlace de eclipsefs-cli puede fallar por símbolos duplicados"
        fi

        # Crear stub vacío de libgcc_s para satisfacer -lgcc_s sin duplicar símbolos
        rm -f "$SYSROOT_LIB/libgcc_s.a"
        ar crs "$SYSROOT_LIB/libgcc_s.a"
        print_status "Stub vacío libgcc_s.a creado en sysroot"
    else
        print_error "Error al compilar eclipse-relibc"
        cd ..
        return 1
    fi
    
    cd ..
}

build_sidewind_project() {
    print_step "Compilando proyecto Sidewind (Workspace)..."
    
    if [ ! -d "$BASE_DIR/eclipse-apps" ]; then
        print_status "Directorio $BASE_DIR/eclipse-apps no encontrado, saltando..."
        return 0
    fi
    
    cd "$BASE_DIR/eclipse-apps"
    
    # Compilar todo el workspace usando el target personalizado de Eclipse
    # parse_stack_sizes es herramienta host (usa stack-sizes/anyhow/byteorder con std) - excluir del build Eclipse
    # WAYLAND_CLIENT_NO_PKG_CONFIG=1 y LIBUDEV_NO_PKG_CONFIG=1 evitan que pkg-config falle al no encontrar
    # las librerías nativas de linux durante el build para el target Eclipse.
    print_status "Compilando workspace Sidewind para target Eclipse (bypassing pkg-config)..."
    
    # Construir eclipse_std primero para tener el bridge de la librería estándar
    print_status "Construyendo eclipse_std (std bridge)..."
    cargo +nightly -Z json-target-spec build -p eclipse_std --target "$ECLIPSE_TARGET" -Z build-std=core,alloc --release
    
    # La std sustituta es eclipse_std vía [patch.crates-io] en eclipse-apps/Cargo.toml.
    # NO usar RUSTFLAGS='--extern std=...libstd-....rlib': en nightly reciente provoca ICE en
    # build-std (alloc) y/o fallos al resolver dependencias (p. ej. smallvec en wayland-proto).

    set +e
    WAYLAND_CLIENT_NO_PKG_CONFIG=1 LIBUDEV_NO_PKG_CONFIG=1 PKG_CONFIG_ALLOW_CROSS=1 \
    cargo +nightly -Z json-target-spec build --workspace --target "$ECLIPSE_TARGET" -Z build-std=core,alloc --release
    _sidewind_build_status=$?
    set -e

    if [ $_sidewind_build_status -eq 0 ]; then
        print_success "Proyecto Sidewind compilado exitosamente"

        local _sw_rel="target/${ECLIPSE_TARGET_NAME}/release"
        mkdir -p "$BASE_DIR/$BUILD_DIR/sysroot/usr/bin"
        if [ -d "$BASE_DIR/$BUILD_DIR" ]; then
            mkdir -p "$BASE_DIR/$BUILD_DIR/usr/bin"
        fi

        # Binarios del workspace Eclipse (ruta = target triple del JSON, no musl)
        local BINS="lunas smithay_app"

        for bin in $BINS; do
            if [ -f "$_sw_rel/$bin" ]; then
                cp "$_sw_rel/$bin" "$BASE_DIR/$BUILD_DIR/sysroot/usr/bin/$bin"
                print_status "Instalado en sysroot: /usr/bin/$bin"
                if [ -d "$BASE_DIR/$BUILD_DIR" ]; then
                    cp "$_sw_rel/$bin" "$BASE_DIR/$BUILD_DIR/usr/bin/$bin"
                fi
            fi
        done
        local APPS="nano terminal glxgears sh"

        for bin in $APPS; do
            if [ -f "$_sw_rel/$bin" ]; then
                cp "$_sw_rel/$bin" "$BASE_DIR/$BUILD_DIR/sysroot/bin/$bin"
                print_status "Instalado en sysroot: /bin/$bin"
                if [ -d "$BASE_DIR/$BUILD_DIR" ]; then
                    cp "$_sw_rel/$bin" "$BASE_DIR/$BUILD_DIR/bin/$bin"
                fi
            fi
        done
    else
        print_error "Error al compilar el proyecto Sidewind"
        cd "$BASE_DIR"
        return 1
    fi
    
    cd "$BASE_DIR"
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
    RUSTFLAGS="-Zunstable-options --cfg eclipse_target $RUSTFLAGS" cargo +nightly -Z unstable-options -Z json-target-spec build --release --target "$ECLIPSE_TARGET" -Z build-std=core,alloc
    
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
#    local TINYX_INSTALL="$BASE_DIR/eclipse-apps/tinyx/install"
#    if [ -f "eclipse-apps/tinyx/install/lib/pkgconfig/xfont.pc" ]; then
#        export PKG_CONFIG_PATH="$TINYX_INSTALL/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
#        export TINYX_USE_LIBXFONT1=1
#    fi
    cd eclipse-apps/tinyx
    if [ ! -f "Makefile" ] || ! grep -q "enable_builtin_fonts" configure 2>/dev/null; then
        print_status "Configurando TinyX (configure-eclipse.sh)..."
        ./configure-eclipse.sh || { cd ../..; print_error "Configure TinyX falló"; return 1; }
    fi
#    SYSROOT_TINYX="${SYSROOT:-$BASE_DIR/$BUILD_DIR/sysroot}"
    print_status "Compilando Xfbdev (make, enlace estático)..."
    # CFLAGS con -fno-PIE para que el build con sysroot no use -fPIE del configure (PIE es para build nativo)
#    TINYX_SYSROOT_CFLAGS="-fno-PIE -O2 -ffunction-sections -fdata-sections -fvisibility=hidden -fno-unwind-tables -fno-asynchronous-unwind-tables -Wall"
#    TINYX_LDFLAGS_STATIC="-static -Wl,-O1 -Wl,-as-needed"
#    if make -j"$(nproc)" CC="gcc --sysroot=$SYSROOT_TINYX -fno-stack-protector -fno-PIE -O2" CFLAGS="$TINYX_SYSROOT_CFLAGS" LDFLAGS="-B$SYSROOT_TINYX/usr/lib -no-pie $TINYX_LDFLAGS_STATIC" LIBS="-lz" 2>/dev/null; then
     print_status "Compilando CRT sin TLS (crt0_start.o, crt0_no_tls.o)..."
        rm -f crt0_no_tls.o crt0_start.o
        gcc -c -O2 -fno-stack-protector -fno-PIE crt0_start.S -o crt0_start.o
        gcc -c -O2 -fno-stack-protector -fno-PIE crt0_no_tls.c -o crt0_no_tls.o
        if [ -f "crt0_no_tls.o" ]; then
            local SYSROOT_LIB="$BASE_DIR/$BUILD_DIR/sysroot/usr/lib"
            TINYX_LDFLAGS_STATIC="-nostartfiles -L$SYSROOT_LIB -Wl,--entry=_start $(pwd)/crt0_start.o $(pwd)/crt0_no_tls.o -static -no-pie -Wl,-O1 -Wl,-as-needed"
            print_status "Enlazando Xfbdev con -nostartfiles, sysroot y CRT con TLS"
        fi
        if make -j"$(nproc)" LDFLAGS="$TINYX_LDFLAGS_STATIC"; then
            print_success "TinyX (Xfbdev) compilado"
        else
            print_error "Make TinyX falló"
            return 1
        fi
#    else
#        print_status "Make con sysroot falló, intentando make nativo (estático, CRT sin TLS)..."
#        make clean 2>/dev/null || true
#        # CRT sin TLS: evita __libc_setup_tls (page fault 0x388 en Eclipse OS)
#        print_status "Compilando CRT sin TLS (crt0_start.o, crt0_no_tls.o)..."
#        rm -f crt0_no_tls.o crt0_start.o
#        gcc -c -O2 -fno-stack-protector -fno-PIE crt0_start.S -o crt0_start.o || true
#        gcc -c -O2 -fno-stack-protector -fno-PIE crt0_no_tls.c -o crt0_no_tls.o || true
#        if [ -f "crt0_start.o" ] && [ -f "crt0_no_tls.o" ]; then
#            TINYX_LDFLAGS_STATIC="-nostartfiles $(pwd)/crt0_start.o $(pwd)/crt0_no_tls.o -static -no-pie -Wl,-O1 -Wl,-as-needed"
#            print_status "Enlazando Xfbdev con -nostartfiles"
#        fi
#        if make -j"$(nproc)" LDFLAGS="$TINYX_LDFLAGS_STATIC"; then
#            print_success "TinyX (Xfbdev) compilado (nativo, estático, sin TLS)"
#        else
#            cd ../..
#            print_error "Make TinyX falló"
#            return 1
#        fi
#    fi
    cd ../..
}

# Función para compilar el proceso init (embedded)
build_eclipse_init() {
    print_step "Compilando eclipse-init..."
    
    # Asegurar que rust-src está instalado
    print_status "Verificando rust-src component..."
    rustup component add rust-src --toolchain nightly 2>/dev/null || true
    
    cd eclipse_kernel/userspace/init
    
    print_status "Compilando eclipse-init..."
    RUSTFLAGS="--cfg eclipse_target ${RUSTFLAGS:-}" cargo +nightly -Z json-target-spec build --release --target ../../../x86_64-unknown-eclipse.json -Zbuild-std=core,alloc
    
    if [ $? -eq 0 ]; then
        print_success "eclipse-init compilado exitosamente"
        
        local init_path="target/x86_64-unknown-eclipse/release/eclipse-init"
        if [ -f "$init_path" ]; then
            local init_size=$(du -h "$init_path" | cut -f1)
            print_status "Init process generado: $init_path ($init_size)"

            # Instalar en el rootfs que luego se mete en EclipseFS
            mkdir -p "../../../$BUILD_DIR/sbin"
            cp "$init_path" "../../../$BUILD_DIR/sbin/eclipse-init"
            chmod +x "../../../$BUILD_DIR/sbin/eclipse-init"
            print_status "eclipse-init instalado en /sbin/eclipse-init (rootfs staging)"
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
    
    # Lista de servicios a compilar (debe coincidir con el orden en eclipse_kernel/src/binaries.rs)
    # NOTA: Si agregas/quitas servicios, actualiza también binaries.rs y syscalls.rs (sys_get_service_binary)
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
        
        # Todos los servicios usan eclipse_std (target x86_64-unknown-eclipse, fn main)
        # gui_service: por defecto labwc; ECLIPSE_COMPOSITOR_LUNAS=1 usa lunas (ET_EXEC estático)
        local _gui_flags=""
        if [ "$service" = "gui_service" ] && [ "${ECLIPSE_COMPOSITOR_LUNAS:-}" = "1" ]; then
            _gui_flags="--features compositor-lunas"
            print_status "gui_service: compilando con compositor-lunas (file:/usr/bin/lunas)"
        fi
        RUSTFLAGS="--cfg eclipse_target ${RUSTFLAGS:-}" cargo +nightly -Z json-target-spec build --release --target ../../../x86_64-unknown-eclipse.json -Zbuild-std=core,alloc $_gui_flags
        local build_ok=$?
        local service_path="target/x86_64-unknown-eclipse/release/$service"
        
        if [ "$build_ok" -eq 0 ]; then
            if [ -f "$service_path" ]; then
                local service_size=$(du -h "$service_path" | cut -f1)
                print_status "$service generado: $service_size"

                # Instalar también en /sbin dentro del rootfs staging (como con eclipse-init)
                mkdir -p "$BASE_DIR/$BUILD_DIR/sbin"
                cp "$service_path" "$BASE_DIR/$BUILD_DIR/sbin/$service"
                chmod +x "$BASE_DIR/$BUILD_DIR/sbin/$service"
                print_status "$service instalado en /sbin/$service (rootfs staging)"
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
    print_step "Compilando kernel Eclipse OS v0.2.0..."
    
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
    cargo +nightly build --no-default-features --release --target "$UEFI_TARGET"
    
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
    
    # Instalador es herramienta host (Linux), usa std. NO usar x86_64-unknown-linux-musl.
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

# Compositor labwc (wlroots) enlazado con musl-gcc; ver eclipse-apps/labwc/scripts/build-labwc-musl.sh
# Requiere: meson, ninja, musl-gcc, cmake, pkg-config, hwdata (hwdata.pc). Opcional: SKIP_LABWC=1 ./build.sh
build_labwc_musl() {
    print_step "Compilando labwc (linux-musl, Meson)..."

    if [ -n "${SKIP_LABWC:-}" ]; then
        print_status "SKIP_LABWC definido; omitiendo labwc."
        return 0
    fi

    if [ ! -d "$BASE_DIR/eclipse-apps/labwc" ]; then
        print_status "eclipse-apps/labwc no encontrado, saltando..."
        return 0
    fi

    for _need in meson ninja musl-gcc; do
        if ! command -v "$_need" &>/dev/null; then
            print_warning "labwc: falta '$_need' en PATH; omitiendo compositor."
            return 0
        fi
    done

    if ! pkg-config --exists hwdata 2>/dev/null; then
        print_warning "labwc: paquete hwdata no detectado (pkg-config hwdata); el configure de wlroots puede fallar. Instala hwdata."
    fi

    local _labwc_script="$BASE_DIR/eclipse-apps/labwc/scripts/build-labwc-musl.sh"
    if [ ! -f "$_labwc_script" ]; then
        print_warning "labwc: no existe $_labwc_script; omitiendo."
        return 0
    fi

    print_status "Ejecutando build-labwc-musl.sh (puede tardar varios minutos)..."
    if bash "$_labwc_script"; then
        print_success "labwc compilado: eclipse-apps/labwc/build/labwc"
    else
        print_warning "Compilación de labwc falló (componente opcional, continuando)."
        return 0
    fi
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
    build_sidewind_project

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

        # Crear directorios base si no existen
        mkdir -p "$BUILD_DIR/usr/bin"
        mkdir -p "$BUILD_DIR/usr/sbin"
        mkdir -p "$BUILD_DIR/lib"

        # Forzar la inclusión del intérprete musl (necesario para binarios dinámicos)
        # El kernel busca /lib/ld-musl-x86_64.so.1 para cargar el ELF dinámico
        for ld in /usr/lib/x86_64-linux-musl/ld-musl-x86_64.so.1 /usr/lib/ld-musl-x86_64.so.1 /lib/ld-musl-x86_64.so.1; do
            if [ -f "$ld" ]; then
                cp -a "$ld" "$BUILD_DIR/lib/ld-musl-x86_64.so.1"
                print_status "Intérprete musl instalado: $ld -> /lib/"
                break
            fi
        done
        
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
version = "0.2.0"
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
tooltip_format = "Eclipse OS v0.2.0 - Desktop Environment"
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
    
    # Copiar smithay_app (Compositor Rust)
    if [ -f "$BUILD_DIR/sysroot/usr/bin/smithay_app" ]; then
        mkdir -p "$BUILD_DIR/usr/bin"
        cp "$BUILD_DIR/sysroot/usr/bin/smithay_app" "$BUILD_DIR/usr/bin/"
        print_status "smithay_app (Rust Compositor) copiado"
    fi
    
    # Crear configuración UEFI básica (no GRUB ya que usamos bootloader UEFI personalizado)
    cat > "$BUILD_DIR/efi/boot/uefi_config.txt" << EOF
# Configuración UEFI para Eclipse OS v0.2.0
# Bootloader personalizado - no requiere GRUB

[system]
kernel_path = "/boot/eclipse_kernel"
userland_path = "/userland/bin/eclipse_userland"

[debug]
enable_debug = false
log_level = "info"
EOF
    
    # Crear symlink para fuentes (TinyX espera /usr/share/fonts/X11/)
    mkdir -p "$BUILD_DIR/usr/share/fonts/X11/misc/"
    if [ -d "eclipse-apps/sources/" ]; then
        cp -f eclipse-apps/sources/* "$BUILD_DIR/usr/share/fonts/X11/misc/"
        print_status "Copiado de fuentes a $BUILD_DIR/usr/share/fonts/X11/misc/"
    fi
    
    print_success "Distribución básica creada en $BUILD_DIR"
}

# Función para crear imagen USB booteable
create_bootable_image() {
    print_step "Creando imagen USB booteable (ROOTLESS mode)..."
    
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
    local ESP_IMG="esp_temp.img"
    local ROOT_IMG="root_temp.img"
    
    # 1. Crear la partición ESP (FAT32) con mtools
    local ESP_SIZE=128
    local ESP_OFFSET=1
    print_status "Creando partición ESP (${ESP_SIZE}MB) sin root..."
    rm -f "$ESP_IMG"
    truncate -s ${ESP_SIZE}M "$ESP_IMG"
    mkfs.fat -F32 -n "ECLIPSE_OS" "$ESP_IMG" > /dev/null
    
    # Crear directorios en la imagen FAT32 usando mtools
    mmd -i "$ESP_IMG" ::/EFI
    mmd -i "$ESP_IMG" ::/EFI/BOOT
    mmd -i "$ESP_IMG" ::/boot
    mmd -i "$ESP_IMG" ::/eclipse
    
    # Copiar archivos usando mcopy
    mcopy -i "$ESP_IMG" "$BOOTLOADER_PATH" ::/EFI/BOOT/BOOTX64.EFI
    mcopy -i "$ESP_IMG" "$KERNEL_PATH" ::/boot/eclipse_kernel
    
    # Crear configuración de boot
    cat > boot_temp.cfg << 'EOF'
# Eclipse OS Boot Configuration
kernel=/boot/eclipse_kernel
resolution=1024x768
debug=false
EOF
    mcopy -i "$ESP_IMG" boot_temp.cfg ::/eclipse/boot.cfg
    rm boot_temp.cfg
    
    # 2. Crear la partición EclipseFS
    local ROOT_SIZE=4096
    local ROOT_OFFSET=130
    print_status "Creando partición EclipseFS (${ROOT_SIZE}MB) sin root..."
    rm -f "$ROOT_IMG"
    truncate -s ${ROOT_SIZE}M "$ROOT_IMG"
    
    if [ -f "mkfs-eclipsefs/target/release/mkfs-eclipsefs" ]; then
        ./mkfs-eclipsefs/target/release/mkfs-eclipsefs -f -L "EclipseOS" -N 10000 "$ROOT_IMG" > /dev/null
        
        if [ -f "populate-eclipsefs/target/release/populate-eclipsefs" ] && [ -d "$BUILD_DIR" ]; then
            print_status "Poblando filesystem EclipseFS con populate-eclipsefs..."
            ./populate-eclipsefs/target/release/populate-eclipsefs "$ROOT_IMG" "$BUILD_DIR" > /dev/null
        fi
    fi
    
    # 3. Ensamblar la imagen final con tabla GPT
    local IMAGE_SIZE=$((ROOT_OFFSET + ROOT_SIZE + 10))
    print_status "Ensamblando imagen final $IMG_FILE (${IMAGE_SIZE}MB)..."
    rm -f "$IMG_FILE"
    truncate -s ${IMAGE_SIZE}M "$IMG_FILE"
    
    # Calcular marcas de fin para parted
    local ESP_END=$((ESP_OFFSET + ESP_SIZE))
    local ROOT_END=$((ROOT_OFFSET + ROOT_SIZE))

    # Usar parted en el archivo local (no requiere sudo para archivos)
    PARTED_CMD="parted"
    "$PARTED_CMD" "$IMG_FILE" --script mklabel gpt
    "$PARTED_CMD" "$IMG_FILE" --script mkpart ESP fat32 ${ESP_OFFSET}MiB ${ESP_END}MiB
    "$PARTED_CMD" "$IMG_FILE" --script set 1 esp on
    "$PARTED_CMD" "$IMG_FILE" --script mkpart primary ext4 ${ROOT_OFFSET}MiB ${ROOT_END}MiB
    
    # Escribir las particiones en los offsets correctos usando dd
    print_status "Escribiendo particiones en la imagen final..."
    # Offset MiB match seek con bs=1M
    dd if="$ESP_IMG" of="$IMG_FILE" bs=1M seek=$ESP_OFFSET conv=notrunc status=none
    dd if="$ROOT_IMG" of="$IMG_FILE" bs=1M seek=$ROOT_OFFSET conv=notrunc status=none
    
    # Limpieza de archivos temporales
    rm -f "$ESP_IMG" "$ROOT_IMG"
    
    print_success "✓ Imagen booteable creada exitosamente (ROOTLESS): $IMG_FILE ($(du -h "$IMG_FILE" | cut -f1))"
    echo ""
    print_success "Para probar ejecuta: ./qemu.sh"
}

# Función para mostrar resumen de construcción
show_build_summary() {
    echo ""
    print_success "Compilación completada exitosamente"
    echo ""
    echo "Binarios compilados:"
    echo "Componentes compilados:"
    echo "  Librería EclipseFS: eclipsefs-lib/target/debug/libeclipsefs_lib.rlib"
    echo "  Init Process: eclipse_kernel/userspace/init/target/x86_64-unknown-linux-musl/release/eclipse-init"
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
    echo "  Rwaybar: eclipse-apps/target/x86_64-unknown-linux-musl/release/rwaybar"
    echo "  labwc (musl): eclipse-apps/labwc/build/labwc"
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
    echo "  - labwc: $BUILD_DIR/usr/bin/labwc"
    echo ""
    
    # Mostrar imagen USB si existe
    if [ -f "eclipse_os.img" ]; then
        echo "Imagen USB booteable:"
        echo "  - eclipse_os.img ($(du -h eclipse_os.img | cut -f1))"
        echo "  - Lista para usar en: sudo ./qemu.sh"
    fi
    
    echo ""
    echo "Eclipse OS v0.2.0 está listo para usar!"
}

# Función para compilar eclipse-apps (IPC + systemd)
# build_eclipse_apps fue eliminado: libs/ipc es una crate std y
# no es compatible con el workspace no_std del target Eclipse.

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
    
    # eclipsefs-cli es herramienta host (Linux), usa std. NO usar x86_64-unknown-linux-musl.
    print_status "Compilando eclipsefs CLI tool..."
    cargo build --release
    
    if [ $? -eq 0 ]; then
        print_success "eclipsefs-cli compilado exitosamente"
        
        local cli_path="target/release/eclipsefs"
        if [ -f "$cli_path" ]; then
            local cli_size=$(du -h "$cli_path" | cut -f1)
            print_status "eclipsefs CLI generado: $cli_path ($cli_size)"
        fi
    else
        print_warning "Compilación de eclipsefs-cli falló (componente opcional, continuando)"
        cd ..
        return 0
    fi
    
    cd ..
}

# Función principal
main() {
    # Ejecutar pasos de construcción
    build_eclipsefs_lib
    build_mkfs_eclipsefs
    build_populate_eclipsefs
    prepare_sysroot
    build_eclipse_libc
    build_eclipsefs_cli
    build_eclipse_init
    build_userspace_services
    build_kernel
    build_bootloader
    build_installer
    build_systemd
    # Sidewind/workspace Eclipse: build_userland → build_sidewind_project
    build_userland
    
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
