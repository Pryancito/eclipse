# zCore (Eclipse OS)

[![CI](https://github.com/rcore-os/zCore/actions/workflows/build.yml/badge.svg?branch=master)](https://github.com/rcore-os/zCore/actions)
[![Docs](https://img.shields.io/badge/docs-pages-green)](https://rcore-os.github.io/zCore/)
[![Estado de Cobertura](https://coveralls.io/repos/github/rcore-os/zCore/badge.svg?branch=master)](https://coveralls.io/github/rcore-os/zCore?branch=master)
[![Issues](https://img.shields.io/github/issues/rcore-os/zCore)](https://github.com/rcore-os/zCore/issues)
[![Forks](https://img.shields.io/github/forks/rcore-os/zCore)](https://github.com/rcore-os/zCore/fork)
![Stars](https://img.shields.io/github/stars/rcore-os/zCore)
![Licencia](https://img.shields.io/github/license/rcore-os/zCore)

Un núcleo de sistema operativo basado en Zircon que proporciona compatibilidad con Linux.

## Resumen del Proyecto

zCore es una reimplementación del micronúcleo `Zircon` en Rust seguro como un programa de espacio de usuario.

- Arquitectura de diseño de zCore.
- Soporte para Zircon y Linux en modo bare-metal.
- Soporte para Zircon y Linux en modo libos.
- Para más guías sobre aplicaciones gráficas y otros detalles, consulte la [documentación original de arquitectura](README-arch.md).

## Iniciar el Núcleo

   ```bash
   cargo qemu --arch x86_64
   ```

   Este comando iniciará zCore usando QEMU para la arquitectura especificada.

   El sistema de archivos predeterminado incluirá la aplicación `busybox` y la biblioteca `musl-libc`. Estos se compilan automáticamente usando la cadena de herramientas de compilación cruzada correspondiente.

## Contenido

- [Iniciar el Núcleo](#iniciar-el-núcleo)
- [Construcción del Proyecto](#construcción-del-proyecto)
  - [Comandos de Construcción](#comandos-de-construcción)
  - [Referencia de Comandos](#referencia-de-comandos)
- [Soporte de Plataformas](#soporte-de-plataformas)
  - [x86_64 (Qemu/ICH9)](#x86_64-qemuich9)
  - [Qemu/virt (RISC-V)](#qemuvirt)
  - [Allwinner D1/Nezha](#allwinner-d1nezha)
  - [StarFive VisionFive](#starfive-visionfive)
  - [CVITEK CR1825](#cvitek-cr1825)

## Construcción del Proyecto

La construcción del proyecto utiliza el [patrón xtask](https://github.com/matklad/cargo-xtask). Las operaciones comunes están encapsuladas en comandos de `cargo`.

Además, se proporciona un [Makefile](Makefile) para compatibilidad con algunos scripts antiguos.

Los entornos de desarrollo probados actualmente incluyen Ubuntu 20.04, Ubuntu 22.04 y Debian 11. 

### Comandos de Construcción

El formato básico de los comandos es `cargo <comando> [--args [valor]]`. Esto es en realidad una abreviatura de `cargo run --package xtask --release -- <comando> [--args [valor]]`. El comando se pasa a la aplicación xtask para su análisis y ejecución.

Muchos comandos dependen de otros para preparar el entorno. El diagrama de dependencias es el siguiente:

```text
┌────────────┐ ┌─────────────┐ ┌─────────────┐
| update-all | | check-style | | zircon-init |
└────────────┘ └─────────────┘ └─────────────┘
┌─────┐ ┌──────┐  ┌─────┐  ┌─────────────┐ ┌─────────────────┐
| asm | | qemu |─→| bin |  | linux-libos | | libos-libc-test |
└─────┘ └──────┘  └─────┘  └─────────────┘ └─────────────────┘
                     |            └───┐┌─────┘   ┌───────────┐
                     ↓                ↓↓      ┌──| libc-test |
                 ┌───────┐        ┌────────┐←─┘  └───────────┘
                 | image |───────→| rootfs |←─┐ ┌────────────┐
                 └───────┘        └────────┘  └─| other-test |
                 ┌────────┐           ↑         └────────────┘
                 | opencv |────→┌───────────┐
                 └────────┘  ┌─→| musl-libc |
                 ┌────────┐  |  └───────────┘
                 | ffmpeg |──┘
                 └────────┘
-------------------------------------------------------------------
Leyenda: A → B (A depende de B, ejecutar A ejecutará automáticamente B primero)
```

### Referencia de Comandos

#### **update-all**
Actualiza la cadena de herramientas, las dependencias y los submódulos de git.
```bash
cargo update-all
```

#### **check-style**
Chequeo estático. Verifica que el código compile con diversas opciones.
```bash
cargo check-style
```

#### **zircon-init**
Descarga los archivos binarios necesarios para el modo Zircon.
```bash
cargo zircon-init
```

#### **qemu**
Inicia zCore en QEMU. Requiere tener QEMU instalado.
```bash
cargo qemu --arch x86_64 --smp 4
```

Soporte para conectar QEMU a GDB:
```bash
cargo qemu --arch x86_64 --smp 4 --gdb 1234
```

#### **rootfs**
Reconstruye el rootfs de Linux.
```bash
cargo rootfs --arch x86_64
```

#### **image**
Construye el archivo de imagen del rootfs de Linux a partir del directorio correspondiente.
```bash
cargo image --arch x86_64
```

## Soporte de Plataformas

### x86_64 (Qemu/ICH9)

Soporte completo para arquitectura x86_64 mediante QEMU, con un driver AHCI/SATA de alto rendimiento.

- **Driver AHCI**: Soporta controladores ICH9, con mapeo dinámico de BAR5 (ABAR) y gestión de puertos.
- **Estado**: El sistema identifica discos SATA, monta el sistema de archivos raíz y arranca con éxito hasta una shell de `busybox`.

### Qemu/virt (RISC-V)

Inicia directamente usando comandos de cargo, ver [Iniciar el Núcleo](#iniciar-el-núcleo).

### Allwinner D1/Nezha

Usa el siguiente comando para construir la imagen del sistema:
```bash
cargo bin -m nezha -o z.bin
```
Luego usa [rustsbi-d1](https://github.com/rustsbi/rustsbi-d1) para desplegar la imagen en Flash o DRAM.

### StarFive VisionFive

Usa el siguiente comando para construir la imagen:
```bash
cargo bin -m visionfive -o z.bin
```

### CVITEK CR1825

Usa el siguiente comando para construir la imagen:
```bash
cargo bin -m cr1825 -o z.bin
```

## Gestión de Paquetes (APK Tools)

zCore (Eclipse OS) utiliza `apk-tools` como gestor de paquetes. Para compilarlo y preparar el entorno:

### Compilación de apk-tools

Para realizar la compilación cruzada para Eclipse OS:

```bash
cd tools/apk
# Configurar el entorno de construcción con el archivo de cross-compilación
meson setup --cross-file meson.cross-eclipse eclipse
# Compilar el binario
ninja -C eclipse
```

El binario resultante se encontrará en `tools/apk/eclipse/src/apk`.

### Preparación del Entorno APK

Para que `apk` funcione correctamente en el sistema, es necesario crear la estructura de archivos mínima:

1.  **Directorios de base de datos**: `/lib/apk/db/`
2.  **Configuración de repositorios**: `/etc/apk/repositories`
3.  **Claves de confianza**: `/etc/apk/keys/`

Ejemplo de configuración de repositorios:
```bash
echo "http://dl-cdn.alpinelinux.org/alpine/v3.23/main" > /etc/apk/repositories
echo "http://dl-cdn.alpinelinux.org/alpine/v3.23/community" >> /etc/apk/repositories
```

## Otros

- [An English README](docs/README_EN.md)
- [Notas para desarrolladores](docs/for-developers.md)
- [Registro de cambios del sistema de construcción](xtask/CHANGELOG.md)
