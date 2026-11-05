# NVIDIA boot parameters

[Español](README-nvidia-cmdline.md) · **English**

The `nvidia.*` command-line flags (`cmdline=` in `zCore/rboot.conf`, separated
by `:`) are parsed in **a single place**,
[`kernel_hal::cmdline::NvidiaParams`](../kernel-hal/src/common/cmdline.rs), and
the kernel publishes them the same way Linux publishes a module's parameters,
under `/sys/module/nvidia/parameters/`.

| Flag | What it does | Linux equivalent |
| --- | --- | --- |
| `nvidia.cuda` | Enables CUDA: turns on both `nvidia.nvidiactl` (`/dev/nvidiactl`, `/dev/nvidia<N>`) and `nvidia.uvm` (`/dev/nvidia-uvm`) at once. Each part set to `=0` stays off. See [README-cuda.en.md](README-cuda.en.md). | loading `nvidia` + `nvidia-uvm` |
| `nvidia.nouveau_uapi` | The DRM node speaks the nouveau uAPI (`DRM_IOCTL_VERSION` = `nouveau` 1.4.0), so Mesa (nouveau/NVK/zink) and labwc run on the GPU. Only enabled if some card can serve it. See [README-nouveau-uapi.en.md](README-nouveau-uapi.en.md). | `nouveau.modeset=1` |
| `nvidia.multigpu` | Declares there is more than one compatible NVIDIA card and that the system must bring them all up, not just the one driving the monitor. | — (Linux probes them all) |
| `nvidia.hwcursor` | The pointer is drawn by the display engine's cursor plane instead of the CPU. | hardware `DRM_MODE_CURSOR` (nouveau's default) |
| `nvidia.nvkms` | Loads nvidia-modeset (nvkms, the same code as `nvidia-modeset.ko`) and hands it every GPU with RM up. It claims each card's display and logs what it sees (the `[nvkms]` block), and on the console card it performs the first modeset: nvkms takes over scanning out the UEFI console, at the same size, on every monitor that has a mode of that size. The desktop looks the same; monitors without that mode are turned off. After that, the compositor's VRAM framebuffers are shown with nvkms flips and the pointer is its cursor plane; system-memory framebuffers are still copied to the console. Excludes the display's own ladder (`nvidia.surfaceflip`, `nvidia.hwcursor`): both want the core channel. See [README-nvkms-port.en.md](README-nvkms-port.en.md). | `nvidia-drm.modeset=1` |
| `nvidia.monitor=BB.DD.F` | Which card drives the monitor: that one becomes `/dev/dri/card0`. PCI address in hex with dots, e.g. `nvidia.monitor=65.00.0` is Linux's `0000:65:00.0` (the `:` already separate the flags). Get the BDF from `dmesg \| grep /dev/dri` or `lspci`. | Xorg's `boot_vga` / `BusID` |
| `nvidia.compute=BB.DD.F` | Which card does compute (SAXPY, NVK EXEC, CE-present). | `CUDA_VISIBLE_DEVICES` |
| `nvidia.wlr_vulkan` / `nvidia.wlr_gles2` / `nvidia.wlr_pixman` | The wlroots renderer for the session, as before. `nvidia.wlr_vulkan` also turns on `nvidia.ctx_per_file`: a process using the GPU through two DRM files at once (labwc with Vulkan has two NVK devices) gets one GPU context per file, like Linux's per-file VM. `nvidia.ctx_per_file=0` turns it off. See [README-nouveau-uapi.en.md](README-nouveau-uapi.en.md). | `WLR_RENDERER` |

Booleans accept the bare form (`nvidia.cuda`) and `=1/true/on/yes` or
`=0/false/off/no`; any other value counts as off.

## `/sys/module/nvidia/parameters`

The same thing `kernel/params.c` does in Linux: one read-only file per
parameter, with a boolean shown as `Y`/`N` and an unset string as `(null)`.

```sh
$ cat /sys/module/nvidia/parameters/nouveau_uapi
Y
$ cat /sys/module/nvidia/parameters/monitor
65.00.0
```

This is what the kernel **applied**, not just what was written: `nouveau_uapi`
reads `N` if no card could serve it even though the flag was present, and
`hwcursor` is what the DRM core will use. That is why a script should ask here
instead of running `grep nvidia.x /proc/cmdline`, which says yes to
`nvidia.x=0`.

Example for two RTX cards with the monitor on the one at `65:00.0`:

```
cmdline=LOG=warn:nvidia.cuda:nvidia.nouveau_uapi:nvidia.multigpu:nvidia.hwcursor:nvidia.monitor=65.00.0:ROOT=/dev/sda2
```
