# Aceleración NVIDIA — camino a rendimiento tipo Linux

Estado de los **6 bloques** hacia un escritorio con rendimiento comparable a
Linux (nouveau/NVK + KMS HW). Actualizado con el trabajo de kernel en Eclipse.

## Política de sesión (userspace)

Con **GPU NVIDIA + `nvidia.nouveau_uapi`** el escritorio arranca en **GLES2 /
zink+NVK** (aceleración GPU real). Present CE + surfaceflip + hwflip se activan
junto con la uAPI.

| Flag | Efecto |
|---|---|
| `nvidia.nouveau_uapi` | uAPI nouveau + **default GLES2/zink** + CE/surfaceflip/hwflip |
| `nvidia.wlr_pixman` | Kill-switch: compositor y clientes en software |
| `nvidia.wlr_vulkan` | Compositor Vulkan nativo (sigue experimental) |
| `nvidia.wlr_gles2` | Igual que el default (aceptado por compat) |
| `nvidia.nosurfaceflip` / `nvidia.nohwflip` / `nvidia.nocepresent` | Apagar present HW puntual |

Si labwc muere 2 veces en la ruta GPU, `eclipse-init` degrada a pixman el resto
del arranque. Firefox (`eclipse-firefox`) usa WebRender GPU en la misma puerta;
sin GPU / con `wlr_pixman` fuerza software. El EVENTFD de explicit-sync espera
a que la fence *aterrice* (no solo al submit) para no muestrear el dmabuf a
medias.

## 1. Scanout por hardware

| Pieza | Estado |
|---|---|
| Software blit CPU → GOP | ✅ fallback |
| CE present (copy engine → GOP) | ✅ auto con nouveau_uapi / compute GPU |
| `nvidia.hwflip` → CE en `page_flip` | ✅ auto con nouveau_uapi (`nohwflip` apaga) |
| `nvidia.surfaceflip` → NVC57E ISO | ✅ auto con nouveau_uapi (`nosurfaceflip` apaga) |
| `vram_offset` en GEM VRAM (`gem_fbmem_offset`) | ✅ alimenta ISO flip |
| Flip NVC57E window / ISO al swapchain | ✅ `eclipse_rm_hwflip_*` |

## 2. Presentación de compositor

| Pieza | Estado |
|---|---|
| Vblank sintético con periodo del modo CRTC | ✅ |
| Atomic `IN_FENCE_FD` / `OUT_FENCE_PTR` | ✅ IN espera async; OUT sync_file señalizado |
| `present_now` prueba `page_flip` antes del blit GOP | ✅ |
| `has_hw_vblank` en `DrmScheme` | ✅ default false (listo para IRQ PDISP) |
| Modifiers / block-linear scanout | ❌ `ADDFB2_MODIFIERS=0` a propósito |

## 3. Sincronización tipo nouveau

| Pieza | Estado |
|---|---|
| ACQUIRE HW same-ctx / cross-ctx | ✅ |
| `SYNCOBJ_WAIT` async | ✅ |
| Syncobj refcount + TIMELINE | ✅ |
| `SYNCOBJ_EVENTFD` espera fence *landed* (no submit) | ✅ |
| External semaphore Vulkan (Mesa/NVK) | ❌ userspace; bloquea wlroots-Vulkan nativo |

## 4. Contexto 3D estable

| Pieza | Estado |
|---|---|
| Prime compute + GRAPHICS | ✅ |
| Sticky ctx0 + `ctx0_reset` en respawn | ✅ |
| `MAX_CTX=32` | ✅ |
| Hang probe / recovery | ✅ |

## 5. Memoria y multi-cliente

| Pieza | Estado |
|---|---|
| Cuotas GEM_NEW | ✅ |
| ATOMIC_CLIENT + eventos por `open` | ✅ |

## 6. Userspace

| Pieza | Estado |
|---|---|
| labwc GLES2/zink **por defecto** con nouveau_uapi | ✅ |
| Firefox WebRender GPU en la misma puerta | ✅ |
| Degrade a pixman tras crash-loop | ✅ |
| wlroots-Vulkan nativo | ❌ necesita external-sem en NVK |

## Cómo probar en RTX

```
# Build
make -C zCore IMAGE=1 GL=1 LINUX=1 GRAPHIC=1

# Cmdline típica (GL=1 ya mete nouveau_uapi + cepresent)
cmdline=...:nvidia.nouveau_uapi:...

# Software forzado si hace falta
cmdline=...:nvidia.nouveau_uapi:nvidia.wlr_pixman:...
```

Mirar: `surfaceflip: READY`, `PRIME OK`, `WLR_RENDERER=gles2`,
`GALLIUM_DRIVER=zink`, `eclipse-firefox` log con `GL=zink ACCEL=1`.
Si labwc muere: `/tmp/labwc.log` y `dmesg | grep nouveau-uapi`.

## Siguiente salto de FPS

Con surfaceflip el display engine escanea VRAM del swapchain (sin copia a GOP)
**si** el BO es VRAM pitch-linear y el ladder NVC57E quedó READY. Pendiente:
modeset raster/SOR propio, modifiers block-linear, vblank HW, external-sem
Vulkan.
