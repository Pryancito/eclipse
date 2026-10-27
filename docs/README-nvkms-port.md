# Plan: port de nvidia-modeset (nvkms) a Eclipse

Continuación de [README-nvidia-multigpu-diseno.md](README-nvidia-multigpu-diseno.md).
Decisión del 27-oct-2026: el modeset de todas las tarjetas lo hace **nvkms, el
mismo código que usa Linux**, no un modeset escrito a mano en el shim.

Hardware de referencia: 2× RTX 2060 SUPER (Turing, TU106, display C5/NVC57x).

---

## 1. Por qué hace falta

Hoy la escalera del display engine de `nvidia-rm-sys/vendor/eclipse_rm_init.c`
(canal core NVC57D, ventana NVC57E, cursor NVC57A) **reutiliza la cabeza que
dejó encendida la UEFI**. Nunca programa raster, SOR ni link training. Solo la
tarjeta de arranque tiene esa cabeza, así que:

- la segunda tarjeta no puede encender su monitor;
- un cambio de modo en la primera (otra resolución, otra frecuencia) tampoco
  es real: solo se cambia lo que se copia, no lo que se escanea.

En Linux todo eso lo hace `nvidia-modeset.ko`: un `NVDevEvoRec` por GPU, con
sus heads, SORs, DisplayPort (SST/MST, link training), HDMI (InfoFrames, FRL),
flips por hardware, cursor y LUT. `nvidia-drm.ko` lo usa a través de la
interfaz `nvkms-kapi`.

## 2. Qué hay en el árbol vendorizado

Todo en `nvidia-rm-sys/vendor/open-gpu-kernel-modules` (580.178.04, MIT):

| Parte | Ruta | Tamaño | Notas |
|---|---|---|---|
| nvkms | `src/nvidia-modeset/src` | ~114k líneas C | lista oficial en `src/nvidia-modeset/srcs.mk` (192 `.c`) |
| kapi | `src/nvidia-modeset/kapi/src` | ~5k líneas C | lo que usa `nvidia-drm`; compilado con nvkms |
| DisplayPort | `src/common/displayport` + `src/nvidia-modeset/src/dp` | ~40k líneas C++ | 35 `.cpp` en `SRCS_CXX` |
| modeset común | `src/common/modeset` (timing, HDMI packet) | ~31k | ya en `srcs.mk` |
| softfloat, push, headsurface, xz | `src/common/...` | — | ya en `srcs.mk` |
| capa Linux | `kernel-open/nvidia-modeset/nvidia-modeset-linux.c` | ~2.2k | **no se compila**: es lo que sustituimos |

Lo que nvkms necesita del entorno:

1. **Hooks del SO**: ~50 funciones `nvkms_*` declaradas en
   `src/nvidia-modeset/os-interface/include/nvidia-modeset-os-interface.h`
   (memoria, copyin/copyout, timers, semáforos, `nvkms_get_usec`,
   `nvkms_usleep`, log, refcount, `nvkms_open_gpu`/`close_gpu`,
   `nvkms_enumerate_gpus`, `nvkms_call_rm`, parámetros de módulo).
   En Linux las implementa `nvidia-modeset-linux.c`.
2. **El RM**: `nvkms_call_rm` termina en `rm_kernel_rmapi_op`
   (`src/nvidia/arch/nvalloc/unix/src/osapi.c`), un `switch` sobre
   `nvidia_kernel_rmapi_ops_t` que llama a `Nv04AllocWithAccessKernel`,
   `Nv04ControlKernel`, `Nv04MapMemoryKernel`… Esas funciones son del núcleo
   del RM y **ya las compilamos**. El `switch` está en la capa unix que
   filtramos, así que lo copiamos a nuestro shim como hicimos con `RmIoctl`
   en `eclipse_rm_frontend.c`.
3. **Callbacks RM → nvkms** (`nvidia_modeset_callbacks_t`): eventos de
   hotplug, vblank y fin de flip. En Linux llegan por `set_callbacks`.
4. **Runtime C++ mínimo**: la biblioteca DP se compila con `-fno-rtti
   -fno-exceptions -fcheck-new`. `operator new/delete` los define
   `dp_object.h` sobre `nvkms_alloc`, y `__cxa_pure_virtual` lo trae
   `nvkms-utils.c`. Hay que comprobar si quedan objetos globales con
   constructor (`.init_array`) y añadir `-fno-threadsafe-statics` si aparecen
   `__cxa_guard_*`.

## 3. Cómo encaja en Eclipse

```
linux-object (DRM core, cardN)            ← ya por tarjeta (PRs #2190, #2192)
    │  ioctls KMS
drivers/src/display/nvidia.rs             ← NvidiaGpu: un nvkms device por GPU
    │  FFI (nuevo módulo nvidia-rm-sys/src/nvkms.rs)
nvkms-kapi  →  nvkms  →  nvkms_call_rm  →  rm_kernel_rmapi_op (shim)  →  RM
                  ↑ hooks nvkms_* (vendor/eclipse_nvkms_os.c + Rust)
```

- Igual que `nvidia-drm` en Linux, el driver de Eclipse habla con nvkms solo
  por **kapi** (`nvKmsKapiGetFunctionsTable`): `allocateDevice`,
  `grabOwnership`, `getDeviceResourcesInfo`, `getDisplays`,
  `getConnectorInfo`, `applyModeSetConfig`, `allocateMemory`/`importMemory`,
  `createSurface`, eventos.
- Un `NvKmsKapiDevice` por `NvidiaGpu`. Nada global: es lo que pide el
  diseño multi-GPU.
- La escalera actual (hwflip/hwcursor/core propio) y nvkms **no pueden
  convivir en la misma GPU**: los dos asignan el canal core. Se elige por GPU
  con un parámetro de arranque, `nvidia.nvkms=1`, y la escalera sigue como
  respaldo hasta que nvkms tenga paridad en hardware real.
- El soft-KMS (blit al framebuffer de la UEFI) no cambia: sigue siendo el
  camino cuando no hay driver con KMS por hardware.

## 4. PRs

Cada uno pequeño, revisable y sin romper el escritorio que ya funciona.

**PR 1: nvkms compila dentro del kernel, sin activar.**
- `build.rs`: segundo `cc::Build` para `src/nvidia-modeset/srcs.mk`
  (reutilizando el parser de `src/srcs_mk.rs`, ampliado a `SRCS_CXX`), con
  las `-I`/`-D` del `Makefile` de nvidia-modeset y nuestras banderas de
  kernel (`-fPIC`, sin SSE). Los `.cpp` con `-std=gnu++11 -fno-rtti
  -fno-exceptions -fcheck-new -fno-threadsafe-statics`.
- Shaders de headsurface (`src/shaders/g_<arch>_shaders`, de maxwell a
  gb20x, Turing incluida) comprimidos con xz y embebidos como objetos de
  solo lectura, igual que hace el `Makefile`. Headsurface solo se usa para
  composición en el kernel (rotación, swapgroups); en un modeset normal no
  se ejecuta, pero el código lo referencia.
- Hooks `nvkms_*` como stubs que devuelven error, en
  `vendor/eclipse_nvkms_os.c`, para que enlace.
- Comprobación: `cargo bin -m virt-x86_64` enlaza y `nm` muestra
  `nvKmsKapiGetFunctionsTable`. Nadie lo llama todavía.

**PR 2: hooks del SO y conexión con el RM.**
- `rm_kernel_rmapi_op` y `nvidia_get_rm_ops` en el shim (copiados de
  `osapi.c` / `nv-modeset-interface.c`), sobre el cliente kernel del RM.
- Hooks reales: memoria sobre el allocator del kernel, timers sobre
  `kernel_hal::timer`, semáforos, `nvkms_get_usec`, log con `klog`.
- Tests de host para la parte Rust (timers, refcount, copyin/out).

**PR 3: `nvidia.nvkms` carga nvkms y le da cada GPU, sin modeset.**
- Los tres controles `OS_UNIX` sin los que `nvAllocDevEvo` no reserva el
  dispositivo, hasta ahora stubs con `NOT_SUPPORTED`, pasan al shim
  (`vendor/eclipse_nvkms_rm.c`) como en `unix_console.c` y
  `dynamic-power.c`: `VT_GET_FB_INFO` contesta con la consola de la UEFI
  (el `screen_info` de Linux, que el driver registra al arrancar) en la GPU
  cuyo BAR1 empieza en ella; `VT_SWITCH` y el refcount de GC6 aceptan sin
  hacer nada, porque aquí no hay consola VGA que devolver ni GC6.
- `nvKmsModuleLoad` una vez, bajo `nvkms_lock`.
- Cliente kapi en C (`vendor/eclipse_nvkms_drv.c`, la mitad de
  `nvidia-drm`): `allocateDevice` por GPU y una consulta de cabezas,
  conectores, pantallas, EDID y modos que el driver escribe en el log.
- Una tarea del arranque espera al bring-up diferido de la GPU de consola,
  carga nvkms en cada tarjeta y después atiende sus timers.
- nvkms y la escalera propia (`surfaceflip`/`hwcursor`) se excluyen por GPU:
  quien llega primero se queda el canal core.
- Como `nvidia-drm` antes de su primer commit, **la pantalla no cambia**:
  nvkms sigue escaneando lo que dejó la UEFI hasta que se aplica un modo.

**PR 4: modeset de la tarjeta de arranque.**
- PR 4a: la memoria de la consola (`NV01_MEMORY_FRAMEBUFFER_CONSOLE`, la
  misma que nvkms usa para restaurar la consola en
  `nvRmImportFbConsoleMemory`) como superficie de kapi, y el primer
  `applyModeSetConfig`, como el primer commit atómico de `nvidia-drm`: cada
  monitor conectado de la tarjeta de arranque, en su propia cabeza, a un modo
  del tamaño de la consola (el preferido si cabe, si no el más cercano a
  60 Hz), con la consola sin escalar como plano primario; las demás cabezas
  se apagan. Se valida (`commit = false`) antes de aplicarlo. El escritorio
  sigue igual, porque la CPU y el CE siguen pintando la misma memoria, pero
  ya lo escanea nvkms. Un monitor sin modo de ese tamaño se queda apagado y
  lo dice el log.
- PR 4b: flips de nvkms para los GEM en VRAM y cursor por kapi.
  - Cada fb en VRAM del compositor pasa a ser una superficie de kapi: su GEM
    se duplica (`NvRmDupObject`) del cliente RM del contexto que lo reservó
    al cliente de kapi, que es en lo que acaba el import por fd de
    `nvidia-drm`. Se envuelve como hace `ImportMemory`.
  - `page_flip` es un flip de kapi sobre la configuración que dejó el modeset:
    solo cambia la superficie del plano primario, y kapi lo convierte en
    `NVKMS_IOCTL_FLIP`. Como `nvidia-drm`, no se encola un flip mientras el
    anterior no ha llegado (`getFlipPendingStatus`); se espera hasta 50 ms.
  - Un fb que nvkms no puede escanear (sysmem, PRIME) se sigue copiando a la
    consola con la CPU o el CE, y antes se vuelve a poner la consola en
    pantalla. Cuando se cierra el GEM de un fb que está en pantalla, se vuelve
    a la consola y se espera a que ese flip llegue antes de soltar la
    superficie.
  - El cursor es el plano de cursor de nvkms: una superficie de 64x64
    A8R8G8B8 en VRAM, reservada por kapi y escrita por la transferencia BAR1
    del RM. Cada movimiento es un flip que solo toca el cursor.
    `nvidia.nvkms` activa el camino de cursor por hardware de la capa DRM.
- PR 4c: la región de consola se describe al RM después del arranque del GSP.
  - En tu hardware el RM no tenía región de consola, así que
    `NV01_MEMORY_FRAMEBUFFER_CONSOLE` fallaba y no había modeset.
  - Reservarla antes del arranque, como Linux (`nvidia.console_preserve`), mueve
    la ventana BAR1 por la que se pinta la consola. Sin nvkms la pantalla se
    congela; con nvkms sale negra.
  - Ahora, justo antes de `allocateDevice`, el shim describe el framebuffer de
    la UEFI como el memdesc de consola reservado, igual que
    `memmgrAllocateConsoleRegion`: VRAM desde el offset 0, porque sin la
    reserva el BAR1 sigue siendo una ventana lineal. No cambia ni el reparto
    del FB ni el BAR1.
  - nvkms importa y escanea la misma memoria en la que pintan la CPU y el CE.
- PR 4d: la LUT por defecto se comprueba en VRAM antes del modeset.
  - En tu hardware el modeset de la consola salía negro con señal, con 4c y
    con `nvidia.console_preserve`, y sin pantalla no había forma de leer el
    log.
  - En Turing, mientras nadie fija una LUT (el primer commit de nvidia-drm no
    lo hace, ni `show_console`), la ILUT de cada ventana y la OLUT de cada
    cabeza salen de la superficie de LUT por defecto de nvkms
    (`nvEvoInitDefaultLutC5`). nvkms la llena con rampas identidad a través de
    la CPU, por BAR1. Si esas escrituras no llegan a la VRAM, todo píxel sale
    negro.
  - Ahora, justo antes del modeset, el shim (`eclipse_nvkms_check_vidmem`) lee
    esa superficie por la ventana PRAMIN de BAR0, que no pasa por BAR1, y la
    compara con lo que ve la CPU. Lo que no coincide se reescribe por la misma
    ventana. Si la CPU lee ceros o la reescritura no lo arregla, no hay modeset
    y la pantalla se queda como la dejó el firmware, con el motivo en el log.
  - La línea del log da también el modo real de BAR1 (`BAR1_BLOCK`), cómo lo
    trata el RM y una muestra de la consola leída por la base de BAR1 y en
    VRAM 0.
- PR 4e: la LUT se repara desde una referencia, no desde lo que lee la CPU.
  - Con 4d la pantalla pasó de negra a blanca. La reparación copiaba a VRAM lo
    que la CPU leía de su mapeo BAR1. Una dirección de BAR1 sin nada detrás se
    lee como `0xffffffff`, y una LUT así lleva cada píxel al máximo.
  - Ahora `eclipse_kms_default_lut` genera la LUT correcta en memoria del
    kernel con el propio `InitDefaultLut` de nvkms, apuntado a ese buffer para
    una llamada. La VRAM se compara con ese buffer y se repara desde él. Lo que
    lee la CPU solo se cuenta: cuántas palabras lee bien y cuántas como
    `0xffffffff`, para el siguiente arreglo, que es el mapeo BAR1 en sí.
  - En tu hardware la referencia salió a ceros: el relleno de nvkms
    (`EvoSetupIdentityBaseLutC5` y la de salida) se salta una LUT que ya está
    marcada como identidad, y la por defecto lo está desde que se creó el
    dispositivo. Ahora las marcas se quitan solo durante esa llamada y se
    ponen otra vez después. El mismo log dio BAR1 virtual (`BAR1_BLOCK
    0x801f74c0`) y 0/256 palabras de la consola iguales por la base de BAR1 y
    en VRAM 0: la base de BAR1 ya no es la VRAM 0 que escanea la pantalla y
    escribe el present por CE.
  - Lo que explica las dos cosas, y que NVK no ejecute nada en esa tarjeta: el
    mismo arranque con `nvidia.nouveau_uapi` leyó el USERD de sus canales como
    `0xBAD0ACEF`/`0xBAD0ACF0`, el centinela de error de la GPU. Todo lo que
    pasa por BAR1 en la tarjeta de consola devuelve eso: el canal parece
    lleno (`NV_ERR_BUSY_RETRY` en el SELFTEST) y el contexto dorado caduca
    (`0x65`). En Linux, `kbusInitBar1` mapea la consola en BAR1 0 antes de
    poner BAR1 en virtual (`bPreserveBar1ConsoleEnabled`, para efifb); aquí
    no se mapeaba nada ahí, la consola del kernel seguía escribiendo en la
    base de BAR1, y el fallo de MMU de BAR1 que eso provoca solo se recupera
    rebindeando BAR1 desde el manejador de fallos del RM, que Eclipse no
    ejecuta. `eclipse_rm_preserve_bar1_console` describe la VRAM 0 como
    consola y activa ese mapeo antes de `gpuStateLoad`, sin `PRIMARY_DEVICE`
    ni la reserva previa al GSP de `nvidia.console_preserve`. El chequeo de la
    LUT dice ahora la primera palabra que lee la CPU y si es ese centinela.
    Con `nvidia.wlr_pixman` (sin nouveau_uapi) el escritorio ya sale por
    nvkms en hardware.
  - Con ese mapeo la tarjeta de consola sigue igual: el SELFTEST que pasó fue
    el de la otra tarjeta, y en la de consola el USERD se sigue leyendo como
    `0xBAD0ACxx` en cuanto termina su bring-up. `eclipse_rm_bar1_recover` hace
    la rama de fallos de BAR de `kgmmuServicePriFaults_GV100`: si la MMU tiene
    un fallo latcheado (o uno de BAR descartado detrás), rebindea BAR1, lo
    limpia y deja en el dmesg el estado, la dirección, el bloque de instancia y
    el `FAULT_INFO`. Corre al final de `gpustep9` y antes del SELFTEST; el
    chequeo de la LUT hace lo mismo y dice qué lee la CPU tras el rebind.
  - Con eso el dmesg mostró un fallo de escritura de la CPU en BAR1 0x80000
    (dentro de la consola) que el GSP ya había limpiado, y un rebind que no
    termina. El hardware pide que no llegue ningún acceso a BAR1 mientras se
    completa el bind (`kbusBar1InstBlkBind_TU102`), y durante el bring-up
    diferido la consola del kernel seguía pintando en BAR1 (la narración del
    RM con `console.overgraphics`, y los presents pendientes desde el tick de
    250 Hz). `kernel_hal::console::hold_presents_for` deja la consola en su
    búfer sombra durante esa ventana, con el mismo límite que la pausa del
    scanout. `eclipse_rm_bar1_recover` dice ahora siempre `BIND_STATUS`, si el
    RM mapeó la consola (`bBar1ConsolePreserved`) y la VRAM 0 leída por BAR1
    frente a PRAMIN, y rebindea también cuando BAR1 lee el centinela aunque la
    MMU no tenga nada latcheado.
  - Con BAR1 sana, labwc con gles2 renderiza por GPU y presenta, pero el
    escritorio no se ve: el present 2D por CE de la tarjeta de consola
    (`eclipse_rm_ce_blit_p2p_2d`, pensado para el P2P desde la otra tarjeta)
    escribía en la dirección PCIe de su propia BAR1, un peer-to-self que el
    root complex no tiene por qué reenviar. Ahora un destino dentro de la
    propia BAR1 y de la consola se escribe como `LOCAL_FB` en la VRAM de la
    consola, como ya hacía la versión 1D (`eclipse_rm_ce_blit`). El "Failed to
    disable CRTC 1" de wlroots no llega al kernel (ninguno de sus cuatro
    ioctls aparece rechazado): sale al perder el seat en un cambio de VT.
- Objetivo: **paridad** con la escalera actual en una sola tarjeta (escritorio,
  cursor, flips sin tearing) con `nvidia.wlr_vulkan`/`wlr_gles2`/`wlr_pixman`.

**PR 5: segunda tarjeta.**
- La misma ruta para cada GPU con monitor, con una superficie en VRAM
  reservada por kapi: card1 enciende su pantalla.
- `GETRESOURCES`/`GETCRTC` de card1 desde su `NvKmsKapiDevice`.
- Validación en tu hardware con un monitor en cada 2060 SUPER.

**Después:** `WLR_DRM_DEVICES` generado con todas las tarjetas con KMS (hoy
está fijado a `/dev/dri/card0` en `xtask/src/linux/mod.rs`,
`xtask/src/linux/desktop.rs` y `tools/eclipse-init/src/main.rs`), y retirar
la escalera cuando nvkms la haya sustituido en hardware real.

## 5. Riesgos

- **Tamaño del binario y tiempo de compilación**: ~190 ficheros más.
  Se compilan solo para el kernel x86_64, como el RM.
- **C++ en el kernel**: primera vez en este árbol. La biblioteca DP está
  hecha para entornos de kernel (sin excepciones ni RTTI), pero si aparecen
  constructores globales hay que ejecutar `.init_array` al arrancar.
- **Contexto de ejecución**: nvkms espera un único lock global (`nvkms_lock`)
  y callbacks desde timers; los hooks deben respetar eso sin bloquear con
  interrupciones desactivadas.
- **Validación**: solo tu hardware prueba el modeset. QEMU no tiene GPU
  NVIDIA, así que allí solo comprobamos que no cambia nada.
