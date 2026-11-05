# eclipse-sdl-probe

[Español](README.md) · **English**

A smoke test and micro-benchmark of **SDL** (SDL2, SDL3 and, through
sdl12-compat, SDL 1.2) for the Eclipse OS desktop. It tells which library,
which video backend and which *render driver* the session's `SDL_*` policy
actually produced (see [docs/README-desktop.en.md](../../docs/README-desktop.en.md),
"SDL" section), draws a moving colour bar and measures the fps.

It is built with the musl cross toolchain from `xtask` (`cargo rootfs` /
`cargo image`) and installed at `/bin/eclipse-sdl-probe`. It is a **dynamic**
binary that links only libc: it opens `libSDL2-2.0.so.0` or `libSDL3.so.0`
with `dlopen` at run time, so the host needs no SDL headers or libraries to
build the image.

## Usage

From a session terminal (`Super+Enter`) or from the desktop menu ("Prueba
SDL2 (renderer)" / "Prueba SDL3 (wl_shm)" / "Prueba SDL2 (por subsistema)"):

```sh
eclipse-sdl-probe                    # SDL2, SDL_Renderer, 300 frames
eclipse-sdl-probe --sdl3 --surface   # SDL3, window surface (pure wl_shm on pixman)
eclipse-sdl-probe --hold             # no limit: stats every 300 frames; a key or closing exits
eclipse-sdl-probe --size 1280x720 --frames 600
```

| Option | Effect |
|---|---|
| `--sdl3` | Probe `libSDL3.so.0` instead of `libSDL2-2.0.so.0`. |
| `--surface` | Draw through `SDL_GetWindowSurface`/`SDL_UpdateWindowSurface`, the path governed by `SDL_FRAMEBUFFER_ACCELERATION`, instead of an `SDL_Renderer` (the one governed by `SDL_RENDER_DRIVER`). |
| `--frames N` | Stop after N frames (300 by default; 0 = until closed). |
| `--hold` | Same as `--frames 0`. |
| `--steps` | Initialise **one subsystem at a time** (TIMER, EVENTS, VIDEO, AUDIO, JOYSTICK, GAMECONTROLLER), announcing each before running it. Draws nothing. |
| `--size WxH` | Window size (640x400 by default). |

## When something hangs inside `SDL_Init`

GZDoom asks for several subsystems in a single `SDL_Init` call and prints
nothing between its version banner and that call, so a hang there looks like a
program that stops for no reason: that is what Freedoom does on Eclipse, the
banner and nothing more. A combined call cannot say WHICH subsystem blocked;
`--steps` can, because the last `step <name> starting` line without its
`step <name> ok` after it names the culprit:

```sh
eclipse-sdl-probe --steps
```

The normal probe falls short here: it only asks for `SDL_INIT_VIDEO`, so it
kept passing while GZDoom hung. Output is unbuffered, so that a half-written
line is not lost in the stdio buffer when the process stalls.

## Expected output

One `SDLPROBE:` line per fact. In the default pixman session:

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

With `nvidia.wlr_gles2` (or `renderer=gl-sw` in QEMU) the renderer becomes
`opengles2`. `video driver in use: x11` with `WAYLAND_DISPLAY` present means
something overrode `SDL_VIDEODRIVER` and the app is taking the detour through
Xwayland. Any failure comes out as `SDLPROBE: FAIL ...` with the matching
`SDL_GetError()` and exit code 1; a failed `dlopen` means the library is
missing (the message still suggests `apk add`; in the Ubuntu image the
packages are `libsdl2-2.0-0` and `libsdl3-0`).

The pixman session's fps are capped by the compositor's *frame callback*
(vsync on the software scanout), not by the CPU: ~60 fps means "all good", not
a performance ceiling.
