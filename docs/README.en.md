# Eclipse OS documentation

[Español](README.md) · **English**

Index of the documentation. The guides are in English and Spanish: each one
has a link at the top to switch language. The starting point is the
[main README](../README.en.md).

## Boot and system

- [How Eclipse's boot is measured](README-boot.en.md)
- [`eclipse-init`: what it does and what it lacks](README-init.en.md)
- [Kernel fault containment (oops)](README-oops.en.md)
- [Design: file-backed `mmap` writeback, `msync` and a shared page cache](mmap-page-cache-design.md)
- [hunter: the in-kernel security subsystem](hunter-security.md)

## Compatibility

- [Linux compatibility driven by `Documentation/`](linux-compat.en.md)
- [FreeBSD compatibility (FreeBSD/amd64 ABI)](README-freebsd.en.md)
- [D-Bus](README-dbus.en.md)

## Desktop, graphics and audio

- [The labwc desktop](README-desktop.en.md)
- [Running an X server (`startx`)](README-xorg.en.md)
- [DRM / KMS: conformance with the Linux UAPI](README-drm.en.md)
- [Blocking `WAIT_VBLANK` and the sync-`ioctl` constraint](README-async-ioctl-vblank.md)
- [Audio: HD Audio, NVIDIA HDMI, PulseAudio and `/dev/dsp`](README-audio.md)

## NVIDIA

- [NVIDIA boot parameters](README-nvidia-cmdline.en.md)
- [nouveau-compatible uAPI (NVK/zink)](README-nouveau-uapi.en.md)
- [NVIDIA acceleration: the road to Linux-like performance](README-nvidia-perf.en.md)
- [Design: a per-GPU NVIDIA graphics stack](README-nvidia-multigpu-diseno.en.md)
- [Plan: porting nvidia-modeset (nvkms)](README-nvkms-port.en.md)
- [CUDA](README-cuda.en.md)

## Development and boards

- [Notes for developers](for-developers.en.md)
- [CI tests and kernel diagnostics](ci-testing.md)
- [T-HEAD C910 Light board (riscv64)](README-C910.en.md)
- [Allwinner D1 board (riscv64)](README-D1.en.md)
- [SiFive fu740 board (riscv64)](README-fu740.en.md)
- [StarFive VisionFive board (riscv64)](README-visionfive.en.md)

## Tools

Every tool under `tools/` has its own README in both languages, for example
[eclipse-bench](../tools/eclipse-bench/README.md),
[SilverSurfer](../tools/SilverSurfer/README.en.md) or
[Docker](../tools/docker/README.md). So do [rboot](../rboot/README.md) and the
[prebuilt binaries](../prebuilt/README.md).

## Dated reports

These documents are reports, audits and measurements from a specific point in
time. They stay in the language they were written in and are not translated:
they describe the code as it was on their date, and some details may be out of
date.

- [Reference benchmarks](README-benchmarks.md) (English)
- [Performance against Linux: measurement and fixes](README-performance.md) (Spanish)
- [Performance audit (August 2026)](README-performance-audit-2026-08.md) (Spanish)
- [Graphics review (September 2026)](README-graphics-review-2026-09.md) (Spanish)
- [Reproducible NVIDIA integration baseline](README-nvidia-integration-baseline.md) (Spanish)
- [NVK (Vulkan) on the real RTX: status](README-nvk-hardware-status.md) (Spanish)
- [Reproducer for the "intermittent" kernel corruption](README-crash-repro.md) (English)
- [Memory-leak hunt](README-memory-leaks.md) (English)
- [e1000e: bug audit (2026-08-06)](README-e1000e-bugs.md) (Spanish)
- [Core tests and x64 SMP (2026-09-06)](core-tests-smp-20260906.md) (English)
- [hunter: adversarial hardening report](hunter-hardening.md) (English)
- [Porting zCore to RiscV64 (QEMU)](porting-rv64.md) (English)
