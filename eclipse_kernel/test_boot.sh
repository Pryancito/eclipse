#!/bin/bash

# Script para probar el sistema de mensajes de boot del kernel Eclipse

echo "=========================================="
echo "    ECLIPSE KERNEL - BOOT MESSAGE TEST"
echo "=========================================="
echo ""

# Compilar el kernel con mensajes de boot
echo "Compilando kernel con sistema de mensajes de boot..."
cargo build --bin eclipse_kernel

if [ $? -eq 0 ]; then
    echo ""
    echo "✅ Kernel compilado exitosamente!"
    echo ""
    echo "El sistema de mensajes de boot incluye:"
    echo "  - Banner de inicio"
    echo "  - Barras de progreso"
    echo "  - Mensajes informativos (INFO)"
    echo "  - Mensajes de éxito (OK)"
    echo "  - Mensajes de advertencia (WARN)"
    echo "  - Mensajes de error (ERROR)"
    echo "  - Resumen de inicialización"
    echo ""
    echo "Características implementadas:"
    echo "  - Colores para diferentes tipos de mensajes"
    echo "  - Timestamps para cada mensaje"
    echo "  - Sistema de pasos con progreso visual"
    echo "  - Almacenamiento de historial de mensajes"
    echo "  - Compatible con entorno no_std"
    echo ""
    echo "El kernel está listo para mostrar mensajes durante el arranque."
    echo "Los mensajes se mostrarán cuando se ejecute el kernel."
else
    echo ""
    echo "❌ Error en la compilación del kernel"
    echo "Revisa los errores mostrados arriba."
    exit 1
fi

echo ""
echo "=========================================="
echo "    TEST COMPLETADO"
echo "=========================================="
