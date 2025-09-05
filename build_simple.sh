#!/bin/bash

# Script de construcción simplificado para Eclipse OS
# Este script compila el kernel y lo prepara para pruebas

set -e

echo "🚀 Iniciando construcción simplificada de Eclipse OS..."

# Colores para output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Función para imprimir mensajes con colores
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

# Verificar que estamos en el directorio correcto
if [ ! -f "eclipse_kernel/Cargo.toml" ]; then
    print_error "No se encontró eclipse_kernel/Cargo.toml. Ejecuta desde el directorio raíz de Eclipse OS."
    exit 1
fi

# Limpiar compilaciones anteriores
print_status "Limpiando compilaciones anteriores..."
cargo clean --manifest-path eclipse_kernel/Cargo.toml

# Compilar el kernel
print_status "Compilando kernel Eclipse..."
cd eclipse_kernel
cargo build --release
if [ $? -eq 0 ]; then
    print_success "Kernel compilado exitosamente"
else
    print_error "Error al compilar el kernel"
    exit 1
fi
cd ..

# Crear directorio de distribución
print_status "Creando directorio de distribución..."
mkdir -p eclipse-os-dist

# Copiar el kernel compilado
print_status "Copiando kernel a la distribución..."
cp eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel eclipse-os-dist/

# Crear script de prueba QEMU directo
print_status "Creando script de prueba QEMU..."
cat > eclipse-os-dist/test_kernel.sh << 'EOF'
#!/bin/bash
echo "🧪 Iniciando Eclipse OS Kernel en QEMU..."
echo "Presiona Ctrl+Alt+G para liberar el mouse de QEMU"
echo "Presiona Ctrl+Alt+Q para salir de QEMU"
echo ""

# Ejecutar QEMU con el kernel directamente
qemu-system-x86_64 \
    -machine q35 \
    -cpu qemu64 \
    -m 512M \
    -kernel eclipse_kernel \
    -netdev user,id=net0 \
    -device e1000,netdev=net0 \
    -vga std \
    -serial stdio \
    -monitor stdio \
    -name "Eclipse OS Kernel" \
    -nographic
EOF

chmod +x eclipse-os-dist/test_kernel.sh

# Crear README para la distribución
print_status "Creando documentación..."
cat > eclipse-os-dist/README.md << 'EOF'
# Eclipse OS Kernel

Este es el kernel Eclipse OS con todos los módulos integrados.

## Características

- ✅ Kernel completo con todos los módulos
- ✅ Sistema de archivos avanzado
- ✅ Interfaz gráfica con soporte NVIDIA
- ✅ Sistema de seguridad robusto
- ✅ Inteligencia artificial integrada
- ✅ Monitoreo del sistema
- ✅ Sistema de personalización
- ✅ Gestión de contenedores
- ✅ Sistema de plugins
- ✅ Gestión de energía y térmica
- ✅ Sistema de privacidad

## Uso

### Compilar desde cero
```bash
./build_simple.sh
```

### Probar en QEMU
```bash
cd eclipse-os-dist
./test_kernel.sh
```

### Compilar solo el kernel
```bash
cd eclipse_kernel
cargo build --release
```

## Estructura

- `eclipse_kernel` - Kernel compilado
- `test_kernel.sh` - Script para probar en QEMU
- `README.md` - Documentación

## Requisitos

- Rust toolchain
- QEMU

## Notas

- El kernel está compilado en modo release para mejor rendimiento
- Se incluyen todas las advertencias del compilador pero no afectan la funcionalidad
- El sistema está diseñado para ser modular y extensible
- Para pruebas, se ejecuta directamente el kernel sin bootloader
EOF

print_success "Distribución Eclipse OS creada exitosamente!"
print_status "Archivos creados:"
echo "  📁 eclipse-os-dist/"
echo "    ├── eclipse_kernel (kernel compilado)"
echo "    ├── test_kernel.sh (script de prueba)"
echo "    └── README.md (documentación)"

print_status "Para probar el sistema:"
echo "  cd eclipse-os-dist && ./test_kernel.sh"

print_success "¡Construcción simplificada finalizada! 🎉"
