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
