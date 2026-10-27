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

**PR 3: nvkms toma la tarjeta de arranque (`nvidia.nvkms=1`).**
- `nvidia.rs`: `allocateDevice` + `grabOwnership` por GPU, conectores y
  modos desde kapi, `applyModeSetConfig` para SETCRTC/flip.
- Objetivo: **paridad** con la escalera actual en una sola tarjeta (escritorio,
  cursor, flips sin tearing) con `nvidia.wlr_vulkan`/`wlr_gles2`/`wlr_pixman`.
- Validación: QEMU (sin NVIDIA, no debe cambiar nada) y tu hardware.

**PR 4: segunda tarjeta.**
- La misma ruta para cada GPU con monitor: card1 enciende su pantalla.
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
