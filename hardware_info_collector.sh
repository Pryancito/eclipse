#!/bin/bash

echo "🔍 Recopilador de Información de Hardware para Redox OS"
echo "======================================================"

echo ""
echo "📋 Información del Sistema:"
echo "   CPU: $(cat /proc/cpuinfo | grep "model name" | head -1 | cut -d: -f2 | xargs)"
echo "   Arquitectura: $(uname -m)"
echo "   Kernel: $(uname -r)"

echo ""
echo "🔧 Controladores PCI de Almacenamiento:"
lspci -nn | grep -E "(SATA|AHCI|NVMe|Storage|IDE)" | while read line; do
    echo "   $line"
done

echo ""
echo "💾 Dispositivos de Almacenamiento Detectados:"
lsblk -d -o NAME,SIZE,MODEL,TRAN | grep -v loop

echo ""
echo "🔌 Controladores PCI Detallados:"
echo ""
echo "=== AHCI/SATA Controllers ==="
lspci -nn | grep -i "sata\|ahci" | while read line; do
    bus_id=$(echo "$line" | cut -d' ' -f1)
    echo "Bus ID: $bus_id"
    lspci -v -s "$bus_id" 2>/dev/null | grep -E "(Vendor|Device|Subsystem|Flags|Memory|I/O)" | sed 's/^/  /'
    echo ""
done

echo "=== NVMe Controllers ==="
lspci -nn | grep -i "nvme\|non-volatile" | while read line; do
    bus_id=$(echo "$line" | cut -d' ' -f1)
    echo "Bus ID: $bus_id"
    lspci -v -s "$bus_id" 2>/dev/null | grep -E "(Vendor|Device|Subsystem|Flags|Memory)" | sed 's/^/  /'
    echo ""
done

echo "🔌 Información USB:"
lsusb | head -10

echo ""
echo "📊 Información de Memoria:"
echo "   RAM Total: $(free -h | grep "Mem:" | awk '{print $2}')"
echo "   Swap: $(free -h | grep "Swap:" | awk '{print $2}')"

echo ""
echo "🎯 IDs Específicos para Configuración Redox:"
echo ""
echo "=== AHCI Controllers ==="
lspci -nn | grep -i "sata\|ahci" | sed 's/.*\[\([0-9a-fA-F:]*\)\].*/  Vendor:Device = 0x\1/' | sed 's/:/ /'

echo ""
echo "=== NVMe Controllers ==="
lspci -nn | grep -i "nvme\|non-volatile" | sed 's/.*\[\([0-9a-fA-F:]*\)\].*/  Vendor:Device = 0x\1/' | sed 's/:/ /'

echo ""
echo "✅ Información recopilada. Guarda esta salida para referencia."
