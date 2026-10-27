# Diseño: pila gráfica NVIDIA por GPU (multi-GPU como en Linux)

Paso 1 del plan «stack NVIDIA multi-GPU». Este documento es el entregable:
no cambia código. Describe cómo lo organiza Linux, qué parte de Eclipse ya es
por GPU y cuál sigue siendo única para toda la máquina, y propone cómo
migrar sin romper el escritorio de una sola tarjeta que ya funciona.

Hardware de referencia: 2× RTX 2060 SUPER (Turing, TU106), pruebas en QEMU y
en real.

---

## 1. Cómo lo hace Linux

| Capa | Linux | Estado por… |
|---|---|---|
| RM (`nvidia.ko`) | un `nv_state_t` por GPU, `/dev/nvidia<N>` + `/dev/nvidiactl` | GPU |
| Modeset (`nvidia-modeset.ko`) | un `NVDevEvoRec` por GPU: heads, ventanas, cursor, flips | GPU |
| DRM (`nvidia-drm.ko`) | un `drm_device` por GPU → `cardN` + `renderD12N` | GPU |
| `drm_mode_config` | CRTCs, conectores, planos, framebuffers, blobs, estado atómico | dispositivo DRM |
| GEM | objetos del dispositivo; **handles por `drm_file`** (por fd abierto) | dispositivo / fd |
| PRIME | dma-buf para pasar buffers entre fds y entre tarjetas | global (fd) |
| Enumeración | udev; el compositor abre todos los `cardN` con recursos | — |

Consecuencias que buscamos reproducir:

- Cada tarjeta responde solo por sus conectores y puede tener su propio
  monitor encendido a la vez que la otra.
- Que una tarjeta falle (un flip atascado, un cursor que no sube) no
  desactiva nada en la otra.
- Un handle GEM de un fd no existe en otro fd ni en otra tarjeta (ENOENT).
- wlroots con varios `WLR_DRM_DEVICES` renderiza en la primaria y copia por
  PRIME a la secundaria (multi-GPU renderer de wlroots).

## 2. Qué ya es por GPU en Eclipse

Bastante más de lo que parece; no hay que empezar de cero:

- `drivers/src/display/nvidia.rs`: `NVIDIA_GPUS: Vec<Arc<NvidiaGpu>>`, un
  `NvidiaGpu` por tarjeta con sus BAR, VRAM y su instancia de RM.
- `linux-object/src/fs/devfs/nvidia.rs`: `/dev/nvidia<N>` por minor estable,
  `nvctl::cards()`, mapeos validados contra la instancia RM de cada GPU.
- `linux-object/src/fs/devfs/drm.rs`: `GPU_NODES` / `GpuNode`, una pareja
  `cardN` + `renderD` por tarjeta (`c538601`).
- `drm/topology.rs`: `GETRESOURCES` responde por la tarjeta del nodo
  (`NodeTopology::OwnCard`), no por la máquina.
- `drm/driver_registry.rs`: separa «primaria», «la del monitor»
  (`nvidia.monitor=BB.DD.F`) y «la de compute» (`nvidia.compute=`), y
  `card0` es la del monitor (`c05b41d`).

## 3. Qué sigue siendo único para toda la máquina

Aquí está el origen de los fallos multi-GPU. Inventario de `static` en la
pila gráfica (sin contar los de «avisar una vez» y contadores de
diagnóstico, que son inofensivos):

### 3.1 Núcleo DRM — `linux-object/src/fs/devfs/drm.rs`

`DRM_STATE: Mutex<DrmState>` es **un solo dispositivo DRM para toda la
máquina**:

- `crtc_fb`, `cursor`, `atomic`, `next_vblank`, `framebuffers`,
  `fb_backing`, `fb_retirements`, `blobs`, `masters`/`leases`: es el
  `drm_mode_config` de Linux, pero compartido. Con dos tarjetas, un
  `SETCRTC` en `card1` pisa el framebuffer que escanea `card0`.
- `handles`, `names`, `next_handle_id`: el espacio de handles GEM es
  global. Un handle creado en `card1` es válido en `card0`.

Y además, fuera de `DrmState`, el estado de scanout es global:

| Static | Qué es | Debería ser |
|---|---|---|
| `CRTC_BLANKED`, `CRTC_ENABLED`, `CRTC_OFF_QUEUE` | DPMS / CRTC on-off | por CRTC |
| `SCANOUT_PAUSED`, `SCANOUT_PAUSE_DEADLINE_NS`, `SCANOUT_STALE` | pausa de scanout | por CRTC |
| `CE_STAGING`, `CE_PRESENT_ENABLED` | buffer y modo de present por CE | por GPU |
| `CURSOR_PATCH`, `CURSOR_UNDER`, `HW_CURSOR` | cursor software/hardware | por CRTC |
| `PANEL_BAND_*`, `PRESENT_SKIP` | detección de bandas sin cambios | por CRTC |
| `VBLANK_LATTICE`, `PENDING_DRM_TIMERS`, `FLIP_EVENT_PENDING`, `FLIPS_IN_FLIGHT`, `LAST_FLIP_PID` | vblank y flips | por CRTC |
| `CONSOLE_BRINGUP_*`, `GPU_READY_WAS_READ`, `COLD_CLIENT_WAITING` | espera de arranque de la tarjeta | por GPU |
| `NOUVEAU_CPU_VMOS` | VMO por handle nouveau | por GPU (y handle por fd) |
| `ATOMIC_ENABLED`, `FLIP_FENCE_ENABLED`, `SCANOUT_MODIFIERS`, `PRESENT_REPAIR`, `CURSOR_FROM_CLIENT`, `GEM_MAP_UNCACHED` | interruptores de cmdline | config global (está bien) |

### 3.2 Driver — `drivers/src/display/nvidia.rs`

Latches de «una vez por arranque» que en realidad son de una tarjeta:

- `SURFACEFLIP_STATE` (0 sin probar / 1 listo / 2 fallido) y sus contadores.
- `CE_PRESENT_WEDGED`, `CE_2D_OOB`.
- `HWCUR_STATE` (static local en `hw_cursor_set`).
- `BOOT_FB_INFO`, `RM_CORE_INIT_STATUS`, `RM_CORE_INIT_ATTEMPTED`.
- `EDID_IN_FLIGHT`, `HWFLIP_TRIED`, `VRAM_REFUSE`.

Si la escalera de surface-flip falla en una tarjeta, queda desactivada para
las dos. Lo mismo con el cursor hardware.

### 3.3 nouveau-UAPI — `drivers/src/display/nouveau_uapi.rs`

Es la ruta de NVK/zink, la que usan `nvidia.wlr_vulkan` y `nvidia.wlr_gles2`:

- `CHAN_NOTIFIER_PA`: **un** notificador de error de canal para toda la
  máquina. Con dos GPUs, el segundo `CHANNEL_ALLOC` sobrescribe el del
  primero y el diagnóstico de un EXEC fallido lee la página equivocada.
- `CLASS_OBJECTS`: tabla de objetos de clase global, indexada por tokens.
- `CTX_WEDGED`: máscara de 32 bits por índice de contexto. Si los índices se
  reparten por GPU, el contexto 3 de una tarjeta y el 3 de la otra
  comparten bit.
- `COMPOSITOR_CTX0`, `HWFLIP`, `SURFACEFLIP`, `CONSOLE_*`: interruptores
  (config global, está bien) mezclados con estado.

### 3.4 Sesión

`WLR_DRM_DEVICES=/dev/dri/card0` fijo en `xtask/src/linux/mod.rs:1322`,
`xtask/src/linux/desktop.rs:3232` y `tools/eclipse-init/src/main.rs:503`.
Aunque el kernel ofreciera dos tarjetas con monitor, labwc solo abriría una.

### 3.5 El modelo de roles

Hoy hay una tarjeta «de consola/monitor» y el resto son «de compute». En
Linux no existe esa distinción: cualquier tarjeta con conectores hace KMS.
Mientras se mantenga, la segunda 2060 SUPER no puede llevar monitor.

## 4. Diseño propuesto

Tres niveles, como en Linux:

```
global      NvidiaConfig (cmdline, leído una vez)   registro de drivers / nodos
            contadores de diagnóstico                PRIME / dma-buf

por GPU     DrmDevice {                              NvidiaGpu {
              mode_config: crtcs, conectores,          display: surfaceflip, hwcursor,
                planos, framebuffers, blobs,             ce_present, boot_fb, edid
              atomic, masters/leases,                  nouveau: notifier_pa,
              crtcs[i]: { fb, blanked, paused,           class_objects, ctx_wedged,
                cursor, vblank, flips, bandas }          nouveau_vmos
              gem_objects                             }
              bringup_wait
            }

por fd      DrmFile { gem_handles: handle → objeto, autenticado, client caps }
```

Reglas:

1. **Cada ioctl llega con su dispositivo.** `DrmScheme`/el nodo abierto
   resuelven el `DrmDevice` del minor; ninguna función de `drm.rs` vuelve a
   tocar un `static` de estado de pantalla.
2. **Handles GEM por fd.** Los objetos viven en el dispositivo; el fd
   guarda la tabla de handles. Entre fds y entre tarjetas solo se pasa por
   FLINK (mismo dispositivo) o PRIME (cualquiera), igual que en Linux.
3. **Los latches de fallo son de su tarjeta.** Viven en `NvidiaGpu`.
4. **Toda la configuración de arranque en un módulo.** Los
   `nvidia.*` del cmdline (incluidos `multigpu`, `hwcursor`, `monitor`,
   `wlr_*`) se leen una vez a un `NvidiaConfig`; el resto del código lo
   consulta. Se coordina con el hilo que está añadiendo los parámetros de
   arranque para no duplicar.
5. **Sin roles.** Toda tarjeta con conectores expone KMS. `nvidia.monitor`
   solo decide qué tarjeta es `card0` (la primaria de wlroots), y
   `nvidia.compute` qué tarjeta prefiere CUDA.
6. **La sesión enumera.** `WLR_DRM_DEVICES` lista `card0:card1:…` con las
   tarjetas que tengan conectores (`card0` primero), generado al arrancar en
   un solo sitio.
7. **`nvidia.wlr_vulkan` / `wlr_gles2` / `wlr_pixman` se mantienen** tal
   como están; el renderer elegido corre en `card0` y wlroots copia a las
   demás.

## 5. Migración (paso 2), en PRs pequeños

Cada PR deja el escritorio de una sola tarjeta igual que antes, pasa los
tests existentes y añade tests de dos GPUs con el driver mock
(`drivers/src/mock`).

| PR | Cambio | Riesgo |
|---|---|---|
| A | `NvidiaConfig`: todos los `nvidia.*` a un módulo | bajo |
| B | `DrmDevice` envuelve el `DrmState` actual (una sola instancia); las funciones reciben el dispositivo | bajo, mecánico |
| C | El estado de scanout/cursor/vblank/flip (§3.1) pasa a `DrmDevice.crtcs[i]` | medio |
| D | Handles GEM por fd (`DrmFile`) | medio: toca PRIME y FLINK |
| E | Latches del driver y tablas nouveau (§3.2, §3.3) a `NvidiaGpu` | medio |
| F | Un `DrmDevice` por GPU, KMS en todas las tarjetas, `WLR_DRM_DEVICES` generado | alto: aquí aparece el segundo monitor |

Hasta F no cambia nada visible. F es el único que conviene probar primero en
real con las dos 2060 SUPER antes de fusionar.

Recomendación: **reescritura incremental, no desde cero.** La parte por GPU
del RM, `/dev/nvidia<N>` y la topología ya está bien encaminada; lo que
falla es el núcleo DRM y los latches, y migrarlos por pasos mantiene el
escritorio actual funcionando en cada PR.

## 6. Validación

- **Tests de unidad:** dos drivers mock registrados; un `SETCRTC`, un flip o
  un cursor en uno no cambia nada en el otro; un handle de `card1` da ENOENT
  en `card0`; un latch de fallo en una GPU no afecta a la otra.
- **QEMU:** NVIDIA no se puede emular, así que el núcleo DRM se prueba con
  dos salidas virtio-gpu (dos monitores, dos `cardN`) para cubrir los PRs
  B–D y F sin hardware.
- **Real (2× RTX 2060 SUPER):** checklist por cada combinación de
  `nvidia.wlr_vulkan` / `wlr_gles2` / `wlr_pixman`:
  un monitor en cada tarjeta, los dos monitores en la misma, desenchufar y
  enchufar, cursor entre pantallas, `nvidia-smi` ve las dos, CUDA y Ollama en
  la tarjeta sin compositor.

## 7. Preguntas abiertas

1. ¿Qué fallos cosméticos ves exactamente (cursor, bandas, parpadeo,
   tearing, colores) y con qué `wlr_*`? Capturas o `dmesg` ayudan a
   priorizar el paso 3.
2. ¿Quieres un monitor en cada 2060 SUPER a la vez, o un escritorio en una y
   la otra para CUDA/Ollama? El diseño soporta las dos cosas, pero cambia el
   orden: con «una para escritorio y otra para compute» el PR F es menos
   urgente.
