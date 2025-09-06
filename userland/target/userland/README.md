# Userland de Eclipse OS

Este directorio contiene las aplicaciones compiladas del userland de Eclipse OS.

## Aplicaciones disponibles:

- **eclipse_userland**: Binario principal del userland con todas las funcionalidades

## Uso:

Estas aplicaciones están diseñadas para ejecutarse sobre el kernel Eclipse OS
y se conectan al compositor Wayland para proporcionar una interfaz gráfica.

## Compilación:

Para compilar el userland, ejecuta:
```bash
./build.sh
```

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
