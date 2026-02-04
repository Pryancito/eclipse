# Eclipse OS - Sistema Operativo en Rust

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Build Status](https://img.shields.io/badge/build-passing-brightgreen.svg)]()
[![Platform](https://img.shields.io/badge/platform-x86__64-lightgrey.svg)]()

Eclipse OS es un sistema operativo moderno escrito en Rust, diseñado para ser eficiente, seguro y fácil de usar. Combina un kernel híbrido con un sistema de userland robusto y un sistema de display avanzado usando DRM (Direct Rendering Manager).

## Características Principales

### 🚀 Microkernel Moderno
- **Arquitectura x86_64**: Soporte completo para procesadores de 64 bits
- **Arquitectura Microkernel**: Servicios del sistema ejecutándose en espacio de usuario
- **Sistema de Mensajes**: Comunicación IPC eficiente entre servidores
- **Multiboot2**: Compatible con bootloaders estándar
- **UEFI**: Soporte nativo para firmware UEFI moderno
- **Gestión de memoria**: Sistema de memoria avanzado con paginación
- **Interrupciones**: Manejo completo de interrupciones del sistema
- **Servidores del Sistema**: FileSystem, Graphics, Network, Input, Audio, AI, Security

### 🖥️ Sistema de Display Avanzado
- **DRM (Direct Rendering Manager)**: Control total de la pantalla en userland
- **VGA Text Mode**: Modo de texto tradicional para compatibilidad
- **NVIDIA GPU Support**: Drivers nativos para GPUs modernas (Turing+) mediante open-gpu-kernel-modules
- **Aceleración por hardware**: Rendimiento optimizado
- **Múltiples monitores**: Soporte para configuraciones multi-pantalla
- **Resoluciones modernas**: Soporte para resoluciones hasta 4K

### 🏗️ Userland Robusto
- **Módulos dinámicos**: Sistema de carga de módulos en tiempo de ejecución
- **IPC (Inter-Process Communication)**: Comunicación eficiente entre procesos
- **Sistema de archivos**: Soporte para FAT32, NTFS y sistemas personalizados
- **Aplicaciones**: Framework para desarrollo de aplicaciones nativas

### 🔧 Herramientas de Desarrollo
- **Scripts de construcción**: Automatización completa del proceso de build
- **Instalador**: Instalador automático para hardware real
- **QEMU**: Soporte completo para emulación
- **Debugging**: Herramientas de depuración integradas

## Arquitectura del Sistema

```
┌─────────────────────────────────────────────────────────────┐
│                    Eclipse OS v0.1.0                        │
├─────────────────────────────────────────────────────────────┤
│  Userland Applications                                      │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐          │
│  │   GUI Apps  │ │  Shell Apps │ │ System Apps │          │
│  └─────────────┘ └─────────────┘ └─────────────┘          │
├─────────────────────────────────────────────────────────────┤
│  System Services                                            │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐          │
│  │ DRM Display │ │ File System │ │   Network   │          │
│  └─────────────┘ └─────────────┘ └─────────────┘          │
├─────────────────────────────────────────────────────────────┤
│  Eclipse Kernel (Microkernel)                               │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐          │
│  │   Memory    │ │ Interrupts  │ │     IPC     │          │
│  │ Management  │ │   Handler   │ │  Messaging  │          │
│  └─────────────┘ └─────────────┘ └─────────────┘          │
├─────────────────────────────────────────────────────────────┤
│  Hardware Layer                                             │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐          │
│  │   CPU       │ │   Memory    │ │   I/O       │          │
│  │ (x86_64)    │ │   (RAM)     │ │  Devices    │          │
│  └─────────────┘ └─────────────┘ └─────────────┘          │
└─────────────────────────────────────────────────────────────┘
```

## 🚀 Inicio Rápido

### Para Nuevos Usuarios

Si eres nuevo en Eclipse OS, sigue estos pasos para comenzar rápidamente:

```bash
# 1. Clonar el repositorio
git clone https://github.com/Pryancito/eclipse.git
cd eclipse

# 2. Instalar dependencias del sistema (Ubuntu/Debian)
sudo apt-get update
sudo apt-get install build-essential qemu-system-x86 ovmf

# 3. Instalar Rust (si no lo tienes)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable
rustup toolchain install nightly
rustup target add x86_64-unknown-none --toolchain nightly
rustup target add x86_64-unknown-uefi --toolchain nightly

# 4. Compilar el proyecto
./build.sh

# 5. Probar en QEMU (opcional, si la compilación fue exitosa)
./qemu.sh
```

## Instalación y Uso

### Requisitos del Sistema

- **Procesador**: x86_64 (64-bit)
- **Memoria**: Mínimo 512MB RAM
- **Almacenamiento**: 1GB de espacio libre
- **Firmware**: UEFI o BIOS compatible
- **Rust**: 1.70+ para compilación

### Compilación Rápida

```bash
# Clonar el repositorio
git clone https://github.com/Pryancito/eclipse.git
cd eclipse

# Compilar todo el sistema
./build.sh

# El sistema se compilará y creará una distribución en eclipse-os-build/

cd install
cargo run
```

### Compilación con DRM

```bash
# Compilar con sistema DRM habilitado
./build.sh

# Ejecutar con DRM
cd eclipse-os-build/userland/bin
./start_drm.sh
```

### Pruebas en QEMU

```bash
# Probar en QEMU con VGA
qemu-system-x86_64 -kernel eclipse-os-build/boot/eclipse_kernel

# Probar en QEMU con UEFI
qemu-system-x86_64 -bios /usr/share/ovmf/OVMF.fd \
  -drive file=eclipse-os-build/efi/boot/bootx64.efi,format=raw
```

### Instalación en Hardware Real

```bash
# Crear imagen booteable
./create_bootable_iso.sh

# Grabar en USB
sudo dd if=eclipse-os-hardware.iso of=/dev/sdX bs=4M status=progress

# O usar el instalador
cd installer
cargo run --release
```

## Sistema de Display

### DRM (Direct Rendering Manager)

Eclipse OS incluye un sistema DRM completo para control avanzado de la pantalla:

```rust
use eclipse_userland::drm_display;

// Mostrar "Eclipse OS" centrado
drm_display::show_eclipse_os_centered()?;

// Mostrar pantalla negra
drm_display::show_black_screen()?;

// Mostrar mensaje de bienvenida completo
drm_display::show_eclipse_welcome()?;
```

### Sistema de Gráficos Multi-Fase

Eclipse OS implementa un sistema de inicialización de gráficos en 6 fases:

1. **Fase 1 - UEFI Bootloader**: Inicialización básica con GOP (Graphics Output Protocol)
2. **Fase 2 - UEFI Kernel Detection**: Detección de hardware gráfico disponible
3. **Fase 3 - DRM Kernel Runtime**: Control avanzado con Direct Rendering Manager
4. **Fase 4 - Advanced Multi-GPU**: Gestión de múltiples GPUs con drivers específicos (NVIDIA, AMD, Intel)
5. **Fase 5 - Window System**: Sistema de ventanas con compositor avanzado
6. **Fase 6 - Widget System**: Sistema de widgets para interfaces de usuario completas

Cada fase se construye sobre la anterior, proporcionando funcionalidades incrementales y permitiendo fallback a fases anteriores en caso de problemas.

### Características del DRM

- **Control total de la pantalla**: Acceso directo al hardware gráfico
- **Aceleración por hardware**: Rendimiento optimizado
- **Múltiples monitores**: Soporte para configuraciones complejas
- **Resoluciones modernas**: Hasta 4K y más
- **Sin limitaciones de VGA**: Libertad total en el diseño
- **Multi-GPU**: Soporte para múltiples tarjetas gráficas
- **Drivers específicos**: Optimizaciones para NVIDIA, AMD e Intel

### Configuración del Display

El sistema se configura automáticamente, pero puedes personalizar:

```ini
[display]
driver = "drm"              # Usar DRM como driver principal
fallback = "vga"            # Fallback a VGA si DRM falla
primary_device = "/dev/dri/card0"  # Dispositivo DRM principal
multi_gpu = true            # Habilitar soporte multi-GPU
compositor = true           # Habilitar compositor de ventanas
```

## 🎮 Soporte para GPUs NVIDIA

Eclipse OS incluye soporte nativo para GPUs NVIDIA modernas a través de la integración con los [NVIDIA open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules).

### GPUs Compatibles

El soporte está disponible para las siguientes arquitecturas:

- **Turing** (2018): RTX 2080 Ti, RTX 2080, RTX 2070, RTX 2060
- **Ampere** (2020): RTX 3090, RTX 3080, RTX 3070, RTX 3060
- **Ada Lovelace** (2022): RTX 4090, RTX 4080, RTX 4070, RTX 4060
- **Hopper** (2022): H100 y GPUs de datacenter

> **Nota:** Se requiere arquitectura Turing o superior. GPUs más antiguas (Pascal, Maxwell, etc.) no son compatibles con los módulos de código abierto de NVIDIA.

### Características

- ✅ **Detección Automática**: Identificación automática de GPUs NVIDIA via PCI
- ✅ **Identificación de Arquitectura**: Reconoce Turing, Ampere, Ada Lovelace y Hopper
- ✅ **Especificaciones de Hardware**: Reporta núcleos CUDA, RT cores, Tensor cores, SM count y VRAM
- ✅ **Multi-GPU**: Soporte para múltiples GPUs NVIDIA
- ✅ **Habilitación de Dispositivos**: Configura I/O, memoria y bus master
- ✅ **CUDA Runtime**: Runtime completo con gestión de contextos, memoria y lanzamiento de kernels
- ✅ **Ray Tracing**: Soporte para RT cores con aceleración de estructuras y pipelines
- ✅ **Display Output**: Salida directa via DisplayPort/HDMI con detección de conectores y configuración de modos
- ✅ **Power Management**: Control de estados de energía, frecuencias y monitoreo térmico
- ✅ **Video Encode/Decode**: NVENC/NVDEC para H.264, H.265, VP9, y AV1

### Mensajes de Arranque

Cuando se detecta una GPU NVIDIA, el kernel mostrará:

```
[NVIDIA] Initializing NVIDIA GPU subsystem...
[NVIDIA] Compatible with open-gpu-kernel-modules
[NVIDIA] Found 1 NVIDIA GPU(s)
[NVIDIA] GPU 0: GeForce RTX 3080
[NVIDIA]   Device ID: 0x2206
[NVIDIA]   Architecture: Ampere
[NVIDIA]   Memory: 10240 MB
[NVIDIA]   CUDA Cores: 8704
[NVIDIA]   SM Count: 68
[NVIDIA]   RT Cores: 68
[NVIDIA]   Tensor Cores: 272
[NVIDIA]   Advanced Features:
[NVIDIA]     ✓ CUDA Runtime
[NVIDIA]     ✓ Ray Tracing (RT Cores)
[NVIDIA]     ✓ DisplayPort/HDMI Output
[NVIDIA]     ✓ Power Management
[NVIDIA]     ✓ Video Encode (NVENC): 3 codecs
[NVIDIA]     ✓ Video Decode (NVDEC): 4 codecs
[NVIDIA]   ✓ Supported by open-gpu-kernel-modules
[NVIDIA]   Device enabled (I/O, Memory, Bus Master)
```

### Documentación Adicional

Para más información sobre el soporte NVIDIA, consulta [docs/NVIDIA_SUPPORT.md](docs/NVIDIA_SUPPORT.md).

## Estructura del Proyecto

```
eclipse-os/
├── eclipse_kernel/          # Kernel principal
│   ├── src/
│   │   ├── main.rs         # Punto de entrada del kernel
│   │   ├── vga_centered_display.rs  # Sistema VGA
│   │   ├── boot_messages.rs        # Mensajes de arranque
│   │   └── ...             # Otros módulos del kernel
│   └── Cargo.toml
├── userland/                # Sistema userland
│   ├── src/
│   │   ├── drm_display.rs  # Sistema DRM
│   │   ├── framebuffer_display.rs  # Sistema framebuffer
│   │   └── ...             # Otros módulos userland
│   ├── drm_display/        # Módulo DRM independiente
│   └── Cargo.toml
├── bootloader-uefi/         # Bootloader UEFI personalizado
├── installer/               # Instalador del sistema
├── eclipse-apps/            # Aplicaciones del sistema
├── build.sh                 # Script de construcción principal
└── README.md               # Este archivo
```

## Desarrollo

### Agregar Nuevas Características

1. **Módulos del Kernel**: Agregar en `eclipse_kernel/src/`
2. **Módulos Userland**: Agregar en `userland/src/`
3. **Aplicaciones**: Agregar en `eclipse-apps/`
4. **Drivers**: Agregar en `eclipse_kernel/src/drivers/`

### Compilación de Módulos Individuales

```bash
# Compilar solo el kernel
cd eclipse_kernel
cargo build --release

# Compilar solo el userland
cd userland
cargo build --release

# Compilar solo el sistema DRM
cd userland/drm_display
cargo build --release
```

### Testing

```bash
# Ejecutar tests del kernel
cd eclipse_kernel
cargo test

# Ejecutar tests del userland
cd userland
cargo test

# Ejecutar tests del DRM
cd userland/drm_display
cargo test
```

## Troubleshooting

### Pantalla Verde en QEMU

Si ves una pantalla verde en QEMU:

1. **Verificar configuración VGA**: El kernel usa VGA por defecto
2. **Probar en hardware real**: El problema puede ser específico de QEMU
3. **Usar DRM**: Cambiar al sistema DRM en userland
4. **Verificar logs**: Revisar mensajes de debug del kernel

### Problemas de DRM

Si el sistema DRM no funciona:

1. **Verificar permisos**: Usuario debe estar en grupo `video`
2. **Verificar dispositivo**: `/dev/dri/card0` debe existir
3. **Usar fallback VGA**: El sistema tiene fallback automático
4. **Revisar logs**: Verificar mensajes de error

### Problemas de Compilación

Si hay errores de compilación:

1. **Actualizar Rust**: `rustup update`
2. **Limpiar cache**: `cargo clean`
3. **Verificar dependencias**: Instalar dependencias del sistema
4. **Revisar logs**: Verificar mensajes de error específicos

## Contribuir

### Cómo Contribuir

1. **Fork** el repositorio
2. **Crear** una rama para tu feature
3. **Commit** tus cambios
4. **Push** a la rama
5. **Crear** un Pull Request

### Estándares de Código

- **Rust**: Seguir las convenciones de Rust
- **Documentación**: Documentar todas las funciones públicas
- **Tests**: Incluir tests para nuevas funcionalidades
- **Commits**: Usar mensajes de commit descriptivos

## Licencia

Eclipse OS está licenciado bajo la Licencia MIT. Ver `LICENSE` para más detalles.

## Estado del Proyecto

- **Versión**: 0.1.0
- **Estado**: En desarrollo activo
- **Kernel**: Funcional con VGA y UEFI
- **Userland**: Sistema DRM implementado
- **Gráficos**: Sistema de 6 fases con soporte Multi-GPU
- **Sistema de Ventanas**: En integración
- **Aplicaciones**: En desarrollo
- **Hardware**: Probado en QEMU y hardware real

## Roadmap

### Características Planificadas

- ✅ **Soporte Wayland**: Integración completa con libwayland y wlroots
- **Sistema de ventanas**: GUI completa con compositor avanzado
- **Multi-GPU avanzado**: Soporte completo para NVIDIA, AMD e Intel
- **Widgets modernos**: Sistema de widgets para interfaces avanzadas
- **Aplicaciones nativas**: Editor, navegador, etc.
- **Soporte de red**: TCP/IP completo
- **Sistema de paquetes**: Gestor de paquetes nativo
- **Multiusuario**: Soporte para múltiples usuarios

## Soporte Wayland

Eclipse OS ahora incluye soporte completo para Wayland con integración de libwayland y wlroots:

### Bibliotecas Soportadas

- **libwayland** - Biblioteca estándar del protocolo Wayland
- **wlroots** - Biblioteca de compositor modular
- **Implementación personalizada** - Fallback a implementación de Eclipse OS

### Instalación de Dependencias

```bash
# Instalar libwayland
sudo apt-get install libwayland-dev

# Instalar wlroots (opcional, recomendado)
sudo apt-get install libwlroots-dev

# O compilar wlroots desde el código fuente
git clone https://gitlab.freedesktop.org/wlroots/wlroots
cd wlroots
meson build
ninja -C build
sudo ninja -C build install
```

### Construcción con Wayland

El sistema de construcción detecta automáticamente las bibliotecas disponibles:

```bash
./build.sh
```

El script mostrará qué backend de Wayland está usando:
- "Found wlroots" - usando wlroots
- "Found wayland-server" - usando libwayland
- "Building with custom implementation" - sin bibliotecas del sistema

Para más información, consulta [WAYLAND_INTEGRATION.md](WAYLAND_INTEGRATION.md)

## Documentación Adicional

- **[FAQ](FAQ.md)** - Preguntas frecuentes y soluciones
- **[CONTRIBUTING](CONTRIBUTING.md)** - Guía para contribuir al proyecto
- **[SECURITY](SECURITY.md)** - Consideraciones de seguridad
- **[CHANGELOG](CHANGELOG.md)** - Historial de cambios
- **[Ejemplos](examples/)** - Ejemplos de uso y tutoriales

## Contacto

- **GitHub**: https://github.com/Pryancito/eclipse
- **Issues**: https://github.com/Pryancito/eclipse/issues
- **Discussions**: https://github.com/Pryancito/eclipse/discussions

---

**Eclipse OS** - Un sistema operativo moderno para el futuro