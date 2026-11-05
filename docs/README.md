# Documentación de Eclipse OS

**Español** · [English](README.en.md)

Índice de la documentación. Las guías están en español y en inglés: cada una
lleva arriba un enlace para cambiar de idioma. El punto de partida es el
[README principal](../README.md).

## Arranque y sistema

- [El arranque de Eclipse: cómo se mide](README-boot.md)
- [`eclipse-init`: qué hace y qué le falta](README-init.md)
- [Contención de fallos del kernel (oops)](README-oops.md)
- [Diseño de `mmap` con escritura a fichero, `msync` y caché de páginas compartida](mmap-page-cache-design.es.md)
- [hunter: subsistema de seguridad del kernel](hunter-security.es.md)

## Compatibilidad

- [Compatibilidad con Linux guiada por `Documentation/`](linux-compat.md)
- [Compatibilidad con FreeBSD (ABI FreeBSD/amd64)](README-freebsd.md)
- [D-Bus](README-dbus.md)

## Escritorio, gráficos y audio

- [Escritorio labwc](README-desktop.md)
- [Ejecutar un servidor X (`startx`)](README-xorg.md)
- [DRM / KMS: conformidad con la UAPI de Linux](README-drm.md)
- [`WAIT_VBLANK` bloqueante y la restricción de `ioctl` síncrono](README-async-ioctl-vblank.es.md)
- [Audio: HD Audio, HDMI de NVIDIA, PulseAudio y `/dev/dsp`](README-audio.es.md)

## NVIDIA

- [Parámetros de arranque NVIDIA](README-nvidia-cmdline.md)
- [uAPI compatible con nouveau (NVK/zink)](README-nouveau-uapi.md)
- [Aceleración NVIDIA: camino a rendimiento tipo Linux](README-nvidia-perf.md)
- [Diseño: pila gráfica NVIDIA por GPU](README-nvidia-multigpu-diseno.md)
- [Plan: port de nvidia-modeset (nvkms)](README-nvkms-port.md)
- [CUDA](README-cuda.md)

## Desarrollo y placas

- [Notas para desarrolladores](for-developers.md)
- [Tests de CI y diagnóstico del kernel](ci-testing.es.md)
- [Placa T-HEAD C910 Light (riscv64)](README-C910.md)
- [Placa Allwinner D1 (riscv64)](README-D1.md)
- [Placa SiFive fu740 (riscv64)](README-fu740.md)
- [Placa StarFive VisionFive (riscv64)](README-visionfive.md)

## Herramientas

Cada herramienta de `tools/` tiene su propio README en los dos idiomas, por
ejemplo [eclipse-bench](../tools/eclipse-bench/README.es.md),
[SilverSurfer](../tools/SilverSurfer/README.md) o
[Docker](../tools/docker/README.es.md). También [rboot](../rboot/README.es.md)
y los [binarios precompilados](../prebuilt/README.es.md).

## Informes fechados

Estos documentos son informes, auditorías y mediciones de un momento concreto.
Se mantienen en el idioma en que se escribieron y no se traducen: describen el
estado del código en su fecha, y puede que algunos datos ya no sean actuales.

- [Benchmarks de referencia](README-benchmarks.md) (inglés)
- [Rendimiento frente a Linux: medición y correcciones](README-performance.md) (español)
- [Auditoría de rendimiento (agosto 2026)](README-performance-audit-2026-08.md) (español)
- [Revisión del apartado gráfico (septiembre 2026)](README-graphics-review-2026-09.md) (español)
- [Baseline reproducible de integración NVIDIA](README-nvidia-integration-baseline.md) (español)
- [NVK (Vulkan) en la RTX real: estado](README-nvk-hardware-status.md) (español)
- [Reproductor de la corrupción "intermitente" del kernel](README-crash-repro.md) (inglés)
- [Caza de fugas de memoria](README-memory-leaks.md) (inglés)
- [e1000e: auditoría de bugs (2026-08-06)](README-e1000e-bugs.md) (español)
- [Tests del core y SMP x64 (2026-09-06)](core-tests-smp-20260906.md) (inglés)
- [hunter: informe de endurecimiento](hunter-hardening.md) (inglés)
- [Port de zCore a RiscV64 (QEMU)](porting-rv64.md) (inglés)
