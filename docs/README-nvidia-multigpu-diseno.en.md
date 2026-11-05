# Design: a per-GPU NVIDIA graphics stack (multi-GPU as in Linux)

[Español](README-nvidia-multigpu-diseno.md) · **English**

Step 1 of the "multi-GPU NVIDIA stack" plan. This document is the deliverable:
it does not change code. It describes how Linux organises it, which part of
Eclipse is already per GPU and which is still single for the whole machine, and
proposes how to migrate without breaking the single-card desktop that already
works.

Reference hardware: 2× RTX 2060 SUPER (Turing, TU106), tested in QEMU and on
real hardware.

---

## 1. How Linux does it

| Layer | Linux | State per… |
|---|---|---|
| RM (`nvidia.ko`) | one `nv_state_t` per GPU, `/dev/nvidia<N>` + `/dev/nvidiactl` | GPU |
| Modeset (`nvidia-modeset.ko`) | one `NVDevEvoRec` per GPU: heads, windows, cursor, flips | GPU |
| DRM (`nvidia-drm.ko`) | one `drm_device` per GPU → `cardN` + `renderD12N` | GPU |
| `drm_mode_config` | CRTCs, connectors, planes, framebuffers, blobs, atomic state | DRM device |
| GEM | device objects; **handles per `drm_file`** (per open fd) | device / fd |
| PRIME | dma-buf to pass buffers between fds and between cards | global (fd) |
| Enumeration | udev; the compositor opens every `cardN` with resources | — |

Consequences we want to reproduce:

- Each card answers only for its own connectors and can have its own monitor on
  at the same time as the other.
- One card failing (a stuck flip, a cursor that does not come up) disables
  nothing on the other.
- A GEM handle from one fd does not exist on another fd or another card
  (ENOENT).
- wlroots with several `WLR_DRM_DEVICES` renders on the primary and copies over
  PRIME to the secondary (wlroots' multi-GPU renderer).

## 2. What is already per GPU in Eclipse

Quite a lot more than it seems; there is no need to start from scratch:

- `drivers/src/display/nvidia.rs`: `NVIDIA_GPUS: Vec<Arc<NvidiaGpu>>`, one
  `NvidiaGpu` per card with its BARs, VRAM and its RM instance.
- `linux-object/src/fs/devfs/nvidia.rs`: `/dev/nvidia<N>` per stable minor,
  `nvctl::cards()`, mappings validated against each GPU's RM instance.
- `linux-object/src/fs/devfs/drm.rs`: `GPU_NODES` / `GpuNode`, one `cardN` +
  `renderD` pair per card (`c538601`).
- `drm/topology.rs`: `GETRESOURCES` answers for the node's card
  (`NodeTopology::OwnCard`), not for the machine.
- `drm/driver_registry.rs`: separates "primary", "the monitor's"
  (`nvidia.monitor=BB.DD.F`) and "the compute one" (`nvidia.compute=`), and
  `card0` is the monitor's (`c05b41d`).

## 3. What is still single for the whole machine

This is where the multi-GPU failures come from. An inventory of `static`s in the
graphics stack (not counting the "warn once" ones and diagnostic counters,
which are harmless):

### 3.1 DRM core — `linux-object/src/fs/devfs/drm.rs`

`DRM_STATE: Mutex<DrmState>` is **a single DRM device for the whole machine**:

- `crtc_fb`, `cursor`, `atomic`, `next_vblank`, `framebuffers`, `fb_backing`,
  `fb_retirements`, `blobs`, `masters`/`leases`: it is Linux's
  `drm_mode_config`, but shared. With two cards, a `SETCRTC` on `card1`
  overwrites the framebuffer `card0` is scanning out.
- `handles`, `names`, `next_handle_id`: the GEM handle space is global. A handle
  created on `card1` is valid on `card0`.

And on top of that, outside `DrmState`, the scanout state is global:

| Static | What it is | Should be |
|---|---|---|
| `CRTC_BLANKED`, `CRTC_ENABLED`, `CRTC_OFF_QUEUE` | DPMS / CRTC on-off | per CRTC |
| `SCANOUT_PAUSED`, `SCANOUT_PAUSE_DEADLINE_NS`, `SCANOUT_STALE` | scanout pause | per CRTC |
| `CE_STAGING`, `CE_PRESENT_ENABLED` | CE present buffer and mode | per GPU |
| `CURSOR_PATCH`, `CURSOR_UNDER`, `HW_CURSOR` | software/hardware cursor | per CRTC |
| `PANEL_BAND_*`, `PRESENT_SKIP` | detection of unchanged bands | per CRTC |
| `VBLANK_LATTICE`, `PENDING_DRM_TIMERS`, `FLIP_EVENT_PENDING`, `FLIPS_IN_FLIGHT`, `LAST_FLIP_PID` | vblank and flips | per CRTC |
| `CONSOLE_BRINGUP_*`, `GPU_READY_WAS_READ`, `COLD_CLIENT_WAITING` | waiting for the card to come up | per GPU |
| `NOUVEAU_CPU_VMOS` | VMO per nouveau handle | per GPU (and handle per fd) |
| `ATOMIC_ENABLED`, `FLIP_FENCE_ENABLED`, `SCANOUT_MODIFIERS`, `PRESENT_REPAIR`, `CURSOR_FROM_CLIENT`, `GEM_MAP_UNCACHED` | cmdline switches | global config (that is fine) |

### 3.2 Driver — `drivers/src/display/nvidia.rs`

"Once per boot" latches that really belong to one card:

- `SURFACEFLIP_STATE` (0 untried / 1 ready / 2 failed) and its counters.
- `CE_PRESENT_WEDGED`, `CE_2D_OOB`.
- `HWCUR_STATE` (a local static in `hw_cursor_set`).
- `BOOT_FB_INFO`, `RM_CORE_INIT_STATUS`, `RM_CORE_INIT_ATTEMPTED`.
- `EDID_IN_FLIGHT`, `HWFLIP_TRIED`, `VRAM_REFUSE`.

If the surface-flip ladder fails on one card, it stays disabled for both. The
same goes for the hardware cursor.

### 3.3 nouveau UAPI — `drivers/src/display/nouveau_uapi.rs`

It is the NVK/zink path, the one `nvidia.wlr_vulkan` and `nvidia.wlr_gles2` use:

- `CHAN_NOTIFIER_PA`: **one** channel error notifier for the whole machine. With
  two GPUs, the second `CHANNEL_ALLOC` overwrites the first one's and the
  diagnosis of a failed EXEC reads the wrong page.
- `CLASS_OBJECTS`: a global class object table, indexed by tokens.
- `CTX_WEDGED`: a 32-bit mask per context index. If indices are handed out per
  GPU, context 3 of one card and context 3 of the other share a bit.
- `COMPOSITOR_CTX0`, `HWFLIP`, `SURFACEFLIP`, `CONSOLE_*`: switches (global
  config, that is fine) mixed with state.

### 3.4 Session

`WLR_DRM_DEVICES=/dev/dri/card0` is hard-coded in `xtask/src/linux/mod.rs:1322`,
`xtask/src/linux/desktop.rs:3232` and `tools/eclipse-init/src/main.rs:503`.
Even if the kernel offered two cards with monitors, labwc would only open one.

### 3.5 The role model

Today there is one "console/monitor" card and the rest are "compute" ones. Linux
has no such distinction: any card with connectors does KMS. As long as it stays,
the second 2060 SUPER cannot drive a monitor.

## 4. Proposed design

Three levels, as in Linux:

```
global      NvidiaConfig (cmdline, read once)        driver / node registry
            diagnostic counters                      PRIME / dma-buf

per GPU     DrmDevice {                              NvidiaGpu {
              mode_config: crtcs, connectors,          display: surfaceflip, hwcursor,
                planes, framebuffers, blobs,             ce_present, boot_fb, edid
              atomic, masters/leases,                  nouveau: notifier_pa,
              crtcs[i]: { fb, blanked, paused,           class_objects, ctx_wedged,
                cursor, vblank, flips, bands }           nouveau_vmos
              gem_objects                             }
              bringup_wait
            }

per fd      DrmFile { gem_handles: handle → object, authenticated, client caps }
```

Rules:

1. **Every ioctl arrives with its device.** `DrmScheme`/the open node resolve
   the minor's `DrmDevice`; no function in `drm.rs` touches a screen-state
   `static` again.
2. **GEM handles per fd.** Objects live in the device; the fd keeps the handle
   table. Between fds and between cards things only pass through FLINK (same
   device) or PRIME (any), as in Linux.
3. **Failure latches belong to their card.** They live in `NvidiaGpu`.
4. **All boot configuration in one module.** The `nvidia.*` cmdline flags
   (including `multigpu`, `hwcursor`, `monitor`, `wlr_*`) are read once into an
   `NvidiaConfig`; the rest of the code queries it. Coordinated with the thread
   adding the boot parameters so as not to duplicate.
5. **No roles.** Every card with connectors exposes KMS. `nvidia.monitor` only
   decides which card is `card0` (wlroots' primary), and `nvidia.compute` which
   card CUDA prefers.
6. **The session enumerates.** `WLR_DRM_DEVICES` lists `card0:card1:…` with the
   cards that have connectors (`card0` first), generated at boot in a single
   place.
7. **`nvidia.wlr_vulkan` / `wlr_gles2` / `wlr_pixman` stay** as they are; the
   chosen renderer runs on `card0` and wlroots copies to the others.

## 5. Migration (step 2), in small PRs

Each PR leaves the single-card desktop as it was, passes the existing tests and
adds two-GPU tests with the mock driver (`drivers/src/mock`).

| PR | Change | Risk |
|---|---|---|
| A | `NvidiaConfig`: every `nvidia.*` into one module | low |
| B | `DrmDevice` wraps the current `DrmState` (a single instance); functions receive the device | low, mechanical |
| C | The scanout/cursor/vblank/flip state (§3.1) moves to `DrmDevice.crtcs[i]` | medium |
| D | GEM handles per fd (`DrmFile`) | medium: touches PRIME and FLINK |
| E | The driver's latches and the nouveau tables (§3.2, §3.3) into `NvidiaGpu` | medium |
| F | One `DrmDevice` per GPU, KMS on every card, generated `WLR_DRM_DEVICES` | high: this is where the second monitor appears |

Until F nothing visible changes. F is the only one worth testing first on real
hardware with the two 2060 SUPERs before merging.

Recommendation: **an incremental rewrite, not from scratch.** The per-GPU part
of the RM, `/dev/nvidia<N>` and the topology are already on the right track;
what fails is the DRM core and the latches, and migrating them step by step
keeps the current desktop working in every PR.

## 6. Validation

- **Unit tests:** two registered mock drivers; a `SETCRTC`, a flip or a cursor
  on one changes nothing on the other; a `card1` handle gives ENOENT on `card0`;
  a failure latch on one GPU does not affect the other.
- **QEMU:** NVIDIA cannot be emulated, so the DRM core is tested with two
  virtio-gpu outputs (two monitors, two `cardN`) to cover PRs B–D and F without
  hardware.
- **Real (2× RTX 2060 SUPER):** a checklist for each combination of
  `nvidia.wlr_vulkan` / `wlr_gles2` / `wlr_pixman`: one monitor on each card,
  both monitors on the same one, unplug and replug, cursor across screens,
  `nvidia-smi` sees both, CUDA and Ollama on the card without a compositor.

## 7. Open questions

1. Which cosmetic failures exactly do you see (cursor, bands, flicker, tearing,
   colours) and with which `wlr_*`? Screenshots or `dmesg` help prioritise
   step 3.
2. ~~One monitor on each card, or one for compute?~~ **Decided (27 Oct): exactly
   as in Linux.** Every card with a monitor does KMS at the same time, and any of
   them can be used for CUDA/Ollama even if it also drives a screen. PR F is part
   of the goal, not optional.
