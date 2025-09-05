# 🌙 Eclipse OS v0.4.0

Un sistema operativo moderno desarrollado en Rust con arquitectura modular y soporte completo para hardware gráfico.

## 🚀 Características Principales

### 🏗️ Arquitectura Modular
- **Kernel `no_std`**: Desarrollado en Rust puro para máximo rendimiento
- **Bootloader UEFI**: Carga segura y rápida del kernel
- **Userland `std`**: Módulos de usuario con acceso completo a la biblioteca estándar
- **IPC Avanzado**: Comunicación eficiente entre kernel y userland

### 🖥️ Soporte Gráfico Completo
- **Graphics Output Protocol (GOP)**: Detección automática de framebuffer
- **DRM Integration**: Soporte completo para Direct Rendering Manager
- **NVIDIA Support**: Módulo especializado para GPUs NVIDIA
- **VGA Fallback**: Compatibilidad con hardware legacy
- **Multi-display**: Gestión de múltiples pantallas

### 🔧 Hardware Management
- **Detección automática** de dispositivos gráficos
- **Monitoreo en tiempo real** de GPU (temperatura, utilización, memoria)
- **Gestión de displays** (resoluciones, refresh rates)
- **Soporte multi-vendor** (NVIDIA, Intel, AMD)

## 📁 Estructura del Proyecto

```
eclipse/
├── eclipse_kernel/          # Kernel principal (no_std)
│   ├── src/
│   │   ├── main_simple.rs   # Kernel simplificado con VGA
│   │   ├── drivers/         # Sistema de drivers modulares
│   │   │   └── modular/     # Drivers modulares (DRM, GPU, Audio, Network)
│   │   ├── display.rs       # Driver de display unificado
│   │   └── shell.rs         # Shell interactivo
│   └── build_kernel_uefi.sh # Script de compilación del kernel
├── bootloader-uefi/         # Bootloader UEFI
│   ├── src/
│   │   ├── main.rs          # Carga ELF y GOP
│   │   └── bootloader.rs    # Lógica del bootloader
│   └── build.sh             # Script de compilación
├── userland/                # Módulos de usuario (std)
│   ├── ipc_common/          # Biblioteca IPC compartida
│   ├── module_loader/       # Cargador de módulos
│   ├── graphics_module/     # Módulo de gráficos básico
│   ├── drm_module/          # Módulo DRM avanzado
│   ├── nvidia_module/       # Módulo específico NVIDIA
│   └── app_framework/       # Framework de aplicaciones
├── installer/               # Instalador del sistema
└── eclipse-os-build/        # Distribución compilada
```

## 🛠️ Requisitos del Sistema

### Hardware Mínimo
- **CPU**: x86_64 con soporte UEFI
- **RAM**: 512MB mínimo, 2GB recomendado
- **GPU**: Cualquier dispositivo compatible con DRM o VGA
- **Almacenamiento**: 1GB para instalación básica

### Software Requerido
- **Rust**: 1.70+ con toolchain `stable`
- **Targets**: `x86_64-unknown-none`, `x86_64-unknown-uefi`
- **Herramientas**: `cargo`, `rustup`, `qemu-system-x86_64` (para testing)

## 🚀 Instalación y Uso

### 1. Compilación Completa
```bash
# Clonar el repositorio
git clone <repository-url>
cd eclipse

# Compilar todo el sistema
./build.sh
```

### 2. Instalación en Disco
```bash
# Instalar Eclipse OS (requiere sudo)
sudo ./install_eclipse_os.sh /dev/sda

# O reinstalar con bootloader estable
sudo ./reinstall_stable.sh /dev/sda
```

### 3. Testing en QEMU
```bash
# Test básico con VGA
cd eclipse_kernel
./test_vga.sh

# Test UEFI completo
cd eclipse-os-complete
./test_uefi.sh
```

## 🔧 Módulos del Sistema

### Kernel (`eclipse_kernel`)
- **Arquitectura**: `no_std` Rust
- **Target**: `x86_64-unknown-none`
- **Características**:
  - Sistema de drivers modulares
  - Soporte VGA completo
  - Shell interactivo
  - Gestión de memoria
  - Manejo de interrupciones

### Bootloader UEFI (`bootloader-uefi`)
- **Arquitectura**: UEFI Rust
- **Target**: `x86_64-unknown-uefi`
- **Características**:
  - Carga ELF64 del kernel
  - Detección GOP automática
  - Configuración de paginación
  - Logging vía puerto serie

### Módulos Userland

#### IPC Common (`ipc_common`)
- Biblioteca compartida para comunicación
- Serialización con `serde` y `bincode`
- Estructuras de mensajes estandarizadas

#### Module Loader (`module_loader`)
- Gestión del ciclo de vida de módulos
- Carga dinámica de componentes
- Monitoreo de estado

#### Graphics Module (`graphics_module`)
- Driver de gráficos básico
- Renderizado 2D simple
- Gestión de colores y fuentes

#### DRM Module (`drm_module`)
- **Dependencias**: `drm`, `drm-fourcc`, `libc`, `nix`
- **Funcionalidades**:
  - Detección automática de dispositivos
  - Soporte multi-vendor (NVIDIA, Intel, AMD)
  - Gestión de displays y modos
  - Monitoreo de rendimiento GPU

#### NVIDIA Module (`nvidia_module`)
- **Dependencias**: `drm`, `drm-fourcc`, `libc`, `nix`
- **Funcionalidades**:
  - Detección específica de hardware NVIDIA
  - Información detallada de GPU
  - Gestión de memoria VRAM
  - Monitoreo de temperatura y potencia
  - Soporte para características CUDA

#### App Framework (`app_framework`)
- Framework para aplicaciones de usuario
- Gestión de aplicaciones
- Terminal integrado
- File manager básico

## 🎮 Características Gráficas

### Soporte de Hardware
- **NVIDIA**: GeForce series (GTX, RTX)
- **Intel**: HD Graphics, Iris Xe
- **AMD**: Radeon series (RX, Vega)
- **VGA**: Compatibilidad legacy

### Modos de Display
- **Resoluciones**: 1920x1080, 2560x1440, 3840x2160
- **Refresh Rates**: 30Hz, 60Hz, 120Hz, 144Hz
- **Formatos**: RGB, BGR, YUV
- **Multi-display**: Soporte para múltiples pantallas

### Monitoreo en Tiempo Real
- **Utilización GPU**: Porcentaje de uso
- **Temperatura**: Monitoreo térmico
- **Memoria**: VRAM total, libre, usada
- **Relojes**: Frecuencias de GPU y memoria
- **Potencia**: Consumo energético

## 🔧 Desarrollo

### Compilación Individual
```bash
# Kernel
cd eclipse_kernel
cargo build --release --target x86_64-unknown-none

# Bootloader
cd bootloader-uefi
cargo build --release --target x86_64-unknown-uefi

# Módulos userland
cd userland/drm_module
cargo build --release

cd userland/nvidia_module
cargo build --release
```

### Testing
```bash
# Test del kernel
cd eclipse_kernel
./test_simple.sh

# Test completo del sistema
./test_system.sh
```

### Debugging
- **Puerto serie**: COM1 (0x3F8) para logs del bootloader
- **VGA**: Salida directa en pantalla
- **Logging**: Sistema de logs integrado

## 📊 Estado del Proyecto

### ✅ Completado
- [x] Kernel básico funcional
- [x] Bootloader UEFI con GOP
- [x] Sistema de drivers modulares
- [x] Soporte VGA completo
- [x] Módulos userland (IPC, Graphics, DRM, NVIDIA)
- [x] Sistema de instalación
- [x] Testing automatizado

### 🚧 En Desarrollo
- [ ] Interfaz gráfica avanzada
- [ ] Soporte para más drivers
- [ ] Optimizaciones de rendimiento
- [ ] Documentación de API

### 📋 Planificado
- [ ] Soporte para Wayland
- [ ] Drivers de red avanzados
- [ ] Sistema de archivos mejorado
- [ ] Aplicaciones de usuario

## 🤝 Contribución

### Cómo Contribuir
1. Fork del repositorio
2. Crear rama para feature (`git checkout -b feature/nueva-funcionalidad`)
3. Commit de cambios (`git commit -am 'Añadir nueva funcionalidad'`)
4. Push a la rama (`git push origin feature/nueva-funcionalidad`)
5. Crear Pull Request

### Estándares de Código
- **Rust**: Seguir `rustfmt` y `clippy`
- **Commits**: Mensajes descriptivos en español
- **Documentación**: Comentarios en español
- **Testing**: Tests para nuevas funcionalidades

## 📄 Licencia

Este proyecto está bajo la licencia MIT. Ver `LICENSE` para más detalles.

## 🙏 Agradecimientos

- **Rust Community**: Por el excelente ecosistema
- **UEFI Forum**: Por la especificación UEFI
- **Linux DRM**: Por la inspiración en el diseño de drivers
- **Contribuidores**: Por su tiempo y esfuerzo

---

**Eclipse OS** - *Un sistema operativo moderno para el futuro* 🌙

Para más información, consulta la documentación en `docs/` o abre un issue en GitHub.