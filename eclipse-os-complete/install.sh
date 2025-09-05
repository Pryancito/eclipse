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
