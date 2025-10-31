#!/bin/bash

# Script de compilación para el userland de Eclipse OS
# Este script compila las aplicaciones del userland

echo "=== Compilando Userland de Eclipse OS ==="

# Verificar que estamos en el directorio correcto
if [ ! -f "Cargo.toml" ]; then
    echo "Error: No se encontró Cargo.toml en el directorio actual"
    exit 1
fi

# Crear directorio de salida
mkdir -p target/userland

# Compilar la biblioteca principal
echo "Compilando biblioteca del userland..."
cargo build --lib --release

if [ $? -ne 0 ]; then
    echo "Error: Falló la compilación de la biblioteca"
    exit 1
fi

# Compilar el binario principal
echo "Compilando binario principal del userland..."
cargo build --bin eclipse_userland --release

if [ $? -ne 0 ]; then
    echo "Error: Falló la compilación del binario principal"
    exit 1
fi

# Copiar binarios al directorio de salida
echo "Copiando binarios al directorio de salida..."
mkdir -p target/userland/
cp target/release/eclipse_userland target/userland/

# Crear archivo de información
echo "Creando archivo de información..."
cat > target/userland/README.md << EOF
# Userland de Eclipse OS

Este directorio contiene las aplicaciones compiladas del userland de Eclipse OS.

## Aplicaciones disponibles:

- **eclipse_userland**: Binario principal del userland con todas las funcionalidades

## Uso:

Estas aplicaciones están diseñadas para ejecutarse sobre el kernel Eclipse OS
y se conectan al compositor Wayland para proporcionar una interfaz gráfica.

## Compilación:

Para compilar el userland, ejecuta:
\`\`\`bash
./build.sh
\`\`\`

## Requisitos:

- Rust toolchain
- Kernel Eclipse OS
- Compositor Wayland (para aplicaciones gráficas)
- Sistema de archivos soportado (FAT32, NTFS)

## Características:

- Soporte para múltiples terminales
- Aplicaciones Wayland nativas
- Sistema de archivos integrado
- Módulos de IA avanzados
- Gestión de aplicaciones
EOF

echo "=== Compilación del Userland completada exitosamente ==="
echo "Binarios disponibles en: target/userland/"
echo ""
echo "Aplicaciones compiladas:"
ls -la target/userland/
