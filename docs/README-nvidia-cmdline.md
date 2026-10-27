# Parámetros de arranque NVIDIA

Los flags `nvidia.*` de la línea de comandos (`cmdline=` en `zCore/rboot.conf`,
separados por `:`) se leen en **un solo sitio**,
[`kernel_hal::cmdline::NvidiaParams`](../kernel-hal/src/common/cmdline.rs), y
el kernel los publica igual que Linux publica los parámetros de un módulo, en
`/sys/module/nvidia/parameters/`.

| Flag | Qué hace | Equivalente en Linux |
| --- | --- | --- |
| `nvidia.cuda` | Activa CUDA: enciende de una vez `nvidia.nvidiactl` (`/dev/nvidiactl`, `/dev/nvidia<N>`) y `nvidia.uvm` (`/dev/nvidia-uvm`). Cada parte con `=0` se queda apagada. Ver [README-cuda.md](README-cuda.md). | cargar `nvidia` + `nvidia-uvm` |
| `nvidia.nouveau_uapi` | El nodo DRM habla la uAPI de nouveau (`DRM_IOCTL_VERSION` = `nouveau` 1.4.0), así Mesa (nouveau/NVK/zink) y labwc corren sobre la GPU. Solo se activa si alguna tarjeta puede servirla. Ver [README-nouveau-uapi.md](README-nouveau-uapi.md). | `nouveau.modeset=1` |
| `nvidia.multigpu` | Declara que hay más de una NVIDIA compatible y que el sistema debe levantarlas todas, no solo la del monitor. | — (Linux sondea todas) |
| `nvidia.hwcursor` | El puntero lo dibuja el plano de cursor del motor de display en vez de la CPU. | `DRM_MODE_CURSOR` en hardware (por defecto en nouveau) |
| `nvidia.nvkms` | Carga nvidia-modeset (nvkms, el mismo código que `nvidia-modeset.ko`) y le da cada GPU con el RM arriba. Reserva el display de cada tarjeta e informa en el log de lo que ve (bloque `[nvkms]`), y en la tarjeta de la consola hace el primer modeset: nvkms pasa a escanear la consola de la UEFI, al mismo tamaño, en cada monitor que tenga un modo de ese tamaño. El escritorio se ve igual; los monitores sin ese modo se apagan. Después, los fb en VRAM del compositor se muestran con flips de nvkms y el puntero es su plano de cursor; los fb en memoria del sistema se siguen copiando a la consola. Excluye la escalera propia del display (`nvidia.surfaceflip`, `nvidia.hwcursor`): las dos piden el canal core. Ver [README-nvkms-port.md](README-nvkms-port.md). | `nvidia-drm.modeset=1` |
| `nvidia.monitor=BB.DD.F` | Qué tarjeta lleva el monitor: esa pasa a ser `/dev/dri/card0`. PCI en hex con puntos, p. ej. `nvidia.monitor=65.00.0` es el `0000:65:00.0` de Linux (los `:` ya separan los flags). La BDF sale de `dmesg \| grep /dev/dri` o de `lspci`. | `boot_vga` / `BusID` de Xorg |
| `nvidia.compute=BB.DD.F` | Qué tarjeta hace cómputo (SAXPY, NVK EXEC, CE-present). | `CUDA_VISIBLE_DEVICES` |
| `nvidia.wlr_vulkan` / `nvidia.wlr_gles2` / `nvidia.wlr_pixman` | Renderer de wlroots para la sesión, igual que hasta ahora. | `WLR_RENDERER` |

Los booleanos aceptan la forma desnuda (`nvidia.cuda`) y `=1/true/on/yes` o
`=0/false/off/no`; cualquier otro valor cuenta como apagado.

## `/sys/module/nvidia/parameters`

Lo mismo que hace `kernel/params.c` en Linux: un fichero por parámetro, de solo
lectura, con un booleano como `Y`/`N` y una cadena no puesta como `(null)`.

```sh
$ cat /sys/module/nvidia/parameters/nouveau_uapi
Y
$ cat /sys/module/nvidia/parameters/monitor
65.00.0
```

Es lo que el kernel **aplicó**, no solo lo escrito: `nouveau_uapi` lee `N` si
ninguna tarjeta podía servirla aunque el flag estuviera, y `hwcursor` es lo que
usará el núcleo DRM. Por eso un script debería preguntar aquí en vez de hacer
`grep nvidia.x /proc/cmdline`, que dice que sí a `nvidia.x=0`.

Ejemplo para dos RTX con el monitor en la de `65:00.0`:

```
cmdline=LOG=warn:nvidia.cuda:nvidia.nouveau_uapi:nvidia.multigpu:nvidia.hwcursor:nvidia.monitor=65.00.0:ROOT=/dev/sda2
```
