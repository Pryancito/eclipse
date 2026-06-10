# zCore (Eclipse OS)

Un núcleo de sistema operativo basado en Zircon que proporciona compatibilidad con Linux.

## Resumen del Proyecto

zCore es una reimplementación del micronúcleo `Zircon` en Rust seguro como un programa de espacio de usuario.

- Arquitectura de diseño de zCore.
- Soporte para Zircon y Linux en modo bare-metal.
- Soporte para Zircon y Linux en modo libos.
- Para más guías sobre aplicaciones gráficas y otros detalles, consulte la [documentación original de arquitectura](README-arch.md).

## Iniciar el Núcleo

   ```bash
   make qemu ARCH=x86_64
   ```

   Este comando iniciará zCore usando QEMU para la arquitectura especificada.

   El sistema de archivos predeterminado incluirá la aplicación `busybox` y la biblioteca `musl-libc`. Estos se compilan automáticamente usando la cadena de herramientas de compilación cruzada correspondiente.

## Configuración del Proceso Inicial (ROOTPROC)

Para cambiar el proceso inicial (init) que zCore ejecuta al arrancar, se debe modificar el archivo de configuración `zCore/rboot.conf`.

Dentro de este archivo, localice la línea `cmdline` y añada el parámetro `ROOTPROC`. Los parámetros en la línea de comandos se separan por el carácter `:`.

**Ejemplo para ejecutar una shell de busybox (predeterminado):**
```ini
cmdline=LOG=warn:ROOTPROC=/bin/busybox?sh
```

**Ejemplo para ejecutar un binario específico con argumentos:**
```ini
cmdline=LOG=warn:ROOTPROC=/path/to/init?--option?value
```

**Formato:**
- `ROOTPROC=/ruta/al/binario`: Especifica la ruta del ejecutable en el sistema de archivos.
- `?`: Se usa para separar el comando de sus argumentos y los argumentos entre sí.

## Contenido

- [Iniciar el Núcleo](#iniciar-el-núcleo)
- [Configuración del Proceso Inicial (ROOTPROC)](#configuración-del-proceso-inicial-rootproc)
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

### x86_64 (Qemu y Hardware Real)

Soporte completo para arquitectura x86_64 en emuladores (QEMU) y en hardware real con mejoras significativas de compatibilidad:

- **Driver AHCI/SATA**: Soporte mejorado con inicialización robusta que incluye el protocolo de handoff BIOS/OS, estabilización del enlace físico PHY (SATA DET) y verificación flexible de firmas de dispositivos (`PORT_SIG`). También se activa el Bus Mastering PCI para prevenir fallos de Master Abort en hardware real.
- **Driver NVMe**: Soporte para controladores de almacenamiento NVMe con consistencia de caché por DMA utilizando instrucciones `clflush`.
- **Detección y Particionado Automático**: Detección dinámica de esquemas de particionamiento MBR y GPT al arranque del sistema. Las particiones (como `/dev/sda1` o `/dev/nvme0n1p1`) se registran automáticamente en `devfs` y se exponen como dispositivos independientes.
- **Entrada y Teclado**: Soporte para teclado PS/2 con mapeo completo de la distribución de teclado en español, permitiendo el uso correcto de caracteres especiales y acentos (`ñ`, `Ñ`, `@`, `#`, `[`, `]`, `{`, `}`, `|`, `\`, `~`, `€`) a través de modificadores (AltGr y Shift).
- **Instalador del Sistema (`install-eclipse`)**: Herramienta de instalación optimizada para desplegar el sistema en discos físicos y virtuales, con detección precisa de tamaño de disco combinando consultas a `sysfs` y la llamada `BLKGETSIZE64`. Realiza escrituras y modificaciones directamente sobre los dispositivos de partición (p. ej. `/dev/sda1` y `/dev/sda2`) para garantizar la consistencia de la caché de bloques y la correcta persistencia de ficheros clave de configuración (`/etc/fstab` y `rboot.conf`).
- **Estabilidad de Memoria ante Presión (OOM)**: Mitigación de pánicos del kernel por agotamiento de heap (BuddyAllocator) mediante límites estrictos de asignación temporal (1 MB) y procesamiento fragmentado (chunked) en syscalls de E/S (`sys_read`, `sys_pread`, etc.), y una estrategia de carga ELF (`sys_execve`) robusta utilizando mapeos dinámicos bajo demanda de `VmObject`s paginados en la región virtual del kernel (`KERNEL_ASPACE`) sin asignar memoria física contigua.
- **Estado**: El sistema arranca con éxito en hardware real e inicializa los controladores de almacenamiento, monta el sistema de archivos de forma nativa e inicia la consola interactiva (`busybox`).

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

Para instalar las claves de confianza de Alpine:
```bash
apk add -X https://dl-cdn.alpinelinux.org/alpine/v3.23/main -u alpine-keys
```

## Otros

- [An English README](docs/README_EN.md)
- [Notas para desarrolladores](docs/for-developers.md)
- [Registro de cambios del sistema de construcción](xtask/CHANGELOG.md)
