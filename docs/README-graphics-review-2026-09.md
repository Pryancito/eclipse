# Revisión del apartado gráfico de Eclipse OS (septiembre 2026)

Revisión de código de toda la pila gráfica sobre el commit `9757fe5`
("cambios en drm.", cabeza de `master` en GitHub el 6 de septiembre de 2026).
Cada hallazgo se ha verificado leyendo el código; los que no se pudieron
confirmar del todo van marcados como **PLAUSIBLE**. No se ha modificado
código: este documento es el entregable.

Alcance (≈36 k líneas):

| Área | Archivos |
|---|---|
| Núcleo DRM/KMS (UAPI) | `linux-object/src/fs/devfs/{drm,drm_scheme,fbdev}.rs`, `fs/{dmabuf,syncobj_file,syncobj_eventfd}.rs`, `linux-syscall/src/file/file.rs` (PRIME), `linux-syscall/src/vm.rs` (mmap) |
| Drivers de display | `drivers/src/scheme/{display,drm,gem_mmap,syncobj}.rs`, `drivers/src/virtio/gpu.rs`, `drivers/src/utils/{shadow_fb,graphic_console}.rs`, `drivers/src/display/uefi.rs`, mock SDL |
| Driver NVIDIA / nouveau-UAPI | `drivers/src/display/{nvidia,nouveau_uapi,nvidia_hooks}.rs` (13,5 k líneas) |
| Userspace | `tools/lunarbg`, `tools/lunarbar`, `tools/drmbench`, `tools/eclipse-xwayland` |
| Build / sesión | `xtask/src/linux/{desktop,xorg,mod}.rs`, `tools/eclipse-init` |

Leyenda de severidad: **CRÍTICA** (memoria del kernel comprometida o pérdida
de trabajo) · **ALTA** (corrupción / cuelgue en uso normal) · **MEDIA**
(comportamiento incorrecto observable) · **BAJA** (deuda, drift, cosmética).

---

## 0. Antes de nada: `master` en GitHub ha perdido 20 commits

Esto no es un bug de código pero es lo más urgente del informe.

- `master` en GitHub es `9757fe5` → `ed8a4c4` → `d7cda02` → `98d1f63` …
- El clon de esta sesión conserva un `master` local en `307f7e0` con **20
  commits** que ya no están en la historia remota: los merges de los PRs
  **#1068, #1069, #1070, #1071 y #1072** y los commits propios `845d1b6`,
  `0295bc5`, `aa3b1aa`, `95b0a37` y `307f7e0`.
- GitHub confirma que #1072 está **merged** (`merged_at` 2026-09-05 21:56 UTC,
  merge commit `b497bfe`), pero ese merge commit no es alcanzable desde
  `master`. `9757fe5` ("cambios en drm.", 06-09 05:58 CEST) tiene como padre
  `ed8a4c4` (05-09 08:22 UTC), es decir, se hizo desde una copia local
  anterior a esos merges y se subió con `--force` (o equivalente).
- Prueba interna: `tools/lunarbar/src/main.rs:52` declara `mod i18n;` pero
  `src/i18n.rs` (añadido en `95b0a37`) no existe en el árbol. **lunarbar no
  compila en `master`** (`cargo check` → `E0583 file not found for module
  i18n`), así que cualquier rootfs construido desde `master` sale sin panel.

Entre lo perdido hay correcciones directamente relevantes para esta
revisión: el fence de `EXEC` con `RELEASE_WFI_EN` (#1072, ver F-A4), la
validación de ADDFB2 y el rechazo de scanout *tiled* (#1071), la
sincronización del cursor BO (`0a5fd93`), el paso de `clippy` limpio
(`a6addf1`), `linux-object/src/{ns,seccomp}.rs`, `tools/eclipse-nvkick`,
`docs/README-flatpak.md` y `scripts/vbox-eclipse.sh`.

**Recuperación sugerida** (los commits siguen en GitHub vía `refs/pull/*` y
en este clon):

```sh
git fetch origin master
git checkout -b recover-master 307f7e0          # el master local completo
git merge 9757fe5                                # re-aplica "cambios en drm."
# resolver conflictos (drm.rs, drm_scheme.rs, shadow_fb.rs, README-drm.md)
git push origin recover-master:master            # es fast-forward: el merge desciende de 9757fe5
```

Equivalente: sobre `master` (`9757fe5`) hacer `git merge 307f7e0` y un push
normal. En cualquier caso, conviene activar la protección de rama (*block
force pushes*) en `master`.

**Recuperado en esta rama.** El propietario confirmó que `master` se perdió,
y que ese `master` es donde tenía «ruidos visuales»; `9757fe5` ("cambios en
drm.") fue su corrección posterior. La rama fusiona `307f7e0` completo
(los 20 commits, PRs #1068-#1072 incluidos) con este criterio para los
conflictos:

- **Diseño de scanout/cursor/DIRTYFB: gana `9757fe5`** (el actual): blits
  parciales alineados a líneas WC (`expand_x_for_wc`), `DIRTYFB` con la
  unión de clips, cursor por parches y `DUMB_PREFER_SHADOW = 1`. El
  `master` perdido había ido por «present siempre a frame completo, DIRTYFB
  → ENOSYS, cursor en tira de ancho completo»; eso es lo que se descarta.
- **Del `master` perdido se conserva todo lo ortogonal**: `driver_for_nouveau`
  (elección de GPU para NVK), validación de `ADDFB2` (formato, offsets,
  modifier lineal, un solo plano; con tests), `clflush` del cursor BO antes
  de leerlo, sincronización FromDevice por bandas del GEM escrito por la GPU
  (`GemSrcSync`) y las banderas de coherencia del CE (`ce_present(…,
  coherent)`), el fence `RELEASE_WFI_EN`, los cookies mmap del canal de
  envío directo (`is_fast_mmap_handle`, para `libeclipse_nvkick`),
  `linux-object/src/{ns,seccomp}.rs`, `i18n.rs`, y todo lo demás de
  NVIDIA, init, xtask y docs.
- Del arreglo «cursor patch never blits unfilled scratch» se porta la idea
  al parche WC (`blit_cursor_patch` solo blitea las filas realmente
  copiadas).

Los hallazgos siguientes se refieren a `9757fe5`.

---

## 1. Críticos

### F-C1. Los ioctls desreferencian punteros de usuario sin validar (todo el árbol)

`linux-object/src/fs/file.rs:770` pasa `arg1` tal cual a
`INode::io_control(cmd, data)`, y `drm_scheme.rs` tiene 39 sitios
`unsafe { &mut *(data as *mut …) }` más punteros anidados
(`fb_id_ptr`/`modes_ptr`/`props_ptr` en GETRESOURCES/GETCONNECTOR,
`clips_ptr` en DIRTYFB `:1921`, `objs_ptr`… en ATOMIC, `handles`/`points` en
SYNCOBJ_*, `data` en los blobs, `name`/`date`/`desc` en VERSION). Lo mismo en
`fbdev.rs:353-370`, y el driver nouveau añade cuatro rutas más con contadores
controlados por el cliente (`nvidia.rs:10107, 10551, 10759, 10846, 11107,
9053, 7667-7680`).

- Escenario: `ioctl(card0, DRM_IOCTL_MODE_RMFB, NULL)` → #PF en el kernel
  (nunca EFAULT). Peor: `ioctl(card0, DRM_IOCTL_GET_CAP, <VA de kernel>)`
  lee 8 bytes y escribe `cap.value` en esa dirección. `renderD128` es 0666 y
  `DrmDev::metadata` documenta que el 0660 de `card0` no se aplica al abrir,
  así que **cualquier proceso** puede leer/escribir memoria del kernel.
- Además, los `&mut *` sobre memoria de usuario no alineada son UB; solo el
  brazo NVIF usa `read_unaligned`.
- Fix: pasar todo por `UserInPtr`/`UserOutPtr`/`UserInOutPtr` con
  comprobación de rango (como ya hacen `sys_drm_prime` y
  `sys_drm_syncobj_fd`), copiar las structs dentro/fuera y devolver EFAULT.
  Es sistémico (`dsp.rs`, `snd.rs`, `pty.rs` igual), pero DRM es con
  diferencia la mayor superficie y la única que recorre arrays de punteros.

### F-C2. `DESTROY_DUMB`/`GEM_CLOSE` liberan las páginas físicas con el `mmap` del cliente aún vivo

`drm.rs:2643-2657` (`gem_close`) elimina la entrada `(handle, Arc<VmObject>,
pid)`, la única referencia al VMO contiguo; `PhysFrame::drop`
(`kernel-hal/src/common/mem.rs:78`) devuelve los frames al allocator. Pero
`DrmDev::get_vmo` (`drm_scheme.rs:102-125`) le dio a `mmap` un
`VmObject::new_physical(handle.phys_addr, …)` **que no posee nada**, así que
el mapeo de usuario sobrevive a los frames.

- Escenario: CREATE_DUMB → MAP_DUMB → mmap → DESTROY_DUMB → seguir
  escribiendo por el mapeo. Los frames se reasignan a la siguiente reserva
  del kernel, a otro dumb buffer o a una pila de corrutina, y el cliente los
  pisa. `release_process` (`drm.rs:2593`) documenta exactamente este peligro
  como motivo de correr solo tras `vmar.clear()`; `gem_close` no tiene esa
  protección.
- Fix: que `get_vmo` devuelva el `Arc<VmObject>` original (o un
  `create_child`/slice de él) para que el mapeo sostenga la referencia; o
  un refcount real por GEM (tabla de handles + mapeos + fbs + dma-bufs),
  como `drm_gem_object` en Linux.

---

## 2. Altos

### F-A1. Los dumb buffers y `/dev/fb0` se mapean a userspace **sin caché (UC)**

`VMObjectPhysical` nace con `CachePolicy::Uncached`
(`zircon-object/src/vm/vmo/physical.rs:19`); nadie llama a
`set_cache_policy` en la ruta DRM/fbdev; `vmar.rs:519` mete la política en
los `MMUFlags` y `kernel-hal/src/bare/arch/x86_64/vm.rs:129-134` la traduce a
`PCD|PWT` = entrada PAT 3 = **UC**. El *retrofit* de `pat.rs` solo redefine
la entrada 7 y solo retipa el alias *physmap* del kernel.

- Consecuencia: en hardware real, pixman renderiza (lee y escribe: blending,
  copias de daño) sobre RAM sin caché. En QEMU/KVM no se nota porque EPT
  ignora el PAT del guest sin dispositivos *passthrough*. Es un candidato
  serio a explicar parte del "7-11 FPS en dual RTX" que los comentarios
  atribuyen solo a BAR1.
- Mismo problema en `fbdev.rs:281` (Xorg `fbdev` escribe a UC, Linux usa
  WC), en el mmap de GEM nouveau (`drm_scheme.rs:121`) y en el export PRIME
  (`drm.rs:526`). Curiosidad colateral: `linux-object/src/vdso.rs:117` mapea
  el vDSO también sin caché.
- Fix: `Cached` para dumb buffers y GEM en sysmem; `WriteCombining` para
  `/dev/fb0` y VRAM (y que `WriteCombining` emita el bit PAT, que `pat.rs`
  dice que ya hace cuando `pat_wc_ready`).

### F-A2. `GEM_CLOSE` deja framebuffers apuntando a memoria liberada

`DrmFramebuffer` (`drm.rs:161`) guarda solo `phys_addr/size`; `gem_close` no
toca `state.framebuffers` (a diferencia de `release_process`, `drm.rs:2618`).
`scanout_region` (`drm.rs:1002+`) y **`repaint_for_cursor` en cada
movimiento del ratón** leen esos frames ya reasignados y los pintan en
pantalla (fuga de información / basura). Secuencia legal en Linux:
CREATE_DUMB → ADDFB → SETCRTC → DESTROY_DUMB.
Fix: que el fb sostenga un `Arc<VmObject>`, o que `gem_close` retire los fbs
construidos sobre el handle.

### F-A3. `RMFB` del fb del CRTC con un flip pendiente entrega **dos** `FLIP_COMPLETE`, uno con `user_data = 0`

`drm.rs:683-737`: si `FLIP_EVENT_PENDING` e `is_crtc_fb`, encola un evento
sintético con `user_data 0`, pero no limpia `FLIP_EVENT_PENDING` ni retira el
job `PendingDrmTimer::Flip`; `queue_flip_event` (`drm.rs:2160`) entrega
después el real. wlroots ≥ 0.16 hace
`page_flip = data; conn = page_flip->conn` → **NULL deref, SIGSEGV del
compositor**; luego llega el evento real sobre un `wlr_drm_page_flip` ya
destruido. Xorg se salva solo porque busca `user_data` en una tabla. Como
SETCRTC con `fb_id = 0` es no-op (`drm_scheme.rs:1823-1829`), `crtc_fb` se
queda obsoleto y el caso es más frecuente que en Linux. Además,
`pending_rmfb` es inerte (el fb se elimina de `framebuffers` en ambas ramas
y no posee memoria) y el evento se codifica a mano duplicando
`push_drm_event`.
Fix: eliminar el evento sintético y resolver la vida del buffer con F-A2.

### F-A4. Driver NVIDIA: el fence de `EXEC` no prueba que el motor terminó (`RELEASE_WFI_DIS`)

`nouveau_uapi.rs:1357` y `eclipse_rm_init.c:2665,3098` liberan el semáforo
host con `WFI_DIS`: el PBDMA escribe el payload al *procesar* el método, con
GR/CE aún ejecutando. `SYNCOBJ_WAIT` vuelve, Mesa/wlroots reutilizan el
staging o muestrean la textura a medio escribir ("structured garbage" de los
docs). **Ya corregido en el `master` local perdido (PR #1072,
`cb3e197`)** — otra razón para recuperarlo.

### F-A5. Driver NVIDIA: `GEM_CPU_PREP` no espera nada mientras `EXEC` ya es asíncrono

`nvidia.rs:11565-11580` valida el handle y devuelve; su propio comentario
dice que es honesto "mientras EXEC sea síncrono", y el *direct submit*
devuelve antes de que el fence aterrice. `nouveau_ws_bo_wait` de Mesa vuelve
al instante con la GPU escribiendo. Fix: rastrear el último payload por GEM
(o por ctx) y esperar aquí.

### F-A6. Driver NVIDIA: carrera de doble `exec_fast_prepare`

`nvidia.rs:8827-8925`: se suelta el lock del slot entre comprobar
`Unprepared` y guardar; dos hilos del mismo proceso (NVK es multihilo; labwc
tiene dos instancias Vulkan) preparan ambos, el segundo pisa el `FastCtx`:
`next_payload` vuelve a 1 (`:8897`) y se pone a cero la *landing zone*
(`:8901`). Un fence del hilo A con payload 3 se compara contra una zona que
el stream de B escribe 1,2,3 → `fence_landed` (`syncobj.rs:169`) reporta
completado un fence viejo; se filtra además el primer mapeo USERD.
Fix: marcar el slot `Preparing` bajo el lock antes de llamar al RM.

### F-A7. Driver NVIDIA: handles GEM, canales y mapeos VM son globales por número

`nvidia.rs:11505` (GEM_INFO), `:8178` (VM_BIND MAP), `:7260`
(`nouveau_gem_close`), `:10449` (CHANNEL_FREE) y el mmap via
`gem_mmap::lookup` ignoran quién llama; un contador global desde
`0x8000_0001`. Cualquier proceso puede cerrar, mapear en su VAS (y leer o
escribir por GPU) los buffers del compositor, o liberar el canal 0 y hacer
que el siguiente `EXEC` del compositor falle. Como el *self-import* PRIME
reparte a propósito el mismo número a otro proceso, el fix necesita una
tabla por pid con entradas de importación, no un simple `owner == caller`.

### F-A8. La consola gráfica blitea frames enteros con las interrupciones apagadas

`drivers/src/utils/shadow_fb.rs:213-260` (`present_with_cursor`) y
`:171-192`: `self.inner.lock()` es `lock::Mutex` (spinlock con `push_off`) y
se mantiene durante todo `display.blit_from` (líneas 223, 250, 256). Un
scroll de consola 1080p empuja ~8 MB con IRQs apagadas: a los 42 MB/s que
`drm.rs:1045-1050` documenta para BAR1, ~200 ms sin timer, xHCI ni NIC, y
otras CPUs girando IRQ-off en el mismo lock. Alcanzable desde el timer IRQ
(`kernel-hal/src/common/console.rs:590-600`). El path DRM trocea a 128 filas
precisamente por esto (`blit_chunked`, `drm.rs:880`), la consola lo deshace.
`blit_cell` además hace `Vec::with_capacity` bajo el lock (`:277`).
Fix: copiar las filas sucias a un scratch bajo el lock y blitear tras
`drop(g)`, o trocear soltando el lock entre bandas.

---

## 3. Medios

### Núcleo DRM

- **F-M1. `PAGE_FLIP` ignora `flags`** (`drm_scheme.rs:1831-1837`): encola
  evento sin `DRM_MODE_PAGE_FLIP_EVENT` (la `VecDeque` de eventos crece sin
  límite y el fd queda siempre legible), acepta ASYNC/TARGET, y no valida
  `fb_id`/`crtc_id` (EIO en vez de ENOENT/EINVAL).
- **F-M2. Dos contadores de secuencia distintos**: `FLIP_SEQ` para flips
  (`drm.rs:2160`) vs `vblank_seq_now()` (tiempo) para vblank. Xorg Present
  lee el MSC con `drmWaitVBlank` (~millones) y recibe flips con msc ≈ 500 →
  MSC hacia atrás. Fix: sellar los flips con `vblank_seq_now()`.
- **F-M3. `WAIT_VBLANK` ignora `sequence` y RELATIVE/ABSOLUTE**
  (`drm_scheme.rs:1839-1889`); el bloqueante devuelve ya (bucle ocupado en el
  cliente, ver `README-async-ioctl-vblank.md`) y el de evento ignora el
  objetivo.
- **F-M4. Cursor BO sin límite de tamaño** (`drm.rs:1309-1361`): se
  anuncia `DRM_CAP_CURSOR_WIDTH/HEIGHT = 64` (`drm_scheme.rs:1479`) pero no
  se aplica; `px * 4` puede desbordar; la copia `Arc::from(src)` (hasta 64
  MiB) se hace **con `DRM_STATE` cogido (IRQ-off)**, y `CURSOR_PATCH` se
  redimensiona bajo otro lock IRQ-off en cada frame y cada movimiento.
- **F-M5. `KD_GRAPHICS` se estampa en el VT *activo* después del blit**
  (`drm.rs:1298` y `:2131`). La puerta de VT se evalúa antes, pero
  `blit_chunked` reactiva IRQs entre bandas: un Ctrl+Alt+F1 a mitad de frame
  pinta el resto sobre la consola y deja el VT de texto en KD_GRAPHICS (sin
  eco de teclado, sin repintado). Fix: `set_kd_mode_vt(owner)` solo si
  `active == owner`, y re-evaluar la puerta por banda.
- **F-M6. El filtro del render node es solo observación**
  (`drm_scheme.rs:1265-1300`) mientras `README-drm.md` afirma EACCES:
  `renderD128` es un segundo nodo KMS completo (una sonda Vulkan
  `wsi_display` puede hacer SET_MASTER/SETCRTC/CURSOR).
- **F-M7. Estado global compartido por todos los fds/procesos**: handles,
  fbs, eventos, cursor, master y `ATOMIC_CLIENT` (`drm.rs:117-140`) son una
  sola instancia. Un `SET_CLIENT_CAP` de `drm_info` o Xwayland cambia las
  propiedades que ve labwc; `GETRESOURCES` lista fbs de procesos muertos
  (nunca reclamados si su handle ya se cerró). El doc reconoce el síntoma
  Xwayland/DROP_MASTER; la causa es no tener estado por apertura.
- **F-M8. El commit atómico aplica estado antes de `present_now` y no
  revierte en error** (`drm.rs:2478-2534`); `TEST_ONLY` sí está limpio.
- **F-M9. ADDFB/ADDFB2 sin validación de formato**: `create_fb`
  (`drm.rs:608-666`) no comprueba `pitch >= width*cpp`; ADDFB ignora
  `bpp/depth`, ADDFB2 ignora `pixel_format/offsets/flags/modifier`
  (`drm_scheme.rs:1755-1795`). Un fb de 16 bpp o NV12 se escanea como
  XRGB8888 desde offset 0. `CREATE_DUMB` hace `bpp.max(32)` (`:1691`) e
  ignora `flags`. *(El master perdido tiene parte de esto en #1071.)*
- **F-M10. `blit_chunked` con IRQs apagadas por banda** (`drm.rs:880`):
  ~983 KiB por banda a 1920 de ancho; a 42 MB/s son ~23 ms IRQ-off × 9
  bandas por frame en hardware real.
- **F-M11. Doc drift en `README-drm.md`**: render node → EACCES (no),
  `DRM_CAP_ADDFB2_MODIFIERS = 1` (el código devuelve 0 a propósito,
  `drm_scheme.rs:1495`), `SYNCOBJ_*` ❌ (implementado, `drm_scheme.rs:220-237`
  + `drivers/src/scheme/syncobj.rs`, tras `nvidia.nouveau_uapi`).

### Drivers de display

- **F-M12. `VirtIoGpu::new_modern` es un stub que programa mal el
  dispositivo** (`virtio/gpu.rs:35-63`, `virtio_pci.rs:105-129`): escribe
  `device_status` por `&mut` sin `write_volatile`, no negocia features ni
  crea colas, fija 1024×768, usa BAR0 como framebuffer (`fb_vaddr = 0`,
  `fb_size` forzado a 3 MiB), `page_flip` siempre OK y `VIRTGPU_GETPARAM`
  devuelve 0 sin escribir el valor (`:246-251`). Funciona solo porque el
  Display registrado es el GOP de UEFI. Fix: transporte moderno de verdad o
  devolver `NotSupported`.
- **F-M13. `flush()` de virtio-gpu copia el frame completo al host, en
  espera activa y con IRQs apagadas** (`gpu.rs:132-137`), en cada present,
  incluido el parpadeo del cursor de consola; `flush_region` existe y nadie
  la usa. En x86 legacy la INTx nunca se reconoce (`virtio_pci.rs:23`)
  (tormenta de IRQ PLAUSIBLE).

### Driver NVIDIA

- **F-M14. `CHANNEL_ALLOC` (ruta ctx-0) mantiene el spinlock IRQ-off
  `nouveau_channels` durante `step16/17` + selftest (~1-2 s)**
  (`nvidia.rs:10209 → 10361-10421`, liberado en `:10437`).
- **F-M15. El rol compositor/cliente se infiere en cada llamada**
  (`nvidia.rs:10253-10256`, `10563-10568`): tras un crash del compositor un
  cliente puede tomar la ruta ctx-0 y hacer `VM_BIND` en el VAS del ctx 0
  mientras `EXEC` va a su ctx N → fallo de MMU. Fix: propiedad de ctx 0
  explícita y pegajosa.
- **F-M16. Timeouts duros de 1 s** (`syncobj.rs:120`, `nvidia.rs:8942,
  10680, 10807`) marcan el contexto WEDGED; Linux usa 10 s.
- **F-M17. `VM_BIND` MAP/UNMAP elimina el mapeo solapado entero en vez de
  partir el rango** (`nvidia.rs:8161-8213, 8302-8318`) — latente mientras
  NVK desmapee rangos exactos. `EXEC_PUSH_NO_PREFETCH` se ignora
  (`:8962-8968`, debería poner `GP_ENTRY1_SYNC` bit 31). `GEM_NEW` acepta
  hasta 4 GiB contiguos de RAM del host por llamada sin cuota (`:11380`).
- **F-M18. El arranque GSP de la GPU de consola se dispara desde un ioctl
  sin privilegios** (`ensure_console_gpu_brought_up`, `:7947 → :4717`) con
  `quiet=false`, pese a que los docs cuentan que cuelga el bus 7 de 9 veces.
  Carrera PLAUSIBLE: `nouveau_release_process` puede liberar un ctx que otro
  hilo aún construye (`:7741-7760` vs `:9334-9430`).

### Userspace

- **F-M19. lunarbg filtra un pool shm por cada rebuild** (`main.rs:717-745,
  371-421`): destruye los `wl_buffer` viejos y espera `release`s que **nunca
  llegan** (wayland-backend descarta eventos de objetos destruidos,
  `client_impl/mod.rs:791`). Cada cambio de escala/modo pierde 16,6 MiB a
  1080p (66 MiB a 4K). La premisa de `fill_guard.rs:94-97` ("unmap sería UAF
  en el compositor") es falsa: `munmap` solo afecta a este proceso. Mismo
  patrón en lunarbar (`main.rs:573-582`).
- **F-M20. lunarbar: `render()` dimensiona el slice shm con `bar.scale`, no
  con el pool asignado** (`main.rs:664-681`, `767-799`): un cambio de escala
  con un `Configure` del mismo tamaño lógico antes de `wl_output.done`
  escribe `scale²` veces más allá del memfd (SIGBUS). Ordenación
  PLAUSIBLE con wlroots. Y un cambio de escala sin cambio de tamaño lógico
  deja la barra `configured=false` para siempre (`:3307-3329`).
- **F-M21. drmbench**: el bench de vblank apunta al pipe 0 sin
  `DRM_VBLANK_HIGH_CRTC` (`drmbench.c:485-507`), la espera del flip no tiene
  timeout ni comprueba el tipo de evento (`:391-395`), `pageflip_latency_us`
  es en realidad el periodo medio (`:429`) y no restaura CRTC/cursor/master
  al salir (`:775-786`).

### Build / sesión

- **F-M22. Los respawn tras crash saltan las puertas `wait_socket`/
  `wait_path`** (`eclipse-init/src/main.rs:1388-1392` vs `start_service`
  `:801-825`): seatd muere → labwc se relanza antes de que exista
  `/run/seatd.sock` → bucle de backoff.
- **F-M23. La selección de renderer diverge en tres sitios**: init
  (`main.rs:1059-1065`, gles2 para no-NVIDIA), wrapper
  (`desktop.rs:1691-1697`, pixman sin `LIBGL_ALWAYS_SOFTWARE`) y
  `/etc/profile` (`mod.rs:890-907`). En QEMU la sesión de arranque va por
  GLES2/llvmpipe pero `labwc` desde un VT va por pixman. `WLR_DRM_NO_MODIFIERS`
  es incondicional en `CHILD_ENV` (`main.rs:163`) y condicional en el resto.
- **F-M24. `pulseaudio.service` espera 8 s a `/dev/snd/pcmC0D0p` antes que
  seatd/labwc** por el orden alfabético de arranque (`mod.rs:2573`,
  `main.rs:816-928`): en máquinas sin audio el escritorio tarda 8 s más.
- **F-M25. `GDK_PIXBUF_MODULE_FILE` apunta a un fichero que nada genera**
  (`desktop.rs:1491-1494`; el autostart que lo creaba se eliminó en
  `:1532-1550`).
- **F-M26. `README-desktop.md` describe una sesión que ya no existe**
  (autostart con swaybg/foot/waybar, config de waybar, backend `builtin` de
  seatd, `apk add swaybg waybar`), y `README-xorg.md` recomienda
  evdev + `AutoAddDevices false` mientras la config generada es libinput +
  `true` (`desktop.rs:1077-1110`).

---

## 4. Bajos y deuda

- `PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE` con las constantes intercambiadas
  (`linux-syscall/src/file/file.rs:799-800`; en `drm.h` 0x2d es HANDLE_TO_FD
  y 0x2e FD_TO_HANDLE). Lo tapa la heurística `h.fd < 0` (`:848`), que es
  justo el "misterio" que el comentario de `:830-840` describe.
- `MAX_LIVE_GEMS = 64` global (`drm.rs:432`) es un parche a una corrupción
  del allocator ("null fn-ptr #PF"), no una solución; conviene abrir un bug
  aparte para la causa.
- `VERSION`/`GET_UNIQUE` devuelven `*_len` incluyendo el NUL; `MAP_DUMB` no
  valida el handle; `DESTROY_DUMB`/`RMFB` ignoran el resultado; offset falso
  `handle << 12` sin separación por tamaño; `poll` siempre `write: true`;
  espacios de ids solapados (fb, handle, KMS 1-4, props 10-26) aunque el
  comentario de `drm.rs:26-31` diga lo contrario.
- `display.rs:170-171` construye un slice de 4 bytes más allá de `fb_size`
  cuando éste no es múltiplo del pitch; `draw_pixel` (`:249`) solo comprueba
  el primer byte. `MockDisplay::from_raw_parts` envuelve un `static` en un
  `Vec`. El formato GOP se asume BGRX sin leer `KCONFIG.fb_mode`.
- NVIDIA: `VramAllocator` se inicializa con una dirección virtual
  (`nvidia.rs:1051`, código muerto); `gp_entry1` trunca `va_len/4` a 21 bits
  sin comprobar; buffer global de narración RM con carreras
  (`os_interface.rs:695-712`); `set_cursor(MOVE)` devuelve `true` sin hacer
  nada (`:7416-7427`); `wait_vblank` es un spin de CPU. `nvidia.rs` son tres
  programas en un archivo (bringup/GSP, KMS/HDMI-audio/cursor, nouveau-UAPI);
  el brazo `NR_EXEC` de 500+ líneas y el de `CHANNEL_ALLOC` deberían ser
  funciones; `GpuBringup` pasos 2-4 son un experimento pre-RM muerto.
- lunarbg/lunarbar: `kill_stale_instances` mata por nombre a nivel de
  sistema; SIGUSR1 antes de instalar el handler mata el fondo; popup y
  tooltip sin `set_buffer_scale` (borrosos en HiDPI); un `Axis` grande
  dispara `value/15` repintados completos; el repintado del popup reasigna
  un `Pixmap` de la salida entera por fila de hover; sonda de mezclador
  bloqueante antes de mapear (`sysinfo.rs:387-467`).
- Build: el wrapper trunca `/tmp/labwc.log` en cada arranque (adiós al log
  del crash); uid 0 codificado en todas partes; PNG de wallpaper de 4,3 MB
  que nada referencia; fuentes bitmap instaladas y luego borradas
  (`xorg.rs:130-133` vs `802-812`); `libeclipse_spawnfix.so` se empaqueta
  pero nunca se precarga; `tools/eclipse-xwayland` es un binario compilado
  sin regla de build ni consumidor; constantes duplicadas
  (`XDG_RUNTIME_DIR`, `PATH`, `DISPLAY`, socket de seatd, VT 7) entre
  `desktop.rs`, `mod.rs` y `eclipse-init`.

---

## 5. Lo que está bien

- Los ioctls de dos pasadas (GETRESOURCES, GETCONNECTOR, GETPLANERESOURCES,
  GETPLANE, OBJ_GETPROPERTIES, GETPROPERTY, GETPROPBLOB) respetan
  `count_*` correctamente; `size_of` asserts en todas las structs.
- Disciplina de locks documentada: reservas de VMO, llamadas al driver y
  staging CE se hacen con `DRM_STATE` soltado; orden entre `DRM_STATE` y
  `PENDING_DRM_TIMERS` consistente.
- Pacing de flips: un solo timer coalescido, cola (no `Option`) de
  completions, `FLIP_EVENT_PENDING` fijado atómicamente con el push, catch-up
  que nunca supera 60 Hz. `TEST_ONLY` atómico realmente sin efectos.
- `blit_from` comprueba `last_src`/`last_dst` antes del store NT, cada ruta
  escalar comprueba por fila, `create_fb` usa `checked_mul`; el cursor se
  compone en RAM cacheada y nunca hace RMW sobre la apertura WC/UC; el
  recorte en los cuatro bordes es correcto.
- NVIDIA: dispatch por NR con tabla de tamaños mínimos, NVIF con asserts de
  layout, refcount del *self-import* PRIME, orden de teardown al salir,
  `sfence` antes del doorbell, latch WEDGED, `RmGate`, `/proc/gpudbg`.
- lunarbg: un solo frame callback pendiente, keep-alive a 1 Hz ocluido,
  timer de planificación absoluta, toda la aritmética shm comprobada, fds
  `OwnedFd` + CLOEXEC, señales async-safe. Encoder PNG correcto (CRC,
  Adler-32, bloques stored verificados con `zlib` en Python).
- Build: tests `sh -n` y de parseo XML sobre cada fichero generado, backoff
  exponencial en init, wrappers que explican en `/dev/console` qué falta.

---

## 6. Orden de corrección sugerido

1. **Recuperar `master`** (sección 0) y proteger la rama.
2. F-C1 (`UserPtr` en todos los `io_control`) — cierra la escalada de
   privilegios y convierte los #PF en EFAULT.
3. F-C2 + F-A2 + F-A3 juntos: dar dueño real a la memoria GEM
   (`Arc<VmObject>` en mapeos y fbs) y borrar el evento sintético de `rmfb`.
4. F-A1: política de caché de los mapeos (una línea por sitio; medir con
   `drmbench` en hardware antes/después).
5. F-A8 + F-M4 + F-M10: nada de blits ni copias grandes con IRQs apagadas.
6. F-M1/F-M2/F-M3: semántica de `PAGE_FLIP`/`WAIT_VBLANK` (la asíncrona de
   `README-async-ioctl-vblank.md` sigue pendiente).
7. NVIDIA: F-A5, F-A6, F-A7, F-M14, F-M15 (y comprobar que #1072 vuelve).
8. Userspace: F-M19 (`munmap` inmediato) y F-M20; revertir la pérdida de
   `i18n.rs`.
9. Actualizar `README-drm.md`, `README-desktop.md` y `README-xorg.md` al
   código real.

---

## 7. Estado de corrección (rama `claude/eclipse-os-graphics-review-j4ya9s`)

Corregido en esta rama, sobre `9757fe5` (la fusión con el `master` local
`307f7e0` de la sección 0 queda pendiente de decisión del propietario):

| Hallazgo | Cambio |
|---|---|
| F-C1 | `access_ok()` en todos los `io_control` de DRM/fbdev y en las cuatro rutas de arrays de nouveau: tamaño del argumento desde `_IOC_SIZE(cmd)` + comprobación de cada puntero anidado (`ucheck`/`ucheck_n`, `user_slice_ok`). Nuevo `FsError::BadAddress` → EFAULT y `kernel_hal::user::user_range_ok`. |
| F-C2, F-A2 | El `mmap` de un dumb buffer comparte el `Arc<VmObject>` contiguo (`drm::handle_vmo`); cada framebuffer guarda su propia referencia (`fb_backing`) que `RMFB` libera. `GEM_CLOSE` ya no deja ni mapeos ni fbs sobre memoria devuelta al allocator. Test `gem_close_keeps_a_framebuffer_and_its_memory_alive`. |
| F-A1 | Dumb buffers mapeados cacheados (WB) de rebote por lo anterior; `/dev/fb0` pide `WriteCombining`. El mapeo de GEM nouveau sigue como estaba (su `phys_addr` puede ser BAR1). |
| F-A3 | `rmfb` sin evento sintético ni cola `pending_rmfb`. |
| F-A8 | `shadow_fb`: el rectángulo sucio y las celdas del cursor se copian bajo el lock y se blitean con él suelto; `blit_lock` (`try_lock`, nunca espera) serializa los presentes. |
| F-M1 | `PAGE_FLIP` valida `flags`/`reserved`, rechaza ASYNC/TARGET, ENOENT en fb desconocido y solo encola evento con `PAGE_FLIP_EVENT`. |
| F-M2 | Los `FLIP_COMPLETE` llevan `vblank_seq_now()`. |
| F-M3 | `WAIT_VBLANK` en modo evento respeta `RELATIVE`/`ABSOLUTE`/`NEXTONMISS` y entrega al alcanzar la secuencia. El bloqueante sigue sin esperar (ioctl síncrono). |
| F-M4 | Cursor > 64×64 → EINVAL; la copia del bitmap ya no se hace con `DRM_STATE` cogido. |
| F-M5 | `KD_GRAPHICS` se estampa en el VT dueño solo si sigue activo; si hubo cambio de VT a mitad de frame, se repinta la consola (`redraw_active_console`). |
| F-M11 | `README-drm.md` actualizado (render node, caps, syncobj, vblank, page flip, punteros). |
| Deuda de CI | Lints `clippy` preexistentes que rompían `deny(warnings)` en `btrfs-rs`, `nvidia.rs`, `nouveau_uapi.rs`, `procfs.rs` y `xhci_hid.rs`; tests de `block_mount` que no compilaban; doctest de `netlink`. |

Segundo bloque (misma rama):

| Hallazgo | Cambio |
|---|---|
| F-A4 | `cb3e197` (PR #1072, perdido en `master`) aplicado con *cherry-pick*: el fence de `EXEC` libera con `RELEASE_WFI_EN`. |
| F-A5 | `GEM_CPU_PREP` espera de verdad: encola una entrada sólo-fence tras todo lo que el proceso tiene en su anillo y espera a que aterrice (`cpu_prep_wait`); `NOWAIT` → EBUSY; 1 s → EBUSY. |
| F-A6 | `FastSlot::Preparing`: el primer hilo reclama la construcción bajo el lock y los demás esperan su veredicto; si el contexto se destruye durante la construcción, el estado se descarta en vez de publicarse. |
| F-A7 | `gem_mmap` lleva poseedores por pid (`register`/`add_ref`/`dec_ref`/`release_pid`/`holds`); `GEM_INFO`, `VM_BIND MAP`, `CPU_PREP`/`FINI` y `GEM_CLOSE` exigen ser creador o importador (`gem_usable_by`), `CHANNEL_FREE` exige ser dueño del canal, y la salida de un proceso suelta también sus auto-importaciones (cerrada la fuga documentada). En la tabla genérica, `gem_close` y el `mmap` de dumb buffers exigen ser el dueño. |
| Userspace | `tools/lunarbar/src/i18n.rs` restaurado del `master` perdido (lunarbar vuelve a compilar). lunarbg y lunarbar desmapean los pools shm retirados de inmediato (los `release` de buffers destruidos nunca llegan; `munmap` no afecta al mapeo del compositor). lunarbar: un cambio de escala reasigna el pool (antes escribía fuera del memfd) y ya no deja la barra sin configurar cuando el tamaño lógico no cambia. |
| Docs | `README-nouveau-uapi.md` (CPU_PREP, poseedores por pid), `README-desktop.md` (sesión real: servicios de init, lunarbar, seatd como demonio) y `README-xorg.md` (config generada: libinput, `AutoAddGPU` off, `ShadowFB`). |

Pendiente (sin cambiar en esta rama): F-M6 (activar el filtro del render
node requiere probarlo en el escritorio software-GL), F-M7 (estado por
apertura), F-M8, F-M9, F-M10, F-M12/F-M13 (virtio-gpu), F-M14/F-M15
(NVIDIA: lock durante `step16/17`, rol pegajoso del compositor), F-M16 a
F-M18, drmbench (F-M21) y build/sesión (F-M22 a F-M25). Nada de lo hecho
en NVIDIA ni en userspace se ha probado en hardware: compila (`make clippy
ARCH=x86_64 LINUX=1`, `cargo check` de las herramientas) y sigue la
semántica de Linux, pero la validación en la RTX queda para el propietario.
