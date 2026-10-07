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
  - [Programas necesarios](#programas-necesarios)
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

### Programas necesarios

El host de compilación es **Linux** (x86_64). Instala estas herramientas **antes** de `cargo rootfs` / `make qemu`: xtask las invoca por nombre (`wget`, `tar`, `make`, …) y sin ellas el build no arranca.

#### Cadena Rust

El archivo [`rust-toolchain.toml`](rust-toolchain.toml) fija `nightly-2026-09-01` y los componentes `rust-src`, `llvm-tools-preview`, `rustfmt` y `clippy`. Con [rustup](https://rustup.rs) instalado, `cargo` descarga esa toolchain sola.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
# userspace del rootfs (eclipse-init, lunarbar, …):
rustup target add x86_64-unknown-linux-musl
```

`PATH` debe incluir `~/.cargo/bin` (`cargo`, `rustc`, `rustup`).

#### Paquetes del sistema

| Programa | Para qué | Paquete Debian/Ubuntu |
|---|---|---|
| `git` | clon de busybox y submódulos | `git` |
| `make` | busybox, rboot, `Makefile` del repo | `make` |
| `gcc` | tests de hilos y herramientas C del rootfs | `build-essential` |
| `tar` | extraer el cross musl (`*-linux-musl-cross.tgz`) | `tar` |
| `wget` o `curl` | apk estático, toolchain musl, `cacert.pem` | `wget` y/o `curl` |
| `sed` | ajustar `.config` de busybox | `sed` |
| `python3` | regenerar headers SPIR-V (opcional) | `python3` |
| `mkfs.vfat` | ESP FAT32 (`cargo image`, `make iso`) | `dosfstools` |
| `mmd` / `mcopy` | copiar EFI al FAT | `mtools` |
| `gzip` | comprimir `efi.img` / rootfs / home | `gzip` |
| `qemu-system-x86_64` | `make qemu` / `cargo qemu` | `qemu-system-x86` |
| `qemu-img` | `make qcow2` | `qemu-utils` |
| `xorriso` | `make iso` | `xorriso` |

El cross **musl** (`x86_64-linux-musl-gcc`, `…-strip`) no se instala a mano: `cargo rootfs` lo baja a `ignored/target/x86_64/x86_64-linux-musl-cross/` (hace falta `wget`/`curl` y `tar`).

Instalación típica en Debian/Ubuntu:

```bash
sudo apt update
sudo apt install --no-install-recommends \
  build-essential git make gcc tar wget curl sed python3 \
  dosfstools mtools gzip qemu-system-x86 qemu-utils xorriso
```

En Fedora: `dnf install gcc make git tar wget curl python3 dosfstools mtools gzip qemu-system-x86 qemu-img xorriso`. En Arch: `pacman -S base-devel git wget python dosfstools mtools qemu-system-x86 xorriso`.

Comprobar que el host está listo:

```bash
command -v cargo git make gcc tar wget curl qemu-system-x86_64 mkfs.vfat mcopy gzip
rustup show
```

No hace falta `btrfs-progs` en el host: las imágenes btrfs las genera xtask. `cmake` / OpenCV / FFmpeg solo si se usa `cargo opencv` / `cargo ffmpeg`.

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
cargo rootfs --arch x86_64 --variant minimal   # sin escritorio
```

#### **image**
Construye el archivo de imagen del rootfs de Linux a partir del directorio correspondiente.
```bash
cargo image --arch x86_64
cargo image --arch x86_64 --variant minimal
```

### Variantes e ISO de distribución

Eclipse se publica en dos **variantes**, y cada una tiene su propio rootfs y sus
propios artefactos:

| Variante | Qué lleva | Rootfs | Sesión |
|---|---|---|---|
| `desktop` (por defecto) | labwc/Xorg y todo el cierre de apk: Mesa, Firefox, XFCE, freedoom | `rootfs/<arch>` | la que diga `desktop=` / `/etc/eclipse/desktop`, por defecto `labwc` |
| `minimal` | el sistema base: busybox, red, `install-eclipse` | `rootfs/<arch>-minimal` | consola (`/etc/eclipse/desktop` = `none`) |

`minimal` **no es un recorte posterior** de la de escritorio: es un rootfs que
nunca ejecuta `desktop::install` ni `xorg::install`, así que los ficheros del
escritorio no llegan a estar en disco. No hay, por tanto, una lista de qué podar
que haya que mantener al día cada vez que se añade un paquete.

> **Hoy `minimal` no tiene sonido ni zonas horarias con nombre.** No por
> diseño: ALSA, PulseAudio, `mpg123`, `tzdata` y `musl-locales` están en la
> lista de paquetes de `xorg::install`, que la variante minimal no ejecuta. El
> rootfs minimal sí escribe `pulseaudio.service` (no lleva `desktop =`, así que
> arranca) y `eclipse-tz`, de modo que el servicio se reintenta contra un
> binario que no existe y `TZ=Europe/Madrid` se queda en UTC. Separar esas
> dependencias de base del conjunto gráfico es una tanda aparte; está anotado
> en la revisión del #1754.

La variante `desktop` **no lleva sufijo** en ninguna ruta intermedia, así que
`rootfs/x86_64`, `zCore/x86_64.img` e `ignored/target/efi.img.gz` siguen donde
siempre y `make qemu` se comporta igual que antes de que existiera este eje.

`make release` recorre la matriz de (variante × arquitectura) y saca una ISO por
combinación, con el nombre diciendo las dos:

```bash
make release                                  # toda la matriz
make release ARCHS=x86_64                     # las dos variantes de x86_64
make release VARIANTS=minimal                 # minimal en las dos arquitecturas
make release ARCHS=x86_64 VARIANTS=desktop    # una sola
```

```text
dist/eclipse-desktop-x86_64.iso
dist/eclipse-minimal-x86_64.iso
dist/eclipse-desktop-aarch64.iso
dist/eclipse-minimal-aarch64.iso
```

Y cada combinación se puede pedir también con el objetivo de siempre:

```bash
make iso                                      # desktop, x86_64
make iso VARIANT=minimal                      # minimal, x86_64
```

Una combinación que falle **no aborta el resto**: `make release` las intenta
todas y acaba con un resumen de qué ISO existe y qué falta.

> **La ISO de arm64 lleva la imagen dentro del kernel.** Una ISO es un solo
> medio, así que el kernel no puede coger su raíz de un segundo disco como hace
> `make qemu ARCH=aarch64`, y el `rayboot` precompilado que arranca aarch64
> tampoco sabe pasarle un initramfs: su `Boot.json` no tiene campo para eso. Así
> que para la ISO el cargador queda fuera de la ecuación — la imagen se enlaza
> **dentro** del kernel (`LINK_USER_IMG=1`, la feature `link-user-img` de
> `zCore/src/fs.rs`) y el kernel lee su raíz de sí mismo. Para arrancarla:
>
> ```bash
> make iso ARCH=aarch64 VARIANT=minimal
> make qemu-iso-aarch64 ARCH=aarch64 VARIANT=minimal
> ```
>
> **Dale 1 GB, no 4.** Con `-m 4G` este kernel no imprime nada después del
> «jump to kernel» de rayboot, que se lee igual que una ISO rota y no lo es; es
> justo al revés que la de x86_64, que necesita 4 GB. `make qemu-iso-aarch64` ya
> trae la receta buena.
>
> **No es un instalador.** La ISO de x86_64 lleva los payloads que
> `install-eclipse` escribe a disco; eso sigue siendo x86-only en
> `xtask/src/linux/image.rs`, así que la de arm64 arranca la sesión de consola de
> su variante y no instala nada.
>
> **riscv64 sigue sin ISO**, y por otro motivo: QEMU arranca su kernel
> directamente, sin medio UEFI, así que no hay ESP que meter en la ISO. El
> detalle está en el objetivo `iso-unsupported-arch` del `Makefile`.

## Soporte de Plataformas

### x86_64 (Qemu y Hardware Real)

Soporte completo para arquitectura x86_64 en emuladores (QEMU) y en hardware real con mejoras significativas de compatibilidad:

- **Driver AHCI/SATA**: Soporte mejorado con inicialización robusta que incluye el protocolo de handoff BIOS/OS, estabilización del enlace físico PHY (SATA DET) y verificación flexible de firmas de dispositivos (`PORT_SIG`). También se activa el Bus Mastering PCI para prevenir fallos de Master Abort en hardware real.
- **Driver NVMe**: Soporte para controladores de almacenamiento NVMe con consistencia de caché por DMA utilizando instrucciones `clflush`.
- **Detección y Particionado Automático**: Detección dinámica de esquemas de particionamiento MBR y GPT al arranque del sistema. Las particiones (como `/dev/sda1` o `/dev/nvme0n1p1`) se registran automáticamente en `devfs` y se exponen como dispositivos independientes.
- **Entrada y Teclado**: Soporte para teclado PS/2 con mapeo completo de la distribución de teclado en español, permitiendo el uso correcto de caracteres especiales y acentos (`ñ`, `Ñ`, `@`, `#`, `[`, `]`, `{`, `}`, `|`, `\`, `~`, `€`) a través de modificadores (AltGr y Shift).
- **Instalador del Sistema (`install-eclipse`)**: Herramienta de instalación optimizada para desplegar el sistema en discos físicos y virtuales, con detección precisa de tamaño de disco combinando consultas a `sysfs` y la llamada `BLKGETSIZE64`. Realiza escrituras y modificaciones directamente sobre los dispositivos de partición (p. ej. `/dev/sda1` y `/dev/sda2`) para garantizar la consistencia de la caché de bloques y la correcta persistencia de ficheros clave de configuración (`/etc/fstab` y `rboot.conf`).
- **Sistemas de Archivos**: El sistema de archivos raíz de Eclipse OS es **btrfs**, con un driver propio de lectura/escritura en el kernel (crate `vendor/btrfs-rs`) y generación de imágenes integrada en la compilación (sin depender de `btrfs-progs`); el sistema de archivos se expande automáticamente al tamaño de la partición en el primer montaje. Se mantiene soporte de **ext2/ext3/ext4** (instalaciones antiguas y discos externos) y **vfat/FAT32** (partición EFI). Las imágenes btrfs generadas son montables por Linux y pasan `btrfs check`.
- **Estabilidad de Memoria ante Presión (OOM)**: Mitigación de pánicos del kernel por agotamiento de heap (BuddyAllocator) mediante límites estrictos de asignación temporal (1 MB) y procesamiento fragmentado (chunked) en syscalls de E/S (`sys_read`, `sys_pread`, etc.), y una estrategia de carga ELF (`sys_execve`) robusta utilizando mapeos dinámicos bajo demanda de `VmObject`s paginados en la región virtual del kernel (`KERNEL_ASPACE`) sin asignar memoria física contigua.
- **Pila Gráfica (DRM/KMS)**: Implementación de la UAPI DRM/KMS de Linux que permite ejecutar software gráfico estándar — `Xorg` (`startx`) mediante los nodos de consola virtual y los `ioctl`s de VT/KD, y compositores Wayland (`wlroots`/`labwc`, con `WLR_RENDERER=pixman` por defecto cuando no hay GPU). Incluye soporte de PRIME (exportación/importación de dma-buf). Ver [docs/README-drm.md](docs/README-drm.md) y [docs/README-xorg.md](docs/README-xorg.md).
- **Seguridad (`hunter`)**: Subsistema de seguridad in-kernel que combina una capa de aplicación de políticas estilo LSM con un sistema de detección de intrusiones (IDS) por comportamiento, registrando cada decisión en un log forense legible desde `/proc/hunter`. Ver [docs/hunter-security.md](docs/hunter-security.md).
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
apk add -X https://dl-cdn.alpinelinux.org/alpine/v3.24/main -u alpine-keys
```

## Documentación

### Compatibilidad con Linux
- [Mapa de compatibilidad guiado por `Documentation/` del kernel](docs/linux-compat.md)

### Gráficos y entorno de escritorio
- [DRM / KMS — conformidad con la UAPI de Linux](docs/README-drm.md)
- [uAPI compatible con nouveau en la GPU NVIDIA (opt-in, `nvidia.nouveau_uapi`)](docs/README-nouveau-uapi.md)
- [Ejecutar un servidor X (`startx`)](docs/README-xorg.md)

### Compatibilidad de ABI
- [Compatibilidad con FreeBSD (ABI FreeBSD/amd64)](docs/README-freebsd.md)

### Seguridad
- [hunter — subsistema de seguridad in-kernel](docs/hunter-security.md)
- [hunter — informe de endurecimiento (red-team)](docs/hunter-hardening.md)

### Plataformas RISC-V
- [StarFive VisionFive](docs/README-visionfive.md)
- [Allwinner D1/Nezha](docs/README-D1.md)
- [Sophgo/CVITEK C910](docs/README-C910.md)
- [StarFive JH7110 (FU740)](docs/README-fu740.md)
- [Notas de portado a RISC-V 64](docs/porting-rv64.md)

## Otros

- [An English README](docs/README_EN.md)
- [Notas para desarrolladores](docs/for-developers.md)
- [Documentación de arquitectura original (upstream zCore)](README-arch.md)
- [Registro de cambios del sistema de construcción](xtask/CHANGELOG.md)
