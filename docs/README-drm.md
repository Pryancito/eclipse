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
| `/dev/dri/renderD128` | 226:128 | ✅ nodo de render; solo `DRM_RENDER_ALLOW` (modeset/dumb/master → EACCES) |
| `/dev/fb0` | 29:0 | ✅ framebuffer legacy (`fbdev`) |
| `/sys/class/drm/card0` | — | ✅ entradas mínimas en sysfs |

### Render nodes (`drm-uapi.rst` → *Render nodes*)

Linux hace que `renderD128` **solo** acepte los ioctls marcados
`DRM_RENDER_ALLOW` en `drm_ioctl.c` (`VERSION`, `GET_CAP`, `GEM_CLOSE`,
`PRIME_*`, `SYNCOBJ_*`, `SET_CLIENT_NAME` y el rango de comandos del driver) y
devuelva **EACCES** a cualquier ioctl de modeset, *dumb buffers* o master/auth.
Eclipse aplica el mismo filtro (`render_allowed` en `drm_scheme.rs`): el
escritorio pincha KMS en `card0` con `WLR_DRM_DEVICES=/dev/dri/card0`, así
que el nodo de render ya no es un segundo KMS abierto.

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

Y ninguno pide alineación: Linux lee y escribe los arreglos anidados con
`get_user`/`put_user`/`copy_to_user`, que toman cualquier dirección, y aquí lo
hacen `uget`/`uput`/`ucopy_out` (`read_unaligned`/`write_unaligned`, junto a
`ucheck_n`). Hasta el 20-oct-2026 los 25 arreglos anidados (los conectores de
`SETCRTC`, los clips de `DIRTYFB`, los ids y arrendatarios de las *leases*, el
encoder y las propiedades de `GETCONNECTOR`, los formatos de `GETPLANE`, los
valores y enums de `GETPROPERTY`, las listas de ids y de propiedades, los
handles y puntos de los syncobj y los cuatro arreglos de `ATOMIC`) se leían con
un deref directo (`*(ptr as *const T).add(i)`): con un puntero impar es UB, y
con las aserciones de depuración un aborto del kernel entero.

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
| `DRM_IOCTL_VERSION` | ✅ | nombre `zcore`, versión 1.0.0 (`nouveau` 1.4.0 con el uAPI nouveau, `eclipse-compute` 0.1.0 en el nodo de cómputo); longitudes `strlen` sin NUL, como `drm_copy_field` |
| `DRM_IOCTL_GET_UNIQUE` | ✅ | el *busid* del master como `drm_getunique`: vacío (longitud 0) hasta que `SET_VERSION` pide la interfaz 1.1 o posterior (`drm_set_busid`), luego `pci:DDDD:BB:SS.F` del dispositivo PCI que nombra el enlace `device` del nodo en sysfs (`drm_pci_set_busid`; `zcore-gpu` para un nodo sin PCI detrás); copia solo si cabe entero, sin NUL, y un `unique_len` con puntero nulo es EFAULT. Se va con el master (`DROP_MASTER` o cierre). Xorg lo parsea para encontrar el `pci_device` del nodo y elegir la GPU primaria |
| `DRM_IOCTL_SET_VERSION` | ✅ | interfaz 1.4 y la versión del driver que da VERSION; valida el par pedido (major y minor) como `drm_setversion` (lo usa Xorg/modesetting); una interfaz 1.1+ aceptada fija el *busid* que da GET_UNIQUE |
| `DRM_IOCTL_GET_MAGIC` / `AUTH_MAGIC` | ✅ | magic por fichero, minted una vez; `AUTH_MAGIC` solo desde el master (EACCES), de un magic de ese nodo (EINVAL) y una sola vez; autentica al dueño del magic (`drm_file.authenticated`, que también tiene quien toma el master y todo `open` de root, como en `drm_file_alloc`) |
| `DRM_IOCTL_SET_MASTER` / `DROP_MASTER` | ✅ | un master por nodo: el primer `open` lo toma (`drm_master_open`), `SET_MASTER` con otro fichero como master es EBUSY, `DROP_MASTER` sin serlo es EINVAL; se suelta al cerrar el fd; conmuta la consola de texto del kernel (KD_GRAPHICS/KD_TEXT) |
| `DRM_IOCTL_GET_CLIENT` / `GET_STATS` / `FINISH` / `SET_UNIQUE` / `BLOCK` / `UNBLOCK` / `ADD_DRAW` / `RM_DRAW` / `UPDATE_DRAW` / `MODE_ATTACHMODE` / `MODE_DETACHMODE` | ✅ | los huecos de `drm_ioctl.c`: `GET_CLIENT` solo responde `idx == 0` (con `auth` = `authenticated`, el pid del que llama, `uid` = *overflowuid* y magic/iocs a 0; otro idx es EINVAL, lo que para la enumeración de libva), `GET_STATS` pone la struct a cero, `FINISH` es un no-op `DRM_AUTH` (EACCES sin autenticar), `SET_UNIQUE` es `drm_invalid_op` y `BLOCK`…`UPDATE_DRAW` no-ops, todos `DRM_ROOT_ONLY` (EACCES sin `CAP_SYS_ADMIN`; el master no se exige, ver más abajo), `ATTACHMODE`/`DETACHMODE` no-ops |
| `DRM_IOCTL_GET_CAP` | ✅ | ver tabla de *caps* |
| `DRM_IOCTL_SET_CLIENT_CAP` | ✅ | `ATOMIC` según `drm.atomic` (ver sección atómico), y como `drm_setclientcap` su valor se guarda a la vez en `atomic`, `universal_planes` y `aspect_ratio_allowed` («No atomic user-space blows up on aspect ratio mode data»): un cliente atómico recibe y devuelve los códigos de aspecto sin pedir `ASPECT_RATIO`, y `ATOMIC` 0 se los quita; `WRITEBACK` solo para clientes atómicos (EINVAL como Linux); `UNIVERSAL_PLANES` y `ASPECT_RATIO` booleanos que se recuerdan por open; `STEREO_3D` aceptado; `CURSOR_PLANE_HOTSPOT` EOPNOTSUPP |
| `DRM_IOCTL_WAIT_VBLANK` | 🟡 | vblank sintético ~60 Hz. Modo evento: respeta `RELATIVE`/`ABSOLUTE`/`NEXTONMISS` y entrega `DRM_EVENT_VBLANK` cuando el contador alcanza la secuencia pedida (nunca antes del siguiente vblank). Modo bloqueante: espera de verdad, hasta el tope de 3 s de Linux (`sys_ioctl` duerme antes del arm síncrono; ver `README-async-ioctl-vblank.md`) |
| `DRM_IOCTL_CRTC_GET_SEQUENCE` / `CRTC_QUEUE_SEQUENCE` | ✅ | `GET_SEQUENCE`: CRTC desconocido ENOENT; `active` es el `mode_valid` de GETCRTC (`crtc_state->enable`), `sequence` el mismo contador que `WAIT_VBLANK` y `sequence_ns` el instante del vblank que cuenta (`drm_vblank_count_and_time`). `QUEUE_SEQUENCE`: CRTC (ENOENT) antes que los flags (fuera de `RELATIVE \| NEXT_ON_MISS`, EINVAL); el objetivo se resuelve como el modo evento de `WAIT_VBLANK` y la respuesta es la secuencia a la que se encoló (la actual si ya pasó); el `DRM_EVENT_CRTC_SEQUENCE` (`user_data`, `time_ns`, `sequence` de 64 bits) llega con ese vblank y nunca antes del siguiente (Linux lo manda al instante si ya pasó). Xorg (`ms_get_crtc_ust_msc`, `ms_queue_vblank`) los sondea al arrancar; sin ellos caía a `drmWaitVBlank` |

### GEM / *dumb buffers* / PRIME

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_MODE_CREATE_DUMB` | ✅ | memoria física contigua vía VMO; *pitch* = `ceil(bpp/8) * width` alineado a 64 B; `size` = `pitch * height` redondeado a página, como todo `dumb_create` (un objeto GEM son páginas enteras); ancho, alto o bpp a 0 son EINVAL como en `drm_mode_create_dumb`. El `mmap` comparte el VMO del buffer (cacheado, WB): el mapeo y cada framebuffer construido sobre el handle mantienen la memoria viva tras `DESTROY_DUMB`, como el refcount de `drm_gem_object` |
| `DRM_IOCTL_MODE_MAP_DUMB` | ✅ | *offset* = `handle << 12`; `mmap` mapea el VMO físico |
| `DRM_IOCTL_MODE_DESTROY_DUMB` | ✅ | |
| `DRM_IOCTL_GEM_CLOSE` | ✅ | |
| `DRM_IOCTL_PRIME_HANDLE_TO_FD` / `FD_TO_HANDLE` | ✅ | dma-buf real (fd de proceso); despachado en la capa de syscalls porque necesita la tabla de fds. El export busca el handle en el fichero primero (`ENOENT` si nadie lo creó o es de otro proceso, como `drm_gem_object_lookup`) y rechaza con `EPERM` un objeto nouveau creado `NO_SHARE` (`nouveau_gem_prime_export`), tenga o no dirección de host. **Un objeto solo VRAM existe pero no se puede exportar desde aquí** (el dma-buf se monta sobre una dirección física de host y no tiene; Linux sí lo exportaría): `EINVAL`, lo que daba todo export antes, y no `ENOENT`; el driver (`gem_aperture`) es quien distingue «existe sin dirección» de «no existe», y se le pregunta **a la tarjeta del nodo en que se abrió el fichero y en nombre del que llama** (`drm_gem_object_lookup(file_priv, handle)`: un handle nombra un objeto de UN fichero en UNA tarjeta): el objeto VRAM de otro proceso, o el que solo tiene la otra tarjeta, es `ENOENT` (antes se preguntaba a la primaria y por handle a secas: `EINVAL` para el ajeno, `ENOENT` para el de `card1` por su propio nodo). El import devuelve el handle que el proceso ya tiene sobre ese mismo buffer (el propio, al reimportar lo que exportó, o el de un import anterior), como `drm_prime_lookup_buf_handle`, cuya tabla llenan el export y el import; antes cada import de un dma-buf genérico creaba un handle nuevo (el de un objeto nouveau ya volvía al original). La tabla de Linux es por fichero y aquí los handles son por proceso. El fd de dma-buf tiene `DMA_BUF_IOCTL_SYNC`, `SET_NAME`, y `EXPORT_SYNC_FILE` / `IMPORT_SYNC_FILE` (también en la capa de syscalls): flags sólo READ/WRITE y al menos uno (`EINVAL`), el export es un `sync_file` close-on-exec con las fences del buffer (las sondas de anillo del driver y lo importado, según `dma_resv_usage_rw`), el import exige un `sync_file` (`EINVAL` si no) y su fence la esperan `CPU_PREP`, el present legacy y el siguiente export. Antes los dos eran `ENOTTY` |
| `DRM_IOCTL_GEM_FLINK` / `GEM_OPEN` | ❌ | interfaz legacy insegura; los clientes nuevos deben usar PRIME (igual que recomienda `drm-uapi.rst`) |

### Framebuffers

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_MODE_ADDFB` | ✅ | el id del fb sale del mismo contador que los blobs de propiedad (desde 30000, por encima de todo id KMS y del rango EDID), como el idr único de Linux: ningún fb comparte número con un CRTC, un plano, una propiedad o un blob |
| `DRM_IOCTL_MODE_ADDFB2` | ✅ | `framebuffer_check` en su orden para el único plano de aquí (XR24/AR24 lineales): flags fuera de `INTERLACED \| MODIFIERS` EINVAL, `MODIFIERS` EINVAL (sin soporte), formato desconocido o no escaneable EINVAL, anchura o altura 0 EINVAL, sin handle EINVAL, una fila de píxeles que no cabe en u32 o un plano cuyo último byte pasa de 4 GiB **ERANGE**, pitch menor que la fila EINVAL, modifier sin la bandera EINVAL; en los planos que el formato no tiene solo se mira el modifier (handle, pitch y offset solo cuentan bajo `MODIFIERS`). Local: `offsets[0]` debe ser 0 y no hay `max_width` (un fb mayor que el panel se recorta) |
| `DRM_IOCTL_MODE_RMFB` | ✅ | apaga el CRTC que mostraba el fb, como `drm_framebuffer_remove` (la salida de proceso conserva la última imagen para la restauración de consola) |
| `DRM_IOCTL_MODE_CLOSEFB` | ✅ | Linux 6.6+: suelta la referencia sin apagar el plano; `pad` distinto de 0 es EINVAL antes de buscar el fb, y un id que no es del fichero ENOENT (`drm_mode_closefb_ioctl`) |
| `DRM_IOCTL_MODE_GETFB` | ✅ | devuelve geometría + handle (cliente master único) |
| `DRM_IOCTL_MODE_GETFB2` | ✅ | formato `XR24`, plano 0 |
| `DRM_IOCTL_MODE_DIRTYFB` | ✅ | blitea la unión de los `drm_clip_rect` (sin *clips*, o si excede el límite, re-escanea todo); los bordes horizontales se expanden a líneas WC de 64 B para no manchar píxeles vecinos. Las comprobaciones de `drm_mode_dirtyfb_ioctl` en su orden: fb desconocido ENOENT; cuenta sin puntero o puntero sin cuenta EINVAL; las banderas se **enmascaran** a `ANNOTATE_COPY \| ANNOTATE_FILL` (un bit que Linux no define se ignora, no es EINVAL); `ANNOTATE_COPY` con cuenta impar EINVAL; más de 256 clips EINVAL |

### KMS (modeset legacy)

| ioctl | Estado | Notas |
|---|---|---|
| `DRM_IOCTL_MODE_GETRESOURCES` | ✅ | 1 CRTC + 1 connector + 1 encoder sintéticos. Las listas siguen el patrón de dos llamadas de Linux: se copia hasta donde el `count` deja (los encoders y modos de `GETCONNECTOR` y los formatos de `GETPLANE`, sólo si cabe la lista entera) y vuelve el total; un `count` con puntero nulo es EFAULT (`put_user`), no un salto |
| `DRM_IOCTL_MODE_GETCRTC` / `SETCRTC` | ✅ | `SETCRTC` con modo hace *scanout* del fb, y con modo y la lista de conectores vacía es EINVAL («Count connectors is 0 but mode set», tras el viewport y antes de leer la lista; cada conector de la lista se busca por id, `drm_connector_lookup`); sin modo apaga el CRTC y quita el modo (el fb que nombre se ignora, como `drm_mode_setcrtc`, y la lista vacía es la suya); `GETCRTC` devuelve el modo actual (`mode_valid=1`) solo mientras el CRTC tiene modo (`crtc_state->enable`): tras un `SETCRTC` sin modo o un `RMFB` del fb en pantalla responde `mode_valid=0` y `fb_id=0` hasta el siguiente modeset; DPMS off no lo quita. Un `SETCRTC` con modo es además un commit del estado que lee la uAPI atómica (`__drm_atomic_helper_set_config`): `ACTIVE=1`, `MODE_ID` con un blob del kernel que guarda ese modo (el mismo id en cada modeset), el conector enrutado al CRTC y el plano primario cubriendo el CRTC desde `(x, y)` del fb (`CRTC_X/Y=0`, `CRTC_W/H` = área activa, `SRC_X/Y = x<<16, y<<16`, `SRC_W/H` = área activa `<<16`); sin modo, y en el `RMFB` del fb en pantalla (`atomic_remove_fb`), se van el modo y `ACTIVE` y el conector se desengancha, pero los rectángulos del plano quedan como estaban. `GETCRTC` devuelve `x`/`y` = `SRC_X>>16`/`SRC_Y>>16` del plano primario, como `drm_mode_getcrtc` en un driver atómico, y el modo es el que puso el último `SETCRTC` (`crtc_state->mode`, el blob de `MODE_ID`) tal como lo devuelve `drm_mode_convert_to_umode`: los timings del cliente, `vrefresh` recalculado con `drm_mode_vrefresh` (×2 con `INTERLACE`, ÷2 con `DBLSCAN`, ÷`vscan`, redondeado al más cercano), `type & DRM_MODE_TYPE_ALL`, el nombre a 31 bytes y relleno de NUL (`strscpy_pad`) y los bits de aspecto solo si ese open puso `DRM_CLIENT_CAP_ASPECT_RATIO` (`drm_file.aspect_ratio_allowed`, por open); solo la consola de arranque, que ningún modeset puso, devuelve los timings nativos. El modo pasa por `drm_mode_validate_basic`: una bandera fuera de `DRM_MODE_FLAG_ALL` (`BCAST`, `PIXMUX`, bits por encima del campo de aspecto) o un layout 3D más allá de `SIDE_BY_SIDE_HALF` son EINVAL. Un `x` o `y` con bits por encima del 16 es ERANGE antes de nada («Universal plane src offsets are only 16.16»: antes del CRTC y del fb, con o sin modo), y un `clock` o `vrefresh` mayor que `INT_MAX` es ERANGE (`drm_mode_convert_umode`), tras el fb y antes del código de aspecto. Un código de aspecto distinto de NONE sin `DRM_CLIENT_CAP_ASPECT_RATIO` es EINVAL («Unexpected aspect-ratio flag bits»), tras el fb y antes de `drm_mode_convert_umode` |
| `DRM_IOCTL_MODE_GETENCODER` | ✅ | encoder `VIRTUAL`, `possible_crtcs=1`; `crtc_id=0` mientras el CRTC no tiene modo |
| `DRM_IOCTL_MODE_GETCONNECTOR` | ✅ | 1 modo = resolución nativa (preferido); propiedades estándar (ver abajo); `encoder_id=0` mientras el CRTC no tiene modo |
| `DRM_IOCTL_MODE_GETPLANERESOURCES` | ✅ | 1 plano primario |
| `DRM_IOCTL_MODE_GETPLANE` | ✅ | formatos `XR24`/`AR24`; `crtc_id` y `fb_id` son el CRTC y el framebuffer que el plano primario muestra (como `plane->state->crtc`/`->fb`): 0 y 0 con el CRTC apagado por `SETCRTC` sin modo o `RMFB` del scanout, sin cambio con DPMS off, y el fb sigue a cada flip; `gamma_size` vuelve a 0 tras la búsqueda (`drm_mode_getplane`: «FIXME: Plane gamma»), no el valor que trajera el cliente, y con ENOENT no se toca; el id de fb propio del driver nunca sale |
| `DRM_IOCTL_MODE_SETPLANE` | ✅ | equivale a *scanout* del fb (ruta primaria SW); escribe los ocho rectángulos del plano tal como llegan (`drm_atomic_helper_update_plane`), que `GETCRTC` (`x`/`y`) y las propiedades del plano leen después; un rectángulo de CRTC que desborda `int` (`crtc_w`/`crtc_h` > INT_MAX, o `crtc_x + crtc_w` / `crtc_y + crtc_h` > INT_MAX) → **ERANGE**, tras la máscara de CRTCs y antes del rectángulo fuente (`__setplane_check`); con `fb_id=0` no hace nada (Linux desactiva el plano, y si es el primario cada driver decide si lo admite con el CRTC encendido) |
| `DRM_IOCTL_MODE_PAGE_FLIP` | ✅ | *scanout*; `DRM_EVENT_FLIP_COMPLETE` (con `crtc_id` y la secuencia de vblank) solo con `PAGE_FLIP_EVENT`; `ASYNC`/`TARGET_*`/flags desconocidos → EINVAL, fb desconocido → ENOENT; el fb nuevo tiene que contener el rectángulo fuente del plano primario (`drm_framebuffer_check_src_coords` con `SRC_X/Y/W/H`, ENOSPC: un viewport desplazado por `SETCRTC`/`SETPLANE`/ATOMIC cuenta), y el modo sólo mide cuando el plano nunca se ha posicionado (aquí se puede empezar con un flip; Linux da EBUSY) |
| `DRM_IOCTL_MODE_OBJ_GETPROPERTIES` | ✅ | tabla de propiedades por objeto; las propiedades `DRM_MODE_PROP_ATOMIC` solo se muestran a clientes atómicos (mismo filtrado que Linux). Un fb, un blob o una propiedad como `obj_id` es EINVAL (`drm_mode_object_find` los encuentra en el mismo idr, con `ANY` o su propio tipo, y no tienen tabla: `!obj->properties`); con otro tipo, ENOENT, como cualquier búsqueda; un id que no es nada sigue siendo ENOENT. El encoder sintético, que `GETRESOURCES` nombra, contesta una lista vacía donde Linux diría EINVAL |
| `DRM_IOCTL_MODE_GETPROPERTY` | ✅ | metadatos completos: flags, nombre, rangos/enums/tipo de objeto |
| `DRM_IOCTL_MODE_OBJ_SETPROPERTY` / `SETPROPERTY` | ✅ | **DPMS sí actúa** (`drm_atomic_connector_commit_dpms`): cualquier valor distinto de "On" apaga el CRTC (pantalla en negro y se dejan de repintar el cursor y el daño), "On" lo enciende y vuelve a mostrar el fb del plano primario sin esperar al siguiente frame, y la propiedad se relee con el valor puesto; con el conector enrutado a un CRTC (modo puesto) el `ACTIVE` del CRTC sigue al DPMS (off = `ACTIVE 0`, on = `ACTIVE 1`, y un ATOMIC que cambie `ACTIVE` después necesita `ALLOW_MODESET`), sin CRTC enrutado sólo cambia el conector (y aquí también el panel: la consola de arranque no está en el estado atómico). Un ATOMIC con `ACTIVE 0 → 1` sin `FB_ID` también vuelve a mostrar el fb del plano. El resto es `set_property_atomic` como en `drm_mode_obj_set_property_ioctl`: con `drm.atomic` un commit atómico de esa sola propiedad, sin evento y sin `ALLOW_MODESET` (un `FB_ID` en el plano primario es un flip; `ACTIVE`/`MODE_ID` que cambian el estado son EINVAL; un `OUT_FENCE_PTR` escribe -1 y no hay fence, que sólo la hace el ioctl ATOMIC); sin `drm.atomic` es `set_property_legacy`, y este KMS no tiene `set_property` más que para DPMS: EINVAL. Un fb, un blob o una propiedad como `obj_id` es EINVAL (`!obj->properties`), no ENOENT, con `ANY` o su propio tipo; con otro tipo, ENOENT |
| `DRM_IOCTL_MODE_GETPROPBLOB` | ✅ | EDID + blobs del almacén de propiedades. Como `drm_mode_getblob_ioctl`: los bytes se copian sólo si `length` es **exactamente** la del blob (el patrón de dos llamadas de `drmModeGetPropertyBlob`), con EFAULT si el puntero no vale; `length` vuelve siempre con la del blob |
| `DRM_IOCTL_MODE_CREATEPROPBLOB` / `DESTROYPROPBLOB` | ✅ | almacén de blobs; `length` 0 (o más de 64 KiB, tope local) EINVAL antes de mirar el puntero, y un puntero nulo o inválido con longitud EFAULT (`copy_from_user`), no EINVAL; destruir un blob de otro fichero o del kernel es EPERM (`drm_mode_destroyblob_ioctl`: «ensure the property was actually created by this user»), no EACCES |
| `DRM_IOCTL_MODE_CURSOR` / `CURSOR2` | ✅ | cursor compuesto por el kernel sobre cada frame (`set_cursor_bo`/`move_cursor`); sin flag o con uno desconocido → EINVAL antes de buscar el CRTC (ENOENT), como `drm_mode_cursor_common`; un handle con ancho o alto 0, o más de 64×64 (el tamaño anunciado en las *caps*) → EINVAL; solo el handle 0 esconde el puntero |
| `DRM_IOCTL_MODE_ATOMIC` | ✅ | **opt-in** con `drm.atomic` (ver sección siguiente) |
| `DRM_IOCTL_MODE_GETGAMMA` / `SETGAMMA` | 🟡 | aceptados como no-op (`gamma_size=0`, sin `GAMMA_LUT`); devolver ENOTTY provocaba una tormenta de SETGAMMA desde Xorg |
| `DRM_IOCTL_MODE_CREATE_LEASE` / `LIST_LESSEES` / `REVOKE_LEASE` | 🟡 | los tres son `DRM_MASTER` (EACCES a quien no lo es). `CREATE_LEASE` valida como `drm_mode_create_lease_ioctl`: `flags` fuera de `O_CLOEXEC \| O_NONBLOCK` → EINVAL; con `object_count` la lista se lee (EFAULT) y cada id debe ser un CRTC, conector o plano (ENOENT si no existe, EINVAL si es un encoder), el conjunto lleva al menos un CRTC y un conector, y un plano si el cliente pidió `UNIVERSAL_PLANES` (EINVAL); sin planos universales cada CRTC arrastra su plano primario y de cursor, y un id repetido en el conjunto es ENOSPC (`idr_alloc`); un objeto que otro arrendatario vivo de este nodo aún tiene es EBUSY (`drm_lease_create` → `_drm_has_leased`), y revocado vuelve a estar libre. El `lessee_id` sale del menor libre desde 1 por nodo. **El fd no restringe nada**: es un `dup` del fd del arrendador, así que un cliente "arrendado" conserva el control completo del dispositivo. `LIST_LESSEES` (`pad` → EINVAL) lista los arrendamientos con objetos, tantos como quepan y el total en `count_lessees`; `REVOKE_LEASE` vacía uno (ENOENT si no existe; repetirlo es un no-op). `GET_LEASE` no existe |
| `DRM_IOCTL_SYNCOBJ_*` | 🟡 | implementados (`CREATE`/`DESTROY`/`WAIT`/`RESET`/`SIGNAL`/`TIMELINE_*`/`QUERY`/`TRANSFER`, `drivers/src/scheme/syncobj.rs`) pero solo activos con `nvidia.nouveau_uapi`; sin él EOPNOTSUPP y `DRM_CAP_SYNCOBJ=0`. `TRANSFER` lee su petición en el orden de `drm_syncobj_transfer_ioctl`: `pad` → EINVAL, destino desconocido → ENOENT, un flag fuera de `WAIT_FOR_SUBMIT` → EINVAL, origen desconocido → ENOENT; sin `WAIT_FOR_SUBMIT` el origen debe llevar ya el punto pedido (EINVAL, `dma_fence_chain_find_seqno`) |

### Propiedades KMS estándar (`drm-kms.rst` → *KMS Properties*)

| Objeto | Propiedad | Tipo | Notas |
|---|---|---|---|
| plane | `type` | enum inmutable | {Overlay, Primary, Cursor} |
| plane | `FB_ID`, `CRTC_ID` | object, atómica | |
| plane | `CRTC_X/Y/W/H`, `SRC_X/Y/W/H` | range, atómicas | `SRC_*` en 16.16 |
| CRTC | `ACTIVE` | range 0–1, atómica | |
| CRTC | `MODE_ID` | blob, atómica | blob de `drm_mode_modeinfo` (68 B) |
| connector | `DPMS` | enum | "On" u "Off" según el CRTC esté encendido; la escritura apaga o enciende el CRTC y mueve su `ACTIVE` si está enrutado (fila de `OBJ_SETPROPERTY`) |
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
- El CRTC y su conector van juntos, sobre el estado en el que el commit
  **deja** al CRTC (`drm_atomic_helper_check_modeset`, "enabled/connectors
  mismatch"): un modo se pone con el `CRTC_ID` del conector en el mismo commit
  y se quita con él a 0; un modo sin conector, un conector sin modo o un
  `CRTC_ID = 0` del conector con el modo puesto son EINVAL. `ACTIVE` sin modo
  en el estado resultante es EINVAL sea `ACTIVE` o `MODE_ID = 0` lo que el
  commit nombre (`drm_atomic_crtc_check`, "active without enabled"). El
  `CRTC_ID` del conector se relee mientras hay modo, activo o no.
- `PAGE_FLIP_EVENT` encola un `DRM_EVENT_FLIP_COMPLETE` por CRTC del commit.
- `PAGE_FLIP_ASYNC` se rechaza (las *caps* async son 0).
- Objeto/propiedad desconocidos → ENOENT; un valor que la propiedad no
  admite → EINVAL **en la propiedad**, antes del commit, como
  `drm_property_change_valid_get`: un `FB_ID`/`CRTC_ID` que no nombra nada o
  supera 32 bits, un `MODE_ID`/`FB_DAMAGE_CLIPS` sin blob, un rango o enum
  fuera de lo anunciado (`IN_FENCE_FD` es `[-1, INT_MAX]`).
- La comprobación del plano (`drm_atomic_plane_check`) mira el estado en el
  que el commit **deja** al plano, no sólo lo que el commit nombra: CRTC y FB
  los dos puestos o los dos quitados (un `FB_ID = 0` o un `CRTC_ID = 0` a
  solas es EINVAL y no aplica nada; el plano se apaga y se enciende con los
  dos en el mismo commit), el rectángulo fuente en 16.16 dentro del fb con el
  que acaba (`src_w > ancho << 16 || src_x > (ancho << 16) - src_w`, un borde
  fraccionario cuenta) → **ENOSPC**, y cada clip de `FB_DAMAGE_CLIPS` un
  rectángulo real desde el origen (EINVAL; el borde lejano se compara con el
  tamaño del fb **desplazado 16 bits**, como hace Linux, así que un clip que
  se pasa en píxeles se acepta y se recorta al presentar); un blob de clips
  que no es un número entero de rectángulos es EINVAL al escenificar. En un
  plano que acaba apagado no se mira nada más. Un rectángulo de CRTC que
  desborda `int` (`crtc_x + crtc_w` o `crtc_y + crtc_h` > INT_MAX; `crtc_w` y
  `crtc_h` solos ya los para la propiedad) → **ERANGE** (`FsError::OutOfRange`),
  antes del rectángulo fuente, como `drm_atomic_plane_check`.
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
presenta igual), y la segunda es una fence de verdad, como en Linux: el slot
del cliente recibe `-1` en cuanto se escenifica la propiedad
(`put_user(-1, fence_ptr)`), el `sync_file` se crea e instala en su tabla de
fds **antes** del commit (`prepare_signaling`; un commit que falla lo retira y
deja el `-1`), y su fence **se señala cuando se entrega la completion del
flip**, en el vblank sintético, con el evento si se pidió `PAGE_FLIP_EVENT` y
sola si no. Un commit con solo `OUT_FENCE_PTR` debe esa completion igual que
uno con evento (y se rechaza igual sobre un CRTC apagado que sigue apagado).
Antes era un `sync_file` **ya señalado**: quien soltaba el buffer anterior al
verla firmada lo hacía con ese buffer todavía en pantalla.

Un commit que falla no deja nada a medias: el estado KMS, el framebuffer del
CRTC y el blob de modo del kernel se restauran, como hace
`drm_atomic_helper_swap_state`, que solo intercambia el estado duplicado
cuando la comprobación y el commit han ido bien. `ACTIVE=0` apaga el CRTC.

Limitaciones deliberadas: un solo modo (el nativo del panel), sin plano de
cursor atómico (el cursor legacy sigue compuesto por el kernel) y solo sobre la
ruta software-KMS (con un driver KMS por hardware la negociación atómica se
rechaza).

## Espera de fence antes de presentar (sincronía implícita)

La ruta atómica espera a `IN_FENCE_FD` desde que se escribió, pero la ruta
**legacy** — `SETCRTC` y `PAGE_FLIP`, la que de verdad corre esta máquina
porque `drm.atomic` es opt-in — no llevaba ninguna fence encima y no esperaba a
nada: `present_now_checked` leía el búfer en el instante en que llegaba el
ioctl. Un compositor que envía su dibujado y voltea sin dar la vuelta por la GPU
ponía en pantalla lo que la GPU hubiera terminado hasta ese momento, o sea
basura en la parte de abajo del frame.

Linux resuelve ese caso desde el *reservation object* del búfer:
`drm_atomic_helper_prepare_planes` recoge las fences que cuelgan del BO y el
commit las espera antes de programar el flip. Aquí no hay reservation object, y
el `EXEC` de nouveau **no lleva lista de búferes**, así que nada dice *qué*
envíos tocaron este búfer. Lo que sí se puede saber es en qué canal envía su
dueño y si ese canal ha drenado — la misma pregunta que responde
`GEM_CPU_PREP`—, y se equivoca en la dirección segura: puede esperar por
trabajo que no tocó el búfer, nunca pasar por alto trabajo que sí.

El dueño es quien **asignó** el objeto GEM, no quien creó el framebuffer: con
*direct scanout* el cliente dibuja en su propio objeto y el compositor lo
importa y hace `ADDFB`, así que el fb lleva el pid del compositor mientras los
píxeles siguen saliendo del canal del cliente.

Detalles:

- Tope de **100 ms**, el mismo que la espera atómica y por el mismo motivo:
  presentar un frame a medias es un defecto visible, no presentar nunca es un
  cuelgue. Pasado el tope se presenta igual y se avisa por consola con
  presupuesto (8 líneas, luego calla).
- Se espera en la ruta **asíncrona** del `ioctl` (`sys_ioctl`, antes del brazo
  síncrono), durmiendo ~1 ms entre sondeos, no haciendo *spin*: girar aquí se
  come un núcleo entero, que es la inanición que ya causó `WAIT_VBLANK`.
- Un `SETCRTC` con `fb_id=0` apaga el pipe y no presenta nada, así que no
  espera.
- **Encendido por defecto.** La escotilla es `drm.flip_fence=off` en la
  `cmdline`, que devuelve el comportamiento anterior (presentar al instante,
  con *tearing* si la GPU sigue dibujando) sin recompilar.
- Se pregunta a **todos** los drivers registrados, no solo al primario: en una
  caja de dos GPUs la que dibuja no tiene por qué ser la que escanea (es
  justamente el reparto que corre este kernel, con la GPU de consola dueña del
  panel y la de cómputo dibujando).
- Sin driver NVIDIA registrado la espera no existe: el gancho del *trait*
  (`DrmScheme::render_fence_for_scanout`) devuelve `None` por defecto, así que
  el escritorio software de QEMU se comporta exactamente como antes.

## Huecos conocidos y justificación

- **Gamma/CTM/HDR**: sin LUTs (`gamma_size=0`, sin `GAMMA_LUT`/`CTM`); el
  *scanout* software no las aplica. Consecuencia visible: gammastep, la luz
  nocturna y la gamma de RandR no hacen nada.
- **Leases**: `CREATE_LEASE` valida la petición como Linux pero devuelve un
  `dup` del fd sin restringir nada, `GET_LEASE` no existe, y los
  arrendamientos son por nodo y duran hasta el apagado (un `DROP_MASTER` no
  los limpia). **Syncobj**: solo con la UAPI de nouveau (arriba).
- **Estado por apertura**: los eventos y `DRM_CLIENT_CAP_ATOMIC` **sí** van
  por fd abierto (`DrmFileState`, que `sys_open` crea por cada `open`). Los
  handles GEM, los framebuffers y el cursor siguen en tablas globales; para
  los handles y los framebuffers el pid del propietario cubre el acceso entre
  procesos (ver arriba), pero no aísla dos fds del mismo proceso. El master
  **sí** es por fd y por nodo (`drm::set_master`/`drop_master`, tomado por el
  primer `open` como en `drm_master_open`), pero **los ioctls que Linux marca
  `DRM_MASTER` todavía no lo exigen**: cualquiera puede emitirlos.
- **`drm-usage-stats.rst` (fdinfo)**. No se exponen estadísticas de
  uso/memoria/engine por `fdinfo`.
- **Render / 3D**. No hay aceleración: se usa el render por software de Mesa
  (`llvmpipe`/`softpipe`).

## Lo que aún diverge (auditoría del 20-sep-2026)

Por orden de lo que rompe a clientes reales. Lo de arriba ya está arreglado;
esto no:

| Divergencia | Qué rompe |
|---|---|
| ~~`SYNCOBJ_TRANSFER` pierde el `dst_point` en el camino software~~ | **Corregido:** `syncobj::transfer` lleva el `dst_point` al enlace de dependencia del destino (`drivers/src/scheme/syncobj.rs`), así que un *timeline* software llega al punto prometido y wlroots `linux-drm-syncobj-v1` libera el frame del cliente |
| ~~Un fd de `sync_file` no admite `SYNC_IOC_MERGE`~~ | **Corregido:** `SyncobjHandle::merge` crea un syncobj que llega a 1 cuando TODAS sus fuentes llegan (`syncobj::merge_fences`, la `dma_fence_array` de Linux). Mesa lo pide en cada adquisición de una imagen de swapchain ya presentada (fence de release del compositor + fence del present anterior); sin el ioctl la adquisición fallaba tras las primeras `image_count` frames y zink mataba el swapchain GLX (`zink: swapchain killed`, luego `GLXBadCurrentWindow`), sin nada en dmesg porque el error del cliente está compilado fuera. Errores como `sync_file_ioctl_merge`: `flags` o `pad` EINVAL, y un `fd2` que no nombra fichero (antes EBADF) o que no es un `sync_file` ENOENT (`sync_file_fdget`); el nombre se corta a 31 bytes y se devuelve terminado en NUL |
| ~~Destruir un syncobj soltaba a sus dependientes al punto prometido~~ | **Corregido:** un syncobj del que aún dependen enlaces y que puede progresar (fence HW en vuelo o enlace propio) se queda como huérfano hasta que nadie lo nombra; antes, el patrón de Mesa (surrogate + transfer + export + destroy inmediato) señalaba el semáforo de acquire antes de que el compositor soltara la imagen |
| ~~Los fd de `sync_file` no se pueden sondear~~ | **Corregido:** `SyncobjHandle::poll` reporta `POLLIN` cuando la fence alcanzó el punto; `subscribe_readiness` + el poller de fences HW despiertan `sys_poll` al aterrizar (sin eso `sync_wait()` de Mesa en GLX/DRI3 mataba el swapchain: `zink: swapchain killed`) |
| ~~El fd de `sync_file` no tenía `SYNC_IOC_FILE_INFO` ni `SYNC_IOC_SET_DEADLINE`, y los dos fds (`sync_file` y syncobj) respondían `ENOSYS` a `read`/`write` y decían ser `O_RDWR`~~ | **Corregido:** los dos son inodos anónimos abiertos con flags 0, como en Linux: `O_RDONLY` (`read` EINVAL, `write` EBADF, `pread`/`pwrite`/`lseek` ESPIPE). El fd de syncobj no tiene `poll` ni `ioctl` (`poll(2)` lo da siempre listo, `epoll_ctl` EPERM, cualquier ioctl ENOTTY, `SYNC_IOC_MERGE` incluido). El de `sync_file` tiene `SYNC_IOC_FILE_INFO` (estado, nombre y la única fence, con driver `eclipse`, timeline `syncobj`, `-ENODEV` si la fence venció, y el nombre que le dio `SYNC_IOC_MERGE`) y `SYNC_IOC_SET_DEADLINE` (comprueba el `pad` y no hace nada). Es lo que `sync_file_info()` de libsync y el `sync_wait` de Mesa consultan sin sondear |
| ~~`IN_FENCE_FD` con un fd que no es un `sync_file` (o que no existe) se presentaba sin esperar, un fd de syncobj se tomaba por su fence actual, y un segundo `IN_FENCE_FD` en el plano pisaba al primero~~ | **Corregido:** como `drm_atomic_plane_set_property`: el fd se resuelve con la semántica de `sync_file_get_fence` (solo un `sync_file`; cualquier otra cosa, EINVAL y el commit no se ejecuta) y un plano cuyo estado ya lleva fence no admite otro `IN_FENCE_FD`, ni siquiera el -1 (EINVAL) |
| ~~`OUT_FENCE_PTR` devuelve una fence ya señalada, y el slot del cliente no se toca si el commit no llega a ejecutarse~~ | **Corregido:** la fence se señala al entregarse la completion del flip (`drm_send_event_helper`), haya evento o no y aunque el fd de DRM se haya cerrado; el slot recibe `-1` al escenificar la propiedad y el fd solo si el commit se ejecuta. Quien marque el ritmo de frames con ella, o la entregue a un cliente como release de su buffer, espera ahora a que el buffer anterior haya salido de pantalla |
| El arm síncrono de `SYNCOBJ_WAIT` puede girar sin ceder la CPU | Una corrutina del kernel atascada; la máquina parece congelada |
| Sin caché de dma-buf por objeto ni de handles PRIME por fichero | Tope GEM subido a 256; sigue sin dedup PRIME por fichero |
| ~~`ADDFB2` no valida formato, flags ni modifiers~~ | **Corregido:** `framebuffer_check` entero (fila de la tabla de ioctls); un modifier con tiling es EINVAL |
| El límite de clips de `DIRTYFB` es 64 donde Linux usa 256, y `ANNOTATE_COPY` no está | Cada frame con más daño cae al blit de pantalla completa por CPU |
| Los ioctls `DRM_MASTER` no comprueban el master (que sí existe) | Todo ioctl `DRM_MASTER` lo puede emitir cualquiera |
| `GET_CLIENT` no existe | libva da la inicialización por fallida: sin VA-API |
| Sin `GAMMA_LUT`/`CTM` | gammastep, luz nocturna y la gamma de RandR no hacen nada |
| Sin `IN_FORMATS` en el plano | Inocuo mientras `DRM_CAP_ADDFB2_MODIFIERS` sea 0; obligatorio el día que se ponga a 1 |
| ~~`CRTC_GET_SEQUENCE`/`QUEUE_SEQUENCE` no existen~~ | **Corregido:** los dos existen sobre el contador sintético (fila de la tabla de ioctls); Xorg ya no sondea en vano ni cae a `drmWaitVBlank` |
| ~~El filtro del nodo de render solo observa~~ | Corregido: `renderD128` rechaza modeset/dumb/master con EACCES |
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
  `GETRESOURCES`, es la inicialización del renderer GL: usa `WLR_RENDERER=pixman`
  y asegúrate de `WLR_DRM_DEVICES=/dev/dri/card0` (el render node ahora
  rechaza modeset/dumb con EACCES).
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
