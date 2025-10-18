#!/bin/bash
# Script de Diagnóstico GPU para Redox OS
# Para usar en el sistema Linux antes de bootear Redox

echo "╔════════════════════════════════════════════════════════╗"
echo "║     Diagnóstico GPU - Sistema Multi-GPU Redox         ║"
echo "╚════════════════════════════════════════════════════════╝"
echo ""

echo "📊 Hardware Detectado:"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# Detectar GPUs con lspci
echo ""
echo "🔍 GPUs en el sistema:"
lspci -nn | grep -i vga

echo ""
echo "🔍 Detalles de GPUs NVIDIA:"
lspci -nn | grep -i nvidia

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "📦 Verificación de Drivers Compilados:"
echo ""

# Verificar que los drivers existen
DRIVERS_PATH="/home/moebius/redox/cookbook/recipes/core/drivers/target/x86_64-unknown-redox"

if [ -f "${DRIVERS_PATH}/build/target/release/nvidiad" ]; then
    echo "✅ nvidiad compilado: $(ls -lh ${DRIVERS_PATH}/build/target/release/nvidiad | awk '{print $5}')"
else
    echo "❌ nvidiad NO compilado"
fi

if [ -f "${DRIVERS_PATH}/build/target/release/amdd" ]; then
    echo "✅ amdd compilado: $(ls -lh ${DRIVERS_PATH}/build/target/release/amdd | awk '{print $5}')"
else
    echo "❌ amdd NO compilado"
fi

if [ -f "${DRIVERS_PATH}/build/target/release/inteld" ]; then
    echo "✅ inteld compilado: $(ls -lh ${DRIVERS_PATH}/build/target/release/inteld | awk '{print $5}')"
else
    echo "❌ inteld NO compilado"
fi

if [ -f "${DRIVERS_PATH}/build/target/release/multi-gpud" ]; then
    echo "✅ multi-gpud compilado: $(ls -lh ${DRIVERS_PATH}/build/target/release/multi-gpud | awk '{print $5}')"
else
    echo "❌ multi-gpud NO compilado"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "⚙️  Verificación de Configuraciones PCI:"
echo ""

# Verificar configs
CONFIG_PATH="/home/moebius/redox/cookbook/recipes/core/drivers/source/graphics"

for driver in nvidiad amdd inteld; do
    if [ -f "${CONFIG_PATH}/${driver}/config.toml" ]; then
        echo "✅ ${CONFIG_PATH}/${driver}/config.toml existe"
    else
        echo "❌ ${CONFIG_PATH}/${driver}/config.toml NO EXISTE"
    fi
done

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "🔬 Device IDs de tus GPUs:"
echo ""

# Extraer device IDs
lspci -nn | grep -i nvidia | while read -r line; do
    device_id=$(echo "$line" | grep -oP '\[10de:\K[0-9a-f]{4}')
    name=$(echo "$line" | sed 's/.*NVIDIA Corporation //' | sed 's/ \[.*//')
    echo "  Device ID: 0x${device_id} - ${name}"
done

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "💡 Recomendaciones:"
echo ""

# Verificar si los device IDs están en el código
RTX2060_IDS=("1f47" "1f08" "1f15" "1f42")
FOUND_IDS=$(lspci -nn | grep -i nvidia | grep -oP '\[10de:\K[0-9a-f]{4}')

for id in $FOUND_IDS; do
    echo "🔍 Verificando device ID 0x${id}..."
    # Por ahora solo mostramos
done

echo ""
echo "📝 Próximos pasos:"
echo "  1. Compilar drivers: cd ~/redox/cookbook && make"
echo "  2. Si falla, revisar errores de compilación"
echo "  3. Instalar en disco/USB"
echo "  4. Bootear y ver logs con: dmesg | grep nvidia"
echo ""
echo "╚════════════════════════════════════════════════════════╝"

