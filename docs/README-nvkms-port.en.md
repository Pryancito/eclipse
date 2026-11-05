# Plan: porting nvidia-modeset (nvkms) to Eclipse

[Español](README-nvkms-port.md) · **English**

A continuation of [README-nvidia-multigpu-diseno.md](README-nvidia-multigpu-diseno.md).
Decision of 27 Oct 2026: the modeset of every card is done by **nvkms, the same
code Linux uses**, not by a modeset written by hand in the shim.

Reference hardware: 2× RTX 2060 SUPER (Turing, TU106, display C5/NVC57x).

---

## 1. Why it is needed

Today the display engine ladder in `nvidia-rm-sys/vendor/eclipse_rm_init.c`
(core channel NVC57D, window NVC57E, cursor NVC57A) **reuses the head the UEFI
left on**. It never programs raster, SOR or link training. Only the boot card has
that head, so:

- the second card cannot turn its monitor on;
- a mode change on the first one (another resolution, another refresh rate) is
  not real either: only what is copied changes, not what is scanned out.

In Linux all of that is done by `nvidia-modeset.ko`: one `NVDevEvoRec` per GPU,
with its heads, SORs, DisplayPort (SST/MST, link training), HDMI (InfoFrames,
FRL), hardware flips, cursor and LUT. `nvidia-drm.ko` uses it through the
`nvkms-kapi` interface.

## 2. What is in the vendored tree

All in `nvidia-rm-sys/vendor/open-gpu-kernel-modules` (580.178.04, MIT):

| Part | Path | Size | Notes |
|---|---|---|---|
| nvkms | `src/nvidia-modeset/src` | ~114k lines of C | official list in `src/nvidia-modeset/srcs.mk` (192 `.c`) |
| kapi | `src/nvidia-modeset/kapi/src` | ~5k lines of C | what `nvidia-drm` uses; built with nvkms |
| DisplayPort | `src/common/displayport` + `src/nvidia-modeset/src/dp` | ~40k lines of C++ | 35 `.cpp` in `SRCS_CXX` |
| common modeset | `src/common/modeset` (timing, HDMI packet) | ~31k | already in `srcs.mk` |
| softfloat, push, headsurface, xz | `src/common/...` | — | already in `srcs.mk` |
| Linux layer | `kernel-open/nvidia-modeset/nvidia-modeset-linux.c` | ~2.2k | **not built**: it is what we replace |

What nvkms needs from its environment:

1. **OS hooks**: ~50 `nvkms_*` functions declared in
   `src/nvidia-modeset/os-interface/include/nvidia-modeset-os-interface.h`
   (memory, copyin/copyout, timers, semaphores, `nvkms_get_usec`,
   `nvkms_usleep`, logging, refcount, `nvkms_open_gpu`/`close_gpu`,
   `nvkms_enumerate_gpus`, `nvkms_call_rm`, module parameters). In Linux they
   are implemented by `nvidia-modeset-linux.c`.
2. **The RM**: `nvkms_call_rm` ends up in `rm_kernel_rmapi_op`
   (`src/nvidia/arch/nvalloc/unix/src/osapi.c`), a `switch` over
   `nvidia_kernel_rmapi_ops_t` that calls `Nv04AllocWithAccessKernel`,
   `Nv04ControlKernel`, `Nv04MapMemoryKernel`… Those functions belong to the RM
   core and **we already build them**. The `switch` is in the unix layer we
   filter out, so we copy it into our shim as we did with `RmIoctl` in
   `eclipse_rm_frontend.c`.
3. **RM → nvkms callbacks** (`nvidia_modeset_callbacks_t`): hotplug, vblank and
   flip-done events. In Linux they arrive through `set_callbacks`.
4. **A minimal C++ runtime**: the DP library is built with `-fno-rtti
   -fno-exceptions -fcheck-new`. `operator new/delete` are defined by
   `dp_object.h` on top of `nvkms_alloc`, and `__cxa_pure_virtual` comes from
   `nvkms-utils.c`. It needs checking whether any global objects with
   constructors (`.init_array`) are left, and adding `-fno-threadsafe-statics` if
   `__cxa_guard_*` appear.

## 3. How it fits into Eclipse

```
linux-object (DRM core, cardN)            ← already per card (PRs #2190, #2192)
    │  KMS ioctls
drivers/src/display/nvidia.rs             ← NvidiaGpu: one nvkms device per GPU
    │  FFI (new module nvidia-rm-sys/src/nvkms.rs)
nvkms-kapi  →  nvkms  →  nvkms_call_rm  →  rm_kernel_rmapi_op (shim)  →  RM
                  ↑ nvkms_* hooks (vendor/eclipse_nvkms_os.c + Rust)
```

- Just like `nvidia-drm` in Linux, Eclipse's driver talks to nvkms only through
  **kapi** (`nvKmsKapiGetFunctionsTable`): `allocateDevice`, `grabOwnership`,
  `getDeviceResourcesInfo`, `getDisplays`, `getConnectorInfo`,
  `applyModeSetConfig`, `allocateMemory`/`importMemory`, `createSurface`,
  events.
- One `NvKmsKapiDevice` per `NvidiaGpu`. Nothing global: it is what the
  multi-GPU design asks for.
- The current ladder (hwflip/hwcursor/our own core) and nvkms **cannot coexist
  on the same GPU**: both allocate the core channel. It is chosen per GPU with a
  boot parameter, `nvidia.nvkms=1`, and the ladder stays as a fallback until
  nvkms reaches parity on real hardware.
- Soft-KMS (blitting to the UEFI framebuffer) does not change: it is still the
  path when there is no driver with hardware KMS.

## 4. PRs

Each one small, reviewable and without breaking the desktop that already works.

**PR 1: nvkms builds inside the kernel, not enabled.**
- `build.rs`: a second `cc::Build` for `src/nvidia-modeset/srcs.mk` (reusing the
  parser in `src/srcs_mk.rs`, extended to `SRCS_CXX`), with the `-I`/`-D` from
  nvidia-modeset's `Makefile` and our kernel flags (`-fPIC`, no SSE). The `.cpp`
  files with `-std=gnu++11 -fno-rtti -fno-exceptions -fcheck-new
  -fno-threadsafe-statics`.
- Headsurface shaders (`src/shaders/g_<arch>_shaders`, from maxwell to gb20x,
  Turing included) compressed with xz and embedded as read-only objects, as the
  `Makefile` does. Headsurface is only used for in-kernel composition (rotation,
  swapgroups); it does not run in a normal modeset, but the code references it.
- `nvkms_*` hooks as stubs returning an error, in `vendor/eclipse_nvkms_os.c`,
  so that it links.
- Check: `cargo bin -m virt-x86_64` links and `nm` shows
  `nvKmsKapiGetFunctionsTable`. Nobody calls it yet.

**PR 2: OS hooks and connection to the RM.**
- `rm_kernel_rmapi_op` and `nvidia_get_rm_ops` in the shim (copied from
  `osapi.c` / `nv-modeset-interface.c`), on top of the RM's kernel client.
- Real hooks: memory on the kernel allocator, timers on `kernel_hal::timer`,
  semaphores, `nvkms_get_usec`, logging with `klog`.
- Host tests for the Rust part (timers, refcount, copyin/out).

**PR 3: `nvidia.nvkms` loads nvkms and hands it every GPU, with no modeset.**
- The three `OS_UNIX` controls without which `nvAllocDevEvo` does not allocate
  the device, until now stubs returning `NOT_SUPPORTED`, move into the shim
  (`vendor/eclipse_nvkms_rm.c`) as in `unix_console.c` and `dynamic-power.c`:
  `VT_GET_FB_INFO` answers with the UEFI console (Linux's `screen_info`, which
  the driver registers at boot) on the GPU whose BAR1 starts at it; `VT_SWITCH`
  and the GC6 refcount accept without doing anything, because there is no VGA
  console to give back here, and no GC6.
- `nvKmsModuleLoad` once, under `nvkms_lock`.
- A kapi client in C (`vendor/eclipse_nvkms_drv.c`, half of `nvidia-drm`):
  `allocateDevice` per GPU and a query of heads, connectors, displays, EDID and
  modes that the driver writes to the log.
- A boot task waits for the console GPU's deferred bring-up, loads nvkms on each
  card and then services its timers.
- nvkms and our own ladder (`surfaceflip`/`hwcursor`) exclude each other per
  GPU: whoever arrives first keeps the core channel.
- Like `nvidia-drm` before its first commit, **the screen does not change**:
  nvkms keeps scanning out what the UEFI left until a mode is applied.

**PR 4: modeset of the boot card.**
- PR 4a: the console memory (`NV01_MEMORY_FRAMEBUFFER_CONSOLE`, the same one
  nvkms uses to restore the console in `nvRmImportFbConsoleMemory`) as a kapi
  surface, and the first `applyModeSetConfig`, like `nvidia-drm`'s first atomic
  commit: each connected monitor on the boot card, on its own head, at a mode the
  size of the console (the preferred one if it fits, otherwise the one closest to
  60 Hz), with the unscaled console as the primary plane; the other heads are
  turned off. It is validated (`commit = false`) before being applied. The
  desktop looks the same, because the CPU and the CE keep painting the same
  memory, but nvkms is now scanning it out. A monitor without a mode of that size
  stays off and the log says so.
- PR 4b: nvkms flips for VRAM GEMs and the cursor through kapi.
  - Each of the compositor's VRAM fbs becomes a kapi surface: its GEM is
    duplicated (`NvRmDupObject`) from the RM client of the context that
    allocated it into kapi's client, which is what `nvidia-drm`'s import by fd
    ends up doing. It is wrapped as `ImportMemory` does.
  - `page_flip` is a kapi flip on the configuration the modeset left: only the
    primary plane's surface changes, and kapi turns it into
    `NVKMS_IOCTL_FLIP`. Like `nvidia-drm`, no flip is queued while the previous
    one has not landed (`getFlipPendingStatus`); it waits up to 50 ms.
  - An fb nvkms cannot scan out (sysmem, PRIME) is still copied to the console
    with the CPU or the CE, and before that the console is put back on screen.
    When the GEM of an on-screen fb is closed, the console comes back and the
    flip is waited for before the surface is released.
  - The cursor is nvkms's cursor plane: a 64x64 A8R8G8B8 surface in VRAM,
    allocated by kapi and written through the RM's BAR1 transfer. Every move is a
    flip that only touches the cursor. `nvidia.nvkms` enables the DRM layer's
    hardware cursor path.
- PR 4c: the console region is described to the RM after the GSP boots.
  - On the reference hardware the RM had no console region, so
    `NV01_MEMORY_FRAMEBUFFER_CONSOLE` failed and there was no modeset.
  - Reserving it before boot, like Linux (`nvidia.console_preserve`, now
    removed), moves the BAR1 window the console is painted through. Without
    nvkms the screen freezes; with nvkms it goes black.
  - Now, right before `allocateDevice`, the shim describes the UEFI framebuffer
    as the reserved console memdesc, like `memmgrAllocateConsoleRegion`: VRAM from
    offset 0, because without the reservation BAR1 is still a linear window.
    Neither the FB layout nor BAR1 changes.
  - nvkms imports and scans out the same memory the CPU and the CE paint into.
- PR 4d: the default LUT is checked in VRAM before the modeset.
  - On the reference hardware the console modeset came out black with signal,
    with 4c and with `nvidia.console_preserve`, and without a screen there was no
    way to read the log.
  - On Turing, while nobody sets a LUT (nvidia-drm's first commit does not, nor
    does `show_console`), each window's ILUT and each head's OLUT come from
    nvkms's default LUT surface (`nvEvoInitDefaultLutC5`). nvkms fills it with
    identity ramps through the CPU, over BAR1. If those writes do not reach VRAM,
    every pixel comes out black.
  - Now, right before the modeset, the shim (`eclipse_nvkms_check_vidmem`) reads
    that surface through BAR0's PRAMIN window, which does not go through BAR1,
    and compares it with what the CPU sees. Whatever does not match is rewritten
    through the same window. If the CPU reads zeros or the rewrite does not fix
    it, there is no modeset and the screen stays as the firmware left it, with
    the reason in the log.
  - The log line also gives BAR1's real mode (`BAR1_BLOCK`), how the RM treats
    it and a sample of the console read through BAR1's base and in VRAM 0.
- PR 4e: the LUT is repaired from a reference, not from what the CPU reads.
  - With 4d the screen went from black to white. The repair copied to VRAM what
    the CPU read from its BAR1 mapping. A BAR1 address with nothing behind it
    reads as `0xffffffff`, and a LUT like that drives every pixel to the maximum.
  - Now `eclipse_kms_default_lut` generates the correct LUT in kernel memory with
    nvkms's own `InitDefaultLut`, pointed at that buffer for one call. VRAM is
    compared with that buffer and repaired from it. What the CPU reads is only
    counted: how many words it reads correctly and how many as `0xffffffff`, for
    the next fix, which is the BAR1 mapping itself.
  - On the reference hardware the reference came out as zeros: nvkms's fill
    (`EvoSetupIdentityBaseLutC5` and the output one) skips a LUT already marked as
    identity, and the default one is marked so from the moment the device is
    created. Now the marks are removed only during that call and put back
    afterwards. The same log showed a virtual BAR1 (`BAR1_BLOCK 0x801f74c0`) and
    0/256 console words equal through BAR1's base and in VRAM 0: BAR1's base is no
    longer the VRAM 0 the display scans out and the CE present writes.
  - What explains both things, and why NVK runs nothing on that card: the same
    boot with `nvidia.nouveau_uapi` read its channels' USERD as
    `0xBAD0ACEF`/`0xBAD0ACF0`, the GPU's error sentinel. Everything going through
    BAR1 on the console card returns that: the channel looks full
    (`NV_ERR_BUSY_RETRY` in the SELFTEST) and the golden context times out
    (`0x65`). In Linux, `kbusInitBar1` maps the console at BAR1 0 before putting
    BAR1 in virtual mode (`bPreserveBar1ConsoleEnabled`, for efifb); here nothing
    was mapped there, the kernel console kept writing to BAR1's base, and the BAR1
    MMU fault that causes can only be recovered by rebinding BAR1 from the RM's
    fault handler, which Eclipse does not run.
    `eclipse_rm_preserve_bar1_console` describes VRAM 0 as the console and enables
    that mapping before `gpuStateLoad`, without `PRIMARY_DEVICE` or the pre-GSP
    reservation of `nvidia.console_preserve`. The LUT check now reports the first
    word the CPU reads and whether it is that sentinel. With `nvidia.wlr_pixman`
    (without nouveau_uapi) the desktop already comes out through nvkms on
    hardware.
  - With that mapping the console card stays the same: the SELFTEST that passed
    was the other card's, and on the console one the USERD still reads as
    `0xBAD0ACxx` as soon as its bring-up finishes. `eclipse_rm_bar1_recover` does
    the BAR-fault branch of `kgmmuServicePriFaults_GV100`: if the MMU has a
    latched fault (or a discarded BAR fault behind it), it rebinds BAR1, clears it
    and leaves the status, address, instance block and `FAULT_INFO` in dmesg. It
    runs at the end of `gpustep9` and before the SELFTEST; the LUT check does the
    same and reports what the CPU reads after the rebind.
  - With that, dmesg showed a CPU write fault on BAR1 0x80000 (inside the
    console) that the GSP had already cleared, and a rebind that never finishes.
    The hardware requires that no BAR1 access arrives while the bind completes
    (`kbusBar1InstBlkBind_TU102`), and during the deferred bring-up the kernel
    console kept painting into BAR1 (the RM's narration with
    `console.overgraphics`, and the pending presents from the 250 Hz tick).
    `kernel_hal::console::hold_presents_for` keeps the console in its shadow
    buffer during that window, with the same limit as the scanout pause.
    `eclipse_rm_bar1_recover` now always reports `BIND_STATUS`, whether the RM
    mapped the console (`bBar1ConsolePreserved`) and VRAM 0 read through BAR1
    versus PRAMIN, and also rebinds when BAR1 reads the sentinel even if the MMU
    has nothing latched.
  - With a healthy BAR1, labwc with gles2 renders on the GPU and presents, but
    the desktop is not visible: the console card's 2D CE present
    (`eclipse_rm_ce_blit_p2p_2d`, meant for P2P from the other card) wrote to the
    PCIe address of its own BAR1, a peer-to-self the root complex has no reason
    to forward. Now a destination inside its own BAR1 and the console is written
    as `LOCAL_FB` into the console's VRAM, as the 1D version
    (`eclipse_rm_ce_blit`) already did. wlroots' "Failed to disable CRTC 1" never
    reaches the kernel (none of its four ioctls shows up rejected): it comes from
    losing the seat on a VT switch.
  - The new path, as in Linux (`nvidia.gem_vram`, off by default until tested):
    a `GEM_NEW` with `VRAM | GART` (what NVK asks for a dma-buf it wants in VRAM,
    and so for the compositor's scanout buffers) goes to contiguous VRAM and the
    RM maps it into BAR1 (`eclipse_rm_gem_map_bar1`, `kbusMapFbApertureSingle`).
    The published address is BAR1's base plus the VA the RM chose: PRIME,
    `ADDFB2` and mmap (write-combining) use it like any other, and the fb carries
    its VRAM offset, so `nvkms_page_flip` flips the screen to it with no copies.
    `gem_free` gives the mapping back; with BAR1 full (192 MiB out of 256 for
    buffers) or any failure, the object goes to system memory as before. If a
    present still copies one of those buffers, the CE reads it as `LOCAL_FB`.
- Goal: **parity** with the current ladder on a single card (desktop, cursor,
  tear-free flips) with `nvidia.wlr_vulkan`/`wlr_gles2`/`wlr_pixman`.

**PR 5: second card.**
- The same path for every GPU with a monitor, with a VRAM surface allocated by
  kapi: card1 turns its screen on.
- card1's `GETRESOURCES`/`GETCRTC` from its `NvKmsKapiDevice`.
- Validation on the reference hardware with a monitor on each 2060 SUPER.

**After that:** `WLR_DRM_DEVICES` generated with every card with KMS (today it
is pinned to `/dev/dri/card0` in `xtask/src/linux/mod.rs`,
`xtask/src/linux/desktop.rs` and `tools/eclipse-init/src/main.rs`), and retiring
the ladder once nvkms has replaced it on real hardware.

## 5. Risks

- **Binary size and build time**: ~190 more files. They are only built for the
  x86_64 kernel, like the RM.
- **C++ in the kernel**: the first time in this tree. The DP library is made for
  kernel environments (no exceptions or RTTI), but if global constructors show
  up, `.init_array` has to be run at boot.
- **Execution context**: nvkms expects a single global lock (`nvkms_lock`) and
  callbacks from timers; the hooks must respect that without blocking with
  interrupts disabled.
- **Validation**: only the reference hardware tests the modeset. QEMU has no
  NVIDIA GPU, so there we only check that nothing changes.
