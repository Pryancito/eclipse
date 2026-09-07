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

## Configuración de userspace

`cargo xtask image` instala los paquetes y genera la configuración; nada de
esto hay que hacerlo a mano en una imagen construida con xtask
(`xtask/src/linux/desktop.rs`, `write_xorg_config`). Para una instalación
manual sobre Alpine:

```sh
apk add xorg-server xf86-video-fbdev xf86-input-libinput xinit mesa-dri-gallium
```

El fichero generado es `/etc/X11/xorg.conf.d/10-eclipse.conf`:

```
Section "ServerFlags"
    Option "AutoAddDevices" "true"    # libinput clasifica cada /dev/input/event*
    Option "DontZap"        "false"
    # No tocar DRM: Xorg moderno sondea /dev/dri/card0 como GPU aunque el
    # Device sea fbdev, y en este kernel ese sondeo se cuelga ("Platform
    # probe for /sys/class/drm/card0" en Xorg.0.log; startx expira e init
    # relanza X cada ~90 s). Sin auto-add/auto-bind de GPU, X se queda en
    # la ruta fbdev.
    Option "AutoAddGPU"     "false"
    Option "AutoBindGPU"    "false"
EndSection

Section "Device"
    Identifier "fb"
    Driver     "fbdev"
    Option     "fbdev"    "/dev/fb0"
    Option     "ShadowFB" "true"
EndSection

Section "Screen"
    Identifier "screen"
    Device     "fb"
EndSection

# libinput para todo nodo evdev enumerado. Sin MatchIsKeyboard/MatchIsPointer:
# dependen de las etiquetas ID_INPUT_* de udev, que este kernel no emite, así
# que no casarían con nada. libinput clasifica el dispositivo por sus propias
# capacidades.
Section "InputClass"
    Identifier      "eclipse-libinput"
    MatchDevicePath "/dev/input/event*"
    Driver          "libinput"
EndSection

Section "ServerLayout"
    Identifier  "layout"
    Screen      "screen"
EndSection
```

Se usa `fbdev` sobre `/dev/fb0` y no `modesetting`: el *scanout* es software
en cualquier caso, así que escribir directamente al framebuffer lineal se
ahorra el *dumb buffer* + commit KMS por frame. El mapeo de `/dev/fb0` es
*write-combining*.

## Probar

```sh
startx
```

Si quieres ver el registro del servidor para diagnosticar:

```sh
Xorg -verbose 6 :0 vt1 2> /tmp/Xorg.log ; cat /tmp/Xorg.log
```

## Diagnóstico: `startx` no arranca y *no aparece ningún log de X*

Si `startx` termina al instante y no se genera `/tmp/Xorg.log` ni
`/var/log/Xorg.0.log`, el servidor X probablemente **muere antes de llegar a
`main()`**, dentro del cargador dinámico de musl (le falta una biblioteca
compartida o un símbolo). `Xorg` enlaza con muchas más `.so` que una aplicación
de consola, así que basta con que falte una para que aborte sin escribir nada en
su propio log.

El núcleo registra ahora en `dmesg` la cadena de `exec` y los errores del
cargador dinámico. Tras intentar `startx`, mira el log del kernel:

```sh
dmesg | grep -E 'EXECVE|XLOG'
```

- Las líneas `EXECVE[pid] "/ruta" argv=[...]` muestran exactamente qué binarios
  se ejecutan. Si **no** aparece ningún `EXECVE` con `Xorg`/`X`/`Xorg.wrap`, el
  problema está en `xinit`/`startx` (no encuentra el servidor): revisa
  `~/.xinitrc`, `$PATH` y que `/usr/bin/X` apunte al servidor.
- Las líneas `XLOG: Error loading shared library ...` o
  `XLOG: Error relocating ...: symbol not found` nombran la biblioteca o el
  símbolo que falta. Instala el paquete que la aporta.

También puedes ejecutar el servidor a mano para ver el error del enlazador en el
acto (musl lo escribe en `stderr`):

```sh
Xorg -version            # si imprime versión, el enlace dinámico está bien
LD_TRACE_LOADED_OBJECTS=1 Xorg   # lista las .so que necesita y cuáles faltan
```

Si `Xorg -version` falla con `Error loading shared library libfoo.so.N`,
instala el paquete correspondiente (`apk add ...`) y reintenta. Cuando
`Xorg -version` imprime la versión, el problema ya no es el enlazado y conviene
mirar `/tmp/Xorg.log` (sección anterior) para el siguiente fallo.

## Notas y limitaciones

- `VT_OPENQRY` devuelve el VT activo, de modo que X se apropia del terminal
  desde el que se lanzó `startx` (y al salir vuelve a él).
- El reenganche por señales `VT_PROCESS` (relsig/acqsig) está implementado al
  estilo de Linux: al pulsar Ctrl+Alt+Fn el kernel envía `relsig` al dueño del
  VT gráfico y espera su `VT_RELDISP` antes de conmutar; al volver envía
  `acqsig` para que el servidor recupere el DRM master y repinte. Si el
  compositor no coopera (VT_AUTO o cuelga), la conmutación es inmediata o se
  fuerza pulsando Ctrl+Alt+Fn otra vez, y la supresión de blits del VT gráfico
  no activo (ver `drm.rs`) mantiene visible la consola de texto como red de
  seguridad.
- La aceleración por GPU no está disponible; usa el renderizado por software de
  Mesa (`llvmpipe`/`softpipe`), por eso se instalan `mesa-dri-gallium` y
  `llvm-libs`.

## Pseudo-terminales (PTY)

Los emuladores de terminal bajo X (xterm, st, rxvt…) necesitan pseudo-terminales
para arrancar una shell. Eclipse OS las expone igual que Linux:

- `/dev/ptmx`: abrir este nodo crea un par maestro/esclavo nuevo y devuelve el
  *maestro*. `ptsname(3)` resuelve el número con `ioctl(TIOCGPTN)` y `unlockpt(3)`
  usa `ioctl(TIOCSPTLCK)` (aceptado; no se exige para abrir el esclavo).
- `/dev/pts/N`: el *esclavo*. El programa que corre en él (la shell) lo ve como
  un terminal real: `tcgetattr`/`tcsetattr`, `TIOCGWINSZ`/`TIOCSWINSZ`,
  grupos de proceso en primer plano (`TIOCGPGRP`/`TIOCSPGRP`) y señales
  (`Ctrl+C`→SIGINT, `SIGWINCH` al redimensionar, `SIGHUP` al cerrar el maestro).

La disciplina de línea (modo canónico/raw, eco, traducción CR/NL) corre en el
lado del esclavo: lo que escribe el emulador en el maestro se procesa y, con eco
activo, vuelve al maestro para que la terminal muestre lo tecleado.
