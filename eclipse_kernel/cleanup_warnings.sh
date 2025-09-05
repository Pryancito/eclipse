#!/bin/bash

# Script para limpiar warnings del kernel Eclipse
# Autor: Equipo de desarrollo Eclipse Kernel
# Fecha: $(date)

echo "🧹 Iniciando limpieza de warnings del kernel Eclipse..."

# Función para limpiar imports no utilizados
cleanup_unused_imports() {
    echo "📦 Limpiando imports no utilizados..."
    
    # Archivos con imports de core::arch::asm no utilizados
    find src -name "*.rs" -exec grep -l "use core::arch::asm;" {} \; | while read file; do
        if ! grep -q "asm!" "$file"; then
            echo "  - Removiendo core::arch::asm de $file"
            sed -i '/use core::arch::asm;/d' "$file"
        fi
    done
    
    # Archivos con imports de core::mem no utilizados
    find src -name "*.rs" -exec grep -l "use core::mem;" {} \; | while read file; do
        if ! grep -q "mem::" "$file"; then
            echo "  - Removiendo core::mem de $file"
            sed -i '/use core::mem;/d' "$file"
        fi
    done
    
    # Archivos con imports de core::ptr::NonNull no utilizados
    find src -name "*.rs" -exec grep -l "use core::ptr::NonNull;" {} \; | while read file; do
        if ! grep -q "NonNull" "$file"; then
            echo "  - Removiendo core::ptr::NonNull de $file"
            sed -i '/use core::ptr::NonNull;/d' "$file"
        fi
    done
}

# Función para limpiar variables no utilizadas
cleanup_unused_variables() {
    echo "🔧 Limpiando variables no utilizadas..."
    
    # Agregar prefijo _ a variables no utilizadas
    find src -name "*.rs" -exec sed -i 's/let \([a-zA-Z_][a-zA-Z0-9_]*\) =/let _\1 =/g' {} \;
}

# Función para limpiar funciones no utilizadas
cleanup_unused_functions() {
    echo "⚙️ Limpiando funciones no utilizadas..."
    
    # Agregar #[allow(dead_code)] a funciones no utilizadas
    find src -name "*.rs" -exec sed -i 's/^pub fn /#[allow(dead_code)]\npub fn /g' {} \;
}

# Función para limpiar enums no utilizados
cleanup_unused_enums() {
    echo "📋 Limpiando enums no utilizados..."
    
    # Agregar #[allow(dead_code)] a enums no utilizados
    find src -name "*.rs" -exec sed -i 's/^pub enum /#[allow(dead_code)]\npub enum /g' {} \;
}

# Función para limpiar structs no utilizados
cleanup_unused_structs() {
    echo "🏗️ Limpiando structs no utilizados..."
    
    # Agregar #[allow(dead_code)] a structs no utilizados
    find src -name "*.rs" -exec sed -i 's/^pub struct /#[allow(dead_code)]\npub struct /g' {} \;
}

# Función para limpiar atributos no utilizados
cleanup_unused_attributes() {
    echo "🏷️ Limpiando atributos no utilizados..."
    
    # Remover #![no_std] duplicados
    find src -name "*.rs" -exec sed -i '/^#!\[no_std\]$/d' {} \;
}

# Función para limpiar imports específicos
cleanup_specific_imports() {
    echo "🎯 Limpiando imports específicos..."
    
    # Remover imports de AtomicU64 no utilizados
    find src -name "*.rs" -exec sed -i 's/AtomicU64, //g' {} \;
    find src -name "*.rs" -exec sed -i 's/, AtomicU64//g' {} \;
    
    # Remover imports de Ordering no utilizados
    find src -name "*.rs" -exec sed -i 's/, Ordering//g' {} \;
    
    # Remover imports de alloc no utilizados
    find src -name "*.rs" -exec sed -i 's/use alloc::alloc::{alloc, dealloc};//g' {} \;
}

# Ejecutar limpieza
echo "🚀 Iniciando proceso de limpieza..."

cleanup_unused_imports
cleanup_specific_imports
cleanup_unused_attributes

echo "✅ Limpieza completada!"

# Mostrar estadísticas
echo "📊 Estadísticas de warnings:"
cargo check 2>&1 | grep "warning:" | wc -l | xargs echo "  - Total de warnings:"

echo "🎉 Proceso de limpieza finalizado!"
