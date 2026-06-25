# DRM / KMS en Eclipse OS — conformidad con la UAPI de Linux

Este documento mapea la implementación de DRM (Direct Rendering Manager) de
Eclipse OS contra la documentación del kernel de Linux
([`Documentation/gpu`](https://github.com/torvalds/linux/tree/master/Documentation/gpu)),
y registra qué está implementado, qué es parcial y qué falta.

## Alcance: qué significa "ser compatible"

`Documentation/gpu` tiene dos clases de contenido muy distintas:

- **Contrato con el espacio de usuario (UAPI)** — `drm-uapi.rst`,
  `drm-usage-stats.rst`, `driver-uapi.rst`. Es lo que de verdad determina si el
  software gráfico de Linux (libdrm, Mesa, wlroots, Xorg…) funciona. **Esto es
  lo que Eclipse OS implementa.**
- **Internals del kernel de Linux** — `drm-internals.rst`, `drm-mm.rst`,
  `drm-kms-helpers.rst`, `drm-ras.rst`, etc. Describen estructuras y *helpers*
  internos del DRM de Linux (TTM, `drm_device`, midlayers de KMS…). No son un
  contrato observable desde userspace: una reimplementación desde cero **no
  necesita reproducirlos**, solo ofrecer la misma UAPI por encima.

Eclipse OS no es Linux: no hay *midlayer* DRM ni drivers de GPU completos. En su
lugar implementa la UAPI directamente sobre una ruta **"software KMS"**: cuando
hay un framebuffer (UEFI GOP, virtio-gpu, …) se sintetizan los objetos KMS
mínimos (1 CRTC + 1 connector + 1 encoder + 1 plane primario) y el *scanout* se
hace copiando el *dumb buffer* del cliente al framebuffer (`blit_from`). Esto es
suficiente para compositores Wayland por software (wlroots/labwc con pixman) y
para Xorg con el driver `fbdev`.

## Nodos de dispositivo

| Nodo | major:minor | Estado |
|---|---|---|
| `/dev/dri/card0` | 226:0 | ✅ nodo primario (KMS + dumb buffers) |
| `/dev/dri/renderD128` | 226:128 | ✅ nodo de render (mismo *backend*) |
| `/dev/fb0` | 29:0 | ✅ framebuffer legacy (`fbdev`) |
| `/sys/class/drm/card0` | — | ✅ entradas mínimas en sysfs |

## Cobertura de la UAPI de DRM (`drm-uapi.rst`)

Leyenda: ✅ implementado · 🟡 parcial / no-op deliberado · ❌ no implementado.

### Genéricos y autenticación

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_VERSION` | ✅ | nombre `zcore`, versión 1.0.0 |
| `DRM_IOCTL_GET_UNIQUE` | ✅ | `zcore-gpu` |
| `DRM_IOCTL_GET_MAGIC` / `AUTH_MAGIC` | 🟡 | cliente único = master implícito |
| `DRM_IOCTL_SET_MASTER` / `DROP_MASTER` | ✅ | conmuta la consola de texto del kernel (KD_GRAPHICS/KD_TEXT) |
| `DRM_IOCTL_GET_CAP` | ✅ | ver tabla de *caps* |
| `DRM_IOCTL_SET_CLIENT_CAP` | 🟡 | rechaza `ATOMIC` y `WRITEBACK` para forzar la ruta legacy |
| `DRM_IOCTL_WAIT_VBLANK` | ✅ | vblank sintético ~60 Hz; modo evento encola `DRM_EVENT_VBLANK` |

### GEM / *dumb buffers*

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_MODE_CREATE_DUMB` | ✅ | memoria física contigua vía VMO; *pitch* alineado a 64 B |
| `DRM_IOCTL_MODE_MAP_DUMB` | ✅ | *offset* = id del handle; `mmap` mapea el VMO físico |
| `DRM_IOCTL_MODE_DESTROY_DUMB` | ✅ | |
| `DRM_IOCTL_GEM_CLOSE` | ✅ | |
| `DRM_IOCTL_PRIME_HANDLE_TO_FD` / `FD_TO_HANDLE` | ❌ | sin dma-buf; `DRM_CAP_PRIME` se anuncia como **0** (honesto) |

### Framebuffers

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_MODE_ADDFB` | ✅ | |
| `DRM_IOCTL_MODE_ADDFB2` | ✅ | usa `handles[0]`/`pitches[0]` |
| `DRM_IOCTL_MODE_RMFB` | ✅ | |
| `DRM_IOCTL_MODE_GETFB` | ✅ | devuelve geometría + handle (cliente master único) |
| `DRM_IOCTL_MODE_GETFB2` | ✅ | formato `XR24`, plano 0 |
| `DRM_IOCTL_MODE_DIRTYFB` | ✅ | re-escanea el framebuffer (flush de *damage*) |

### KMS (modeset legacy)

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_MODE_GETRESOURCES` | ✅ | 1 CRTC + 1 connector + 1 encoder sintéticos |
| `DRM_IOCTL_MODE_GETCRTC` / `SETCRTC` | ✅ | `SETCRTC` con fb hace *scanout* |
| `DRM_IOCTL_MODE_GETENCODER` | ✅ | encoder `NONE`, `possible_crtcs=1` |
| `DRM_IOCTL_MODE_GETCONNECTOR` | ✅ | `VIRTUAL`, 1 modo = resolución nativa (preferido) |
| `DRM_IOCTL_MODE_GETPLANERESOURCES` | ✅ | 1 plano primario |
| `DRM_IOCTL_MODE_GETPLANE` | ✅ | formatos `XR24`/`AR24` |
| `DRM_IOCTL_MODE_SETPLANE` | ✅ | equivale a *scanout* del fb (ruta primaria SW) |
| `DRM_IOCTL_MODE_PAGE_FLIP` | ✅ | *scanout* + `DRM_EVENT_FLIP_COMPLETE` con `crtc_id` |
| `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` | 🟡 | solo la prop `type` del plano primario |
| `DRM_IOCTL_MODE_GETPROPERTY` | 🟡 | enum `type` {Overlay, Primary, Cursor} |
| `DRM_IOCTL_MODE_OBJ_SETPROPERTY` | 🟡 | aceptado como no-op (sin estado programable) |
| `DRM_IOCTL_MODE_CURSOR` / `CURSOR2` | ❌ | sin cursor HW → wlroots usa cursor por software |
| `DRM_IOCTL_MODE_GETGAMMA` / `SETGAMMA` | ❌ | sin LUT |
| `DRM_IOCTL_MODE_ATOMIC` | ❌ | rechazado a propósito (se usa la ruta legacy) |
| `DRM_IOCTL_MODE_CREATEPROPBLOB` / `DESTROYPROPBLOB` | ❌ | sistema de *blobs* (ligado a atomic) |
| `DRM_IOCTL_MODE_CREATE_LEASE` … | ❌ | *leases* no soportados |

### Capacidades (`DRM_IOCTL_GET_CAP`)

| Capacidad | Valor | Notas |
|---|---|---|
| `DRM_CAP_DUMB_BUFFER` | 1 | |
| `DRM_CAP_VBLANK_HIGH_CRTC` | (n/d) | un solo CRTC |
| `DRM_CAP_PRIME` | **0** | sin dma-buf (antes anunciaba 3 sin implementarlo) |
| `DRM_CAP_TIMESTAMP_MONOTONIC` | 1 | |
| `DRM_CAP_CURSOR_WIDTH` / `HEIGHT` | 64 | informativo; sin cursor HW |
| `DRM_CAP_ADDFB2_MODIFIERS` | 1 | |
| `DRM_CAP_CRTC_IN_VBLANK_EVENT` | 1 | el evento de flip lleva `crtc_id` |

### `fbdev` (`/dev/fb0`, API legacy de framebuffer)

| ioctl | Estado |
|---|---|
| `FBIOGET_VSCREENINFO` / `FBIOGET_FSCREENINFO` | ✅ |
| `FBIOPUT_VSCREENINFO` | 🟡 (resolución fija; devuelve la real) |
| `FBIOPAN_DISPLAY` / `FBIOBLANK` | 🟡 (no-op) |
| `FBIOGETCMAP` / `FBIOPUTCMAP` | 🟡 (no-op; TrueColor) |

## Huecos conocidos y justificación

- **KMS atómico** (`drm-uapi.rst` → *Atomic*). No implementado a propósito: la
  ruta de *scanout* por software no tiene estado de objetos programable, así que
  rechazamos `DRM_CLIENT_CAP_ATOMIC` para que los compositores caigan a la ruta
  *legacy*, que sí funciona. Implementarlo requeriría además el sistema de
  propiedades/*blobs* completo.
- **PRIME / dma-buf**. Los *dumb buffers* se comparten por `mmap`, no por
  descriptores dma-buf. `DRM_CAP_PRIME` ahora se anuncia como `0` para no
  desviar a los clientes basados en GBM por un camino que no podemos servir.
- **Cursor por hardware**. Sin plano de cursor; los compositores hacen
  *composición* del cursor por software. Las dimensiones de cursor se siguen
  anunciando (informativas).
- **`drm-usage-stats.rst` (fdinfo)**. No se exponen estadísticas de
  uso/memoria/engine por `fdinfo`.
- **Render / 3D**. No hay aceleración: se usa el render por software de Mesa
  (`llvmpipe`/`softpipe`).

## Cómo probar

- **Wayland (wlroots/labwc)**: el compositor abre `/dev/dri/card0`, asigna
  *dumb buffers*, hace `ADDFB`/`PAGE_FLIP`; el kernel hace *scanout* al
  framebuffer. Ver los registros `[drm] …` con `LOG=error`.
- **Xorg**: driver `fbdev` sobre `/dev/fb0`. Ver [`README-xorg.md`](README-xorg.md).

## Mapa de archivos

| Archivo | Rol |
|---|---|
| `linux-object/src/fs/devfs/drm_scheme.rs` | dispatch de ioctls de `/dev/dri/card*` |
| `linux-object/src/fs/devfs/drm.rs` | núcleo DRM: GEM, framebuffers, *scanout*, eventos, KMS sintético |
| `linux-object/src/fs/devfs/fbdev.rs` | `/dev/fb0` (API `fbdev` legacy) |
| `drivers/src/scheme/drm.rs` | trait `DrmScheme` para drivers (virtio-gpu, nvidia) |
| `drivers/src/virtio/gpu.rs` | driver virtio-gpu |
| `linux-object/src/fs/sysfs.rs` | `/sys/class/drm/card0` |
