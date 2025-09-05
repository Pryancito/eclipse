# Eclipse OS Kernel

Este es el kernel Eclipse OS con todos los módulos integrados.

## Características

- ✅ Kernel completo con todos los módulos
- ✅ Sistema de archivos avanzado
- ✅ Interfaz gráfica con soporte NVIDIA
- ✅ Sistema de seguridad robusto
- ✅ Inteligencia artificial integrada
- ✅ Monitoreo del sistema
- ✅ Sistema de personalización
- ✅ Gestión de contenedores
- ✅ Sistema de plugins
- ✅ Gestión de energía y térmica
- ✅ Sistema de privacidad

## Uso

### Compilar desde cero
```bash
./build_simple.sh
```

### Probar en QEMU
```bash
cd eclipse-os-dist
./test_kernel.sh
```

### Compilar solo el kernel
```bash
cd eclipse_kernel
cargo build --release
```

## Estructura

- `eclipse_kernel` - Kernel compilado
- `test_kernel.sh` - Script para probar en QEMU
- `README.md` - Documentación

## Requisitos

- Rust toolchain
- QEMU

## Notas

- El kernel está compilado en modo release para mejor rendimiento
- Se incluyen todas las advertencias del compilador pero no afectan la funcionalidad
- El sistema está diseñado para ser modular y extensible
- Para pruebas, se ejecuta directamente el kernel sin bootloader
