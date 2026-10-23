# Baseline reproducible para integración NVIDIA (Eclipse OS)

Este flujo estandariza cómo capturar evidencia cuando hay fallos de integración
gráfica (arranque de GPU, GSP-RM, KMS/DRM, render node y userspace).

## 1) Prerrequisitos

- Build bare-metal x86_64 con el submódulo NVIDIA presente:
  - `nvidia-rm-sys/vendor/open-gpu-kernel-modules`
  - Si falta: `git submodule update --init --recursive`
- Firmware GSP en rootfs: `/lib/firmware/nvidia/gsp/gsp.bin`
- Arrancar con logs visibles (`LOG=warn` o `LOG=error`).

### La versión del firmware GSP debe coincidir EXACTAMENTE

`_kgspFwContainerVerifyVersion` (kernel_gsp.c) compara la sección
`.fwversion` de `gsp.bin` con el `NV_VERSION_STRING` del RM vendorizado y
devuelve `NV_ERR_INVALID_DATA` si difieren en un solo byte. No hay
compatibilidad entre versiones cercanas: la ABI de GSP-RM no es estable.

`xtask` ya no lleva la versión escrita a mano: la lee de
`nvidia-rm-sys/vendor/open-gpu-kernel-modules/version.mk`, así que al
re-pinchar el submódulo el firmware le sigue solo. Busca la imagen en este
orden: caché local (`ignored/nvidia-gsp-cache`), `ECLIPSE_GSP_BIN`,
linux-firmware, y por último el extractor oficial de NVIDIA.

Ojo: `linux-firmware` **solo publica las versiones que soporta Nouveau**
(535.113.01 y 570.144, ni en el repo de NVIDIA ni en el de kernel.org hay
más). Para cualquier otra versión hay que generarla, y de eso se encarga:

```sh
cargo nvidia-firmware              # la deja en ignored/nvidia-gsp-cache/
cargo nvidia-firmware --out x.bin  # y además la copia ahí
cargo nvidia-firmware --force      # ignora la caché y la vuelve a obtener
```

Por dentro ejecuta el `nouveau/extract-firmware-nouveau.py` de NVIDIA, que
descarga el `.run` que toca desde `download.nvidia.com` (varios cientos de
MB) y extrae `gsp_tu10x.bin`. NVIDIA documenta este uso en
`nouveau/extract-firmware-nouveau.txt`.

Usa el script de `origin/main`, no el del tag fijado, porque NVIDIA avisa de
que el tag trae una copia desfasada — y 580.178.04 es justo un caso: su
script busca `<sym>_image_prod_data` mientras que su propio bindata declara
`<sym>_BINDATA_LABEL_IMAGE_PROD_data`, y muere con «array
kgspBinArchiveBooterLoadUcode_TU102_image_prod_data not found».

Si la máquina no llega a `download.nvidia.com`, genera el fichero donde sí
llegue y tráelo:

```sh
# en una máquina con salida
cargo nvidia-firmware --out gsp-<version>.bin
# aquí
ECLIPSE_GSP_BIN=gsp-<version>.bin cargo nvidia-firmware
```

Solo se instala la imagen de Turing (`gsp_tu10x.bin`, que cubre
TU102/TU104/TU106/TU116/TU117). Ampere en adelante necesita la otra imagen
del mismo instalador (`gsp_ga10x.bin`) y que el kernel elija entre las dos
por familia de chip; hoy `zCore` lee una sola ruta.

## 2) Captura rápida (recomendada)

En hardware real:

```sh
nvidia-baseline
```

Genera un paquete en `/tmp/nvidia-baseline-<timestamp>` con:

- `/proc/cmdline`
- `/proc/gpubaseline` (resumen consolidado por GPU)
- `/proc/gpudbg`
- `/proc/gpuedid`
- `/proc/gpusnd`
- `/proc/gpusurvive`
- `dmesg`
- `vulkaninfo` y `drmbench` si están instalados

Para incluir etapas activas de bring-up (más riesgo, más señal):

```sh
RUN_GPU_STEPS=1 nvidia-baseline
```

Esto añade `gpuinit`, `gpustep15`, `gpustep17` y `gpustep23`.

## 2b) GPU de cómputo (nodos + userspace)

En una caja con dos NVIDIA, la que **no** lleva el framebuffer GOP es la GPU
de cómputo: GSP-RM arranca sola, el canal `TURING_COMPUTE_A` se monta en el
boot, y el present CE/P2P copia frames al GOP.

Nodos DRM extra (nombre de driver `eclipse-compute`, para que Mesa/NVK los
ignoren; labwc sigue en `card0`):

- `/dev/dri/card1` y `/dev/dri/renderD129`
- `/sys/class/drm/card1`, `renderD129`
- `/proc/gpuroles` — consola vs cómputo, BDF y si el RM está atado

Pin opcional en cmdline (hex con puntos; `:` ya separa tokens):

```txt
nvidia.compute=65.00.0
```

Lanzar SAXPY desde userspace (el ISO trae `/bin/ecl-compute` y
`/bin/ecl-vkcompute`):

```sh
ecl-compute info          # GPU, RM, nouveau_uapi=, exec_fast=
ecl-compute saxpy         # canario kernel (SASS embebido, ioctl card1)
ecl-compute bench
ecl-vkcompute             # canario Vulkan: vkCmdDispatch + NAK en card0
```

`ecl-compute` habla el ioctl `DRM_COMMAND_BASE+0x50` (`nr=0x90`) sobre el
nodo `eclipse-compute` (card1 / renderD129). Mesa/NVK **no** usan ese nodo:
un dispatch Vulkan va a **card0** (`DRM_IOCTL_VERSION` nombre `nouveau`).
`ecl-vkcompute` es dinámico (`dlopen libvulkan.so.1`); no hay SDK Vulkan en
el build, el SPIR-V SAXPY está embebido.

`cat /proc/gpustep23` sigue existiendo como canario.

`GEM_NEW` con solo `DOMAIN_VRAM` (sin GART) reserva `NV01_MEMORY_LOCAL_USER`
de verdad y no publica mmap CPU. GART y `GART|VRAM` siguen en sysmem
(HOST_VISIBLE). El wait del CE-present corre **fuera** de los locks RM y de
`RmGate`; un fallo sigue latiendo `CE_PRESENT_WEDGED` y cae al blit CPU.

## 2c) El userspace de NVIDIA (`nvidia-smi`, NVML, `libcuda`)

Los clientes de verdad de `/dev/nvidiactl` (la escotilla `nvidia.nvidiactl`
de la cmdline) son los binarios de NVIDIA. `nvidia.cuda` enciende de una vez
todas las escotillas que necesita CUDA: hoy `nvidia.nvidiactl` y
`nvidia.uvm` (`/dev/nvidia-uvm`). Cada una sigue valiendo por su nombre, y
su `=0` la deja apagada aunque esté `nvidia.cuda`. `cargo image --arch x86_64` los
mete en la imagen sin pedirlos; para dejarlos fuera:

```sh
ECLIPSE_NVIDIA_USERSPACE=0 cargo image --arch x86_64 --variant <variante>
```

Apagado también quita los que una construcción anterior dejó en la raíz,
porque la raíz nunca se vacía. Van en la raíz instalada (`rootfs.btrfs.gz`) y
nunca en la imagen que arranca QEMU: la de `minimal` no lleva `usr/lib` ni
`usr/bin`, y de la de `desktop`, que los copia enteros, se quitan, porque QEMU
no tiene GPU NVIDIA y `libcuda` con su JIT de PTX son 130 MiB de initramfs en
RAM.

Instala `/usr/bin/nvidia-smi` y, en `/usr/lib/x86_64-linux-gnu`,
`libnvidia-ml`, `libcuda` y `libnvidia-ptxjitcompiler`, con sus enlaces de
soname. Tienen que ser **de la misma versión exacta que el RM**: abren con
`NV_ESC_CHECK_VERSION_STR` en modo estricto, así que la versión se lee del
`version.mk` del submódulo, como la del firmware GSP.

Salen del archivo de Ubuntu (`archive.ubuntu.com`, paquetes
`libnvidia-compute-<rama>` y `nvidia-utils-<rama>`), que lleva los binarios
de NVIDIA tal cual, los mismos del `.run`. Como son glibc y la imagen es
musl, también se instala la glibc de Ubuntu 26.10 (`libc6` y `libgcc-s1`,
fijados por nombre y SHA-256): el cargador en `/lib64/ld-linux-x86-64.so.2`
y las bibliotecas en `/usr/lib/x86_64-linux-gnu`, que ese cargador mira
antes que `/usr/lib`, donde están las de musl. Una raíz que ya trae su
propio cargador glibc se queda con su glibc entera.

Los paquetes quedan en `ignored/nvidia-userspace-cache`. En una máquina sin
salida a `archive.ubuntu.com` basta con dejarlos ahí a mano; si no están, la
construcción avisa y sigue sin ellos. Para desempaquetar hace falta `tar` de
GNU con `zstd`, y `sha256sum`.

Probarlo, arrancando con `nvidia.cuda` (o solo `nvidia.nvidiactl` para
`nvidia-smi`):

```sh
nvidia-smi
nvidia-smi -L
dmesg | grep nvidiactl
```

Sin GPU NVIDIA (QEMU) `nvidia-smi` dice «Failed to initialize NVML: GPU
access blocked by the operating system»: no hay núcleo RM y las escapadas
dan EINVAL. Para verlo en QEMU hay que meter la raíz entera en su imagen a
mano, antes de `make build`:

```sh
cargo xtask mk-sfs --dir rootfs/x86_64-minimal --image zCore/x86_64-minimal.img --size 629145600
```

CUDA se prueba paso a paso con `cuda-probe`. Cuando un paso falla,
`dmesg | grep -E 'nvidia|mmap'` nombra la llamada de NVIDIA que el núcleo aún
no sirve, o el `mmap` que rechazó, con cualquier `LOG`: un
`CUDA_ERROR_OUT_OF_MEMORY` (2) de `cuInit` es libcuda sin poder reservar su
espacio de direcciones. Arrancando además con `BOOTTRACE=cuda-probe`,
`/proc/bootprofile` lista cada llamada de `cuda-probe` que falló, con su
errno: es lo que hay detrás de un `CUDA_ERROR_OPERATING_SYSTEM` (304).

```sh
cuda-probe
dmesg | grep -E 'nvidia|mmap'
grep -E 'FAIL| e[0-9]' /proc/bootprofile
```

## 3) Semántica de render node (despliegue seguro)

Por defecto, `renderD128` mantiene modo compatibilidad (observa y registra
ioctls fuera de `DRM_RENDER_ALLOW`).

Para forzar semántica Linux estricta en render node (EACCES fuera del set):

```txt
drm.render_strict
```

en la cmdline del kernel.

## 4) Fallback operativo

- Si falla la ruta NVIDIA acelerada, mantener compositor y scanout en ruta
  software estable.
- Usar la evidencia de `gpubaseline` + `dmesg` para ubicar el primer punto de
  ruptura antes de habilitar más flags.
