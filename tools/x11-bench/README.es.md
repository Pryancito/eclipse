# Banco de interacción de X11 con el kernel

[English](README.md) · **Español**

Una prueba de humo en QEMU de la superficie del kernel que usa un servidor X
(`startx`) en Eclipse OS (zCore). Arranca zCore por UEFI con un init estático
diminuto en C (`xtest.c`) que, en un único arranque real, ejercita todas las
primitivas en las que se apoya Xorg:

- **Framebuffer**: `open("/dev/fb0")`, `FBIOGET_VSCREENINFO`, `mmap`, dibujar.
- **Entrada (evdev)**: `open("/dev/input/event0")`, `EVIOCGNAME`, `EVIOCGBIT`.
- **Paso de la VT a modo gráfico**: la secuencia de `xf86OpenConsole`/
  `LinuxInit` de kdrive: `VT_OPENQRY`/`VT_GETSTATE`/`VT_ACTIVATE`/
  `VT_WAITACTIVE`, luego `KDSKBMODE(K_RAW)` y `KDSETMODE(KD_GRAPHICS)` (los dos
  pasan el modo *por valor*), comprobando que `KDGETMODE` devuelve
  `KD_GRAPHICS`. Es el paso que se queda con la consola; tratar mal el
  argumento por valor hace fallar al kernel.
- **TTY**: `TIOCGWINSZ` sobre una tubería (tiene que funcionar, no `ENOTTY`).
- **AF_UNIX**: de sistema de ficheros *y* abstracto (`\0/tmp/.X11-unix/Xn`,
  el transporte principal de Xlib): `bind`/`listen`/`connect`, una escritura
  del cliente *antes* del `accept`, y luego `accept`/`read` (el saludo de
  establecimiento de conexión de X11).
- **Bucle de eventos**: `select`, `epoll` y `eventfd` informando de un socket
  listo.
- **Planificador**: `SIGALRM` con `setitimer`.
- **Hilo de entrada**: `pthread_create` + `join`.
- **`socketpair`**, y la escritura a temporal + `rename` atómico de **xauth**.

## Ejecutar

```sh
tools/x11-bench/run.sh
```

Imprime una línea `XTEST: [PASS|FAIL] <nombre>` por comprobación y `BENCH OK`
cuando pasan todas. Lo esperado:

```
XTEST: [PASS] fb screeninfo
XTEST: [PASS] fb mmap+draw
XTEST: [PASS] evdev EVIOCGNAME ps2-input
XTEST: [PASS] evdev EVIOCGBIT
XTEST: [PASS] vt graphics handoff
XTEST: [PASS] TIOCGWINSZ on pipe
XTEST: [PASS] unix fs socket
XTEST: [PASS] unix abstract socket
XTEST: [PASS] select
XTEST: [PASS] epoll
XTEST: [PASS] eventfd
XTEST: [PASS] SIGALRM/setitimer
XTEST: [PASS] pthread
XTEST: [PASS] socketpair
XTEST: [PASS] xauth write+rename
XTEST: 15/15 passed
XTEST: done
```

## Requisitos

- `qemu-system-x86_64`
- la cadena de herramientas `x86_64-linux-musl-cross` en `target/x86_64/`
  (la descarga `cargo rootfs`)
- `rboot/OVMF.fd`

Los tests unitarios complementarios del lado del host para el saludo AF_UNIX
están en `linux-object/src/net/unix.rs` (`cargo test -p linux-object
net::unix`).
