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
| `/dev/dri/renderD128` | 226:128 | 🟡 nodo de render; el filtro `DRM_RENDER_ALLOW` está escrito pero **solo observa** (ver abajo) |
| `/dev/fb0` | 29:0 | ✅ framebuffer legacy (`fbdev`) |
| `/sys/class/drm/card0` | — | ✅ entradas mínimas en sysfs |

### Render nodes (`drm-uapi.rst` → *Render nodes*)

Linux hace que `renderD128` **solo** acepte los ioctls marcados
`DRM_RENDER_ALLOW` en `drm_ioctl.c` (`VERSION`, `GET_CAP`, `GEM_CLOSE`,
`PRIME_*`, `SYNCOBJ_*`, `SET_CLIENT_NAME` y el rango de comandos del driver) y
devuelva **EACCES** a cualquier ioctl de modeset, *dumb buffers* o master/auth.
Eclipse tiene el filtro escrito (`render_allowed`) pero **todavía solo
observa**: registra en el klog el ioctl que Linux habría rechazado y lo deja
pasar, porque activarlo a ciegas rompería el escritorio software-GL que hoy
arranca por ese nodo. Hasta que se active, `renderD128` se comporta como un
segundo nodo KMS completo (ver `drm_scheme.rs`, `io_control`).

### Punteros de usuario

Todo argumento de ioctl (y cada puntero anidado: `fb_id_ptr`, `modes_ptr`,
`clips_ptr`, `handles`, `data` de los blobs, `values_ptr`/`enum_blob_ptr` de
`GETPROPERTY`, `op_ptr`/`push_ptr` de nouveau…) se comprueba contra la mitad de
usuario del espacio de direcciones antes de tocarlo; una dirección nula o de
kernel devuelve **EFAULT**, como `access_ok()` en Linux.

> Esta frase fue falsa hasta el 20-sep-2026: los dos punteros de salida de
> `GETPROPERTY` eran los únicos anidados del fichero sin comprobar, y el ioctl
> no pide privilegios, así que cualquier cliente podía dar una dirección del
> kernel y hacer que el kernel escribiera allí.

### Despacho por número de ioctl

Linux nunca despacha por el comando codificado: `drm_ioctl()` toma
`nr = _IOC_NR(cmd)`, busca el manejador por ese número y **reconcilia** después
el tamaño (`drm_ioctl_kernel`: reservar `max(in, out, drv)`, copiar lo que el
cliente mandó, rellenar el resto con ceros, ejecutar, devolver `out_size`). Eso
es lo que hace que un libdrm de 2019 siga funcionando con un kernel de 2026.
Eclipse hace lo mismo desde el 20-sep-2026: comparar el comando completo
convertía cada estructura que crecía un campo en un ioctl desconocido, y costó
tres parches a mano (los tamaños con deadline de `SYNCOBJ_WAIT`/`TIMELINE_WAIT`,
`SYNCOBJ_HANDLE_TO_FD` a 24 bytes y los de PRIME). Un número de ioctl de núcleo
desconocido responde **EINVAL**, como Linux, no ENOSYS.

### Propiedad de handles y framebuffers

Linux lleva la tabla de handles GEM en un idr **por fichero abierto** y la lista
de framebuffers en `file_priv->fbs`, y resuelve cada id de la uAPI contra el
fichero que llama (`drm_gem_object_lookup(file, handle)`). Aquí las tablas son
globales y el sustituto es el pid del propietario, comprobado en las mismas
entradas: exportar por PRIME, `mmap` de un objeto del driver, `ADDFB`/`ADDFB2`,
`RMFB`/`CLOSEFB` y el handle que devuelven `GETFB`/`GETFB2`. Los caminos
internos del kernel (pid 0) y el de presentación siguen sin comprobar nada, que
es la misma separación que hace Linux.

Es **más débil** que Linux en un punto: dos fds del mismo proceso no quedan
aislados entre sí. Cierra las rutas entre procesos, que eran las graves.

## Cobertura de la UAPI de DRM (`drm-uapi.rst`)

Leyenda: ✅ implementado · 🟡 parcial / no-op deliberado · ❌ no implementado.

### Genéricos y autenticación

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_VERSION` | ✅ | nombre `zcore`, versión 1.0.0 |
| `DRM_IOCTL_GET_UNIQUE` | 🟡 | `zcore-gpu` (no es un *busid* parseable `pci:…`) |
| `DRM_IOCTL_SET_VERSION` | ✅ | interfaz 1.4, driver 1.0; valida majors como `drm_setversion` (lo usa Xorg/modesetting) |
| `DRM_IOCTL_GET_MAGIC` / `AUTH_MAGIC` | 🟡 | cliente único = master implícito |
| `DRM_IOCTL_SET_MASTER` / `DROP_MASTER` | ✅ | conmuta la consola de texto del kernel (KD_GRAPHICS/KD_TEXT) |
| `DRM_IOCTL_GET_CAP` | ✅ | ver tabla de *caps* |
| `DRM_IOCTL_SET_CLIENT_CAP` | ✅ | `ATOMIC` según `drm.atomic` (ver sección atómico); `WRITEBACK` solo para clientes atómicos (EINVAL como Linux); resto aceptado |
| `DRM_IOCTL_WAIT_VBLANK` | 🟡 | vblank sintético ~60 Hz. Modo evento: respeta `RELATIVE`/`ABSOLUTE`/`NEXTONMISS` y entrega `DRM_EVENT_VBLANK` cuando el contador alcanza la secuencia pedida (nunca antes del siguiente vblank). Modo bloqueante: espera de verdad, hasta el tope de 3 s de Linux (`sys_ioctl` duerme antes del arm síncrono; ver `README-async-ioctl-vblank.md`) |

### GEM / *dumb buffers* / PRIME

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_MODE_CREATE_DUMB` | ✅ | memoria física contigua vía VMO; *pitch* alineado a 64 B. El `mmap` comparte el VMO del buffer (cacheado, WB): el mapeo y cada framebuffer construido sobre el handle mantienen la memoria viva tras `DESTROY_DUMB`, como el refcount de `drm_gem_object` |
| `DRM_IOCTL_MODE_MAP_DUMB` | ✅ | *offset* = `handle << 12`; `mmap` mapea el VMO físico |
| `DRM_IOCTL_MODE_DESTROY_DUMB` | ✅ | |
| `DRM_IOCTL_GEM_CLOSE` | ✅ | |
| `DRM_IOCTL_PRIME_HANDLE_TO_FD` / `FD_TO_HANDLE` | ✅ | dma-buf real (fd de proceso); despachado en la capa de syscalls porque necesita la tabla de fds |
| `DRM_IOCTL_GEM_FLINK` / `GEM_OPEN` | ❌ | interfaz legacy insegura; los clientes nuevos deben usar PRIME (igual que recomienda `drm-uapi.rst`) |

### Framebuffers

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_MODE_ADDFB` | ✅ | |
| `DRM_IOCTL_MODE_ADDFB2` | ✅ | usa `handles[0]`/`pitches[0]` |
| `DRM_IOCTL_MODE_RMFB` | ✅ | |
| `DRM_IOCTL_MODE_CLOSEFB` | ✅ | Linux 6.6+: suelta la referencia sin apagar el plano |
| `DRM_IOCTL_MODE_GETFB` | ✅ | devuelve geometría + handle (cliente master único) |
| `DRM_IOCTL_MODE_GETFB2` | ✅ | formato `XR24`, plano 0 |
| `DRM_IOCTL_MODE_DIRTYFB` | ✅ | blitea la unión de los `drm_clip_rect` (sin *clips*, o si excede el límite, re-escanea todo); los bordes horizontales se expanden a líneas WC de 64 B para no manchar píxeles vecinos |

### KMS (modeset legacy)

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_MODE_GETRESOURCES` | ✅ | 1 CRTC + 1 connector + 1 encoder sintéticos |
| `DRM_IOCTL_MODE_GETCRTC` / `SETCRTC` | ✅ | `SETCRTC` con fb hace *scanout*; `GETCRTC` devuelve el modo actual (`mode_valid=1`) |
| `DRM_IOCTL_MODE_GETENCODER` | ✅ | encoder `VIRTUAL`, `possible_crtcs=1` |
| `DRM_IOCTL_MODE_GETCONNECTOR` | ✅ | 1 modo = resolución nativa (preferido); propiedades estándar (ver abajo) |
| `DRM_IOCTL_MODE_GETPLANERESOURCES` | ✅ | 1 plano primario |
| `DRM_IOCTL_MODE_GETPLANE` | ✅ | formatos `XR24`/`AR24` |
| `DRM_IOCTL_MODE_SETPLANE` | ✅ | equivale a *scanout* del fb (ruta primaria SW) |
| `DRM_IOCTL_MODE_PAGE_FLIP` | ✅ | *scanout*; `DRM_EVENT_FLIP_COMPLETE` (con `crtc_id` y la secuencia de vblank) solo con `PAGE_FLIP_EVENT`; `ASYNC`/`TARGET_*`/flags desconocidos → EINVAL, fb desconocido → ENOENT |
| `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` | ✅ | tabla de propiedades por objeto; las propiedades `DRM_MODE_PROP_ATOMIC` solo se muestran a clientes atómicos (mismo filtrado que Linux) |
| `DRM_IOCTL_MODE_GETPROPERTY` | ✅ | metadatos completos: flags, nombre, rangos/enums/tipo de objeto |
| `DRM_IOCTL_MODE_OBJ_SETPROPERTY` / `SETPROPERTY` | 🟡 | **DPMS sí actúa**: cualquier valor distinto de "On" apaga el CRTC (pantalla en negro y se dejan de repintar el cursor y el daño), y la propiedad se relee con el valor puesto. El resto se acepta como no-op |
| `DRM_IOCTL_MODE_GETPROPBLOB` | ✅ | EDID + blobs del almacén de propiedades |
| `DRM_IOCTL_MODE_CREATEPROPBLOB` / `DESTROYPROPBLOB` | ✅ | almacén de blobs; destruir un blob del kernel da EACCES (Linux: EPERM) |
| `DRM_IOCTL_MODE_CURSOR` / `CURSOR2` | ✅ | cursor compuesto por el kernel sobre cada frame (`set_cursor_bo`/`move_cursor`); más de 64×64 → EINVAL (el tamaño anunciado en las *caps*) |
| `DRM_IOCTL_MODE_ATOMIC` | ✅ | **opt-in** con `drm.atomic` (ver sección siguiente) |
| `DRM_IOCTL_MODE_GETGAMMA` / `SETGAMMA` | 🟡 | aceptados como no-op (`gamma_size=0`, sin `GAMMA_LUT`); devolver ENOTTY provocaba una tormenta de SETGAMMA desde Xorg |
| `DRM_IOCTL_MODE_CREATE_LEASE` | 🟡 | aceptado, **no restringido**: devuelve un `dup` del fd, así que un cliente "arrendado" conserva el control completo del dispositivo. `LIST_LESSEES` responde cero |
| `DRM_IOCTL_SYNCOBJ_*` | 🟡 | implementados (`CREATE`/`DESTROY`/`WAIT`/`RESET`/`SIGNAL`/`TIMELINE_*`/`QUERY`/`TRANSFER`, `drivers/src/scheme/syncobj.rs`) pero solo activos con `nvidia.nouveau_uapi`; sin él EOPNOTSUPP y `DRM_CAP_SYNCOBJ=0` |

### Propiedades KMS estándar (`drm-kms.rst` → *KMS Properties*)

| Objeto | Propiedad | Tipo | Notas |
|---|---|---|---|
| plane | `type` | enum inmutable | {Overlay, Primary, Cursor} |
| plane | `FB_ID`, `CRTC_ID` | object, atómica | |
| plane | `CRTC_X/Y/W/H`, `SRC_X/Y/W/H` | range, atómicas | `SRC_*` en 16.16 |
| CRTC | `ACTIVE` | range 0–1, atómica | |
| CRTC | `MODE_ID` | blob, atómica | blob de `drm_mode_modeinfo` (68 B) |
| connector | `DPMS` | enum | siempre "On"; escritura aceptada como no-op |
| connector | `link-status` | enum | siempre "Good" |
| connector | `non-desktop` | range inmutable | 0 |
| connector | `EDID` | blob inmutable | EDID real (UEFI/DDC) si existe |
| connector | `CRTC_ID` | object, atómica | |

### Capacidades (`DRM_IOCTL_GET_CAP`)

| Capacidad | Valor | Notas |
|---|---|---|
| `DRM_CAP_DUMB_BUFFER` | 1 | |
| `DRM_CAP_DUMB_PREFERRED_DEPTH` | 24 | scanout XRGB8888 |
| `DRM_CAP_DUMB_PREFER_SHADOW` | 1 | el *present* es un blit CPU sobre PCIe: renderizar a *shadow* y copiar es exactamente lo aconsejable |
| `DRM_CAP_PRIME` | 3 | IMPORT \| EXPORT (dma-buf real) |
| `DRM_CAP_TIMESTAMP_MONOTONIC` | 1 | |
| `DRM_CAP_ASYNC_PAGE_FLIP` | 0 | |
| `DRM_CAP_CURSOR_WIDTH` / `HEIGHT` | 64 | cursor compuesto por el kernel |
| `DRM_CAP_ADDFB2_MODIFIERS` | 0 | a propósito: con 1 wlroots probaba *modifiers* con *tiling* que el scanout software no entiende |
| `DRM_CAP_PAGE_FLIP_TARGET` | 0 | |
| `DRM_CAP_CRTC_IN_VBLANK_EVENT` | 1 | el evento de flip lleva `crtc_id` |
| `DRM_CAP_SYNCOBJ` / `SYNCOBJ_TIMELINE` | 0 ó 1 | 1 con `nvidia.nouveau_uapi`, 0 sin él — igual que los ioctls, que responden EOPNOTSUPP sin el flag |
| `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP` | 0 | |

## KMS atómico (`drm-uapi.rst` → *Atomic Mode Setting*) — opt-in

La ruta atómica completa está implementada para el *pipeline* sintético
(software KMS): negociación con `DRM_CLIENT_CAP_ATOMIC`, propiedades atómicas
en los tres objetos, blobs de modo (`CREATEPROPBLOB`/`MODE_ID`) y
`DRM_IOCTL_MODE_ATOMIC` con la semántica de Linux:

- `TEST_ONLY` valida sin tocar estado (y `TEST_ONLY`+`PAGE_FLIP_EVENT` es
  EINVAL, como en Linux).
- `ALLOW_MODESET` es obligatorio para cambios de `MODE_ID`/`ACTIVE`
  ("requires full modeset").
- `PAGE_FLIP_EVENT` encola un `DRM_EVENT_FLIP_COMPLETE` por CRTC del commit.
- `PAGE_FLIP_ASYNC` se rechaza (las *caps* async son 0).
- Objeto/propiedad desconocidos → ENOENT; valores fuera de rango → EINVAL.
- El commit de un `FB_ID` hace el *scanout* (mismo blit que la ruta legacy).

**Cómo activarlo**: es estrictamente **opt-in** mientras la ruta legacy siga
siendo la probada en hardware real — el mismo despliegue que hizo nouveau con
`nouveau.atomic=1`. Arranca con el flag `drm.atomic` en la `cmdline` del kernel
(`zCore/rboot.conf`):

```ini
cmdline=LOG=warn:drm.atomic:ROOTPROC=/bin/busybox?sh
```

y quita `WLR_DRM_NO_ATOMIC=1` del entorno para que wlroots use la ruta
atómica. Sin el flag, `SET_CLIENT_CAP(ATOMIC)` devuelve **EOPNOTSUPP** (igual
que un driver Linux sin `DRIVER_ATOMIC`) y los compositores caen a la ruta
legacy exactamente como antes.

`IN_FENCE_FD` y `OUT_FENCE_PTR` **sí existen**: la primera espera de verdad a
la fence del cliente antes de presentar (con un tope de 100 ms, tras el cual
presenta igual), y la segunda instala un `sync_file` **ya señalado** — honesto
mientras el present sea síncrono, porque cuando el ioctl vuelve los píxeles ya
están en pantalla, pero no sirve para marcar el ritmo de frames.

Un commit que falla no deja nada a medias: el estado KMS, el framebuffer del
CRTC y el blob de modo del kernel se restauran, como hace
`drm_atomic_helper_swap_state`, que solo intercambia el estado duplicado
cuando la comprobación y el commit han ido bien. `ACTIVE=0` apaga el CRTC.

Limitaciones deliberadas: un solo modo (el nativo del panel), sin plano de
cursor atómico (el cursor legacy sigue compuesto por el kernel) y solo sobre la
ruta software-KMS (con un driver KMS por hardware la negociación atómica se
rechaza).

## Huecos conocidos y justificación

- **`GET_UNIQUE` no devuelve un busid `pci:…`**. libdrm moderno deriva el bus
  del sysfs (`/sys/class/drm/card0/device`), que sí está; solo afectaría a
  clientes legacy que parseen el *busid*.
- **Gamma/CTM/HDR**: sin LUTs (`gamma_size=0`, sin `GAMMA_LUT`/`CTM`); el
  *scanout* software no las aplica. Consecuencia visible: gammastep, la luz
  nocturna y la gamma de RandR no hacen nada.
- **Leases**: `CREATE_LEASE` devuelve un `dup` del fd sin restringir nada, y
  `GET_LEASE`/`REVOKE_LEASE` no existen. **Syncobj**: solo con la UAPI de
  nouveau (arriba).
- **Estado por apertura**: los eventos y `DRM_CLIENT_CAP_ATOMIC` **sí** van
  por fd abierto (`DrmFileState`, que `sys_open` crea por cada `open`). Los
  handles GEM, los framebuffers, el cursor y el estado de master siguen en
  tablas globales; para los handles y los framebuffers el pid del propietario
  cubre el acceso entre procesos (ver arriba), pero no aísla dos fds del mismo
  proceso, y **no hay estado de master en absoluto**: `SET_MASTER` no registra
  nada, así que todos los ioctls que Linux marca `DRM_MASTER` los puede emitir
  cualquiera.
- **`drm-usage-stats.rst` (fdinfo)**. No se exponen estadísticas de
  uso/memoria/engine por `fdinfo`.
- **Render / 3D**. No hay aceleración: se usa el render por software de Mesa
  (`llvmpipe`/`softpipe`).

## Lo que aún diverge (auditoría del 20-sep-2026)

Por orden de lo que rompe a clientes reales. Lo de arriba ya está arreglado;
esto no:

| Divergencia | Qué rompe |
|---|---|
| `SYNCOBJ_TRANSFER` pierde el `dst_point` en el camino software | wlroots `linux-drm-syncobj-v1`: el frame del cliente no se libera nunca |
| Los fd de `sync_file` no se pueden sondear (`poll` dice siempre "no legible") | `sync_wait()` de Mesa, que es literalmente un `poll()`, se cuelga |
| `OUT_FENCE_PTR` devuelve una fence ya señalada | Quien marque el ritmo de frames con ella suelta buffers aún en escaneo |
| El arm síncrono de `SYNCOBJ_WAIT` puede girar sin ceder la CPU | Una corrutina del kernel atascada; la máquina parece congelada |
| Sin caché de dma-buf por objeto ni de handles PRIME por fichero | wlroots agota el tope global de 64 objetos GEM a los 64 frames |
| `ADDFB2` no valida formato, flags ni modifiers | Un modifier con tiling se acepta y se escanea como lineal: basura |
| El límite de clips de `DIRTYFB` es 64 donde Linux usa 256, y `ANNOTATE_COPY` no está | Cada frame con más daño cae al blit de pantalla completa por CPU |
| Sin estado de master | Todo ioctl `DRM_MASTER` lo puede emitir cualquiera |
| `GET_CLIENT` no existe | libva da la inicialización por fallida: sin VA-API |
| Sin `GAMMA_LUT`/`CTM` | gammastep, luz nocturna y la gamma de RandR no hacen nada |
| Sin `IN_FORMATS` en el plano | Inocuo mientras `DRM_CAP_ADDFB2_MODIFIERS` sea 0; obligatorio el día que se ponga a 1 |
| `CRTC_GET_SEQUENCE`/`QUEUE_SEQUENCE` no existen | Pero `DRM_CAP_CRTC_IN_VBLANK_EVENT` dice 1: Xorg sondea, falla y cae a `drmWaitVBlank` |
| El filtro del nodo de render solo observa | `renderD128` es un segundo nodo KMS completo, con dumb buffers incluidos |
| Sin `fdinfo` (`drm-usage-stats.rst`) | `nvtop` y similares no ven nada |

## Cómo lanzar labwc

El kernel implementa la ruta **legacy-KMS + dumb buffers + scanout por
software** (y, opt-in, la atómica). Para que wlroots/labwc usen la ruta por
defecto (y NO intenten GBM/EGL/GL, que no hay aceleración) hay que forzar el
renderer **pixman** y el KMS legacy por variables de entorno:

```sh
# Gestor de asientos (o arráncalo en /etc/init.d/rcS). Da acceso a
# /dev/dri/card0 y /dev/input/event* sin logind.
seatd -g video &

# Directorio de runtime para el socket de Wayland (modo 0700, del usuario).
export XDG_RUNTIME_DIR=/run/user/0
mkdir -p "$XDG_RUNTIME_DIR" && chmod 0700 "$XDG_RUNTIME_DIR"

# Render por software (sin GBM/EGL) y KMS legacy (no atómico, sin modifiers).
export WLR_RENDERER=pixman
export WLR_DRM_NO_ATOMIC=1     # innecesario sin drm.atomic; quítalo para probar la ruta atómica
export WLR_DRM_NO_MODIFIERS=1

labwc
```

> **Importante**: sin `WLR_RENDERER=pixman`, wlroots intenta primero el renderer
> GLES2 sobre GBM/EGL (Mesa). Como aquí no hay GPU, esa ruta puede **colgarse,
> fallar o funcionar extremadamente lenta**: Mesa cae a `llvmpipe` (render por
> CPU) y cada frame además se vuelve a copiar al framebuffer DRM por software.
> El síntoma típico del fallo de inicialización es que el log del kernel se queda
> en `[drm] VERSION …` y nunca llega al *scanout* (pantalla congelada). Si sí
> arranca con `WLR_RENDERER=gles2`, el compositor seguirá siendo muy lento por
> ese doble trabajo en CPU. Forzar pixman evita esa ruta por completo.

## Diagnóstico

Con `LOG=error`, el kernel registra el avance de la negociación DRM. Ojo: casi
todas las líneas de abajo son `log::debug!`, así que **no** se ven con
`LOG=error`; hace falta `LOG=debug` para la traza completa. Las que sí salen a
cualquier nivel son las de `klog_info!` (VERSION bajo nouveau, DROP_MASTER, la
cap de syncobj, el nodo de render, el primer present y el apagado del CRTC).
Una sesión sana (ruta legacy) imprime, en orden:

```
[drm] VERSION — /dev/dri/card0 opened by userspace (minor=0)
[drm] GET_CAP cap=0x1 -> 1
[drm] SET_CLIENT_CAP cap=2 -> accepted        # UNIVERSAL_PLANES
[drm] SET_CLIENT_CAP ATOMIC -> EOPNOTSUPP ... # sin drm.atomic (forzamos legacy)
[drm] SET_MASTER (minor=0)
[drm] GETRESOURCES: software KMS -> 1 crtc, 1 connector ...
[drm] GETCONNECTOR id=2 connected=true modes=1 ...
[drm] CREATE_DUMB 1920x1080 bpp=32 -> handle=1 ...
[drm] SETCRTC crtc=1 fb=1 ...
[drm] scanout: fb=1 ... -> display ...
```

Con `drm.atomic` y la ruta atómica activa se ve además:

```
[drm] SET_CLIENT_CAP ATOMIC=1 -> accepted
[drm] CREATEPROPBLOB len=68 -> blob=30000     # el MODE_ID del compositor
[drm] ATOMIC objs=3 test_only=true allow_modeset=true ...   # commit de prueba
[drm] ATOMIC objs=3 test_only=false ... fb=Some(1) ...      # modeset real
```

- Si se detiene en `VERSION` (o `minor=128`, el render node) y no aparece
  `GETRESOURCES`, es la inicialización del renderer GL: usa `WLR_RENDERER=pixman`.
- `[drm] render node: ioctl ... is NOT in Linux's DRM_RENDER_ALLOW set --
  allowed anyway for now` es lo que de verdad se imprime: el filtro observa y
  deja pasar. Nada devuelve EACCES todavía.
- Si llega a `SETCRTC`/`scanout` pero no se ve nada, el problema está en el
  *blit* al framebuffer (ver [`drm.rs`](../linux-object/src/fs/devfs/drm.rs)).
- `[drm] UNHANDLED ioctl …` indica un ioctl que labwc pide y aún no manejamos
  (el `drm nr` identifica el `DRM_IOCTL_*`).

La consola de texto del kernel cede a gráficos (`KD_GRAPHICS`) solo en el primer
*scanout* real, no al hacer `SET_MASTER`: así, si labwc se atasca antes de
pintar, el terminal sigue usable y sus logs visibles. El modo se aplica al VT
que el compositor reclamó en su primer present; si el usuario cambia de VT a
mitad de un frame, el VT de texto conserva su modo y se repinta.

## Cómo probar (Xorg)

- **Xorg**: driver `fbdev` sobre `/dev/fb0`. Ver [`README-xorg.md`](README-xorg.md).

## `fbdev` (`/dev/fb0`, API legacy de framebuffer)

| ioctl | Estado |
|---|---|
| `FBIOGET_VSCREENINFO` / `FBIOGET_FSCREENINFO` | ✅ |
| `FBIOPUT_VSCREENINFO` | 🟡 (resolución fija; devuelve la real) |
| `FBIOPAN_DISPLAY` / `FBIOBLANK` | 🟡 (no-op) |
| `FBIOGETCMAP` / `FBIOPUTCMAP` | 🟡 (no-op; TrueColor) |

## Mapa de archivos

| Archivo | Rol |
|---|---|
| `linux-object/src/fs/devfs/drm_scheme.rs` | dispatch de ioctls de `/dev/dri/card*` (incl. atómico, blobs, filtro de render node) |
| `linux-object/src/fs/devfs/drm.rs` | núcleo DRM: GEM, framebuffers, *scanout*, eventos, KMS sintético, commit atómico |
| `linux-object/src/fs/devfs/fbdev.rs` | `/dev/fb0` (API `fbdev` legacy) |
| `linux-object/src/fs/dmabuf.rs` | dma-buf (PRIME) |
| `drivers/src/scheme/drm.rs` | trait `DrmScheme` para drivers (virtio-gpu, nvidia) |
| `drivers/src/virtio/gpu.rs` | driver virtio-gpu |
| `linux-object/src/fs/sysfs.rs` | `/sys/class/drm/card0` |
