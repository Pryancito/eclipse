#!/bin/bash
# Script para verificar el estado de compilación de todos los componentes de Eclipse OS

set -e

# Colores para output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo "╔══════════════════════════════════════════════════════════════════════╗"
echo "║          ECLIPSE OS - VERIFICACIÓN DE ESTADO DE BUILD                ║"
echo "╚══════════════════════════════════════════════════════════════════════╝"
echo ""

# Función para imprimir mensajes
print_status() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

print_success() {
    echo -e "${GREEN}[✓]${NC} $1"
}

print_error() {
    echo -e "${RED}[✗]${NC} $1"
}

print_checking() {
    echo -e "${YELLOW}[...]${NC} Verificando $1..."
}

# Contadores
TOTAL=0
SUCCESS=0
FAILED=0

# Función para verificar un componente
check_component() {
    local name=$1
    local dir=$2
    local extra_args=$3
    
    TOTAL=$((TOTAL + 1))
    print_checking "$name"
    
    if [ -d "$dir" ]; then
        cd "$dir"
        if cargo check $extra_args &>/dev/null; then
            print_success "$name - compilación correcta"
            SUCCESS=$((SUCCESS + 1))
        else
            print_error "$name - errores de compilación"
            FAILED=$((FAILED + 1))
        fi
        cd - > /dev/null
    else
        print_error "$name - directorio no encontrado"
        FAILED=$((FAILED + 1))
    fi
}

# Verificar componentes principales
echo "═══════════════════════════════════════════════════════════════════════"
echo "  Verificando componentes principales"
echo "═══════════════════════════════════════════════════════════════════════"

check_component "EclipseFS Lib (std)" "eclipsefs-lib" "--features std"
check_component "EclipseFS Lib (no_std)" "eclipsefs-lib" "--no-default-features"
check_component "mkfs-eclipsefs" "mkfs-eclipsefs" ""
check_component "eclipsefs-cli" "eclipsefs-cli" ""
check_component "eclipsefs-fuse" "eclipsefs-fuse" ""

echo ""
echo "═══════════════════════════════════════════════════════════════════════"
echo "  Verificando userland"
echo "═══════════════════════════════════════════════════════════════════════"

check_component "Userland" "userland" ""
check_component "Module Loader" "userland/module_loader" ""

echo ""
echo "═══════════════════════════════════════════════════════════════════════"
echo "  Verificando aplicaciones"
echo "═══════════════════════════════════════════════════════════════════════"

if [ -d "eclipse-apps" ]; then
    for app_dir in eclipse-apps/*/; do
        if [ -f "${app_dir}Cargo.toml" ]; then
            app_name=$(basename "$app_dir")
            check_component "App: $app_name" "$app_dir" ""
        fi
    done
fi

echo ""
echo "═══════════════════════════════════════════════════════════════════════"
echo "  RESUMEN"
echo "═══════════════════════════════════════════════════════════════════════"
echo -e "Total de componentes: ${BLUE}$TOTAL${NC}"
echo -e "Compilación exitosa:  ${GREEN}$SUCCESS${NC}"
echo -e "Errores de compilación: ${RED}$FAILED${NC}"

if [ $FAILED -eq 0 ]; then
    echo ""
    print_success "¡Todos los componentes compilaron correctamente!"
    echo ""
    exit 0
else
    echo ""
    print_error "Algunos componentes tienen errores de compilación"
    echo ""
    exit 1
fi
