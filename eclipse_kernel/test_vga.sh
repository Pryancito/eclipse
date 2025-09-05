#!/bin/bash

# Script para probar el kernel Eclipse con mensajes VGA

echo "=========================================="
echo "    ECLIPSE KERNEL - VGA TEST"
echo "=========================================="
echo ""

# Compilar el kernel
echo "Compilando kernel Eclipse..."
cargo build --target x86_64-unknown-none --release

if [ $? -eq 0 ]; then
    echo "✅ Kernel compilado exitosamente!"
    echo ""
    echo "Probando kernel en QEMU con VGA..."
    echo "Los mensajes deberían aparecer en la pantalla VGA"
    echo ""
    echo "Presiona Ctrl+Alt+G para salir de QEMU"
    echo ""
    
    # Crear un disco virtual con GRUB
    echo "Creando disco virtual con GRUB..."
    
    # Crear directorio temporal
    mkdir -p /tmp/eclipse-test
    
    # Crear archivo de configuración GRUB
    cat > /tmp/eclipse-test/grub.cfg << 'EOF'
menuentry "Eclipse Kernel" {
    multiboot /eclipse_kernel
    boot
}
EOF
    
    # Crear imagen de disco
    grub-mkrescue -o /tmp/eclipse-test/eclipse.iso /tmp/eclipse-test/
    
    if [ $? -eq 0 ]; then
        echo "✅ Imagen GRUB creada exitosamente!"
        echo ""
        echo "Ejecutando QEMU..."
        echo "Los mensajes VGA deberían aparecer en la pantalla"
        echo ""
        
        # Ejecutar QEMU
        qemu-system-x86_64 -cdrom /tmp/eclipse-test/eclipse.iso -m 512M -nographic -no-reboot
        
        # Limpiar archivos temporales
        rm -rf /tmp/eclipse-test
    else
        echo "❌ Error creando imagen GRUB"
        echo "Instala grub-mkrescue: sudo apt install grub-pc-bin"
        exit 1
    fi
else
    echo "❌ Error compilando el kernel"
    exit 1
fi

echo ""
echo "=========================================="
echo "    TEST COMPLETADO"
echo "=========================================="
