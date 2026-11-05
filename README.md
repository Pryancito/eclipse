# Eclipse OS

**Español** · [English](README.en.md)

Eclipse OS es un sistema operativo completo escrito en Rust. Su núcleo nace de
[zCore](https://github.com/rcore-os/zCore) (una reimplementación del micronúcleo
Zircon de Fuchsia en Rust seguro) y le añade una personalidad **compatible con
Linux** suficiente para arrancar un escritorio Wayland, navegadores, audio y
aceleración NVIDIA sobre paquetes de Ubuntu.

- **Núcleo**: Zircon + capa de compatibilidad Linux (`linux-object`,
  `linux-syscall`), y además una ABI de FreeBSD/amd64.
- **Arquitecturas**: x86_64 (QEMU y hardware real), aarch64 (QEMU) y riscv64
  (QEMU y varias placas).
- **Userspace**: init propio (`eclipse-init`), base estática con busybox y
  paquetes de **Ubuntu 26.10** instalables con `apt`.
- **Escritorio**: sesión Wayland con labwc, panel y fondo nativos (`lunarbar`,
  `lunarbg`) y varias apariencias; Xorg disponible como alternativa.
- **Almacenamiento**: raíz **btrfs** con driver propio de lectura/escritura, más
  ext2/3/4 y FAT32.
- **Seguridad**: `hunter`, subsistema in-kernel de políticas e IDS.

La documentación técnica vive en [`docs/`](docs/README.md).

## Contenido

- [Arranque rápido](#arranque-rápido)
- [Preparar el host](#preparar-el-host)
- [Compilar](#compilar)
  - [Comandos de xtask](#comandos-de-xtask)
  - [Objetivos del Makefile](#objetivos-del-makefile)
  - [Variantes e ISO de distribución](#variantes-e-iso-de-distribución)
- [Opciones de arranque](#opciones-de-arranque)
- [Plataformas](#plataformas)
- [Paquetes (apt)](#paquetes-apt)
- [Documentación](#documentación)

## Arranque rápido

```bash
make qemu ARCH=x86_64
```

Construye el rootfs, la imagen y el núcleo, y arranca Eclipse en QEMU con la
sesión de escritorio `labwc`. Para la variante de consola:

```bash
make qemu VARIANT=minimal
```

Otras variables útiles de `make qemu`: `SMP=4`, `GRAPHIC=off`,
`AUDIO=off`, `DESKTOP=xorg` (sesión X en lugar de labwc) y `KOPTS=a:b` para
pasar opciones extra a la cmdline del núcleo.

## Preparar el host

El host de compilación es **Linux x86_64**, preferiblemente **Debian o Ubuntu**:
el rootfs de escritorio se arma con `apt-get download` y `dpkg-deb` del propio
host (con un estado de apt aislado, así que la versión del host da igual; se
usa a diario con Ubuntu 24.04). Instala estas herramientas **antes** de
`cargo rootfs` / `make qemu`: xtask las invoca por nombre y sin ellas el build
no arranca.

### Cadena Rust

[`rust-toolchain.toml`](rust-toolchain.toml) fija `nightly-2026-09-01` con los
componentes `rust-src`, `llvm-tools-preview`, `rustfmt` y `clippy`. Con
[rustup](https://rustup.rs) instalado, `cargo` descarga esa toolchain sola.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
# userspace del rootfs (eclipse-init, lunarbar, …):
rustup target add x86_64-unknown-linux-musl
```

`PATH` debe incluir `~/.cargo/bin`.

### Paquetes del sistema

| Programa | Para qué | Paquete Debian/Ubuntu |
|---|---|---|
| `git` | busybox y submódulos | `git` |
| `make`, `gcc` | busybox, rboot, herramientas C del rootfs | `build-essential` |
| `tar`, `wget` o `curl` | cross musl, apk estático, `cacert.pem` | `tar`, `wget`, `curl` |
| `apt-get`, `dpkg-deb` | paquetes de Ubuntu del rootfs de escritorio | `apt`, `dpkg` (ya vienen en Debian/Ubuntu) |
| `sed` | ajustar `.config` de busybox | `sed` |
| `python3` | regenerar cabeceras SPIR-V (opcional) | `python3` |
| `mkfs.vfat`, `mmd`/`mcopy` | ESP FAT32 (`cargo image`, `make iso`) | `dosfstools`, `mtools` |
| `gzip` | comprimir `efi.img` / rootfs / home | `gzip` |
| `qemu-system-x86_64`, `qemu-img` | `make qemu`, `make qcow2` | `qemu-system-x86`, `qemu-utils` |
| `xorriso` | `make iso` | `xorriso` |

El cross **musl** (`x86_64-linux-musl-gcc`, `…-strip`) no se instala a mano:
`cargo rootfs` lo baja a `ignored/target/x86_64/x86_64-linux-musl-cross/`.

```bash
sudo apt update
sudo apt install --no-install-recommends \
  build-essential git make gcc tar wget curl sed python3 \
  dosfstools mtools gzip qemu-system-x86 qemu-utils xorriso
```

En Fedora o Arch se pueden instalar las mismas herramientas
(`dnf install gcc make git tar wget curl python3 dosfstools mtools gzip qemu-system-x86 qemu-img xorriso`,
`pacman -S base-devel git wget python dosfstools mtools qemu-system-x86 xorriso`),
pero sin `apt-get` el rootfs de escritorio no puede bajar sus paquetes; usa
`VARIANT=minimal` o `ECLIPSE_PKG=apk` (camino Alpine heredado).

Comprobar que el host está listo:

```bash
command -v cargo git make gcc tar wget curl qemu-system-x86_64 mkfs.vfat mcopy gzip apt-get dpkg-deb
rustup show
```

No hace falta `btrfs-progs`: las imágenes btrfs las genera xtask. `cmake`,
OpenCV y FFmpeg solo se necesitan para `cargo opencv` / `cargo ffmpeg`. Hay una
imagen Docker de desarrollo en [`tools/docker`](tools/docker/README.md).

## Compilar

La construcción sigue el [patrón xtask](https://github.com/matklad/cargo-xtask):
`cargo <comando> [--args]` es una abreviatura de
`cargo run --package xtask --release -- <comando> [--args]`. El
[Makefile](Makefile) envuelve los mismos pasos y es lo que usa la CI.

### Comandos de xtask

Muchos comandos preparan primero lo que necesitan:

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
Leyenda: A → B (A depende de B; ejecutar A ejecuta antes B)
```

| Comando | Qué hace | Ejemplo |
|---|---|---|
| `update-all` | Actualiza toolchain, dependencias y submódulos | `cargo update-all` |
| `check-style` | Comprueba que el código compila con varias combinaciones de features | `cargo check-style` |
| `zircon-init` | Descarga los binarios precompilados del modo Zircon ([detalles](prebuilt/README.md)) | `cargo zircon-init` |
| `dump` | Imprime la configuración de build | `cargo xtask dump` |
| `git-proxy` | Pone o quita un proxy para git | `cargo git-proxy --global --port 12345` |
| `rootfs` | Reconstruye el rootfs de Linux de una arquitectura | `cargo rootfs --arch x86_64 [--variant minimal]` |
| `verify-rootfs` | Comprueba que el rootfs tiene lo que el arranque necesita | `cargo verify-rootfs --arch x86_64` |
| `musl-libs`, `opencv`, `ffmpeg` | Copian esas bibliotecas al rootfs | `cargo musl-libs --arch riscv64` |
| `libc-test`, `other-test` | Añaden suites de pruebas al rootfs | `cargo libc-test --arch x86_64` |
| `image` | Genera la imagen del rootfs | `cargo image --arch x86_64 [--variant minimal]` |
| `asm` | Desensambla el núcleo (por defecto en `target/zcore.asm`) | `cargo asm -m virt-riscv64 -o riscv64.asm` |
| `bin` | Genera el binario crudo del núcleo para una máquina | `cargo bin -m nezha -o z.bin` |
| `qemu` | Arranca en QEMU (`--gdb <puerto>` para depurar) | `cargo qemu --arch x86_64 --smp 4` |
| `gdb` | Lanza gdb conectado a un puerto | `cargo gdb --arch riscv64 --port 1234` |
| `linux-libos`, `libos-libc-test` | Ejecutan el modo LibOS en el host | `cargo linux-libos --args /bin/busybox` |

La lista completa y sus opciones: `cargo xtask --help`. Las máquinas válidas para
`-m` están en [`config/machine-features.toml`](config/machine-features.toml).

### Objetivos del Makefile

| Objetivo | Qué hace |
|---|---|
| `make qemu` | Imagen + arranque en QEMU (`ARCH`, `VARIANT`, `SMP`, `DESKTOP`, `KOPTS`, …) |
| `make image` / `make rootfs` | Solo la imagen / solo el rootfs |
| `make iso` | ISO UEFI arrancable de una variante y arquitectura |
| `make release` | Todas las ISO de la matriz variante × arquitectura en `dist/` |
| `make qcow2` / `make img` | Disco virtual / imagen cruda instalada |
| `make vbox` | Arranca la imagen en VirtualBox |
| `make qemu-iso-aarch64` | Arranca la ISO de aarch64 con la receta de memoria correcta |
| `make check`, `make doc` | `cargo check-style` / documentación de rustdoc |
| `make clean`, `make cleanup`, `make clean-everything` | Limpia rootfs e imágenes; además `ignored/target`; además todo `ignored/` (descargas) |

### Variantes e ISO de distribución

Eclipse se publica en dos **variantes**, cada una con su propio rootfs y sus
propios artefactos:

| Variante | Qué lleva | Rootfs | Sesión |
|---|---|---|---|
| `desktop` (por defecto) | labwc/Xorg y todos los paquetes de escritorio de Ubuntu: Mesa, Firefox, XFCE, PulseAudio, freedoom | `rootfs/<arch>` | la de `desktop=` / `/etc/eclipse/desktop`, por defecto `labwc` |
| `minimal` | el sistema base: busybox, red, `eclipse-init`, `install-eclipse` | `rootfs/<arch>-minimal` | consola (`/etc/eclipse/desktop` = `none`) |

`minimal` **no es un recorte** de la de escritorio: es un rootfs que nunca
instala la pila gráfica, así que no hay una lista de qué podar que mantener.

> Hoy `minimal` **no tiene sonido ni zonas horarias con nombre**: ALSA,
> PulseAudio, `mpg123` y `tzdata` están en la lista de paquetes de escritorio.
> Separarlos es una tarea pendiente.

La variante `desktop` **no lleva sufijo** en ninguna ruta, así que
`rootfs/x86_64`, `zCore/x86_64.img` e `ignored/target/efi.img.gz` siguen donde
siempre.

```bash
make release                                  # toda la matriz
make release ARCHS=x86_64                     # las dos variantes de x86_64
make release VARIANTS=minimal                 # minimal en las dos arquitecturas
make iso                                      # desktop, x86_64
make iso VARIANT=minimal                      # minimal, x86_64
```

```text
dist/eclipse-desktop-x86_64.iso
dist/eclipse-minimal-x86_64.iso
dist/eclipse-desktop-aarch64.iso
dist/eclipse-minimal-aarch64.iso
```

Una combinación que falle **no aborta el resto**: `make release` las intenta
todas y termina con un resumen. La CI construye y arranca la ISO minimal de
x86_64 en cada cambio (`scripts/iso-smoke.sh`).

Particularidades por arquitectura:

- **x86_64**: la ISO es live e instalador (`install-eclipse`). Necesita 4 GB de
  RAM en QEMU.
- **aarch64**: la imagen va **enlazada dentro del núcleo** (`LINK_USER_IMG=1`,
  feature `link-user-img`), porque el `rayboot` precompilado no sabe pasar un
  initramfs. Arráncala con `make qemu-iso-aarch64 ARCH=aarch64 VARIANT=minimal`
  y **1 GB** de RAM (con `-m 4G` no imprime nada tras «jump to kernel»). No
  instala: arranca la sesión de consola.
- **riscv64**: sin ISO. QEMU arranca el núcleo directamente, sin UEFI (ver el
  objetivo `iso-unsupported-arch` del `Makefile`).

## Opciones de arranque

En x86_64 la línea de comandos del núcleo está en
[`zCore/rboot.conf`](zCore/rboot.conf) (en la ESP:
`\EFI\Boot\rboot.conf`), en la clave `cmdline`. Las opciones se separan con
`:` y, dentro de un comando, `?` separa el programa de sus argumentos.

| Opción | Efecto | Por defecto |
|---|---|---|
| `INIT=` | Proceso PID 1. Solo se ejecuta si existe. | `/sbin/init` → `eclipse-init` (con el `init` de busybox de respaldo) |
| `SHELL=` | Intérprete de cada terminal virtual (tty1–tty6, PID 101–106) | `/bin/busybox?sh` |
| `ROOTPROC=` | Alias obsoleto de `SHELL=` | — |
| `LOG=` | Nivel de log del núcleo: `error`, `warn`, `info`, `debug`, `trace` | `error` |
| `ROOT=` | Dispositivo raíz; `install-eclipse` lo rellena al instalar | el medio de arranque |
| `desktop=` | Sesión: `labwc`, `xorg` o `none` (manda sobre `/etc/eclipse/desktop`) | `labwc` |

```ini
cmdline=LOG=warn:INIT=/sbin/init:SHELL=/bin/busybox?sh
cmdline=LOG=warn:INIT=/ruta/a/init?--opcion?valor
```

Hay más parámetros por subsistema (NVIDIA, DRM, hunter…), descritos en sus
guías; por ejemplo [`nvidia.*`](docs/README-nvidia-cmdline.md). El proceso de
arranque y cómo medirlo está en [docs/README-boot.md](docs/README-boot.md), y el
init en [docs/README-init.md](docs/README-init.md).

## Plataformas

### x86_64 (QEMU y hardware real)

- **Almacenamiento**: AHCI/SATA (con handoff BIOS/OS, estabilización del enlace
  PHY y Bus Mastering PCI) y NVMe (coherencia de caché DMA con `clflush`).
  Detección de particiones MBR y GPT; cada partición aparece en `devfs`
  (`/dev/sda1`, `/dev/nvme0n1p1`…).
- **Sistemas de archivos**: raíz **btrfs** con driver propio
  (`vendor/btrfs-rs`) y generación de imágenes integrada (sin `btrfs-progs`); se
  expande al tamaño de la partición en el primer montaje y las imágenes pasan
  `btrfs check`. También ext2/ext3/ext4 y vfat/FAT32 (partición EFI).
- **Gráficos**: UAPI DRM/KMS de Linux con PRIME; Wayland (wlroots/labwc, con
  `pixman` si no hay GPU) y Xorg. GPUs NVIDIA con el driver RM, CUDA y una uAPI
  compatible con nouveau para NVK/zink. Ver
  [DRM](docs/README-drm.md), [Xorg](docs/README-xorg.md) y
  [NVIDIA](docs/README-nouveau-uapi.md).
- **Entrada**: teclado PS/2 y USB HID (xHCI), con distribución española completa
  (`ñ`, `@`, `€`, `{`, `}`… con AltGr y Shift).
- **Audio**: Intel HDA con ALSA y PulseAudio ([guía](docs/README-audio.md)).
- **Red**: Intel e1000/e1000e/ixgbe, Realtek y otros ([`drivers/src/net`](drivers/src/net)), con DHCP propio (`edhcpc`).
- **Instalador**: `install-eclipse` escribe el sistema en un disco físico o
  virtual, con detección del tamaño por `sysfs` y `BLKGETSIZE64`, y rellena
  `/etc/fstab` y `rboot.conf`.
- **Memoria**: syscalls de E/S troceadas y carga ELF con `VmObject`s paginados
  para no agotar el heap del núcleo; herramientas de diagnóstico en
  `/proc/memhogs` y [oops](docs/README-oops.md).
- **Seguridad**: [hunter](docs/hunter-security.md), políticas estilo LSM + IDS
  por comportamiento, con log forense en `/proc/hunter` y arranque medido.

### aarch64 (QEMU virt)

`make qemu ARCH=aarch64` o la ISO descrita arriba.

### RISC-V

- **QEMU virt**: `make qemu ARCH=riscv64`.
- **Placas** (la imagen de usuario va enlazada en el núcleo):

| Placa | Comando | Guía |
|---|---|---|
| Allwinner D1 / Nezha | `cargo bin -m nezha -o z.bin` | [README-D1](docs/README-D1.md) |
| StarFive VisionFive | `cargo bin -m visionfive -o z.bin` | [README-visionfive](docs/README-visionfive.md) |
| CVITEK CR1825 | `cargo bin -m cr1825 -o z.bin` | — |
| SiFive FU740 (Unmatched) | `cargo bin -m fu740 -o z.bin` | [README-fu740](docs/README-fu740.md) |
| T-Head C910 | — | [README-C910](docs/README-C910.md) |

## Paquetes (apt)

Eclipse usa **apt/dpkg** con paquetes de **Ubuntu 26.10 (`stonking`)** para el
userspace dinámico. No es Ubuntu completo: el init y las herramientas propias
siguen siendo estáticos.

- En el sistema: `apt-get update && apt-get install <paquete>`.
- En el build: `ECLIPSE_PKG=apt` (por defecto en x86_64 y aarch64) o
  `ECLIPSE_PKG=apk` (camino Alpine heredado; riscv64 sigue en apk).
- El conjunto de escritorio se arma en
  [`xtask/src/linux/apt_rootfs.rs`](xtask/src/linux/apt_rootfs.rs).

## Documentación

El índice completo, con todas las guías en español e inglés, está en
[docs/README.md](docs/README.md). Lo más consultado:

- [Escritorio labwc y apariencias](docs/README-desktop.md)
- [Arranque](docs/README-boot.md) e [init](docs/README-init.md)
- [Compatibilidad con Linux](docs/linux-compat.md) y [con FreeBSD](docs/README-freebsd.md)
- [DRM/KMS](docs/README-drm.md), [Xorg](docs/README-xorg.md), [audio](docs/README-audio.md)
- [hunter](docs/hunter-security.md)
- [Notas para desarrolladores](docs/for-developers.md) y [CI](docs/ci-testing.md)
- [README original de zCore](README-arch.md)

## Licencia

[MIT](LICENSE).
