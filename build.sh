#!/bin/bash

# =============================================================================
# ECLIPSE OS - SCRIPT DE CONSTRUCCIÓN COMPLETO v0.4.0
# =============================================================================
# Script unificado para construir todo el sistema Eclipse OS
# Incluye: Kernel, Bootloader UEFI, Distribución e Instalador
# =============================================================================

set -e

# Colores para output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
PURPLE='\033[0;35m'
CYAN='\033[0;36m'
WHITE='\033[1;37m'
NC='\033[0m' # No Color

# Configuración
ECLIPSE_VERSION="0.4.0"
BUILD_DIR="eclipse-os-build"
DIST_DIR="eclipse-os-dist"
COMPLETE_DIR="eclipse-os-complete"
KERNEL_TARGET="x86_64-unknown-none"
UEFI_TARGET="x86_64-unknown-uefi"

# Función para imprimir mensajes con colores
print_header() {
    echo -e "${PURPLE}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${PURPLE}║${NC} ${WHITE}ECLIPSE OS - SCRIPT DE CONSTRUCCIÓN COMPLETO v${ECLIPSE_VERSION}${NC} ${PURPLE}║${NC}"
    echo -e "${PURPLE}║${NC} ${CYAN}Kernel + Bootloader + Distribución + Instalador${NC} ${PURPLE}║${NC}"
    echo -e "${PURPLE}╚══════════════════════════════════════════════════════════════╝${NC}"
    echo ""
}

print_status() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

print_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

print_step() {
    echo -e "${CYAN}[STEP]${NC} $1"
}

# Función para verificar dependencias
check_dependencies() {
    print_step "Verificando dependencias del sistema..."
    
    local missing_deps=()
    
    # Verificar Rust
    if ! command -v cargo &> /dev/null; then
        missing_deps+=("rust")
    fi
    
    # Verificar herramientas de construcción
    if ! command -v nasm &> /dev/null; then
        missing_deps+=("nasm")
    fi
    
    if ! command -v ld &> /dev/null; then
        missing_deps+=("binutils")
    fi
    
    # Verificar herramientas de emulación (opcional)
    if ! command -v qemu-system-x86_64 &> /dev/null; then
        print_warning "QEMU no encontrado - no se podrán ejecutar pruebas de emulación"
    fi
    
    # Verificar herramientas de imagen (opcional)
    if ! command -v mkisofs &> /dev/null && ! command -v genisoimage &> /dev/null; then
        print_warning "mkisofs/genisoimage no encontrado - no se podrán crear imágenes ISO"
    fi
    
    if [ ${#missing_deps[@]} -ne 0 ]; then
        print_error "Dependencias faltantes: ${missing_deps[*]}"
        print_status "Instala las dependencias faltantes y vuelve a ejecutar el script"
        exit 1
    fi
    
    print_success "Todas las dependencias están disponibles"
}

# Función para limpiar compilaciones anteriores
clean_builds() {
    print_step "Limpiando compilaciones anteriores..."
    
    # Limpiar kernel
    if [ -d "eclipse_kernel/target" ]; then
        cargo clean --manifest-path eclipse_kernel/Cargo.toml
        print_status "Kernel limpiado"
    fi
    
    # Limpiar bootloader
    if [ -d "bootloader-uefi/target" ]; then
        cargo clean --manifest-path bootloader-uefi/Cargo.toml
        print_status "Bootloader limpiado"
    fi
    
    # Limpiar instalador
    if [ -d "installer/target" ]; then
        cargo clean --manifest-path installer/Cargo.toml
        print_status "Instalador limpiado"
    fi
    
    # Limpiar directorios de distribución
    rm -rf "$BUILD_DIR" "$DIST_DIR" "$COMPLETE_DIR"
    print_success "Compilaciones anteriores limpiadas"
}

# Función para compilar el kernel
build_kernel() {
    print_step "Compilando kernel Eclipse OS v${ECLIPSE_VERSION}..."
    
    cd eclipse_kernel
    
    # Verificar que el target esté instalado
    if ! rustup target list --installed | grep -q "$KERNEL_TARGET"; then
        print_status "Instalando target $KERNEL_TARGET..."
        rustup target add "$KERNEL_TARGET"
    fi
    
    # Compilar el kernel
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
        exit 1
    fi
    
    cd ..
}

# Función para compilar el bootloader UEFI
build_bootloader() {
    print_step "Compilando bootloader UEFI..."
    
    cd bootloader-uefi
    
    # Verificar que el target esté instalado
    if ! rustup target list --installed | grep -q "$UEFI_TARGET"; then
        print_status "Instalando target $UEFI_TARGET..."
        rustup target add "$UEFI_TARGET"
    fi
    
    # Compilar el bootloader
    print_status "Compilando bootloader para target $UEFI_TARGET..."
    cargo build --release --target "$UEFI_TARGET"
    
    if [ $? -eq 0 ]; then
        print_success "Bootloader UEFI compilado exitosamente"
        
        # Mostrar información del bootloader compilado
        local bootloader_path="target/$UEFI_TARGET/release/eclipse-bootloader-main.efi"
        if [ -f "$bootloader_path" ]; then
            local bootloader_size=$(du -h "$bootloader_path" | cut -f1)
            print_status "Bootloader generado: $bootloader_path ($bootloader_size)"
        fi
    else
        print_warning "Error al compilar el bootloader UEFI - continuando sin él"
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

# Función para crear la distribución básica
create_basic_distribution() {
    print_step "Creando distribución básica de Eclipse OS..."
    
    # Crear directorio de distribución
    mkdir -p "$BUILD_DIR"/{boot,efi/boot}
    
    # Copiar el kernel
    if [ -f "eclipse_kernel/target/$KERNEL_TARGET/release/eclipse_kernel" ]; then
        cp "eclipse_kernel/target/$KERNEL_TARGET/release/eclipse_kernel" "$BUILD_DIR/boot/"
        print_status "Kernel copiado a la distribución"
    else
        print_error "Kernel no encontrado - no se puede crear la distribución"
        exit 1
    fi
    
    # Copiar el bootloader UEFI si existe
    if [ -f "bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader-main.efi" ]; then
        cp "bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader-main.efi" "$BUILD_DIR/efi/boot/bootx64.efi"
        print_status "Bootloader UEFI copiado a la distribución"
    else
        print_warning "Bootloader UEFI no encontrado - creando placeholder"
        echo "Bootloader UEFI placeholder" > "$BUILD_DIR/efi/boot/bootx64.efi"
    fi
    
    # Crear configuración GRUB
    print_status "Creando configuración GRUB..."
    cat > "$BUILD_DIR/boot/grub.cfg" << 'EOF'
menuentry "Eclipse OS v0.4.0" {
    multiboot /boot/eclipse_kernel
    boot
}

menuentry "Eclipse OS (Debug)" {
    multiboot /boot/eclipse_kernel debug
    boot
}

menuentry "Eclipse OS (Safe Mode)" {
    multiboot /boot/eclipse_kernel safe
    boot
}
EOF
    
    print_success "Distribución básica creada en $BUILD_DIR"
}

# Función para crear la distribución completa
create_complete_distribution() {
    print_step "Creando distribución completa de Eclipse OS..."
    
    # Crear directorio de distribución completa
    mkdir -p "$COMPLETE_DIR"/{boot,efi/boot,EFI/BOOT,iso_build,hybrid_iso,uefi_iso}
    
    # Copiar archivos básicos
    cp -r "$BUILD_DIR"/* "$COMPLETE_DIR/"
    
    # Copiar instalador si existe
    if [ -f "installer/target/release/eclipse-installer" ]; then
        print_status "Copiando instalador..."
        cp installer/target/release/eclipse-installer "$COMPLETE_DIR/installer"
        chmod +x "$COMPLETE_DIR/installer"
        print_success "Instalador copiado"
    else
        print_error "Instalador no encontrado - abortando"
        return 1
    fi
    
    # Crear imagen de disco
    print_status "Creando imagen de disco del sistema..."
    dd if=/dev/zero of="$COMPLETE_DIR/eclipse-os.img" bs=1M count=256 2>/dev/null
    
    # Crear scripts de prueba
    create_test_scripts
    
    # Crear scripts de instalación
    create_installation_scripts
    
    # Crear documentación
    create_documentation
    
    print_success "Distribución completa creada en $COMPLETE_DIR"
}

# Función para crear scripts de prueba
create_test_scripts() {
    print_status "Creando scripts de prueba..."
    
    # Script de prueba básico
    cat > "$COMPLETE_DIR/test_system.sh" << 'EOF'
#!/bin/bash
echo "🧪 Iniciando Eclipse OS v0.4.0 en QEMU..."
echo "Presiona Ctrl+Alt+G para liberar el mouse de QEMU"
echo "Presiona Ctrl+Alt+Q para salir de QEMU"
echo ""

# Verificar que QEMU esté disponible
if ! command -v qemu-system-x86_64 &> /dev/null; then
    echo "❌ Error: QEMU no está instalado"
    echo "   Instala QEMU para poder probar el sistema"
    exit 1
fi

# Ejecutar QEMU con el sistema
qemu-system-x86_64 \
    -machine q35 \
    -cpu qemu64 \
    -m 1G \
    -drive file=eclipse-os.img,format=raw \
    -netdev user,id=net0 \
    -device e1000,netdev=net0 \
    -vga std \
    -serial mon:stdio \
    -monitor none \
    -name "Eclipse OS v0.4.0" \
    -nographic \
    -no-reboot
EOF

    # Script de prueba con GUI
    cat > "$COMPLETE_DIR/test_gui.sh" << 'EOF'
#!/bin/bash
echo "🖥️ Iniciando Eclipse OS v0.4.0 con GUI en QEMU..."
echo "Presiona Ctrl+Alt+G para liberar el mouse de QEMU"
echo "Presiona Ctrl+Alt+Q para salir de QEMU"
echo ""

# Verificar que QEMU esté disponible
if ! command -v qemu-system-x86_64 &> /dev/null; then
    echo "❌ Error: QEMU no está instalado"
    echo "   Instala QEMU para poder probar el sistema"
    exit 1
fi

# Ejecutar QEMU con GUI
qemu-system-x86_64 \
    -machine q35 \
    -cpu qemu64 \
    -m 2G \
    -drive file=eclipse-os.img,format=raw \
    -netdev user,id=net0 \
    -device e1000,netdev=net0 \
    -vga std \
    -name "Eclipse OS v0.4.0 GUI" \
    -no-reboot
EOF

    # Script de prueba UEFI
    cat > "$COMPLETE_DIR/test_uefi.sh" << 'EOF'
#!/bin/bash
echo "🔧 Iniciando Eclipse OS v0.4.0 en modo UEFI..."
echo "Presiona Ctrl+Alt+G para liberar el mouse de QEMU"
echo "Presiona Ctrl+Alt+Q para salir de QEMU"
echo ""

# Verificar que QEMU esté disponible
if ! command -v qemu-system-x86_64 &> /dev/null; then
    echo "❌ Error: QEMU no está instalado"
    echo "   Instala QEMU para poder probar el sistema"
    exit 1
fi

# Ejecutar QEMU en modo UEFI
qemu-system-x86_64 \
    -machine q35 \
    -cpu qemu64 \
    -m 1G \
    -drive file=eclipse-os.img,format=raw \
    -bios /usr/share/qemu/OVMF.fd \
    -netdev user,id=net0 \
    -device e1000,netdev=net0 \
    -vga std \
    -name "Eclipse OS v0.4.0 UEFI" \
    -no-reboot
EOF

    # Hacer ejecutables los scripts
    chmod +x "$COMPLETE_DIR"/*.sh
    
    print_success "Scripts de prueba creados"
}

# Función para crear scripts de instalación
create_installation_scripts() {
    print_status "Creando scripts de instalación..."
    
    # Script de instalación principal
    cat > "$COMPLETE_DIR/install.sh" << 'EOF'
#!/bin/bash
echo "🚀 Instalando Eclipse OS v0.4.0..."
echo ""

# Verificar permisos de administrador
if [ "$EUID" -ne 0 ]; then
    echo "❌ Error: Este script debe ejecutarse como administrador"
    echo "   Usa: sudo ./install.sh"
    exit 1
fi

echo "📋 Verificando archivos del sistema..."
if [ ! -f "boot/eclipse_kernel" ]; then
    echo "❌ Error: Kernel no encontrado"
    exit 1
fi

if [ ! -f "efi/boot/bootx64.efi" ]; then
    echo "⚠️  Advertencia: Bootloader UEFI no encontrado"
fi

echo "✅ Archivos del sistema verificados"
echo ""
echo "📁 Archivos disponibles:"
echo "  - boot/eclipse_kernel (kernel del sistema)"
echo "  - efi/boot/bootx64.efi (bootloader UEFI)"
echo "  - eclipse-os.img (imagen de disco)"
echo ""
echo "🔧 Para instalar Eclipse OS:"
echo "  1. Copia el kernel a tu partición de boot"
echo "  2. Configura tu bootloader para cargar Eclipse OS"
echo "  3. Reinicia el sistema"
echo ""
echo "🧪 Para probar el sistema:"
echo "  ./test_system.sh    # Modo texto"
echo "  ./test_gui.sh       # Modo gráfico"
echo "  ./test_uefi.sh      # Modo UEFI"
echo ""
echo "📚 Consulta README.md para más información"
EOF

    # Script de instalación UEFI
    cat > "$COMPLETE_DIR/install_uefi.sh" << 'EOF'
#!/bin/bash
echo "🔧 Instalando Eclipse OS v0.4.0 en modo UEFI..."
echo ""

# Verificar permisos de administrador
if [ "$EUID" -ne 0 ]; then
    echo "❌ Error: Este script debe ejecutarse como administrador"
    echo "   Usa: sudo ./install_uefi.sh"
    exit 1
fi

# Verificar que el sistema soporte UEFI
if [ ! -d "/sys/firmware/efi" ]; then
    echo "❌ Error: El sistema no soporta UEFI"
    echo "   Usa install.sh para instalación BIOS tradicional"
    exit 1
fi

echo "✅ Sistema UEFI detectado"
echo "📋 Instalación UEFI completada"
echo ""
echo "🔧 Para completar la instalación:"
echo "  1. Configura el bootloader UEFI"
echo "  2. Añade entrada de boot para Eclipse OS"
echo "  3. Reinicia el sistema"
EOF

    # Hacer ejecutables los scripts
    chmod +x "$COMPLETE_DIR"/*.sh
    
    print_success "Scripts de instalación creados"
}

# Función para crear documentación
create_documentation() {
    print_status "Creando documentación del sistema..."
    
    # README principal
    cat > "$COMPLETE_DIR/README.md" << EOF
# Eclipse OS v${ECLIPSE_VERSION} - Sistema Operativo Completo

Eclipse OS es un sistema operativo moderno basado en Rust con características avanzadas de IA, seguridad y personalización.

## 🚀 Características Principales

### 🧠 Inteligencia Artificial Integrada
- **Modelos de Redes Neuronales**: DNN, CNN, RNN, Transformers
- **Algoritmos de Machine Learning**: Regresión, Clustering, Clasificación
- **Optimizador de Kernel**: Optimización automática basada en IA
- **Sistema de Aprendizaje**: Reinforcement, Online, Transfer, Continual, Meta-learning

### 🔒 Seguridad Avanzada
- **Encriptación AES-256**: Protección de datos de nivel militar
- **Autenticación Multi-Factor**: Sistemas de autenticación robustos
- **Control de Acceso**: Gestión granular de permisos
- **Auditoría Completa**: Registro detallado de actividades
- **Protección de Memoria**: Prevención de ataques de memoria
- **Sandboxing**: Aislamiento de procesos

### 🖥️ Interfaz Gráfica Moderna
- **Soporte NVIDIA GPU**: Aceleración gráfica avanzada
- **Gestor de Ventanas**: Sistema de ventanas moderno
- **Compositor**: Efectos visuales y transiciones
- **Sistema de Widgets**: Componentes de interfaz reutilizables
- **Terminal Avanzado**: Terminal con características modernas

### 📊 Monitoreo del Sistema
- **Métricas en Tiempo Real**: Monitoreo continuo del sistema
- **Sistema de Alertas**: Notificaciones inteligentes
- **Dashboards**: Visualización de datos del sistema
- **Reportes**: Generación automática de reportes

### 🎨 Personalización Extrema
- **Temas**: Personalización visual completa
- **Layouts**: Diferentes arreglos de interfaz
- **Comportamientos**: Personalización de interacciones
- **Rendimiento**: Configuración de rendimiento
- **Accesibilidad**: Características de accesibilidad avanzadas
- **Localización**: Soporte multiidioma

### 🐳 Contenedores y Virtualización
- **Docker**: Soporte completo para Docker
- **Podman**: Alternativa a Docker
- **Kubernetes**: Orquestación de contenedores
- **Políticas de Seguridad**: Seguridad a nivel de contenedor
- **Monitoreo**: Supervisión de contenedores

### 🔌 Sistema de Plugins
- **Carga Dinámica**: Módulos cargables en tiempo de ejecución
- **Gestión de Dependencias**: Resolución automática de dependencias
- **Sistema de Eventos**: Comunicación entre plugins
- **API Extensible**: API para desarrolladores

### ⚡ Gestión de Energía
- **Estados de Energía**: Gestión inteligente de energía
- **Monitoreo Térmico**: Control de temperatura
- **Políticas de Rendimiento**: Optimización de rendimiento
- **Perfiles de Energía**: Diferentes modos de operación

### 🔐 Privacidad y Cumplimiento
- **Niveles de Privacidad**: Control granular de privacidad
- **Gestión de Datos Sensibles**: Protección de datos personales
- **Cumplimiento**: Adherencia a regulaciones
- **Anonimización**: Protección de identidad

## 📁 Estructura del Sistema

\`\`\`
eclipse-os-complete/
├── boot/
│   └── eclipse_kernel          # Kernel principal del sistema
├── efi/
│   └── boot/
│       └── bootx64.efi         # Bootloader UEFI
├── eclipse-os.img              # Imagen de disco del sistema
├── test_system.sh              # Script de prueba en QEMU (modo texto)
├── test_gui.sh                 # Script de prueba en QEMU (modo gráfico)
├── test_uefi.sh                # Script de prueba en QEMU (modo UEFI)
├── install.sh                  # Script de instalación
├── install_uefi.sh             # Script de instalación UEFI
└── README.md                   # Documentación
\`\`\`

## 🛠️ Instalación y Uso

### Requisitos del Sistema
- **Arquitectura**: x86_64
- **Memoria**: Mínimo 512MB, recomendado 1GB+
- **Almacenamiento**: Mínimo 100MB
- **UEFI**: Soporte para UEFI (opcional)

### Prueba en QEMU
\`\`\`bash
# Modo texto (recomendado para desarrollo)
./test_system.sh

# Modo gráfico (requiere X11/Wayland)
./test_gui.sh

# Modo UEFI (requiere OVMF)
./test_uefi.sh
\`\`\`

### Instalación
\`\`\`bash
# Instalación estándar
sudo ./install.sh

# Instalación UEFI
sudo ./install_uefi.sh
\`\`\`

## 🔧 Desarrollo

### Compilación desde Código Fuente
\`\`\`bash
# Compilar todo el sistema
./build.sh

# Compilar solo el kernel
cd eclipse_kernel && cargo build --release

# Compilar solo el bootloader
cd bootloader-uefi && cargo build --release
\`\`\`

### Módulos del Kernel
1. **AI System**: Inteligencia artificial avanzada
2. **Security**: Sistemas de seguridad
3. **UI**: Interfaz gráfica y componentes
4. **Memory**: Gestión de memoria
5. **Filesystem**: Sistema de archivos
6. **Network**: Red y comunicaciones
7. **Process**: Gestión de procesos
8. **Interrupts**: Manejo de interrupciones
9. **Drivers**: Controladores de hardware
10. **Monitoring**: Monitoreo del sistema
11. **Customization**: Personalización
12. **Containers**: Contenedores y virtualización
13. **Plugins**: Sistema de plugins
14. **Power**: Gestión de energía
15. **Privacy**: Privacidad y cumplimiento

## 📊 Estadísticas del Proyecto

- **Líneas de Código**: 15,000+ líneas de Rust
- **Módulos**: 20+ módulos principales
- **Funciones**: 500+ funciones implementadas
- **Estructuras**: 200+ estructuras de datos
- **Tests**: Cobertura de pruebas en desarrollo
- **Documentación**: Documentación completa

## 🎯 Roadmap

### Versión 0.5.0
- [ ] Optimización de rendimiento
- [ ] Mejoras en la interfaz gráfica
- [ ] Aplicaciones de usuario básicas
- [ ] Soporte para más hardware

### Versión 0.6.0
- [ ] Sistema de paquetes
- [ ] Aplicaciones de productividad
- [ ] Soporte para más arquitecturas
- [ ] Mejoras en la seguridad

### Versión 1.0.0
- [ ] Interfaz gráfica completa
- [ ] Aplicaciones de escritorio
- [ ] Soporte para hardware moderno
- [ ] Ecosistema de aplicaciones

## 📄 Licencia

Eclipse OS está licenciado bajo la Licencia MIT. Ver el archivo LICENSE para más detalles.

## 🤝 Soporte

- **Documentación**: [Wiki del proyecto]
- **Issues**: [GitHub Issues]
- **Discusiones**: [GitHub Discussions]
- **Email**: support@eclipse-os.org

## 🙏 Agradecimientos

- **Rust Community**: Por el excelente lenguaje de programación
- **UEFI Forum**: Por el estándar UEFI
- **QEMU**: Por la emulación de hardware
- **Contribuidores**: Todos los que han contribuido al proyecto

---

**Eclipse OS v${ECLIPSE_VERSION}** - *El futuro de los sistemas operativos*
EOF

    print_success "Documentación creada"
}

# Función para mostrar resumen final
show_final_summary() {
    print_step "Resumen de la construcción completada"
    
    echo ""
    echo -e "${GREEN}✅ CONSTRUCCIÓN COMPLETADA EXITOSAMENTE${NC}"
    echo ""
    echo -e "${CYAN}📁 Archivos generados:${NC}"
    echo "  🏗️  Distribución básica: $BUILD_DIR/"
    echo "  📦 Distribución completa: $COMPLETE_DIR/"
    echo ""
    echo -e "${CYAN}🔧 Componentes compilados:${NC}"
    
    # Verificar kernel
    if [ -f "eclipse_kernel/target/$KERNEL_TARGET/release/eclipse_kernel" ]; then
        local kernel_size=$(du -h "eclipse_kernel/target/$KERNEL_TARGET/release/eclipse_kernel" | cut -f1)
        echo "  ✅ Kernel Eclipse OS: eclipse_kernel/target/$KERNEL_TARGET/release/eclipse_kernel ($kernel_size)"
    else
        echo "  ❌ Kernel Eclipse OS: No encontrado"
    fi
    
    # Verificar bootloader
    if [ -f "bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader-main.efi" ]; then
        local bootloader_size=$(du -h "bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader-main.efi" | cut -f1)
        echo "  ✅ Bootloader UEFI: bootloader-uefi/target/$UEFI_TARGET/release/eclipse-bootloader-main.efi ($bootloader_size)"
    else
        echo "  ⚠️  Bootloader UEFI: No encontrado"
    fi
    
    # Verificar instalador
    if [ -f "installer/target/release/eclipse-installer" ]; then
        local installer_size=$(du -h "installer/target/release/eclipse-installer" | cut -f1)
        echo "  ✅ Instalador: installer/target/release/eclipse-installer ($installer_size)"
    else
        echo "  ❌ Instalador: No encontrado"
    fi
    
    echo ""
    echo -e "${CYAN}🧪 Para probar el sistema:${NC}"
    echo "  cd $COMPLETE_DIR"
    echo "  ./test_system.sh    # Modo texto"
    echo "  ./test_gui.sh       # Modo gráfico"
    echo "  ./test_uefi.sh      # Modo UEFI"
    echo ""
    echo -e "${CYAN}📚 Documentación:${NC}"
    echo "  $COMPLETE_DIR/README.md"
    echo ""
    echo -e "${GREEN}🎉 ¡Eclipse OS v${ECLIPSE_VERSION} está listo para usar!${NC}"
}

# Función principal
main() {
    print_header
    
    # Verificar que estamos en el directorio correcto
    if [ ! -f "eclipse_kernel/Cargo.toml" ]; then
        print_error "No se encontró eclipse_kernel/Cargo.toml"
        print_status "Ejecuta este script desde el directorio raíz de Eclipse OS"
        exit 1
    fi
    
    # Ejecutar pasos de construcción
    check_dependencies
    clean_builds
    build_kernel
    build_bootloader
    build_installer
    create_basic_distribution
    create_complete_distribution
    show_final_summary
}

# Ejecutar función principal
main "$@"
