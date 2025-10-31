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
BUILD_DIR="eclipse-os-build"

echo "╔══════════════════════════════════════════════════════════════════════╗"
echo "║ ECLIPSE OS - SCRIPT DE CONSTRUCCIÓN COMPLETO v0.6.0 ║"
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

# Función para compilar el kernel
build_kernel() {
    print_step "Compilando kernel Eclipse OS v0.6.0..."
    
    # Compilar el kernel directamente con cargo (forzar uso de linker.ld absoluto)
    print_status "Compilando kernel para target $KERNEL_TARGET..."
    cd eclipse_kernel
    if [ "${KERNEL_MINIMAL:-0}" = "1" ]; then
        print_status "Modo MINIMAL: compilando kernel sin características opcionales"
        rustup run nightly cargo build --target x86_64-unknown-none --release
    else
        rustup run nightly cargo build --target x86_64-unknown-none --release --features cosmic-desktop,ai-models
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

        # Crear directorios /usr/bin y /usr/sbin si no existen
        mkdir -p "$BUILD_DIR/usr/bin"
        mkdir -p "$BUILD_DIR/usr/sbin"
        
        # Copiar systemd si existe
        if [ -f "eclipse-apps/systemd/target/release/eclipse-systemd" ]; then
            cp "eclipse-apps/systemd/target/release/eclipse-systemd" "$BUILD_DIR/userland/bin/"
            # También instalar en /usr/bin/ para que el kernel lo encuentre
            cp "eclipse-apps/systemd/target/release/eclipse-systemd" "$BUILD_DIR/usr/sbin/"
            chmod +x "$BUILD_DIR/usr/sbin/eclipse-systemd"
            print_status "Systemd copiado e instalado en /usr/sbin/"
        fi
        
        # Copiar binarios de Wayland y COSMIC a /usr/bin/
        if [ -f "eclipse-apps/services/waylandd/target/release/eclipse_wayland" ]; then
            cp "eclipse-apps/services/waylandd/target/release/eclipse_wayland" "$BUILD_DIR/usr/bin/"
            chmod +x "$BUILD_DIR/usr/bin/eclipse_wayland"
            print_status "eclipse_wayland instalado en /usr/bin/"
        fi
        
        if [ -f "eclipse-apps/apps/cosmic/target/release/eclipse_cosmic" ]; then
            cp "eclipse-apps/apps/cosmic/target/release/eclipse_cosmic" "$BUILD_DIR/usr/bin/"
            chmod +x "$BUILD_DIR/usr/bin/eclipse_cosmic"
            print_status "eclipse_cosmic instalado en /usr/bin/"
        fi
        
        if [ -f "eclipse-apps/apps/rwaybar/target/release/rwaybar" ]; then
            cp "eclipse-apps/apps/rwaybar/target/release/rwaybar" "$BUILD_DIR/usr/bin/"
            chmod +x "$BUILD_DIR/usr/bin/rwaybar"
            print_status "rwaybar instalado en /usr/bin/"
        fi
        
        if [ -f "eclipse-apps/apps/eclipse_taskbar/target/release/eclipse_taskbar" ]; then
            cp "eclipse-apps/apps/eclipse_taskbar/target/release/eclipse_taskbar" "$BUILD_DIR/usr/bin/"
            chmod +x "$BUILD_DIR/usr/bin/eclipse_taskbar"
            print_status "eclipse_taskbar instalado en /usr/bin/"
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

[desktop_environment]
wayland_server = "/userland/bin/eclipse_wayland"
cosmic_desktop = "/userland/bin/eclipse_cosmic"
rwaybar = "/userland/bin/rwaybar"
eclipse_taskbar = "/userland/bin/eclipse_taskbar"
eclipse_notifications = "/userland/bin/eclipse_notifications"
eclipse_window_manager = "/userland/bin/eclipse_window_manager"

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

echo "🚀 Iniciando eclipse_wayland (servidor Wayland + IPC)..."
./eclipse_wayland &
WAYLANDD_PID=$!
sleep 3

echo "🖥️ Iniciando eclipse_cosmic (desktop environment)..."
./eclipse_cosmic &
COSMIC_PID=$!
sleep 3

echo "📊 Iniciando rwaybar (barra de tareas Wayland)..."
./rwaybar --config /userland/config/rwaybar.toml &
RWAYBAR_PID=$!
sleep 2

echo "🔔 Iniciando eclipse_notifications..."
./eclipse_notifications &
NOTIFICATIONS_PID=$!
sleep 1

echo "🖼️ Iniciando eclipse_window_manager..."
./eclipse_window_manager &
WINDOW_MANAGER_PID=$!
sleep 1

echo "✅ Eclipse OS Desktop Environment iniciado completamente!"
echo "   - eclipse_wayland PID: $WAYLANDD_PID"
echo "   - eclipse_cosmic PID: $COSMIC_PID"
echo "   - rwaybar PID: $RWAYBAR_PID"
echo "   - eclipse_notifications PID: $NOTIFICATIONS_PID"
echo "   - eclipse_window_manager PID: $WINDOW_MANAGER_PID"

# Mantener el script en ejecución
wait $WINDOW_MANAGER_PID $NOTIFICATIONS_PID $RWAYBAR_PID $COSMIC_PID $WAYLANDD_PID
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
tooltip_format = "Eclipse OS v0.6.0 - Desktop Environment"
EOF
        print_status "Configuración de rwaybar creada"
        
        print_success "Módulos userland copiados a la distribución"
    fi
    
    # Copiar binarios de eclipse-apps si existen
    if [ -f "eclipse-apps/target/release/eclipse_wayland" ]; then
        cp "eclipse-apps/target/release/eclipse_wayland" "$BUILD_DIR/userland/bin/"
        print_status "eclipse_wayland copiado"
    fi

    if [ -f "eclipse-apps/target/release/eclipse_cosmic" ]; then
        cp "eclipse-apps/target/release/eclipse_cosmic" "$BUILD_DIR/userland/bin/"
        print_status "eclipse_cosmic copiado"
    fi

    if [ -f "eclipse-apps/target/release/rwaybar" ]; then
        cp "eclipse-apps/target/release/rwaybar" "$BUILD_DIR/userland/bin/"
        print_status "rwaybar copiado"
    fi

    if [ -f "eclipse-apps/target/release/eclipse_taskbar" ]; then
        cp "eclipse-apps/target/release/eclipse_taskbar" "$BUILD_DIR/userland/bin/"
        print_status "eclipse_taskbar copiado"
    fi

    if [ -f "eclipse-apps/target/release/eclipse_notifications" ]; then
        cp "eclipse-apps/target/release/eclipse_notifications" "$BUILD_DIR/userland/bin/"
        print_status "eclipse_notifications copiado"
    fi

    if [ -f "eclipse-apps/target/release/eclipse_window_manager" ]; then
        cp "eclipse-apps/target/release/eclipse_window_manager" "$BUILD_DIR/userland/bin/"
        print_status "eclipse_window_manager copiado"
    fi

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
    print_success "Compilación completada exitosamente"
    echo ""
    echo "Binarios compilados:"
    echo "Componentes compilados:"
    echo "  Librería EclipseFS: eclipsefs-lib/target/debug/libeclipsefs_lib.rlib"
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
    echo "Desktop Environment:"
    echo "  eclipse_wayland: eclipse-apps/target/release/eclipse_wayland"
    echo "  eclipse_cosmic: eclipse-apps/target/release/eclipse_cosmic"
    echo "  rwaybar: eclipse-apps/target/release/rwaybar"
    echo "  eclipse_taskbar: eclipse-apps/target/release/eclipse_taskbar"
    echo "  eclipse_notifications: eclipse-apps/target/release/eclipse_notifications"
    echo "  eclipse_window_manager: eclipse-apps/target/release/eclipse_window_manager"
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

# Función para compilar eclipse-apps (IPC + waylandd + cosmic + rwaybar + notificaciones + window manager)
build_eclipse_apps() {
    print_step "Compilando workspace eclipse-apps (IPC + waylandd + cosmic + rwaybar + notificaciones + window manager)..."

    if [ ! -d "eclipse-apps" ]; then
        print_status "Directorio eclipse-apps no encontrado, saltando..."
        return 0
    fi

    cd eclipse-apps

    print_status "Compilando librerías IPC..."
    cargo build -p ipc_simple --release || { cd ..; print_error "Fallo compilando ipc_simple"; return 1; }
    cargo build -p ipc_common --release || { cd ..; print_error "Fallo compilando ipc_common"; return 1; }

    print_status "Compilando eclipse_wayland..."
    cargo build -p eclipse_wayland --release || { cd ..; print_error "Fallo compilando eclipse_wayland"; return 1; }

    print_status "Compilando eclipse_cosmic..."
    cargo build -p eclipse_cosmic --release || { cd ..; print_error "Fallo compilando eclipse_cosmic"; return 1; }

    print_status "Compilando rwaybar..."
    cargo build -p rwaybar --release || { cd ..; print_error "Fallo compilando rwaybar"; return 1; }

    print_status "Compilando eclipse_taskbar..."
    cargo build -p eclipse_taskbar --release || { cd ..; print_error "Fallo compilando eclipse_taskbar"; return 1; }

    print_status "Compilando eclipse_notifications..."
    cargo build -p eclipse_notifications --release || { cd ..; print_error "Fallo compilando eclipse_notifications"; return 1; }

    print_status "Compilando eclipse_window_manager..."
    cargo build -p eclipse_window_manager --release || { cd ..; print_error "Fallo compilando eclipse_window_manager"; return 1; }

    print_success "eclipse-apps compilado completamente"
    cd ..
}

# Función principal
main() {
    # Ejecutar pasos de construcción
    build_eclipsefs_lib
    build_kernel
    build_bootloader
    build_installer
    build_systemd
    build_eclipse_apps
    build_userland
    
    # Crear distribución completa para compatibilidad con instalador
    create_basic_distribution
    
    show_build_summary
}

# Ejecutar función principal
main "$@"
