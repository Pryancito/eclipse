# eclipse-sdl-probe

Prueba de humo y micro-bench de **SDL** (SDL2, SDL3 y, a través de
sdl12-compat, SDL 1.2) para el escritorio de Eclipse OS. Dice qué biblioteca,
qué backend de vídeo y qué *render driver* ha producido realmente la política
`SDL_*` de la sesión (ver [docs/README-desktop.md](../../docs/README-desktop.md),
sección «SDL»), dibuja una barra de color en movimiento y mide los fps.

Se compila con la toolchain musl cruzada desde `xtask` (`cargo rootfs` /
`cargo image`) y se instala en `/bin/eclipse-sdl-probe`. Es un binario
**dinámico** que solo enlaza libc: abre `libSDL2-2.0.so.0` o `libSDL3.so.0`
con `dlopen` en tiempo de ejecución, así que el host no necesita cabeceras ni
bibliotecas SDL para construir la imagen.

## Uso

Desde un terminal de la sesión (`Super+Enter`) o desde el menú de escritorio
(«Prueba SDL2 (renderer)» / «Prueba SDL3 (wl_shm)»):

```sh
eclipse-sdl-probe                    # SDL2, SDL_Renderer, 300 frames
eclipse-sdl-probe --sdl3 --surface   # SDL3, superficie de ventana (wl_shm puro en pixman)
eclipse-sdl-probe --hold             # sin límite: stats cada 300 frames; tecla o cerrar para salir
eclipse-sdl-probe --size 1280x720 --frames 600
```

| Opción | Efecto |
|---|---|
| `--sdl3` | Sondea `libSDL3.so.0` en vez de `libSDL2-2.0.so.0`. |
| `--surface` | Dibuja por `SDL_GetWindowSurface`/`SDL_UpdateWindowSurface`, la ruta que gobierna `SDL_FRAMEBUFFER_ACCELERATION`, en lugar de un `SDL_Renderer` (la que gobierna `SDL_RENDER_DRIVER`). |
| `--frames N` | Para tras N frames (300 por defecto; 0 = hasta cerrar). |
| `--hold` | Igual que `--frames 0`. |
| `--size WxH` | Tamaño de la ventana (640x400 por defecto). |

## Salida esperada

Una línea `SDLPROBE:` por hecho. En la sesión pixman por defecto:

```
SDLPROBE: library libSDL2-2.0.so.0
SDLPROBE: env SDL_VIDEODRIVER=wayland,x11
SDLPROBE: env SDL_RENDER_DRIVER=software
SDLPROBE: env SDL_FRAMEBUFFER_ACCELERATION=0
...
SDLPROBE: version 2.32.x
SDLPROBE: video drivers compiled in: wayland x11 kmsdrm offscreen dummy
SDLPROBE: render drivers compiled in: opengl opengles2 software
SDLPROBE: video driver in use: wayland
SDLPROBE: draw path: renderer 'software' (SDL_RENDER_DRIVER=software)
SDLPROBE: 300 frames in 5.02 s: 59.8 fps, 16.73 ms/frame
SDLPROBE: OK
```

Con `nvidia.wlr_gles2` (o `renderer=gl-sw` en QEMU) el renderer pasa a
`opengles2`. `video driver in use: x11` con `WAYLAND_DISPLAY` presente
significa que algo pisó `SDL_VIDEODRIVER` y la app está dando el rodeo por
Xwayland. Cualquier fallo sale como `SDLPROBE: FAIL ...` con el
`SDL_GetError()` correspondiente y código de salida 1; `dlopen` fallido
indica el `apk add` que falta.

Los fps de la sesión pixman los limita el *frame callback* del compositor
(vsync sobre el scanout software), no la CPU: ~60 fps es «todo bien», no
un techo de rendimiento.
