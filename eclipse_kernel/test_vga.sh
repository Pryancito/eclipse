#!/bin/bash

# Script para probar el kernel Eclipse con mensajes VGA

echo "=========================================="
echo "    ECLIPSE KERNEL - VGA TEST"
echo "=========================================="
echo ""

# Verificar que estamos en el directorio correcto
if [ ! -f "Cargo.toml" ]; then
    echo "❌ Error: No se encontró Cargo.toml"
    echo "   Asegúrate de estar en el directorio eclipse_kernel/"
    exit 1
fi

# Compilar el kernel
echo "Compilando kernel Eclipse..."
cargo build --target x86_64-unknown-none --release

if [ $? -eq 0 ]; then
    echo "✅ Kernel compilado exitosamente!"
    echo ""
    echo "Probando kernel en QEMU con VGA..."
    echo "Los mensajes deberían aparecer en la pantalla VGA"
    echo ""
    echo "Presiona Ctrl+Alt+G para liberar el mouse de QEMU"
    echo "Presiona Ctrl+Alt+Q para salir de QEMU"
    echo ""
    
    # Crear un disco virtual con GRUB
    echo "Creando disco virtual con GRUB..."
    
    # Crear directorio temporal
    mkdir -p /tmp/eclipse-test/boot/grub
    
    # Copiar el kernel compilado
    cp target/x86_64-unknown-none/release/eclipse_kernel /tmp/eclipse-test/boot/eclipse_kernel
    
    # Crear archivo de configuración GRUB
    cat > /tmp/eclipse-test/boot/grub/grub.cfg << 'EOF'
set timeout=5
set default=0

menuentry "Eclipse OS Kernel" {
    multiboot /boot/eclipse_kernel
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
        
        # Ejecutar QEMU con VGA habilitada
        qemu-system-x86_64 \
            -cdrom /tmp/eclipse-test/eclipse.iso \
            -m 1G \
            -vga std \
            -name "Eclipse OS Kernel Test" \
            -no-reboot
        
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
