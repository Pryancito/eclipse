# Baseline reproducible para integración NVIDIA (Eclipse OS)

Este flujo estandariza cómo capturar evidencia cuando hay fallos de integración
gráfica (arranque de GPU, GSP-RM, KMS/DRM, render node y userspace).

## 1) Prerrequisitos

- Build bare-metal x86_64 con el submódulo NVIDIA presente:
  - `nvidia-rm-sys/vendor/open-gpu-kernel-modules`
  - Si falta: `git submodule update --init --recursive`
- Firmware GSP en rootfs: `/lib/firmware/nvidia/gsp/gsp.bin`
- Arrancar con logs visibles (`LOG=warn` o `LOG=error`).

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

Cuando la GPU de cómputo es la misma que respalda `card0` (lo normal con dos
NVIDIA), `card1`/`renderD129` llevan en sysfs un BDF sintético
(`0000:ee:00.0`, vendor `0x0000`) para que libdrm no los fusione con `card0`.
Ese alias tiene clase PCI `0x120000` (acelerador de proceso), **no** clase
display: los enumeradores de GPU (`fastfetch`, `lspci`) cuentan toda entrada
de clase `0x03` en `/sys/bus/pci/devices`, y con clase display salía una
tercera GPU inexistente ("Unknown Device 0000").

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
