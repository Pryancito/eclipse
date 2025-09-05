#!/bin/bash

# Script de demostración de instalación de Eclipse OS
# Este script muestra cómo instalar Eclipse OS paso a paso

echo "🌙 Demostración de Instalación de Eclipse OS"
echo "============================================"
echo ""

# Verificar que estamos en el directorio correcto
if [ ! -f "eclipse_kernel/Cargo.toml" ]; then
    echo "❌ Error: No se encontró el proyecto Eclipse OS"
    echo "   Asegúrate de estar en el directorio raíz del proyecto"
    exit 1
fi

echo "📋 Pasos para instalar Eclipse OS:"
echo "=================================="
echo ""
echo "1. 🔧 Compilar el kernel y bootloader"
echo "2. 💾 Seleccionar disco de destino"
echo "3. 🗂️  Crear particiones"
echo "4. 📦 Instalar bootloader UEFI"
echo "5. 🎉 ¡Listo para usar!"
echo ""

# Paso 1: Compilar
echo "🔧 Paso 1: Compilando kernel y bootloader..."
echo "============================================="

# Compilar kernel
echo "   Compilando kernel Eclipse..."
cargo build --release --target x86_64-unknown-none --manifest-path eclipse_kernel/Cargo.toml

if [ $? -ne 0 ]; then
    echo "❌ Error compilando kernel"
    exit 1
fi

# Compilar bootloader
echo "   Compilando bootloader UEFI..."
cd bootloader-uefi
./build.sh
if [ $? -ne 0 ]; then
    echo "❌ Error compilando bootloader"
    exit 1
fi
cd ..

echo "✅ Compilación completada"
echo ""

# Paso 2: Mostrar discos disponibles
echo "💾 Paso 2: Discos disponibles para instalación"
echo "=============================================="
echo ""

echo "Discos detectados:"
lsblk -d -o NAME,SIZE,MODEL,TYPE | grep disk | nl
echo ""

# Paso 3: Mostrar opciones de instalación
echo "🚀 Paso 3: Opciones de instalación"
echo "=================================="
echo ""
echo "Tienes varias opciones para instalar Eclipse OS:"
echo ""
echo "1. 📱 Instalación directa (recomendado):"
echo "   sudo ./install_eclipse_os.sh /dev/sdX"
echo ""
echo "2. 🖥️  Instalación con ISO:"
echo "   ./build_installer.sh"
echo "   # Luego graba la ISO en un USB"
echo ""
echo "3. 🔧 Instalación manual:"
echo "   # Usa el instalador interactivo"
echo "   cd installer && cargo run"
echo ""

# Paso 4: Mostrar advertencias
echo "⚠️  Paso 4: Advertencias importantes"
echo "==================================="
echo ""
echo "ANTES DE INSTALAR:"
echo "  - Haz una copia de seguridad de tus datos importantes"
echo "  - Asegúrate de seleccionar el disco correcto"
echo "  - La instalación borrará TODOS los datos del disco"
echo "  - Verifica que tu sistema soporte UEFI"
echo ""

# Paso 5: Mostrar requisitos del sistema
echo "📋 Paso 5: Requisitos del sistema"
echo "================================="
echo ""
echo "Mínimos:"
echo "  - Disco duro con al menos 1GB de espacio libre"
echo "  - Sistema UEFI compatible"
echo "  - 512MB de RAM"
echo ""
echo "Recomendados:"
echo "  - Disco duro con 2GB+ de espacio libre"
echo "  - 1GB+ de RAM"
echo "  - Procesador x86_64"
echo ""

# Paso 6: Mostrar comandos de ejemplo
echo "💡 Paso 6: Comandos de ejemplo"
echo "=============================="
echo ""
echo "Para instalar en /dev/sda (cambia por tu disco):"
echo "  sudo ./install_eclipse_os.sh /dev/sda"
echo ""
echo "Para instalación automática (sin confirmación):"
echo "  sudo ./install_eclipse_os.sh --auto /dev/sda"
echo ""
echo "Para ver ayuda:"
echo "  ./install_eclipse_os.sh --help"
echo ""

# Paso 7: Mostrar verificación post-instalación
echo "✅ Paso 7: Verificación post-instalación"
echo "========================================"
echo ""
echo "Después de instalar:"
echo "  1. Reinicia el sistema"
echo "  2. Entra a la configuración UEFI/BIOS"
echo "  3. Selecciona el disco como dispositivo de arranque"
echo "  4. Guarda y reinicia"
echo "  5. Eclipse OS debería arrancar automáticamente"
echo ""

# Paso 8: Mostrar resolución de problemas
echo "🔧 Paso 8: Resolución de problemas"
echo "=================================="
echo ""
echo "Si Eclipse OS no arranca:"
echo "  - Verifica que UEFI esté habilitado"
echo "  - Asegúrate de que el disco esté en la lista de arranque"
echo "  - Verifica que las particiones se crearon correctamente"
echo "  - Usa 'lsblk' para verificar la estructura del disco"
echo ""

echo "🎉 ¡Demostración completada!"
echo "============================"
echo ""
echo "¿Estás listo para instalar Eclipse OS?"
echo "Ejecuta: sudo ./install_eclipse_os.sh /dev/sdX"
echo "(reemplaza /dev/sdX con tu disco de destino)"
echo ""
