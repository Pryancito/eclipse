#!/bin/bash

# Script de construcción para el instalador de Eclipse OS
# Este script compila el instalador y crea una ISO de instalación

echo "🌙 Construyendo instalador de Eclipse OS..."
echo "==========================================="

# Configurar variables de entorno
export RUSTFLAGS="-C target-cpu=native -C opt-level=2"
export CARGO_TARGET_DIR="target_installer"

# Crear directorio de compilación
mkdir -p $CARGO_TARGET_DIR

# 1. Compilar el kernel Eclipse
echo ""
echo "🔧 Paso 1: Compilando kernel Eclipse..."
cargo build --release --target x86_64-unknown-none --manifest-path eclipse_kernel/Cargo.toml

if [ $? -ne 0 ]; then
    echo "❌ Error compilando el kernel Eclipse"
    exit 1
fi

echo "✅ Kernel Eclipse compilado exitosamente"

# 2. Compilar el bootloader UEFI
echo ""
echo "🔧 Paso 2: Compilando bootloader UEFI..."
cd bootloader-uefi
./build.sh
if [ $? -ne 0 ]; then
    echo "❌ Error compilando bootloader UEFI"
    exit 1
fi
cd ..

echo "✅ Bootloader UEFI compilado exitosamente"

# 3. Compilar el instalador
echo ""
echo "🔧 Paso 3: Compilando instalador..."
cd installer
cargo build --release
if [ $? -ne 0 ]; then
    echo "❌ Error compilando instalador"
    exit 1
fi
cd ..

echo "✅ Instalador compilado exitosamente"

# 4. Crear estructura de directorios para ISO de instalación
echo ""
echo "🔧 Paso 4: Creando estructura de instalación..."
mkdir -p /tmp/eclipse-installer/{boot,efi/boot,installer}

# Copiar archivos necesarios
cp target_hardware/x86_64-unknown-none/release/eclipse_kernel /tmp/eclipse-installer/boot/
cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /tmp/eclipse-installer/efi/boot/BOOTX64.EFI
cp installer/target/release/eclipse-installer /tmp/eclipse-installer/installer/

# Crear script de instalación automática
cat > /tmp/eclipse-installer/install.sh << 'INSTALL_EOF'
#!/bin/bash

echo "🌙 Eclipse OS - Instalación Automática"
echo "======================================"
echo ""
echo "Este script instalará Eclipse OS en tu disco duro."
echo "⚠️  ADVERTENCIA: Esto borrará todos los datos del disco seleccionado."
echo ""

# Verificar permisos de root
if [ "$EUID" -ne 0 ]; then
    echo "❌ Error: Este script debe ejecutarse como root"
    echo "   Usa: sudo ./install.sh"
    exit 1
fi

# Mostrar discos disponibles
echo "💾 Discos disponibles:"
lsblk -d -o NAME,SIZE,MODEL,TYPE | grep disk
echo ""

# Solicitar disco de destino
read -p "Ingresa el nombre del disco donde instalar (ej: /dev/sda): " DISK

if [ ! -b "$DISK" ]; then
    echo "❌ Error: $DISK no es un dispositivo de bloque válido"
    exit 1
fi

# Confirmar instalación
echo ""
echo "⚠️  ADVERTENCIA: Esto borrará TODOS los datos en $DISK"
read -p "¿Estás seguro de que quieres continuar? (escribe 'SI' para confirmar): " CONFIRM

if [ "$CONFIRM" != "SI" ]; then
    echo "❌ Instalación cancelada"
    exit 1
fi

# Ejecutar instalador
echo ""
echo "🚀 Iniciando instalación..."
./installer/eclipse-installer --disk "$DISK" --auto-install

if [ $? -eq 0 ]; then
    echo ""
    echo "🎉 ¡Instalación completada exitosamente!"
    echo "🔄 Reinicia el sistema para usar Eclipse OS"
else
    echo ""
    echo "❌ Error durante la instalación"
    exit 1
fi
INSTALL_EOF

chmod +x /tmp/eclipse-installer/install.sh

# Crear archivo README
cat > /tmp/eclipse-installer/README.txt << 'README_EOF'
🌙 Eclipse OS - Instalación
===========================

Versión: 1.0
Arquitectura: x86_64
Tipo: ISO de instalación

Instrucciones de instalación:
1. Graba esta ISO en un USB o DVD
2. Arranca desde el USB/DVD
3. Ejecuta: sudo ./install.sh
4. Sigue las instrucciones en pantalla

Requisitos:
- Disco duro con al menos 1GB de espacio libre
- Sistema UEFI compatible
- Conexión a internet (opcional)

Características:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado
- Instalador automático
- Sistema de archivos optimizado

Desarrollado con ❤️ en Rust
README_EOF

# 5. Crear ISO de instalación
echo ""
echo "🔧 Paso 5: Creando ISO de instalación..."

xorriso -as mkisofs \
    -iso-level 3 \
    -full-iso9660-filenames \
    -volid "ECLIPSE_OS_INSTALLER" \
    -appid "Eclipse OS Installer v1.0" \
    -publisher "Eclipse OS Team" \
    -preparer "Eclipse OS Installer Builder" \
    -eltorito-boot efi/boot/bootx64.efi \
    -no-emul-boot \
    -boot-load-size 4 \
    -boot-info-table \
    -isohybrid-gpt-basdat \
    -output eclipse-os-installer.iso \
    /tmp/eclipse-installer/

if [ $? -ne 0 ]; then
    echo "❌ Error creando ISO de instalación"
    exit 1
fi

# 6. Aplicar isohybrid para compatibilidad UEFI
echo ""
echo "🔧 Paso 6: Aplicando isohybrid para compatibilidad UEFI..."
isohybrid --uefi eclipse-os-installer.iso

if [ $? -ne 0 ]; then
    echo "⚠️  Advertencia: isohybrid falló, pero la ISO puede funcionar"
fi

# 7. Limpiar archivos temporales
echo ""
echo "🔧 Paso 7: Limpiando archivos temporales..."
rm -rf /tmp/eclipse-installer

# 8. Mostrar resumen
echo ""
echo "🎉 ¡Construcción completada exitosamente!"
echo "========================================"
echo ""
echo "📋 Archivos generados:"
echo "  - eclipse-os-installer.iso (ISO de instalación)"
echo "  - installer/target/release/eclipse-installer (Instalador binario)"
echo "  - target_installer/ (Directorio de compilación)"
echo ""
echo "🚀 Para probar la ISO de instalación:"
echo "  qemu-system-x86_64 -cdrom eclipse-os-installer.iso -m 1G -bios /usr/share/qemu/OVMF.fd"
echo ""
echo "💾 Para grabar en USB:"
echo "  sudo dd if=eclipse-os-installer.iso of=/dev/sdX bs=4M status=progress"
echo ""
echo "🔍 Características de la ISO de instalación:"
echo "  - Instalador automático"
echo "  - Bootloader UEFI personalizado"
echo "  - Kernel Eclipse optimizado"
echo "  - Compatible con hardware real"
echo ""
echo "✨ ¡Listo para instalar Eclipse OS!"
