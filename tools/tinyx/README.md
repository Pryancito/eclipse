# TinyX (Xfbdev) para Eclipse OS

Servidor X mínimo basado en **kdrive** para Eclipse OS (zCore). Es el fork
[`tinycorelinux/tinyx`](https://github.com/tinycorelinux/tinyx) (Tiny Core
Linux), que rescata y mantiene los servidores **Xvesa** y **Xfbdev**.

En Eclipse OS usamos **`Xfbdev`**: un único binario que pinta directamente en el
framebuffer (`/dev/fb0`) y lee la entrada del teclado (la VT) y del ratón
(`/dev/input/mice`). Es justo la superficie de núcleo que valida
[`tools/x11-bench`](../x11-bench/) y que describe
[`docs/README-xorg.md`](../../docs/README-xorg.md).

## ¿Por qué TinyX y no Xorg?

`docs/README-xorg.md` explica cómo correr el **Xorg completo** (vía paquetes de
Alpine). Funciona, pero `Xorg` enlaza con decenas de bibliotecas compartidas y
espera `udev`/DRM; basta con que falte una `.so` para que aborte antes de
`main()`. TinyX evita todo eso:

| | Xorg | TinyX `Xfbdev` |
|---|---|---|
| Forma | servidor + drivers + muchas `.so` | un binario estático |
| Vídeo | `fbdev`/DRM | `/dev/fb0` directo |
| Entrada | `libinput`/`udev`/`evdev` | VT + `/dev/input/mice` |
| XKB | obligatorio | **no** (usa el keymap de consola) |
| GLX / DRI | sí | **no** |

`Xvesa` (el otro servidor de TinyX) usa `vm86`/E/S de puertos VESA reales y **no
es adecuado** para Eclipse OS; por eso el build solo genera `Xfbdev`.

## Disposición

```
tools/tinyx/
├── README.md                 ← este documento
├── build-tinyx.sh            ← cross-compila Xfbdev estático y lo copia al rootfs
├── build-xsysroot-static.sh  ← compila .a (fontenc, freetype, png, zlib, …)
├── fetch-xsysroot.sh         ← sysroot Alpine con headers/libXfont 1.x
├── fetch-xfonts.sh           ← fuentes bitmap fixed/cursor
├── eclipse/
│   ├── startx                ← launcher
│   └── xinitrc               ← ejemplo de sesión X
└── src/                      ← fuente TinyX vendorizada (tinyx 1.3, kdrive)
```

## Dependencias

`Xfbdev` solo necesita las cabeceras de kernel `linux/fb.h` (las aporta la
toolchain musl) y un puñado de bibliotecas de desarrollo de X que **no** se
vendorizan aquí:

- **protos**: `xorgproto` (xproto, randrproto, renderproto, fixesproto,
  damageproto, xcmiscproto, xextproto, xf86bigfontproto, scrnsaverproto,
  bigreqsproto, resourceproto, fontsproto, inputproto, kbproto)
- **`xtrans`** (cabeceras)
- **`libXfont` versión 1.x** y **`libfontenc`**

No usa `pixman`, `xkb`, `openssl` ni `udev`.

Proporciónalas mediante un *sysroot* apuntado con `XSYSROOT`. Por ejemplo, con
un sysroot cruzado de Alpine:

```sh
apk add --root "$XSYSROOT" --arch x86_64 \
    xorgproto-dev xtrans libxfont-dev libfontenc-dev util-macros
```

Si no encuentras `libXfont` v1 empaquetada, la fuente está en
<https://www.x.org/archive/individual/lib/> (ver el README de upstream en
`src/README`).

## Compilar

Con la integración en xtask (recomendado):

```sh
cargo rootfs --arch x86_64
```

Eso descarga la toolchain musl, rellena el sysroot X (`tools/tinyx/fetch-xsysroot.sh`),
compila las dependencias estáticas (`tools/tinyx/build-xsysroot-static.sh`),
genera un **`Xfbdev` totalmente estático** (sin `.so` en runtime), instala fuentes
bitmap y `startx` en el rootfs.

Manualmente:

```sh
tools/tinyx/build-tinyx.sh          # sysroot + build automáticos
tools/tinyx/build-tinyx.sh clean
```

## Fuentes en tiempo de ejecución

kdrive abre fuentes mediante `libXfont`; necesita al menos las fuentes de mapa
de bits `fixed` y `cursor`. Instálalas en el rootfs (p. ej. Alpine
`font-misc-misc` + `font-cursor-misc`) o pásale una ruta de fuentes:

```sh
Xfbdev :0 -fp /usr/share/fonts/misc ...
```

## Arrancar en Eclipse OS

Comprueba primero que el framebuffer y la entrada existen (`/dev/fb0`,
`/dev/input/mice`, `/dev/tty0`). Luego, por ejemplo:

```sh
# servidor en :0, 1024x768, tomando la VT 1
startx
# o manualmente:
Xfbdev :0 -screen 1024x768 -mouse /dev/input/mice vt1 &
export DISPLAY=:0
sh /etc/X11/xinitrc.tinyx        # o tus propios clientes X
```

Opciones útiles de `Xfbdev` (uso de kdrive):

- `-screen WIDTHxHEIGHT[xDEPTH]` — características de pantalla.
- `-mouse path[,n]` — dispositivo de ratón y nº de botones.
- `-zaphod` — desactiva el cambio de pantalla con el cursor.
- `-nozap` — no terminar con Ctrl+Alt+Retroceso.
- `vtXX` — usar la VT `XX` en vez de la siguiente libre.

Para usarlo como proceso inicial, apunta `ROOTPROC` en `zCore/rboot.conf` a un
script que lance `Xfbdev` y tu cliente (ver `README.md` raíz, sección ROOTPROC).

## Diagnóstico

Como con Xorg, el núcleo registra la cadena de `exec` y los errores del cargador
dinámico. Si `Xfbdev` muere sin pintar nada:

```sh
dmesg | grep -E 'EXECVE|XLOG'
```

- `XLOG: Error loading shared library …` / `Error relocating …` → falta una
  biblioteca (no debería ocurrir: `Xfbdev` es **estático** y no necesita `.so` X).
- `could not open default font 'fixed'` → faltan las fuentes (ver arriba).
- pantalla en negro pero sin error → revisa `-screen` y que `/dev/fb0`
  responda a `FBIOGET_VSCREENINFO` (lo valida `tools/x11-bench`).

## Licencia

La base de código original es MIT; los cambios del fork de Tiny Core Linux son
GPLv3 (ver `src/COPYING` y `src/README`).
