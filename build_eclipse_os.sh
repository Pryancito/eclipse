#!/bin/bash

# Script de compilación completo para Eclipse OS
# Integra bootloader UEFI + Kernel Eclipse + Sistema de archivos

set -e

echo "🌙 Eclipse OS - Sistema de Compilación Completo"
echo "=============================================="
echo ""

# Colores para output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
PURPLE='\033[0;35m'
CYAN='\033[0;36m'
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

# Verificar dependencias
print_status "Verificando dependencias..."

# Verificar Rust
if ! command -v rustc &> /dev/null; then
    print_error "Rust no está instalado. Instala Rust desde https://rustup.rs/"
    exit 1
fi

# Verificar cargo
if ! command -v cargo &> /dev/null; then
    print_error "Cargo no está instalado. Instala Rust desde https://rustup.rs/"
    exit 1
fi

# Verificar target x86_64-unknown-uefi
if ! rustup target list --installed | grep -q "x86_64-unknown-uefi"; then
    print_status "Instalando target x86_64-unknown-uefi..."
    rustup target add x86_64-unknown-uefi
fi

# Verificar herramientas de compilación
if ! command -v nasm &> /dev/null; then
    print_warning "NASM no está instalado. Instalando..."
    sudo apt-get update && sudo apt-get install -y nasm
fi

if ! command -v objcopy &> /dev/null; then
    print_warning "binutils no está instalado. Instalando..."
    sudo apt-get install -y binutils
fi

if ! command -v xorriso &> /dev/null; then
    print_warning "xorriso no está instalado. Instalando..."
    sudo apt-get install -y xorriso
fi

if ! command -v qemu-system-x86_64 &> /dev/null; then
    print_warning "QEMU no está instalado. Instalando..."
    sudo apt-get install -y qemu-system-x86_64
fi

print_success "Dependencias verificadas"

# Crear directorio de salida
OUTPUT_DIR="eclipse-os-build"
mkdir -p "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR/efi/boot"
mkdir -p "$OUTPUT_DIR/boot"

print_status "Compilando bootloader UEFI..."

# Compilar bootloader UEFI
cd bootloader-uefi
cargo build --release --target x86_64-unknown-uefi
if [ $? -ne 0 ]; then
    print_error "Error compilando bootloader UEFI"
    exit 1
fi

# Copiar bootloader compilado
cp target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi ../$OUTPUT_DIR/efi/boot/bootx64.efi
print_success "Bootloader UEFI compilado exitosamente"

cd ..

print_status "Compilando kernel Eclipse..."

# Compilar kernel Eclipse
cd eclipse_kernel
cargo build --release --target x86_64-unknown-none
if [ $? -ne 0 ]; then
    print_error "Error compilando kernel Eclipse"
    exit 1
fi

# Crear kernel binario
objcopy --strip-all -O binary target/x86_64-unknown-none/release/eclipse_kernel ../$OUTPUT_DIR/boot/eclipse_kernel.bin
print_success "Kernel Eclipse compilado exitosamente"

cd ..

print_status "Creando sistema de archivos..."

# Crear archivo de configuración GRUB
cat > "$OUTPUT_DIR/boot/grub.cfg" << 'EOF'
set timeout=5
set default=0

menuentry "Eclipse OS" {
    multiboot2 /boot/eclipse_kernel.bin
    boot
}

menuentry "Eclipse OS (Debug)" {
    multiboot2 /boot/eclipse_kernel.bin debug
    boot
}
EOF

print_success "Sistema de archivos creado"

print_status "Creando imagen ISO..."

# Crear imagen ISO híbrida (UEFI + BIOS)
xorriso -as mkisofs \
    -R -J -c boot/boot.catalog \
    -b boot/grub/stage2_eltorito \
    -no-emul-boot -boot-load-size 4 -boot-info-table \
    -eltorito-alt-boot -e efi/boot/bootx64.efi -no-emul-boot \
    -isohybrid-gpt-basdat \
    -o eclipse-os.iso \
    "$OUTPUT_DIR"

if [ $? -eq 0 ]; then
    print_success "Imagen ISO creada exitosamente: eclipse-os.iso"
else
    print_error "Error creando imagen ISO"
    exit 1
fi

print_status "Creando imagen de disco UEFI..."

# Crear imagen de disco UEFI
dd if=/dev/zero of=eclipse-os-uefi.img bs=1M count=64
mkfs.fat -F 32 eclipse-os-uefi.img

# Montar imagen
sudo mkdir -p /mnt/eclipse-temp
sudo mount eclipse-os-uefi.img /mnt/eclipse-temp

# Copiar archivos
sudo mkdir -p /mnt/eclipse-temp/EFI/BOOT
sudo cp "$OUTPUT_DIR/efi/boot/bootx64.efi" /mnt/eclipse-temp/EFI/BOOT/
sudo cp "$OUTPUT_DIR/boot/eclipse_kernel.bin" /mnt/eclipse-temp/

# Desmontar
sudo umount /mnt/eclipse-temp
sudo rmdir /mnt/eclipse-temp

print_success "Imagen de disco UEFI creada: eclipse-os-uefi.img"

print_status "Creando script de prueba QEMU..."

# Crear script de prueba
cat > test_eclipse_os.sh << 'EOF'
#!/bin/bash

echo "🚀 Iniciando Eclipse OS en QEMU..."
echo ""

# Probar imagen ISO
echo "📀 Probando imagen ISO (BIOS + UEFI)..."
qemu-system-x86_64 \
    -cdrom eclipse-os.iso \
    -m 512M \
    -netdev user,id=net0 \
    -device rtl8139,netdev=net0 \
    -serial stdio \
    -monitor stdio \
    -name "Eclipse OS Test"

echo ""
echo "💾 Probando imagen de disco UEFI..."
qemu-system-x86_64 \
    -drive file=eclipse-os-uefi.img,format=raw \
    -m 512M \
    -netdev user,id=net0 \
    -device rtl8139,netdev=net0 \
    -serial stdio \
    -monitor stdio \
    -name "Eclipse OS UEFI Test"
EOF

chmod +x test_eclipse_os.sh

print_success "Script de prueba creado: test_eclipse_os.sh"

# Mostrar resumen
echo ""
echo "🎉 Eclipse OS compilado exitosamente!"
echo "====================================="
echo ""
echo "📁 Archivos generados:"
echo "  • eclipse-os.iso - Imagen ISO híbrida (BIOS + UEFI)"
echo "  • eclipse-os-uefi.img - Imagen de disco UEFI"
echo "  • test_eclipse_os.sh - Script de prueba QEMU"
echo ""
echo "🚀 Para probar Eclipse OS:"
echo "  ./test_eclipse_os.sh"
echo ""
echo "📊 Estadísticas de compilación:"
echo "  • Bootloader UEFI: ✅ Compilado"
echo "  • Kernel Eclipse: ✅ Compilado"
echo "  • Sistema de archivos: ✅ Creado"
echo "  • Imagen ISO: ✅ Creada"
echo "  • Imagen UEFI: ✅ Creada"
echo ""
print_success "¡Eclipse OS está listo para usar!"
