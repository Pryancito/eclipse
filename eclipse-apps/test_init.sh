#!/bin/bash
echo "Probando Eclipse Init System..."
cd init
if cargo check; then
    echo "✓ Init compila correctamente"
else
    echo "✗ Error al compilar init"
    exit 1
fi
if cargo build --release; then
    echo "✓ Build release exitoso"
else
    echo "✗ Error en build release"
    exit 1
fi
if [ -f "target/release/eclipse-init" ]; then
    echo "✓ Ejecutable creado correctamente"
    ls -la target/release/eclipse-init
else
    echo "✗ Ejecutable no encontrado"
    exit 1
fi
echo "✅ Todas las pruebas del Init System completadas"
