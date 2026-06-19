# Ejecutar un servidor X (`startx`) en Eclipse OS

Eclipse OS (zCore) expone la capa de consola/terminal virtual que un servidor X
de Linux necesita para tomar el control de la pantalla. Este documento explica
qué soporta el núcleo y cómo configurar el espacio de usuario (p. ej. Alpine)
para que `startx` funcione.

## Qué proporciona el núcleo

El núcleo implementa los dispositivos e `ioctl`s que `Xorg` usa en su rutina
`xf86OpenConsole`:

- **Nodos de consola**: `/dev/tty`, `/dev/tty0` (VT activo), `/dev/console` y
  `/dev/tty1`..`/dev/tty6`.
- **`ioctl`s de VT** (`<linux/vt.h>`): `VT_OPENQRY`, `VT_GETMODE`, `VT_SETMODE`,
  `VT_GETSTATE`, `VT_ACTIVATE`, `VT_WAITACTIVE`, `VT_RELDISP`, `VT_DISALLOCATE`.
- **`ioctl`s de KD** (`<linux/kd.h>`): `KDGETMODE`/`KDSETMODE`
  (`KD_TEXT`/`KD_GRAPHICS`, por VT), `KDGKBMODE`/`KDSKBMODE` (modo de teclado) y
  `KDGKBTYPE`. Cuando X pone el teclado en `K_OFF`/`K_RAW`, el núcleo deja de
  inyectar caracteres "cocidos" en ese TTY; los eventos crudos siguen llegando
  por `/dev/input/event*`.
- **Framebuffer**: `/dev/fb0` con `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO` y,
  como la resolución es fija, acepta `FBIOPUT_VSCREENINFO`, `FBIOPAN_DISPLAY` y
  `FBIOBLANK` para que el driver `fbdev` de X arranque.
- **Entrada**: `/dev/input/event*` y `/dev/input/mice` (PS/2 y virtio).
- **DRM**: `/dev/dri/card0` (parcial).
- **Cambio de VT**: Ctrl+Alt+F1..F6. El modo KD es por-VT, así que al salir del
  VT gráfico de X se sigue viendo una consola de texto normal.

## Configuración de userspace recomendada

El núcleo no incluye `udev` ni un KMS/DRM completo, así que conviene forzar el
driver **`fbdev`** y declarar la entrada **`evdev`** de forma estática en lugar
de depender del autodescubrimiento de `libinput`/`udev`.

Instala los paquetes (en Alpine):

```sh
apk add xorg-server xf86-video-fbdev xf86-input-evdev xinit mesa-dri-gallium
```

Crea `/etc/X11/xorg.conf.d/10-eclipse.conf` con:

```
Section "ServerFlags"
    Option "AutoAddDevices" "false"   # no hay udev: añadimos la entrada a mano
    Option "DontZap"        "false"
EndSection

Section "Device"
    Identifier "fb"
    Driver     "fbdev"
    Option     "fbdev" "/dev/fb0"
EndSection

Section "Screen"
    Identifier "screen"
    Device     "fb"
EndSection

Section "InputDevice"
    Identifier "keyboard"
    Driver     "evdev"
    Option     "Device" "/dev/input/event0"
    Option     "CoreKeyboard"
EndSection

Section "InputDevice"
    Identifier "mouse"
    Driver     "evdev"
    Option     "Device" "/dev/input/mice"
    Option     "CorePointer"
EndSection

Section "ServerLayout"
    Identifier  "layout"
    Screen      "screen"
    InputDevice "keyboard"
    InputDevice "mouse"
EndSection
```

Ajusta los `event0`/`mice` a los nodos reales que aparezcan en `/dev/input/`.

## Probar

```sh
startx
```

Si quieres ver el registro del servidor para diagnosticar:

```sh
Xorg -verbose 6 :0 vt1 2> /tmp/Xorg.log ; cat /tmp/Xorg.log
```

## Notas y limitaciones

- `VT_OPENQRY` devuelve el VT activo, de modo que X se apropia del terminal
  desde el que se lanzó `startx` (y al salir vuelve a él).
- El reenganche por señales `VT_PROCESS` (relsig/acqsig) se acepta pero no se
  emiten señales: cambiar de VT mientras X corre y volver puede requerir que la
  aplicación repinte.
- La aceleración por GPU no está disponible; usa el renderizado por software de
  Mesa (`llvmpipe`/`softpipe`), por eso se instalan `mesa-dri-gallium` y
  `llvm-libs`.
