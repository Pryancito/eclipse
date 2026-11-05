# NVIDIA acceleration — the road to Linux-like performance

[Español](README-nvidia-perf.md) · **English**

Status of the **6 building blocks** towards a desktop with performance
comparable to Linux (nouveau/NVK + hardware KMS). Updated with the kernel work
done in Eclipse.

## Session policy (userspace)

With an **NVIDIA GPU + `nvidia.nouveau_uapi`** the desktop starts on **GLES2 /
zink+NVK** (real GPU acceleration). CE present + surfaceflip + hwflip are
enabled together with the uAPI.

| Flag | Effect |
|---|---|
| `nvidia.nouveau_uapi` | nouveau uAPI + **GLES2/zink default** + CE/surfaceflip/hwflip |
| `nvidia.wlr_pixman` | Kill switch: compositor and clients in software |
| `nvidia.wlr_vulkan` | Native Vulkan compositor. Pins the Vulkan loader to the NVK ICD, and if it does not start it falls back to **GLES2/zink** (still the GPU) before pixman. Preflight in `/tmp/labwc-vulkan-preflight.log` |
| `nvidia.wlr_gles2` | Same as the default (accepted for compatibility) |
| `nvidia.nosurfaceflip` / `nvidia.nohwflip` / `nvidia.nocepresent` | Turn off one specific hardware present path |

If labwc dies twice on the GPU path, `eclipse-init` downgrades to pixman for
the rest of the boot. Firefox (`eclipse-firefox`) uses GPU WebRender behind the
same gate; without a GPU / with `wlr_pixman` it forces software. The
explicit-sync EVENTFD waits for the fence to *land* (not just to be submitted)
so the dmabuf is never sampled half-way.

## 1. Hardware scanout

| Piece | Status |
|---|---|
| Software blit CPU → GOP | ✅ fallback |
| CE present (copy engine → GOP) | ✅ automatic with nouveau_uapi / compute GPU |
| `nvidia.hwflip` → CE on `page_flip` | ✅ automatic with nouveau_uapi (`nohwflip` turns it off) |
| `nvidia.surfaceflip` → NVC57E ISO | ✅ automatic with nouveau_uapi (`nosurfaceflip` turns it off) |
| `vram_offset` in GEM VRAM (`gem_fbmem_offset`) | ✅ feeds ISO flip |
| NVC57E window / ISO flip to the swapchain | ✅ `eclipse_rm_hwflip_*` |

## 2. Compositor presentation

| Piece | Status |
|---|---|
| Synthetic vblank with the CRTC mode's period | ✅ |
| Atomic `IN_FENCE_FD` / `OUT_FENCE_PTR` | ✅ IN waits asynchronously; OUT sync_file signalled |
| `present_now` tries `page_flip` before the GOP blit | ✅ |
| `has_hw_vblank` in `DrmScheme` | ✅ defaults to false (ready for the PDISP IRQ) |
| Modifiers / block-linear scanout | ❌ `ADDFB2_MODIFIERS=0` on purpose |

## 3. nouveau-style synchronisation

| Piece | Status |
|---|---|
| HW ACQUIRE same-ctx / cross-ctx | ✅ |
| Asynchronous `SYNCOBJ_WAIT` | ✅ |
| Syncobj refcount + TIMELINE | ✅ |
| `SYNCOBJ_EVENTFD` waits for the *landed* fence (not the submit) | ✅ |
| Vulkan external semaphore (Mesa/NVK) | ❌ userspace; blocks native wlroots-Vulkan |

## 4. Stable 3D context

| Piece | Status |
|---|---|
| Prime compute + GRAPHICS | ✅ |
| Sticky ctx0 + `ctx0_reset` on respawn | ✅ |
| `MAX_CTX=32` | ✅ |
| Hang probe / recovery | ✅ |

## 5. Memory and multiple clients

| Piece | Status |
|---|---|
| GEM_NEW quotas | ✅ |
| ATOMIC_CLIENT + per-`open` events | ✅ |

## 6. Userspace

| Piece | Status |
|---|---|
| labwc GLES2/zink **by default** with nouveau_uapi | ✅ |
| Firefox GPU WebRender behind the same gate | ✅ |
| Downgrade to pixman after a crash loop | ✅ |
| Native wlroots-Vulkan | ❌ needs external-sem in NVK |

## How to test on an RTX

```
# Build
make -C zCore IMAGE=1 GL=1 LINUX=1 GRAPHIC=1

# Typical cmdline (GL=1 already adds nouveau_uapi + cepresent)
cmdline=...:nvidia.nouveau_uapi:...

# Forced software if needed
cmdline=...:nvidia.nouveau_uapi:nvidia.wlr_pixman:...
```

Look for: `surfaceflip: READY`, `PRIME OK`, `WLR_RENDERER=gles2`,
`GALLIUM_DRIVER=zink`, and an `eclipse-firefox` log with `GL=zink ACCEL=1`.
If labwc dies: `/tmp/labwc.log` and `dmesg | grep nouveau-uapi`.

## Next FPS jump

With surfaceflip the display engine scans out the swapchain's VRAM (with no copy
to the GOP) **if** the BO is pitch-linear VRAM and the NVC57E ladder reached
READY. Pending: our own raster/SOR modeset, block-linear modifiers, hardware
vblank, Vulkan external-sem.
