# Preguntas Frecuentes (FAQ)

## General

### ¿Qué es Eclipse OS?

Eclipse OS es un sistema operativo moderno escrito en Rust, diseñado para ser eficiente, seguro y fácil de usar. Combina un kernel híbrido con un sistema de userland robusto y un sistema de display avanzado usando DRM (Direct Rendering Manager).

### ¿Por qué Rust?

Rust proporciona:
- **Seguridad de memoria** sin necesidad de recolección de basura
- **Seguridad de concurrencia** a nivel de compilador
- **Rendimiento** comparable a C/C++
- **Abstracciones de cero costo**
- **Prevención de bugs** en tiempo de compilación

### ¿En qué estado está el proyecto?

Eclipse OS está en **desarrollo activo** (v0.1.0). El kernel básico y el sistema de archivos están funcionales, pero muchas características están aún en desarrollo. **No se recomienda para uso en producción todavía.**

## Compilación

### ¿Qué necesito para compilar Eclipse OS?

- Rust 1.70 o superior
- QEMU (para pruebas)
- Herramientas de construcción básicas (gcc, make, etc.)
- En Linux: paquetes de desarrollo estándar

Ver la [Guía de Inicio Rápido](README.md#-inicio-rápido) para instrucciones detalladas.

### ¿Puedo compilar en Windows/macOS?

Sí, pero con limitaciones:
- **Windows**: Puedes compilar los componentes userland, pero necesitarás WSL2 o una VM Linux para compilar el kernel completo
- **macOS**: Similar a Windows, puedes compilar la mayoría de componentes pero el kernel necesita un entorno Linux

### La compilación falla con "toolchain 'nightly' is not installed"

Necesitas instalar la toolchain nightly de Rust:

```bash
rustup toolchain install nightly
rustup target add x86_64-unknown-none --toolchain nightly
rustup target add x86_64-unknown-uefi --toolchain nightly
```

### ¿Cuánto espacio en disco necesito?

- Código fuente: ~50 MB
- Compilación completa: ~500 MB - 1 GB
- Imagen del sistema: ~100 MB

## Uso

### ¿Puedo ejecutar Eclipse OS en hardware real?

Sí, pero **no se recomienda todavía**. El proyecto está en desarrollo y puede ser inestable. Se recomienda probar en QEMU primero.

### ¿Cómo pruebo Eclipse OS en QEMU?

```bash
# Después de compilar
./qemu.sh
```

### ¿Qué aplicaciones puedo ejecutar?

Actualmente Eclipse OS incluye:
- Sistema de archivos EclipseFS
- Aplicaciones de prueba básicas
- Terminal básico (en desarrollo)
- Compositor Wayland (en desarrollo)

### ¿Puedo ejecutar aplicaciones Linux?

No directamente. Eclipse OS tiene su propia ABI y no es compatible con binarios de Linux. En el futuro se podría implementar compatibilidad a través de emulación.

## Desarrollo

### ¿Cómo puedo contribuir?

Ver la [Guía de Contribución](CONTRIBUTING.md) para detalles completos. En resumen:
1. Fork el repositorio
2. Crea una rama para tu característica
3. Haz tus cambios siguiendo los estándares de código
4. Envía un Pull Request

### ¿Qué áreas necesitan ayuda?

- Sistema de ventanas/compositor
- Drivers de hardware
- Aplicaciones userland
- Documentación
- Pruebas y reporte de bugs

### ¿Necesito experiencia en desarrollo de OS?

No necesariamente. Hay áreas que no requieren experiencia profunda en OS:
- Documentación
- Aplicaciones userland
- Pruebas
- Sistema de archivos

Para desarrollo del kernel, experiencia previa es útil pero no estrictamente necesaria si estás dispuesto a aprender.

## EclipseFS

### ¿Qué es EclipseFS?

EclipseFS es el sistema de archivos personalizado de Eclipse OS, inspirado en RedoxFS. Incluye características modernas como:
- Journaling para integridad de datos
- Encriptación transparente
- Copy-on-Write
- Snapshots
- Compresión

### ¿Puedo montar EclipseFS en Linux?

Sí, usando el driver FUSE incluido:

```bash
cd eclipsefs-fuse
cargo build --release
./target/release/eclipsefs-fuse /mnt/punto_montaje imagen.eclipsefs
```

### ¿Es compatible con ext4/FAT32?

No, EclipseFS es un sistema de archivos independiente. Sin embargo, Eclipse OS puede leer FAT32 (soporte NTFS en desarrollo).

## Troubleshooting

### Veo una pantalla verde/negra en QEMU

Esto es normal durante el arranque. El sistema puede tardar unos segundos en inicializar el display.

### El build falla con errores de linking

Asegúrate de tener instaladas las herramientas de desarrollo:

```bash
# Ubuntu/Debian
sudo apt-get install build-essential

# Fedora
sudo dnf groupinstall "Development Tools"
```

### "Permission denied" al ejecutar scripts

Haz los scripts ejecutables:

```bash
chmod +x build.sh qemu.sh test_*.sh
```

## Licencia y Legal

### ¿Bajo qué licencia está Eclipse OS?

Eclipse OS está licenciado bajo la Licencia MIT. Ver [LICENSE](LICENSE) para detalles.

### ¿Puedo usar Eclipse OS en proyectos comerciales?

Sí, la licencia MIT permite uso comercial. Solo debes incluir el aviso de copyright y licencia.

### ¿Puedo modificar y redistribuir Eclipse OS?

Sí, puedes modificar y redistribuir bajo los términos de la licencia MIT.

## Roadmap

### ¿Cuándo estará lista la versión 1.0?

No hay una fecha específica. El proyecto está en desarrollo activo y la v1.0 se lanzará cuando las características core estén estables y bien probadas.

### ¿Qué características están planificadas?

Ver el [Roadmap](README.md#roadmap) en el README principal.

### ¿Habrá soporte para arquitecturas ARM?

Está planificado para el futuro, pero actualmente el enfoque es en x86_64.

## Contacto

### ¿Dónde puedo obtener ayuda?

- [GitHub Issues](https://github.com/Pryancito/eclipse/issues) - Para bugs y preguntas técnicas
- [GitHub Discussions](https://github.com/Pryancito/eclipse/discussions) - Para discusiones generales

### ¿Cómo reporto un bug?

Crea un issue en GitHub con:
- Descripción clara del problema
- Pasos para reproducir
- Versión de Eclipse OS
- Tu sistema operativo y versión de Rust

---

**¿No encuentras tu pregunta?** Abre un [issue](https://github.com/Pryancito/eclipse/issues) o inicia una [discusión](https://github.com/Pryancito/eclipse/discussions).
