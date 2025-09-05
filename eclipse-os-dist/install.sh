#!/bin/bash

# Script de instalación para Eclipse OS
# Instala el sistema en un USB UEFI

if [ "$EUID" -ne 0 ]; then
    echo "Por favor ejecuta como root: sudo ./install.sh"
    exit 1
fi

echo "=========================================="
echo "    ECLIPSE OS - INSTALADOR"
echo "=========================================="
echo ""

# Listar dispositivos USB disponibles
echo "Dispositivos USB disponibles:"
lsblk | grep -E "sd[a-z]"
echo ""

read -p "Ingresa el dispositivo USB (ej: /dev/sdb): " USB_DEVICE

if [ ! -b "$USB_DEVICE" ]; then
    echo "Error: $USB_DEVICE no es un dispositivo válido"
    exit 1
fi

echo ""
echo "⚠️  ADVERTENCIA: Esto formateará $USB_DEVICE"
read -p "¿Continuar? (y/N): " CONFIRM

if [ "$CONFIRM" != "y" ] && [ "$CONFIRM" != "Y" ]; then
    echo "Instalación cancelada"
    exit 0
fi

# Formatear USB
echo "Formateando USB..."
mkfs.fat -F32 "$USB_DEVICE"1

# Montar USB
MOUNT_POINT="/mnt/eclipse-usb"
mkdir -p "$MOUNT_POINT"
mount "$USB_DEVICE"1 "$MOUNT_POINT"

# Copiar archivos
echo "Copiando archivos del sistema..."
cp -r EFI "$MOUNT_POINT/"
cp -r boot "$MOUNT_POINT/"
cp vmlinuz-eclipse "$MOUNT_POINT/"

# Desmontar
umount "$MOUNT_POINT"

echo ""
echo "✅ Eclipse OS instalado exitosamente en $USB_DEVICE"
echo "   El USB está listo para arrancar en modo UEFI"
