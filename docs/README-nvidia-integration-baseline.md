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
