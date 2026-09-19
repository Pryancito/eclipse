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
más). Para cualquier otra versión hay que generarla con el script que el
propio submódulo trae:

```sh
nvidia-rm-sys/vendor/open-gpu-kernel-modules/nouveau/extract-firmware-nouveau.py \
  -i nvidia-rm-sys/vendor/open-gpu-kernel-modules -o /tmp/gspfw -d
export ECLIPSE_GSP_BIN=/tmp/gspfw/nvidia/tu102/gsp/gsp-<version>.bin
```

`-d` descarga el `.run` que toca desde `download.nvidia.com` (varios cientos
de MB) y extrae `gsp_tu10x.bin`. NVIDIA documenta este uso en
`nouveau/extract-firmware-nouveau.txt`.

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
