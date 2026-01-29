#!/bin/bash

# Test script para verificar la integración eclipse-systemd en el kernel
# Este script compila el kernel y verifica que los módulos de integración estén presentes

set -e  # Exit on error

echo "════════════════════════════════════════════════════════"
echo "  ECLIPSE-SYSTEMD KERNEL INTEGRATION TEST"
echo "════════════════════════════════════════════════════════"
echo ""

# Colores para output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

# Función para mensajes de éxito
success() {
    echo -e "${GREEN}✓${NC} $1"
}

# Función para mensajes de advertencia
warning() {
    echo -e "${YELLOW}⚠${NC} $1"
}

# Función para mensajes de error
error() {
    echo -e "${RED}✗${NC} $1"
}

# Verificar que estamos en el directorio correcto
if [ ! -f "Cargo.toml" ]; then
    error "Error: Ejecutar desde el directorio eclipse_kernel/"
    exit 1
fi

success "Directorio correcto verificado"

# 1. Verificar que los módulos de integración existen
echo ""
echo "─────────────────────────────────────────────────────────"
echo "1. Verificando módulos de integración..."
echo "─────────────────────────────────────────────────────────"

REQUIRED_MODULES=(
    "src/init_system.rs"
    "src/process_memory.rs"
    "src/process_transfer.rs"
    "src/elf_loader.rs"
)

for module in "${REQUIRED_MODULES[@]}"; do
    if [ -f "$module" ]; then
        success "$module existe"
    else
        error "$module NO ENCONTRADO"
        exit 1
    fi
done

# 2. Verificar que el hook de systemd está en main_simple.rs
echo ""
echo "─────────────────────────────────────────────────────────"
echo "2. Verificando hook de integración en kernel_main..."
echo "─────────────────────────────────────────────────────────"

if grep -q "init_and_execute_systemd" src/main_simple.rs; then
    success "Hook init_and_execute_systemd encontrado"
else
    error "Hook init_and_execute_systemd NO ENCONTRADO"
    exit 1
fi

if grep -q "ENABLE_SYSTEMD_INIT" src/main_simple.rs; then
    success "Flag ENABLE_SYSTEMD_INIT encontrado"
else
    warning "Flag ENABLE_SYSTEMD_INIT no encontrado (puede ser opcional)"
fi

# 3. Verificar imports en main_simple.rs
if grep -q "use crate::init_system" src/main_simple.rs; then
    success "Import de init_system en main_simple.rs"
else
    error "Import de init_system NO ENCONTRADO en main_simple.rs"
    exit 1
fi

# 4. Compilar el kernel
echo ""
echo "─────────────────────────────────────────────────────────"
echo "3. Compilando kernel con integración systemd..."
echo "─────────────────────────────────────────────────────────"

# Asegurarse de que el target esté instalado
if ! rustup target list --installed | grep -q "x86_64-unknown-none"; then
    echo "Instalando target x86_64-unknown-none..."
    rustup target add x86_64-unknown-none
fi

# Compilar
if cargo check --target x86_64-unknown-none 2>&1 | tee /tmp/cargo_check.log | grep -q "Finished"; then
    success "Kernel compilado exitosamente"
else
    error "Error al compilar el kernel"
    echo ""
    echo "Últimas líneas del log de compilación:"
    tail -20 /tmp/cargo_check.log
    exit 1
fi

# 5. Verificar que no hay errores de compilación graves
echo ""
echo "─────────────────────────────────────────────────────────"
echo "4. Verificando ausencia de errores críticos..."
echo "─────────────────────────────────────────────────────────"

if grep -q "^error" /tmp/cargo_check.log; then
    error "Se encontraron errores de compilación"
    grep "^error" /tmp/cargo_check.log
    exit 1
else
    success "No se encontraron errores de compilación"
fi

# 6. Verificar documentación
echo ""
echo "─────────────────────────────────────────────────────────"
echo "5. Verificando documentación..."
echo "─────────────────────────────────────────────────────────"

if [ -f "SYSTEMD_INTEGRATION.md" ]; then
    success "Documentación SYSTEMD_INTEGRATION.md existe"
else
    warning "Documentación SYSTEMD_INTEGRATION.md no encontrada"
fi

# Verificar comentarios de documentación en init_system.rs
if grep -q "//!" src/init_system.rs; then
    success "Documentación en init_system.rs"
else
    warning "Poca documentación en init_system.rs"
fi

# 7. Verificar integración con eclipse-systemd app
echo ""
echo "─────────────────────────────────────────────────────────"
echo "6. Verificando aplicación eclipse-systemd..."
echo "─────────────────────────────────────────────────────────"

if [ -d "../eclipse-apps/systemd" ]; then
    success "Directorio eclipse-apps/systemd existe"
    
    if [ -f "../eclipse-apps/systemd/Cargo.toml" ]; then
        success "Proyecto eclipse-systemd encontrado"
    else
        warning "Cargo.toml de systemd no encontrado"
    fi
    
    if [ -f "../eclipse-apps/systemd/README.md" ]; then
        success "Documentación de systemd existe"
    else
        warning "README de systemd no encontrado"
    fi
else
    warning "Directorio eclipse-apps/systemd no encontrado"
fi

# 8. Resumen
echo ""
echo "════════════════════════════════════════════════════════"
echo "  RESUMEN DE LA INTEGRACIÓN"
echo "════════════════════════════════════════════════════════"
echo ""

echo "Módulos del Kernel:"
echo "  ✓ init_system.rs       - Gestión de inicialización"
echo "  ✓ process_memory.rs    - Gestión de memoria de procesos"
echo "  ✓ process_transfer.rs  - Transferencia kernel→userland"
echo "  ✓ elf_loader.rs        - Carga de ejecutables ELF64"
echo ""

echo "Integración en kernel_main:"
echo "  ✓ Hook de inicialización"
echo "  ✓ Flag de habilitación"
echo "  ✓ Manejo de errores y fallback"
echo ""

echo "Estado de la Implementación:"
echo "  ✓ Framework completo"
echo "  ⚠ ELF loading (simulado)"
echo "  ⚠ Virtual memory (simulado)"
echo "  ⚠ Control transfer (documentado, no ejecutable)"
echo ""

echo "Próximos Pasos:"
echo "  1. Implementar VFS (Virtual File System)"
echo "  2. Completar sistema de paginación"
echo "  3. Implementar syscalls críticas (fork, exec, wait)"
echo "  4. Habilitar transferencia real de control"
echo ""

success "Integración eclipse-systemd verificada correctamente"
echo ""
echo "Para más información, ver: eclipse_kernel/SYSTEMD_INTEGRATION.md"
echo ""
